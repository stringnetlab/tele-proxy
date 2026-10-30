use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::application::preview::service::PreviewOptions;
use crate::application::proxy_service::ProxyService;
use crate::domain::errors::ProxyError;
use crate::domain::preview::result::PreviewResponse;
use crate::domain::validators::validate_url_strict;
use crate::infrastructure::preview::renderer::RenderFormat;
use crate::interfaces::control_api::ApiError;
use crate::interfaces::proxy_handler::validate_crypt_id;

/// Parámetros de `/mt/{crypt_id}`. `content`, `force` aceptan forma booleana laxa (`1|true|yes|on`)
/// al estilo Microlink; `ttl` en segundos.
#[derive(Debug, Deserialize)]
pub struct MetaQuery {
    pub url: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub ttl: Option<u64>,
    #[serde(default)]
    pub force: Option<String>,
}

/// Parámetros de `/pv/{crypt_id}`. `format` = `screenshot` (default) | `pdf`; `fullPage` y
/// `timeout` opcionales (el timeout se acota contra `preview.max_render_timeout_ms` en el service).
#[derive(Debug, Deserialize)]
pub struct RenderQuery {
    pub url: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default, rename = "fullPage")]
    pub full_page: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn truthy(raw: &Option<String>) -> bool {
    matches!(
        raw.as_deref().map(|v| v.trim().to_ascii_lowercase()),
        Some(ref v) if matches!(v.as_str(), "1" | "true" | "yes" | "on")
    )
}

/// `GET /mt/{crypt_id}` — metadatos de una URL (JSend). Nunca devuelve el body crudo: solo el
/// `PreviewData` extraído por la cascada de detectores.
pub async fn mt_handler(
    State(service): State<Arc<ProxyService>>,
    Path(crypt_id): Path<String>,
    Query(query): Query<MetaQuery>,
) -> Result<Response, ApiError> {
    let verbose = service.settings().verbose_errors;
    // El registro del error ocurre en `ApiError::into_response`, que entra este span: adjuntar
    // `crypt_id` da a los errores de preview la misma trazabilidad que al camino del proxy. Llamar
    // aquí a `log_domain_error` duplicaría el evento (ERROR_DICTIONARY.md § Log del error).
    let span = tracing::info_span!("preview", crypt_id = %crypt_id);
    let api_err = |e: ProxyError| ApiError::with_span_verbose(e, span.clone(), verbose);

    if !validate_crypt_id(&crypt_id) {
        return Err(api_err(ProxyError::InvalidCryptId {
            reason: "Formato inválido: 12 caracteres alfanuméricos".to_string(),
        }));
    }

    let url = require_url(&query.url).map_err(&api_err)?;
    validate_url_strict(&url).map_err(&api_err)?;

    let want_content = truthy(&query.content);
    let force = truthy(&query.force);
    // La clave de caché solo depende de lo que cambia el payload: el flag `content`. `ttl`/`force`
    // no alteran los bytes guardados, así que no fragmentan la entrada.
    let params_canonical = format!("content={}", want_content as u8);

    let opts = PreviewOptions {
        want_content,
        ttl: query.ttl,
        force,
        params_canonical,
    };

    let outcome = service
        .meta_preview(&crypt_id, &url, &opts)
        .await
        .map_err(&api_err)?;

    let body = PreviewResponse::success(outcome.data);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0, no-store"),
    );
    if outcome.degraded {
        headers.insert("X-Degraded-Mode", HeaderValue::from_static("true"));
    }
    headers.insert(
        "X-Cache",
        HeaderValue::from_static(if outcome.cache_hit { "HIT" } else { "MISS" }),
    );

    Ok((StatusCode::OK, headers, Json(body)).into_response())
}

/// `GET /pv/{crypt_id}` — binario renderizado (screenshot PNG o PDF) vía el sidecar browserless.
/// Fail-soft: sin sidecar o con sidecar caído devuelve `503 render_unavailable` (con `Retry-After`),
/// nunca un 500.
pub async fn pv_handler(
    State(service): State<Arc<ProxyService>>,
    Path(crypt_id): Path<String>,
    Query(query): Query<RenderQuery>,
) -> Result<Response, ApiError> {
    let verbose = service.settings().verbose_errors;
    // El registro del error ocurre en `ApiError::into_response`, que entra este span: adjuntar
    // `crypt_id` da a los errores de preview la misma trazabilidad que al camino del proxy. Llamar
    // aquí a `log_domain_error` duplicaría el evento (ERROR_DICTIONARY.md § Log del error).
    let span = tracing::info_span!("preview", crypt_id = %crypt_id);
    let api_err = |e: ProxyError| ApiError::with_span_verbose(e, span.clone(), verbose);

    if !validate_crypt_id(&crypt_id) {
        return Err(api_err(ProxyError::InvalidCryptId {
            reason: "Formato inválido: 12 caracteres alfanuméricos".to_string(),
        }));
    }

    let url = require_url(&query.url).map_err(&api_err)?;
    validate_url_strict(&url).map_err(&api_err)?;

    let format_str = query
        .format
        .as_deref()
        .map(|f| f.trim().to_ascii_lowercase())
        .filter(|f| !f.is_empty());
    let format = match format_str.as_deref() {
        None | Some("screenshot") => RenderFormat::Screenshot,
        Some("pdf") => RenderFormat::Pdf,
        Some(other) => {
            return Err(api_err(ProxyError::InvalidPreviewParams {
                reason: format!("formato desconocido: {other} (usa screenshot|pdf)"),
            }))
        }
    };

    let output = service
        .render_preview(
            &crypt_id,
            &url,
            format,
            truthy_or_none(&query.full_page),
            query.timeout,
        )
        .await
        .map_err(&api_err)?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(output.content_type),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0, no-store"),
    );

    Ok((StatusCode::OK, headers, output.bytes).into_response())
}

fn truthy_or_none(raw: &Option<String>) -> Option<bool> {
    raw.as_ref().map(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn require_url(raw: &Option<String>) -> Result<String, ProxyError> {
    let url = raw.as_deref().map(str::trim).filter(|u| !u.is_empty());
    match url {
        Some(u) => Ok(u.to_string()),
        None => Err(ProxyError::InvalidPreviewParams {
            reason: "falta el parámetro obligatorio `url`".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_acepta_formas_laxas() {
        assert!(truthy(&Some("1".into())));
        assert!(truthy(&Some("TRUE".into())));
        assert!(truthy(&Some(" yes ".into())));
        assert!(!truthy(&Some("0".into())));
        assert!(!truthy(&None));
    }

    #[test]
    fn truthy_or_none_distingue_ausencia() {
        assert_eq!(truthy_or_none(&None), None);
        assert_eq!(truthy_or_none(&Some("1".into())), Some(true));
        assert_eq!(truthy_or_none(&Some("no".into())), Some(false));
    }
}
