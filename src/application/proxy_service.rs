use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::domain::errors::ProxyError;
use crate::domain::runtime_settings::RuntimeSettings;
use crate::domain::services::{CacheStore, ConfigFetcher, DnsResolver, LuaExecutor};

use super::settings_store::SettingsStore;

pub struct ProxyService {
    config_fetcher: Arc<dyn ConfigFetcher>,
    cache_store: Arc<dyn CacheStore>,
    dns_resolver: Arc<dyn DnsResolver>,
    lua_executor: Arc<dyn LuaExecutor>,
    degraded_mode: Arc<AtomicBool>,
    settings: Arc<SettingsStore>,
}

impl ProxyService {
    pub fn new(
        config_fetcher: Arc<dyn ConfigFetcher>,
        cache_store: Arc<dyn CacheStore>,
        dns_resolver: Arc<dyn DnsResolver>,
        lua_executor: Arc<dyn LuaExecutor>,
        settings: Arc<SettingsStore>,
    ) -> Self {
        Self {
            config_fetcher,
            cache_store,
            dns_resolver,
            lua_executor,
            degraded_mode: Arc::new(AtomicBool::new(false)),
            settings,
        }
    }

    /// Flag de degradación compartido con el almacén de caché: Valkey puede caerse y volver sin
    /// reiniciar el proceso, y `X-Degraded-Mode` tiene que decir la verdad en ambos sentidos.
    pub fn with_degraded_flag(mut self, degraded: Arc<AtomicBool>) -> Self {
        self.degraded_mode = degraded;
        self
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
