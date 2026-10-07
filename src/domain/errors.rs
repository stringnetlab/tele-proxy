use serde::Serialize;
use thiserror::Error;
use tracing::Level;

#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("crypt_id inválido: {reason}")]
    InvalidCryptId { reason: String },

    #[error("Dominio no permitido: {domain}")]
    DomainNotWhitelisted { domain: String },

    #[error("Límite de peticiones excedido")]
    RateLimitExceeded {
        current_count: u32,
        max_requests: u32,
        retry_after_secs: u32,
    },

    #[error("Bloqueo anti-SSRF: {reason}")]
    SsrfBlocked {
        url: String,
        resolved_ip: String,
        reason: String,
    },

    #[error("Formato de URL inválido: {reason}")]
    InvalidUrlFormat { url: String, reason: String },

    #[error("Configuración inválida: {field} {reason}")]
    InvalidConfig { field: String, reason: String },

    #[error("Falló la resolución DNS de {hostname}: {reason}")]
    DnsResolutionFailed { hostname: String, reason: String },

    #[error("Violación del sandbox Lua: intento de llamar a {attempted_function}")]
    LuaSandboxViolation { attempted_function: String },

    #[error("ReDoS bloqueado: pattern={pattern}, transcurrido={elapsed_ms}ms")]
    ReDosBlocked { pattern: String, elapsed_ms: u64 },

    #[error("Falló la verificación de integridad de {resource_type}")]
    IntegrityCheckFailed {
        resource_type: String,
        expected_hash: String,
        actual_hash: String,
    },

    #[error("Cuerpo demasiado grande: {content_length} bytes (máx: {max_allowed})")]
    PayloadTooLarge {
        content_length: u64,
        max_allowed: u64,
    },

    #[error("Error del origen: status={upstream_status}, motivo={reason}")]
    UpstreamError {
        url: String,
        upstream_status: u16,
        reason: String,
    },

    #[error("Timeout del origen: {timeout_ms}ms (url={url})")]
    UpstreamTimeout { url: String, timeout_ms: u64 },

    #[error("Timeout del script: {elapsed_ms}ms (máx: {timeout_ms}ms)")]
    ScriptTimeout { timeout_ms: u64, elapsed_ms: u64 },

    #[error("Límite de memoria del script excedido: {used_mb}MB (máx: {memory_limit_mb}MB)")]
    ScriptMemoryLimit { memory_limit_mb: u32, used_mb: u32 },

    #[error("Timeout del webhook: url={webhook_url}, timeout={timeout_ms}ms")]
    WebhookTimeout {
        webhook_url: String,
        timeout_ms: u64,
    },

    #[error("El webhook falló: {reason}")]
    WebhookFailed { url: String, reason: String },

    #[error("No autorizado: {reason}")]
    Unauthorized { reason: String },

    #[error("Configuración no encontrada para el cliente: {internal_id}")]
    ConfigNotFound { internal_id: String },

    #[error("Error interno: {reason}")]
    Internal { reason: String },
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Solo en `MODO=desarrollo`: campos estructurados del error (field/reason del dominio,
    /// resolved_ip, retry_after_secs, ...). En producción nunca se serializa.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<std::collections::HashMap<&'static str, String>>,
}

impl ProxyError {
    pub fn to_http_status(&self) -> u16 {
        match self {
            Self::InvalidCryptId { .. } => 404,
            Self::DomainNotWhitelisted { .. } => 403,
            Self::RateLimitExceeded { .. } => 429,
            Self::SsrfBlocked { .. } => 403,
            Self::InvalidUrlFormat { .. } => 400,
            Self::InvalidConfig { .. } => 400,
            Self::DnsResolutionFailed { .. } => 502,
            Self::LuaSandboxViolation { .. } => 500,
            Self::ReDosBlocked { .. } => 500,
            Self::IntegrityCheckFailed { .. } => 500,
            Self::PayloadTooLarge { .. } => 413,
            Self::UpstreamError { .. } => 502,
            Self::UpstreamTimeout { .. } => 504,
            Self::ScriptTimeout { .. } => 500,
            Self::ScriptMemoryLimit { .. } => 500,
            Self::WebhookTimeout { .. } => 500,
            Self::WebhookFailed { .. } => 502,
            Self::Unauthorized { .. } => 401,
            Self::ConfigNotFound { .. } => 404,
            Self::Internal { .. } => 500,
        }
    }

