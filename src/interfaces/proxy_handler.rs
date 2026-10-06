use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use serde::Deserialize;

use crate::application::cache_key::response_cache_key;
use crate::application::fallback::{FallbackService, FallbackSource};
use crate::application::proxy_service::ProxyService;
use crate::domain::errors::{log_domain_error, ProxyError};
use crate::domain::models::{CachedResponse, ProxyContext};
use crate::domain::validators::{domain_matches_whitelist, extract_domain, validate_url_strict};
use crate::infrastructure::http_client;
use crate::interfaces::control_api::ApiError;

const MAX_RESPONSE_SIZE: u64 = 100 * 1024 * 1024;
const CACHEABLE_SIZE_LIMIT: usize = 5 * 1024 * 1024;
const UPSTREAM_TIMEOUT_SECS: u64 = 30;
const STALE_CACHE_TTL: u64 = 86400;

#[derive(Deserialize)]
pub struct ProxyQuery {
    pub url: String,
    pub mime: Option<String>,
}

pub async fn proxy_handler(
    State(service): State<Arc<ProxyService>>,
    Path(crypt_id): Path<String>,
    Query(query): Query<ProxyQuery>,
) -> Result<Response, ApiError> {
    if !validate_crypt_id(&crypt_id) {
        return Err(ApiError::from(ProxyError::InvalidCryptId {
            reason: "Invalid format: must be 12 alphanumeric characters".to_string(),
        }));
    }

    let config = service.config_fetcher().get_by_crypt_id(&crypt_id).await?;

    let limit = service
        .cache_store()
        .check_rate_limit(
            &crypt_id,
            config.rate_limit.max_requests,
            config.rate_limit.window_seconds,
        )
        .await;

    if !limit.allowed {
        return Err(ApiError::from(ProxyError::RateLimitExceeded {
            current_count: limit.current_count,
            max_requests: config.rate_limit.max_requests,
            retry_after_secs: limit.retry_after_secs,
        }));
    }

    let url = validate_url_strict(&query.url)?;
    let domain = extract_domain(&url)?;

    if !domain_matches_whitelist(&domain, &config.whitelist) {
        return Err(ApiError::from(ProxyError::DomainNotWhitelisted { domain }));
    }

    // El limiter local (limit.degraded) también es degradación: la cuota aplicada es por
    // instancia, y el cliente tiene que poder saberlo.
    let degraded = service.is_degraded() || config.is_degraded() || limit.degraded;

    let cache_key = response_cache_key(&config.internal_id, config.config_version, &query.url);

    if let Some(cached) = service.cache_store().get_response(&cache_key).await? {
        let mut resp = build_response_from_cached(&cached)?;
        if degraded {
            resp.headers_mut()
                .insert("X-Degraded-Mode", HeaderValue::from_static("true"));
        }
        return Ok(resp);
    }

    let hostname = url.host_str().ok_or_else(|| ProxyError::InvalidUrlFormat {
        url: query.url.clone(),
        reason: "URL has no host".to_string(),
    })?;

    let resolved_ip = service
        .dns_resolver()
        .resolve_and_validate(hostname)
        .await?;

    // BDD Feature 5: the fallback MIME is taken from the explicit "?mime=" param when
    // present, otherwise inferred from the requested URL's file extension. Without the
    // extension fallback, a ".jpg" request that errors resolves mime to octet-stream and
    // the embedded fallback wrongly returns a JSON error body, breaking <img> layout.
    let fallback_mime = query
        .mime
        .as_deref()
        .or_else(|| infer_mime_from_url(&query.url));

    let upstream_result = make_upstream_request(&url, resolved_ip).await;

    let upstream_response = match upstream_result {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(error = %e, url = %query.url, "Upstream request failed, trying fallbacks");
            return try_fallback_or_error(&service, &config, &query.url, fallback_mime, e)
                .await
                .map_err(ApiError::from);
        }
    };

    let status = upstream_response.status().as_u16();

    if status >= 400 {
        tracing::warn!(status = status, url = %query.url, "Upstream returned error, trying fallbacks");
        return try_fallback_or_error(
            &service,
            &config,
            &query.url,
            fallback_mime,
            ProxyError::UpstreamError {
                url: query.url.clone(),
                upstream_status: status,
            },
        )
        .await
        .map_err(ApiError::from);
    }

    let headers = upstream_response.headers().clone();
    // El tope se aplica durante el stream, no sobre el cuerpo ya bufferizado: un origen puede
    // anunciar un Content-Length pequeño y enviar mucho más.
    let body_bytes = http_client::read_body_capped(upstream_response, MAX_RESPONSE_SIZE).await?;

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let body_bytes =
        apply_lua_scripting(&service, &config, &url, query.mime.as_deref(), body_bytes).await?;

    let cacheable = body_bytes.len() <= CACHEABLE_SIZE_LIMIT;
    if cacheable {
        let cached = CachedResponse {
            status,
            headers: headers
                .iter()
                .filter(|(k, _)| !must_not_forward_header(k.as_str()))
                .filter_map(|(k, v)| v.to_str().ok().map(|val| (k.to_string(), val.to_string())))
                .collect(),
            body: body_bytes.clone(),
        };

        let ttl = determine_cache_ttl(content_type);
        if let Err(e) = service
            .cache_store()
            .set_response(&cache_key, &cached, ttl)
            .await
        {
            tracing::warn!(error = %e, "Failed to cache response");
        }

        if let Err(e) = FallbackService::store_stale_fallback(
            service.cache_store().as_ref(),
            &config.internal_id,
            config.config_version,
            &query.url,
            &cached,
            STALE_CACHE_TTL,
        )
        .await
        {
            tracing::warn!(error = %e, "Failed to store stale fallback");
        }
    }

    let mut response_headers = HeaderMap::new();

    for (key, value) in headers.iter() {
        if must_not_forward_header(key.as_str()) {
            continue;
        }
        if let Ok(val) = value.to_str() {
            response_headers.insert(
                key,
                HeaderValue::from_str(val).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
    }

    response_headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    response_headers.insert("X-Proxy-By", HeaderValue::from_static("teleproxy.velone.ai"));
    response_headers.insert(
        "X-Cache",
        HeaderValue::from_static(if cacheable { "MISS" } else { "BYPASS" }),
    );

    if degraded {
        response_headers.insert("X-Degraded-Mode", HeaderValue::from_static("true"));
    }

    build_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        response_headers,
        body_bytes,
    )
    .map_err(ApiError::from)
}

