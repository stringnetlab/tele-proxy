# Diccionario de Errores del Dominio

Este documento define todos los errores del sistema `tele-proxy` y cómo se materializan en respuesta.
El motor del ciclo de vida del request es **Pingora** (`ProxyHttp`); la API de control sigue siendo
Axum, servida como `BackgroundService`. Cada error debe:

1. Derivar de `thiserror::Error` en `src/domain/errors.rs`, con un `error_code` de `&'static str` estable.
2. Tener un status HTTP asignado en `to_http_status()` (única fuente de verdad del status).
3. Registrarse vía `tracing` con `event = <error_code>` y los campos estructurados de la tabla, en el
   nivel indicado por `log_level()` (nivel **por error**, nunca derivado del rango del status).
4. Mapearse a `BError` (`Box<pingora_core::Error>`) de forma explícita y sin perder el `error_code` ni
   el status (§ "Mapeo hacia Pingora"). El mapeo vive en el adaptador que sí conoce Pingora, no en
   `domain/`.
5. Documentar aquí su camino de respuesta: `short-circuit`, `degradación`, `5xx` o `API`.

## Contrato de la respuesta de error

```json
{ "error": "<error_code>", "message": "<Display del error>" }
```

Cabeceras obligatorias:

| Cabecera        | Cuándo                                        | Valor                                             |
| --------------- | --------------------------------------------- | ------------------------------------------------- |
| `Content-Type`  | siempre                                       | `application/json`                                 |
| `Retry-After`   | `rate_limit_exceeded` (429)                    | segundos restantes de la ventana                   |
| `X-Cache`       | siempre que la respuesta sea del camino proxy  | `MISS` / `HIT` / `BYPASS` / `FALLBACK`             |

`ErrorResponse` (en `domain/errors.rs`) es el cuerpo serializable de ese contrato.

## Tabla de Errores

Camino de respuesta: **short-circuit** = el filtro escribe la respuesta y devuelve `Ok(true)`;
**degradación** = el request termina **200** con el cuerpo original o el fallback, y el error solo
vive en el log; **5xx** = el callback devuelve `Err(BError)` y `fail_to_proxy` responde;
**API** = lo produce Axum en `/api/v1/`.

| Código de Error          | Variante Rust          | Causa Raíz                                                | HTTP Status | Nivel de Log | Campos Estructurados                                        | Camino        |
| ------------------------ | ---------------------- | --------------------------------------------------------- | ----------- | ------------ | ----------------------------------------------------------- | ------------- |
| `invalid_crypt_id`       | `InvalidCryptId`       | El `crypt_id` no tiene 12 chars o no existe en CouchDB     | 404         | ERROR        | `crypt_id`†, `reason`                                       | short-circuit |
| `domain_not_whitelisted` | `DomainNotWhitelisted` | El dominio de la URL no está en la whitelist               | 403         | WARN         | `crypt_id`†, `domain`                                       | short-circuit |
| `rate_limit_exceeded`    | `RateLimitExceeded`    | Se superó `max_requests` en `window_seconds`               | 429         | WARN         | `crypt_id`†, `current_count`, `max_requests`, `retry_after_secs` | short-circuit |
| `ssrf_blocked`           | `SsrfBlocked`          | La URL resuelve a IP privada, loopback o de metadatos      | 403         | ERROR        | `crypt_id`†, `url`, `resolved_ip`, `reason`                 | short-circuit |
| `invalid_url_format`     | `InvalidUrlFormat`     | URL con esquema no HTTP/S, credenciales o fragmentos       | 400         | WARN         | `crypt_id`†, `url`, `reason`                                | short-circuit |
| `dns_resolution_failed`  | `DnsResolutionFailed`  | Todos los resolvers DoT fallaron o DNSSEC inválido         | 502         | ERROR        | `hostname`, `reason`                                        | 5xx           |
| `lua_sandbox_violation`  | `LuaSandboxViolation`  | El script intenta usar `os`, `io`, `package`, etc.         | 500         | ERROR        | `crypt_id`†, `attempted_function`                           | degradación   |
| `redos_blocked`          | `ReDosBlocked`         | Compilación o ejecución de regex excede el presupuesto     | 500         | ERROR        | `crypt_id`†, `pattern`, `elapsed_ms`                        | degradación   |
| `integrity_check_failed` | `IntegrityCheckFailed` | El hash del script o del fallback no coincide              | 500         | ERROR        | `crypt_id`†, `resource_type`, `expected_hash`, `actual_hash`| 5xx           |
| `payload_too_large`      | `PayloadTooLarge`      | El cuerpo excede `max_response_size_bytes` (100 MB)        | 413         | WARN         | `crypt_id`†, `content_length`, `max_allowed`                | 5xx‡          |
| `upstream_error`         | `UpstreamError`        | El origen devolvió 4xx/5xx sin fallback aplicable          | 502         | ERROR        | `crypt_id`†, `url`, `upstream_status`                       | 5xx           |
| `script_timeout`         | `ScriptTimeout`        | La ejecución de Lua excede `timeout_ms`                    | 500         | WARN         | `crypt_id`†, `timeout_ms`, `elapsed_ms`                     | degradación   |
| `script_memory_limit`    | `ScriptMemoryLimit`    | El script excede `memory_limit_mb`                         | 500         | WARN         | `crypt_id`†, `memory_limit_mb`, `used_mb`                   | degradación   |
| `webhook_timeout`        | `WebhookTimeout`       | Llamada HTTP desde Lua excede su timeout                   | 500         | WARN         | `crypt_id`†, `webhook_url`, `timeout_ms`                    | degradación   |
| `webhook_failed`         | `WebhookFailed`        | El webhook del sandbox no pudo completarse (fallo de transporte o TLS) | 502 | WARN        | `url`, `reason`                                             | degradación (hoy solo log) |
| `unauthorized`           | `Unauthorized`         | Token Bearer inválido o ausente en `/api/v1/`              | 401         | WARN         | `ip_address`†, `reason`                                     | API           |
| `config_not_found`       | `ConfigNotFound`       | La configuración del cliente no existe en CouchDB          | 404         | ERROR        | `internal_id`                                               | 5xx           |
| `internal_error`         | `Internal`             | Fallo no clasificado del proxy o de la API de control      | 500         | ERROR        | `crypt_id`†, `reason`                                       | 5xx / API     |

