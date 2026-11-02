use async_trait::async_trait;
use std::net::IpAddr;

use super::errors::ProxyError;
use super::models::{
    AdminUser, CachedResponse, ClientConfig, ClientConfigUpdate, RateLimitDecision, WebhookRequest,
    WebhookResponse,
};
use super::runtime_settings::{RuntimeSettingsDocument, RuntimeSettingsPatch};

#[async_trait]
pub trait ConfigFetcher: Send + Sync {
    async fn get_by_crypt_id(&self, crypt_id: &str) -> Result<ClientConfig, ProxyError>;
    async fn get_by_token_hash(&self, token_hash: &str) -> Result<ClientConfig, ProxyError>;
    async fn update_config(
        &self,
        internal_id: &str,
        update: &ClientConfigUpdate,
    ) -> Result<ClientConfig, ProxyError>;
    async fn rotate_crypt_id(&self, internal_id: &str) -> Result<String, ProxyError>;
    async fn invalidate_cache(&self, internal_id: &str);
}

/// Persistencia del documento global `runtime_settings`. Dos métodos bastan porque el seed del
/// primer arranque es un patch completo sobre los valores por defecto: no hace falta un camino de
/// escritura aparte.
#[async_trait]
pub trait SettingsRepository: Send + Sync {
    /// `Ok(None)` cuando el documento todavía no existe (primer arranque).
    async fn load_settings(&self) -> Result<Option<RuntimeSettingsDocument>, ProxyError>;
    /// Lee el documento, aplica el patch validándolo **contra el estado actual** y escribe con el
    /// `_rev` leído; un 409 se resuelve releyendo una vez, igual que `ensure_design_doc`. Validar
    /// contra el estado actual y no contra el snapshot del llamador es lo que evita que dos PATCH
    /// concurrentes dejen persistido un valor que ninguno de los dos validó.
    async fn patch_settings(
        &self,
        patch: &RuntimeSettingsPatch,
    ) -> Result<RuntimeSettingsDocument, ProxyError>;
}

#[async_trait]
pub trait CacheStore: Send + Sync {
    async fn get_response(&self, key: &str) -> Result<Option<CachedResponse>, ProxyError>;
    async fn set_response(
        &self,
        key: &str,
        response: &CachedResponse,
        ttl_seconds: u64,
    ) -> Result<(), ProxyError>;
    async fn get_ip(&self, hostname: &str) -> Result<Option<IpAddr>, ProxyError>;
    async fn set_ip(&self, hostname: &str, ip: &IpAddr, ttl_seconds: u64)
        -> Result<(), ProxyError>;
    /// Infallible por contrato: si el almacén distribuido no responde decide el limiter local,
    /// así que el límite nunca se omite (`docs/spec.md` Fase 3). Un `Result` aquí invitaría a
    /// un ramo de error inalcanzable en el handler.
    async fn check_rate_limit(
        &self,
        crypt_id: &str,
        max_requests: u32,
        window_seconds: u64,
    ) -> RateLimitDecision;
    /// Borra las respuestas cacheadas de un cliente (`px:{internal_id}:*`, incluidas las copias
    /// `:stale` usadas por el fallback) y devuelve cuántas claves eliminó. Existe porque subir
    /// `config_version` invalida *toda* la caché del cliente como efecto secundario: sin esto, no
    /// hay forma de forzar un re-fetch sin tocar la configuración.
    async fn purge_client_responses(&self, internal_id: &str) -> Result<u64, ProxyError>;

