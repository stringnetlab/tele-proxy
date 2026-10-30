use serde::{Deserialize, Serialize};

use crate::domain::models::RateLimitConfig;

/// Cotas por defecto de la sección `preview`. Reproducen los valores de `docs/PLAN_PREVIEW.md`
/// D5/D6; `enabled=false` es el default fail-safe: un documento de CouchDB anterior al campo
/// parsea con preview desactivado y no cambia de comportamiento.
pub const DEFAULT_PREVIEW_TTL_SECONDS: u64 = 86_400;
pub const DEFAULT_PREVIEW_TTL_MAX_SECONDS: u64 = 2_678_400; // 31 días
pub const DEFAULT_PREVIEW_MAX_RENDER_TIMEOUT_MS: u64 = 45_000;
/// Tope de cuerpo a parsear (2 MB). link-preview-js no limita y es su bug; aquí sí.
pub const DEFAULT_PREVIEW_MAX_BODY_BYTES: u64 = 2_097_152;
pub const DEFAULT_PREVIEW_RATE_LIMIT_REQUESTS: u32 = 60;
pub const DEFAULT_PREVIEW_RATE_LIMIT_WINDOW_SECONDS: u64 = 60;

/// Sección `preview` de `ClientConfig` (D6). `#[serde(default)]` a nivel de campo y de struct:
/// los documentos viejos siguen parseando y el preview queda desactivado por defecto.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PreviewConfig {
    /// Interruptor maestro. `false` (default) → cualquier `/mt/` o `/pv/` devuelve 403
    /// `preview_disabled`.
    pub enabled: bool,
    pub capabilities: PreviewCapabilities,
    /// Bucket propio, más estricto que el de `/aq/` porque el fetch y sobre todo el render son
    /// caros. Se aplica por capacidad (`rlp:{crypt_id}:{cap}`).
    pub rate_limit: RateLimitConfig,
    pub ttl_default_seconds: u64,
    pub ttl_max_seconds: u64,
    /// Techo del deadline de render del sidecar; el `timeout` de la petición lo acota.
    pub max_render_timeout_ms: u64,
    /// Cuerpo máximo que se parsea para metadatos/contenido.
    pub max_body_bytes: u64,
    pub screenshot: ScreenshotConfig,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capabilities: PreviewCapabilities::default(),
            rate_limit: RateLimitConfig {
                max_requests: DEFAULT_PREVIEW_RATE_LIMIT_REQUESTS,
                window_seconds: DEFAULT_PREVIEW_RATE_LIMIT_WINDOW_SECONDS,
            },
            ttl_default_seconds: DEFAULT_PREVIEW_TTL_SECONDS,
            ttl_max_seconds: DEFAULT_PREVIEW_TTL_MAX_SECONDS,
            max_render_timeout_ms: DEFAULT_PREVIEW_MAX_RENDER_TIMEOUT_MS,
            max_body_bytes: DEFAULT_PREVIEW_MAX_BODY_BYTES,
            screenshot: ScreenshotConfig::default(),
        }
    }
}

/// Capacidades activas por cliente. `meta` y `content` son baratas (fetch + parseo puro) y van
/// activadas por defecto; `screenshot` y `pdf` necesitan el sidecar de render y van apagadas
/// hasta que el operador las enciende explícitamente.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PreviewCapabilities {
    pub meta: bool,
    pub content: bool,
    pub screenshot: bool,
    pub pdf: bool,
}

impl Default for PreviewCapabilities {
    fn default() -> Self {
        Self {
            meta: true,
            content: true,
            screenshot: false,
            pdf: false,
        }
    }
}

/// Límites del render de screenshot (D6). `default_viewport` es `[ancho, alto]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScreenshotConfig {
    pub max_full_page: bool,
    pub default_viewport: [u32; 2],
}

impl Default for ScreenshotConfig {
    fn default() -> Self {
        Self {
            max_full_page: true,
            default_viewport: [1920, 1080],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_desactivado_por_defecto() {
        let config = PreviewConfig::default();
        assert!(!config.enabled, "fail-safe: enabled debe nacer en false");
        assert!(config.capabilities.meta);
        assert!(!config.capabilities.screenshot);
    }

    #[test]
    fn documento_viejo_sin_seccion_preview_parsea_desactivado() {
        // Un JSON sin el campo `preview` (documento anterior a la feature) deserializa al
        // default gracias a `#[serde(default)]` del campo en `ClientConfig`.
        let json = r#"{ "enabled": true }"#;
        let wrapper: PreviewWrapper = serde_json::from_str(json).expect("parsea");
        assert!(!wrapper.preview.enabled);
    }

    #[derive(Deserialize)]
    struct PreviewWrapper {
        #[serde(default)]
        preview: PreviewConfig,
        #[allow(dead_code)]
        enabled: bool,
    }

    #[test]
    fn campos_parciales_toman_default() {
        let json = r#"{ "enabled": true, "capabilities": { "pdf": true } }"#;
        let config: PreviewConfig = serde_json::from_str(json).expect("parsea");
        assert!(config.enabled);
        assert!(config.capabilities.pdf);
        assert!(
            config.capabilities.meta,
            "los no mencionados quedan en su default"
        );
        assert_eq!(config.ttl_default_seconds, DEFAULT_PREVIEW_TTL_SECONDS);
        assert_eq!(config.screenshot.default_viewport, [1920, 1080]);
    }
}
