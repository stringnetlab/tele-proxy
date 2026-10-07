use std::str::FromStr;
use std::sync::Arc;

use axum::extract::State;
use axum::Router;
use tracing_subscriber::{fmt, EnvFilter};

use tele_proxy::application::admin_service::{AdminService, AdminSettings};
use tele_proxy::application::cache_key::sha256_hex;
use tele_proxy::application::changes_feed::ChangesFeedListener;
use tele_proxy::application::identity::{GoogleIdentityProvider, IdentityProviders};
use tele_proxy::application::lua_engine::SandboxedLuaEngine;
use tele_proxy::application::proxy_service::ProxyService;
use tele_proxy::application::webhook_service::{self, ValidatingWebhookFetcher};
use tele_proxy::domain::services::AdminRepository;
use tele_proxy::domain::validators::validate_allowed_domain_list;
use tele_proxy::infrastructure::couchdb_repo::CouchDbRepository;
use tele_proxy::infrastructure::dns_resolver::SecureDnsResolver;
use tele_proxy::infrastructure::local_rate_limiter::LocalRateLimiter;
use tele_proxy::infrastructure::valkey_cache::{ValkeyCacheStore, ValkeySessionStore};
use tele_proxy::interfaces::admin_api::admin_routes;
use tele_proxy::interfaces::admin_ui::admin_ui_routes;
use tele_proxy::interfaces::control_api::{control_routes, ControlState};
use tele_proxy::interfaces::proxy_handler::proxy_handler;

#[derive(Debug)]
struct AppConfig {
    http_proxy_port: u16,
    http_control_port: u16,
    http_host: String,
    couchdb_url: String,
    couchdb_user: String,
    couchdb_password: String,
    couchdb_db_name: String,
    valkey_url: String,
    config_cache_ttl: u64,
    config_cache_max_capacity: u64,
    lua_timeout_ms: u64,
    lua_memory_limit_mb: u32,
    webhook_timeout_ms: u64,
    /// `MODO=desarrollo`: errores HTTP verbosos con motivo interno y `details`. Cualquier otro
    /// valor (o ausente) mantiene los errores escuetos de producción.
    modo_desarrollo: bool,
    /// User-Agent por defecto para las peticiones al origen cuando el cliente no envía el
    /// suyo. Vacío = el User-Agent por defecto de reqwest.
    upstream_user_agent: String,
}

impl AppConfig {
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            http_proxy_port: env_var_or("HTTP_PROXY_PORT", "8080")?
                .parse()
                .map_err(|e| format!("Invalid HTTP_PROXY_PORT: {}", e))?,
            http_control_port: env_var_or("HTTP_CONTROL_PORT", "8081")?
                .parse()
                .map_err(|e| format!("Invalid HTTP_CONTROL_PORT: {}", e))?,
            http_host: env_var_or("HTTP_HOST", "0.0.0.0")?,
            couchdb_url: env_var_or("COUCHDB_URL", "http://couchdb:5984")?,
            couchdb_user: std::env::var("COUCHDB_USER")
                .map_err(|_| "COUCHDB_USER is required".to_string())?,
            couchdb_password: std::env::var("COUCHDB_PASSWORD")
                .map_err(|_| "COUCHDB_PASSWORD is required".to_string())?,
            couchdb_db_name: env_var_or("COUCHDB_DB_NAME", "tele_proxy_configs")?,
            valkey_url: env_var_or("VALKEY_URL", "redis://:valkeypass@valkey:6379")?,
            config_cache_ttl: env_var_or("CONFIG_CACHE_TTL_SECONDS", "300")?
                .parse()
                .map_err(|e| format!("Invalid CONFIG_CACHE_TTL_SECONDS: {}", e))?,
            config_cache_max_capacity: env_var_or("CONFIG_CACHE_MAX_CAPACITY", "10000")?
                .parse()
                .map_err(|e| format!("Invalid CONFIG_CACHE_MAX_CAPACITY: {}", e))?,
            lua_timeout_ms: parse_in_range("LUA_TIMEOUT_MS", "200", 1u64, 60_000u64)?,
            lua_memory_limit_mb: env_var_or("LUA_MEMORY_LIMIT_MB", "50")?
                .parse()
                .map_err(|e| format!("Invalid LUA_MEMORY_LIMIT_MB: {}", e))?,
            webhook_timeout_ms: parse_in_range("WEBHOOK_TIMEOUT_MS", "5000", 1u64, 60_000u64)?,
            modo_desarrollo: std::env::var("MODO")
                .map(|value| value.eq_ignore_ascii_case("desarrollo"))
                .unwrap_or(false),
            upstream_user_agent: env_var_or("UPSTREAM_USER_AGENT", "")?,
        })
    }
}

