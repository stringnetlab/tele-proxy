# Variables de Entorno

Lista **canónica** de las variables de `tele-proxy`. Ninguna variable que no aparezca aquí debe
leerse desde el código: si hace falta una nueva, se documenta primero en este archivo y en el
`docker-compose.yml`.

Estado de cada variable:

- **activa** — la lee hoy `AppConfig::from_env()` (`src/main.rs:33`) o el `EnvFilter` de tracing.
- **designada** — la prescribe `docs/spec.md` pero todavía no está cableada; la columna *Dónde*
  indica el módulo y la fase que la consume. Un `grep` de la variable en `src/` debe encontrar su
  punto de lectura antes de cerrar esa fase.

## Aplicación

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `RUST_LOG` | Filtro de `tracing-subscriber` (`EnvFilter`) | `warn,tele_proxy=info` | No | **activa** — `init_tracing()` (`main.rs:88`); el default se usa solo si la variable no está |
| `RUST_BACKTRACE` | Backtrace en pánico | `1` | No | la consume el runtime de Rust, no el código |
| `MODO` | `desarrollo` → errores HTTP verbosos (motivo interno + `details`) y `GET /` lista los endpoints; cualquier otro valor o ausente → respuestas escuetas | escueto | No | **activa** — `AppConfig::from_env()` (main.rs); viaja a `ProxyService::with_verbose_errors` |

El efecto de `MODO` son dos, ambos sobre las **respuestas HTTP**: (1) la verbosidad de los
cuerpos de error que devuelven `/aq/` y `/api/v1/` — en `desarrollo` un 5xx expone el motivo
interno completo y un objeto `details` con los campos estructurados del error (`reason`,
`domain`, `retry_after_secs`, ...), y en cualquier otro modo los 5xx siguen siendo
`{"error":"internal_error","message":"Error interno del servidor"}` y los 4xx mantienen su mensaje
actual sin `details`; y (2) el cuerpo de `GET /` en el puerto del proxy — en `desarrollo`
incluye la lista de endpoints definidos, en producción solo el nombre del proyecto (`TELE -
PROXY`). La comparación es case-insensitive (`DESARROLLO` también activa). No existe
`RUST_ENV`: el entorno se distingue por `RUST_LOG` (logs) y `MODO` (cuerpos de respuesta), y el
binario corre en primer plano dentro del contenedor (ver *Runtime de Pingora*).

## Runtime de Pingora (`ServerConf`)

Con Pingora, el número de threads y el apagado ordenado **no** son variables de Axum: se fijan en
`pingora_core::server::configuration::ServerConf`, construido en `main.rs`. Defaults verificados en
el rev pineado de `pingora-core` (`server/configuration/mod.rs:219-245`): `threads: 1` (`:236`),
`work_stealing: true` (`:238`), `upstream_keepalive_pool_size: 128` (`:240`),
`grace_period_seconds: None` (`:245`), `daemon: false` (`:228`). No se citan los de crates.io 0.9.0:
la API que importa es la del rev de `Cargo.lock` (ver *Rev de Pingora* en `spec.md`).

| Variable | Campo de `ServerConf` | Default nuestro | Default Pingora | Req |
| --- | --- | --- | --- | --- |
| `PROXY_WORKER_THREADS` | `threads: usize` | `std::thread::available_parallelism()` | **1** | No |
| `PROXY_WORK_STEALING` | `work_stealing: bool` | `true` | `true` | No |
| `PROXY_GRACE_PERIOD_SECONDS` | `grace_period_seconds: Option<u64>` | `10` | `None` | No |
| `UPSTREAM_KEEPALIVE_POOL_SIZE` | `upstream_keepalive_pool_size: usize` | `128` | `128` | No |

> **Estado real**: hoy **ninguna** de las cuatro es leída por `main.rs` — el binario sigue arrancando
> dos listeners Axum con `#[tokio::main]` y no construye `ServerConf`. Quedan documentadas como el
> contrato que la portación debe cumplir (`spec.md`, "Estado de la migración Axum/reqwest -> Pingora",
> Fase 3). Un operador que las fije en el `.env` no observa ningún efecto hasta que la portación esté
> terminada; cambiar `PROXY_WORKER_THREADS` hoy no cambia nada.

