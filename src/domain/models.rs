use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::domain::header_rules::HeaderRule;

/// Naturaleza de un documento `client_config`. Un `Admin` identifica a un operador del panel de
/// administración: **nunca** puede usar el proxy ni la API de cliente (la separación se fuerza en
/// `interfaces/`, no solo en el modelo). `serde(default)` mantiene compatibles los documentos de
/// CouchDB anteriores al campo: todo cliente existente parsea como `Client`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClientKind {
    #[default]
    Client,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub r#type: String,
    pub internal_id: String,
    pub crypt_id: String,
    pub bearer_token_hash: String,
    pub config_version: u64,
    #[serde(default)]
    pub kind: ClientKind,
    /// Cliente especial de confianza: salta la capa de whitelist de dominios y puede salir a
    /// CUALQUIER dominio. El anti-SSRF de IP (IP privadas, pinning DNS, resolución validada)
    /// **sigue aplicando igual** — wildcard solo salta la whitelist. Solo la API admin puede
    /// fijarlo/modificarlo; el `PUT` de cliente ni lo recibe. `serde(default)`: los documentos
    /// de CouchDB anteriores al campo parsean como `false`.
    #[serde(default)]
    pub wildcard: bool,
    pub whitelist: Vec<String>,
    pub rate_limit: RateLimitConfig,
    pub max_scripting_body_bytes: u64,
    pub scripting: ScriptingConfig,
    pub error_handling: ErrorHandlingConfig,
    /// Reglas de modificación de headers de respuesta (Cloudflare Ruleset Engine-like).
    /// `serde(default)` mantiene compatibles los documentos de CouchDB anteriores al campo.
    #[serde(default)]
    pub header_rules: Vec<HeaderRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub max_requests: u32,
    pub window_seconds: u64,
}

