use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header;
use url::Url;

use crate::domain::errors::ProxyError;
use crate::domain::services::DnsResolver;
use crate::domain::validators::{extract_domain, host_allowed, validate_url_strict};
use crate::infrastructure::http_client;

/// Tope de saltos de redirect. Cada salto se **revalida** (whitelist + anti-SSRF de IP) porque
/// `pinned_client` lleva `redirect::Policy::none()`: seguir un `Location:` sin revalidar
/// convertiría un host autorizado en un proxy hacia redes internas.
pub const MAX_REDIRECTS: usize = 5;

/// Respuesta del fetch de preview: todo lo que la cascada de detectores y el análisis antibot
/// necesitan, ya acotado y con la URL final resuelta.
#[derive(Debug, Clone)]
pub struct FetchedResponse {
    pub status: u16,
    /// URL tras los saltos de redirect revalidados (la que se reporta en `data.url`).
    pub final_url: String,
    /// `Content-Type` completo del origen (con parámetros, p.ej. `charset=`).
    pub content_type: Option<String>,
    /// Headers en minúscula (nombre → primer valor), para la detección antibot.
    pub headers: Vec<(String, String)>,
    /// Nombres de cookie presentes en `Set-Cookie`.
    pub cookies: Vec<String>,
    /// Cuerpo crudo (sin decodificar charset); lo decodifica `parse_html`.
    pub body: Vec<u8>,
}

/// Fetch validado de preview. Repite el pipeline anti-SSRF de `/aq/` y del webhook (esquema →
/// whitelist → DNS validado → pinning TCP) y añade el seguimiento **manual y revalidado** de
/// redirects, que el proxy de imágenes no necesita.
pub struct PreviewFetcher {
    dns_resolver: Arc<dyn DnsResolver>,
    timeout: Duration,
    max_body_bytes: u64,
}

impl PreviewFetcher {
    pub fn new(dns_resolver: Arc<dyn DnsResolver>, timeout: Duration, max_body_bytes: u64) -> Self {
        Self {
            dns_resolver,
            timeout,
            max_body_bytes,
        }
    }

    pub async fn fetch(
        &self,
        url_str: &str,
        whitelist: &[String],
        wildcard: bool,
    ) -> Result<FetchedResponse, ProxyError> {
        let mut current = validate_url_strict(url_str)?;

        for _hop in 0..=MAX_REDIRECTS {
            let domain = extract_domain(&current)?;
            if !host_allowed(
                &domain,
                current.port_or_known_default(),
                whitelist,
                wildcard,
            ) {
                return Err(ProxyError::DomainNotWhitelisted { domain });
            }

            let hostname = current
                .host_str()
                .ok_or_else(|| ProxyError::InvalidUrlFormat {
                    url: current.to_string(),
                    reason: "URL sin host".to_string(),
                })?;
            let port =
                current
                    .port_or_known_default()
                    .ok_or_else(|| ProxyError::InvalidUrlFormat {
                        url: current.to_string(),
                        reason: "URL sin puerto determinable".to_string(),
                    })?;

            // IP literal: `validate_url_strict` ya rechazó las privadas/loopback/metadata. Nombre:
            // el resolver valida la IP resuelta antes de conectar.
            let resolved_ip = match hostname.parse::<IpAddr>() {
                Ok(ip) => ip,
                Err(_) => self.dns_resolver.resolve_and_validate(hostname).await?,
            };

            let client = http_client::pinned_client(hostname, resolved_ip, port, self.timeout)?;

            let response = client
                .get(current.as_str())
                .header(header::USER_AGENT, "teleproxy-preview/1.0")
                .header(header::ACCEPT, "text/html,application/xhtml+xml,*/*;q=0.8")
                .send()
                .await
                .map_err(|e| self.map_send_error(&current, e))?;

            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| ProxyError::UpstreamError {
                        url: current.to_string(),
                        upstream_status: status.as_u16(),
                        reason: "redirect sin cabecera Location".to_string(),
                    })?;
                // `Location` puede ser relativo: se resuelve contra la URL actual y se revalida
                // (esquema/host) en el siguiente salto del bucle.
                let next = current
                    .join(location)
                    .map_err(|e| ProxyError::InvalidUrlFormat {
                        url: location.to_string(),
                        reason: format!("Location inválido: {e}"),
                    })?;
                current = validate_url_strict(next.as_str())?;
                continue;
            }

            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let headers = collect_headers(response.headers());
            let cookies = collect_cookie_names(response.headers());

            let body = http_client::read_body_capped(response, self.max_body_bytes)
                .await
                .map_err(convert_payload_too_large)?;

            return Ok(FetchedResponse {
                status: status.as_u16(),
                final_url: current.to_string(),
                content_type,
                headers,
                cookies,
                body,
            });
        }

        Err(ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: format!("demasiados redirects (máx {MAX_REDIRECTS})"),
        })
    }

    fn map_send_error(&self, url: &Url, e: reqwest::Error) -> ProxyError {
        if e.is_timeout() {
            ProxyError::UpstreamTimeout {
                url: url.to_string(),
                timeout_ms: self.timeout.as_millis() as u64,
            }
        } else {
            ProxyError::UpstreamError {
                url: url.to_string(),
                upstream_status: 0,
                reason: e.to_string(),
            }
        }
    }
}

fn convert_payload_too_large(e: ProxyError) -> ProxyError {
    match e {
        ProxyError::PayloadTooLarge {
            content_length,
            max_allowed,
        } => ProxyError::PreviewPayloadTooLarge {
            content_length,
            max_allowed,
        },
        other => other,
    }
}

fn collect_headers(headers: &header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_ascii_lowercase(), v.to_string()))
        })
        .collect()
}

/// Extrae el **nombre** de cada cookie de los `Set-Cookie` (lo anterior al primer `=`), que es lo
/// único que la tabla antibot necesita.
fn collect_cookie_names(headers: &header::HeaderMap) -> Vec<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|raw| raw.split('=').next())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_names_se_extraen_del_set_cookie() {
        let mut headers = header::HeaderMap::new();
        headers.append(
            header::SET_COOKIE,
            header::HeaderValue::from_static("__cf_bm=abc; Path=/; HttpOnly"),
        );
        headers.append(
            header::SET_COOKIE,
            header::HeaderValue::from_static("datadome=xyz; Secure"),
        );
        let names = collect_cookie_names(&headers);
        assert_eq!(names, vec!["__cf_bm".to_string(), "datadome".to_string()]);
    }

    #[test]
    fn headers_se_normalizan_a_minuscula() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::SERVER,
            header::HeaderValue::from_static("Cloudflare"),
        );
        let pairs = collect_headers(&headers);
        assert!(pairs
            .iter()
            .any(|(k, v)| k == "server" && v == "Cloudflare"));
    }

    #[test]
    fn payload_too_large_se_convierte_en_preview() {
        let e = ProxyError::PayloadTooLarge {
            content_length: 3,
            max_allowed: 2,
        };
        assert!(matches!(
            convert_payload_too_large(e),
            ProxyError::PreviewPayloadTooLarge { .. }
        ));
    }
}
