use crate::domain::errors::ProxyError;
use crate::domain::models::CachedResponse;
use crate::domain::services::CacheStore;

const FALLBACK_IMAGE_SVG: &[u8] = include_bytes!("../assets/fallback_image.svg");
const FALLBACK_JSON: &[u8] = include_bytes!("../assets/fallback_json.json");
const FALLBACK_HTML: &[u8] = include_bytes!("../assets/fallback_html.html");

pub struct FallbackService;

impl FallbackService {
    pub async fn resolve(
        cache_store: &dyn CacheStore,
        internal_id: &str,
        config_version: u64,
        url: &str,
        mime_hint: Option<&str>,
    ) -> Option<FallbackResponse> {
        let url_hash = crate::application::cache_key::sha256_hex(url);

        let stale_key = format!("px:{}:{}:{}:stale", internal_id, config_version, url_hash);
        if let Ok(Some(cached)) = cache_store.get_response(&stale_key).await {
            tracing::debug!(key = %stale_key, "Serving stale cache fallback");
            return Some(FallbackResponse {
                source: FallbackSource::ClientCache,
                cached,
            });
        }

        let mime = mime_hint.unwrap_or("application/octet-stream");
        let global_key = crate::application::cache_key::fallback_cache_key(mime);
        if let Ok(Some(cached)) = cache_store.get_response(&global_key).await {
            tracing::debug!(key = %global_key, "Serving global Valkey fallback");
            return Some(FallbackResponse {
                source: FallbackSource::GlobalCache,
                cached,
            });
        }

        let (content_type, body) = embedded_fallback_for_mime(mime);
        tracing::debug!(mime = %mime, content_type = %content_type, "Serving embedded binary fallback");
        Some(FallbackResponse {
            source: FallbackSource::Embedded,
            cached: CachedResponse {
                status: 200,
                headers: vec![("content-type".to_string(), content_type.to_string())],
                body: body.to_vec(),
            },
        })
    }

    pub async fn store_global_fallback(
        cache_store: &dyn CacheStore,
        mime: &str,
        response: &CachedResponse,
    ) -> Result<(), ProxyError> {
        let key = crate::application::cache_key::fallback_cache_key(mime);
        cache_store.set_response(&key, response, 0).await
    }

    pub async fn store_stale_fallback(
        cache_store: &dyn CacheStore,
        internal_id: &str,
        config_version: u64,
        url: &str,
        response: &CachedResponse,
        ttl_seconds: u64,
    ) -> Result<(), ProxyError> {
        let url_hash = crate::application::cache_key::sha256_hex(url);
        let key = format!("px:{}:{}:{}:stale", internal_id, config_version, url_hash);
        cache_store.set_response(&key, response, ttl_seconds).await
    }
}

#[derive(Debug)]
pub struct FallbackResponse {
    pub source: FallbackSource,
    pub cached: CachedResponse,
}

#[derive(Debug, PartialEq)]
pub enum FallbackSource {
    ClientCache,
    GlobalCache,
    Embedded,
}

fn embedded_fallback_for_mime(mime: &str) -> (&'static str, &'static [u8]) {
    if mime.starts_with("image/") {
        ("image/svg+xml", FALLBACK_IMAGE_SVG)
    } else if mime.starts_with("application/json") || mime.contains("json") {
        ("application/json", FALLBACK_JSON)
    } else if mime.starts_with("text/html") {
        ("text/html", FALLBACK_HTML)
    } else {
        ("application/json", FALLBACK_JSON)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_fallback_image() {
        let (ct, body) = embedded_fallback_for_mime("image/png");
        assert_eq!(ct, "image/svg+xml");
        assert!(!body.is_empty());
    }

    #[test]
    fn test_embedded_fallback_json() {
        let (ct, body) = embedded_fallback_for_mime("application/json");
        assert_eq!(ct, "application/json");
        assert!(!body.is_empty());
    }

    #[test]
    fn test_embedded_fallback_html() {
        let (ct, body) = embedded_fallback_for_mime("text/html");
        assert_eq!(ct, "text/html");
        assert!(!body.is_empty());
    }

    #[test]
    fn test_embedded_fallback_unknown_mime() {
        let (ct, body) = embedded_fallback_for_mime("application/octet-stream");
        assert_eq!(ct, "application/json");
        assert!(!body.is_empty());
    }
}