fn env_var_or(key: &str, default: &str) -> Result<String, String> {
    Ok(std::env::var(key).unwrap_or_else(|_| default.to_string()))
}

/// Variables de entorno de la administración global (§ Administración de `docs/ENVIRONMENT.md`).
/// Los numéricos usan `parse_in_range` (fuera de rango = el arranque falla). El master token se
/// guarda hasheado desde aquí: el valor en claro solo vive en el entorno del despliegue.
fn admin_settings_from_env(control_port: u16) -> Result<AdminSettings, String> {
    let master = std::env::var("MASTER_BEARER_TOKEN").unwrap_or_default();
    let master_token_hash = if master.trim().is_empty() {
        None
    } else {
        Some(format!("sha256:{}", sha256_hex(master.trim())))
    };

    let redirect_default = format!(
        "http://127.0.0.1:{control_port}/api/v1/admin/auth/google/callback"
    );
    let google_redirect_uri = env_var_or("GOOGLE_REDIRECT_URI", &redirect_default)?;
    let cookie_secure = google_redirect_uri.starts_with("https://");

    let allowed_domains = validate_allowed_domain_list(&env_var_or(
        "ADMIN_ALLOWED_DOMAINS",
        "gmail.com,stringnet.pe",
    )?)
    .map_err(|e| e.to_string())?;

    Ok(AdminSettings {
        master_token_hash,
        allowed_domains,
        session_ttl_seconds: parse_in_range(
            "ADMIN_SESSION_TTL_SECONDS",
            "3600",
            300u64,
            86_400u64,
        )?,
        oauth_state_ttl_seconds: parse_in_range(
            "ADMIN_OAUTH_STATE_TTL_SECONDS",
            "300",
            60u64,
            3_600u64,
        )?,
        login_rate_limit_requests: parse_in_range(
            "ADMIN_LOGIN_RATE_LIMIT_REQUESTS",
            "5",
            1u32,
            100u32,
        )?,
        login_rate_limit_window_seconds: parse_in_range(
            "ADMIN_LOGIN_RATE_LIMIT_WINDOW_SECONDS",
            "60",
            1u64,
            3_600u64,
        )?,
        admin_rate_limit_requests: parse_in_range("ADMIN_RATE_LIMIT_REQUESTS", "120", 1u32, 10_000u32)?,
        admin_rate_limit_window_seconds: parse_in_range(
            "ADMIN_RATE_LIMIT_WINDOW_SECONDS",
            "60",
            1u64,
            3_600u64,
        )?,
        control_rate_limit_requests: parse_in_range(
            "CONTROL_RATE_LIMIT_REQUESTS",
            "60",
            1u32,
            10_000u32,
        )?,
        control_rate_limit_window_seconds: parse_in_range(
            "CONTROL_RATE_LIMIT_WINDOW_SECONDS",
            "60",
            1u64,
            3_600u64,
        )?,
        cookie_secure,
    })
}

/// `docs/ENVIRONMENT.md` § Validaciones: fuera de rango el arranque **falla**, no se degrada al
/// default — un `LUA_TIMEOUT_MS` de 600000 escrito por error no puede convertir el sandbox en un
/// callejón sin salida silencioso. Las activas hoy son los dos deadlines del sandbox
/// (`LUA_TIMEOUT_MS`, `WEBHOOK_TIMEOUT_MS`); las `MAX_*` y las `PROXY_*` siguen *designadas*, así
/// que su rango se empieza a validar en la fase que cablea su lectura (Fase 3/4).
fn parse_in_range<T>(key: &str, default: &str, min: T, max: T) -> Result<T, String>
where
    T: FromStr + PartialOrd + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    let raw = env_var_or(key, default)?;
    let value: T = raw.parse().map_err(|e| format!("Invalid {key}: {e}"))?;
    if value < min || value > max {
        return Err(format!("Invalid {key}: {value} is outside {min}..={max}"));
    }
    Ok(value)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,tele_proxy=info"));

    fmt()
        .json()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();
}

/// `GET /` en el puerto del proxy. En producción solo el nombre del proyecto; en
/// `MODO=desarrollo` también la lista de endpoints definidos, para descubrir la API a mano.
async fn index(State(service): State<Arc<ProxyService>>) -> String {
    index_body(service.verbose_errors())
}