† Campo derivado del **span de la petición** (`ProxyCtx`), no de la variante del error: `crypt_id`,
`internal_id`, `config_version` y `ip_address` se adjuntan con `tracing::info_span!` en
`request_filter` (parte A, en cuanto se valida el `crypt_id`) y se guardan en `ProxyCtx.span`. Como un span solo es *current* mientras está
entrado en el thread —y Pingora corre cada callback en un poll distinto—, **todo punto de emisión lo
entra con `ctx.span.in_scope(..)`**; por eso ningún log repite esos cuatro campos. Las variantes del
dominio solo cargan lo específico del fallo.

‡ `payload_too_large` solo puede responder 413 si el origen anunció `Content-Length` por encima del
límite **antes** de reenviar las cabeceras. Si el exceso se descubre en
`upstream_response_body_filter` (cabeceras ya enviadas), la respuesta no puede cambiar de status: se
corta la transmisión con `Err(BError)` y el evento se registra en el log. El cuerpo parcial **no** se
guarda en Valkey.

Nivel de log, criterio aplicado: los intentos de escape o abuso (`ssrf_blocked`,
`lua_sandbox_violation`, `redos_blocked`, `integrity_check_failed`), los fallos de infraestructura
(`dns_resolution_failed`, `upstream_error`, `config_not_found`, `internal_error`) y los IDs inexistentes
(`invalid_crypt_id`, por enumeración de clientes) son **ERROR**. Las rechazos por política esperables
(`domain_not_whitelisted`, `invalid_url_format`, `unauthorized`), los límites de recursos del propio
cliente (`rate_limit_exceeded`, `payload_too_large`) y las degradaciones del sandbox (`script_timeout`,
`script_memory_limit`, `webhook_timeout`, `webhook_failed`) son **WARN**, porque el servicio sigue
prestando respuesta. De los dos últimos el cliente nunca ve un 502: `application/lua_engine.rs:384`
registra el fallo y devuelve al script la cadena `webhook failed: <error_code>`; el 502 del diccionario
es el `to_http_status()` de la variante, y solo se convertiría en respuesta si el script propagara el
error. Lo que sí aborta la petición son `ssrf_blocked` y `domain_not_whitelisted` surgidos de un
webhook: quedan guardados en el estado de la ejecución de forma **persistente** (`RunState.blocked`,
resistente a `pcall`) y `interfaces/proxy_handler.rs:245` los reenvía como el mismo 403 que devolvería
la URL del `?url=`.

## Vocabulario de `reason`

`reason` es texto libre para el `message`, pero el filtrado y las alertas dependen de un valor estable.
Códigos definidos:

| Error                  | Valores de `reason`                                                          |
| ---------------------- | ---------------------------------------------------------------------------- |
| `invalid_crypt_id`     | `not_12_chars`, `not_found`                                                   |
| `invalid_url_format`   | `missing_url_param`, `scheme_not_http`, `credentials_present`, `fragment_present`, `no_hostname`, `no_port`, `method_not_supported` |
| `ssrf_blocked`         | `private_ip`, `loopback`, `link_local`, `metadata_ip`, `unique_local`, `redirect_to_private_ip`, `webhook_private_ip` |
| `unauthorized`         | `missing_token`, `invalid_token`, `token_revoked`                             |
| `internal_error`       | contexto del fallo (no se expone al cliente; ver § Reglas)                    |

> **Estado de esta tabla**: los valores estables son el contrato que hay que alcanzar; el código los
> emite todavía como frase legible en inglés (`"URL directly references private IP"` en
> `domain/validators.rs:147`, `"DNS resolved to private IP"` en
> `infrastructure/dns_resolver.rs:192`, `"Unsupported webhook method 'TRACE'"` en
> `application/webhook_service.rs:26`). Normalizar cada `construct` a estos códigos es una tarea de
> Fase 3, no una corrección de la portación: ningún comportamiento cambia, solo el valor filtrable.
> `webhook_failed` queda fuera de la tabla porque su `reason` es el texto del fallo de transporte de
> `reqwest`; lo que sí es estable es que **no** lleva credenciales (el validador las prohíbe) y que
> viaja solo al log, nunca al `message` que ve el cliente.

## Eventos de log que no son errores de dominio

Conviven en `tracing` con los códigos anteriores y usan el mismo contrato de `event` + campos.