    pub fn to_error_code(&self) -> &'static str {
        match self {
            Self::InvalidCryptId { .. } => "invalid_crypt_id",
            Self::DomainNotWhitelisted { .. } => "domain_not_whitelisted",
            Self::RateLimitExceeded { .. } => "rate_limit_exceeded",
            Self::SsrfBlocked { .. } => "ssrf_blocked",
            Self::InvalidUrlFormat { .. } => "invalid_url_format",
            Self::InvalidConfig { .. } => "invalid_config",
            Self::DnsResolutionFailed { .. } => "dns_resolution_failed",
            Self::LuaSandboxViolation { .. } => "lua_sandbox_violation",
            Self::ReDosBlocked { .. } => "redos_blocked",
            Self::IntegrityCheckFailed { .. } => "integrity_check_failed",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::UpstreamError { .. } => "upstream_error",
            Self::UpstreamTimeout { .. } => "upstream_timeout",
            Self::ScriptTimeout { .. } => "script_timeout",
            Self::ScriptMemoryLimit { .. } => "script_memory_limit",
            Self::WebhookTimeout { .. } => "webhook_timeout",
            Self::WebhookFailed { .. } => "webhook_failed",
            Self::Unauthorized { .. } => "unauthorized",
            Self::ConfigNotFound { .. } => "config_not_found",
            Self::Internal { .. } => "internal_error",
        }
    }

    /// Nivel por error: los límites esperables y las degradaciones son WARN; la seguridad y la
    /// infraestructura son ERROR. No se deduce del rango del status.
    pub fn log_level(&self) -> Level {
        match self {
            Self::DomainNotWhitelisted { .. }
            | Self::RateLimitExceeded { .. }
            | Self::InvalidUrlFormat { .. }
            | Self::InvalidConfig { .. }
            | Self::PayloadTooLarge { .. }
            | Self::ScriptTimeout { .. }
            | Self::ScriptMemoryLimit { .. }
            | Self::WebhookTimeout { .. }
            | Self::WebhookFailed { .. }
            | Self::UpstreamTimeout { .. }
            | Self::Unauthorized { .. } => Level::WARN,

            Self::InvalidCryptId { .. }
            | Self::SsrfBlocked { .. }
            | Self::DnsResolutionFailed { .. }
            | Self::LuaSandboxViolation { .. }
            | Self::ReDosBlocked { .. }
            | Self::IntegrityCheckFailed { .. }
            | Self::UpstreamError { .. }
            | Self::ConfigNotFound { .. }
            | Self::Internal { .. } => Level::ERROR,
        }
    }

    /// Mensaje escueto para el cuerpo de error en producción (los 5xx no filtran el motivo
    /// interno, docs/ERROR_DICTIONARY.md § Camino Axum). Los fallos del **origen** se
    /// distinguen de los fallos del propio proxy: `upstream_error`/`dns_resolution_failed`
    /// → "El origen no responde", `upstream_timeout` → "El origen no responde a tiempo". En
    /// `MODO=desarrollo` el cuerpo lleva el mensaje completo (`ApiError` con `verbose`).
    pub fn escueto_message(&self) -> &'static str {
        match self {
            Self::UpstreamError { .. } | Self::DnsResolutionFailed { .. } => {
                "El origen no responde"
            }
            Self::UpstreamTimeout { .. } => "El origen no responde a tiempo",
            _ => "Error interno del servidor",
        }
    }

    /// `Retry-After` en segundos; `None` para todo error que no sea 429.
    pub fn retry_after_secs(&self) -> Option<u32> {
        match self {
            Self::RateLimitExceeded {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        }
    }

    /// Campos del diccionario que la variante sabe por sí misma. El span aporta `crypt_id`,
    /// `internal_id`, `config_version` e `ip_address`.
    pub fn error_fields(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidCryptId { reason } => vec![("reason", reason.clone())],
            Self::DomainNotWhitelisted { domain } => vec![("domain", domain.clone())],
            Self::RateLimitExceeded {
                current_count,
                max_requests,
                retry_after_secs,
            } => vec![
                ("current_count", current_count.to_string()),
                ("max_requests", max_requests.to_string()),
                ("retry_after_secs", retry_after_secs.to_string()),
            ],
            Self::SsrfBlocked {
                url,
                resolved_ip,
                reason,
            } => vec![
                ("url", url.clone()),
                ("resolved_ip", resolved_ip.clone()),
                ("reason", reason.clone()),
            ],
            Self::InvalidUrlFormat { url, reason } => {
                vec![("url", url.clone()), ("reason", reason.clone())]
            }
            Self::InvalidConfig { field, reason } => {
                vec![("field", field.clone()), ("reason", reason.clone())]
            }
            Self::DnsResolutionFailed { hostname, reason } => {
                vec![("hostname", hostname.clone()), ("reason", reason.clone())]
            }
            Self::LuaSandboxViolation { attempted_function } => {
                vec![("attempted_function", attempted_function.clone())]
            }
            Self::ReDosBlocked {
                pattern,
                elapsed_ms,
            } => vec![
                ("pattern", pattern.clone()),
                ("elapsed_ms", elapsed_ms.to_string()),
            ],
            Self::IntegrityCheckFailed {
                resource_type,
                expected_hash,
                actual_hash,
            } => vec![
                ("resource_type", resource_type.clone()),
                ("expected_hash", expected_hash.clone()),
                ("actual_hash", actual_hash.clone()),
            ],
            Self::PayloadTooLarge {
                content_length,
                max_allowed,
            } => vec![
                ("content_length", content_length.to_string()),
                ("max_allowed", max_allowed.to_string()),
            ],
            Self::UpstreamError {
                url,
                upstream_status,
                reason,
            } => vec![
                ("url", url.clone()),
                ("upstream_status", upstream_status.to_string()),
                ("reason", reason.clone()),
            ],
            Self::UpstreamTimeout { url, timeout_ms } => {
                vec![("url", url.clone()), ("timeout_ms", timeout_ms.to_string())]
            }
            Self::ScriptTimeout {
                timeout_ms,
                elapsed_ms,
            } => vec![
                ("timeout_ms", timeout_ms.to_string()),
                ("elapsed_ms", elapsed_ms.to_string()),
            ],
            Self::ScriptMemoryLimit {
                memory_limit_mb,
                used_mb,
            } => vec![
                ("memory_limit_mb", memory_limit_mb.to_string()),
                ("used_mb", used_mb.to_string()),
            ],
            Self::WebhookTimeout {
                webhook_url,
                timeout_ms,
            } => vec![
                ("webhook_url", webhook_url.clone()),
                ("timeout_ms", timeout_ms.to_string()),
            ],
            Self::WebhookFailed { url, reason } => {
                vec![("url", url.clone()), ("reason", reason.clone())]
            }
            Self::Unauthorized { reason } => vec![("reason", reason.clone())],
            Self::ConfigNotFound { internal_id } => vec![("internal_id", internal_id.clone())],
            Self::Internal { reason } => vec![("reason", reason.clone())],
        }
    }

    /// Fuente del error a efectos de `ErrorSource` de Pingora.
    pub fn is_client_error(&self) -> bool {
        matches!(self.to_http_status(), 400..=499)
    }

    /// URL target cuando la variante la porta; `None` para errores que no tienen una URL
    /// asociada (auth, config, rate limit, etc.). Se incluye en el `ErrorResponse` para que el
    /// cliente sepa qué recurso falló sin tener que parsear el mensaje.
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::SsrfBlocked { url, .. }
            | Self::InvalidUrlFormat { url, .. }
            | Self::UpstreamError { url, .. }
            | Self::UpstreamTimeout { url, .. }
            | Self::WebhookFailed { url, .. } => Some(url.as_str()),
            Self::WebhookTimeout { webhook_url, .. } => Some(webhook_url.as_str()),
            _ => None,
        }
    }
}

