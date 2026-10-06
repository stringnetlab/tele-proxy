use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lru::LruCache;

use crate::domain::models::RateLimitDecision;

/// Tope de `crypt_id`s seguidos a la vez. Al ser LRU, un cliente que invente identificador
/// aleatorios no puede hacer crecer el proceso sin cota.
const MAX_TRACKED_KEYS: usize = 10_000;

/// Ventana fija en proceso que decide el rate limit **solo** cuando Valkey no responde
/// (`docs/spec.md`, Fase 3: "Nunca se omiten ambos mecanismos"). No es un bypass: aplica los
/// mismos umbrales, con la diferencia de que la cuota es por instancia.
///
/// `pingora_limits::rate::Rate` sería el equivalente en la Fase 3, pero vive tras el feature
/// `proxy` (`Cargo.toml:18`) y ese feature no compila en Windows, así que la red de seguridad
/// tiene que existir sin él.
pub struct LocalRateLimiter {
    windows: Mutex<LruCache<String, Window>>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    count: u32,
    expires_at: Instant,
}

impl Default for LocalRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalRateLimiter {
    pub fn new() -> Self {
        let capacity = NonZeroUsize::new(MAX_TRACKED_KEYS).unwrap_or(NonZeroUsize::MIN);
        Self {
            windows: Mutex::new(LruCache::new(capacity)),
        }
    }

    pub fn check(&self, key: &str, max_requests: u32, window_seconds: u64) -> RateLimitDecision {
        self.check_at(Instant::now(), key, max_requests, window_seconds)
    }

    fn check_at(
        &self,
        now: Instant,
        key: &str,
        max_requests: u32,
        window_seconds: u64,
    ) -> RateLimitDecision {
        // window_seconds = 0 reabriría la ventana en cada request y dejaría el límite sin
        // efecto; 1 s es el mínimo que sigue siendo una restricción real.
        let window = Duration::from_secs(window_seconds.max(1));
        let mut windows = self.lock();

        let current = windows
            .get(key)
            .copied()
            .filter(|existing| existing.expires_at > now);
        let refreshed = match current {
            Some(existing) => Window {
                count: existing.count.saturating_add(1),
                expires_at: existing.expires_at,
            },
            None => Window {
                count: 1,
                expires_at: now + window,
            },
        };
        let _ = windows.put(key.to_string(), refreshed);

        RateLimitDecision {
            allowed: refreshed.count <= max_requests,
            current_count: refreshed.count,
            retry_after_secs: retry_after_secs(refreshed.expires_at, now),
            degraded: true,
        }
    }

    /// `lock().unwrap()` violaría la regla 5 de `docs/ERROR_DICTIONARY.md`. Un panic con el
    /// candado tomado deja intacto el contenido del mapa (aquí solo se mutan contadores), así
    /// que recuperar el guard protegiendo el proxy es preferible a propagar el fallo.
    fn lock(&self) -> MutexGuard<'_, LruCache<String, Window>> {
        self.windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Segundos redondeados hacia arriba, nunca 0: `Retry-After: 0` invitaría al cliente a reintentar
/// inmediatamente y anularía el límite.
fn retry_after_secs(expires_at: Instant, now: Instant) -> u32 {
    if expires_at <= now {
        return 1;
    }
    let ms = expires_at.duration_since(now).as_millis();
    ms.div_ceil(1_000).clamp(1, u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> (LocalRateLimiter, Instant) {
        (LocalRateLimiter::new(), Instant::now())
    }

    #[test]
    fn permite_hasta_el_maximo_y_devuelve_el_contador_real() {
        let (limiter, now) = base();
        for expected in 1..=3_u32 {
            let d = limiter.check_at(now, "abc", 3, 60);
            assert!(d.allowed);
            assert_eq!(d.current_count, expected);
        }
        let d = limiter.check_at(now, "abc", 3, 60);
        assert!(!d.allowed);
        assert_eq!(d.current_count, 4);
        assert!(d.degraded);
    }

    #[test]
    fn la_ventana_se_reabre_al_expirar() {
        let (limiter, now) = base();
        limiter.check_at(now, "abc", 2, 60);
        limiter.check_at(now, "abc", 2, 60);
        assert!(!limiter.check_at(now, "abc", 2, 60).allowed);

        let despues = now + Duration::from_secs(60);
        let d = limiter.check_at(despues, "abc", 2, 60);
        assert!(d.allowed);
        assert_eq!(d.current_count, 1);
        assert_eq!(d.retry_after_secs, 60);
    }

    #[test]
    fn retry_after_nunca_es_cero() {
        let (limiter, now) = base();
        limiter.check_at(now, "abc", 1, 60);
        let justo_antes = now + Duration::from_millis(59_999);
        let d = limiter.check_at(justo_antes, "abc", 1, 60);
        assert!(!d.allowed);
        assert_eq!(d.retry_after_secs, 1);
        assert_eq!(limiter.check_at(now, "x", 1, 0).retry_after_secs, 1);
    }

    #[test]
    fn las_claves_no_comparten_cuota() {
        let (limiter, now) = base();
        limiter.check_at(now, "aaa", 1, 60);
        assert!(!limiter.check_at(now, "aaa", 1, 60).allowed);
        assert!(limiter.check_at(now, "bbb", 1, 60).allowed);
    }
}
