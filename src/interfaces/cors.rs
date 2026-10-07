use axum::http::{header, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Cache del preflight (OPTIONS) en segundos.
const PREFLIGHT_MAX_AGE_SECS: u64 = 600;

/// Capa CORS del listener de control para la UI externa (proyecto SvelteKit en otro origen).
/// `None` cuando la allowlist está vacía: **sin capa y sin cabeceras CORS**, exactamente el
/// comportamiento anterior (retrocompatible). Con lista: orígenes exactos ya validados por
/// `validate_cors_allowed_origins` (nunca `*` — además incompatible con credenciales),
/// `Access-Control-Allow-Credentials: true` (la cookie de sesión viaja cross-origin) y el
/// preflight cacheado 600 s. El CSRF en mutaciones se refuerza aparte con el header
/// `X-Admin-UI` (extractor `AdminIdentity`), no con el esquema de cookies.
pub fn cors_layer(allowed_origins: &[String]) -> Option<CorsLayer> {
    if allowed_origins.is_empty() {
        return None;
    }
    let origins: Vec<HeaderValue> = allowed_origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect();
    if origins.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_credentials(true)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
                header::HeaderName::from_static("x-admin-ui"),
            ])
            .max_age(std::time::Duration::from_secs(PREFLIGHT_MAX_AGE_SECS)),
    )
}

#[cfg(test)]
mod tests {
    use super::cors_layer;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn app(origins: &[&str]) -> Router {
        let origins: Vec<String> = origins.iter().map(|origin| origin.to_string()).collect();
        let layer = cors_layer(&origins).expect("CORS activo con lista no vacía");
        Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer(layer)
    }

    fn preflight(origin: &str) -> Request<axum::body::Body> {
        Request::builder()
            .method("OPTIONS")
            .uri("/x")
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .body(axum::body::Body::empty())
            .expect("preflight de prueba")
    }

    #[tokio::test]
    async fn preflight_de_origen_allowlistado_recibe_cabeceras_cors() {
        let response = app(&["https://admteleproxy.velone.ai"])
            .oneshot(preflight("https://admteleproxy.velone.ai"))
            .await
            .expect("respuesta de preflight");

        // tower-http responde el preflight exitoso con 200 y cuerpo vacío.
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://admteleproxy.velone.ai")
        );
        assert_eq!(
            headers
                .get("access-control-allow-credentials")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert!(headers.get("access-control-allow-methods").is_some());
        assert!(headers
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("x-admin-ui")));
        assert_eq!(
            headers
                .get("access-control-max-age")
                .and_then(|value| value.to_str().ok()),
            Some("600")
        );
    }

    #[tokio::test]
    async fn origen_no_listado_no_recibe_allow_origin() {
        let response = app(&["https://admteleproxy.velone.ai"])
            .oneshot(preflight("https://evil.example"))
            .await
            .expect("respuesta de preflight");

        // El preflight sigue respondiendo, pero sin Access-Control-Allow-Origin: el navegador
        // bloquea la petición real.
        assert!(response.status().is_success());
        assert!(response.headers().get("access-control-allow-origin").is_none());
    }

    #[test]
    fn lista_vacia_no_monta_capa_cors() {
        assert!(cors_layer(&[]).is_none());
        let origins: Vec<String> = Vec::new();
        assert!(cors_layer(&origins).is_none());
    }
}