| `event`               | Nivel | Cuándo                                                        | Campos                          | Estado |
| --------------------- | ----- | ------------------------------------------------------------- | ------------------------------- | ------ |
| `request_completed`   | INFO  | `logging()` al terminar cada request del proxy                 | `method`, `path`, `status`, `x_cache`, `elapsed_ms` | pendiente — call site en Fase 3 |
| `rate_limit_degraded` | WARN  | Valkey indisponible; decide el contador en-proceso `LocalRateLimiter`  | `crypt_id`, `limit`, `window_seconds` | **emitido** — `infrastructure/valkey_cache.rs` |
| `mime_discrepancy`    | WARN  | `?mime=` forzado difiere del `Content-Type` real del origen     | `crypt_id`, `forced_mime`, `real_mime` | pendiente — `?mime=` se aplica sin comparar |
| `invalid_config`      | WARN  | `PUT /api/v1/clients/config` fuera de cota, con dominios prohibidos o con un hash fuera de formato | `internal_id`†, `field`, `reason` | **emitido** — vía `log_domain_error` |
| `crypt_id_rotated`    | INFO  | Rotación de `crypt_id` consumada                               | `internal_id`, `old_crypt_id`, `new_crypt_id` | **emitido** — `infrastructure/couchdb_repo.rs`, en `rotate_crypt_id` |

> `crypt_id_rotated` se emite **después** del `PUT` a CouchDB y de invalidar la caché: si alguno de
> esos pasos falla, la rotación no se consumó y no hay pista que registrar. `internal_id` es
> obligatorio en el evento porque es el único identificador que sobrevive al cambio de `crypt_id`.

> Estos dos últimos niveles son la excepción documentada en `docs/BDD.md` (DoD 5): en producción solo
> se emiten `WARN`/`ERROR`, más `request_completed` y `crypt_id_rotated` en `INFO`.

## Implementación en Rust

`src/domain/errors.rs` — el enum **no** importa `axum` ni `pingora`; solo `thiserror` y `serde`.

```rust
use serde::Serialize;
use thiserror::Error;
use tracing::Level;

#[derive(Error, Debug)]
pub enum ProxyError {
    #[error("Invalid crypt_id: {reason}")]
    InvalidCryptId { reason: String },

    #[error("Domain not whitelisted: {domain}")]
    DomainNotWhitelisted { domain: String },

    #[error("Rate limit exceeded")]
    RateLimitExceeded {
        current_count: u32,
        max_requests: u32,
        retry_after_secs: u32,
    },

    #[error("SSRF blocked: {reason}")]
    SsrfBlocked { url: String, resolved_ip: String, reason: String },

    #[error("Invalid URL format: {reason}")]
    InvalidUrlFormat { url: String, reason: String },

    #[error("DNS resolution failed for {hostname}: {reason}")]
    DnsResolutionFailed { hostname: String, reason: String },

    #[error("Lua sandbox violation: attempted to call {attempted_function}")]
    LuaSandboxViolation { attempted_function: String },

    #[error("ReDoS blocked: pattern={pattern}, elapsed={elapsed_ms}ms")]
    ReDosBlocked { pattern: String, elapsed_ms: u64 },

    #[error("Integrity check failed for {resource_type}")]
    IntegrityCheckFailed {
        resource_type: String,
        expected_hash: String,
        actual_hash: String,
    },

    #[error("Payload too large: {content_length} bytes (max: {max_allowed})")]
    PayloadTooLarge { content_length: u64, max_allowed: u64 },

    #[error("Upstream error: status={upstream_status}")]
    UpstreamError { url: String, upstream_status: u16 },

    #[error("Script timeout: {elapsed_ms}ms (max: {timeout_ms}ms)")]
    ScriptTimeout { timeout_ms: u64, elapsed_ms: u64 },

    #[error("Script memory limit exceeded: {used_mb}MB (max: {memory_limit_mb}MB)")]
    ScriptMemoryLimit { memory_limit_mb: u32, used_mb: u32 },

    #[error("Webhook timeout: url={webhook_url}, timeout={timeout_ms}ms")]
    WebhookTimeout { webhook_url: String, timeout_ms: u64 },

    #[error("Webhook failed: {reason}")]
    WebhookFailed { url: String, reason: String },

    #[error("Unauthorized: {reason}")]
    Unauthorized { reason: String },

    #[error("Configuration not found for client: {internal_id}")]
    ConfigNotFound { internal_id: String },

    #[error("Internal error: {reason}")]
    Internal { reason: String },
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
}
```

`to_http_status()` y `to_error_code()` siguen el orden de la tabla. `log_level()` **no** se deriva del
status: un `403` de SSRF es `ERROR` y un `500` de timeout de script es `WARN`.

