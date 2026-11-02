use std::collections::HashMap;

use crate::application::cache_key::{client_fallback_cache_key, sha256_hex, sha256_hex_bytes};
use crate::domain::errors::ProxyError;
use crate::domain::models::{CachedResponse, FallbackResource};
use crate::domain::services::{CacheStore, FallbackFetcher};

const FALLBACK_IMAGE_SVG: &[u8] = include_bytes!("../assets/fallback_image.svg");
const FALLBACK_JSON: &[u8] = include_bytes!("../assets/fallback_json.json");
const FALLBACK_HTML: &[u8] = include_bytes!("../assets/fallback_html.html");

/// Datos de la petición que la cascada de fallbacks necesita. `fallback_urls` es la config del
/// cliente (patrón MIME → recurso); `mime_hint` selecciona el patrón y además valida los cuerpos
/// cacheados, y `ttl_seconds` fija la vida de la copia del fallback configurado.
pub struct FallbackContext<'a> {
    pub internal_id: &'a str,
    pub config_version: u64,
    pub url: &'a str,
    pub mime_hint: Option<&'a str>,
    pub fallback_urls: &'a HashMap<String, FallbackResource>,
    pub ttl_seconds: u64,
}

pub struct FallbackService;

impl FallbackService {
    /// Cascada: copia stale por URL → `fallback_urls` configurado → caché global por MIME →
    /// recurso embebido. Cada nivel puede descartarse (cuerpo con MIME contradictorio, hash
    /// incorrecto, fetch fallido) y el siguiente toma el relevo: un fallback roto nunca puede
    /// empeorar el error original.
    pub async fn resolve(
        cache_store: &dyn CacheStore,
        fetcher: &dyn FallbackFetcher,
        ctx: &FallbackContext<'_>,
    ) -> Option<FallbackResponse> {
        let url_hash = sha256_hex(ctx.url);

        let stale_key = format!(
            "px:{}:{}:{}:stale",
            ctx.internal_id, ctx.config_version, url_hash
        );
        if let Ok(Some(cached)) = cache_store.get_response(&stale_key).await {
            if matches_expected(ctx.mime_hint, &cached.body) {
                tracing::debug!(key = %stale_key, "Sirviendo fallback desde caché stale");
                return Some(FallbackResponse {
                    source: FallbackSource::ClientCache,
                    cached,
                });
            }
            tracing::warn!(
                url = %ctx.url,
                "El fallback stale contradice el MIME esperado; se descarta"
            );
        }

        if let Some(fallback) = resolve_configured(cache_store, fetcher, ctx).await {
            return Some(fallback);
        }

        let mime = ctx.mime_hint.unwrap_or("application/octet-stream");
        let global_key = crate::application::cache_key::fallback_cache_key(mime);
        if let Ok(Some(cached)) = cache_store.get_response(&global_key).await {
            if matches_expected(ctx.mime_hint, &cached.body) {
                tracing::debug!(key = %global_key, "Sirviendo fallback global de Valkey");
                return Some(FallbackResponse {
                    source: FallbackSource::GlobalCache,
                    cached,
                });
            }
            tracing::warn!(
                key = %global_key,
                "El fallback global contradice el MIME esperado; se usa el embebido"
            );
        }

        let (content_type, body) = embedded_fallback_for_mime(mime);
        tracing::debug!(mime = %mime, content_type = %content_type, "Sirviendo fallback binario embebido");
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

/// Nivel `error_handling.fallback_urls`. Primero busca la copia cacheada de este cliente (bajo
/// `config_version`), y solo si no hay una copia válida sale a la red. Todo fallo (patrón sin
/// coincidencia, red, status no-2xx, cuerpo vacío, MIME contradictorio, hash distinto) devuelve
/// `None` para que la cascada siga con el nivel global.
async fn resolve_configured(
    cache_store: &dyn CacheStore,
    fetcher: &dyn FallbackFetcher,
    ctx: &FallbackContext<'_>,
) -> Option<FallbackResponse> {
    let (pattern, resource) = select_fallback(ctx.fallback_urls, ctx.mime_hint)?;
    let cache_key = client_fallback_cache_key(ctx.internal_id, ctx.config_version, ctx.mime_hint);

    if let Ok(Some(cached)) = cache_store.get_response(&cache_key).await {
        if matches_expected(ctx.mime_hint, &cached.body)
            && hash_matches(&resource.hash, &cached.body)
        {
            tracing::debug!(key = %cache_key, pattern = %pattern, "Sirviendo fallback configurado desde caché");
            return Some(FallbackResponse {
                source: FallbackSource::Configured,
                cached,
            });
        }
        tracing::warn!(
            key = %cache_key,
            pattern = %pattern,
            "El fallback configurado cacheado no coincide con el MIME o el hash; se refetchea"
        );
    }

    let fetched = match fetcher.fetch(&resource.url).await {
        Ok(fetched) => fetched,
        Err(e) => {
            reject_configured(pattern, resource, &format!("fetch: {e}"));
            return None;
        }
    };

    if !(200..300).contains(&fetched.status) {
        reject_configured(pattern, resource, &format!("status {}", fetched.status));
        return None;
    }
    if fetched.body.is_empty() {
        reject_configured(pattern, resource, "cuerpo vacío");
        return None;
    }
    if !matches_expected(ctx.mime_hint, &fetched.body) {
        reject_configured(pattern, resource, "MIME contradictorio");
        return None;
    }
    if !hash_matches(&resource.hash, &fetched.body) {
        reject_configured(pattern, resource, "sha256 distinto");
        return None;
    }

    if let Err(e) = cache_store
        .set_response(&cache_key, &fetched, ctx.ttl_seconds)
        .await
    {
        tracing::warn!(error = %e, "No se pudo cachear el fallback configurado");
    }

    tracing::debug!(
        pattern = %pattern,
        fallback_url = %resource.url,
        status = fetched.status,
        "Fallback configurado descargado y verificado"
    );
    Some(FallbackResponse {
        source: FallbackSource::Configured,
        cached: fetched,
    })
}

/// Patrón con prioridad: coincidencia exacta → `type/*` → `*/*`. Sin `mime_hint` solo encaja
/// `*/*`, porque un fallback tipado no puede asignarse a un tipo desconocido.
fn select_fallback<'a>(
    fallback_urls: &'a HashMap<String, FallbackResource>,
    mime_hint: Option<&str>,
) -> Option<(&'a str, &'a FallbackResource)> {
    use crate::domain::mime;

    let hint = mime_hint.map(|hint| mime::essence(hint).to_ascii_lowercase());
    let mut family: Option<(&str, &FallbackResource)> = None;
    let mut global: Option<(&str, &FallbackResource)> = None;

    for (pattern, resource) in fallback_urls {
        let key = pattern.trim().to_ascii_lowercase();

        if let Some(hint) = hint.as_deref() {
            if key == hint {
                return Some((pattern.as_str(), resource));
            }
            if let Some((major, subtype)) = key.split_once('/') {
                if subtype == "*" && hint.starts_with(&format!("{major}/")) {
                    family.get_or_insert((pattern.as_str(), resource));
                    continue;
                }
            }
        }

        if key == "*/*" {
            global.get_or_insert((pattern.as_str(), resource));
        }
    }

    family.or(global)
}

fn reject_configured(pattern: &str, resource: &FallbackResource, reason: &str) {
    tracing::warn!(
        event = "fallback_configured_rejected",
        pattern = %pattern,
        fallback_url = %resource.url,
        reason = %reason,
        "Fallback configurado descartado; se continúa con el siguiente nivel"
    );
}

fn hash_matches(expected: &str, body: &[u8]) -> bool {
    match expected.strip_prefix("sha256:") {
        Some(digest) => digest.eq_ignore_ascii_case(&sha256_hex_bytes(body)),
        // El `PUT` valida el formato; un documento escrito a mano sin `sha256:` no se sirve.
        None => false,
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
    Configured,
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

/// Un candidato de la caché solo sirve si sus primeros bytes no contradicen el tipo esperado. El
/// cuerpo desconocido se acepta: la ausencia de firma no prueba que esté mal.
fn matches_expected(expected_mime: Option<&str>, body: &[u8]) -> bool {
    use crate::domain::mime;
    let Some(expected) = expected_mime.filter(|expected| mime::is_binary(expected)) else {
        return true;
    };
    mime::sniff(body).is_none_or(|detected| mime::agrees(expected, detected))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

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

    #[test]
    fn un_candidato_html_no_sirve_como_imagen() {
        let html = b"<!DOCTYPE html><html><body>challenge</body></html>";
        assert!(!matches_expected(Some("image/jpeg"), html));
        assert!(!matches_expected(Some("application/pdf"), html));
        let png = b"\x89PNG\r\n\x1A\n....";
        assert!(matches_expected(Some("image/jpeg"), png));
        assert!(matches_expected(None, html));
        assert!(matches_expected(Some("text/html"), html));
        // Sin firma reconocible no se puede probar que el candidato esté mal.
        assert!(matches_expected(Some("image/jpeg"), b"bytes opacos"));
    }

    fn resource(url: &str) -> FallbackResource {
        FallbackResource {
            url: url.to_string(),
            hash: String::new(),
        }
    }

    #[test]
    fn el_patron_exacto_gana_al_comodin_de_familia_y_al_global() {
        let map: HashMap<String, FallbackResource> = [
            ("*/*".to_string(), resource("https://cdn/any.bin")),
            ("image/*".to_string(), resource("https://cdn/image.png")),
            ("image/webp".to_string(), resource("https://cdn/webp.png")),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            select_fallback(&map, Some("image/webp")).unwrap().1.url,
            "https://cdn/webp.png"
        );
        assert_eq!(
            select_fallback(&map, Some("image/jpeg")).unwrap().1.url,
            "https://cdn/image.png"
        );
        assert_eq!(
            select_fallback(&map, Some("text/plain")).unwrap().1.url,
            "https://cdn/any.bin"
        );
        assert_eq!(
            select_fallback(&map, None).unwrap().1.url,
            "https://cdn/any.bin"
        );
    }

    #[test]
    fn sin_patron_global_un_mime_desconocido_no_selecciona_nada() {
        let map: HashMap<String, FallbackResource> =
            [("image/*".to_string(), resource("https://cdn/image.png"))]
                .into_iter()
                .collect();

        assert!(select_fallback(&map, Some("application/json")).is_none());
        assert!(select_fallback(&map, None).is_none());
        assert!(select_fallback(&map, Some("image/png; q=1")).is_some());
    }

    #[test]
    fn el_hash_sha256_se_compara_en_minusculas() {
        let body = b"png-bytes";
        let digest = sha256_hex_bytes(body);
        assert!(hash_matches(&format!("sha256:{digest}"), body));
        assert!(hash_matches(
            &format!("sha256:{}", digest.to_uppercase()),
            body
        ));
        assert!(!hash_matches(&format!("sha256:{}", "0".repeat(64)), body));
        assert!(!hash_matches("md5:abc", body));
    }

    struct StubCache {
        entries: Mutex<Vec<(String, CachedResponse)>>,
    }

    impl StubCache {
        fn new(entries: Vec<(&str, CachedResponse)>) -> Self {
            Self {
                entries: Mutex::new(
                    entries
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                ),
            }
        }
    }

    #[async_trait]
    impl CacheStore for StubCache {
        async fn get_response(&self, key: &str) -> Result<Option<CachedResponse>, ProxyError> {
            Ok(self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.clone()))
        }
        async fn set_response(
            &self,
            key: &str,
            response: &CachedResponse,
            _ttl: u64,
        ) -> Result<(), ProxyError> {
            self.entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((key.to_string(), response.clone()));
            Ok(())
        }
        async fn get_ip(&self, _: &str) -> Result<Option<std::net::IpAddr>, ProxyError> {
            Ok(None)
        }
        async fn set_ip(&self, _: &str, _: &std::net::IpAddr, _: u64) -> Result<(), ProxyError> {
            Ok(())
        }
        async fn check_rate_limit(
            &self,
            _: &str,
            _: u32,
            _: u64,
        ) -> crate::domain::models::RateLimitDecision {
            crate::domain::models::RateLimitDecision {
                allowed: true,
                current_count: 0,
                retry_after_secs: 0,
                degraded: false,
            }
        }
        async fn purge_client_responses(&self, _: &str) -> Result<u64, ProxyError> {
            Ok(0)
        }
    }

    struct StubFetcher {
        response: Result<CachedResponse, String>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl FallbackFetcher for StubFetcher {
        async fn fetch(&self, url: &str) -> Result<CachedResponse, ProxyError> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push(url.to_string());
            }
            match &self.response {
                Ok(response) => Ok(response.clone()),
                Err(reason) => Err(ProxyError::UpstreamError {
                    url: url.to_string(),
                    upstream_status: 0,
                    reason: reason.clone(),
                }),
            }
        }
    }

    fn context<'a>(
        urls: &'a HashMap<String, FallbackResource>,
        mime_hint: Option<&'a str>,
    ) -> FallbackContext<'a> {
        FallbackContext {
            internal_id: "client_1",
            config_version: 7,
            url: "https://origin.example.com/missing.png",
            mime_hint,
            fallback_urls: urls,
            ttl_seconds: 60,
        }
    }

    fn png_response() -> CachedResponse {
        CachedResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "image/png".to_string())],
            body: b"\x89PNG\r\n\x1A\nbody".to_vec(),
        }
    }

    fn configured() -> StubFetcher {
        StubFetcher {
            response: Ok(png_response()),
            calls: Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn el_fallback_configurado_se_verifica_cachead_y_sirve() {
        let body = png_response().body;
        let mut urls = HashMap::new();
        urls.insert(
            "image/*".to_string(),
            FallbackResource {
                url: "https://cdn.example.com/error.png".to_string(),
                hash: format!("sha256:{}", sha256_hex_bytes(&body)),
            },
        );
        let cache = StubCache::new(vec![]);
        let fetcher = configured();

        let response =
            FallbackService::resolve(&cache, &fetcher, &context(&urls, Some("image/png")))
                .await
                .expect("fallback configurado");

        assert_eq!(response.source, FallbackSource::Configured);
        assert_eq!(response.cached.body, body);
        assert_eq!(
            fetcher.calls.lock().unwrap().as_slice(),
            ["https://cdn.example.com/error.png"]
        );

        // La segunda resolución sale de la copia cacheada, sin red.
        let second = FallbackService::resolve(&cache, &fetcher, &context(&urls, Some("image/png")))
            .await
            .expect("copia cacheada");
        assert_eq!(second.source, FallbackSource::Configured);
        assert_eq!(fetcher.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn un_hash_incorrecto_se_descarta_y_sigue_la_cascada() {
        let mut urls = HashMap::new();
        urls.insert(
            "image/*".to_string(),
            FallbackResource {
                url: "https://cdn.example.com/error.png".to_string(),
                hash: format!("sha256:{}", "0".repeat(64)),
            },
        );
        let cache = StubCache::new(vec![]);
        let fetcher = configured();

        let response =
            FallbackService::resolve(&cache, &fetcher, &context(&urls, Some("image/png")))
                .await
                .expect("cascada");

        assert_eq!(
            response.source,
            FallbackSource::Embedded,
            "un hash inválido no puede servir contenido sin verificar"
        );
    }

    #[tokio::test]
    async fn un_status_no_exitoso_o_un_cuerpo_con_mime_erroneo_se_descartan() {
        let body = png_response().body;
        let hash = format!("sha256:{}", sha256_hex_bytes(&body));

        for (label, response) in [
            (
                "500",
                CachedResponse {
                    status: 500,
                    ..png_response()
                },
            ),
            (
                "html",
                CachedResponse {
                    body: b"<html>error</html>".to_vec(),
                    ..png_response()
                },
            ),
            (
                "vacío",
                CachedResponse {
                    body: Vec::new(),
                    ..png_response()
                },
            ),
        ] {
            let mut urls = HashMap::new();
            urls.insert(
                "image/*".to_string(),
                FallbackResource {
                    url: "https://cdn.example.com/error.png".to_string(),
                    hash: hash.clone(),
                },
            );
            let cache = StubCache::new(vec![]);
            let fetcher = StubFetcher {
                response: Ok(response),
                calls: Mutex::new(Vec::new()),
            };

            let resolved =
                FallbackService::resolve(&cache, &fetcher, &context(&urls, Some("image/png")))
                    .await;
            assert_eq!(
                resolved.map(|r| r.source),
                Some(FallbackSource::Embedded),
                "{label}"
            );
        }
    }

    #[tokio::test]
    async fn un_fetch_fallido_no_impide_el_fallback_embebido() {
        let mut urls = HashMap::new();
        urls.insert(
            "image/*".to_string(),
            FallbackResource {
                url: "https://cdn.example.com/error.png".to_string(),
                hash: format!("sha256:{}", sha256_hex_bytes(&png_response().body)),
            },
        );
        let cache = StubCache::new(vec![]);
        let fetcher = StubFetcher {
            response: Err("timeout".to_string()),
            calls: Mutex::new(Vec::new()),
        };

        let response =
            FallbackService::resolve(&cache, &fetcher, &context(&urls, Some("image/png")))
                .await
                .expect("embebido");
        assert_eq!(response.source, FallbackSource::Embedded);
    }

    #[tokio::test]
    async fn sin_patron_aplicable_no_se_sale_a_la_red() {
        let mut urls = HashMap::new();
        urls.insert(
            "image/*".to_string(),
            FallbackResource {
                url: "https://cdn.example.com/error.png".to_string(),
                hash: format!("sha256:{}", sha256_hex_bytes(&png_response().body)),
            },
        );
        let cache = StubCache::new(vec![]);
        let fetcher = configured();

        let response =
            FallbackService::resolve(&cache, &fetcher, &context(&urls, Some("application/json")))
                .await
                .expect("embebido");

        assert_eq!(response.source, FallbackSource::Embedded);
        assert!(fetcher.calls.lock().unwrap().is_empty());
    }
}