/// Resultado de aplicar el rate limit. `current_count` y `retry_after_secs` salen del
/// contador real (no de `max_requests + 1`) para que `Retry-After` y el log digan la verdad.
/// `degraded` indica que decidió el limiter local porque Valkey no respondió: el límite se
/// sigue aplicando, pero por proceso, y el cliente debe recibir `X-Degraded-Mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitDecision {
    pub allowed: bool,
    pub current_count: u32,
    pub retry_after_secs: u32,
    pub degraded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptingConfig {
    pub enabled: bool,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub code_hash: String,
    /// Expresión del motor de reglas (`header_rules`): el script solo se ejecuta cuando la
    /// respuesta la cumple. Vacía = siempre (compatibilidad con configs anteriores).
    #[serde(default)]
    pub expression: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorHandlingConfig {
    pub mode: ErrorMode,
    #[serde(default)]
    pub fallback_urls: HashMap<String, FallbackResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorMode {
    Transparent,
    Wrapped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackResource {
    pub url: String,
    pub hash: String,
}

#[derive(Debug, Clone)]
pub struct ProxyContext {
    pub crypt_id: String,
    pub internal_id: String,
    pub config_version: u64,
    pub target_url: url::Url,
    pub mime_hint: Option<String>,
    pub config: ClientConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Orden de salida del sandbox Lua (`proxy.http_request`). La `whitelist` viaja dentro de la
/// orden porque el validador tiene que decidir con la config del cliente que disparó el script,
/// no con una lista global del motor de red.
#[derive(Debug, Clone)]
pub struct WebhookRequest {
    pub url: String,
    pub method: String,
    pub body: Option<Vec<u8>>,
    pub timeout_ms: u64,
    pub whitelist: Vec<String>,
    /// Heredado de `ClientConfig.wildcard`: si es true, el webhook del sandbox salta la
    /// whitelist de dominios (el anti-SSRF de IP sigue aplicando, igual que en el proxy).
    pub wildcard: bool,
}

#[derive(Debug, Clone)]
pub struct WebhookResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfigResponse {
    pub crypt_id: String,
    pub config_version: u64,
    pub whitelist: Vec<String>,
    pub rate_limit: RateLimitConfig,
    pub max_scripting_body_bytes: u64,
    pub scripting: ScriptingConfigResponse,
    pub error_handling: ErrorHandlingConfig,
    #[serde(default)]
    pub header_rules: Vec<HeaderRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptingConfigResponse {
    pub enabled: bool,
    pub code_hash: String,
}

impl ClientConfig {
    pub fn default_degraded() -> Self {
        Self {
            id: "default_degraded".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "default_degraded".to_string(),
            crypt_id: "default".to_string(),
            bearer_token_hash: String::new(),
            config_version: 0,
            kind: ClientKind::Client,
            wildcard: false,
            whitelist: vec![],
            rate_limit: RateLimitConfig {
                max_requests: 3,
                window_seconds: 60,
            },
            max_scripting_body_bytes: 0,
            scripting: ScriptingConfig {
                enabled: false,
                code: String::new(),
                code_hash: String::new(),
                expression: String::new(),
            },
            error_handling: ErrorHandlingConfig {
                mode: ErrorMode::Transparent,
                fallback_urls: HashMap::new(),
            },
            header_rules: Vec::new(),
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.internal_id == "default_degraded"
    }
}

impl From<ClientConfig> for ClientConfigResponse {
    fn from(config: ClientConfig) -> Self {
        Self {
            crypt_id: config.crypt_id,
            config_version: config.config_version,
            whitelist: config.whitelist,
            rate_limit: config.rate_limit,
            max_scripting_body_bytes: config.max_scripting_body_bytes,
            scripting: ScriptingConfigResponse {
                enabled: config.scripting.enabled,
                code_hash: config.scripting.code_hash,
            },
            error_handling: config.error_handling,
            header_rules: config.header_rules,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfigUpdate {
    pub whitelist: Option<Vec<String>>,
    pub rate_limit: Option<RateLimitConfig>,
    pub max_scripting_body_bytes: Option<u64>,
    pub scripting: Option<ScriptingUpdate>,
    pub error_handling: Option<ErrorHandlingConfig>,
    /// Sustituye la lista completa de reglas (como `whitelist`): enviar `[]` las desactiva.
    pub header_rules: Option<Vec<HeaderRule>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptingUpdate {
    pub enabled: Option<bool>,
    pub code: Option<String>,
    pub code_hash: Option<String>,
    pub expression: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotateIdResponse {
    pub new_crypt_id: String,
    pub rotated_at: String,
}

/// Operador autorizado del panel de administración. Documento CouchDB independiente de los
/// `client_config` (`type: "admin_user"`, `_id = admin_user:{email}`): los admins no usan el
/// proxy y los clientes no pueden ser admins; el modelo solo describe, la separación se fuerza
/// en `interfaces/` (403 en la API de cliente y en `/aq/`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminUser {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub r#type: String,
    pub email: String,
    /// Rol de autorización. Hoy solo existe `"admin"`: se guarda como String para poder
    /// añadir roles sin migración de documentos.
    pub role: String,
    #[serde(default = "default_admin_active")]
    pub active: bool,
    /// ISO 8601 (`chrono::Utc::now().to_rfc3339()`), mismo formato que `RotateIdResponse`.
    pub created_at: String,
    pub updated_at: String,
    /// Email del actor que creó el admin, o `"master_token"` cuando fue el token maestro.
    pub created_by: String,
}

fn default_admin_active() -> bool {
    true
}

impl AdminUser {
    pub fn doc_id(email: &str) -> String {
        format!("admin_user:{}", email.to_lowercase())
    }
}

/// Vista pública de un admin (no hay secretos que ocultar, pero se sigue el patrón de
/// `ClientConfigResponse`: snake_case en el alambre, sin campos internos de CouchDB).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminUserResponse {
    pub email: String,
    pub role: String,
    pub active: bool,
    pub created_at: String,
    pub created_by: String,
}

impl From<AdminUser> for AdminUserResponse {
    fn from(admin: AdminUser) -> Self {
        Self {
            email: admin.email,
            role: admin.role,
            active: admin.active,
            created_at: admin.created_at,
            created_by: admin.created_by,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_degraded_is_restrictive() {
        let config = ClientConfig::default_degraded();
        assert!(config.whitelist.is_empty());
        assert_eq!(config.rate_limit.max_requests, 3);
        assert_eq!(config.rate_limit.window_seconds, 60);
        assert!(!config.scripting.enabled);
        assert_eq!(config.error_handling.mode, ErrorMode::Transparent);
        assert_eq!(config.max_scripting_body_bytes, 0);
    }

    #[test]
    fn test_default_degraded_detection() {
        let config = ClientConfig::default_degraded();
        assert!(config.is_degraded());
        assert_eq!(config.internal_id, "default_degraded");
    }

    #[test]
    fn documento_sin_wildcard_parsea_como_false() {
        // Compatibilidad con documentos de CouchDB anteriores al campo: sin `wildcard` en el
        // JSON, el config parsea con `wildcard: false` (no salta la whitelist nadie por sorpresa).
        let json = r#"{
            "_id": "cliente_viejo",
            "type": "client_config",
            "internal_id": "cliente_viejo",
            "crypt_id": "abcdefghijkl",
            "bearer_token_hash": "sha256:aa",
            "config_version": 3,
            "whitelist": ["example.com"],
            "rate_limit": {"max_requests": 50, "window_seconds": 60},
            "max_scripting_body_bytes": 5242880,
            "scripting": {"enabled": false},
            "error_handling": {"mode": "transparent"}
        }"#;
        let config: ClientConfig = serde_json::from_str(json).expect("documento existente");
        assert!(!config.wildcard);
        assert_eq!(config.kind, ClientKind::Client);

        // Y con el campo explícito se respeta.
        let json_wildcard = json.replace("\"config_version\": 3", "\"config_version\": 3, \"wildcard\": true");
        let config: ClientConfig = serde_json::from_str(&json_wildcard).expect("documento wildcard");
        assert!(config.wildcard);
    }

    #[test]
    fn test_normal_config_is_not_degraded() {
        let config = ClientConfig {
            id: "client_123".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "client_123".to_string(),
            crypt_id: "abc123".to_string(),
            bearer_token_hash: "sha256:abc".to_string(),
            config_version: 1,
            kind: ClientKind::Client,
            wildcard: false,
            whitelist: vec!["example.com".to_string()],
            rate_limit: RateLimitConfig {
                max_requests: 50,
                window_seconds: 60,
            },
            max_scripting_body_bytes: 5242880,
            scripting: ScriptingConfig {
                enabled: false,
                code: String::new(),
                code_hash: String::new(),
                expression: String::new(),
            },
            error_handling: ErrorHandlingConfig {
                mode: ErrorMode::Wrapped,
                fallback_urls: HashMap::new(),
            },
            header_rules: Vec::new(),
        };
        assert!(!config.is_degraded());
    }
}