```rust
impl ProxyError {
    pub fn to_http_status(&self) -> u16 {
        match self {
            Self::InvalidCryptId { .. } => 404,
            Self::DomainNotWhitelisted { .. } => 403,
            Self::RateLimitExceeded { .. } => 429,
            Self::SsrfBlocked { .. } => 403,
            Self::InvalidUrlFormat { .. } => 400,
            Self::DnsResolutionFailed { .. } => 502,
            Self::LuaSandboxViolation { .. } => 500,
            Self::ReDosBlocked { .. } => 500,
            Self::IntegrityCheckFailed { .. } => 500,
            Self::PayloadTooLarge { .. } => 413,
            Self::UpstreamError { .. } => 502,
            Self::ScriptTimeout { .. } => 500,
            Self::ScriptMemoryLimit { .. } => 500,
            Self::WebhookTimeout { .. } => 500,
            Self::WebhookFailed { .. } => 502,
            Self::Unauthorized { .. } => 401,
            Self::ConfigNotFound { .. } => 404,
            Self::Internal { .. } => 500,
        }
    }

    pub fn to_error_code(&self) -> &'static str {
        match self {
            Self::InvalidCryptId { .. } => "invalid_crypt_id",
            Self::DomainNotWhitelisted { .. } => "domain_not_whitelisted",
            Self::RateLimitExceeded { .. } => "rate_limit_exceeded",
            Self::SsrfBlocked { .. } => "ssrf_blocked",
            Self::InvalidUrlFormat { .. } => "invalid_url_format",
            Self::DnsResolutionFailed { .. } => "dns_resolution_failed",
            Self::LuaSandboxViolation { .. } => "lua_sandbox_violation",
            Self::ReDosBlocked { .. } => "redos_blocked",
            Self::IntegrityCheckFailed { .. } => "integrity_check_failed",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::UpstreamError { .. } => "upstream_error",
            Self::ScriptTimeout { .. } => "script_timeout",
            Self::ScriptMemoryLimit { .. } => "script_memory_limit",
            Self::WebhookTimeout { .. } => "webhook_timeout",
            Self::WebhookFailed { .. } => "webhook_failed",
            Self::Unauthorized { .. } => "unauthorized",
            Self::ConfigNotFound { .. } => "config_not_found",
            Self::Internal { .. } => "internal_error",
        }
    }

    /// Nivel por error: los límites esperables y las degradaciones son WARN; la seguridad y la
    /// infraestructura son ERROR. No se deduce del rango del status.
    pub fn log_level(&self) -> Level {
        match self {
            Self::DomainNotWhitelisted { .. }
            | Self::RateLimitExceeded { .. }
            | Self::InvalidUrlFormat { .. }
            | Self::PayloadTooLarge { .. }
            | Self::ScriptTimeout { .. }
            | Self::ScriptMemoryLimit { .. }
            | Self::WebhookTimeout { .. }
            | Self::WebhookFailed { .. }
            | Self::Unauthorized { .. } => Level::WARN,

            Self::InvalidCryptId { .. }
            | Self::SsrfBlocked { .. }
            | Self::DnsResolutionFailed { .. }
            | Self::LuaSandboxViolation { .. }
            | Self::ReDosBlocked { .. }
            | Self::IntegrityCheckFailed { .. }
            | Self::UpstreamError { .. }
            | Self::ConfigNotFound { .. }
            | Self::Internal { .. } => Level::ERROR,
        }
    }

    /// `Retry-After` en segundos; `None` para todo error que no sea 429.
    pub fn retry_after_secs(&self) -> Option<u32> {
        match self {
            Self::RateLimitExceeded { retry_after_secs, .. } => Some(*retry_after_secs),
            _ => None,
        }
    }

    /// Campos del diccionario que la variante sabe por sí misma. El span aporta `crypt_id`,
    /// `internal_id`, `config_version` e `ip_address`.
    pub fn error_fields(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::InvalidCryptId { reason } => vec![("reason", reason.clone())],
            Self::DomainNotWhitelisted { domain } => vec![("domain", domain.clone())],
            Self::RateLimitExceeded { current_count, max_requests, retry_after_secs } => vec![
                ("current_count", current_count.to_string()),
                ("max_requests", max_requests.to_string()),
                ("retry_after_secs", retry_after_secs.to_string()),
            ],
            Self::SsrfBlocked { url, resolved_ip, reason } => vec![
                ("url", url.clone()), ("resolved_ip", resolved_ip.clone()), ("reason", reason.clone()),
            ],
            Self::InvalidUrlFormat { url, reason } => vec![("url", url.clone()), ("reason", reason.clone())],
            Self::DnsResolutionFailed { hostname, reason } => vec![("hostname", hostname.clone()), ("reason", reason.clone())],
            Self::LuaSandboxViolation { attempted_function } => vec![("attempted_function", attempted_function.clone())],
            Self::ReDosBlocked { pattern, elapsed_ms } => vec![("pattern", pattern.clone()), ("elapsed_ms", elapsed_ms.to_string())],
            Self::IntegrityCheckFailed { resource_type, expected_hash, actual_hash } => vec![
                ("resource_type", resource_type.clone()),
                ("expected_hash", expected_hash.clone()),
                ("actual_hash", actual_hash.clone()),
            ],
            Self::PayloadTooLarge { content_length, max_allowed } => vec![
                ("content_length", content_length.to_string()), ("max_allowed", max_allowed.to_string()),
            ],
            Self::UpstreamError { url, upstream_status } => vec![("url", url.clone()), ("upstream_status", upstream_status.to_string())],
            Self::ScriptTimeout { timeout_ms, elapsed_ms } => vec![("timeout_ms", timeout_ms.to_string()), ("elapsed_ms", elapsed_ms.to_string())],
            Self::ScriptMemoryLimit { memory_limit_mb, used_mb } => vec![("memory_limit_mb", memory_limit_mb.to_string()), ("used_mb", used_mb.to_string())],
            Self::WebhookTimeout { webhook_url, timeout_ms } => vec![("webhook_url", webhook_url.clone()), ("timeout_ms", timeout_ms.to_string())],
            Self::WebhookFailed { url, reason } => vec![("url", url.clone()), ("reason", reason.clone())],
            Self::Unauthorized { reason } => vec![("reason", reason.clone())],
            Self::ConfigNotFound { internal_id } => vec![("internal_id", internal_id.clone())],
            Self::Internal { reason } => vec![("reason", reason.clone())],
        }
    }

    /// Fuente del error a efectos de `ErrorSource` de Pingora.
    pub fn is_client_error(&self) -> bool {
        matches!(self.to_http_status(), 400..=499)
    }
}
```

