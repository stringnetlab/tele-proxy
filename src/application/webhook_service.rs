use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::errors::ProxyError;
use crate::domain::models::{WebhookRequest, WebhookResponse};
use crate::domain::services::{DnsResolver, WebhookFetcher};
use crate::domain::validators::{domain_matches_whitelist, extract_domain, validate_url_strict};
use crate::infrastructure::http_client;

/// Techo del cuerpo que un webhook puede devolver. La respuesta se reinyecta en la VM de Lua como
/// argumento de `proxy.http_request`, así que se fija en la escala de `MAX_SCRIPTING_BODY_BYTES`
/// (5 MB): un origen lento no puede obligar al sandbox a bufferizar más de lo que el propio script
/// tendría permitido recibir.
pub const MAX_RESPONSE_BYTES: u64 = 5 * 1024 * 1024;

/// Verbos que el sandbox puede expresar. Se comprueban antes de resolver el nombre: un método que
/// el fetcher no sabe ejecutar no debe gastar una búsqueda DNS ni dejar rastro en el origen.
const SUPPORTED_METHODS: [&str; 4] = ["GET", "POST", "PUT", "DELETE"];

fn unsupported_method(url: &str, method: &str) -> ProxyError {
    ProxyError::InvalidUrlFormat {
        url: url.to_string(),
        reason: format!("Unsupported webhook method '{method}'"),
    }
}

/// Único camino de salida para los webhooks del sandbox Lua. Aplica, en el mismo orden que el
/// proxy de imágenes, los cuatro filtros que `docs/BDD.md` (Feature 6) exige antes de abrir un
/// socket: esquema/credenciales/fragmento, whitelist del cliente, resolución DNS segura y
/// fijación de la conexión a la IP validada.
pub struct ValidatingWebhookFetcher {
    dns_resolver: Arc<dyn DnsResolver>,
    /// Techo del `WEBHOOK_TIMEOUT_MS` de entorno: una petición que pida más se recorta aquí.
    timeout_ms: u64,
    max_response_bytes: u64,
}

impl ValidatingWebhookFetcher {
    pub fn new(
        dns_resolver: Arc<dyn DnsResolver>,
        timeout_ms: u64,
        max_response_bytes: u64,
    ) -> Self {
        Self {
            dns_resolver,
            timeout_ms,
            max_response_bytes,
        }
    }
}

#[async_trait]
impl WebhookFetcher for ValidatingWebhookFetcher {
    async fn fetch(&self, request: &WebhookRequest) -> Result<WebhookResponse, ProxyError> {
        // `validate_url_strict` ya rechaza una IP privada escrita literalmente en la URL; el
        // resto del peligro (un nombre que apunta a `169.254.169.254`) se cierra con el resolver.
        let url = validate_url_strict(&request.url)?;
        let domain = extract_domain(&url)?;

        if !domain_matches_whitelist(&domain, &request.whitelist) {
            return Err(ProxyError::DomainNotWhitelisted { domain });
        }

        if !SUPPORTED_METHODS.contains(&request.method.as_str()) {
            return Err(unsupported_method(&request.url, &request.method));
        }

        let hostname = url.host_str().ok_or_else(|| ProxyError::InvalidUrlFormat {
            url: request.url.clone(),
            reason: "URL has no host".to_string(),
        })?;

        let port = url
            .port_or_known_default()
            .ok_or_else(|| ProxyError::InvalidUrlFormat {
                url: request.url.clone(),
                reason: "URL has no determinable port".to_string(),
            })?;

        let resolved_ip = match hostname.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => self.dns_resolver.resolve_and_validate(hostname).await?,
        };

        let timeout = Duration::from_millis(request.timeout_ms.min(self.timeout_ms));
        let client = http_client::pinned_client(hostname, resolved_ip, port, timeout)?;

        let mut request_builder = match request.method.as_str() {
            "GET" => client.get(url.as_str()),
            "POST" => client.post(url.as_str()),
            "PUT" => client.put(url.as_str()),
            "DELETE" => client.delete(url.as_str()),
            other => return Err(unsupported_method(&request.url, other)),
        };

        if let Some(body) = &request.body {
            request_builder = request_builder.body(body.clone());
        }

        let response = request_builder.send().await.map_err(|e| {
            if e.is_timeout() {
                ProxyError::WebhookTimeout {
                    webhook_url: request.url.clone(),
                    timeout_ms: timeout.as_millis() as u64,
                }
            } else {
                // El `Display` de reqwest puede traer la URL (nunca credenciales: el validador
                // las prohíbe) y solo viaja hasta el log; un 502 no se lo muestra al cliente.
                ProxyError::WebhookFailed {
                    url: request.url.clone(),
                    reason: e.to_string(),
                }
            }
        })?;