> **Trampa**: el `threads` de Pingora vale **1** por defecto. Si `PROXY_WORKER_THREADS` no se fija
> explícitamente desde el número de CPU, el proxy sirve tráfico mono-thread aunque la máquina tenga
> 8 núcleos. Por eso `ServerConf` se construye y se completa **antes** de
> `Server::new_with_opt_and_conf(opt, conf)`: el constructor guarda `configuration: Arc<ServerConf>`
> (`server/mod.rs:478`) y ya no hay forma cómoda de cambiarlo después; `http_proxy_service` recibe
> precisamente `&server.configuration`.

> `UPSTREAM_KEEPALIVE_POOL_SIZE` mapea a un campo declarado **inestable** por Pingora
> (`upstream_keepalive_pool_size`, `server/configuration/mod.rs:108-110`: "may be renamed or removed
> in the future"). Al subir el rev hay que revisar que el campo siga existiendo; si desaparece, el
> pool se configura vía `ConnectorOptions` y esta variable pasa a documentarse como *no aplicable*.

`Opt` (`server/configuration/mod.rs:270-310`) solo porta flags de CLI: `-u/--upgrade`,
`-d/--daemon`, `-t/--test`, `-c/--conf` (y un `--nocapture` oculto para `cargo test`). **No** existe
`--no-daemon` en este rev: para no daemonizarse simplemente no se pasa `-d`, ya que `daemon: false`
es el default del conf. **No** se usa el YAML de `-c`: la configuración del servidor se construye en
código a partir de estas variables (`new_with_opt_and_conf` ignora la ruta de `-c` con un `warn!`,
`server/mod.rs:453-460`). El proceso **nunca** hace `daemon: true` (Docker/Kubernetes ya supervisan
el proceso; un daemonizado rompe el `HEALTHCHECK` y el `stop_grace_period`).

## Escucha HTTP

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `HTTP_HOST` | Host de escucha de ambos servicios | `0.0.0.0` | No | **activa** |
| `HTTP_PROXY_PORT` | Puerto del endpoint público `/aq/` | `8080` | No | **activa** |
| `HTTP_CONTROL_PORT` | Puerto de la API de control `/api/v1/` | `8081` | No | **activa** |

Pingora recibe la dirección ya compuesta como `&str`: `service.add_tcp(&format!("{HTTP_HOST}:{HTTP_PROXY_PORT}"))`.
La API de control (`axum`) se enlaza con la misma notación dentro de su `BackgroundService`. No hay
variables `HTTP_PROXY_ADDR`/`HTTP_CONTROL_ADDR`: un solo host + dos puertos.

## CouchDB

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `COUCHDB_URL` | URL de conexión | `http://couchdb:5984` | Sí | **activa** (con default, así que el arranque no falla si falta) |
| `COUCHDB_USER` | Usuario | — | **Sí, sin default** | **activa** — `main.rs:43`, falla el arranque si falta |
| `COUCHDB_PASSWORD` | Contraseña | — | **Sí, sin default** | **activa** — `main.rs:45`, falla el arranque si falta |
| `COUCHDB_DB_NAME` | Base de datos | `tele_proxy_configs` | No | **activa** — la base **se crea a mano** (ver `DEPLOYMENT.md`) |
| `COUCHDB_TIMEOUT_MS` | Timeout de conexión | `5000` | No | designada — `infrastructure/couchdb.rs`, Fase 1 |
| `COUCHDB_MAX_RETRIES` | Reintentos del feed `_changes` | `3` | No | designada — polling del feed, Fase 5 |

## Valkey

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `VALKEY_URL` | URL completa (formato Redis) | — | Sí | **activa** — se compone desde `VALKEY_PASSWORD`, nunca con la contraseña literal |
| `VALKEY_PASSWORD` | Contraseña, inyectada por compose/`.env` | — | **Sí** | **activa** — solo como `${VALKEY_PASSWORD}` en el `docker-compose.yml` |
| `VALKEY_MAX_CONNECTIONS` | Conexiones del pool | `50` | No | designada — `infrastructure/valkey.rs`, Fase 1 |
| `VALKEY_TIMEOUT_MS` | Timeout de operaciones | `3000` | No | designada — Fase 1 |

`VALKEY_URL` se construye como `redis://:${VALKEY_PASSWORD}@valkey:6379`. Las contraseñas **no** se
escriben en este repositorio ni en ningún documento: el valor real vive en el `.env` local (ignorado)
y en el secret store de Dokploy.

## Caché

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `CONFIG_CACHE_TTL_SECONDS` | TTL de la caché Moka de configs | `300` | No | **activa** |
| `CONFIG_CACHE_MAX_CAPACITY` | Entradas máximas de Moka | `10000` | No | **activa** |
| `DEFAULT_CACHE_TTL_SECONDS` | TTL de respuestas cacheadas en Valkey | `3600` | No | designada — Fase 3 |
| `IP_CACHE_TTL_SECONDS` | TTL de la caché LRU de DNS | `300` | No | designada — `infrastructure/dns_resolver.rs`, Fase 2 |
| `MAX_CACHEABLE_SIZE_BYTES` | Tamaño máximo cacheable (5 MB) | `5242880` | No | designada — decide `CacheDirective::Bypass`, Fase 3 |

## Límites del sistema

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `MAX_RESPONSE_SIZE_BYTES` | Tamaño máximo de respuesta del origen (100 MB) | `104857600` | No | designada — `upstream_response_body_filter`, Fase 3 |
| `MAX_SCRIPTING_BODY_BYTES` | Cuerpo máximo que entra a Lua (5 MB) | `5242880` | No | designada como default: el bypass decide por `ClientConfig.max_scripting_body_bytes` (documento CouchDB), no por esta variable |
| `UPSTREAM_USER_AGENT` | User-Agent por defecto para las peticiones al origen cuando el **cliente no envía el suyo**. Vacío = el de reqwest. El `User-Agent` del cliente, si llega, siempre se reenvía tal cual (lo mismo `Accept-Language`) | `""` | No | **activa** — `AppConfig::from_env()` → `ProxyService::with_upstream_user_agent` |
| `LUA_TIMEOUT_MS` | Deadline de ejecución Lua | `200` | No | **activa** — parseada y validada por `parse_in_range` (`main.rs:57`, la función en `main.rs:75`) |
| `LUA_MEMORY_LIMIT_MB` | Límite de memoria del sandbox | `50` | No | **activa** |
| `WEBHOOK_TIMEOUT_MS` | Timeout de `proxy.http_request` | `5000` | No | **activa** — parseada por `parse_in_range` (`main.rs:61`); es el techo de `WebhookRequest.timeout_ms` (`application/webhook_service.rs:88`) y el deadline del puente Lua->tokio (`application/lua_engine.rs:228`) |

## Rate limiting

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `DEFAULT_RATE_LIMIT_REQUESTS` | Peticiones por ventana cuando el cliente no la fija | `50` | No | designada — Fase 5 |
| `DEFAULT_RATE_LIMIT_WINDOW_SECONDS` | Ventana por defecto | `60` | No | designada — Fase 5 |
| `GLOBAL_RATE_LIMIT_RPM` | Techo global del sistema | `10000` | No | designada — `pingora-limits` in-process, Fase 5 |

El límite por cliente viene del documento CouchDB (`rate_limit.max_requests`, `window_seconds`); estas
variables solo aportan el default cuando el documento omite el campo.

## DNS (DoT + DNSSEC)

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `DNS_TIMEOUT_MS` | Timeout de resolución | `3000` | No | designada — Fase 2 |
| `DNS_RETRIES_PER_SERVER` | Reintentos por servidor | `2` | No | designada — Fase 2 |
| `DNS_CACHE_SIZE` | Entradas de la LRU | `256` | No | designada — Fase 2 |
| `DNS_DOT_SERVERS` | Cadena de fallback, en orden | `dns.quad9.net:853,security.cloudflare-dns.com:853,dns.adguard-dns.com:853,security-dns.nl:853,dns.google:853` | No | designada — la lista **no** puede fijarse por default silencioso si el operador exige otro proveedor |

La lista **primaria** no es una variable de entorno: es `config/dns_resolvers.json`, compilada en el
binario con `include_str!` (`main.rs:157`) y parseada por `SecureDnsResolver::from_json`. Ahí viven
los `ip`, `port`, `server_name`, `dnssec` y `ecs` por proveedor, además de `timeout_ms`,
`retries_per_server`, `cache_size` e `ip_cache_ttl_seconds` en `settings`. `DNS_DOT_SERVERS` es
únicamente la cadena de reserva cuando el JSON no basta; los cuatro valores de `settings` se leen de
las variables `DNS_*`/`IP_CACHE_TTL_SECONDS` cuando esas fases cableen su lectura.

> **Nombre de CleanBrowsing**: el `server_name` correcto es `security-dns.nl` (el SNI del resolvedor
> neerlandés, que es el que resuelve `185.228.168.9`), no `dns.cleanbrowsing.org` — ese dominio es el
> de la documentación y su certificado no ampara el resolvedor. Si el operador prefiere el endpoint
> estadounidense, el SNI es `security-dns.us` con `185.228.169.9`.

## Seguridad

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `CRYPT_ID_LENGTH` | Longitud del nanoid de `crypt_id` | `12` | No | designada — el validador hoy compara `12` fijo (`RUST_STYLE_GUIDE.md:435`) |
| `BEARER_TOKEN_HASH_ALGORITHM` | Algoritmo de hash de tokens Bearer | `sha256` | No | designada — middleware de `/api/v1/`, Fase 5 |
| `MASTER_BEARER_TOKEN` | Fallback de emergencia para la API admin (`/api/v1/admin/*`). **Solo** autoriza rutas admin: nunca la API de cliente ni el proxy. En memoria se guarda siempre hasheado (`sha256:<hex>`); vacío/ausente = master deshabilitado | vacío (deshabilitado) | No | **activa** — `admin_settings_from_env()` (`main.rs`), verificado en `AdminService::verify_master_token` |

El token Bearer en sí **no** es una variable de entorno: se valida por hash contra CouchDB.

## Administración

Variables de la administración global (panel `/admin/` + API `/api/v1/admin/*`). El login es
Google Social Login (OAuth2 + OIDC) con lista de dominios permitidos; el master token es el
fallback de emergencia. Toda mutación queda auditada (`event=admin_audit` en el log).

| Variable | Descripción | Default | Req | Dónde |
| --- | --- | --- | --- | --- |
| `GOOGLE_CLIENT_ID` | Client ID OAuth2 de Google. Ausente = login Google deshabilitado (la API admin sigue con master token; `/auth/google/url` responde 503) | — | No | **activa** — arranque, `GoogleIdentityProvider` |
| `GOOGLE_CLIENT_SECRET` | Client secret OAuth2 de Google | — | No | **activa** — idem |
| `GOOGLE_REDIRECT_URI` | Callback del OAuth flow. También decide el flag `Secure` de la cookie de sesión (solo si el esquema es `https`) | `http://127.0.0.1:{HTTP_CONTROL_PORT}/api/v1/admin/auth/google/callback` | No | **activa** — idem |
| `ADMIN_ALLOWED_DOMAINS` | Dominios cuyos emails pueden ser admin, separados por comas. Comparación **exacta** case-insensitive: un subdominio no coincide con el dominio base. Lista vacía = el arranque falla | `gmail.com,stringnet.pe` | No | **activa** — validada por `validate_allowed_domain_list` |
| `ADMIN_SESSION_TTL_SECONDS` | Vida de la sesión admin en Valkey (cookie `teleproxy_admin`, HttpOnly + SameSite=Lax) | `3600` | No | **activa** — `parse_in_range` |
| `ADMIN_OAUTH_STATE_TTL_SECONDS` | Vida del state OAuth (un solo uso, `GETDEL`) | `300` | No | **activa** — `parse_in_range` |
| `ADMIN_LOGIN_RATE_LIMIT_REQUESTS` | Cuota por IP de las rutas de login (`/auth/google/url`, `/auth/google/callback`) | `5` | No | **activa** — `parse_in_range`, limiter local |
| `ADMIN_LOGIN_RATE_LIMIT_WINDOW_SECONDS` | Ventana del rate limit de login | `60` | No | **activa** — `parse_in_range` |
| `ADMIN_RATE_LIMIT_REQUESTS` | Cuota por IP del resto del router admin | `120` | No | **activa** — `parse_in_range` |
| `ADMIN_RATE_LIMIT_WINDOW_SECONDS` | Ventana del rate limit general admin | `60` | No | **activa** — `parse_in_range` |
| `CONTROL_RATE_LIMIT_REQUESTS` | Cuota por IP de la API de control de clientes (`/api/v1/clients/*`; hallazgo A1) | `60` | No | **activa** — `parse_in_range` |
| `CONTROL_RATE_LIMIT_WINDOW_SECONDS` | Ventana del rate limit de la API de control | `60` | No | **activa** — `parse_in_range` |
| `GIT_SHA` | Hash del commit desplegado; `GET /` lo devuelve tal cual para visibilidad del release. **Se deriva solo en el build**: el Dockerfile resuelve `git rev-parse HEAD` sobre el `.git` del contexto (por eso `.dockerignore` ya no lo excluye) y lo hornea en el binario. En runtime solo se lee si se fija explícitamente (vacío = el horneado manda) | derivado del `.git` en build; `desconocido` si no hay `.git` | No | **activa** — `commit_sha` (main.rs), build en Dockerfile |

Los rate limits de estas APIs son por proceso (`LocalRateLimiter`, LRU acotada): son endpoints
internos/loopback, así que la cuota por instancia es suficiente y no hace falta el contador
distribuido de Valkey (ese sigue reservado al límite por `crypt_id` del proxy público). Las
rutas de auth además llevan body cap de 8 KB (`DefaultBodyLimit`).

## Validaciones

Rangos aceptados; fuera de ellos el arranque **falla** (no se degrada al default):

| Regla | Rango | Quién la aplica | Estado |
| --- | --- | --- | --- |
| Timeouts en ms (`*_TIMEOUT_MS`) | `1..=60000` | `parse_in_range` (`main.rs:75`), invocado desde `AppConfig::from_env` | **activa** para `LUA_TIMEOUT_MS` y `WEBHOOK_TIMEOUT_MS`; las demás (`COUCHDB_`, `VALKEY_`, `DNS_`) están designadas |
| `ADMIN_SESSION_TTL_SECONDS` | `300..=86400` | `parse_in_range` en `admin_settings_from_env` | **activa** |
| `ADMIN_OAUTH_STATE_TTL_SECONDS` | `60..=3600` | idem | **activa** |
| `ADMIN_LOGIN_RATE_LIMIT_REQUESTS` | `1..=100` | idem | **activa** |
| `ADMIN_LOGIN_RATE_LIMIT_WINDOW_SECONDS` | `1..=3600` | idem | **activa** |
| `ADMIN_RATE_LIMIT_REQUESTS` / `CONTROL_RATE_LIMIT_REQUESTS` | `1..=10000` | idem | **activa** |
| `ADMIN_RATE_LIMIT_WINDOW_SECONDS` / `CONTROL_RATE_LIMIT_WINDOW_SECONDS` | `1..=3600` | idem | **activa** |
| `ADMIN_ALLOWED_DOMAINS` | lista no vacía de dominios válidos | `validate_allowed_domain_list` | **activa** — lista vacía o dominio inválido aborta el arranque |
| Límites de tamaño en bytes (`MAX_*`) | `1..=1073741824` | idem | pendiente — las tres `MAX_*` están designadas, nadie las parsea todavía |
| `MAX_SCRIPTING_BODY_BYTES` | `<= MAX_CACHEABLE_SIZE_BYTES` o bypass coherente | Fase 4 | el bypass **está implementado**, pero decide el campo del documento CouchDB (`ClientConfig.max_scripting_body_bytes`, leído en `application/lua_engine.rs:437`), no esta variable; su cota (`1048576..=52428800`) la valida `PUT /config`. La relación con el tamaño cacheable sigue sin forzarse |
| Rate limits | `1..=100000` RPM | Fase 5 | pendiente |
| `CRYPT_ID_LENGTH` | `8..=24` | Fase 1 | pendiente — el validador compara `12` fijo |
| `PROXY_WORKER_THREADS` | `1..=` paralelismo real del host | arranque | pendiente — ver la trampa de más abajo |

> **Trampa**: la fila `PROXY_WORKER_THREADS` dice *arranque*, pero el arranque aún no lee esa variable
> (§ Runtime de Pingora). Hasta la Fase 3 ninguna implementación puede rechazar un valor absurdo; el
> rango se cablea junto con `ServerConf`.

`parse_in_range` es el único punto que convierte una variable de entorno numérica en un valor con
rango: si `LUA_TIMEOUT_MS=99999`, el proceso **no** arranca. Verificado ejecutando el binario, que
emite un único evento y sale con código 1:

```json
{"timestamp":"…","level":"ERROR","fields":{"message":"Error de configuración, abortando el arranque","error":"Invalid LUA_TIMEOUT_MS: 99999 is outside 1..=60000"}}
```

El mensaje sale por `tracing`, no por `eprintln!` (`RUST_STYLE_GUIDE.md:109` lo prohíbe):
`init_tracing()` (`main.rs:88`) se llama **antes** de `AppConfig::from_env()`, así que el fallo de
arranque viaja por el mismo canal JSON que el resto de logs. `tracing_subscriber::fmt` escribe en
**stdout**, que es lo que recoge el driver de logs del contenedor.

Los límites **por cliente** (`max_requests 1..=10000`, `window_seconds 1..=3600`,
`max_scripting_body_bytes 1048576..=52428800`, `mode in {transparent, wrapped}`, `code_hash`) no se
leen del entorno: los valida `PUT /api/v1/clients/config` y responde **400** (ver `spec.md`, Fase 5).

## Ejemplo de uso en Docker Compose

Refleja el bloque `environment:` de `docker-compose.yml` a fecha de este documento; si el compose
cambia, cambia esta lista (no al revés):

```yaml
services:
  tele-proxy:
    environment:
      - RUST_LOG=${RUST_LOG:-warn,tele_proxy=info}
      - RUST_BACKTRACE=${RUST_BACKTRACE:-1}
      - HTTP_HOST=0.0.0.0
      - HTTP_PROXY_PORT=8080
      - HTTP_CONTROL_PORT=8081
      - PROXY_WORKER_THREADS=${PROXY_WORKER_THREADS:-4}
      - PROXY_WORK_STEALING=${PROXY_WORK_STEALING:-true}
      - PROXY_GRACE_PERIOD_SECONDS=${PROXY_GRACE_PERIOD_SECONDS:-10}
      - UPSTREAM_KEEPALIVE_POOL_SIZE=${UPSTREAM_KEEPALIVE_POOL_SIZE:-128}
      - COUCHDB_URL=http://couchdb:5984
      - COUCHDB_USER=${COUCHDB_USER}
      - COUCHDB_PASSWORD=${COUCHDB_PASSWORD}
      - COUCHDB_DB_NAME=${COUCHDB_DB_NAME:-tele_proxy_configs}
      - VALKEY_URL=redis://:${VALKEY_PASSWORD}@valkey:6379
      - LUA_TIMEOUT_MS=${LUA_TIMEOUT_MS:-200}
      - LUA_MEMORY_LIMIT_MB=${LUA_MEMORY_LIMIT_MB:-50}
      - WEBHOOK_TIMEOUT_MS=${WEBHOOK_TIMEOUT_MS:-5000}
    stop_grace_period: 15s
```

La lista **no** es solo de variables activas: `PROXY_*` y `UPSTREAM_KEEPALIVE_POOL_SIZE` están
designadas —el compose las pasa y `ServerConf` las leerá en la Fase 3— y se mantienen aquí porque ya
forman parte del contrato de despliegue. Las tres últimas sí tienen efecto inmediato. Las tres
contraseñas se referencian desde el `.env` del host: ningún valor real se commitea. `VALKEY_URL` se
**compone** con `${VALKEY_PASSWORD}`, así que la contraseña nunca aparece en un documento ni en el
repositorio. El `stop_grace_period` debe superar a `PROXY_GRACE_PERIOD_SECONDS` (ver `DEPLOYMENT.md`).