fn index_body(verbose: bool) -> String {
    const NAME: &str = "TELE - PROXY";
    if !verbose {
        return NAME.to_string();
    }
    let endpoints = [
        "",
        "Endpoints del proxy (puerto HTTP_PROXY_PORT):",
        "  GET  /aq/{crypt_id}/?url=<url>[&mime=<mime>]   Proxy publico multi-cliente",
        "  GET  /health                                   Healthcheck del contenedor",
        "  GET  /                                         Este indice",
        "",
        "API de control (puerto HTTP_CONTROL_PORT, loopback, auth Bearer):",
        "  GET  /api/v1/clients/config                    Configuracion actual (sin secretos)",
        "  PUT  /api/v1/clients/config                    Actualizacion parcial validada",
        "  POST /api/v1/clients/rotate-id                 Rota el crypt_id",
        "  GET  /health                                   Healthcheck",
        "",
        "Administracion (mismo puerto, cookie de sesion o master token):",
        "  GET  /admin/                                   Panel de administracion",
        "  GET  /api/v1/admin/auth/google/url             URL de login Google",
        "  GET  /api/v1/admin/me                          Identidad autenticada",
        "  GET/POST /api/v1/admin/clients                 Listar/crear clientes",
        "  GET/POST /api/v1/admin/admins                  Listar/anadir administradores",
    ]
    .join("\n");
    format!("{NAME}\n{endpoints}\n")
}

