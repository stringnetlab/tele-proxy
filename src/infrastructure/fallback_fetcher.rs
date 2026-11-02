use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header;

use crate::application::settings_store::SettingsStore;
use crate::domain::errors::ProxyError;
use crate::domain::models::CachedResponse;
use crate::domain::services::{DnsResolver, FallbackFetcher};
use crate::domain::validators::validate_url_strict;
use crate::infrastructure::http_client;

/// Techo del cuerpo de un fallback configurado. Es un recurso pequeño que se sirve en lugar de un
/// error, no el contenido real: aceptar hasta `max_response_bytes` (hasta 512 MB) convertiría la
/// ruta de error en el mayor consumidor de memoria del proceso.
pub const MAX_FALLBACK_BYTES: u64 = 10 * 1024 * 1024;

/// Salida de red de los `error_handling.fallback_urls`. Aplica el mismo pipeline anti-SSRF que el
/// webhook (esquema/credenciales → IP literal → DNS validado → pinning TCP) pero **sin whitelist
/// del cliente**: la URL la eligió el operador y se validó al escribir la config. Los redirects no
/// se siguen (`pinned_client` usa `Policy::none()`), así que un 3xx se reporta tal cual y el
/// llamador lo descarta.
pub struct ValidatingFallbackFetcher {
    dns_resolver: Arc<dyn DnsResolver>,
    /// Ajustes recargables: `fallback_timeout_ms` se lee por fetch para que un cambio en caliente
    /// afecte al siguiente error sin reconstruir el fetcher.
    settings: Arc<SettingsStore>,
}

impl ValidatingFallbackFetcher {
    pub fn new(dns_resolver: Arc<dyn DnsResolver>, settings: Arc<SettingsStore>) -> Self {
        Self {
            dns_resolver,
            settings,
        }
    }
}

#[async_trait]
impl FallbackFetcher for ValidatingFallbackFetcher {
    async fn fetch(&self, url_str: &str) -> Result<CachedResponse, ProxyError> {
        let url = validate_url_strict(url_str)?;

        let hostname = url.host_str().ok_or_else(|| ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: "URL sin host".to_string(),
        })?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ProxyError::InvalidUrlFormat {
                url: url_str.to_string(),
                reason: "URL sin puerto determinable".to_string(),
            })?;

        let resolved_ip = match hostname.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => self.dns_resolver.resolve_and_validate(hostname).await?,
        };

        let timeout = Duration::from_millis(self.settings.snapshot().fallback_timeout_ms);
        let client = http_client::pinned_client(hostname, resolved_ip, port, timeout)?;

        let response = client
            .get(url.as_str())
            .header(header::USER_AGENT, "teleproxy-fallback/1.0")
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProxyError::UpstreamTimeout {
                        url: url_str.to_string(),
                        timeout_ms: timeout.as_millis() as u64,
                    }
                } else {
                    ProxyError::UpstreamError {
                        url: url_str.to_string(),
                        upstream_status: 0,
                        reason: e.to_string(),
                    }
                }
            })?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = http_client::read_body_capped(response, MAX_FALLBACK_BYTES).await?;

        // Solo se conserva el `content-type`: el resto de cabeceras del origen del fallback
        // (cookies, cache-control, redirecciones) no tiene por qué llegar al cliente.
        let headers = content_type
            .into_iter()
            .map(|value| ("content-type".to_string(), value))
            .collect();

        Ok(CachedResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::Mutex;

    use super::*;
    use crate::domain::runtime_settings::RuntimeSettings;

    struct RecordingResolver {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingResolver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .map(|calls| calls.clone())
                .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
        }
    }

    #[async_trait]
    impl DnsResolver for RecordingResolver {
        async fn resolve_and_validate(&self, hostname: &str) -> Result<IpAddr, ProxyError> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(hostname.to_string());
            }
            Err(ProxyError::SsrfBlocked {
                url: hostname.to_string(),
                resolved_ip: "169.254.169.254".to_string(),
                reason: "DNS resolved to private IP".to_string(),
            })
        }
    }

    fn fetcher(resolver: &Arc<RecordingResolver>) -> ValidatingFallbackFetcher {
        ValidatingFallbackFetcher::new(
            Arc::clone(resolver) as Arc<dyn DnsResolver>,
            Arc::new(SettingsStore::new(RuntimeSettings::defaults())),
        )
    }

    #[tokio::test]
    async fn una_ip_privada_literal_se_rechaza_sin_consultar_el_resolver() {
        let resolver = RecordingResolver::new();
        let error = fetcher(&resolver)
            .fetch("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("SSRF");

        assert!(matches!(error, ProxyError::SsrfBlocked { .. }), "{error:?}");
        assert!(resolver.calls().is_empty());
    }

    #[tokio::test]
    async fn esquema_o_credenciales_se_rechazan_antes_de_resolver() {
        let resolver = RecordingResolver::new();
        for url in [
            "file:///etc/passwd",
            "https://user:pass@cdn.example.com/f.png",
        ] {
            let error = fetcher(&resolver)
                .fetch(url)
                .await
                .expect_err("URL inválida");
            assert!(
                matches!(error, ProxyError::InvalidUrlFormat { .. }),
                "{url}: {error:?}"
            );
        }
        assert!(resolver.calls().is_empty());
    }

    #[tokio::test]
    async fn un_host_publico_pasa_por_el_resolver_validado() {
        let resolver = RecordingResolver::new();
        let error = fetcher(&resolver)
            .fetch("https://cdn.example.com/fallback.png")
            .await
            .expect_err("sin red en tests");

        assert!(matches!(error, ProxyError::SsrfBlocked { .. }), "{error:?}");
        assert_eq!(resolver.calls(), vec!["cdn.example.com".to_string()]);
    }
}
