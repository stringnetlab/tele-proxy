use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;

use crate::interfaces::control_api::ControlState;

/// UI mínima del panel, un solo archivo vanilla (sin frameworks ni CDN: CSP-friendly) embebido
/// con `include_str!`. Funcional, no decorativa: tabla de clientes con paginación, CRUD de
/// admins y login Google (con fallback de master token en localStorage).
const ADMIN_INDEX_HTML: &str = include_str!("../assets/admin/index.html");

pub fn admin_ui_routes() -> Router<ControlState> {
    Router::new()
        .route("/admin", get(redirect_a_admin))
        .route("/admin/", get(admin_index))
}

async fn redirect_a_admin() -> Redirect {
    Redirect::temporary("/admin/")
}

async fn admin_index() -> Response {
    let mut response = Html(ADMIN_INDEX_HTML).into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        axum::http::header::HeaderName::from_static("x-content-type-options"),
        axum::http::HeaderValue::from_static("nosniff"),
    );
    // CSP acorde al single-file: sin CDNs, con inline permitido solo para estilo y script propio.
    headers.insert(
        axum::http::header::HeaderName::from_static("content-security-policy"),
        axum::http::HeaderValue::from_static(
            "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'",
        ),
    );
    response
}
