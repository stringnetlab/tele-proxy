use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequestParts, State};
use axum::http::header::{HeaderValue, CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sha2::{Digest, Sha256};
use tracing::Span;

use crate::application::admin_service::AdminService;
use crate::application::proxy_service::ProxyService;
use crate::domain::errors::{log_domain_error, ErrorResponse, ProxyError};
use crate::domain::models::{ClientConfig, ClientConfigResponse, ClientConfigUpdate, RotateIdResponse};
use crate::domain::validators::validate_config_update;
use crate::infrastructure::local_rate_limiter::LocalRateLimiter;

/// Estado compartido del listener de control: el `ProxyService` de las rutas de cliente, el
/// `AdminService` del router admin y los limiters por IP (login / admin / control). Un solo
/// estado permite mergear `control_routes()` y `admin_routes()` en el mismo listener.
#[derive(Clone)]
pub struct ControlState {
    pub service: Arc<ProxyService>,
    pub admin: Arc<AdminService>,
    pub login_limiter: Arc<LocalRateLimiter>,
    pub admin_limiter: Arc<LocalRateLimiter>,
    pub control_limiter: Arc<LocalRateLimiter>,
    /// `MODO=desarrollo`: cuerpos de error verbosos (se propaga a `ApiError::detailed`).
    pub modo_desarrollo: bool,
}

/// IP del cliente para el rate limit por IP de las APIs de control/admin. `axum::serve` sin
/// `into_make_service_with_connect_info` no pone `ConnectInfo` en las extensiones, así que la
/// fuente es `X-Forwarded-For`/`X-Real-IP` (el despliegue va tras un proxy) y la última
/// instancia un literal fijo: sin IP no hay bypass de la cuota.
pub struct ClientIp(pub String);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let from_header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let ip = from_header("x-forwarded-for")
            .or_else(|| from_header("x-real-ip"))
            .or_else(|| {
                parts
                    .extensions
                    .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                    .map(|connect| connect.0.ip().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());
        Ok(Self(ip))
    }
}

/// Separa admin de cliente en las rutas de la API de control: un documento `kind == Admin`
/// nunca puede operar como cliente (defensa en profundidad; el master token tampoco se acepta
/// aquí, solo en `/api/v1/admin/*`).
pub fn ensure_client_kind(config: &ClientConfig) -> Result<(), ProxyError> {
    AdminService::ensure_client_kind(config)
}

/// Aplica el rate limit por IP de una de las tres APIs. El límite es por proceso (`LocalRateLimiter`,
/// LRU acotada): la API admin/control es loopback, así que la cuota por instancia es suficiente
/// y no hace falta el contador distribuido de Valkey.
pub fn check_ip_rate_limit(
    limiter: &LocalRateLimiter,
    ip: &str,
    max_requests: u32,
    window_seconds: u64,
    scope: &str,
) -> Result<(), ProxyError> {
    let decision = limiter.check(&format!("{scope}:{ip}"), max_requests, window_seconds);
    if decision.allowed {
        return Ok(());
    }
    Err(ProxyError::RateLimitExceeded {
        current_count: decision.current_count,
        max_requests,
        retry_after_secs: decision.retry_after_secs,
    })
}

/// Adaptador HTTP del dominio hacia Axum. Vive en `interfaces/` porque `domain/` no importa `axum`
/// (docs/ERROR_DICTIONARY.md, § Camino Axum).
pub struct ApiError {
    error: ProxyError,
    span: Span,
    /// `MODO=desarrollo`: el cuerpo del error expone el motivo interno completo y los campos
    /// estructurados (`details`). En cualquier otro modo los 5xx siguen siendo escuetos.
    verbose: bool,
}

impl ApiError {
    /// `internal_id`/`crypt_id` no viajan dentro del `ProxyError`: los aporta el span de la
    /// petición (docs/ERROR_DICTIONARY.md, nota †). Como el evento se emite en `into_response`,
    /// ya fuera del handler, el span tiene que viajar con el error.
    pub fn with_span(error: ProxyError, span: Span) -> Self {
        Self::with_span_verbose(error, span, false)
    }

    pub fn with_span_verbose(error: ProxyError, span: Span, verbose: bool) -> Self {
        Self {
            error,
            span,
            verbose,
        }
    }

    /// Conversión con el flag de verbosidad del servicio (`ProxyService::verbose_errors`).
    pub fn detailed(error: ProxyError, verbose: bool) -> Self {
        Self::with_span_verbose(error, Span::current(), verbose)
    }
}

impl From<ProxyError> for ApiError {
    fn from(error: ProxyError) -> Self {
        Self::with_span_verbose(error, Span::current(), false)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.error.to_http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // Un 5xx no filtra el motivo interno al cliente: eso vive solo en el log. En
        // `MODO=desarrollo` el cuerpo es diagnóstico completo a propósito. En producción los
        // fallos del origen se distinguen de los del propio proxy (Bad gateway / Gateway
        // timeout) sin filtrar detalles internos.
        let message = if self.verbose || !status.is_server_error() {
            self.error.to_string()
        } else {
            self.error.escueto_message().to_string()
        };
        let details = self.verbose.then(|| {
            self.error
                .error_fields()
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>()
        });
        let body = ErrorResponse {
            error: self.error.to_error_code().to_string(),
            message,
            url: self.error.url().map(str::to_string),
            details,
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
            Vec::from(r#"{"error":"internal_error","message":"Error interno del servidor"}"#)
        });

        (status, response_headers, payload).into_response()
    }
}

pub fn control_routes() -> Router<ControlState> {
    Router::new()
        .route("/api/v1/clients/config", get(get_config).put(update_config))
        .route("/api/v1/clients/rotate-id", post(rotate_id))
}

/// Rate limit por IP (`CONTROL_RATE_LIMIT_*`, hallazgo A1) aplicado a las tres rutas de cliente.
fn check_control_rate(state: &ControlState, ip: &ClientIp) -> Result<(), ProxyError> {
    let settings = state.admin.settings();
    check_ip_rate_limit(
        &state.control_limiter,
        &ip.0,
        settings.control_rate_limit_requests,
        settings.control_rate_limit_window_seconds,
        "control",
    )
}

async fn get_config(
    State(state): State<ControlState>,
    headers: HeaderMap,
    ip: ClientIp,
) -> Result<Json<ClientConfigResponse>, ApiError> {
    let verbose = state.modo_desarrollo;
    check_control_rate(&state, &ip).map_err(|e| ApiError::detailed(e, verbose))?;
    let token = extract_bearer_token(&headers)?;
    let token_hash = hash_token(&token);
    let config = state
        .service
        .config_fetcher()
        .get_by_token_hash(&token_hash)
        .await
        .map_err(|e| ApiError::detailed(e, verbose))?;
    ensure_client_kind(&config).map_err(|e| ApiError::detailed(e, verbose))?;
    Ok(Json(ClientConfigResponse::from(config)))
}

async fn update_config(
    State(state): State<ControlState>,
    headers: HeaderMap,
    ip: ClientIp,
    body: Result<Json<ClientConfigUpdate>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let verbose = state.modo_desarrollo;
    check_control_rate(&state, &ip).map_err(|e| ApiError::detailed(e, verbose))?;
    // Un cuerpo que no deserializa (p. ej. `error_handling.mode = "aggressive"`) es 400
    // `invalid_config`, no el 422 en texto plano que devuelve Axum por defecto.
    let Json(update) = body.map_err(|rejection| {
        ApiError::detailed(
            ProxyError::InvalidConfig {
                field: "body".to_string(),
                reason: rejection.body_text(),
            },
            verbose,
        )
    })?;

    let token = extract_bearer_token(&headers)?;
    let token_hash = hash_token(&token);
    let config = state
        .service
        .config_fetcher()
        .get_by_token_hash(&token_hash)
        .await
        .map_err(|e| ApiError::detailed(e, verbose))?;
    ensure_client_kind(&config).map_err(|e| ApiError::detailed(e, verbose))?;

    // Se valida antes de escribir: CouchDB no debe recibir un documento fuera de cota. El span se
    // adjunta al error porque el registro ocurre en `into_response`.
    let span = tracing::info_span!("config_update", internal_id = %config.internal_id);
    validate_config_update(&update)
        .map_err(|error| ApiError::with_span_verbose(error, span, verbose))?;

    let updated = state
        .service
        .config_fetcher()
        .update_config(&config.internal_id, &update)
        .await
        .map_err(|e| ApiError::detailed(e, verbose))?;
    Ok(Json(serde_json::json!({
        "config_version": updated.config_version,
        "updated_at": chrono::Utc::now().to_rfc3339()
    })))
}

async fn rotate_id(
    State(state): State<ControlState>,
    headers: HeaderMap,
    ip: ClientIp,
) -> Result<Json<RotateIdResponse>, ApiError> {
    let verbose = state.modo_desarrollo;
    check_control_rate(&state, &ip).map_err(|e| ApiError::detailed(e, verbose))?;
    let token = extract_bearer_token(&headers)?;
    let token_hash = hash_token(&token);
    let config = state
        .service
        .config_fetcher()
        .get_by_token_hash(&token_hash)
        .await
        .map_err(|e| ApiError::detailed(e, verbose))?;
    ensure_client_kind(&config).map_err(|e| ApiError::detailed(e, verbose))?;
    let new_crypt_id = state
        .service
        .config_fetcher()
        .rotate_crypt_id(&config.internal_id)
        .await
        .map_err(|e| ApiError::detailed(e, verbose))?;
    Ok(Json(RotateIdResponse {
        new_crypt_id,
        rotated_at: chrono::Utc::now().to_rfc3339(),
    }))
}

pub(crate) fn extract_bearer_token(headers: &HeaderMap) -> Result<String, ProxyError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body must be readable");
        serde_json::from_slice(&bytes).expect("error body must be JSON")
    }

    #[tokio::test]
    async fn error_5xx_es_escueto_fuera_de_desarrollo() {
        let response = ApiError::detailed(
            ProxyError::Internal {
                reason: "CouchDB connection refused at 192.168.0.10:5984".to_string(),
            },
            false,
        )
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(response).await;
        assert_eq!(body["error"], "internal_error");
        assert_eq!(body["message"], "Error interno del servidor");
        assert!(body.get("details").is_none(), "{body}");
    }

    #[tokio::test]
    async fn error_5xx_expone_el_motivo_interno_en_desarrollo() {
        let response = ApiError::detailed(
            ProxyError::Internal {
                reason: "CouchDB connection refused at 192.168.0.10:5984".to_string(),
            },
            true,
        )
        .into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(response).await;
        assert_eq!(body["error"], "internal_error");
        assert!(
            body["message"]
                .as_str()
                .expect("message must be a string")
                .contains("CouchDB connection refused"),
            "{body}"
        );
        assert_eq!(
            body["details"]["reason"],
            "CouchDB connection refused at 192.168.0.10:5984"
        );
    }

    #[tokio::test]
    async fn error_5xx_mantiene_el_mensaje_escueto_de_produccion() {
        // Los fallos del origen se distinguen del propio proxy sin filtrar detalles internos.
        let timeout = body_json(
            ApiError::detailed(
                ProxyError::UpstreamTimeout {
                    url: "https://lento.example/x.jpg".to_string(),
                    timeout_ms: 30_000,
                },
                false,
            )
            .into_response(),
        )
        .await;
        assert_eq!(timeout["error"], "upstream_timeout");
        assert_eq!(timeout["message"], "El origen no responde a tiempo");
        assert!(timeout.get("details").is_none(), "{timeout}");

        let desarrollo = body_json(
            ApiError::detailed(
                ProxyError::UpstreamTimeout {
                    url: "https://lento.example/x.jpg".to_string(),
                    timeout_ms: 30_000,
                },
                true,
            )
            .into_response(),
        )
        .await;
        assert!(desarrollo["message"]
            .as_str()
            .expect("message must be a string")
            .contains("lento.example"));
        assert_eq!(desarrollo["details"]["timeout_ms"], "30000");
    }

    #[tokio::test]
    async fn error_4xx_mantiene_el_mensaje_y_suma_details_en_desarrollo() {
        let make_error = || ProxyError::InvalidConfig {
            field: "rate_limit.max_requests".to_string(),
            reason: "0 is outside 1..=10000".to_string(),
        };

        let prod = body_json(ApiError::detailed(make_error(), false).into_response()).await;
        assert_eq!(
            prod["message"],
            "Configuración inválida: rate_limit.max_requests 0 is outside 1..=10000"
        );
        assert!(prod.get("details").is_none(), "{prod}");

        let dev = body_json(ApiError::detailed(make_error(), true).into_response()).await;
        assert_eq!(dev["message"], prod["message"]);
        assert_eq!(dev["details"]["field"], "rate_limit.max_requests");
        assert_eq!(dev["details"]["reason"], "0 is outside 1..=10000");
    }

    #[test]
    fn kind_admin_es_rechazado_en_la_api_de_cliente() {
        let mut config = ClientConfig {
            id: "admin_doc".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "admin_doc".to_string(),
            crypt_id: "abcdefghijkl".to_string(),
            bearer_token_hash: "sha256:x".to_string(),
            config_version: 1,
            kind: crate::domain::models::ClientKind::Admin,
            wildcard: false,
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
                expression: String::new(),
            },
            error_handling: crate::domain::models::ErrorHandlingConfig {
                mode: crate::domain::models::ErrorMode::Transparent,
                fallback_urls: Default::default(),
            },
            header_rules: Vec::new(),
        };

        // Un admin no puede usar la API de cliente: 403 `forbidden`.
        let error = ensure_client_kind(&config).expect_err("admin debe ser rechazado");
        assert_eq!(error.to_http_status(), 403);
        assert_eq!(error.to_error_code(), "forbidden");

        // Un cliente normal sigue pasando.
        config.kind = crate::domain::models::ClientKind::Client;
        assert!(ensure_client_kind(&config).is_ok());
    }

    #[test]
    fn rate_limit_por_ip_devuelve_429_estandar() {
        let limiter = LocalRateLimiter::new();
        for _ in 0..2 {
            assert!(check_ip_rate_limit(&limiter, "10.0.0.1", 2, 60, "control").is_ok());
        }
        let error = check_ip_rate_limit(&limiter, "10.0.0.1", 2, 60, "control")
            .expect_err("tercera petición excede la cuota");
        assert_eq!(error.to_http_status(), 429);
        assert_eq!(error.to_error_code(), "rate_limit_exceeded");
        assert_eq!(error.retry_after_secs(), Some(60));

        // Otra IP tiene su propia cuota.
        assert!(check_ip_rate_limit(&limiter, "10.0.0.2", 2, 60, "control").is_ok());
    }

    #[tokio::test]
    async fn client_ip_prefiere_x_forwarded_for() {
        let request = axum::http::Request::builder()
            .header("x-forwarded-for", "203.0.113.7, 70.41.3.18")
            .body(())
            .expect("request de prueba");
        let (mut parts, _) = request.into_parts();
        let ClientIp(ip) = ClientIp::from_request_parts(&mut parts, &())
            .await
            .expect("extractor infalible");
        assert_eq!(ip, "203.0.113.7");

        // Sin cabeceras de proxy: literal fijo, nunca una cuota compartida accidental.
        let request = axum::http::Request::builder().body(()).expect("request");
        let (mut parts, _) = request.into_parts();
        let ClientIp(ip) = ClientIp::from_request_parts(&mut parts, &())
            .await
            .expect("extractor infalible");
        assert_eq!(ip, "unknown");
    }
}
