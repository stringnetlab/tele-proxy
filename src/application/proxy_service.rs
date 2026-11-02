use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::domain::errors::ProxyError;
use crate::domain::runtime_settings::RuntimeSettings;
use crate::domain::services::{
    CacheStore, ConfigFetcher, DnsResolver, FallbackFetcher, LuaExecutor,
};
use crate::infrastructure::preview::renderer::BrowserlessRenderer;

use super::settings_store::SettingsStore;

pub struct ProxyService {
    config_fetcher: Arc<dyn ConfigFetcher>,
    cache_store: Arc<dyn CacheStore>,
    dns_resolver: Arc<dyn DnsResolver>,
    lua_executor: Arc<dyn LuaExecutor>,
    /// Salida de red de los `error_handling.fallback_urls`. Se inyecta desde el arranque para que
    /// el pipeline anti-SSRF sea el mismo que el del webhook (mismo resolver validado).
    fallback_fetcher: Arc<dyn FallbackFetcher>,
    degraded_mode: Arc<AtomicBool>,
    settings: Arc<SettingsStore>,
    /// Sidecar de render (browserless). `None` cuando no se configuró `BROWSERLESS_URL`: `/pv/`
    /// degrada a `render_unavailable` en vez de intentar red.
    renderer: Option<Arc<BrowserlessRenderer>>,
}

impl ProxyService {
    pub fn new(
        config_fetcher: Arc<dyn ConfigFetcher>,
        cache_store: Arc<dyn CacheStore>,
        dns_resolver: Arc<dyn DnsResolver>,
        lua_executor: Arc<dyn LuaExecutor>,
        fallback_fetcher: Arc<dyn FallbackFetcher>,
        settings: Arc<SettingsStore>,
    ) -> Self {
        Self {
            config_fetcher,
            cache_store,
            dns_resolver,
            lua_executor,
            fallback_fetcher,
            degraded_mode: Arc::new(AtomicBool::new(false)),
            settings,
            renderer: None,
        }
    }

    /// Flag de degradación compartido con el almacén de caché: Valkey puede caerse y volver sin
    /// reiniciar el proceso, y `X-Degraded-Mode` tiene que decir la verdad en ambos sentidos.
    pub fn with_degraded_flag(mut self, degraded: Arc<AtomicBool>) -> Self {
        self.degraded_mode = degraded;
        self
    }

    /// Engancha el sidecar de render. Se llama en el arranque solo si `BROWSERLESS_URL` está
    /// configurado; sin él, `/pv/` responde `503 render_unavailable` (fail-soft).
    pub fn with_renderer(mut self, renderer: Arc<BrowserlessRenderer>) -> Self {
        self.renderer = Some(renderer);
        self
    }

    pub fn renderer(&self) -> Option<&Arc<BrowserlessRenderer>> {
        self.renderer.as_ref()
    }

    /// Snapshot de los ajustes globales (`MODO` verboso, User-Agent del origen, timeouts y límites
    /// que antes eran `const`). **Se llama una vez por petición** y el resultado se pasa hacia
    /// abajo: leer el store en cada punto de uso permitiría que un cambio a mitad del request
    /// mezclara dos configuraciones dentro de la misma petición.
    pub fn settings(&self) -> Arc<RuntimeSettings> {
        self.settings.snapshot()
    }

    pub fn settings_store(&self) -> &Arc<SettingsStore> {
        &self.settings
    }

    pub fn config_fetcher(&self) -> &Arc<dyn ConfigFetcher> {
        &self.config_fetcher
    }

    pub fn cache_store(&self) -> &Arc<dyn CacheStore> {
        &self.cache_store
    }

    pub fn dns_resolver(&self) -> &Arc<dyn DnsResolver> {
        &self.dns_resolver
    }

    pub fn lua_executor(&self) -> &Arc<dyn LuaExecutor> {
        &self.lua_executor
    }

    pub fn fallback_fetcher(&self) -> &Arc<dyn FallbackFetcher> {
        &self.fallback_fetcher
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded_mode.load(Ordering::Relaxed)
    }

    /// Purga las respuestas cacheadas de un cliente sin tocar su configuración: la vía para forzar
    /// un re-fetch cuando hace falta, sin el efecto colateral de subir `config_version`.
    pub async fn purge_client_responses(&self, internal_id: &str) -> Result<u64, ProxyError> {
        let purged = self.cache_store.purge_client_responses(internal_id).await?;
        tracing::info!(
            event = "cache_purged",
            internal_id = %internal_id,
            purged_keys = purged,
            "Caché de respuestas purgada"
        );
        Ok(purged)
    }
}