async fn apply_lua_scripting(
    service: &ProxyService,
    config: &crate::domain::models::ClientConfig,
    url: &url::Url,
    mime_hint: Option<&str>,
    body_bytes: Vec<u8>,
) -> Result<Vec<u8>, ProxyError> {
    if !config.scripting.enabled || config.scripting.code.is_empty() {
        return Ok(body_bytes);
    }

    let context = ProxyContext {
        crypt_id: config.crypt_id.clone(),
        internal_id: config.internal_id.clone(),
        config_version: config.config_version,
        target_url: url.clone(),
        mime_hint: mime_hint.map(String::from),
        config: config.clone(),
    };

    match service
        .lua_executor()
        .execute(&config.scripting.code, &body_bytes, &context)
        .await
    {
        Ok(transformed) => Ok(transformed),
        // Un script no puede convertir la petición en un proxy hacia redes internas: si el
        // webhook del sandbox fue rechazado, la petición entera se aborta con el mismo 403 que
        // devolvería la URL del `?url=`. Cualquier otro fallo del sandbox sí degrada al cuerpo
        // original, y su único registro queda en el log.
        Err(e @ (ProxyError::SsrfBlocked { .. } | ProxyError::DomainNotWhitelisted { .. })) => {
            Err(e)
        }
        Err(e) => {
            log_domain_error(&e);
            Ok(body_bytes)
        }
    }
}

async fn try_fallback_or_error(
    service: &ProxyService,
    config: &crate::domain::models::ClientConfig,
    url: &str,
    mime_hint: Option<&str>,
    original_error: ProxyError,
) -> Result<Response, ProxyError> {
    if config.error_handling.mode == crate::domain::models::ErrorMode::Transparent {
        return Err(original_error);
    }

    match FallbackService::resolve(
        service.cache_store().as_ref(),
        &config.internal_id,
        config.config_version,
        url,
        mime_hint,
    )
    .await
    {
        Some(fallback) => {
            let x_fallback = match fallback.source {
                FallbackSource::ClientCache => "client-cache",
                FallbackSource::GlobalCache => "global-cache",
                FallbackSource::Embedded => "embedded",
            };

            let mut headers = HeaderMap::new();
            for (key, value) in &fallback.cached.headers {
                if must_not_forward_header(key) {
                    continue;
                }
                if let (Ok(name), Ok(val)) = (
                    key.parse::<axum::http::header::HeaderName>(),
                    HeaderValue::from_str(value),
                ) {
                    headers.insert(name, val);
                }
            }
            headers.insert(
                "X-Content-Type-Options",
                HeaderValue::from_static("nosniff"),
            );
            headers.insert("X-Proxy-By", HeaderValue::from_static("tele.velone.ai"));
            headers.insert("X-Cache", HeaderValue::from_static("FALLBACK"));
            headers.insert("X-Fallback-Source", HeaderValue::from_static(x_fallback));

            tracing::info!(source = x_fallback, url = %url, "Serving fallback response");

            build_response(StatusCode::OK, headers, fallback.cached.body)
        }
        None => Err(original_error),
    }
}