/// Nivel por variante; `event` = error_code del diccionario. Único punto que registra un
/// `ProxyError`: quien lo llama ya está dentro del span del request.
pub fn log_domain_error(e: &ProxyError) {
    let campos = e
        .error_fields()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");

    // `tracing::event!` mete el nivel en un `static`, así que rechaza un `Level` dinámico (E0435).
    macro_rules! emitir {
        ($nivel:ident) => {
            tracing::$nivel!(
                event = e.to_error_code(),
                status = e.to_http_status(),
                error_message = %e,
                fields = %campos,
                "error del proxy"
            )
        };
    }

    match e.log_level() {
        Level::TRACE => emitir!(trace),
        Level::DEBUG => emitir!(debug),
        Level::INFO => emitir!(info),
        Level::WARN => emitir!(warn),
        Level::ERROR => emitir!(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nivel_por_variante_no_deriva_del_status() {
        // 403 de SSRF es ERROR; 500 de timeout de script es WARN.
        let ssrf = ProxyError::SsrfBlocked {
            url: "http://169.254.169.254/".to_string(),
            resolved_ip: "169.254.169.254".to_string(),
            reason: "metadata_ip".to_string(),
        };
        assert_eq!(ssrf.log_level(), Level::ERROR);
        assert_eq!(ssrf.to_http_status(), 403);

        let timeout = ProxyError::ScriptTimeout {
            timeout_ms: 200,
            elapsed_ms: 210,
        };
        assert_eq!(timeout.log_level(), Level::WARN);
        assert_eq!(timeout.to_http_status(), 500);

        let crypt_id = ProxyError::InvalidCryptId {
            reason: "not_12_chars".to_string(),
        };
        assert_eq!(crypt_id.log_level(), Level::ERROR);
    }

    #[test]
    fn retry_after_solo_en_rate_limit() {
        let e = ProxyError::RateLimitExceeded {
            current_count: 51,
            max_requests: 50,
            retry_after_secs: 60,
        };
        assert_eq!(e.retry_after_secs(), Some(60));
        assert_eq!(e.to_http_status(), 429);

        assert_eq!(
            ProxyError::Internal {
                reason: "x".to_string()
            }
            .retry_after_secs(),
            None
        );
    }

    #[test]
    fn error_fields_contiene_el_retry_after() {
        let e = ProxyError::RateLimitExceeded {
            current_count: 51,
            max_requests: 50,
            retry_after_secs: 60,
        };
        let fields = e.error_fields();
        assert!(fields
            .iter()
            .any(|(k, v)| *k == "retry_after_secs" && v == "60"));
        assert!(fields
            .iter()
            .any(|(k, v)| *k == "current_count" && v == "51"));
    }

    #[test]
    fn is_client_error_por_rango_de_status() {
        assert!(ProxyError::Unauthorized {
            reason: "missing_token".to_string()
        }
        .is_client_error());
        assert!(!ProxyError::UpstreamError {
            url: "https://example.com".to_string(),
            upstream_status: 503,
            reason: "service unavailable".to_string(),
        }
        .is_client_error());
    }

    #[test]
    fn error_code_es_estable() {
        assert_eq!(
            ProxyError::DomainNotWhitelisted {
                domain: "evil.com".to_string()
            }
            .to_error_code(),
            "domain_not_whitelisted"
        );
    }

    #[test]
    fn upstream_timeout_es_504_warn_y_escueto() {
        let e = ProxyError::UpstreamTimeout {
            url: "https://lento.example/x.jpg".to_string(),
            timeout_ms: 30_000,
        };
        assert_eq!(e.to_http_status(), 504);
        assert_eq!(e.to_error_code(), "upstream_timeout");
        assert_eq!(e.log_level(), Level::WARN);
        assert_eq!(e.escueto_message(), "El origen no responde a tiempo");
        assert_eq!(e.url(), Some("https://lento.example/x.jpg"));
    }

    #[test]
    fn escueto_message_distingue_fallo_de_origen_del_propio_proxy() {
        assert_eq!(
            ProxyError::UpstreamError {
                url: "https://a.example".to_string(),
                upstream_status: 503,
                reason: "connect refused".to_string(),
            }
            .escueto_message(),
            "El origen no responde"
        );
        assert_eq!(
            ProxyError::DnsResolutionFailed {
                hostname: "a.example".to_string(),
                reason: "nxdomain".to_string(),
            }
            .escueto_message(),
            "El origen no responde"
        );
        assert_eq!(
            ProxyError::Internal {
                reason: "algo interno".to_string(),
            }
            .escueto_message(),
            "Error interno del servidor"
        );
    }
}
