# Guía de Estilo y Reglas de Codificación Rust

Este documento establece las convenciones de codificación para `tele-proxy`. Todo el código debe adherirse a estas reglas.

## 1. Manejo de Errores

### Prohibido

```rust
// ❌ NUNCA usar unwrap() en código de producción
let value = some_option.unwrap();
let result = some_result.unwrap();

// ❌ NUNCA usar expect() en código de producción
let value = some_option.expect("should exist");
```

### Requerido

```rust
// ✅ Usar el operador ? para propagar errores
let config = fetch_config(client_id).await?;

// ✅ Usar match o if let para manejo explícito
match result {
    Ok(value) => process(value),
    Err(e) => {
        tracing::error!(error = %e, "Failed to process");
        // la variante vive en ERROR_DICTIONARY.md: no se inventan variantes sobre la marcha
        return Err(ProxyError::Internal {
            reason: format!("procesamiento del cuerpo: {e}"),
        });
    }
}

// ✅ Usar ok_or() para convertir Option a Result
let value = option.ok_or(ProxyError::ConfigNotFound {
    internal_id: internal_id.clone(),
})?;
```

### Excepciones

Solo se permite `.unwrap()` en:

- Código de pruebas (`#[cfg(test)]`)
- Constantes conocidas en tiempo de compilación (`Duration::from_secs(30)` dentro de `Lazy`/`const`)

`main.rs` es la **única** zona tolerada (arranque, donde el fallo debe ser fatal), pero incluso ahí
se prefiere `?` + salida con código distinto de cero imprimiendo la causa: un `unwrap()` en el
arranque produce un `panic` que Dokploy y el `HEALTHCHECK` no distinguen de un fallo de red.

### Mapeo hacia Pingora (obligatorio)

Los callbacks de `ProxyHttp` devuelven `pingora_core::Result<T>`, que es `Result<T, BError>` con
`pub type BError = Box<pingora_error::Error>`. El puente es explícito y **una sola vez**, en
`src/application/proxy_service.rs`: `domain/` no importa `pingora` (regla de dependencias de §7/§8).

```rust
use pingora_core::prelude::*;   // Error, BError, ErrorType::*, ErrorSource, Result

/// `ErrorSource` es un tipo de Pingora: `domain/` no puede devolverlo, así que la derivación
/// vive junto al mapeo, en `application/proxy_service.rs`.
fn pingora_source(e: &ProxyError) -> ErrorSource {
    if matches!(e, ProxyError::UpstreamError { .. } | ProxyError::DnsResolutionFailed { .. }) {
        ErrorSource::Upstream
    } else if e.is_client_error() {
        ErrorSource::Downstream
    } else {
        ErrorSource::Internal
    }
}

// ✅ dominio -> Pingora: el error_code y el status sobreviven el mapeo
impl From<ProxyError> for BError {
    fn from(e: ProxyError) -> BError {
        // CustomCode(&'static str, u16): código del diccionario + status, y la causa encadenada
        let etype = ErrorType::new_code(e.to_error_code(), e.to_http_status());
        Error::create(etype, pingora_source(&e), Some(e.to_string().into()), Some(Box::new(e)))
    }
}

// ❌ NUNCA aplane un error del dominio en un 500 genérico
return Err(Error::new(ErrorType::InternalError));
```

Reglas derivadas (verificadas contra el fuente de `pingora-error`/`pingora-proxy` del rev pineado en `Cargo.lock`):

- `Error::because(..)` deja `esource = Unset`, y el default de `fail_to_proxy` convierte `Unset` en 500.
  Por eso se usa `Error::create` con `ErrorSource` explícito.
- El default de `fail_to_proxy` solo respeta `ErrorType::HTTPStatus(code)`; `CustomCode` cae en las
  reglas de `esource`. Se **sobrescribe** `fail_to_proxy` y el status se recupera del `ProxyError`
  encadenado: `e.root_cause().downcast_ref::<ProxyError>()`.
- `session.respond_error(code)` responde HTML pregenerado sin `Retry-After`: el contrato JSON exige
  escribir la respuesta a mano (`ResponseHeader::build` + `write_response_header` + `write_response_body`).
- `request_filter` devuelve `Ok(true)` para **cortar** el flujo después de haber escrito la respuesta
  él mismo, nunca `Err` para un 4xx esperado. `early_request_filter` devuelve `Result<()>`: su único
  modo de detener el request es un `Err`, que por la regla anterior queda reservado a fallos del
  motor. En este proyecto se deja sin implementar.
- Un `Err` desde un callback solo se usa para fallos del **motor** (conexión, serie, IO) o cuando el
  cuerpo ya empezó a fluir y no se puede cambiar el status.