## Mapeo hacia Pingora

Los callbacks de `ProxyHttp` devuelven `pingora_core::Result<T>`, que es
`Result<T, BError>` con `pub type BError = Box<Error>` y `pub type Result<T, E = BError>`
(`pingora-error/src/lib.rs:27` y `:29`). El puente vive en `src/application/proxy_service.rs`, junto
a los filtros, porque `domain/` no importa `pingora`.

Verificado contra el fuente de `pingora-error` / `pingora-proxy` del rev pineado en `Cargo.lock`:

- `Error::create(etype: ErrorType, esource: ErrorSource, context: Option<ImmutStr>, cause: Option<Box<dyn Error + Send + Sync>>) -> BError` (`:203`)
- `Error::because<S: Into<ImmutStr>, E: Into<Box<dyn ErrorTrait + Send + Sync>>>(e: ErrorType, context: S, cause: E) -> BError` (`:256`) — deja `esource = Unset`
- `ImmutStr`: existe `From<&'static str>` y `From<String>`
- `ErrorType::new_code(name: &'static str, code: u16) -> Self` (asociado `const fn`, `:157`) → variante `CustomCode(&'static str, u16)`
- `ErrorType::HTTPStatus(u16)`, `InternalError`, `ConnectError`, `ReadTimedout`, `TLSHandshakeFailure`, `Custom(&'static str)`, `UnknownError`
- `Error::root_cause(&self) -> &(dyn ErrorTrait + Send + Sync + 'static)` (`:459`) permite `downcast_ref::<ProxyError>()`

El default de `fail_to_proxy` en el rev (`pingora-proxy/src/proxy_trait.rs:653-691`) decide el status así:

```rust
let code = match e.etype() {
    HTTPStatus(code) => *code,
    _ => match e.esource() {
        ErrorSource::Upstream => 502,
        ErrorSource::Downstream => match e.etype() {
            WriteError | ReadError | ConnectionClosed => 0, // conexión ya muerta
            _ => 400,
        },
        ErrorSource::Internal | ErrorSource::Unset => 500,
    },
};
if code > 0 {
    session.respond_error(code).await.unwrap_or_else(|e| { /* log interno */ });
}
FailToProxy { error_code: code, can_reuse_downstream: false }  // :775-779
```

Consecuencias que fija este diccionario:

1. `CustomCode` **no** produce el status: cae en las reglas de `esource`. Por eso se sobrescribe
   `fail_to_proxy` y el status se toma del `ProxyError` recuperado, nunca del default.
