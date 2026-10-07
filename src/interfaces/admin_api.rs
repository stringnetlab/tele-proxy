use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::application::admin_service::AdminClientUpdate;
use crate::domain::errors::{log_domain_error, ProxyError};
use crate::domain::header_rules::HeaderRule;
use crate::domain::models::{
    ClientConfig, ClientKind, ErrorHandlingConfig, RateLimitConfig, RotateIdResponse,
};
use crate::domain::validators::validate_config_update;
use crate::interfaces::control_api::{
    check_ip_rate_limit, ensure_client_kind, ApiError, ClientIp, ControlState,
};

/// Router de la administración global (`/api/v1/admin/*`). Todo pasa por `AdminIdentity` —
/// cookie de sesión válida o master token — salvo las rutas de auth, que son el camino hacia la
/// sesión y llevan su propio rate limit estricto.
pub fn admin_routes() -> Router<ControlState> {
    Router::new()
        .route("/api/v1/admin/auth/google/url", get(auth_google_url))
        .route("/api/v1/admin/auth/google/callback", get(auth_google_callback))
        // Body cap en las rutas de auth: rate limit + techo de 8 KB es el "bandwidth limit" del
        // login (mitiga abuso de ancho de banda en endpoints públicos).
        .route_layer(DefaultBodyLimit::max(8192))
        .route("/api/v1/admin/auth/logout", post(auth_logout))
        .route("/api/v1/admin/me", get(me))
        .route("/api/v1/admin/admins", get(list_admins).post(create_admin))
        .route(
            "/api/v1/admin/admins/{email}",
            patch(set_admin_active).delete(delete_admin),
        )
        .route("/api/v1/admin/clients", get(list_clients).post(create_client))
        .route(
            "/api/v1/admin/clients/{id}",
            get(get_client).put(update_client).delete(delete_client),
        )
        .route("/api/v1/admin/clients/{id}/rotate-token", post(rotate_client_token))
        .route("/api/v1/admin/clients/{id}/rotate-id", post(rotate_client_id))
}

/// Identidad autenticada del panel: sesión (`via: "session"`) o master token (`via: "master"`).
pub struct AdminIdentity {
    pub email: String,
    pub via_master: bool,
}

impl AdminIdentity {
    /// Actor para la auditoría: el email, o `"master_token"` cuando autoriza el master.
    fn actor(&self) -> &str {
        if self.via_master {
            "master_token"
        } else {
            &self.email
        }
    }
}

impl axum::extract::FromRequestParts<ControlState> for AdminIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &ControlState,
    ) -> Result<Self, Self::Rejection> {
        let verbose = state.modo_desarrollo;

        // CSRF por header: cuando CORS está activo (UI externa configurada), las mutaciones del
        // router admin exigen `X-Admin-UI`. SameSite=None hace que la cookie viaje en
        // cross-origin, así que la cookie sola ya no basta como defensa CSRF: el header no se
        // envía en un formulario/imagen cross-origin clásico. Aplica también al master token.
        // Con CORS desactivado (modo embebido) no se exige nada: comportamiento actual.
        // Las rutas de auth son GET de navegación (callback Google, url de login) y quedan fuera.
        if state.admin.settings().cors_enabled()
            && matches!(
                parts.method,
                axum::http::Method::POST
                    | axum::http::Method::PUT
                    | axum::http::Method::PATCH
                    | axum::http::Method::DELETE
            )
            && !parts.headers.contains_key("x-admin-ui")
        {
            return Err(ApiError::detailed(
                ProxyError::Forbidden {
                    reason: "falta el header X-Admin-UI (CSRF)".to_string(),
                },
                verbose,
            ));
        }

        // 1) Cookie de sesión (el camino normal).
        if let Some(session_id) = session_cookie(&parts.headers) {
            if let Some(email) = state.admin.resolve_session(&session_id).await {
                return Ok(Self {
                    email,
                    via_master: false,
                });
            }
        }

        // 2) Master token como Bearer (fallback de emergencia). El master SOLO autoriza este
        //    router: las rutas de cliente ni lo miran.
        if let Ok(token) = super::control_api::extract_bearer_token(&parts.headers) {
            if state.admin.verify_master_token(&token) {
                return Ok(Self {
                    email: "master_token".to_string(),
                    via_master: true,
                });
            }
        }

        Err(ApiError::detailed(
            ProxyError::Unauthorized {
                reason: "se requiere sesión de administrador o master token".to_string(),
            },
            verbose,
        ))
    }
}