- `ProxyError -> to_http_status() -> JSON` sigue siendo la única fuente de verdad del status.

## 2. Observabilidad

### Prohibido

```rust
// ❌ NUNCA usar println! o eprintln!
println!("Processing request");
eprintln!("Error occurred");

// ❌ NUNCA usar log::info! directamente
log::info!("Request received");
```

### Requerido

```rust
// ✅ Usar tracing con campos estructurados
tracing::info!(
    crypt_id = %crypt_id,
    url = %url,
    "Processing request"
);

// ✅ el span del request se crea UNA sola vez, en request_filter, y viaja en el CTX.
//    Un span solo es "current" mientras está entrado en el thread, y Pingora invoca cada
//    callback en un poll distinto: `#[tracing::instrument]` por callback NO propagaria
//    `crypt_id` a los logs de los demás callbacks ni a `logging()`.
//    No se usa `early_request_filter`: corre antes de los módulos downstream (control de
//    acceso / rate limit) y el propio doc de Pingora pide dejar la lógica en `request_filter`
//    siempre que pueda (`proxy_trait.rs:118-120`).
async fn request_filter(
    &self,
    session: &mut Session,
    ctx: &mut Self::CTX,
) -> pingora_core::Result<bool>
where
    Self::CTX: Send + Sync,   // el trait lo declara asi (`proxy_trait.rs:105-107`); el `impl` debe repetirlo
{
    // ya se parseo `/aq/{crypt_id}/` y `ctx.crypt_id` / `internal_id` / `config_version` estan llenos
    ctx.span = tracing::info_span!(
        "proxy_request",
        crypt_id = %ctx.crypt_id,
        internal_id = %ctx.internal_id,
        config_version = ctx.config_version,
        ip_address = %session.client_addr().map(|a| a.ip().to_string()).unwrap_or_default(),
    );
    // `Ok(false)` continua hacia el upstream; `Ok(true)` corta despues de haber
    // escrito la respuesta con `write_response_header`.
    Ok(false)
}

// ✅ y se entra en el punto de emisión (`client_addr()` viene por el `Deref` al Session de core)
ctx.span.in_scope(|| {
    tracing::warn!(event = "ssrf_blocked", url = %url, resolved_ip = %ip, "SSRF attempt blocked");
});

// ✅ Logs de seguridad con niveles apropiados
tracing::warn!(
    event = "ssrf_blocked",
    crypt_id = %crypt_id,
    url = %url,
    resolved_ip = %ip,
    "SSRF attempt blocked"
);