        let status = response.status().as_u16();
        let body = http_client::read_body_capped(response, self.max_response_bytes).await?;

        Ok(WebhookResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::Mutex;

    use super::*;

    /// Resolver de test que registra cada hostname pedido y se comporta como `SecureDnsResolver`
    /// cuando el nombre apunta a una red interna. Que `calls` quede vacío es la prueba de que la
    /// petición se rechazó **antes** de la fase de abrir un socket.
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

    fn request(url: &str, method: &str, whitelist: &[&str]) -> WebhookRequest {
        WebhookRequest {
            url: url.to_string(),
            method: method.to_string(),
            body: None,
            timeout_ms: 1_000,
            whitelist: whitelist.iter().map(|entry| entry.to_string()).collect(),
        }
    }

    async fn fetch_with(resolver: &Arc<RecordingResolver>, req: &WebhookRequest) -> ProxyError {
        let fetcher = ValidatingWebhookFetcher::new(
            Arc::clone(resolver) as Arc<dyn DnsResolver>,
            5_000,
            MAX_RESPONSE_BYTES,
        );
        match fetcher.fetch(req).await {
            Err(error) => error,
            Ok(_) => panic!("el webhook no debía resolverse sin red en los tests"),
        }
    }

    #[tokio::test]
    async fn ip_privada_literal_se_rechaza_sin_consultar_el_resolver() {
        let resolver = RecordingResolver::new();
        let req = request(
            "http://169.254.169.254/latest/meta-data/",
            "GET",
            &["169.254.169.254"],
        );

        let error = fetch_with(&resolver, &req).await;

        assert!(matches!(error, ProxyError::SsrfBlocked { .. }), "{error:?}");
        assert!(
            resolver.calls().is_empty(),
            "no se debe resolver ningún nombre"
        );
    }

    #[tokio::test]
    async fn host_no_autorizado_se_rechaza_sin_consultar_el_resolver() {
        let resolver = RecordingResolver::new();
        let req = request(
            "http://metadata.google.internal/computeMetadata/v1/",
            "GET",
            &["api.example.com"],
        );

        let error = fetch_with(&resolver, &req).await;

        assert!(
            matches!(error, ProxyError::DomainNotWhitelisted { .. }),
            "{error:?}"
        );
        assert!(
            resolver.calls().is_empty(),
            "no se debe resolver ningún nombre"
        );
    }

    #[tokio::test]
    async fn url_con_credenciales_o_esquema_ajeno_se_rechaza_antes_de_todo() {
        let resolver = RecordingResolver::new();

        let credentialed = fetch_with(
            &resolver,
            &request(
                "https://user:pass@api.example.com/",
                "GET",
                &["api.example.com"],
            ),
        )
        .await;
        assert!(
            matches!(credentialed, ProxyError::InvalidUrlFormat { .. }),
            "{credentialed:?}"
        );

        let scheme = fetch_with(
            &resolver,
            &request("file:///etc/passwd", "GET", &["api.example.com"]),
        )
        .await;
        assert!(
            matches!(scheme, ProxyError::InvalidUrlFormat { .. }),
            "{scheme:?}"
        );

        assert!(resolver.calls().is_empty());
    }

    #[tokio::test]
    async fn el_verbo_no_soportado_se_rechaza_sin_gastar_resolucion() {
        let resolver = RecordingResolver::new();
        let req = request(
            "https://api.example.com/v1/ping",
            "TRACE",
            &["api.example.com"],
        );

        let error = fetch_with(&resolver, &req).await;

        match error {
            ProxyError::InvalidUrlFormat { reason, .. } => {
                assert!(reason.contains("TRACE"), "razón poco clara: {reason}");
            }
            other => panic!("variante inesperada: {other:?}"),
        }
        assert!(
            resolver.calls().is_empty(),
            "no se debe resolver ningún nombre"
        );
    }

    /// El caso de `docs/BDD.md` Feature 6: un nombre que sí está en la whitelist pero que resuelve
    /// a una red interna. La IP se valida **después** de la whitelist y **antes** de conectar, así
    /// que el error es el de SSRF del resolver y no un fallo de conexión.
    #[tokio::test]
    async fn un_host_autorizado_que_resuelve_a_red_interna_se_bloquea_al_resolver() {
        let resolver = RecordingResolver::new();
        let req = request(
            "https://internal-meta.example.com/token",
            "GET",
            &["internal-meta.example.com"],
        );

        let error = fetch_with(&resolver, &req).await;

        assert!(matches!(error, ProxyError::SsrfBlocked { .. }), "{error:?}");
        assert_eq!(
            resolver.calls(),
            vec!["internal-meta.example.com".to_string()]
        );
    }
}