/// Extrae el valor crudo de la cookie `teleproxy_admin`.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(COOKIE)?.to_str().ok()?;
    cookie_header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == "teleproxy_admin").then(|| value.to_string())
    })
}

/// Cabecera `Set-Cookie` de la sesión: HttpOnly, `Secure` solo si el callback de Google es
/// https (mismo criterio de transporte), y `SameSite` según `AdminSettings::cookie_same_site`
/// (`None` con UI externa, `Lax` en modo embebido). SameSite=None exige Secure: garantizado
/// porque la UI externa en HTTPS implica `GOOGLE_REDIRECT_URI` en https.
fn session_cookie_header(
    session_id: &str,
    ttl_seconds: u64,
    secure: bool,
    same_site: &str,
) -> String {
    let mut cookie = format!(
        "teleproxy_admin={session_id}; HttpOnly; SameSite={same_site}; Path=/; Max-Age={ttl_seconds}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn expired_cookie_header(secure: bool, same_site: &str) -> String {
    let mut cookie =
        format!("teleproxy_admin=; HttpOnly; SameSite={same_site}; Path=/; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn set_cookie(response: &mut Response, value: &str) {
    match HeaderValue::from_str(value) {
        Ok(header) => {
            response.headers_mut().insert(SET_COOKIE, header);
        }
        Err(e) => {
            tracing::error!(error = %e, "No se pudo construir la cabecera Set-Cookie");
        }
    }
}

fn check_login_rate(state: &ControlState, ip: &ClientIp) -> Result<(), ProxyError> {
    let settings = state.admin.settings();
    check_ip_rate_limit(
        &state.login_limiter,
        &ip.0,
        settings.login_rate_limit_requests,
        settings.login_rate_limit_window_seconds,
        "admin_login",
    )
}

fn check_admin_rate(state: &ControlState, ip: &ClientIp) -> Result<(), ProxyError> {
    let settings = state.admin.settings();
    check_ip_rate_limit(
        &state.admin_limiter,
        &ip.0,
        settings.admin_rate_limit_requests,
        settings.admin_rate_limit_window_seconds,
        "admin",
    )
}

fn api_error(state: &ControlState, error: ProxyError) -> ApiError {
    ApiError::detailed(error, state.modo_desarrollo)
}

// --- Auth ---

async fn auth_google_url(
    State(state): State<ControlState>,
    ip: ClientIp,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_login_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let url = state
        .admin
        .google_login_url()
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(Json(serde_json::json!({ "url": url })))
}

#[derive(Deserialize)]
struct GoogleCallbackQuery {
    code: String,
    state: String,
}

async fn auth_google_callback(
    State(state): State<ControlState>,
    ip: ClientIp,
    Query(query): Query<GoogleCallbackQuery>,
) -> Result<Response, ApiError> {
    // Es una ruta de navegador: los errores se renderizan como HTML mínimo, no como JSON.
    if let Err(error) = check_login_rate(&state, &ip) {
        return Ok(html_error_page(&state, error));
    }
    match state
        .admin
        .complete_google_login(&query.code, &query.state)
        .await
    {
        Ok(session) => {
            let settings = state.admin.settings();
            // 302 (found): navegación GET, el navegador sigue al destino con la cookie. Axum 0.8
            // no expone `Redirect::found`, así que se construye a mano.
            let location = settings.post_login_redirect();
            let mut response = (
                StatusCode::FOUND,
                [(
                    axum::http::header::LOCATION,
                    HeaderValue::from_str(&location).unwrap_or_else(|_| {
                        HeaderValue::from_static("/admin/")
                    }),
                )],
            )
                .into_response();
            set_cookie(
                &mut response,
                &session_cookie_header(
                    &session.session_id,
                    settings.session_ttl_seconds,
                    settings.cookie_secure,
                    settings.cookie_same_site(),
                ),
            );
            Ok(response)
        }
        Err(error) => Ok(html_error_page(&state, error)),
    }
}

async fn auth_logout(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    if !identity.via_master {
        if let Some(session_id) = session_cookie(&headers) {
            state
                .admin
                .destroy_session(&session_id)
                .await
                .map_err(|e| api_error(&state, e))?;
        }
    }
    let mut response = Json(serde_json::json!({ "ok": true })).into_response();
    let settings = state.admin.settings();
    set_cookie(
        &mut response,
        &expired_cookie_header(settings.cookie_secure, settings.cookie_same_site()),
    );
    Ok(response)
}

async fn me(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    Ok(Json(serde_json::json!({
        "email": identity.email,
        "role": "admin",
        "via": if identity.via_master { "master" } else { "session" },
    })))
}

/// Error como HTML mínimo para rutas de navegador (el callback de Google). Respeta la misma
/// regla de verbosidad que `ApiError`: 4xx con mensaje, 5xx escueto fuera de desarrollo.
fn html_error_page(state: &ControlState, error: ProxyError) -> Response {
    let status =
        StatusCode::from_u16(error.to_http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let message = if state.modo_desarrollo || !status.is_server_error() {
        error.to_string()
    } else {
        error.escueto_message().to_string()
    };
    log_domain_error(&error);

    let body = format!(
        "<!doctype html><html lang=\"es\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Error de autenticación</title></head><body>\
         <h1>Error {}</h1><p>{}</p><p><a href=\"/admin/\">Volver al panel</a></p></body></html>",
        status.as_u16(),
        escape_html(&message)
    );
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

/// Escapado mínimo para interpolar texto en HTML.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// --- Administradores ---

async fn list_admins(
    State(state): State<ControlState>,
    _identity: AdminIdentity,
    ip: ClientIp,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let admins = state.admin.list_admins().await.map_err(|e| api_error(&state, e))?;
    Ok(Json(serde_json::json!({ "admins": admins })))
}

#[derive(Deserialize)]
struct CreateAdminBody {
    email: String,
}

async fn create_admin(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    body: Result<Json<CreateAdminBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<crate::domain::models::AdminUserResponse>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let Json(body) = body.map_err(|rejection| {
        api_error(
            &state,
            ProxyError::InvalidConfig {
                field: "body".to_string(),
                reason: rejection.body_text(),
            },
        )
    })?;
    let actor = identity.actor().to_string();
    let admin = state
        .admin
        .create_admin(&body.email, &actor)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(Json(admin))
}

#[derive(Deserialize)]
struct SetAdminActiveBody {
    active: bool,
}

async fn set_admin_active(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    Path(email): Path<String>,
    body: Result<Json<SetAdminActiveBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<crate::domain::models::AdminUserResponse>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let Json(body) = body.map_err(|rejection| {
        api_error(
            &state,
            ProxyError::InvalidConfig {
                field: "body".to_string(),
                reason: rejection.body_text(),
            },
        )
    })?;
    let actor = identity.actor().to_string();
    let admin = state
        .admin
        .set_admin_active(&email, body.active, &actor)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(Json(admin))
}

async fn delete_admin(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    Path(email): Path<String>,
) -> Result<StatusCode, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let actor = identity.actor().to_string();
    state
        .admin
        .delete_admin(&email, &actor)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Clientes ---

/// Vista de cliente para el panel. snake_case como el resto de responses; `bearer_token_hash`
/// jamás se serializa y el código Lua solo aparece (`scripting_code`) en el GET individual —
/// los listados lo omiten.
#[derive(Serialize)]
struct AdminClientView {
    internal_id: String,
    crypt_id: String,
    kind: ClientKind,
    wildcard: bool,
    config_version: u64,
    whitelist: Vec<String>,
    rate_limit: RateLimitConfig,
    max_scripting_body_bytes: u64,
    scripting_enabled: bool,
    scripting_code_hash: String,
    scripting_expression: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripting_code: Option<String>,
    error_handling: ErrorHandlingConfig,
    header_rules: Vec<HeaderRule>,
}

impl AdminClientView {
    fn new(config: &ClientConfig, include_code: bool) -> Self {
        Self {
            internal_id: config.internal_id.clone(),
            crypt_id: config.crypt_id.clone(),
            kind: config.kind,
            wildcard: config.wildcard,
            config_version: config.config_version,
            whitelist: config.whitelist.clone(),
            rate_limit: config.rate_limit.clone(),
            max_scripting_body_bytes: config.max_scripting_body_bytes,
            scripting_enabled: config.scripting.enabled,
            scripting_code_hash: config.scripting.code_hash.clone(),
            scripting_expression: config.scripting.expression.clone(),
            scripting_code: include_code.then(|| config.scripting.code.clone()),
            error_handling: config.error_handling.clone(),
            header_rules: config.header_rules.clone(),
        }
    }
}

#[derive(Deserialize)]
struct ListClientsQuery {
    limit: Option<u64>,
    skip: Option<u64>,
}

async fn list_clients(
    State(state): State<ControlState>,
    _identity: AdminIdentity,
    ip: ClientIp,
    Query(query): Query<ListClientsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let (clients, total) = state
        .admin
        .list_clients(query.limit.unwrap_or(50), query.skip.unwrap_or(0))
        .await
        .map_err(|e| api_error(&state, e))?;
    let clients: Vec<AdminClientView> = clients
        .iter()
        .filter(|config| ensure_client_kind(config).is_ok())
        .map(|config| AdminClientView::new(config, false))
        .collect();
    Ok(Json(serde_json::json!({ "clients": clients, "total": total })))
}

async fn create_client(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    body: Option<Json<CreateClientBody>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let actor = identity.actor().to_string();
    let wildcard = body.and_then(|Json(body)| body.wildcard);
    let (config, token) = state
        .admin
        .create_client(&actor, wildcard)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(Json(serde_json::json!({
        "token": token,
        "client": AdminClientView::new(&config, false),
    })))
}

/// Body opcional de `POST /api/v1/admin/clients`: permite crear el cliente ya como wildcard.
#[derive(Deserialize)]
struct CreateClientBody {
    wildcard: Option<bool>,
}

async fn get_client(
    State(state): State<ControlState>,
    _identity: AdminIdentity,
    ip: ClientIp,
    Path(id): Path<String>,
) -> Result<Json<AdminClientView>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let config = state.admin.get_client(&id).await.map_err(|e| api_error(&state, e))?;
    Ok(Json(AdminClientView::new(&config, true)))
}

async fn update_client(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    Path(id): Path<String>,
    body: Result<Json<AdminClientUpdate>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let Json(update) = body.map_err(|rejection| {
        api_error(
            &state,
            ProxyError::InvalidConfig {
                field: "body".to_string(),
                reason: rejection.body_text(),
            },
        )
    })?;
    // El `ClientConfigUpdate` interno pasa por las mismas cotas que el `PUT` de la API de
    // cliente (se valida antes de escribir); `wildcard` es solo-admin y viaja aparte. El
    // `kind` no forma parte de ninguno de los dos payloads, así que no puede tocarse.
    validate_config_update(&update.config).map_err(|e| api_error(&state, e))?;
    let actor = identity.actor().to_string();
    let updated = state
        .admin
        .update_client(&id, &update, &actor)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(Json(serde_json::json!({
        "config_version": updated.config_version,
        "updated_at": chrono::Utc::now().to_rfc3339(),
    })))
}

async fn rotate_client_token(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let actor = identity.actor().to_string();
    let (config, token) = state
        .admin
        .rotate_client_token(&id, &actor)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(Json(serde_json::json!({
        "token": token,
        "client": AdminClientView::new(&config, false),
    })))
}

async fn rotate_client_id(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    Path(id): Path<String>,
) -> Result<Json<RotateIdResponse>, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let actor = identity.actor().to_string();
    let new_crypt_id = state
        .admin
        .rotate_client_id(&id, &actor)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(Json(RotateIdResponse {
        new_crypt_id,
        rotated_at: chrono::Utc::now().to_rfc3339(),
    }))
}

async fn delete_client(
    State(state): State<ControlState>,
    identity: AdminIdentity,
    ip: ClientIp,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    check_admin_rate(&state, &ip).map_err(|e| api_error(&state, e))?;
    let actor = identity.actor().to_string();
    state
        .admin
        .delete_client(&id, &actor)
        .await
        .map_err(|e| api_error(&state, e))?;
    Ok(StatusCode::NO_CONTENT)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cookie_se_extrae_del_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_static("otra=1; teleproxy_admin=abc_123; tercera=3"),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("abc_123"));

        let headers = HeaderMap::new();
        assert!(session_cookie(&headers).is_none());
    }

    #[test]
    fn cookie_de_sesion_lleva_httponly_secure_y_samesite_condicional() {
        let cookie = session_cookie_header("sid", 3600, false, "Lax");
        assert!(cookie.starts_with("teleproxy_admin=sid; HttpOnly; SameSite=Lax; Path=/"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(!cookie.contains("Secure"));

        let secure = session_cookie_header("sid", 3600, true, "Lax");
        assert!(secure.contains("; Secure"));

        // UI externa: SameSite=None para que la cookie viaje cross-origin (con Secure).
        let externa = session_cookie_header("sid", 3600, true, "None");
        assert!(externa.contains("SameSite=None"));
        assert!(externa.contains("; Secure"));

        let expired = expired_cookie_header(false, "Lax");
        assert!(expired.contains("Max-Age=0"));
    }

    #[test]
    fn vista_de_cliente_nunca_serializa_secrets() {
        let config = ClientConfig {
            id: "c1".to_string(),
            rev: None,
            r#type: "client_config".to_string(),
            internal_id: "c1".to_string(),
            crypt_id: "crypt_id_123".to_string(),
            bearer_token_hash: "sha256:secreto".to_string(),
            config_version: 1,
            kind: ClientKind::Client,
            wildcard: true,
            whitelist: vec!["example.com".to_string()],
            rate_limit: RateLimitConfig {
                max_requests: 50,
                window_seconds: 60,
            },
            max_scripting_body_bytes: 0,
            scripting: crate::domain::models::ScriptingConfig {
                enabled: true,
                code: "return body".to_string(),
                code_hash: "sha256:hash".to_string(),
                expression: String::new(),
            },
            error_handling: ErrorHandlingConfig {
                mode: crate::domain::models::ErrorMode::Transparent,
                fallback_urls: Default::default(),
            },
            header_rules: Vec::new(),
        };

        // Listado: sin código Lua y sin el hash del bearer token.
        let listed = serde_json::to_value(AdminClientView::new(&config, false))
            .expect("serializar vista de listado");
        assert!(listed.get("scripting_code").is_none());
        assert!(!listed.to_string().contains("secreto"));
        assert!(!listed.to_string().contains("return body"));
        assert_eq!(listed["kind"], "client");
        assert_eq!(listed["wildcard"], true);

        // GET individual: el código Lua sí está (los admins editan scripts), el hash del
        // bearer token sigue sin estar.
        let individual = serde_json::to_value(AdminClientView::new(&config, true))
            .expect("serializar vista individual");
        assert_eq!(individual["scripting_code"], "return body");
        assert!(!individual.to_string().contains("secreto"));
    }

    // --- Callback de Google y CSRF: requieren un ControlState de verdad ---

    use crate::application::admin_service::tests as fakes;
    use crate::application::admin_service::AdminService;
    use crate::application::identity::VerifiedIdentity;
    use crate::application::proxy_service::ProxyService;
    use crate::domain::services::{AdminRepository, CacheStore, ConfigFetcher, DnsResolver, LuaExecutor};
    use axum::extract::FromRequestParts;
    use axum::http::Request;
    use std::sync::Arc;

    /// Stubs mínimos para construir un `ProxyService` real: el extractor `AdminIdentity` y el
    /// callback no tocan el servicio del proxy, solo el `AdminService`.
    struct StubFetcher;
    #[async_trait::async_trait]
    impl ConfigFetcher for StubFetcher {
        async fn get_by_crypt_id(&self, _: &str) -> Result<ClientConfig, ProxyError> {
            unimplemented!("stub de test")
        }
        async fn get_by_token_hash(&self, _: &str) -> Result<ClientConfig, ProxyError> {
            unimplemented!("stub de test")
        }
        async fn update_config(
            &self,
            _: &str,
            _: &crate::domain::models::ClientConfigUpdate,
        ) -> Result<ClientConfig, ProxyError> {
            unimplemented!("stub de test")
        }
        async fn rotate_crypt_id(&self, _: &str) -> Result<String, ProxyError> {
            unimplemented!("stub de test")
        }
        async fn invalidate_cache(&self, _: &str) {}
    }

    struct StubCache;
    #[async_trait::async_trait]
    impl CacheStore for StubCache {
        async fn get_response(
            &self,
            _: &str,
        ) -> Result<Option<crate::domain::models::CachedResponse>, ProxyError> {
            unimplemented!("stub de test")
        }
        async fn set_response(
            &self,
            _: &str,
            _: &crate::domain::models::CachedResponse,
            _: u64,
        ) -> Result<(), ProxyError> {
            unimplemented!("stub de test")
        }
        async fn get_ip(&self, _: &str) -> Result<Option<std::net::IpAddr>, ProxyError> {
            unimplemented!("stub de test")
        }
        async fn set_ip(
            &self,
            _: &str,
            _: &std::net::IpAddr,
            _: u64,
        ) -> Result<(), ProxyError> {
            unimplemented!("stub de test")
        }
        async fn check_rate_limit(
            &self,
            _: &str,
            _: u32,
            _: u64,
        ) -> crate::domain::models::RateLimitDecision {
            unimplemented!("stub de test")
        }
    }

    struct StubDns;
    #[async_trait::async_trait]
    impl DnsResolver for StubDns {
        async fn resolve_and_validate(&self, _: &str) -> Result<std::net::IpAddr, ProxyError> {
            unimplemented!("stub de test")
        }
    }

    struct StubLua;
    #[async_trait::async_trait]
    impl LuaExecutor for StubLua {
        async fn execute(
            &self,
            _: &str,
            _: &[u8],
            _: &crate::domain::models::ProxyContext,
        ) -> Result<Vec<u8>, ProxyError> {
            unimplemented!("stub de test")
        }
    }

    fn test_control_state(mut settings: crate::application::admin_service::AdminSettings) -> ControlState {
        let repo = Arc::new(fakes::FakeRepo::default());
        repo.admins
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(fakes::admin("juan@gmail.com", true));
        settings.cookie_secure = true;
        let providers = fakes::google(VerifiedIdentity {
            email: "juan@gmail.com".to_string(),
            email_verified: true,
        });
        let admin = Arc::new(AdminService::new(
            repo as Arc<dyn crate::domain::services::AdminRepository>,
            Arc::new(fakes::FakeSessions::default()),
            providers,
            settings,
        ));
        ControlState {
            service: Arc::new(ProxyService::new(
                Arc::new(StubFetcher),
                Arc::new(StubCache),
                Arc::new(StubDns),
                Arc::new(StubLua),
            )),
            admin,
            login_limiter: Arc::new(crate::infrastructure::local_rate_limiter::LocalRateLimiter::new()),
            admin_limiter: Arc::new(crate::infrastructure::local_rate_limiter::LocalRateLimiter::new()),
            control_limiter: Arc::new(crate::infrastructure::local_rate_limiter::LocalRateLimiter::new()),
            modo_desarrollo: false,
        }
    }

    fn master_parts(method: &str, extra_headers: &[(&str, &str)]) -> axum::http::request::Parts {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .header(axum::http::header::AUTHORIZATION, "Bearer supersecret");
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(()).expect("request de prueba");
        request.into_parts().0
    }

    /// El callback devuelve `Result<Response, ApiError>`; ApiError no es Debug (lleva Span), así
    /// que se desempaqueta a mano para poder hacer assertions sobre el error.
    async fn callback_response(
        state: ControlState,
    ) -> Response {
        let url = state.admin.google_login_url().await.expect("url de login");
        let oauth_state = url.split("state=").nth(1).expect("state en la url").to_string();
        match auth_google_callback(
            State(state),
            ClientIp("10.0.0.1".to_string()),
            Query(GoogleCallbackQuery {
                code: "code".to_string(),
                state: oauth_state,
            }),
        )
        .await
        {
            Ok(response) => response.into_response(),
            Err(error) => {
                let status = error.into_response().status();
                panic!("el callback falló: status {status}");
            }
        }
    }

    #[tokio::test]
    async fn callback_redirige_a_admin_ui_url_con_samesite_none_cuando_hay_ui_externa() {
        let mut settings = fakes::settings(None);
        settings.admin_ui_url = Some("https://admteleproxy.velone.ai/".to_string());
        settings.cors_allowed_origins = vec!["https://admteleproxy.velone.ai".to_string()];

        let response = callback_response(test_control_state(settings)).await;

        assert_eq!(response.status(), StatusCode::FOUND);
        // Redirect a la UI externa (sin slash final) y cookie SameSite=None + Secure.
        assert_eq!(
            response.headers().get(axum::http::header::LOCATION).and_then(|v| v.to_str().ok()),
            Some("https://admteleproxy.velone.ai")
        );
        let cookie = response
            .headers()
            .get(SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("Set-Cookie del callback");
        assert!(cookie.contains("SameSite=None"), "{cookie}");
        assert!(cookie.contains("; Secure"), "{cookie}");
    }

    #[tokio::test]
    async fn callback_redirige_a_admin_embebido_con_samesite_lax_sin_ui_externa() {
        let response = callback_response(test_control_state(fakes::settings(None))).await;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers().get(axum::http::header::LOCATION).and_then(|v| v.to_str().ok()),
            Some("/admin/")
        );
        let cookie = response
            .headers()
            .get(SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("Set-Cookie del callback");
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    }

    #[tokio::test]
    async fn csrf_header_en_mutaciones_con_cors_activo() {
        let mut settings = fakes::settings(Some("supersecret"));
        settings.cors_allowed_origins = vec!["https://admteleproxy.velone.ai".to_string()];
        let state = test_control_state(settings);

        // Mutación sin X-Admin-UI → 403 `forbidden`, aunque el master token sea correcto.
        let mut parts = master_parts("POST", &[]);
        let rejection = match AdminIdentity::from_request_parts(&mut parts, &state).await {
            Ok(_) => panic!("sin header X-Admin-UI debe ser 403"),
            Err(error) => error,
        };
        assert_eq!(rejection.into_response().status(), StatusCode::FORBIDDEN);

        // Con el header → el master token autoriza.
        let mut parts = master_parts("POST", &[("x-admin-ui", "1")]);
        let identity = match AdminIdentity::from_request_parts(&mut parts, &state).await {
            Ok(identity) => identity,
            Err(error) => {
                let status = error.into_response().status();
                panic!("con header X-Admin-UI debe pasar: status {status}");
            }
        };
        assert!(identity.via_master);

        // GET no es mutación: no aplica (las rutas de auth de navegador siguen funcionando).
        let mut parts = master_parts("GET", &[]);
        assert!(AdminIdentity::from_request_parts(&mut parts, &state).await.is_ok());

        // Sin CORS activo (modo embebido) no se exige el header: comportamiento actual.
        let state = test_control_state(fakes::settings(Some("supersecret")));
        let mut parts = master_parts("POST", &[]);
        assert!(AdminIdentity::from_request_parts(&mut parts, &state).await.is_ok());
    }
}
