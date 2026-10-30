use serde::{Deserialize, Serialize};

use super::errors::ProxyError;

/// Límites duros de cada campo. Fuera de rango se rechaza la escritura **entera**: un clamp
/// silencioso dejaría al panel y al proxy discrepando sobre el valor efectivo.
pub const UPSTREAM_TIMEOUT_SECS_RANGE: (u64, u64) = (1, 300);
pub const MAX_RESPONSE_BYTES_RANGE: (u64, u64) = (1 << 20, 512 << 20);
pub const CACHEABLE_SIZE_BYTES_RANGE: (u64, u64) = (1 << 10, 512 << 20);
pub const STALE_CACHE_TTL_SECS_RANGE: (u64, u64) = (60, 604_800);
pub const LUA_TIMEOUT_MS_RANGE: (u64, u64) = (1, 60_000);
pub const LUA_MEMORY_LIMIT_MB_RANGE: (u32, u32) = (1, 1_024);
pub const WEBHOOK_TIMEOUT_MS_RANGE: (u64, u64) = (1, 60_000);
/// TTL de la caché de respuestas. El mínimo es 1 porque un `SETEX` con TTL 0 es un error en
/// Valkey, no un "no cachear".
pub const CACHE_TTL_SECS_RANGE: (u64, u64) = (1, 604_800);
pub const UPSTREAM_USER_AGENT_MAX_LEN: usize = 256;

pub const RUNTIME_SETTINGS_DOC_ID: &str = "runtime_settings";
pub const RUNTIME_SETTINGS_DOC_TYPE: &str = "runtime_settings";
pub const RUNTIME_SETTINGS_SCHEMA_VERSION: u32 = 1;

/// Ajustes globales del proxy recargables en caliente. Hasta ahora vivían congelados: unos en el
/// arranque (`AppConfig::from_env` los inyectaba por constructor en objetos que nunca se
/// reconstruyen) y otros como `const` del compilador en `interfaces/proxy_handler.rs`, así que
/// cambiarlos exigía un redeploy. `defaults()` reproduce **exactamente** esos valores históricos,
/// de modo que un despliegue sin el documento en CouchDB se comporta igual que antes.
///
/// Quedan fuera a propósito: los puertos y `HTTP_HOST` (recargarlos exige rebindear listeners),
/// las URLs y credenciales de CouchDB/Valkey (son la infraestructura que sostiene la recarga), el
/// material de identidad (master token, CORS, Google OAuth) y `config/dns_resolvers.json`
/// (política anti-SSRF, embebida con `include_str!`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSettings {
    /// User-Agent por defecto para el origen; vacío = el de reqwest. Nunca pisa el que envíe el
    /// cliente de la petición.
    #[serde(default)]
    pub upstream_user_agent: String,
    #[serde(default = "defaults_upstream_timeout_secs")]
    pub upstream_timeout_secs: u64,
    /// Techo de lectura del cuerpo del origen (`read_body_capped`).
    #[serde(default = "defaults_max_response_bytes")]
    pub max_response_bytes: u64,
    /// Por encima de este tamaño la respuesta no se cachea en Valkey.
    #[serde(default = "defaults_cacheable_size_bytes")]
    pub cacheable_size_bytes: u64,
    #[serde(default = "defaults_stale_cache_ttl_secs")]
    pub stale_cache_ttl_secs: u64,
    /// TTL de la caché de respuestas por familia de MIME (`determine_cache_ttl`).
    #[serde(default = "defaults_cache_ttl_images_secs")]
    pub cache_ttl_images_secs: u64,
    #[serde(default = "defaults_cache_ttl_assets_secs")]
    pub cache_ttl_assets_secs: u64,
    #[serde(default = "defaults_cache_ttl_default_secs")]
    pub cache_ttl_default_secs: u64,
    #[serde(default = "defaults_lua_timeout_ms")]
    pub lua_timeout_ms: u64,
    #[serde(default = "defaults_lua_memory_limit_mb")]
    pub lua_memory_limit_mb: u32,
    #[serde(default = "defaults_webhook_timeout_ms")]
    pub webhook_timeout_ms: u64,
    /// `MODO=desarrollo`: los errores HTTP exponen el motivo interno y `details`. Editable solo
    /// por rol `admin` con entrada de auditoría: activarlo revela información interna.
    #[serde(default)]
    pub verbose_errors: bool,
}