2. `session.respond_error()` **no** sirve para el contrato: su respuesta pregenerada va sin
   `Content-Type`, con `content-length: 0` y sin `Retry-After` (ver § "Escritura de la respuesta en
   el proxy"), y además fuerza `set_keepalive(None)`. La respuesta JSON se escribe a mano con
   `write_response_header` + `write_response_body`.
3. `ErrorSource::Unset` → 500 silencioso. El mapeo debe fijar `esource` explícitamente.
4. El retorno es `FailToProxy { error_code: u16, can_reuse_downstream: bool }`, no un `()`:
   `error_code` alimenta el log interno de Pingora y `can_reuse_downstream` decide si la conexión
   del cliente sobrevive al error. `error_while_proxy` (`:609`) es un `fn` **síncrono** que devuelve
   `Box<Error>` y sirve para reetiquetar el error según idempotencia/reuse; no emite la respuesta.
5. `suppress_error_log` (`:571`, `fn` síncrono, recibe `&Session` y `&CTX`) es la única forma de que
   un error propio no se duplique en el log interno.

```rust
use pingora_core::prelude::*;   // Error, BError, ErrorType::*, ErrorSource, Result

/// Fuente de Pingora. `domain/` no conoce `ErrorSource`, así que se deriva aquí.
fn pingora_source(e: &ProxyError) -> ErrorSource {
    if matches!(e, ProxyError::UpstreamError { .. } | ProxyError::DnsResolutionFailed { .. }) {
        ErrorSource::Upstream
    } else if e.is_client_error() {
        ErrorSource::Downstream
    } else {
        ErrorSource::Internal
    }
}

impl From<ProxyError> for BError {
    fn from(e: ProxyError) -> BError {
        // CustomCode preserva el error_code del diccionario dentro del tipo de error de Pingora.
        let etype = ErrorType::new_code(e.to_error_code(), e.to_http_status());
        Error::create(etype, pingora_source(&e), Some(e.to_string().into()), Some(Box::new(e)))
    }
}

/// Recupera el error del dominio encadenado como causa. `None` para fallos puros del motor.
pub fn proxy_error_of(e: &(dyn std::error::Error + Send + Sync)) -> Option<&ProxyError> {
    e.downcast_ref::<ProxyError>()
}
```

`pingora_core::prelude` (`lib.rs:125-131`) trae `Opt`, `Server`, `background_service`, `HttpPeer` y
**todo** `pingora_error` (`ErrorType::*` + `*`), así que `Error`, `BError`, `ErrorSource` y `Result`
no requieren un `use` aparte. Lo que **no** está en ningún prelude y hay que importar a mano:
`pingora_proxy::FailToProxy` (crate root, `lib.rs:103`) y `pingora_core::server::configuration::ServerConf`
(`pingora-proxy::prelude` tampoco lo expone; su contenido es solo `{http_proxy, http_proxy_service,
ProxyHttp, ProxyWarnLogContext, Session}`, `lib.rs:105-107`).

`Error::create` y no `Error::because`: `because` fija `esource = Unset`, que el default de
`fail_to_proxy` traduce en 500 aunque el caso sea un 403 de SSRF.

Sobrescribir `fail_to_proxy` (respeta el contrato JSON y el nivel del diccionario):

```rust
async fn fail_to_proxy(
    &self,
    session: &mut Session,
    e: &BError,
    ctx: &mut Self::CTX,
) -> FailToProxy {
    let recovered = proxy_error_of(e.root_cause());
    let status = match recovered {
        Some(pe) => pe.to_http_status(),
        // Sin ProxyError: replica las reglas del default para no perder la semántica del motor.
        None => match e.esource() {
            ErrorSource::Upstream => 502,
            ErrorSource::Downstream => 400,
            ErrorSource::Internal | ErrorSource::Unset => 500,
        },
    };
    let body = ErrorResponse {
        error: match recovered {
            Some(pe) => pe.to_error_code().to_string(),
            None => String::from("internal_error"),
        },
        // Un 5xx no filtra el motivo interno al cliente: eso vive solo en el log.
        message: if status >= 500 { String::from("Internal proxy error") } else { e.to_string() },
    };
    write_json_error(session, status, &body, ctx, recovered).await;
    FailToProxy { error_code: status, can_reuse_downstream: false }
}
```

`recovered` es el `ProxyError` encadenado; si el fallo vino del motor (sin `ProxyError`), el contrato
se cumple igualmente con `internal_error`.

## Log del error

`log_domain_error` es el único punto que registra un `ProxyError`. Vive en `src/domain/errors.rs` (no
depende de Pingora ni de Axum) y aplica las tres reglas del encabezado: nivel de `log_level()`,
`event = to_error_code()`, campos de `error_fields()`.

`tracing` exige que los nombres de campo sean literales en tiempo de compilación, así que la lista
dinámica de `error_fields()` se emite agrupada en `fields` como pares `k=v`; los campos fijos
(`event`, `status`, `error_message`) son los que indexan las consultas de logs. `crypt_id`,
`internal_id`, `config_version` y `ip_address` los aporta el span del request (nota † de la tabla),
no este helper: **quien lo llama** entra el span (`ctx.span.in_scope(|| log_domain_error(&e))` en el
camino proxy; en Axum, `ApiError` carga el `Span` y `into_response()` lo entra antes de registrar,
porque el evento se emite fuera del handler). El helper no recibe ni crea spans, porque `domain/` no
conoce `ProxyCtx`.

```rust
use tracing::Level;   // el nivel lo devuelve log_level()

/// Nivel por variante; `event` = error_code del diccionario. Único punto que registra un
/// `ProxyError`: quien lo llama ya está dentro del span del request.
pub fn log_domain_error(e: &ProxyError) {
    let campos = e
        .error_fields()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");

    // `tracing::event!` mete el nivel en un `static`, así que rechaza un `Level` dinámico (E0435).
    macro_rules! emitir {
        ($nivel:ident) => {
            tracing::$nivel!(
                event = e.to_error_code(),
                status = e.to_http_status(),
                error_message = %e,
                fields = %campos,
                "proxy error"
            )
        };
    }

    match e.log_level() {
        Level::TRACE => emitir!(trace),
        Level::DEBUG => emitir!(debug),
        Level::INFO => emitir!(info),
        Level::WARN => emitir!(warn),
        Level::ERROR => emitir!(error),
    }
}
```

El nivel se selecciona con `match` sobre `log_level()`, no pasándolo como expresión a
`tracing::event!`: la macro lo usa para construir un `tracing::level_defs::Level` en un `static`, de
modo que un `Level` en tiempo de ejecución es `E0435`. Lo que sí está prohibido es derivar el nivel
del rango del status (`if status >= 500 { error! … }`): la variante manda. El helper se llama **una
sola vez** por request, al materializar la respuesta de error: `logging()` ya registra el error del
motor al final del ciclo, y duplicar el evento convertiría el diccionario en ruido.

## Escritura de la respuesta en el proxy

Unica función que escribe errores del camino proxy; la usan tanto los `short-circuit` de los filtros
como `fail_to_proxy`, para que el log y el body no se bifurquen.

```rust
async fn write_json_error(
    session: &mut Session,
    status: u16,
    body: &ErrorResponse,
    ctx: &ProxyCtx,
    pe: Option<&ProxyError>,
) {
    let payload = serde_json::to_vec(body).unwrap_or_else(|_| {
        Vec::from(r#"{"error":"internal_error","message":"Internal proxy error"}"#)
    });
    let bytes = Bytes::from(payload);

    let mut header = match ResponseHeader::build(status, Some(bytes.len())) {
        Ok(h) => h,
        Err(e) => {
            // status no convertible: se responde 500 con el mismo contrato
            tracing::error!(error = %e, status, "status de error inválido, se degrada a 500");
            match ResponseHeader::build(500u16, Some(bytes.len())) {
                Ok(h) => h,
                Err(e) => { tracing::error!(error = %e, "no se pudo construir la respuesta de error"); return; }
            }
        }
    };

    // El status real siempre viene de ProxyError::to_http_status(), que es el que recibió `status`.
    let mut extra: Vec<(&'static str, String)> = vec![
        ("content-type", "application/json".to_string()),
        ("x-cache", ctx.cache.as_str().to_string()),
    ];
    if status == 429 {
        if let Some(secs) = pe.and_then(ProxyError::retry_after_secs) {
            extra.push(("retry-after", secs.to_string()));
        }
    }
    for (name, value) in extra {
        if let Err(e) = header.insert_header(name, value) {
            tracing::error!(error = %e, header = name, "cabecera de error rechazada");
        }
    }

    // 1) log: event + campos del diccionario, en el nivel del diccionario
    match pe {
        Some(err) => log_domain_error(err),
        None => tracing::error!(
            event = "internal_error",
            status,
            error_message = %body.message,
            "Server error"
        ),
    }

    // 2) respuesta: cabeceras y cuerpo en dos tiempos
    if status == 429 || status == 403 {
        // rate_limit_exceeded y los bloqueos de seguridad: el cliente debe reabrir la conexión
        session.set_keepalive(None);
    }
    if let Err(e) = session.write_response_header(Box::new(header), !bytes.is_empty()).await {
        tracing::warn!(error = %e, "el cliente cerró antes de la respuesta de error");
        return;
    }
    if !bytes.is_empty() {
        if let Err(e) = session.write_response_body(Some(bytes), true).await {
            tracing::warn!(error = %e, "el cliente cerró durante el cuerpo de error");
        }
    }
}
```

Firmas verificadas contra el fuente de `pingora-proxy` / `pingora-core` / `pingora-http` del rev
pineado en `Cargo.lock`:

- `pingora_proxy::Session::write_response_header(&mut self, resp: Box<ResponseHeader>, end_of_stream: bool) -> Result<()>` (`pingora-proxy/src/lib.rs:715`)
  — pasa por `response_header_filter` de los módulos (`:720-721`), así que `X-Cache` y las cabeceras del motor se
  conservan; no usar `HttpSession::write_response_header` directo.
- `pingora_proxy::Session::write_response_body(&mut self, body: Option<Bytes>, end_of_stream: bool) -> Result<()>` (`:740`)
- `pingora_proxy::Session` implementa `Deref<Target = HttpSession<DS>>` con `DS = ()` (`:1151-1160`), y por
  eso `set_keepalive(Option<u64>)` está disponible sin bajar a `as_downstream_mut()`.
- `ResponseHeader::build(code: impl TryInto<StatusCode>, size_hint: Option<usize>) -> Result<Self>`
- `ResponseHeader::insert_header(&mut self, name: impl IntoCaseHeaderName, value: impl TryInto<HeaderValue>) -> Result<()>`
  — `&'static str` implementa `IntoCaseHeaderName`; el nombre conserva el casing recibido, por eso se
  escriben en minúsculas (`x-cache`, `retry-after`).
- `session.respond_error(code)` / `respond_error_with_body(code, bytes)` **no** se usan. En este rev
  `Session::generate_error` (`pingora-core/src/protocols/http/server.rs:621-628`) devuelve
  502/400 desde las estáticas de `error_resp.rs` y el resto desde `gen_error_response` (`:26-36`),
  que fija `server`, `date`, `content-length: 0` y `cache-control: private, no-store` — **sin
  `Content-Type`** (no es `text/html`) y sin `Retry-After`; `respond_error` además manda cuerpo
  vacío, y `respond_error_with_body` solo ajusta el `content-length`, así que tampoco puede emitir
  el JSON del contrato. Además `write_error_response` llama `set_keepalive(None)`
  (`server.rs:646-653`), es decir cierra la conexión downstream por sí solo: el control de la
  reutilización tiene que quedar en `FailToProxy.can_reuse_downstream`, no repartido.

Un error de escritura aquí **no** sube al filtro que lo invocó: el request ya está condenado y el
`logging()` final registrará el fallo de escritura. Propagar `Err` desde `fail_to_proxy` o desde un
short-circuit causaría doble respuesta.

## Camino Axum (API de control)

`/api/v1/` no pasa por Pingora. El adaptador `IntoResponse` **no puede vivir en `domain/`** (traería
`axum` al dominio); se define un newtype en `src/interfaces/control_api.rs`:

```rust
pub struct ApiError {
    error: ProxyError,
    span: Span,
}

impl ApiError {
    /// `internal_id`/`crypt_id` los aporta el span de la petición (nota †). Como el evento se emite
    /// en `into_response`, ya fuera del handler, el span viaja con el error.
    pub fn with_span(error: ProxyError, span: Span) -> Self { Self { error, span } }
}

impl From<ProxyError> for ApiError {
    fn from(error: ProxyError) -> Self { Self { error, span: Span::current() } }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.error.to_http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let message = if status.is_server_error() {
            String::from("Internal server error")
        } else {
            self.error.to_string()
        };
        let body = ErrorResponse { error: self.error.to_error_code().to_string(), message };

        let _guard = self.span.enter();
        log_domain_error(&self.error);
        drop(_guard);

        // Mismo contrato que el camino proxy: JSON + Retry-After cuando la variante lo trae.
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(secs) = self.error.retry_after_secs() {
            if let Ok(value) = HeaderValue::from_str(&secs.to_string()) {
                headers.insert(RETRY_AFTER, value);
            }
        }
        let payload = serde_json::to_vec(&body)
            .unwrap_or_else(|_| Vec::from(r#"{"error":"internal_error","message":"Internal server error"}"#));
        (status, headers, payload).into_response()
    }
}
```

El 500 de `internal_error` tampoco revela `reason` en el `message` de la API: se loguea y se responde
un mensaje genérico. `From<ProxyError>` captura `Span::current()`, que sirve para los errores que nacen
sin contexto propio (token ausente, doc no hallado); un handler que sí lo tenga —`PUT /config` toma el
`internal_id` tras resolver el token— construye el span explícito y usa `ApiError::with_span`, porque
`into_response()` corre cuando el span del handler ya se cerró.

Los 4xx de `PUT /config` se producen **antes** de escribir en CouchDB: `validate_config_update`
(`domain/validators.rs`) comprueba las cotas de `api_contract.yaml` y devuelve `invalid_config`, así la
API no puede aceptar un documento que un `GET` posterior rechazaría. Un cuerpo que no deserializa
(p. ej. `error_handling.mode = "aggressive"`) se normaliza a 400 `invalid_config` en lugar del 422 en
texto plano que devuelve Axum por defecto.

## Reglas

1. El `error_code` es contrato público: **no se renombra** ni se reutiliza para otra causa. Cambiarlo
   exige actualizar `docs/api_contract.yaml` y `docs/BDD.md` en el mismo commit.
2. Status y nivel solo se leen de `to_http_status()` / `log_level()`. Está prohibido
   `match status { 400..=499 => warn! … }`.
3. Ningún `message` expone credenciales, tokens, rutas internas ni el contenido del cuerpo del origen.
4. Los errores de degradación no cambian el status de la respuesta: se sirve 200 con el cuerpo original
   o el fallback, y el evento queda en el log con su nivel.
5. No hay `.unwrap()` en estas rutas; `serde_json` y `ResponseHeader` se manejan con `match`/`map_err`.
6. Cada variante nueva nace con: fila en la tabla, `to_http_status`, `to_error_code`, `log_level`,
   `error_fields`, y un escenario Gherkin en `specs/`.

## Divergencias conocidas del código contra este diccionario

### Cerradas

Verificadas leyendo `src/domain/errors.rs` y `src/interfaces/control_api.rs`; cada una con su test.

| # | Exigía este documento                                                            | Estado del código                                                                |
| - | -------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| 1 | el adaptador va en `interfaces/control_api.rs` como `ApiError`                     | `IntoResponse` sale de `domain/errors.rs`; `domain/` ya no importa `axum`          |
| 2 | nivel por variante; `ssrf_blocked`/`invalid_crypt_id` = ERROR, `script_timeout` = WARN | `log_level()` casea la variante, no el rango del status                            |
| 3 | `event = <error_code>` + `error_fields()` + campos del span                        | `log_domain_error` emite `event`/`status`/`error_message`/`fields`; `ApiError` entra el span |
| 4 | `Retry-After` obligatorio en 429                                                   | ambas rutas lo emiten desde `retry_after_secs()`                                   |
| 5 | `RateLimitExceeded` carga `retry_after_secs`                                       | la variante trae `current_count`/`max_requests`/`retry_after_secs`                 |

### Pendientes

| # | Estado del código                                                                             | Este documento exige                                                                       | Bloqueado en |
| - | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ------------ |
| 6 | no hay mapeo a `BError`                                                                        | `impl From<ProxyError> for BError` en `application/proxy_service.rs`                          | Fase 3 (`src/` aún no importa `pingora`) |
| 7 | los 4xx del proxy los produce el handler Axum (`interfaces/proxy_handler.rs`), que bufferiza el cuerpo | short-circuit en `request_filter` + `write_json_error`                                   | Fase 3        |
| 8 | no existe un punto que emita `request_completed` al cerrar cada request | `logging()` emite el evento INFO con `method`/`path`/`status`/`x_cache`/`elapsed_ms` (BDD DoD 5)  | Fase 3        |
| 9 | `?mime=` se aplica sin comparar con el `Content-Type` recibido                     | WARN `mime_discrepancy` con `forced_mime` y `real_mime` cuando difieran                           | sin bloqueo   |

### Dónde se equivocaba este documento

Corregido al cerrar la divergencia #2, verificado compilando con `tracing` 0.1.44:

- Afirmaba que `tracing::event!` acepta el nivel como expresión posicional. **No**: la macro mete el
  nivel en un `static` y un `Level` en tiempo de ejecución es `E0435`. El `match` sobre `log_level()`
  de § Log del error es la forma correcta, no un capricho del estilo.
- Mostraba `pub struct ApiError(pub ProxyError);`, que no podía cumplir la nota †: sin infraestructura
  de spans en la API de control, `internal_id` jamás llegaba al evento. El newtype real carga el
  `Span` (§ Camino Axum).
