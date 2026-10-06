use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hickory_resolver::config::{
    ConnectionConfig, NameServerConfig, ProtocolConfig, ResolverConfig, ResolverOpts,
};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::TokioResolver;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::domain::errors::ProxyError;
use crate::domain::services::DnsResolver;
use crate::domain::validators::is_private_ip;

#[derive(Debug, Deserialize)]
pub struct DnsResolverConfig {
    pub resolvers: Vec<ResolverEntry>,
    pub settings: DnsSettings,
}

#[derive(Debug, Deserialize)]
pub struct ResolverEntry {
    pub name: String,
    pub priority: u32,
    pub ip: String,
    pub port: u16,
    pub protocol: String,
    pub dnssec: bool,
    /// TLS Server Name Indication (SNI) hostname used for DNS-over-TLS certificate
    /// verification. This is the resolver's real domain (e.g. `dns.quad9.net`), which is
    /// distinct from the human-readable `name` label.
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default)]
    pub ecs: bool,
}

#[derive(Debug, Deserialize)]
pub struct DnsSettings {
    pub timeout_ms: u64,
    pub retries_per_server: u32,
    pub cache_size: usize,
    pub ip_cache_ttl_seconds: u64,
}

pub struct SecureDnsResolver {
    resolvers: Vec<ResolverEntry>,
    timeout: Duration,
    retries: u32,
    ip_cache: Arc<RwLock<lru::LruCache<String, (IpAddr, std::time::Instant)>>>,
    cache_ttl: Duration,
}

impl SecureDnsResolver {
    pub fn new(config: DnsResolverConfig) -> Self {
        let mut resolvers = config.resolvers;
        resolvers.sort_by_key(|r| r.priority);

        let cache_ttl = Duration::from_secs(config.settings.ip_cache_ttl_seconds);

        Self {
            resolvers,
            timeout: Duration::from_millis(config.settings.timeout_ms),
            retries: config.settings.retries_per_server,
            ip_cache: Arc::new(RwLock::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(config.settings.cache_size)
                    .unwrap_or_else(|| std::num::NonZeroUsize::new(256).expect("256 is non-zero")),
            ))),
            cache_ttl,
        }
    }

    pub fn from_json(json: &str) -> Result<Self, ProxyError> {
        let config: DnsResolverConfig =
            serde_json::from_str(json).map_err(|e| ProxyError::Internal {
                reason: format!("Failed to parse DNS config: {}", e),
            })?;
        Ok(Self::new(config))
    }

    async fn get_cached_ip(&self, hostname: &str) -> Option<IpAddr> {
        let mut cache = self.ip_cache.write().await;
        if let Some((ip, timestamp)) = cache.get(hostname) {
            if timestamp.elapsed() < self.cache_ttl {
                return Some(*ip);
            }
            cache.pop(hostname);
        }
        None
    }

    async fn cache_ip(&self, hostname: String, ip: IpAddr) {
        let mut cache = self.ip_cache.write().await;
        cache.put(hostname, (ip, std::time::Instant::now()));
    }

    async fn resolve_with_fallback(&self, hostname: &str) -> Result<IpAddr, ProxyError> {
        let mut last_error = String::new();

        for resolver in &self.resolvers {
            match self.resolve_with_resolver(hostname, resolver).await {
                Ok(ip) => return Ok(ip),
                Err(e) => {
                    last_error = format!("{} failed: {}", resolver.name, e);
                    tracing::warn!(
                        resolver = %resolver.name,
                        hostname = %hostname,
                        error = %e,
                        "DNS resolver failed, trying next"
                    );
                    continue;
                }
            }
        }

        Err(ProxyError::DnsResolutionFailed {
            hostname: hostname.to_string(),
            reason: format!("All resolvers failed. Last error: {}", last_error),
        })
    }

    async fn resolve_with_resolver(
        &self,
        hostname: &str,
        resolver_config: &ResolverEntry,
    ) -> Result<IpAddr, ProxyError> {
        let ns_ip: IpAddr =
            resolver_config
                .ip
                .parse()
                .map_err(|e| ProxyError::DnsResolutionFailed {
                    hostname: hostname.to_string(),
                    reason: format!("Invalid resolver IP {}: {}", resolver_config.ip, e),
                })?;

        let mut opts = ResolverOpts::default();
        opts.timeout = self.timeout;
        opts.attempts = self.retries as usize;
        opts.validate = resolver_config.dnssec;

        let server_name: Arc<str> = Arc::from(
            resolver_config
                .server_name
                .clone()
                .unwrap_or_else(|| resolver_config.name.replace(' ', "-").to_lowercase())
                .as_str(),
        );
        let mut connection = ConnectionConfig::new(ProtocolConfig::Tls { server_name });
        connection.port = resolver_config.port;

        let ns_config = NameServerConfig::new(ns_ip, false, vec![connection]);

        let config = ResolverConfig::from_parts(None, vec![], vec![ns_config]);

        let resolver = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
            .with_options(opts)
            .build()
            .map_err(|e| ProxyError::DnsResolutionFailed {
                hostname: hostname.to_string(),
                reason: format!("Failed to build resolver: {}", e),
            })?;

        let response =
            resolver
                .lookup_ip(hostname)
                .await
                .map_err(|e| ProxyError::DnsResolutionFailed {
                    hostname: hostname.to_string(),
                    reason: format!("Lookup failed: {}", e),
                })?;

        // Deployment networks may lack IPv6 routing, so prefer an IPv4 address when the
        // name has both A and AAAA records; fall back to IPv6 only if no A record exists.
        let chosen_ip = response
            .iter()
            .find(|ip| ip.is_ipv4())
            .or_else(|| response.iter().next());

        if let Some(ip) = chosen_ip {
            if is_private_ip(&ip) {
                tracing::warn!(
                    hostname = %hostname,
                    resolved_ip = %ip,
                    "DNS resolved to private IP, blocking"
                );
                return Err(ProxyError::SsrfBlocked {
                    url: hostname.to_string(),
                    resolved_ip: ip.to_string(),
                    reason: "DNS resolved to private IP".to_string(),
                });
            }
            return Ok(ip);
        }

        Err(ProxyError::DnsResolutionFailed {
            hostname: hostname.to_string(),
            reason: "No IPs returned from resolver".to_string(),
        })
    }
}