tracing::error!(
    event = "lua_sandbox_violation",
    crypt_id = %crypt_id,
    attempted_function = %func_name,
    "Lua sandbox violation"
);
```

### Requerido en Pingora

El log **por solicitud** vive en el callback `logging()`, no disperso en los filtros, para que
lleve el resultado final del proxy (status, bytes, reintentos, error):

```rust
// ✅ una línea estructurada por request, dentro del span del request, con el error del motor si lo hubo
async fn logging(&self, session: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX)
where
    Self::CTX: Send + Sync,
{
    let error = match e {
        Some(err) => err.to_string(),
        None => "none".to_string(),
    };
    let req = session.req_header();   // `Deref` al Session de pingora-core: `req_header()` existe
    ctx.span.in_scope(|| {
        // `crypt_id`, `internal_id` y `ip_address` NO se repiten aqui: los aporta el span
        tracing::info!(
            event = "request_completed",
            method = %req.method,
            path = %req.uri.path(),
            status = session.response_written().map_or(0, |h| h.status.as_u16()),
            bytes = session.body_bytes_sent(),
            x_cache = ctx.cache.as_str(),
            elapsed_ms = ctx.inicio.elapsed().as_millis() as u64,
            error = %error,
            "proxy request finished"
        );
    });
}
```

Firmas y accesores verificados contra el rev pineado — `git+…/pingora?rev=4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19`, declarado en `Cargo.toml` y que `Cargo.lock` repite en el `source` de cada crate `pingora-*` —, no contra crates.io:

- El trait es `ProxyHttp<DS = ()>` (`proxy_trait.rs:48`) y sus callbacks declaran `Session<DS>`. Este
  proyecto **no** parametriza `DS`: `impl ProxyHttp for ProxyService` usa el default `DS = ()`
  (`pingora-core/src/protocols/http/custom/server.rs:33` + `impl Session for ()` en `:191`), que es
  lo que exige `http_proxy_service<SV>(..) where SV: ProxyHttp` (`pingora-proxy/src/lib.rs:1730`).
  Por tanto en el `impl` se escribe `session: &mut Session` (alias de `Session<()>`); el `<DS>` de
  las firmas citadas abajo pertenece a la declaración del trait.
- `async fn logging(&self, _session: &mut Session<DS>, _e: Option<&Error>, _ctx: &mut Self::CTX)`
  (`proxy_trait.rs:529`); el doc de Pingora avisa que **él ya emite un log de error** si el request
  falló (`:527`): `logging()` es el access log (INFO), no un segundo registro del error. El duplicado
  se corta con `fn suppress_error_log(&self, &Session<DS>, &Self::CTX, &Error) -> bool`
  (`proxy_trait.rs:571`, default `false`): se devuelve `true` cuando el `Error` viene de un
  `ProxyError` propio, porque ese ya se registró con su `error_code`.
- El `Session` de `pingora-proxy` hace `Deref` al `Session` de `pingora-core`, de ahí que existan
  `response_written() -> Option<&ResponseHeader>` (`server.rs:549`) y `body_bytes_sent() -> usize`
  (`server.rs:853`).
- **No existe** `Session::upstream_response_status()` en el rev: el status final se lee de la respuesta
  ya escrita (`h.status`, `ResponseHeader` se `Deref` a `http::response::Parts`). Si se necesita el
  status *del origen* antes de reescribir la respuesta, se guarda en `ProxyCtx` desde
  `upstream_response_filter`.
- `ctx.cache` es el `CacheDirective` del CTX y se registra con `ctx.cache.as_str()`: el mismo
  texto (`HIT`/`MISS`/`FALLBACK`/`BYPASS`) que `response_filter` escribe en el header `x-cache`.
  La decisión se lee del CTX, no re-leyendo la respuesta ya enviada.

- Todo log de seguridad/errores lleva `event = "<nombre_del_diccionario>"` y los campos que la
  entrada del diccionario prescribe (`crypt_id`, `url`, `reason`, ...).
- Nunca loguee un token, el `Authorization` completo ni el valor de una cabecera de credencial.

## 3. Concurrencia y Tokio

### Reglas

```rust
// ✅ Operaciones de bloqueo en spawn_blocking
let result = tokio::task::spawn_blocking(move || {
    // Código síncrono pesado (Lua, hashing, etc.)
    expensive_computation()
})
.await
.map_err(|e| ProxyError::Internal { reason: format!("spawn_blocking join: {e}") })?;

// ✅ Usar RwLock para lecturas frecuentes, escrituras raras
use tokio::sync::RwLock;
let config = Arc::new(RwLock::new(config));

// ✅ Usar Mutex para estado mutable compartido
use tokio::sync::Mutex;
let counter = Arc::new(Mutex::new(0u64));

// ❌ NUNCA bloquear el reactor de Tokio
async fn bad_example() {
    std::thread::sleep(Duration::from_secs(1)); // ❌ Bloquea el reactor
}

async fn good_example() {
    tokio::time::sleep(Duration::from_secs(1)).await; // ✅ No bloquea
}
```

### Reglas del runtime de Pingora

```rust
// ✅ el numero de threads por servicio se fija en ServerConf, NO en Opt
//    (Opt solo porta flags de CLI: -d/--daemon, -c/--conf, --no-daemon)
//    verificado en el rev pineado de pingora-core: `threads: usize` (`server/configuration/mod.rs:74`)
//    y `work_stealing: bool` (`:78`) — no son `Option` —, y `ServerConf` (`:48`) implementa
//    `Default`, asi que el resto de campos se hereda.
let conf = pingora_core::server::configuration::ServerConf {
    // `threads` de Pingora vale 1 por defecto: hay que fijarlo, y sin dependencia `num_cpus`
    threads: std::thread::available_parallelism()?.get(),
    work_stealing: true,
    ..Default::default()
};

// ✅ con env vars propias (no hay fichero `.yaml` de Pingora) el ServerConf se inyecta con
//    `new_with_opt_and_conf`, que es infalible (`server/mod.rs:455`). `Server::new` (`:492`)
//    o bien lee `-c <conf>` o bien genera un `ServerConf` default: los threads propios se
//    perderian.
let mut server = pingora_core::server::Server::new_with_opt_and_conf(opt, conf);

// ✅ estado por request en el CTX: sin locks y sin caches en el objeto del servicio
pub struct ProxyCtx {
    crypt_id: String,
    internal_id: String,
    config_version: u64,
    body_bytes: u64,            // total propio por request; el tope duro lo da `Session::upstream_body_bytes_received()`
    origin_status: u16,         // status del origen, capturado en upstream_response_filter
    cache: CacheDirective,
    span: tracing::Span,        // lo crea request_filter; `Span::none()` al construir
    inicio: std::time::Instant, // para `elapsed_ms` del access log
}

