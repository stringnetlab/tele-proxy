use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use redis::AsyncCommands;

use crate::domain::errors::ProxyError;
use crate::domain::models::{CachedResponse, RateLimitDecision};
use crate::domain::services::CacheStore;
use crate::infrastructure::local_rate_limiter::LocalRateLimiter;

/// `INCR` + `PEXPIRE` en un solo comando: separados, un proceso que muera entre los dos deja la
/// clave sin TTL y el contador queda envenenado para siempre (`docs/spec.md` Fase 3).
/// Devuelve `{contador, PTTL}` para que `Retry-After` salga del valor real y no de un supuesto.
const RATE_LIMIT_SCRIPT: &str = r#"
local count = redis.call('INCR', KEYS[1])
if count == 1 then
  redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[1]))
end
return {count, redis.call('PTTL', KEYS[1])}
"#;

pub struct ValkeyCacheStore {
    conn: Option<redis::aio::ConnectionManager>,
    local: LocalRateLimiter,
}

impl ValkeyCacheStore {
    pub async fn new(url: &str) -> Result<Self, ProxyError> {
        let client = redis::Client::open(url).map_err(|e| ProxyError::Internal {
            reason: format!("Invalid Valkey URL: {}", e),
        })?;

        let conn = match redis::aio::ConnectionManager::new(client).await {
            Ok(conn) => Some(conn),
            Err(e) => {
                tracing::warn!(error = %e, "Valkey connection failed at startup, entering degraded cache mode");
                None
            }
        };
        Ok(Self {
            conn,
            local: LocalRateLimiter::new(),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// `None` significa "Valkey no decidió": sin conexión, el `EVAL` falló o la respuesta no
    /// tiene la forma esperada. El llamador cae al limiter local; nunca se permite por omitir.
    async fn distributed_count(&self, key: &str, window_seconds: u64) -> Option<(u32, i64)> {
        let Some(conn) = &self.conn else {
            tracing::warn!(
                event = "rate_limit_degraded",
                "Valkey unavailable, deciding locally"
            );
            return None;
        };

        let mut conn = conn.clone();
        let window_ms = window_seconds.saturating_mul(1000);

        let reply: Vec<i64> = match redis::cmd("EVAL")
            .arg(RATE_LIMIT_SCRIPT)
            .arg(1)
            .arg(key)
            .arg(window_ms)
            .query_async(&mut conn)
            .await
        {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!(error = %e, event = "rate_limit_degraded", "Valkey EVAL failed, deciding locally");
                return None;
            }
        };

        match (reply.first(), reply.get(1)) {
            (Some(count), Some(ttl_ms)) => {
                let count = u32::try_from(*count).unwrap_or(u32::MAX);
                Some((count, *ttl_ms))
            }
            _ => {
                tracing::warn!(
                    event = "rate_limit_degraded",
                    len = reply.len(),
                    "Unexpected Valkey EVAL reply, deciding locally"
                );
                None
            }
        }
    }
}

/// TTL restante en segundos, redondeado hacia arriba y nunca 0. Un `PTTL` negativo (clave ya
/// expirada entre el `INCR` y el `PTTL`, o sin TTL) se resuelve con la ventana completa, que es
/// lo que tardaría en reabrirse.
fn retry_after_secs(ttl_ms: i64, window_seconds: u64) -> u32 {
    let ms = if ttl_ms > 0 {
        ttl_ms as u64
    } else {
        Duration::from_secs(window_seconds).as_millis() as u64
    };
    let secs = ms.div_ceil(1_000).clamp(1, u32::MAX as u64);
    secs as u32
}

#[async_trait]
impl CacheStore for ValkeyCacheStore {
    async fn get_response(&self, key: &str) -> Result<Option<CachedResponse>, ProxyError> {
        let Some(conn) = &self.conn else {
            return Ok(None);
        };

        let mut conn = conn.clone();

        let data: Option<Vec<u8>> = match conn.get(key).await {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "Valkey GET failed, skipping cache");
                return Ok(None);
            }
        };

        match data {
            Some(bytes) => {
                // Decode errors are treated as a cache miss so stale entries written by a
                // previous serializer (or a different format) never fail the request path.
                match postcard::from_bytes::<CachedResponse>(&bytes) {
                    Ok(response) => Ok(Some(response)),
                    Err(e) => {
                        tracing::warn!(error = %e, key = %key, "Failed to decode cached response, treating as miss");
                        Ok(None)
                    }
                }
            }
            None => Ok(None),
        }
    }

    async fn set_response(
        &self,
        key: &str,
        response: &CachedResponse,
        ttl_seconds: u64,
    ) -> Result<(), ProxyError> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };

        let mut conn = conn.clone();

        let bytes = postcard::to_allocvec(response).map_err(|e| ProxyError::Internal {
            reason: format!("Failed to serialize response for cache: {}", e),
        })?;

        let result = if ttl_seconds == 0 {
            conn.set::<_, _, ()>(key, bytes).await
        } else {
            conn.set_ex::<_, _, ()>(key, bytes, ttl_seconds).await
        };

        if let Err(e) = result {
            tracing::warn!(error = %e, key = %key, "Valkey SET failed, skipping cache store");
        }

        Ok(())
    }

    async fn get_ip(&self, hostname: &str) -> Result<Option<IpAddr>, ProxyError> {
        let Some(conn) = &self.conn else {
            return Ok(None);
        };

        let mut conn = conn.clone();
        let key = format!("ip_cache:{}", hostname);

        let data: Option<String> = match conn.get(&key).await {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(error = %e, hostname = %hostname, "Valkey GET ip failed");
                return Ok(None);
            }
        };

        match data {
            Some(ip_str) => {
                let ip: IpAddr = ip_str.parse().map_err(|e| ProxyError::Internal {
                    reason: format!("Invalid cached IP: {}", e),
                })?;
                Ok(Some(ip))
            }
            None => Ok(None),
        }
    }

    async fn set_ip(
        &self,
        hostname: &str,
        ip: &IpAddr,
        ttl_seconds: u64,
    ) -> Result<(), ProxyError> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };

        let mut conn = conn.clone();
        let key = format!("ip_cache:{}", hostname);

        if let Err(e) = conn
            .set_ex::<_, _, ()>(&key, ip.to_string(), ttl_seconds)
            .await
        {
            tracing::warn!(error = %e, hostname = %hostname, "Valkey SET ip failed");
        }

        Ok(())
    }

    async fn check_rate_limit(
        &self,
        crypt_id: &str,
        max_requests: u32,
        window_seconds: u64,
    ) -> RateLimitDecision {
        let key = format!("rl:{}", crypt_id);
        match self.distributed_count(&key, window_seconds).await {
            Some((current_count, ttl_ms)) => RateLimitDecision {
                allowed: current_count <= max_requests,
                current_count,
                retry_after_secs: retry_after_secs(ttl_ms, window_seconds),
                degraded: false,
            },
            None => self.local.check(crypt_id, max_requests, window_seconds),
        }
    }
}
