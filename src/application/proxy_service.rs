use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::domain::services::{CacheStore, ConfigFetcher, DnsResolver, LuaExecutor};

pub struct ProxyService {
    config_fetcher: Arc<dyn ConfigFetcher>,
    cache_store: Arc<dyn CacheStore>,
    dns_resolver: Arc<dyn DnsResolver>,
    lua_executor: Arc<dyn LuaExecutor>,
    degraded_mode: Arc<AtomicBool>,
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
        }
    }

    pub fn with_degraded_mode(self, degraded: bool) -> Self {
        self.degraded_mode.store(degraded, Ordering::Relaxed);
        self
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
