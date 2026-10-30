use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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

const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// El intento de reconexión corre en la ruta de petición, así que va acotado: `ConnectionManager`
/// por defecto no pone timeout de conexión y reintentaría la política interna antes de devolver.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Claves por iteración del `SCAN` de purga: suficientemente grande para no alargar el barrido en
/// clientes con mucha caché, suficientemente pequeño para no bloquear el servidor.
const PURGE_SCAN_COUNT: u32 = 500;

/// Estado del reintento de conexión. `next_attempt` es el instante a partir del cual se permite
/// un nuevo intento; `reconnect` serializa los intentos para que N peticiones concurrentes no
/// abran N conexiones.
struct ReconnectState {
    next_attempt: Instant,
    backoff: Duration,
}

pub struct ValkeyCacheStore {
    client: redis::Client,
    conn: RwLock<Option<redis::aio::ConnectionManager>>,
    reconnect: tokio::sync::Mutex<ReconnectState>,
    /// El mismo flag que lee `ProxyService` para `X-Degraded-Mode`: se limpia al reconectar y se
    /// vuelve a activar si el intento falla, para que el degradado no sea un veredicto de arranque.
    degraded: Arc<AtomicBool>,
    local: LocalRateLimiter,
}

impl ValkeyCacheStore {
    pub async fn new(url: &str, degraded: Arc<AtomicBool>) -> Result<Self, ProxyError> {
        let client = redis::Client::open(url).map_err(|e| ProxyError::Internal {
            reason: format!("URL de Valkey inválida: {}", e),
        })?;

        let store = Self {
            client,
            conn: RwLock::new(None),
            reconnect: tokio::sync::Mutex::new(ReconnectState {
                next_attempt: Instant::now(),
                backoff: RECONNECT_BACKOFF_MIN,
            }),
            degraded,
            local: LocalRateLimiter::new(),
        };

        match store.attempt_connect().await {
            Ok(()) => store.degraded.store(false, Ordering::Relaxed),
            Err(e) => {
                tracing::warn!(error = %e, "Falló la conexión a Valkey en el arranque, entrando en modo de caché degradado");
                store.degraded.store(true, Ordering::Relaxed);
                let mut state = store.reconnect.lock().await;
                state.next_attempt = Instant::now() + state.backoff;
            }
        }

        Ok(store)
    }

    /// Manager vivo, o `None` si Valkey no responde. La ruta rápida es un `clone` del manager sin
    /// lock asincrónico; si no hay conexión, se reintenta con backoff — un fallo en el arranque ya
    /// no condena al proceso a vivir sin caché ni rate limiting distribuido para siempre.
    async fn connection(&self) -> Option<redis::aio::ConnectionManager> {
        if let Some(conn) = self.conn.read().ok().and_then(|guard| guard.clone()) {
            return Some(conn);
        }

        let mut state = self.reconnect.lock().await;

        // Doble chequeo: otra petición pudo reconectar mientras esperábamos este lock.
        if let Some(conn) = self.conn.read().ok().and_then(|guard| guard.clone()) {
            return Some(conn);
        }

        if Instant::now() < state.next_attempt {
            return None;
        }

        match self.attempt_connect().await {
            Ok(()) => {
                tracing::info!("Valkey recuperado, reactivando caché y rate limiting distribuido");
                state.backoff = RECONNECT_BACKOFF_MIN;
                state.next_attempt = Instant::now();
                self.degraded.store(false, Ordering::Relaxed);
                self.conn.read().ok().and_then(|guard| guard.clone())
            }
            Err(e) => {
                tracing::warn!(error = %e, backoff_secs = state.backoff.as_secs(), "Sigue la caída de Valkey, reintentando más tarde");
                state.next_attempt = Instant::now() + state.backoff;
                state.backoff = (state.backoff * 2).min(RECONNECT_BACKOFF_MAX);
                self.degraded.store(true, Ordering::Relaxed);
                None
            }
        }
    }