/// `new_ctx()` corre antes de conocer el cliente: el span se rellena en `request_filter`.
impl ProxyCtx {
    fn new() -> Self {
        Self {
            crypt_id: String::new(),
            internal_id: String::new(),
            config_version: 0,
            body_bytes: 0,
            origin_status: 0,
            cache: CacheDirective::Miss,
            span: tracing::Span::none(),
            inicio: std::time::Instant::now(),
        }
    }
}

/// Decision de cache del request. La variante se registra y se envía siempre a través de
/// `as_str()` (abajo): el campo `x_cache` del access log y el header `x-cache` del cliente
/// salen de la misma función, así que no pueden bifurcar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDirective {
    Hit,
    Miss,
    Fallback,
    Bypass, // superado max_response_size_bytes: se reenvia en streaming y no se cachea
}

/// Un unico mapeo variante -> texto. Lo consumen `response_filter`/`write_json_error` al escribir
/// el header `x-cache` y `logging()` al registrar el campo `x_cache`, para que el log y lo que ve
/// el cliente no bifurquen. En el alambre van mayúsculas; en el código, variantes UpperCamelCase.
impl CacheDirective {
    pub const fn as_str(self) -> &'static str {
        match self {
            CacheDirective::Hit => "HIT",
            CacheDirective::Miss => "MISS",
            CacheDirective::Fallback => "FALLBACK",
            CacheDirective::Bypass => "BYPASS",
        }
    }
}

// ❌ NUNCA guardes el `ClientConfig` de un cliente en campos del `ProxyHttp`:
//    el objeto del servicio es compartido por todas las conexiones.

// ✅ mlua es sincrono: cualquier ejecucion de Lua corre en spawn_blocking con deadline.
//    `timeout(spawn_blocking(..))` anida dos fallos distintos: `Elapsed` por fuera y
//    `JoinError` por dentro. `ScriptTimeout` pide `timeout_ms` y `elapsed_ms`
//    (ERROR_DICTIONARY.md), asi que el reloj se arranca antes de lanzar el task.
let inicio = std::time::Instant::now();
let ejecucion = tokio::time::timeout(
    lua_timeout,
    tokio::task::spawn_blocking(move || engine.execute(&script, body, ctx)),
)
.await;

let script_result = match ejecucion {
    Err(_) => Err(ProxyError::ScriptTimeout {
        timeout_ms: lua_timeout.as_millis() as u64,
        elapsed_ms: inicio.elapsed().as_millis() as u64,
    }),
    // JoinError = el task de Lua panicó: es un fallo interno, no un error del script
    Ok(Err(join)) => Err(ProxyError::Internal {
        reason: format!("spawn_blocking join: {join}"),
    }),
    Ok(Ok(salida)) => salida,
};
let salida = script_result?;

// ✅ el `timeout` de arriba NO cancela el task: `spawn_blocking` sigue corriendo y un script
//    con bucle infinito quema un thread del pool aunque el filtro ya haya respondido. El
//    deadline real vive DENTRO del sandbox, en `infrastructure/lua_engine.rs`, con el hook de
//    vm de mlua (verificado en mlua 0.12.1: `debug.rs:275` + `state.rs:756`, callback
//    `Fn(&Lua, &Debug) -> Result<VmState>`). `VmState` solo tiene `Continue | Yield`, asi que
//    abortar == devolver `Err`; `set_interrupt` NO existe con el feature `lua54` (es `luau`-only).
//    Va dentro de `LuaEngine::execute` (hilo bloqueado), con su propio reloj:
let inicio_vm = std::time::Instant::now();
let lua = mlua::Lua::new();
let limite = Duration::from_millis(200);
lua.set_hook(
    mlua::HookTriggers::new().every_nth_instruction(2000),
    move |_lua, _debug| {
        if inicio_vm.elapsed() > limite {
            Err(mlua::Error::RuntimeError("lua deadline exceeded".into()))
        } else {
            Ok(mlua::VmState::Continue)
        }
    },
)?;