    /// Cuerpo de preview cacheado (JSON ya serializado de `PreviewData`). Los defaults son
    /// no-op/miss para que los stubs de test no tengan que implementarlos; el almacén real
    /// (`ValkeyCacheStore`) los sobrescribe. Un fallo de Valkey degrada a miss, nunca a error.
    async fn get_preview(&self, key: &str) -> Result<Option<Vec<u8>>, ProxyError> {
        let _ = key;
        Ok(None)
    }
    async fn set_preview(
        &self,
        key: &str,
        body: &[u8],
        ttl_seconds: u64,
    ) -> Result<(), ProxyError> {
        let _ = (key, body, ttl_seconds);
        Ok(())
    }
    /// Rate limit **por capacidad** de preview (`rlp:{crypt_id}:{capability}`). El default delega
    /// en `check_rate_limit` (sin aislamiento por capacidad) para los stubs; el almacén real lo
    /// sobrescribe con un bucket propio por capacidad.
    async fn check_preview_rate_limit(
        &self,
        crypt_id: &str,
        capability: &str,
        max_requests: u32,
        window_seconds: u64,
    ) -> RateLimitDecision {
        let _ = capability;
        self.check_rate_limit(crypt_id, max_requests, window_seconds)
            .await
    }
}

#[async_trait]
pub trait DnsResolver: Send + Sync {
    async fn resolve_and_validate(&self, hostname: &str) -> Result<IpAddr, ProxyError>;
}

#[async_trait]
pub trait LuaExecutor: Send + Sync {
    async fn execute(
        &self,
        script: &str,
        body: &[u8],
        context: &super::models::ProxyContext,
    ) -> Result<Vec<u8>, ProxyError>;
}

/// Toda salida de red que no sea el propio proxy (webhooks desde Lua, fetch de `fallback_urls`)
/// pasa por aquí: `docs/spec.md` § Pipeline anti-SSRF compartido prohíbe instanciar un cliente
/// HTTP ad-hoc en `application/` o en el motor de scripting.
#[async_trait]
pub trait WebhookFetcher: Send + Sync {
    async fn fetch(&self, request: &WebhookRequest) -> Result<WebhookResponse, ProxyError>;
}

/// Salida de red para un `error_handling.fallback_urls` configurado. Va por un puerto propio, no
/// por `WebhookFetcher`, porque aquí no hay whitelist de cliente: la URL la fija el operador en el
/// `PUT` y solo tiene que sobrevivir al anti-SSRF (esquema, IP privada literal y pinning DNS).
/// Devuelve un `CachedResponse` con el `content-type` del origen (único header que se sirve) para
/// que el llamador lo valide con hash/magic bytes y lo reutilice como entrada de caché.
#[async_trait]
pub trait FallbackFetcher: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<CachedResponse, ProxyError>;
}

/// Persistencia de la administración global: operadores (`AdminUser`) y gestión CRUD de
/// clientes desde el panel. Los tests usan implementaciones manuales del trait (el proyecto no
/// depende de mockall; véase el patrón de `interfaces/control_api.rs`).
#[async_trait]
pub trait AdminRepository: Send + Sync {
    async fn get_admin(&self, email: &str) -> Result<AdminUser, ProxyError>;
    async fn list_admins(&self) -> Result<Vec<AdminUser>, ProxyError>;
    /// Escribe el documento completo y devuelve el admin con el `_rev` actualizado.
    async fn put_admin(&self, admin: &AdminUser) -> Result<AdminUser, ProxyError>;
    async fn delete_admin(&self, email: &str) -> Result<(), ProxyError>;
    /// Página de clientes `(skip..skip+limit)` contra la vista `list_clients`; el `u64` es
    /// `total_rows` de la vista, para paginar la UI sin conteos extra.
    async fn list_clients(
        &self,
        limit: u64,
        skip: u64,
    ) -> Result<(Vec<ClientConfig>, u64), ProxyError>;
    async fn get_client_by_internal_id(
        &self,
        internal_id: &str,
    ) -> Result<ClientConfig, ProxyError>;
    /// Escribe el documento completo, invalida la caché local bajo los identificadores
    /// anteriores y devuelve el config con el `_rev` actualizado.
    async fn put_client(&self, config: &ClientConfig) -> Result<ClientConfig, ProxyError>;
    async fn delete_client(&self, internal_id: &str) -> Result<(), ProxyError>;
    async fn client_count(&self) -> Result<u64, ProxyError>;
}
