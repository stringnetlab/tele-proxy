use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::domain::services::{CacheStore, ConfigFetcher, DnsResolver, LuaExecutor};

pub struct ProxyService {
    config_fetcher: Arc<dyn ConfigFetcher>,
    cache_store: Arc<dyn CacheStore>,
    dns_resolver: Arc<dyn DnsResolver>,
    lua_executor: Arc<dyn LuaExecutor>,
    degraded_mode: Arc<AtomicBool>,
    verbose_errors: bool,
    /// User-Agent por defecto para las peticiones al origen (env `UPSTREAM_USER_AGENT`).
    /// Se usa solo cuando el cliente de la petición no envía su propio `User-Agent`.
    upstream_user_agent: String,
}

impl ProxyService {
    pub fn new(
        config_fetcher: Arc<dyn ConfigFetcher>,
        cache_store: Arc<dyn CacheStore>,
        dns_resolver: Arc<dyn DnsResolver>,
        lua_executor: Arc<dyn LuaExecutor>,
    ) -> Self {
        Self {
            config_fetcher,
            cache_store,
            dns_resolver,
            lua_executor,
            degraded_mode: Arc::new(AtomicBool::new(false)),
            verbose_errors: false,
            upstream_user_agent: String::new(),
        }
    }

    pub fn with_degraded_mode(self, degraded: bool) -> Self {
        self.degraded_mode.store(degraded, Ordering::Relaxed);
        self
    }

    /// `MODO=desarrollo`: los errores HTTP (`ApiError`) exponen el motivo interno completo y
    /// los campos estructurados en `details`. Cualquier otro modo mantiene los errores escuetos.
    pub fn with_verbose_errors(mut self, verbose: bool) -> Self {
        self.verbose_errors = verbose;
        self
    }

    pub fn verbose_errors(&self) -> bool {
        self.verbose_errors
    }

    /// User-Agent por defecto para el origen (vacío = el de reqwest). Nunca pisa el
    /// `User-Agent` que el cliente envíe: ese se reenvía tal cual.
    pub fn with_upstream_user_agent(mut self, user_agent: String) -> Self {
        self.upstream_user_agent = user_agent;
        self
    }

    pub fn upstream_user_agent(&self) -> &str {
        &self.upstream_user_agent
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
}
