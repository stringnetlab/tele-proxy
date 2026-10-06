use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use tracing_subscriber::{fmt, EnvFilter};

use tele_proxy::application::changes_feed::ChangesFeedListener;
use tele_proxy::application::lua_engine::SandboxedLuaEngine;
use tele_proxy::application::proxy_service::ProxyService;
use tele_proxy::application::webhook_service::{self, ValidatingWebhookFetcher};
use tele_proxy::infrastructure::couchdb_repo::CouchDbRepository;
use tele_proxy::infrastructure::dns_resolver::SecureDnsResolver;
use tele_proxy::infrastructure::valkey_cache::ValkeyCacheStore;
use tele_proxy::interfaces::control_api::control_routes;
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
        })
    }
}

fn env_var_or(key: &str, default: &str) -> Result<String, String> {
    Ok(std::env::var(key).unwrap_or_else(|_| default.to_string()))
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

async fn index() -> &'static str {
    "TELE - PROXY"
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
            tracing::error!(error = %e, "Configuration error, aborting startup");
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
        "Starting tele-proxy"
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
        tracing::warn!(error = %e, "Failed to create CouchDB design doc (will retry on first request)");
    }

    if let Err(e) = config_fetcher.seed_demo_client_if_empty().await {
        tracing::warn!(error = %e, "Failed to seed demo client");
    }

    let cache_store = match ValkeyCacheStore::new(&config.valkey_url).await {
        Ok(store) => Arc::new(store),
        Err(e) => {
            tracing::error!(error = %e, "Failed to initialize Valkey cache store");
            std::process::exit(1);
        }
    };

    let valkey_degraded = !cache_store.is_connected();
    if valkey_degraded {
        tracing::warn!("Valkey unavailable at startup — cache and rate limiting disabled");
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
            cache_store,
            Arc::clone(&dns_resolver) as Arc<_>,
            lua_executor,
        )
        .with_degraded_mode(valkey_degraded)
        .with_verbose_errors(config.modo_desarrollo),
    );

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

    let control_app = control_routes()
        .merge(Router::new().route("/health", axum::routing::get(healthcheck)))
        .with_state(Arc::clone(&proxy_service));

    let proxy_addr = format!("{}:{}", config.http_host, config.http_proxy_port);
    let control_addr = format!("{}:{}", config.http_host, config.http_control_port);

    tracing::info!(addr = %proxy_addr, "Proxy listener starting");
    tracing::info!(addr = %control_addr, "Control API listener starting");

    let proxy_listener = match tokio::net::TcpListener::bind(&proxy_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %proxy_addr, error = %e, "Failed to bind proxy listener");
            std::process::exit(1);
        }
    };

    let control_listener = match tokio::net::TcpListener::bind(&control_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %control_addr, error = %e, "Failed to bind control listener");
            std::process::exit(1);
        }
    };

    let proxy_server = tokio::spawn(async move {
        if let Err(e) = axum::serve(proxy_listener, proxy_app).await {
            tracing::error!(error = %e, "Proxy server error");
        }
    });

    let control_server = tokio::spawn(async move {
        if let Err(e) = axum::serve(control_listener, control_app).await {
            tracing::error!(error = %e, "Control API server error");
        }
    });

    tokio::select! {
        _ = proxy_server => tracing::error!("Proxy server exited unexpectedly"),
        _ = control_server => tracing::error!("Control API server exited unexpectedly"),
        _ = changes_task => tracing::error!("Changes feed listener exited unexpectedly"),
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown signal received");
        }
    }
}