async fn healthcheck() -> &'static str {
    "OK"
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    init_tracing();

    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            // `init_tracing()` ya corrió, así que el fallo de arranque sale por el mismo canal
            // estructurado que el resto de logs; un `eprintln!` aquí se perdería en el agregador.
            tracing::error!(error = %e, "Error de configuración, abortando el arranque");
            std::process::exit(1);
        }
    };

    tracing::info!(
        proxy_port = config.http_proxy_port,
        control_port = config.http_control_port,
        modo = if config.modo_desarrollo {
            "desarrollo"
        } else {
            "produccion"
        },
        "Iniciando tele-proxy"
    );

    let config_fetcher = Arc::new(CouchDbRepository::new(
        config.couchdb_url.clone(),
        config.couchdb_db_name.clone(),
        config.couchdb_user.clone(),
        config.couchdb_password.clone(),
        config.config_cache_ttl,
        config.config_cache_max_capacity,
    ));

    if let Err(e) = config_fetcher.ensure_design_doc().await {
        tracing::warn!(error = %e, "No se pudo crear el design doc de CouchDB (se reintentará en la primera petición)");
    }

    if let Err(e) = config_fetcher.seed_demo_client_if_empty(config.modo_desarrollo).await {
        tracing::warn!(error = %e, "No se pudo sembrar el cliente demo");
    }

    let cache_store = match ValkeyCacheStore::new(&config.valkey_url).await {
        Ok(store) => Arc::new(store),
        Err(e) => {
            tracing::error!(error = %e, "No se pudo inicializar el almacén de caché de Valkey");
            std::process::exit(1);
        }
    };

    let valkey_degraded = !cache_store.is_connected();
    if valkey_degraded {
        tracing::warn!(
            "Valkey no disponible en el arranque — caché y rate limiting deshabilitados"
        );
    }

    let dns_config_json = include_str!("../config/dns_resolvers.json");
    let dns_resolver = Arc::new(
        SecureDnsResolver::from_json(dns_config_json).expect("Failed to load DNS resolver config"),
    );

    // El webhook del sandbox Lua comparte el resolver validado con el proxy: `docs/spec.md` §
    // Pipeline anti-SSRF compartido prohíbe que una segunda ruta de salida resuelva por su cuenta.
    let webhook_fetcher = Arc::new(ValidatingWebhookFetcher::new(
        Arc::clone(&dns_resolver) as Arc<_>,
        config.webhook_timeout_ms,
        webhook_service::MAX_RESPONSE_BYTES,
    ));

    let lua_executor = Arc::new(SandboxedLuaEngine::new(
        config.lua_timeout_ms,
        config.lua_memory_limit_mb,
        config.webhook_timeout_ms,
        webhook_fetcher,
    ));

    let proxy_service = Arc::new(
        ProxyService::new(
            Arc::clone(&config_fetcher) as Arc<_>,
            Arc::clone(&cache_store) as Arc<dyn tele_proxy::domain::services::CacheStore>,
            Arc::clone(&dns_resolver) as Arc<_>,
            lua_executor,
        )
        .with_degraded_mode(valkey_degraded)
        .with_verbose_errors(config.modo_desarrollo)
        .with_upstream_user_agent(config.upstream_user_agent),
    );

    // --- Administración global (panel /api/v1/admin + /admin/) ---
    let admin_settings = match admin_settings_from_env(config.http_control_port) {
        Ok(settings) => settings,
        Err(e) => {
            tracing::error!(error = %e, "Error de configuración de administración, abortando el arranque");
            std::process::exit(1);
        }
    };

    let mut identity_providers = IdentityProviders::new();
    let google_client_id = std::env::var("GOOGLE_CLIENT_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let google_client_secret = std::env::var("GOOGLE_CLIENT_SECRET")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match (google_client_id, google_client_secret) {
        (Some(client_id), Some(client_secret)) => {
            let redirect_uri = std::env::var("GOOGLE_REDIRECT_URI")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "http://127.0.0.1:{}/api/v1/admin/auth/google/callback",
                        config.http_control_port
                    )
                });
            match GoogleIdentityProvider::new(client_id, client_secret, redirect_uri) {
                Ok(provider) => {
                    identity_providers.register(Arc::new(provider));
                    tracing::info!("Login de administración con Google habilitado");
                }
                Err(e) => {
                    tracing::error!(error = %e, "No se pudo construir el proveedor de Google; login Google deshabilitado (queda el master token)");
                }
            }
        }
        _ => {
            tracing::warn!("GOOGLE_CLIENT_ID/GOOGLE_CLIENT_SECRET ausentes: login Google deshabilitado; la API admin funciona solo con master token");
        }
    }

    let admin_repo: Arc<dyn AdminRepository> = config_fetcher.clone();
    let session_store = Arc::new(ValkeySessionStore::new(cache_store.connection_manager()));
    let admin_service = Arc::new(AdminService::new(
        admin_repo,
        session_store,
        identity_providers,
        admin_settings,
    ));

    let changes_listener = ChangesFeedListener::new(
        config.couchdb_url,
        config.couchdb_db_name,
        config.couchdb_user,
        config.couchdb_password,
        Arc::clone(&config_fetcher) as Arc<_>,
    );

    let changes_task = tokio::spawn(async move {
        changes_listener.run().await;
    });

    let proxy_app = Router::new()
        .route("/aq/{crypt_id}/", axum::routing::get(proxy_handler))
        .route("/health", axum::routing::get(healthcheck))
        .route("/", axum::routing::get(index))
        .with_state(Arc::clone(&proxy_service));

    let control_state = ControlState {
        service: Arc::clone(&proxy_service),
        admin: admin_service,
        login_limiter: Arc::new(LocalRateLimiter::new()),
        admin_limiter: Arc::new(LocalRateLimiter::new()),
        control_limiter: Arc::new(LocalRateLimiter::new()),
        modo_desarrollo: config.modo_desarrollo,
    };

    let control_app = control_routes()
        .merge(admin_routes())
        .merge(admin_ui_routes())
        .merge(Router::new().route("/health", axum::routing::get(healthcheck)))
        .with_state(control_state);

    let proxy_addr = format!("{}:{}", config.http_host, config.http_proxy_port);
    let control_addr = format!("{}:{}", config.http_host, config.http_control_port);

    tracing::info!(addr = %proxy_addr, "Iniciando listener del proxy");
    tracing::info!(addr = %control_addr, "Iniciando listener de la API de control");

    let proxy_listener = match tokio::net::TcpListener::bind(&proxy_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %proxy_addr, error = %e, "No se pudo bindear el listener del proxy");
            std::process::exit(1);
        }
    };

    let control_listener = match tokio::net::TcpListener::bind(&control_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %control_addr, error = %e, "No se pudo bindear el listener de control");
            std::process::exit(1);
        }
    };

    let proxy_server = tokio::spawn(async move {
        if let Err(e) = axum::serve(proxy_listener, proxy_app).await {
            tracing::error!(error = %e, "Error del servidor del proxy");
        }
    });

    let control_server = tokio::spawn(async move {
        if let Err(e) = axum::serve(control_listener, control_app).await {
            tracing::error!(error = %e, "Error del servidor de la API de control");
        }
    });

    tokio::select! {
        _ = proxy_server => tracing::error!("El servidor del proxy terminó inesperadamente"),
        _ = control_server => tracing::error!("El servidor de la API de control terminó inesperadamente"),
        _ = changes_task => tracing::error!("El listener del feed de cambios terminó inesperadamente"),
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Señal de apagado recibida");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::index_body;

    #[test]
    fn index_en_produccion_es_solo_el_nombre() {
        let body = index_body(false);
        assert_eq!(body, "TELE - PROXY");
        assert!(!body.contains("/aq/"), "{body}");
    }

    #[test]
    fn index_en_desarrollo_lista_los_endpoints() {
        let body = index_body(true);
        assert!(body.starts_with("TELE - PROXY\n"), "{body}");
        for endpoint in [
            "/aq/{crypt_id}/",
            "/api/v1/clients/config",
            "/api/v1/clients/rotate-id",
            "/health",
        ] {
            assert!(body.contains(endpoint), "falta {endpoint} en:\n{body}");
        }
    }
}