    async fn attempt_connect(&self) -> Result<(), redis::RedisError> {
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_number_of_retries(1)
            .set_connection_timeout(Some(CONNECT_TIMEOUT));
        let manager =
            redis::aio::ConnectionManager::new_with_config(self.client.clone(), config).await?;
        let mut guard = self
            .conn
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(manager);
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.conn
            .read()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// El flag compartido de degradación, para el listener de control y los tests.
    pub fn degraded_flag(&self) -> &Arc<AtomicBool> {
        &self.degraded
    }

    /// `None` significa "Valkey no decidió": sin conexión, el `EVAL` falló o la respuesta no
    /// tiene la forma esperada. El llamador cae al limiter local; nunca se permite por omitir.
    async fn distributed_count(&self, key: &str, window_seconds: u64) -> Option<(u32, i64)> {
        let Some(mut conn) = self.connection().await else {
            tracing::warn!(
                event = "rate_limit_degraded",
                "Valkey no disponible, decidiendo localmente"
            );
            return None;
        };

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
                tracing::warn!(error = %e, event = "rate_limit_degraded", "EVAL de Valkey falló, decidiendo localmente");
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
                    "Respuesta EVAL de Valkey inesperada, decidiendo localmente"
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

/// Prefijo `glob` de la purga de un cliente. El `internal_id` acaba en el `MATCH` de un `SCAN`:
/// con metacaracteres la purga barrería claves de otros clientes, así que se rechaza antes de
/// tocar Valkey.
fn purge_pattern(internal_id: &str) -> Result<String, ProxyError> {
    if internal_id.is_empty()
        || !internal_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
    {
        return Err(ProxyError::InvalidConfig {
            field: "internal_id".to_string(),
            reason: "El id de cliente no puede usarse como patrón de purga".to_string(),
        });
    }

    Ok(format!("px:{}:*", internal_id))
}

#[async_trait]
impl CacheStore for ValkeyCacheStore {
    async fn get_response(&self, key: &str) -> Result<Option<CachedResponse>, ProxyError> {
        let Some(mut conn) = self.connection().await else {
            return Ok(None);
        };

        let data: Option<Vec<u8>> = match conn.get(key).await {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "GET de Valkey falló, omitiendo la caché");
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
                        tracing::warn!(error = %e, key = %key, "No se pudo decodificar la respuesta en caché, tratando como miss");
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
        let Some(mut conn) = self.connection().await else {
            return Ok(());
        };

        let bytes = postcard::to_allocvec(response).map_err(|e| ProxyError::Internal {
            reason: format!("No se pudo serializar la respuesta para la caché: {}", e),
        })?;

        let result = if ttl_seconds == 0 {
            conn.set::<_, _, ()>(key, bytes).await
        } else {
            conn.set_ex::<_, _, ()>(key, bytes, ttl_seconds).await
        };

        if let Err(e) = result {
            tracing::warn!(error = %e, key = %key, "SET de Valkey falló, omitiendo el guardado en caché");
        }

        Ok(())
    }

    async fn get_ip(&self, hostname: &str) -> Result<Option<IpAddr>, ProxyError> {
        let Some(mut conn) = self.connection().await else {
            return Ok(None);
        };
        let key = format!("ip_cache:{}", hostname);

        let data: Option<String> = match conn.get(&key).await {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(error = %e, hostname = %hostname, "GET ip de Valkey falló");
                return Ok(None);
            }
        };

        match data {
            Some(ip_str) => {
                let ip: IpAddr = ip_str.parse().map_err(|e| ProxyError::Internal {
                    reason: format!("IP cacheada inválida: {}", e),
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
        let Some(mut conn) = self.connection().await else {
            return Ok(());
        };
        let key = format!("ip_cache:{}", hostname);

        if let Err(e) = conn
            .set_ex::<_, _, ()>(&key, ip.to_string(), ttl_seconds)
            .await
        {
            tracing::warn!(error = %e, hostname = %hostname, "SET ip de Valkey falló");
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

    async fn purge_client_responses(&self, internal_id: &str) -> Result<u64, ProxyError> {
        let pattern = purge_pattern(internal_id)?;

        let Some(mut conn) = self.connection().await else {
            return Err(ProxyError::ServiceUnavailable {
                reason: "Valkey no disponible para purgar la caché".to_string(),
            });
        };

        // SCAN en vez de KEYS: KEYS bloquea el servidor el tiempo que tarde el barrido completo.
        let mut cursor: u64 = 0;
        let mut removed: u64 = 0;

        loop {
            let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(PURGE_SCAN_COUNT)
                .query_async(&mut conn)
                .await
                .map_err(|e| ProxyError::Internal {
                    reason: format!("SCAN de purga en Valkey falló: {}", e),
                })?;

            if !keys.is_empty() {
                let deleted: u64 = conn.del(&keys).await.map_err(|e| ProxyError::Internal {
                    reason: format!("DEL de purga en Valkey falló: {}", e),
                })?;
                removed = removed.saturating_add(deleted);
            }

            cursor = next;
            if cursor == 0 {
                return Ok(removed);
            }
        }
    }
}

/// Almacén de sesiones y estados OAuth de la API admin sobre la **misma** conexión viva de Valkey
/// que la caché (`ValkeyCacheStore::connection`), así que se recupera con ella: un Valkey caído en
/// el arranque ya no deja el login admin inutilizable para siempre. Sigue devolviendo error en vez
/// de fingir persistencia (un login que no persiste la sesión es peor que uno que falla en claro).
pub struct ValkeySessionStore {
    cache: Arc<ValkeyCacheStore>,
}

impl ValkeySessionStore {
    pub fn new(cache: Arc<ValkeyCacheStore>) -> Self {
        Self { cache }
    }

    async fn require_conn(&self) -> Result<redis::aio::ConnectionManager, ProxyError> {
        self.cache.connection().await.ok_or_else(|| {
            tracing::warn!(
                event = "session_store_degraded",
                "Valkey no disponible para el almacén de sesiones"
            );
            ProxyError::ServiceUnavailable {
                reason: "Valkey no disponible".to_string(),
            }
        })
    }
}

#[async_trait]
impl crate::application::admin_service::SessionStore for ValkeySessionStore {
    async fn setex(&self, key: &str, value: &str, ttl_seconds: u64) -> Result<(), ProxyError> {
        let mut conn = self.require_conn().await?;
        conn.set_ex::<_, _, ()>(key, value, ttl_seconds)
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("SETEX de sesión en Valkey falló: {}", e),
            })
    }

    async fn get(&self, key: &str) -> Result<Option<String>, ProxyError> {
        let mut conn = self.require_conn().await?;
        conn.get(key).await.map_err(|e| ProxyError::Internal {
            reason: format!("GET de sesión en Valkey falló: {}", e),
        })
    }

    async fn getdel(&self, key: &str) -> Result<Option<String>, ProxyError> {
        let mut conn = self.require_conn().await?;
        redis::cmd("GETDEL")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("GETDEL en Valkey falló: {}", e),
            })
    }

    async fn del(&self, key: &str) -> Result<(), ProxyError> {
        let mut conn = self.require_conn().await?;
        conn.del::<_, ()>(key)
            .await
            .map_err(|e| ProxyError::Internal {
                reason: format!("DEL en Valkey falló: {}", e),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un `internal_id` con metacaracteres de glob ensancharía el `SCAN` y borraría claves de otros
    /// clientes: se rechaza antes de construir el patrón.
    #[test]
    fn el_patron_de_purga_rechaza_ids_con_metacaracteres() {
        assert_eq!(
            purge_pattern("cliente-1").expect("id válido"),
            "px:cliente-1:*"
        );

        for hostile in ["", "*", "a?b", "a[bc]", "a b", "a\\b"] {
            let err = purge_pattern(hostile).expect_err("id hostil para el glob");
            assert!(
                matches!(err, ProxyError::InvalidConfig { .. }),
                "id {hostile:?} debería rechazar el patrón"
            );
        }
    }

    /// Un Valkey inalcanzable en el arranque no puede condenar al proceso: el almacén se construye
    /// igual, marca degradado, y la reconexión se reintenta bajo demanda en cada operación.
    #[tokio::test]
    async fn arranca_degradado_cuando_valkey_no_responde() {
        let degraded = Arc::new(AtomicBool::new(false));
        let store = ValkeyCacheStore::new("redis://127.0.0.1:1", Arc::clone(&degraded))
            .await
            .expect("el almacén se construye aunque Valkey no responda");

        assert!(!store.is_connected());
        assert!(degraded.load(Ordering::Relaxed));
        assert!(store.connection().await.is_none());
    }

    /// La caída sigue siendo tolerante en el request path: la caché reporta miss y el limiter local
    /// decide, avisando de que la cuota es por instancia.
    #[tokio::test]
    async fn sirve_miss_y_cuota_local_sin_valkey() {
        let store = ValkeyCacheStore::new("redis://127.0.0.1:1", Arc::new(AtomicBool::new(false)))
            .await
            .expect("el almacén se construye aunque Valkey no responda");

        assert!(store
            .get_response("px:interno:1:deadbeef")
            .await
            .unwrap()
            .is_none());

        let decision = store.check_rate_limit("crypt12345678", 5, 60).await;
        assert!(decision.allowed);
        assert!(decision.degraded);
    }
}