fn defaults_upstream_timeout_secs() -> u64 {
    30
}
fn defaults_max_response_bytes() -> u64 {
    100 * 1024 * 1024
}
fn defaults_cacheable_size_bytes() -> u64 {
    5 * 1024 * 1024
}
fn defaults_stale_cache_ttl_secs() -> u64 {
    86_400
}
fn defaults_cache_ttl_images_secs() -> u64 {
    3_600
}
fn defaults_cache_ttl_assets_secs() -> u64 {
    1_800
}
fn defaults_cache_ttl_default_secs() -> u64 {
    300
}
fn defaults_lua_timeout_ms() -> u64 {
    200
}
fn defaults_lua_memory_limit_mb() -> u32 {
    50
}
fn defaults_webhook_timeout_ms() -> u64 {
    5_000
}

/// Campos opcionales: es a la vez el cuerpo del `PATCH` de la API y la forma tolerante de leer el
/// documento (un campo ausente o añadido en una release futura toma su valor por defecto en vez
/// de romper la deserialización). `Default` = "ningún cambio".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_response_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cacheable_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_cache_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_images_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_assets_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_default_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lua_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lua_memory_limit_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbose_errors: Option<bool>,
}

impl RuntimeSettingsPatch {
    /// Patch explícito con todos los campos: es lo que se persiste, para que el documento diga por
    /// sí solo qué está activo sin depender de los defaults del código.
    pub fn full(settings: &RuntimeSettings) -> Self {
        Self {
            upstream_user_agent: Some(settings.upstream_user_agent.clone()),
            upstream_timeout_secs: Some(settings.upstream_timeout_secs),
            max_response_bytes: Some(settings.max_response_bytes),
            cacheable_size_bytes: Some(settings.cacheable_size_bytes),
            stale_cache_ttl_secs: Some(settings.stale_cache_ttl_secs),
            cache_ttl_images_secs: Some(settings.cache_ttl_images_secs),
            cache_ttl_assets_secs: Some(settings.cache_ttl_assets_secs),
            cache_ttl_default_secs: Some(settings.cache_ttl_default_secs),
            lua_timeout_ms: Some(settings.lua_timeout_ms),
            lua_memory_limit_mb: Some(settings.lua_memory_limit_mb),
            webhook_timeout_ms: Some(settings.webhook_timeout_ms),
            verbose_errors: Some(settings.verbose_errors),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Sobreescritura de `runtime_settings` en CouchDB. El `_rev` se serializa, así que la escritura
/// conserva la concurrencia optimista del repositorio; el histórico de cambios lo guarda el propio
/// CouchDB, por lo que no se duplica aquí un `previous_settings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettingsDocument {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(rename = "type")]
    pub doc_type: String,
    pub schema_version: u32,
    pub updated_at: String,
    pub settings: RuntimeSettingsPatch,
}

impl RuntimeSettingsDocument {
    pub fn new(settings: &RuntimeSettings, rev: Option<String>) -> Self {
        Self {
            id: RUNTIME_SETTINGS_DOC_ID.to_string(),
            rev,
            doc_type: RUNTIME_SETTINGS_DOC_TYPE.to_string(),
            schema_version: RUNTIME_SETTINGS_SCHEMA_VERSION,
            updated_at: chrono::Utc::now().to_rfc3339(),
            settings: RuntimeSettingsPatch::full(settings),
        }
    }

    /// Ajustes efectivos del documento sobre los valores por defecto. Un documento inválido se
    /// propaga como error y el llamador conserva el último valor bueno.
    pub fn resolve(&self) -> Result<RuntimeSettings, ProxyError> {
        RuntimeSettings::defaults().apply_patch(&self.settings)
    }
}

impl RuntimeSettings {
    pub fn defaults() -> Self {
        Self {
            upstream_user_agent: String::new(),
            upstream_timeout_secs: defaults_upstream_timeout_secs(),
            max_response_bytes: defaults_max_response_bytes(),
            cacheable_size_bytes: defaults_cacheable_size_bytes(),
            stale_cache_ttl_secs: defaults_stale_cache_ttl_secs(),
            cache_ttl_images_secs: defaults_cache_ttl_images_secs(),
            cache_ttl_assets_secs: defaults_cache_ttl_assets_secs(),
            cache_ttl_default_secs: defaults_cache_ttl_default_secs(),
            lua_timeout_ms: defaults_lua_timeout_ms(),
            lua_memory_limit_mb: defaults_lua_memory_limit_mb(),
            webhook_timeout_ms: defaults_webhook_timeout_ms(),
            verbose_errors: false,
        }
    }

    /// TTL de la caché de respuestas según la familia de MIME. Reproduce la tabla histórica de
    /// `determine_cache_ttl` (imágenes > assets de texto > resto), ahora ajustable en caliente.
    pub fn cache_ttl_secs(&self, content_type: &str) -> u64 {
        if content_type.starts_with("image/") {
            self.cache_ttl_images_secs
        } else if content_type.starts_with("text/css")
            || content_type.starts_with("application/javascript")
        {
            self.cache_ttl_assets_secs
        } else {
            self.cache_ttl_default_secs
        }
    }

    /// Aplica el patch y valida el resultado **completo**, no campo a campo: es lo único que
    /// permite comprobar invariantes entre campos (`cacheable_size_bytes <= max_response_bytes`).
    pub fn apply_patch(&self, patch: &RuntimeSettingsPatch) -> Result<Self, ProxyError> {
        let mut next = self.clone();

        if let Some(value) = &patch.upstream_user_agent {
            next.upstream_user_agent = value.trim().to_string();
        }
        if let Some(value) = patch.upstream_timeout_secs {
            next.upstream_timeout_secs = value;
        }
        if let Some(value) = patch.max_response_bytes {
            next.max_response_bytes = value;
        }
        if let Some(value) = patch.cacheable_size_bytes {
            next.cacheable_size_bytes = value;
        }
        if let Some(value) = patch.stale_cache_ttl_secs {
            next.stale_cache_ttl_secs = value;
        }
        if let Some(value) = patch.cache_ttl_images_secs {
            next.cache_ttl_images_secs = value;
        }
        if let Some(value) = patch.cache_ttl_assets_secs {
            next.cache_ttl_assets_secs = value;
        }
        if let Some(value) = patch.cache_ttl_default_secs {
            next.cache_ttl_default_secs = value;
        }
        if let Some(value) = patch.lua_timeout_ms {
            next.lua_timeout_ms = value;
        }
        if let Some(value) = patch.lua_memory_limit_mb {
            next.lua_memory_limit_mb = value;
        }
        if let Some(value) = patch.webhook_timeout_ms {
            next.webhook_timeout_ms = value;
        }
        if let Some(value) = patch.verbose_errors {
            next.verbose_errors = value;
        }

        next.validate()?;
        Ok(next)
    }

    pub fn validate(&self) -> Result<(), ProxyError> {
        require_in_range(
            "upstream_timeout_secs",
            self.upstream_timeout_secs,
            UPSTREAM_TIMEOUT_SECS_RANGE,
        )?;
        require_in_range(
            "max_response_bytes",
            self.max_response_bytes,
            MAX_RESPONSE_BYTES_RANGE,
        )?;
        require_in_range(
            "cacheable_size_bytes",
            self.cacheable_size_bytes,
            CACHEABLE_SIZE_BYTES_RANGE,
        )?;
        require_in_range(
            "stale_cache_ttl_secs",
            self.stale_cache_ttl_secs,
            STALE_CACHE_TTL_SECS_RANGE,
        )?;
        require_in_range(
            "cache_ttl_images_secs",
            self.cache_ttl_images_secs,
            CACHE_TTL_SECS_RANGE,
        )?;
        require_in_range(
            "cache_ttl_assets_secs",
            self.cache_ttl_assets_secs,
            CACHE_TTL_SECS_RANGE,
        )?;
        require_in_range(
            "cache_ttl_default_secs",
            self.cache_ttl_default_secs,
            CACHE_TTL_SECS_RANGE,
        )?;
        require_in_range("lua_timeout_ms", self.lua_timeout_ms, LUA_TIMEOUT_MS_RANGE)?;
        require_in_range(
            "lua_memory_limit_mb",
            self.lua_memory_limit_mb,
            LUA_MEMORY_LIMIT_MB_RANGE,
        )?;
        require_in_range(
            "webhook_timeout_ms",
            self.webhook_timeout_ms,
            WEBHOOK_TIMEOUT_MS_RANGE,
        )?;

        // Va a la cabecera `User-Agent` del origen: un salto de línea ahí sería inyección de
        // cabeceras. reqwest la rechazaría, pero el error llegaría tarde y sin campo culpable.
        if self.upstream_user_agent.len() > UPSTREAM_USER_AGENT_MAX_LEN {
            return Err(ProxyError::InvalidConfig {
                field: "upstream_user_agent".to_string(),
                reason: format!("supera los {} caracteres", UPSTREAM_USER_AGENT_MAX_LEN),
            });
        }
        if self
            .upstream_user_agent
            .chars()
            .any(|c| c.is_ascii_control())
        {
            return Err(ProxyError::InvalidConfig {
                field: "upstream_user_agent".to_string(),
                reason: "contiene caracteres de control".to_string(),
            });
        }

        if self.cacheable_size_bytes > self.max_response_bytes {
            return Err(ProxyError::InvalidConfig {
                field: "cacheable_size_bytes".to_string(),
                reason: format!(
                    "no puede superar max_response_bytes ({})",
                    self.max_response_bytes
                ),
            });
        }

        Ok(())
    }
}

fn require_in_range<T>(field: &str, value: T, (min, max): (T, T)) -> Result<(), ProxyError>
where
    T: PartialOrd + Copy + std::fmt::Display,
{
    if value < min || value > max {
        return Err(ProxyError::InvalidConfig {
            field: field.to_string(),
            reason: format!("{} fuera del rango permitido [{}, {}]", value, min, max),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_upstream_timeout(secs: u64) -> RuntimeSettingsPatch {
        RuntimeSettingsPatch {
            upstream_timeout_secs: Some(secs),
            ..Default::default()
        }
    }

    #[test]
    fn los_defaults_reproducen_los_valores_congelados_historicos() {
        // Si esto cambia, cambia el comportamiento de un despliegue sin documento en CouchDB.
        let s = RuntimeSettings::defaults();
        assert_eq!(s.upstream_timeout_secs, 30);
        assert_eq!(s.max_response_bytes, 100 * 1024 * 1024);
        assert_eq!(s.cacheable_size_bytes, 5 * 1024 * 1024);
        assert_eq!(s.stale_cache_ttl_secs, 86_400);
        assert_eq!(s.cache_ttl_images_secs, 3_600);
        assert_eq!(s.cache_ttl_assets_secs, 1_800);
        assert_eq!(s.cache_ttl_default_secs, 300);
        assert_eq!(s.lua_timeout_ms, 200);
        assert_eq!(s.lua_memory_limit_mb, 50);
        assert_eq!(s.webhook_timeout_ms, 5_000);
        assert!(s.upstream_user_agent.is_empty());
        assert!(!s.verbose_errors);
        s.validate().expect("los defaults deben ser válidos");
    }

    #[test]
    fn el_ttl_por_mime_sigue_la_tabla_historica() {
        let s = RuntimeSettings::defaults();
        assert_eq!(s.cache_ttl_secs("image/png"), 3_600);
        assert_eq!(s.cache_ttl_secs("text/css; charset=utf-8"), 1_800);
        assert_eq!(s.cache_ttl_secs("application/javascript"), 1_800);
        assert_eq!(s.cache_ttl_secs("application/json"), 300);
        assert_eq!(s.cache_ttl_secs("application/octet-stream"), 300);
    }

    #[test]
    fn un_patch_vacio_no_cambia_nada() {
        let s = RuntimeSettings::defaults();
        assert_eq!(s.apply_patch(&RuntimeSettingsPatch::default()).unwrap(), s);
        assert!(RuntimeSettingsPatch::default().is_empty());
        assert!(!RuntimeSettingsPatch::full(&s).is_empty());
    }

    #[test]
    fn un_valor_fuera_de_rango_se_rechaza_con_el_campo_culpable() {
        let err = RuntimeSettings::defaults()
            .apply_patch(&patch_upstream_timeout(3_600))
            .expect_err("3600s agotaría las conexiones al origen");
        match err {
            ProxyError::InvalidConfig { field, reason } => {
                assert_eq!(field, "upstream_timeout_secs");
                assert!(
                    reason.contains("3600"),
                    "el motivo cita el valor: {}",
                    reason
                );
                assert!(
                    reason.contains("[1, 300]"),
                    "el motivo cita el rango: {}",
                    reason
                );
            }
            other => panic!("error inesperado: {:?}", other),
        }
    }

    #[test]
    fn el_limite_de_memoria_lua_ahora_tiene_techo() {
        // Antes se leía con `env_var_or` sin cota superior: un valor enorme era OOM por petición.
        let err = RuntimeSettings::defaults()
            .apply_patch(&RuntimeSettingsPatch {
                lua_memory_limit_mb: Some(64_000),
                ..Default::default()
            })
            .expect_err("debe rechazarse");
        assert!(matches!(err, ProxyError::InvalidConfig { .. }));
    }

    #[test]
    fn cacheable_no_puede_superar_el_techo_de_lectura() {
        let err = RuntimeSettings::defaults()
            .apply_patch(&RuntimeSettingsPatch {
                max_response_bytes: Some(1 << 20),
                cacheable_size_bytes: Some(8 << 20),
                ..Default::default()
            })
            .expect_err("invariante entre campos");
        match err {
            ProxyError::InvalidConfig { field, reason } => {
                assert_eq!(field, "cacheable_size_bytes");
                assert!(reason.contains("max_response_bytes"));
            }
            other => panic!("error inesperado: {:?}", other),
        }
    }

    #[test]
    fn un_user_agent_con_saltos_de_linea_se_rechaza() {
        let err = RuntimeSettings::defaults()
            .apply_patch(&RuntimeSettingsPatch {
                upstream_user_agent: Some("bot/1.0\r\nX-Inject: 1".to_string()),
                ..Default::default()
            })
            .expect_err("inyección de cabeceras");
        match err {
            ProxyError::InvalidConfig { field, reason } => {
                assert_eq!(field, "upstream_user_agent");
                assert!(reason.contains("control"));
            }
            other => panic!("error inesperado: {:?}", other),
        }
    }

    #[test]
    fn el_documento_resuelve_los_campos_ausentes_a_defaults() {
        let json = r#"{
            "_id": "runtime_settings",
            "_rev": "2-abc",
            "type": "runtime_settings",
            "schema_version": 1,
            "updated_at": "2026-10-30T00:00:00Z",
            "settings": { "lua_timeout_ms": 900 }
        }"#;
        let doc: RuntimeSettingsDocument = serde_json::from_str(json).unwrap();
        let resolved = doc.resolve().expect("documento válido");
        assert_eq!(resolved.lua_timeout_ms, 900);
        assert_eq!(resolved.upstream_timeout_secs, 30);
        assert_eq!(doc.rev.as_deref(), Some("2-abc"));
    }

    #[test]
    fn el_documento_se_persiste_con_todos_los_campos_explicitos() {
        let doc = RuntimeSettingsDocument::new(&RuntimeSettings::defaults(), None);
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["settings"]["upstream_timeout_secs"], 30);
        assert_eq!(json["type"], "runtime_settings");
        assert!(json.get("_rev").is_none());
    }
}
