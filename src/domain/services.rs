use async_trait::async_trait;
use std::net::IpAddr;

use super::errors::ProxyError;
use super::models::{
    CachedResponse, ClientConfig, ClientConfigUpdate, RateLimitDecision, WebhookRequest,
    WebhookResponse,
};

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
