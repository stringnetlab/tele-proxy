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
use crate::domain::header_rules::{self, RuleContext};
use crate::domain::models::{CachedResponse, ClientConfig, ProxyContext};
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
    request_headers: HeaderMap,
) -> Result<Response, ApiError> {
    // `MODO=desarrollo`: los errores de este handler llevan motivo interno y `details`.
    let verbose = service.verbose_errors();
    let api_err = |e: ProxyError| ApiError::detailed(e, verbose);

    if !validate_crypt_id(&crypt_id) {
        return Err(api_err(ProxyError::InvalidCryptId {
            reason: "Invalid format: must be 12 alphanumeric characters".to_string(),
        }));
    }

    let config = service
        .config_fetcher()
        .get_by_crypt_id(&crypt_id)
        .await
        .map_err(&api_err)?;

    let limit = service
        .cache_store()
        .check_rate_limit(
            &crypt_id,
            config.rate_limit.max_requests,
            config.rate_limit.window_seconds,
        )
        .await;

    if !limit.allowed {
        return Err(api_err(ProxyError::RateLimitExceeded {
            current_count: limit.current_count,
            max_requests: config.rate_limit.max_requests,
            retry_after_secs: limit.retry_after_secs,
        }));
    }

    let url = validate_url_strict(&query.url).map_err(&api_err)?;
    let domain = extract_domain(&url).map_err(&api_err)?;

    if !domain_matches_whitelist(&domain, &config.whitelist) {
        return Err(api_err(ProxyError::DomainNotWhitelisted { domain }));
    }

    // El limiter local (limit.degraded) también es degradación: la cuota aplicada es por
    // instancia, y el cliente tiene que poder saberlo.
    let degraded = service.is_degraded() || config.is_degraded() || limit.degraded;

    let cache_key = response_cache_key(&config.internal_id, config.config_version, &query.url);

    if let Some(cached) = service
        .cache_store()
        .get_response(&cache_key)
        .await
        .map_err(&api_err)?
    {
        let mut resp = build_response_from_cached(&cached, &config, &request_headers, &url)
            .map_err(&api_err)?;
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
        .await
        .map_err(&api_err)?;

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
            return try_fallback_or_error(
                &service,
                &config,
                &query.url,
                fallback_mime,
                &request_headers,
                e,
            )
            .await
            .map_err(&api_err);
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
            &request_headers,
            ProxyError::UpstreamError {
                url: query.url.clone(),
                upstream_status: status,
            },
        )
        .await
        .map_err(&api_err);
    }

    let headers = upstream_response.headers().clone();
    // El tope se aplica durante el stream, no sobre el cuerpo ya bufferizado: un origen puede
    // anunciar un Content-Length pequeño y enviar mucho más.
    let body_bytes = http_client::read_body_capped(upstream_response, MAX_RESPONSE_SIZE)
        .await
        .map_err(&api_err)?;

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let body_bytes =
        apply_lua_scripting(&service, &config, &url, query.mime.as_deref(), body_bytes)
            .await
            .map_err(&api_err)?;

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

    let mut response_pairs: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !must_not_forward_header(k.as_str()))
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|val| (k.as_str().to_string(), val.to_string()))
        })
        .collect();

    // Las reglas de headers del cliente se aplican sobre los headers crudos del upstream, antes
    // de fijar los headers de seguridad del proxy (que siempre ganan, aunque una regla los
    // nombrara — `PUT /config` ya prohíbe esos nombres).
    apply_client_header_rules(&config, &request_headers, &url, status, &mut response_pairs);

    let mut response_headers = pairs_to_header_map(response_pairs);

    response_headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    response_headers.insert(
        "X-Proxy-By",
        HeaderValue::from_static("teleproxy.velone.ai"),
    );
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
    .map_err(&api_err)
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
    request_headers: &HeaderMap,
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

            let mut pairs: Vec<(String, String)> = fallback
                .cached
                .headers
                .iter()
                .filter(|(key, _)| !must_not_forward_header(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            if let Ok(parsed_url) = url::Url::parse(url) {
                apply_client_header_rules(
                    config,
                    request_headers,
                    &parsed_url,
                    fallback.cached.status,
                    &mut pairs,
                );
            }
            let mut headers = pairs_to_header_map(pairs);
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

fn build_response_from_cached(
    cached: &CachedResponse,
    config: &ClientConfig,
    request_headers: &HeaderMap,
    url: &url::Url,
) -> Result<Response, ProxyError> {
    // Los headers cacheados son los crudos del upstream (las reglas se aplican al servir, no al
    // guardar): una regla nueva cobra incluso sobre contenido ya cacheado de la misma versión.
    let mut pairs: Vec<(String, String)> = cached
        .headers
        .iter()
        .filter(|(key, _)| !must_not_forward_header(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    apply_client_header_rules(config, request_headers, url, cached.status, &mut pairs);
    let mut headers = pairs_to_header_map(pairs);

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

/// Aplica las `header_rules` del cliente sobre los pares `(header, valor)` de la respuesta que
/// se está sirviendo. Construye el contexto de evaluación a partir de la request del cliente
/// (los headers que este envió al proxy), la URL destino y los headers de respuesta **antes** de
/// aplicar ninguna regla: cada regla ve las mutaciones de las anteriores, pero ninguna condiciona
/// a su propio resultado.
fn apply_client_header_rules(
    config: &ClientConfig,
    request_headers: &HeaderMap,
    url: &url::Url,
    status: u16,
    response_pairs: &mut Vec<(String, String)>,
) {
    if config.header_rules.is_empty() {
        return;
    }

    let ctx = RuleContext {
        method: "GET".to_string(),
        url: url.clone(),
        request_headers: header_pairs(request_headers),
        response_status: status,
        response_headers: response_pairs.clone(),
    };
    header_rules::apply_header_rules(&config.header_rules, &ctx, response_pairs);
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect()
}

fn pairs_to_header_map(pairs: Vec<(String, String)>) -> HeaderMap {
    let mut map = HeaderMap::with_capacity(pairs.len());
    for (name, value) in pairs {
        if let (Ok(header_name), Ok(header_value)) = (
            name.parse::<axum::http::header::HeaderName>(),
            HeaderValue::from_str(&value),
        ) {
            map.append(header_name, header_value);
        }
    }
    map
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
    use axum::http::{HeaderMap, HeaderValue};

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

    #[test]
    fn pairs_roundtrip_preserves_duplicates() {
        let mut map = HeaderMap::new();
        map.append("x-a", HeaderValue::from_static("1"));
        map.append("set-cookie", HeaderValue::from_static("a=1"));
        map.append("set-cookie", HeaderValue::from_static("b=2"));

        let pairs = super::header_pairs(&map);
        assert_eq!(
            pairs,
            vec![
                ("x-a".to_string(), "1".to_string()),
                ("set-cookie".to_string(), "a=1".to_string()),
                ("set-cookie".to_string(), "b=2".to_string()),
            ]
        );

        let rebuilt = super::pairs_to_header_map(pairs);
        let cookies: Vec<_> = rebuilt
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(cookies, vec!["a=1".to_string(), "b=2".to_string()]);
    }

    #[test]
    fn client_header_rules_apply_over_upstream_pairs() {
        use crate::domain::header_rules::{
            HeaderActionParameters, HeaderOperation, HeaderOperationKind, HeaderRule,
            HeaderRuleAction,
        };
        use std::collections::HashMap;

        let config = crate::domain::models::ClientConfig {
            id: "c1".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "c1".to_string(),
            crypt_id: "crypt_id_123".to_string(),
            bearer_token_hash: "sha256:x".to_string(),
            config_version: 1,
            whitelist: vec![],
            rate_limit: crate::domain::models::RateLimitConfig {
                max_requests: 50,
                window_seconds: 60,
            },
            max_scripting_body_bytes: 0,
            scripting: crate::domain::models::ScriptingConfig {
                enabled: false,
                code: String::new(),
                code_hash: String::new(),
            },
            error_handling: crate::domain::models::ErrorHandlingConfig {
                mode: crate::domain::models::ErrorMode::Wrapped,
                fallback_urls: HashMap::new(),
            },
            header_rules: vec![HeaderRule {
                expression: "http.response.status == 200 and starts_with(http.response.content_type, \"application/json\")".to_string(),
                action: HeaderRuleAction::Set,
                action_parameters: HeaderActionParameters {
                    headers: vec![HeaderOperation {
                        name: "x-algo".to_string(),
                        operation: HeaderOperationKind::Set,
                        value: Some("asi".to_string()),
                    }],
                },
            }],
        };

        let mut request = HeaderMap::new();
        request.insert("x-client", HeaderValue::from_static("ios"));

        // Coincide la expresión: el header se inyecta.
        let mut pairs = vec![("content-type".to_string(), "application/json".to_string())];
        super::apply_client_header_rules(
            &config,
            &request,
            &url::Url::parse("https://example.com/api").unwrap(),
            200,
            &mut pairs,
        );
        assert!(pairs.contains(&("x-algo".to_string(), "asi".to_string())));

        // No coincide: el upstream mandó HTML y la regla no aplica.
        let mut html_pairs = vec![("content-type".to_string(), "text/html".to_string())];
        super::apply_client_header_rules(
            &config,
            &request,
            &url::Url::parse("https://example.com/page").unwrap(),
            200,
            &mut html_pairs,
        );
        assert!(!html_pairs.iter().any(|(k, _)| k == "x-algo"));

        // Sin reglas configuradas es un no-op (el fast-path no construye contexto).
        let mut no_rules = config.clone();
        no_rules.header_rules = Vec::new();
        let mut untouched = vec![("content-type".to_string(), "application/json".to_string())];
        super::apply_client_header_rules(
            &no_rules,
            &request,
            &url::Url::parse("https://example.com/api").unwrap(),
            200,
            &mut untouched,
        );
        assert_eq!(untouched.len(), 1);
    }
}