// ❌ NUNCA dejes el `mlua` corriendo en el thread del reactor ni `unwrap` en el join:
//    un panic dentro del sandbox vaciaría el pool de blocking y colgaría el proxy.
```

## 4. Estructura de Módulos

### Organización DDD

Árbol **objetivo** (Pingora como motor del ciclo de vida). Un módulo solo del `src/` si todavía no
existe: créelo en la fase que lo necesita, no antes.

```
src/
├── main.rs                  # composition root: pingora_core::Server + http_proxy_service + background_service
│
├── domain/                  # Lógica de negocio pura, sin IO y sin tipos de Pingora
│   ├── models.rs            # ClientConfig, ProxyContext, CachedResponse, ErrorMode
│   ├── errors.rs            # ProxyError (thiserror) + log_level/error_fields/log_domain_error
│   ├── services.rs          # Puertos: ConfigFetcher, CacheStore, DnsResolver, LuaExecutor
│   └── validators.rs        # is_private_ip, validate_url_strict, domain_matches_whitelist
│
├── application/             # Casos de uso y orquestación
│   ├── proxy_service.rs     # impl ProxyHttp (Pingora): filtros + upstream_peer + logging
│   │                        #   + From<ProxyError> for BError y fail_to_proxy
│   ├── fallback.rs          # cadena de 3 niveles (stale -> global -> embebido)
│   ├── lua_engine.rs        # SandboxedLuaEngine (implementa LuaExecutor)
│   ├── webhook_service.rs   # HTTP saliente desde Lua: pasa SIEMPRE por DnsResolver
│   ├── changes_feed.rs      # listener de _changes, servido como BackgroundService
│   └── cache_key.rs         # px:*, px:*:stale, px:defaults:{mime}, rl:{crypt_id}
│
├── infrastructure/          # Adaptadores de salida
│   ├── couchdb_repo.rs      # CouchDbRepository + caché moka
│   ├── valkey_cache.rs      # ValkeyCacheStore (redis, postcard)
│   ├── dns_resolver.rs      # SecureDnsResolver (DoT + DNSSEC + LRU en-proceso)
│   └── http_client.rs       # unico constructor de cliente HTTP saliente
│
└── interfaces/              # Superficies de entrada
    ├── proxy_handler.rs     # ⚠️ DEPRECATED: hoy contiene el handler Axum; se vacía al migrar
    │                        #    su lógica a los filtros de application/proxy_service.rs
    └── control_api.rs       # Router Axum /api/v1/* expuesto como BackgroundService en :8081
```

Reglas de dependencias:

- `domain` no importa `pingora`, `axum`, `redis`, `reqwest` ni `mlua`. El mapeo `ProxyError -> BError`
  vive en `application/proxy_service.rs`, que ya depende del motor; en `domain` solo se permite
  `tracing::Level` (lo devuelve `log_level()`).
- `application` depende de `domain` y de los puertos; nunca de un adaptador concreto.
- `infrastructure` e `interfaces` dependen hacia adentro. `main.rs` es el único lugar que conoce
  los tipos concretos y los inyecta como `Arc<dyn …>`.
- **Ningún código fuera de `infrastructure/http_client.rs` y `infrastructure/dns_resolver.rs`
  abre un socket saliente.** Ni un webhook, ni un fallback remoto, ni una llamada de test manual.

### Convenciones de Nombres

```rust
// ✅ Structs: PascalCase
pub struct ClientConfig { ... }
pub struct ProxyContext { ... }

// ✅ Traits: PascalCase
pub trait ConfigFetcher { ... }
pub trait CacheStore { ... }

// ✅ Funciones: snake_case
pub fn fetch_config(client_id: &str) -> Result<ClientConfig> { ... }

// ✅ Constantes: SCREAMING_SNAKE_CASE
pub const MAX_RESPONSE_SIZE: u64 = 104_857_600;

// ✅ Módulos: snake_case
mod proxy_service;
mod lua_engine;
```

## 5. Pruebas (TDD)

### Estructura

```rust
// ✅ Cada módulo de dominio debe tener pruebas
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_url_rejects_private_ip() {
        let url = "http://169.254.169.254/latest/meta-data/";
        let result = validate_url(url);
        assert!(matches!(result, Err(ProxyError::SsrfBlocked { .. })));
    }

    #[test]
    fn test_validate_url_accepts_public_domain() {
        let url = "https://shutterstock.com/image.jpg";
        let result = validate_url(url);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_async_function() {
        let result = async_operation().await;
        assert_eq!(result, expected_value);
    }
}
```

### Cobertura Mínima

- **Camino feliz**: Al menos 1 prueba por función pública
- **Casos de error**: Al menos 2 pruebas de error por función
- **Casos límite**: Pruebas para valores extremos (0, MAX, empty, etc.)

### Pruebas y plataformas

Pingora solo compila en Linux/Unix, así que las pruebas se separan por capa:

| Capa | Dónde corre | Feature |
|---|---|---|
| `domain` (validators, models, errors) | host (Windows incluido) | `default = []` |
| `application` (fallback, cache_key, lua_engine) con puertos mockeados | host | `default = []` |
| `infrastructure` (Valkey/CouchDB/DoT reales) | Docker `rust:bookworm` | `--features proxy` |
| Filtros `ProxyHttp` (`proxy_service`) | Docker `rust:bookworm` | `--features proxy` |

```rust
// ✅ la logica de decision se extrae a una funcion pura para poder probarla sin Pingora
fn cache_directive_for(total_bytes: u64, mime: &str) -> CacheDirective { ... }