#[async_trait]
impl DnsResolver for SecureDnsResolver {
    async fn resolve_and_validate(&self, hostname: &str) -> Result<IpAddr, ProxyError> {
        if let Some(cached_ip) = self.get_cached_ip(hostname).await {
            tracing::debug!(hostname = %hostname, ip = %cached_ip, "DNS cache hit");
            return Ok(cached_ip);
        }

        let ip = self.resolve_with_fallback(hostname).await?;

        self.cache_ip(hostname.to_string(), ip).await;

        tracing::debug!(hostname = %hostname, ip = %ip, "DNS resolved and cached");

        Ok(ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DnsResolverConfig {
        DnsResolverConfig {
            resolvers: vec![ResolverEntry {
                name: "Test DNS".to_string(),
                priority: 1,
                ip: "8.8.8.8".to_string(),
                port: 53,
                protocol: "udp".to_string(),
                dnssec: false,
                server_name: None,
                ecs: false,
            }],
            settings: DnsSettings {
                timeout_ms: 3000,
                retries_per_server: 2,
                cache_size: 256,
                ip_cache_ttl_seconds: 300,
            },
        }
    }

    #[tokio::test]
    async fn test_cache_stores_and_retrieves_ip() {
        let resolver = SecureDnsResolver::new(test_config());

        let hostname = "example.com";
        let ip: IpAddr = "93.184.216.34".parse().unwrap();

        resolver.cache_ip(hostname.to_string(), ip).await;

        let cached = resolver.get_cached_ip(hostname).await;
        assert_eq!(cached, Some(ip));
    }

    #[tokio::test]
    async fn test_cache_returns_none_for_unknown_host() {
        let resolver = SecureDnsResolver::new(test_config());

        let cached = resolver.get_cached_ip("unknown.com").await;
        assert_eq!(cached, None);
    }
}