fn build_response_from_cached(cached: &CachedResponse) -> Result<Response, ProxyError> {
    let mut headers = HeaderMap::new();

    for (key, value) in &cached.headers {
        if must_not_forward_header(key) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            key.parse::<axum::http::header::HeaderName>(),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, val);
        }
    }

    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("X-Proxy-By", HeaderValue::from_static("tele.velone.ai"));
    headers.insert("X-Cache", HeaderValue::from_static("HIT"));

    build_response(
        StatusCode::from_u16(cached.status).unwrap_or(StatusCode::OK),
        headers,
        cached.body.clone(),
    )
}

async fn make_upstream_request(
    url: &url::Url,
    resolved_ip: std::net::IpAddr,
) -> Result<reqwest::Response, ProxyError> {
    let hostname = url.host_str().ok_or_else(|| ProxyError::Internal {
        reason: "Upstream URL has no host".to_string(),
    })?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ProxyError::Internal {
            reason: "Upstream URL has no determinable port".to_string(),
        })?;

    let client = http_client::pinned_client(
        hostname,
        resolved_ip,
        port,
        std::time::Duration::from_secs(UPSTREAM_TIMEOUT_SECS),
    )?;

    client.get(url.as_str()).send().await.map_err(|e| {
        tracing::warn!(
            url = %url,
            pinned_ip = %resolved_ip,
            port = port,
            error = %e,
            "Upstream connection failed"
        );
        ProxyError::UpstreamError {
            url: url.to_string(),
            upstream_status: 502,
        }
    })
}

// A reverse proxy must not forward transport-framing or content-encoding headers
// from upstream: hyper computes Content-Length for our buffered body, and reqwest
// has already transparently decoded any Content-Encoding. Blindly copying these
// (notably Transfer-Encoding: chunked) makes the h1 server encoder abort the
// connection with "user sent unexpected header" before any response is sent.
fn must_not_forward_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length"
            | "content-encoding"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
    )
}

// Derive a MIME type from the requested URL's path extension so wrapped-mode fallbacks
// pick the right embedded placeholder (e.g. an image SVG for ".jpg") when the caller
// did not pass an explicit "?mime=" param. Returns None for unknown or extension-less
// paths, letting the fallback chain default to the JSON error body.
fn infer_mime_from_url(raw_url: &str) -> Option<&'static str> {
    let path = url::Url::parse(raw_url).ok()?.path().to_string();
    let last_segment = path.rsplit('/').next()?;
    let (stem, ext) = last_segment.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "avif" => Some("image/avif"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "html" | "htm" => Some("text/html"),
        "css" => Some("text/css"),
        "js" | "mjs" => Some("application/javascript"),
        "json" => Some("application/json"),
        "pdf" => Some("application/pdf"),
        "txt" => Some("text/plain"),
        _ => None,
    }
}

fn determine_cache_ttl(content_type: &str) -> u64 {
    if content_type.starts_with("image/") {
        3600
    } else if content_type.starts_with("text/css")
        || content_type.starts_with("application/javascript")
    {
        1800
    } else {
        300
    }
}

fn build_response(
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<Response, ProxyError> {
    Response::builder()
        .status(status)
        .body(Body::from(body))
        .map(|mut resp| {
            *resp.headers_mut() = headers;
            resp
        })
        .map_err(|e| ProxyError::Internal {
            reason: format!("Failed to build response: {}", e),
        })
}

fn validate_crypt_id(crypt_id: &str) -> bool {
    if crypt_id.len() != 12 {
        return false;
    }
    crypt_id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::{infer_mime_from_url, must_not_forward_header};

    #[test]
    fn strips_framing_and_encoding_headers() {
        for name in [
            "transfer-encoding",
            "Transfer-Encoding",
            "content-length",
            "CONTENT-ENCODING",
            "connection",
            "upgrade",
            "keep-alive",
            "te",
            "trailer",
            "proxy-authorization",
        ] {
            assert!(must_not_forward_header(name), "{name} must be stripped");
        }
    }

    #[test]
    fn keeps_end_to_end_headers() {
        for name in [
            "content-type",
            "cache-control",
            "etag",
            "last-modified",
            "server",
            "x-custom",
        ] {
            assert!(!must_not_forward_header(name), "{name} must be forwarded");
        }
    }

    #[test]
    fn infers_mime_from_image_extension() {
        assert_eq!(
            infer_mime_from_url("https://cdn.example.com/a/b/photo.JPG"),
            Some("image/jpeg")
        );
        assert_eq!(
            infer_mime_from_url("https://cdn.example.com/logo.png?v=2"),
            Some("image/png")
        );
        assert_eq!(
            infer_mime_from_url("https://cdn.example.com/x/y.jpg"),
            Some("image/jpeg")
        );
    }

    #[test]
    fn returns_none_without_known_extension() {
        assert_eq!(
            infer_mime_from_url("https://example.com/api/getAsset"),
            None
        );
        assert_eq!(infer_mime_from_url("https://example.com/"), None);
        assert_eq!(infer_mime_from_url("not a url"), None);
        assert_eq!(infer_mime_from_url("https://example.com/.hidden"), None);
    }
}