#[test]
fn cache_directive_bypass_over_limit() {
    assert!(matches!(cache_directive_for(6_000_000, "image/png"), CacheDirective::Bypass));
}
```

- Los escenarios Gherkin de `specs/*.feature` (generado desde `docs/BDD.md`, un archivo por Feature)
  son el contrato: cada Scenario se nombra en el test (`test_feature5_scenario2_…`) para poder trazar.
- No se prueba contra red externa real en la suite por defecto; un test que necesite Internet se
  marca con `#[ignore]` y se ejecuta a mano.

## 6. Seguridad

### Validación de Entradas

```rust
// ✅ Validar TODAS las entradas del usuario
pub fn validate_crypt_id(crypt_id: &str) -> Result<(), ProxyError> {
    if crypt_id.len() != 12 {
        return Err(ProxyError::InvalidCryptId {
            reason: "Length must be 12".to_string()
        });
    }

    if !crypt_id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err(ProxyError::InvalidCryptId {
            reason: "Invalid characters".to_string()
        });
    }

    Ok(())
}

// ✅ Validar URLs estrictamente
pub fn validate_url_strict(url_str: &str) -> Result<Url, ProxyError> {
    let url = Url::parse(url_str)
        .map_err(|e| ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: e.to_string()
        })?;

    // Validar esquema
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: "Only http and https schemes allowed".to_string()
        });
    }

    // Validar que no haya credenciales
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProxyError::InvalidUrlFormat {
            url: url_str.to_string(),
            reason: "Credentials not allowed in URL".to_string()
        });
    }

    Ok(url)
}
```

### No Confiar en Entradas

```rust
// ❌ NUNCA confiar en entradas del usuario
let domain = extract_domain(user_input);
fetch_resource(domain); // ❌ Peligroso

// ✅ SIEMPRE validar antes de usar
let domain = extract_domain(user_input)?;
validate_domain_whitelist(&domain)?;
fetch_resource(domain).await?; // ✅ Seguro
```

## 7. Documentación

### Comentarios

````rust
// ✅ Documentar funciones públicas con doc comments
/// Valida que una URL no apunte a IPs privadas (anti-SSRF)
///
/// # Arguments
/// * `url` - URL a validar
///
/// # Returns
/// * `Ok(Url)` - URL válida y segura
/// * `Err(ProxyError::SsrfBlocked)` - URL apunta a IP privada
///
/// # Examples
/// ```
/// let url = validate_url("https://example.com/image.jpg").unwrap();
/// ```
pub fn validate_url(url: &str) -> Result<Url, ProxyError> {
    // ...
}

// ❌ NUNCA comentar código obvio
let x = 5; // Set x to 5 ❌

// ✅ Explicar el "por qué", no el "qué"
// Usamos DNS-over-TLS para prevenir envenenamiento de caché DNS
let resolver = create_secure_dns_resolver();
````

## 8. Dependencias

### Reglas

- Usar solo crates mantenidos activamente (último release < 6 meses)
- Ejecutar `cargo audit`/`cargo deny` en CI/CD
- Minimizar dependencias (preferir std cuando sea posible)
- Usar versiones específicas en `Cargo.toml` (no `*`)
- **Pingora se toma de git con `rev` explícito, nunca de una rama móvil sin fijar y nunca de
  crates.io.** El `0.9.0` publicado tiene una API distinta del commit que usa este proyecto
  (`proxy_trait.rs` declara filtros de cuerpo `async` y `Session<DS = ()>` genérico; en crates.io
  0.9.0 son síncronos y `Session` no es genérico), así que un `version = "0.9"` recompilaría con
  otras firmas y rompería todo lo citado en esta guía. La reproducibilidad la da el par
  `rev` + `Cargo.lock` + `--locked`; la lista comparativa está en `spec.md`, "Rev de Pingora contra
  el que está escrita esta documentación".
- Consecuencia operativa: este manifiesto ya usa `rev`, así que el grafo no se mueve solo. Cuando se
  cambie el hash, el `source` de `Cargo.lock` se reescribe (`?rev=<viejo>` -> `?rev=<nuevo>`) y hace
  falta **una** compilación en línea sin `--locked` (`cargo update -p pingora-core -p pingora-proxy`)
  antes de cualquier build `--locked` posterior. Lo mismo aplica cuando el manifiesto añade un crate
  del workspace que el lock no conoce (p. ej. `pingora-limits`).
- El lock ya está en ese estado regenerado: sus 12 entradas `pingora-*` (`core`, `proxy`, `http`,
  `limits` —los cuatro declarados— más los transitivos del workspace `cache`, `error`,
  `header-serde`, `lru`, `pool`, `runtime`, `rustls` y `timeout`) llevan
  `source = "git+…?rev=4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19#4487f7b2…"`. Cualquier `grep
  'pingora?branch=' Cargo.lock` debe dar vacío: `?branch=main` es la forma pre-regeneración y con
  ella `cargo build --locked` aborta antes de compilar.
- `Cargo.lock` commiteado y toda compilación de verificación con `--locked`.

```toml
[dependencies]
# ✅ Versiones específicas, desde crates.io
tokio = { version = "1.53", features = ["full"] }
axum = "0.8"                     # solo API de control, servida como BackgroundService

# ✅ Pingora: git + rev exacto (el mismo que figura en Cargo.lock).
#    OBLIGATORIO declarar `rustls`: pingora-core y pingora-proxy traen `default = []`, y su
#    feature `tls` no existe —sin `features = ["rustls"]` el binario compila igual pero el
#    handshake TLS al upstream falla en runtime. `default-features = false` se escribe igual
#    para que el manifiesto diga en claro que no se hereda ningún backend TLS.
pingora-core = { git = "https://github.com/cloudflare/pingora", rev = "4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19", default-features = false, features = ["rustls"], optional = true }
pingora-proxy = { git = "https://github.com/cloudflare/pingora", rev = "4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19", default-features = false, features = ["rustls"], optional = true }
pingora-http = { git = "https://github.com/cloudflare/pingora", rev = "4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19", optional = true }
# ✅ rate limiter local de respaldo: `pingora_limits::rate::Rate`, sin features TLS
pingora-limits = { git = "https://github.com/cloudflare/pingora", rev = "4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19", optional = true }

# ❌ `pingora-cache` NO se declara: la persistencia es la capa propia del proyecto
#    (`CacheStore` sobre Valkey) y el módulo `HttpCache` de Pingora no se habilita
#    (ver spec.md, "Decisión de caché"). Si alguna fase lo habilita, habrá que
#    añadirlo directo porque pingora-proxy no re-exporta su API —su `prelude`
#    (lib.rs:105-107) solo trae http_proxy, http_proxy_service, ProxyHttp,
#    ProxyWarnLogContext y Session, y `pingora_cache::prelude` está vacío (`:60`)—.

# ❌ NUNCA usar wildcard
tokio = { version = "*", features = ["full"] }
# ❌ NUNCA rama movil: un push de Cloudflare cambiaria la API bajo los docs
pingora-proxy = { git = "…", branch = "main" }
# ❌ NUNCA crates.io para Pingora en este proyecto: mismo numero, otra API
pingora-proxy = { version = "0.9", features = ["rustls"] }
```

El feature `proxy` sigue siendo opcional **solo como deuda de migración**: deja compilar
`domain`/`application` en Windows. No es una decisión de arquitectura; cuando la Fase 3 esté
cerrada, `proxy` pasa al `default` y el código Axum del proxy se elimina.

## 9. Performance

### Reglas

```rust
// ✅ cuerpo grande = streaming por chunk en el filtro de Pingora, con contador en el CTX
//    verificado en el rev pineado (`proxy_trait.rs:461` y `:494`): es `async fn`, asi que aqui
//    dentro si se puede await (scripting Lua en `spawn_blocking`, webhook) sin bloquear el worker
async fn upstream_response_body_filter(
    &self,
    session: &mut Session,
    body: &mut Option<Bytes>,
    end_of_stream: bool,
    ctx: &mut Self::CTX,
) -> pingora_core::Result<Option<std::time::Duration>>
where
    Self::CTX: Send + Sync,
{
    if let Some(chunk) = body.as_ref() {
        ctx.body_bytes += chunk.len() as u64;
        // el tope duro lo mide Pingora: no hay que reimplementar el contador
        if session.upstream_body_bytes_received() as u64 > MAX_RESPONSE_SIZE {
            // aborta el request a mitad de cuerpo: el status ya no se puede cambiar,
            // así que payload_too_large solo puede responder 413 si Content-Length se
            // conocía de antemano (ver nota ‡ del diccionario)
            return Err(ProxyError::PayloadTooLarge {
                content_length: ctx.body_bytes,
                max_allowed: MAX_RESPONSE_SIZE,
            }
            .into());
        }
        // la copia cacheable se mide aqui: el modulo HttpCache de Pingora NO se habilita
        // (la caché es la capa propia sobre Valkey), y mientras nadie llame a
        // `HttpCache::enable(..)` la fase es `Disabled(NeverEnabled)` — cualquier
        // `session.cache.set_max_file_size_bytes(..)` haria `panic!`
        // (`pingora-cache/src/lib.rs:776-784`).
        if ctx.body_bytes <= CACHEABLE_SIZE_LIMIT {
            ctx.collect(chunk); // solo lo necesario para cachear
        } else {
            ctx.cache = CacheDirective::Bypass; // se reenvia en streaming, sin retener copia
        }
    }
    let _ = end_of_stream; // en `end_of_stream` se persiste en Valkey y se emite X-Cache
    Ok(None) // sin delay de reenvio
}

// ❌ NUNCA acumules la respuesta completa antes de responder al cliente
let full = response.bytes().await?;  // ❌ Axum/reqwest: 15 MB en RAM y TTFT = descarga completa

// ✅ cero copias donde se pueda: Bytes es ref-counted
let shared: Bytes = chunk.clone();

// ✅ reutiliza conexiones: un solo pool por (IP, puerto, SNI) gestionado por Pingora
//    NO construya un cliente HTTP por request

// ✅ Usar Arc para datos compartidos
let config = Arc::new(config);
let config_clone = Arc::clone(&config);

// ✅ Usar &str en lugar de String cuando sea posible
fn process(input: &str) -> Result<()> { ... } // ✅
fn process(input: String) -> Result<()> { ... } // ❌ Copia innecesaria
```

Límites de tamaño que deben respetarse en todo camino (definidos en `application/cache_key.rs`
o constantes del servicio, nunca duplicados como literales mágicos):

| Constante | Valor | Efecto |
|---|---|---|
| `MAX_RESPONSE_SIZE` | 100 MB | aborta la descarga con `payload_too_large` |
| `CACHEABLE_SIZE_LIMIT` | 5 MB | sobre pasar: `X-Cache: BYPASS`, sirve igual |
| `STALE_CACHE_TTL` | 86400 s | TTL de la copia `:stale` usada por el fallback |

## 10. Commits

### Formato

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

### Tipos

- `feat`: Nueva funcionalidad
- `fix`: Corrección de bug
- `test`: Añadir o modificar pruebas
- `docs`: Cambios en documentación
- `refactor`: Refactorización sin cambios funcionales
- `perf`: Mejoras de performance
- `chore`: Tareas de mantenimiento

### Ejemplos

```
feat(phase-2): implement DNS resolution with Quad9 ECS

- Add hickory-resolver with DoT and DNSSEC
- Implement fallback chain (Quad9 -> Cloudflare -> AdGuard)
- Add unit tests for DNS validation

Closes #12
```

```
fix(phase-3): prevent SSRF via redirect following

- Pingora no sigue redirects: reenvia el 3xx tal cual
- Validate Location in upstream_response_filter before handing it downstream
- Reject redirects that resolve to private/loopback/link-local ranges
- Add integration test for redirect blocking (event=ssrf_blocked, reason=redirect_to_private_ip)

Fixes #23
```

```
feat(phase-3): port proxy lifecycle to Pingora

- Replace Axum proxy handler with ProxyHttp filter callbacks
- Pin upstream socket in upstream_peer via HttpPeer::new(SocketAddr, tls, sni)
- Serve control API and _changes listener as BackgroundService
- Stream response body in upstream_response_body_filter with byte counter in CTX
```

## 11. Verificación

```bash
# codigo independiente de plataforma (Windows / host)
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test

# con el motor Pingora: SOLO Linux, dentro de Docker
docker run --rm -v "$PWD":/src -w /src -v teleproxy_cargo:/usr/local/cargo \
    -v teleproxy_target:/src/target rust:bookworm \
    cargo clippy --all-targets --features proxy -- -D warnings
```

- Toda compilación de verificación lleva `--locked`.
- **Un `exit 0` del envoltorio no es prueba**: hay que leer el log real del comando (warnings y
  errores de `rustc`/`clippy` aparecen en el body incluso cuando el script que lo invierte devuelve 0).
- El `cargo build --release` del `Dockerfile` usa `--features proxy`: si la imagen se construye sin
  el feature, binariamente no hay proxy y el `HEALTHCHECK` pasa igualmente. Verificar el feature,
  no solo que la imagen arranque.
- Al tocar `infrastructure/http_client.rs` o cualquier cosa bajo `cfg(feature = "proxy-openssl")`,
  verificar **ambas variantes**: el comando de arriba con `--features proxy` y la réplica con
  `--features proxy-openssl` (son hermanas excluyentes; compilan código distinto tras el cfg de
  `pinned_client`, docs/DEPLOYMENT.md § "Variante proxy-openssl").
