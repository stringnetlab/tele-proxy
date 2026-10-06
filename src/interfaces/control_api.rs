use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::header::{HeaderValue, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sha2::{Digest, Sha256};
use tracing::Span;

use crate::application::proxy_service::ProxyService;
use crate::domain::errors::{log_domain_error, ErrorResponse, ProxyError};
use crate::domain::models::{ClientConfigResponse, ClientConfigUpdate, RotateIdResponse};
use crate::domain::validators::validate_config_update;

/// Adaptador HTTP del dominio hacia Axum. Vive en `interfaces/` porque `domain/` no importa `axum`
/// (docs/ERROR_DICTIONARY.md, § Camino Axum).
pub struct ApiError {
    error: ProxyError,
    span: Span,
}

impl ApiError {
    /// `internal_id`/`crypt_id` no viajan dentro del `ProxyError`: los aporta el span de la
    /// petición (docs/ERROR_DICTIONARY.md, nota †). Como el evento se emite en `into_response`,
    /// ya fuera del handler, el span tiene que viajar con el error.
    pub fn with_span(error: ProxyError, span: Span) -> Self {
        Self { error, span }
    }
}

impl From<ProxyError> for ApiError {
    fn from(error: ProxyError) -> Self {
        Self {
            error,
            span: Span::current(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.error.to_http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // Un 5xx no filtra el motivo interno al cliente: eso vive solo en el log.
        let message = if status.is_server_error() {
            String::from("Internal server error")
        } else {
            self.error.to_string()
        };
        let body = ErrorResponse {
            error: self.error.to_error_code().to_string(),
            message,
            url: self.error.url().map(str::to_string),
        };

        let _guard = self.span.enter();
        log_domain_error(&self.error);
        drop(_guard);

        let mut response_headers = HeaderMap::new();
        response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(secs) = self.error.retry_after_secs() {
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                response_headers.insert(RETRY_AFTER, value);
            }
        }

        let payload = serde_json::to_vec(&body).unwrap_or_else(|_| {
            Vec::from(r#"{"error":"internal_error","message":"Internal server error"}"#)
        });

        (status, response_headers, payload).into_response()
    }
}

pub fn control_routes() -> Router<Arc<ProxyService>> {
    Router::new()
        .route("/api/v1/clients/config", get(get_config).put(update_config))
        .route("/api/v1/clients/rotate-id", post(rotate_id))
}

async fn get_config(
    State(service): State<Arc<ProxyService>>,
    headers: HeaderMap,
) -> Result<Json<ClientConfigResponse>, ApiError> {
    let token = extract_bearer_token(&headers)?;
    let token_hash = hash_token(&token);
    let config = service
        .config_fetcher()
        .get_by_token_hash(&token_hash)
        .await?;
    Ok(Json(ClientConfigResponse::from(config)))
}

async fn update_config(
    State(service): State<Arc<ProxyService>>,
    headers: HeaderMap,
    body: Result<Json<ClientConfigUpdate>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Un cuerpo que no deserializa (p. ej. `error_handling.mode = "aggressive"`) es 400
    // `invalid_config`, no el 422 en texto plano que devuelve Axum por defecto.
    let Json(update) = body.map_err(|rejection| {
        ApiError::from(ProxyError::InvalidConfig {
            field: "body".to_string(),
            reason: rejection.body_text(),
        })
    })?;

    let token = extract_bearer_token(&headers)?;
    let token_hash = hash_token(&token);
    let config = service
        .config_fetcher()
        .get_by_token_hash(&token_hash)
        .await?;

    // Se valida antes de escribir: CouchDB no debe recibir un documento fuera de cota. El span se
    // adjunta al error porque el registro ocurre en `into_response`.
    let span = tracing::info_span!("config_update", internal_id = %config.internal_id);
    validate_config_update(&update).map_err(|error| ApiError::with_span(error, span))?;

    let updated = service
        .config_fetcher()
        .update_config(&config.internal_id, &update)
        .await?;
    Ok(Json(serde_json::json!({
        "config_version": updated.config_version,
        "updated_at": chrono::Utc::now().to_rfc3339()
    })))
}

async fn rotate_id(
    State(service): State<Arc<ProxyService>>,
    headers: HeaderMap,
) -> Result<Json<RotateIdResponse>, ApiError> {
    let token = extract_bearer_token(&headers)?;
    let token_hash = hash_token(&token);
    let config = service
        .config_fetcher()
        .get_by_token_hash(&token_hash)
        .await?;
    let new_crypt_id = service
        .config_fetcher()
        .rotate_crypt_id(&config.internal_id)
        .await?;
    Ok(Json(RotateIdResponse {
        new_crypt_id,
        rotated_at: chrono::Utc::now().to_rfc3339(),
    }))
}

fn extract_bearer_token(headers: &HeaderMap) -> Result<String, ProxyError> {
    let auth_header = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ProxyError::Unauthorized {
            reason: "Missing Authorization header".to_string(),
        })?;

    if let Some(token) = auth_header.strip_prefix("Bearer ") {
        if token.is_empty() {
            return Err(ProxyError::Unauthorized {
                reason: "Empty bearer token".to_string(),
            });
        }
        Ok(token.to_string())
    } else {
        Err(ProxyError::Unauthorized {
            reason: "Invalid Authorization format, expected Bearer".to_string(),
        })
    }
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    // digest 0.11 dropped the LowerHex impl on the output array, so hex-encode manually.
    let hex = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}
