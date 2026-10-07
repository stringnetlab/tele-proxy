# 📄 Especificación Técnica: `tele-proxy`

**Repositorio**: `https://github.com/stringnetlab/tele-proxy`  
**Versión**: 1.0.0  
**Fecha**: Octubre 2026  
**Despliegue**: Docker Compose vía Dokploy  
**Enfoque**: API First, Security-Driven, DDD, TDD/BDD.

---

## 🏗️ 1. Arquitectura y Stack Tecnológico

- **Lenguaje**: Rust (Edition 2021)
- **Motor de Proxy**: `pingora` 0.9 (Cloudflare). Crates: `pingora-proxy` (`ProxyHttp`), `pingora-core` (`Server`, `HttpPeer`, `BackgroundService`), `pingora-http` (`RequestHeader`/`ResponseHeader`), `pingora-limits` (declarado, **sin uso**: el rate limit in-process lo da `infrastructure::local_rate_limiter`, ver Fase 3). TLS del upstream con el feature `rustls`.
- **Runtime**: `tokio` + `pingora-runtime`. Con `work_stealing = true` (el default) es **un solo** runtime *work-stealing* con `ServerConf.threads` worker threads (`pingora-runtime/src/lib.rs:471`); con `work_stealing = false` son `threads` runtimes de un hilo cada uno (`:593`). El default de `threads` es **1**, no «uno por CPU»: hay que fijarlo en `ServerConf` (ver `ENVIRONMENT.md`). El proceso no usa `#[tokio::main]` propio: los `spawn_blocking` de Lua se cuelan en el runtime de Pingora.
- **API de Control**: `axum` 0.8 en el **mismo binario**, montada como `BackgroundService` de Pingora (recibe el `ShutdownWatch` y por tanto se apaga de forma ordenada con el proxy).
- **Scripting**: `mlua` (Lua 5.4, sandboxed estricto) en `tokio::task::spawn_blocking`.
- **Base de Datos**: CouchDB (Configuración y metadatos).
- **Caché**: Valkey (Compatible con Redis, serialización `postcard`; ver nota histórica en §3).
- **Caché Local Config**: `moka` (para configs de CouchDB, TTL 5m).
- **DNS**: `hickory-resolver` (DoT con DNSSEC, cadena de fallback) + caché LRU **in-process** (`lru`).
- **IDs**: `nanoid` (12 caracteres, URL-safe).
- **Observabilidad**: `tracing` + `tracing-subscriber` (JSON, solo errores en fase 1).

### Restricción de plataforma (obligatoria)

Pingora es **Linux/Unix-only**: usa `epoll`/`io-uring`, `daemon()` y señales POSIX, y su `Server::run_forever()` no está soportado en Windows. Por tanto:

1. Todo `check`/`clippy`/`test`/`build` que active el feature `proxy` se ejecuta en Linux (Docker `rust:bookworm`).
2. En Windows solo se trabaja el código independiente de plataforma (`domain`, `application`, `infrastructure` sin `ProxyHttp`), con `default = []`.
3. El `Dockerfile` construye con `cargo build --release --locked --features proxy`.

---

## 🔐 2. Modelo de Seguridad de Dos Niveles

### Nivel 1: Endpoint Público (Sin Auth)

- **Ruta**: `GET /aq/<crypt_id>/?url=<encoded_url>&mime=<optional>`
- **`crypt_id`**: Nanoid de 12 caracteres (`A-Za-z0-9_-`).
- **Seguridad**: Whitelist de dominios + Rate Limit por `crypt_id` + Validación estricta de URL (anti-SSRF).
- **Logging**: Solo registra errores (4xx, 5xx, bloqueos).

### Nivel 2: Endpoints de Control (Auth Requerida)

- **Ruta**: `/api/v1/*`
- **Seguridad**: Header `Authorization: Bearer <token>`. Hash del token validado contra CouchDB.
- **Aislamiento**: Un token solo accede a los datos de su `internal_id` asociado.

### Protección Anti-SSRF (Obligatoria)

Los cinco pasos son ineludibles y se mapean al ciclo de vida `ProxyHttp` de Pingora:

| # | Requisito | Fase de Pingora |
|---|-----------|-----------------|
| 1 | Parseo estricto de URL (sin credenciales, sin fragmentos, esquema http/https) | `request_filter` (ver Fase 3: `early_request_filter` queda sin implementar) |
| 2 | Whitelist de dominios (sufijo, caso-insensible) | `request_filter` |
| 3 | Resolución DNS manual vía DoT con DNSSEC (Quad9 ECS -> Cloudflare -> AdGuard -> CleanBrowsing -> Google) y rechazo de IPs privadas (IPv4/IPv6) | `upstream_peer` (antes de construir el `HttpPeer`) |
| 4 | Conexión TCP forzada a la IP validada (previene DNS rebinding) | `HttpPeer::new(SocketAddr::from((ip_validada, puerto)), tls, sni = hostname)` |
| 5 | Validación de redirects (3xx) con la misma lógica de IP privada | `upstream_response_filter` |

Detalles de implementación exigidos:

- **Pinning real (paso 4)**: el peer se construye **siempre** desde un `SocketAddr` obtenido por `SecureDnsResolver`, nunca desde un hostname. Pingora no debe resolver el nombre por su cuenta; el `hostname` original solo viaja en SNI y en el header `Host` (`upstream_request_filter`).
- **Redirects (paso 5)**: Pingora no sigue redirects, reenvía el 3xx al cliente. Por tanto `upstream_response_filter` debe, ante `301/302/303/307/308`, parsear `Location`, volver a pasar `validate_url_strict` + whitelist + `resolve_and_validate`. Si el destino es inválido o apunta a una IP privada, se **elimina `Location`** y se responde `ProxyError::SsrfBlocked { reason: "redirect_to_private_ip" }`; nunca se filtra una `Location` sin validar.
- **Sin credenciales ni scheme engañoso**: `url.username()/password()` vacíos y `scheme in {http, https}`.
- **Cualquier salida de red que no sea el propio proxy** (webhooks `proxy.http_request` desde Lua, fetch de `fallback_urls`, health-check externo) **debe** reutilizar el mismo pipeline de los pasos 1-4 a través de `infrastructure/http_client.rs`. Está prohibido instanciar un cliente HTTP ad-hoc en `application/` o `lua_engine.rs`.

---

## 🗄️ 3. Modelos de Datos

### CouchDB: Documento de Cliente

```json
{
  "_id": "client_int_9f8e7d",
  "type": "client_config",
  "internal_id": "client_int_9f8e7d",
  "crypt_id": "V1StGXR8_Z5j",
  "bearer_token_hash": "sha256:...",
  "config_version": 1,
  "whitelist": ["shutterstock.com", "gettyimages.com"],
  "rate_limit": { "max_requests": 50, "window_seconds": 60 },
  "max_scripting_body_bytes": 5242880,
  "scripting": {
    "enabled": true,
    "code": "function handle(req, res) ... end",
    "code_hash": "sha256:abc123..."
  },
  "error_handling": {
    "mode": "wrapped",
    "fallback_urls": {
      "image/*": { "url": "https://cdn.../error.png", "hash": "sha256:..." }
    }
  }
}
```

### Valkey: Claves

| Clave | Contenido | TTL |
|-------|-----------|-----|
| `px:{internal_id}:{config_version}:{sha256(url)}` | Respuesta cacheable (cabeceras + cuerpo), serializada con **postcard** | Dinámico por MIME: `image/*` 3600s, `text/css`/`application/javascript` 1800s, resto 300s |
| `px:{internal_id}:{config_version}:{sha256(url)}:stale` | Copia del `px:` anterior para servir como fallback (Nivel 1) | 86400s |
| `rl:{crypt_id}` | Contador de rate limit (`INCR` + `PEXPIRE` atómicos) | `window_seconds` |
| `px:defaults:{mime}` | Fallback global (Nivel 2) | Sin TTL |

- **Serialización**: `postcard` (feature `alloc`). Motivo: `bincode` no se mantiene (la versión 3.0.0 de crates.io es un `compile_error!` y la 1.3 está en fin de vida); `postcard` es `serde`-based y activo. Cualquier referencia a `bincode` en documentación previa es obsoleta.
- **Caché de IPs**: vive en **proceso** (`lru::LruCache`, capacidad `cache_size`, TTL `ip_cache_ttl_seconds` de `config/dns_resolvers.json`), **no** en Valkey. Razón: el pinning anti-rebinding debe atarse al ciclo de vida del resolver y compartir el `SocketAddr` ya validado con `upstream_peer`; un `GET` a Valkey añadiría latencia y una segunda fuente de verdad. La clave `ip_cache:{hostname}` queda reservada pero no la escribe nadie.
- **Codificación de la caché de respuesta en Pingora**: el cuerpo se almacena ya *decodificado* (Pingora no descomprime por sí mismo; si el upstream manda `Content-Encoding`, la respuesta se marca como **no cacheable** y se reenvía tal cual, o bien se descomprime explícitamente antes de guardar — la decisión está en `docs/DIAGRAMS.md`).

---

## 🚀 4. Plan de Desarrollo por Fases

El agente de código debe implementar **una fase a la vez**. No avanzar a la siguiente hasta que la fase actual compile, tenga pruebas y esté commiteada.

### Fase 1: Cimientos y Estructura DDD

- [ ] Inicializar repositorio en `stringnetlab/tele-proxy` vía `gh repo create`.
- [ ] Crear `Cargo.toml` con todas las dependencias.
- [ ] Estructurar carpetas: `src/domain`, `src/application`, `src/infrastructure`, `src/interfaces`.
- [ ] Definir `Traits` base (`ConfigFetcher`, `CacheStore`, `LuaExecutor`).
- [ ] Crear `docker-compose.yml` base con CouchDB, Valkey y el servicio Rust.
- [ ] Configurar `tracing` para logs JSON estructurados (nivel ERROR/WARN).

### Fase 2: Seguridad de Red y DNS (Anti-SSRF)

- [ ] Embeber `config/dns_resolvers.json` (cadena real del archivo, en orden de `priority`: Quad9 ECS → Cloudflare Security → AdGuard → CleanBrowsing Security → Google Public DNS; todos `port: 853`, `protocol: "dot"`, `dnssec: true`).
- [ ] Implementar `hickory-resolver` con DoT y DNSSEC.
- [ ] Crear función `resolve_and_validate(hostname)` que rechace IPs privadas.
- [ ] Implementar validador estricto de URLs (`validate_url_strict`).
- [ ] Escribir pruebas unitarias para detección de IPs privadas y URLs malformadas.

### Fase 3: Motor de Proxy (Pingora) y Caché

Implementar `impl ProxyHttp for ProxyService` en `src/application/proxy_service.rs` con `type CTX = RequestContext` (estado por solicitud: `crypt_id`, `internal_id`, `config_version`, URL validada, `SocketAddr` pinneado, `X-Cache` decidido, bytes acumulados, TTL, cuerpo para caché).

- [ ] `request_filter` (`proxy_trait.rs:105`, `Result<bool>`): aquí se hace **todo** el control de acceso de la fase 1 — extraer y validar `crypt_id` (`^[A-Za-z0-9_-]{12}$`), parsear `url`/`mime`, cargar `ClientConfig` (`ConfigFetcher` + caché Moka), comprobar whitelist y aplicar el rate limit. Devolver `Ok(true)` solo después de haber escrito la respuesta (429 con `Retry-After`, 403, 400, 404) mediante `session.write_response_header(Box::new(header), true).await`; `Ok(false)` continúa hacia el upstream. La **única** ruta que no pasa por `crypt_id` es `GET /health` en el puerto del proxy: se reconoce aquí mismo y se responde `200` `text/plain` con cuerpo `OK` **antes** de cualquier validación, porque es el endpoint del `HEALTHCHECK` del `Dockerfile` (ver `api_contract.yaml`, `/health`). Cualquier otra ruta distinta de `/aq/{crypt_id}/` es `404` sin tocar el origen.
- [ ] `early_request_filter` (`proxy_trait.rs:121`, `Result<()>`): **se deja sin implementar**. Corre antes que cualquier módulo downstream —incluidos los de control de acceso y rate limit—, así que el doc de Pingora pide explícitamente que la lógica quede en `request_filter` siempre que pueda (`proxy_trait.rs:113-118`). La autenticación por `crypt_id` es control de acceso: ponerla aquí la colocaría fuera de la protección de los módulos.
- [x] Rate limit: Valkey (`INCR` + `PEXPIRE` **atómicos** vía `EVAL` con `RATE_LIMIT_SCRIPT` en `infrastructure/valkey_cache.rs`, y el contador real que devuelve el script se devuelve en `Retry-After` y en el log). Si Valkey no responde, decidir con el limiter local `infrastructure::local_rate_limiter::LocalRateLimiter` (ventana fija LRU sobre `crypt_id`, mismos umbrales, `Retry-After` nunca 0) y registrar `event: rate_limit_degraded` (WARN). **Nunca** se omiten ambos mecanismos: el límite jamás se salta por conveniencia. `pingora-limits` está declarado en el feature `proxy` pero **sin uso**: `pingora_limits::rate::Rate` estima eventos/segundo sobre un intervalo fijo y no puede producir el `Retry-After` ni el conteo por `window_seconds` que exige el contrato, así que `LocalRateLimiter` sigue siendo el limiter de la Fase 3 salvo decisión contraria.
- [ ] `upstream_peer`: pasos 3 y 4 del anti-SSRF. `SecureDnsResolver::resolve_and_validate(hostname)` -> `HttpPeer::new(SocketAddr::from((ip, puerto)), tls, sni = hostname)` con `options.verify_cert = true` y `options.verify_hostname = true`. El peer **nunca** se construye desde un hostname.
- [ ] `upstream_request_filter`: reescribir la request-line al path real del upstream (descartar `/aq/{crypt_id}/`), fijar `Host: {hostname}` y añadir `X-Forwarded-For`/`X-Forwarded-Proto`. **No** se borran cabeceras hop-by-hop a mano: `PeerOptions.http_upstream_request_policy` (`pingora-core/src/upstreams/peer.rs:545`, default `standard()` en `:462`) ya elimina `connection`, `keep-alive`, `upgrade`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding` y `HTTP2-Settings`, y quita las extensiones nominadas por `Connection`; si el cliente nombra `Host`, campos de origen o pseudo-headers, Pingora **rechaza** la request (`proxy_common.rs:178` y ss.). El servicio fija `deny_upgrades()` para no reenviar ningún handshake de upgrade. Lo que el filtro **sí** debe dejar intacto es `content-length`/`content-encoding`: son end-to-end y describen el cuerpo de la request; eliminarlos corromperia un body codificado.
- [ ] `upstream_response_filter`: validar `Location` en 3xx (paso 5 del anti-SSRF), evaluar `Content-Type` efectivo y decidir cacheabilidad. **Solo cabeceras**: la firma del rev pineado es `async fn upstream_response_filter(&self, &mut Session<DS>, &mut ResponseHeader, &mut CTX) -> Result<()>` (`pingora-proxy/src/proxy_trait.rs:381`), así que aquí no hay cuerpo que transformar; el scripting Lua vive en `upstream_response_body_filter` (`:461`), que en este rev **también es `async`** y por tanto puede awaiting (`spawn_blocking`, webhook) sin bloquear el worker.
- [ ] `response_filter`: headers de seguridad **después** de caché (`X-Content-Type-Options: nosniff`, `X-Proxy-By: tele.velone.ai`, `X-Cache`).
- [ ] `response_body_filter` / `upstream_response_body_filter`: **streaming por chunks** (`async fn(&self, &mut Session<DS>, &mut Option<Bytes>, end_of_stream: bool, &mut CTX) -> Result<Option<Duration>>`; `proxy_trait.rs:461` y `:494`, ambos con `where Self::CTX: Send + Sync`). Para el tope duro se usa el contador que ya trae Pingora — `Session::upstream_body_bytes_received()` (`pingora-proxy/src/lib.rs:921`) — complementado con el acumulado propio en `CTX` cuando hace falta el total por request; al superar `max_response_size_bytes` se aborta con `ProxyError::PayloadTooLarge` sin haber bufferizado el cuerpo completo.
- [ ] **Decisión de caché:** no se habilita el módulo `HttpCache` de Pingora. La persistencia es la capa propia del proyecto (`CacheStore` sobre Valkey, claves `px:{internal_id}:{ver}:{sha256(url)}` en postcard, TTL por MIME y copia `:stale`), que expresa cosas que `HttpCache` no expresa —fallback global por MIME, 3 niveles, `X-Cache: FALLBACK`—. Consecuencia obligada: mientras nadie llame a `HttpCache::enable(..)` (`pingora-cache/src/lib.rs:571`) la fase del session es `Disabled(NoCacheReason::NeverEnabled)` (`:382`), y por tanto **todo** `session.cache.set_max_file_size_bytes(..)` (`:776`), `track_body_bytes_for_max_file_size(..)` (`:795`) y `exceeded_max_file_size()` (`:814`) hace `panic!`/`assert!` — el setter con `panic!("wrong phase")` explícito en `:778`. El tope de 5 MB cacheable se sigue midiendo con el contador del `CTX`; las respuestas mayores se reenvían en streaming y se marcan `X-Cache: BYPASS`. Adoptar `HttpCache` exigiría escribir un adaptador `pingora_cache::storage::Storage` sobre Valkey con vida `'static` (`enable` pide `&'static (dyn Storage + Sync)`) y renunciar al esquema actual; queda como alternativa registrada, no como tarea de estas fases.
- [ ] `fail_to_proxy` / `error_while_proxy`: cadena de fallback de 3 niveles (ver Fase 5) y emisión del `ProxyError` como JSON.
- [ ] `logging`: punto único de log de acceso/seguridad (solo errores en fase 1). Pingora ya emite un log de error interno cuando `e: Option<&Error>` es `Some`; para no duplicarlo se sobrescribe `fn suppress_error_log(&self, &Session<DS>, &Self::CTX, &Error) -> bool` (`proxy_trait.rs:571`, default `false`) devolviendo `true` para los `BError` que el servicio construyó desde un `ProxyError` — así el registro sale una sola vez con el `error_code` del diccionario.
- [ ] Arranque (en `main.rs`): `Server::new_with_opt_and_conf(opt, conf)` -> `bootstrap()` -> `pingora_proxy::http_proxy_service(&server.configuration, ProxyService::new(…))` -> `svc.add_tcp(&format!("{HTTP_HOST}:{HTTP_PROXY_PORT}"))` (no existe ninguna variable `HTTP_PROXY_ADDR`: `ENVIRONMENT.md` define host y puerto por separado y `Service::add_tcp` recibe un `&str` ya compuesto) -> `server.add_service(svc)` -> `run_forever()`. Verificado en el rev pineado de `pingora-core`: `Server::new(opt: impl Into<Option<Opt>>) -> Result<Server>` (hay que propagar con `?`), `Server::new_with_opt_and_conf(opt, conf) -> Server` (`server/mod.rs:455`, **infalible**: es el constructor a usar porque `threads` viene de `ServerConf`, no de un `.yaml`; `Server::new` ignoraría los threads propios), `configuration: Arc<ServerConf>` (por eso se pasa `&server.configuration`), `bootstrap(&mut self)` (`:604`), `add_service(&mut self, impl ServiceWithDependents + 'static) -> ServiceHandle` (`:546`) y `run_forever(self) -> !` (`:626`) — consume el `Server`, por lo que es la **última** línea del `main`.

### Fase 4: Scripting Lua Sandboxed

- [x] Configurar `mlua` deshabilitando `os`, `io`, `package`, `debug`, `dofile`, `loadfile`, `load` (`application/lua_engine.rs`, `INIT_SCRIPT`; los tests `test_sandbox_blocks_io` / `test_sandbox_blocks_os` lo sostienen).
- [x] Inyectar objeto global `proxy` con funciones seguras: `log`, `regex_replace` (con límite de tiempo anti-ReDoS de `REDOS_TIMEOUT_MS = 100 ms`) y `http_request` (webhook con timeout).
- [x] `proxy.http_request` **debe** enrutar por `application/webhook_service.rs` -> `infrastructure/http_client.rs`, que aplica whitelist + `resolve_and_validate` + pinning + `redirect::Policy::none()`. Prohibido crear un `reqwest::Client` dentro del sandbox. Implementado en `ValidatingWebhookFetcher::fetch`; el `reqwest::Client` solo se construye en `http_client::pinned_client`. La función expuesta a Lua es **síncrona** (una `create_async_function` de mlua no puede invocarse desde la VM síncrona: `attempt to yield from outside a coroutine`) y hace de puente con `tokio::runtime::Handle::try_current()` + `spawn` + `recv_timeout(timeout + 250 ms)`, de modo que el socket nunca se abre en el hilo de la VM. `SsrfBlocked` y `DomainNotWhitelisted` son **sticky**: abortan la petición aunque el script envuelva la llamada en `pcall`.
- [x] El timeout de Lua debe **interrumpir** la ejecución (deadline real dentro del sandbox: contador de instrucciones de Lua o línea muerta verificable), no comprobar `elapsed` después de que la llamada haya vuelto. Con `mlua` 0.12 y el feature `lua54` el mecanismo es `Lua::set_hook(HookTriggers::new().every_nth_instruction(N), callback)` (`mlua-0.12.1/src/debug.rs:343`, `src/state.rs:756`; el callback es `Fn(&Lua, &Debug) -> Result<VmState>`); el closure devuelve `Err(mlua::Error::RuntimeError(..))` para abortar, porque `VmState` solo ofrece `Continue | Yield`. **`set_interrupt` no sirve**: está tras `#[cfg(feature = "luau")]`. Un `tokio::time::timeout` alrededor de `spawn_blocking` no cancela el task: hay que cancelar por dentro del hook. Implementado con `set_global_hook` (los hilos nuevos de la VM heredan el hook) sobre `every_nth_instruction(2000)` comparando contra un `deadline: Instant`; el `tokio::time::timeout` de `timeout_ms + 100 ms` alrededor del `JoinHandle` queda como red para el caso de que el hook no dispare, y `test_bucle_infinito_se_interrumpe_por_deadline` prueba que `while true do end` devuelve `script_timeout` en menos de un segundo.
- [x] Verificación obligatoria de `scripting.code_hash` (`sha256:`) antes de ejecutar; si no coincide, `ProxyError::IntegrityCheckFailed` y se sirve el cuerpo sin transformar (`verify_script_integrity` en `lua_engine.rs`; `interfaces/proxy_handler.rs::apply_lua_scripting` degrada al cuerpo original y deja el ERROR en el log). Un `code_hash` vacío se acepta: son configs anteriores al contrato, deuda registrada en `DIAGRAMS.md`.
- [x] Implementar lógica de bypass de Lua si el cuerpo supera `max_scripting_body_bytes` (`execute()` devuelve el cuerpo sin transformar antes de levantar la VM).
- [x] Ejecutar Lua en `tokio::task::spawn_blocking` para no bloquear el runtime de Pingora: sus worker threads son los de `ServerConf.threads` (**1** si no se fija) y un bloqueo ahí para **todo** el proxy, no solo la request culpable.

### Fase 5: Manejo de Errores, Fallbacks y API de Control

- [ ] Cadena de 3 niveles: **1)** `px:{internal_id}:{ver}:{sha256}:stale` en Valkey -> **2)** `px:defaults:{mime}` global -> **3)** recurso embebido (`include_bytes!`) según MIME. Configurar además `error_handling.fallback_urls` (fetch validado por el mismo pipeline anti-SSRF) y `scripting.code_hash`, que hoy están declarados pero sin uso.
- [ ] API de control `axum` corriendo como `BackgroundService` de Pingora (`pingora_core::services::background::background_service("control-api", …)`), en el puerto `HTTP_CONTROL_PORT`, apagándose con el `ShutdownWatch`.
- [ ] Middleware de autenticación Bearer (hash SHA-256 del token contra CouchDB) para `/api/v1/`.
- [ ] Endpoints: `GET /api/v1/clients/config`, `PUT /api/v1/clients/config`, `POST /api/v1/clients/rotate-id`, `GET /health`. **No existe** `POST /api/v1/clients` (los clientes se provisionan fuera de este servicio); la documentación de despliegue no debe prometerlo.
- [x] `PUT /config` valida todos los límites del contrato (`max_requests 1..=10000`, `window_seconds 1..=3600`, `max_scripting_body_bytes 1048576..=52428800`, `mode in {transparent, wrapped}`, `code_hash`) **antes** de escribir en CouchDB y devuelve **400**: `invalid_config` para cotas, entradas de `whitelist` que no son host desnudo o son IP privadas y hashes fuera de formato; `invalid_url_format` para la URL de un fallback. `mode` lo restringe serde al deserializar, y un cuerpo que no deserializa se normaliza a 400 `invalid_config` en lugar del 422 en texto plano de Axum. Implementado en `domain::validators::validate_config_update`, llamado desde `interfaces/control_api.rs`.
- [ ] Escucha del `_changes` feed de CouchDB (polling, sin `SCAN/DEL` masivos) para invalidar config y `config_version`.

### Estado de la migración Axum/reqwest -> Pingora

El repositorio todavía sirve `/aq/{crypt_id}/` con Axum + `reqwest` (handler `src/interfaces/proxy_handler.rs`). La tabla siguiente es el inventario honesto del trabajo:

| Componente | Hoy | Objetivo Pingora | Acción |
|------------|-----|------------------|--------|
| Ciclo de vida del proxy | handler Axum `proxy_handler` | `impl ProxyHttp` | Portar (Fase 3) |
| Body del upstream | `http_client::read_body_capped`: acumula chunks y aborta con `payload_too_large` al superar el tope, pero termina el cuerpo entero en RAM antes de responder | `response_body_filter` por chunks, reenviando sin acumular | Portar; el tope ya se aplica durante el stream (no sobre un `Content-Length` anunciado), lo que falta es el reenvío chunk a chunk del escenario BDD de 15 MB |
| Rate limit | `EVAL` atómico (`INCR`+`PEXPIRE`) con fallback `LocalRateLimiter` y `Retry-After` real | igual, dentro de `request_filter` | Corregido; queda portar el call site (Fase 3) |
| Conexión upstream | `http_client::pinned_client(hostname, ip, port, timeout)`: `.resolve()` con el nombre original (SNI intacto) y `redirect::Policy::none()` | `HttpPeer` con `SocketAddr` pinneado + keepalive pool | Helper único ya en uso; portar el call site |
| Caché de respuesta | `postcard` + `X-Cache` HIT/MISS/FALLBACK | igual, más `BYPASS` en no-cacheables | Alinear contrato |
| API de control | 2 listeners `tokio::spawn` | `BackgroundService` | Reorganizar arranque |
| Webhooks Lua | `application/webhook_service.rs` + `http_client::pinned_client`, con la llamada síncrona de la VM puenteada a `Handle::spawn` | lo mismo, desde `upstream_response_body_filter` | **Corregido (P1)**; queda el call site de Fase 3 |

Mientras la portación no esté terminada, el feature `proxy` queda **opcional** (`default = []`) para que el desarrollo en Windows siga siendo posible; esto es deuda de migración, no una decisión arquitectónica.

#### Rev de Pingora contra el que está escrita esta documentación

`Cargo.toml` declara el rev explícito — `git+https://github.com/cloudflare/pingora?rev=4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19` — y `Cargo.lock` lo repite en el `source` de cada crate; esos crates declaran versión `0.9.0`. **No confundir con `pingora-proxy` 0.9.0 publicado en crates.io**: entre el tag 0.9.0 (2026-09-04) y ese commit de `main` la API cambió, y todos los hechos de esta documentación —firmas, números de línea, `Session<DS>`— se verificaron contra el checkout del rev pineado (`target/cargo-home/git/checkouts/pingora-*`), no contra crates.io. Diferencias que rompen compilación si se mezcla:

| Punto | crates.io 0.9.0 | rev pineado (`main` @ 4487f7b) |
|-------|-----------------|-------------------------------|
| `Session` | `pub struct Session` | `pub struct Session<DS = ()>` (genérico de downstream) |
| `upstream_response_body_filter` / `response_body_filter` | `fn` síncronos | `async fn` (`proxy_trait.rs:461`, `:494`) |
| `ProxyHttp` | sin `early_request_filter`, sin subrequests | añade `early_request_filter`, `adjust_upstream_modules`, `persist_connection_context`, `on_connection_reuse`, `suppress_proxy_warn_log`, `should_serve_stale`, familia `purge_*`, `allow_spawning_subrequest` |
| Framing del upstream | el filtro manual era obligatorio | `PeerOptions::http_upstream_request_policy` sanea hop-by-hop por defecto |

Consecuencia operativa: la reproducibilidad la da el triplete `rev` en `Cargo.toml` + `Cargo.lock` +
`cargo build --locked`. El `rev` es lo que impide que regenerar el lock (por ejemplo al añadir
`pingora-limits`) desplace el commit a un `main` más nuevo y deje obsoletas las líneas citadas aquí.
Cambiar a crates.io, en cambio, exije re-verificar cada firma y reescribir los bloques de
`RUST_STYLE_GUIDE.md`/`ARCHITECTURE.md` que asumen filtros asíncronos.

> **Resuelto (2026-10-02)**: el `Cargo.lock` ya está regenerado con el `rev` explícito — las 12
> entradas `pingora-*` llevan `source = "git+…?rev=4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19#4487f7b2…"`,
> incluido `pingora-limits` (ahora declarado) y `pingora-rustls` (lo trae `features = ["rustls"]`).
> `grep 'pingora?branch=' Cargo.lock` da vacío, así que `cargo build --locked` y `docker build` ya no
> abortan por lock desalineado. La regeneración se hizo online (con `--offline` falla: `no matching
> package named `ouroboros``) y unificó `zstd` en 0.13.3; el detalle y el recipe están en
> `DEPLOYMENT.md`, "Subir el Rev de Pingora". Lo que sigue vigente como verificación es compilar el
> feature `proxy` **en Linux**, que es donde pingora existe.

---

## 🐳 5. Configuración de Despliegue (Dokploy Ready)

El `docker-compose.yml` debe estar listo para ser levantado por Dokploy, con healthchecks y red aislada. La versión `3.8` ya no se declara (Compose v2 la ignora con advertencia) y el build debe fijar dependencias y features:

```yaml
services:
  tele-proxy:
    build:
      context: .
    ports:
      - "${HTTP_PROXY_PORT:-8080}:8080" # Proxy público (Pingora: add_tcp)
      - "127.0.0.1:${HTTP_CONTROL_PORT:-8081}:8081" # API de control (axum): solo loopback
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
    depends_on:
      couchdb:
        condition: service_healthy
      valkey:
        condition: service_healthy
    restart: unless-stopped
    stop_grace_period: 15s # > PROXY_GRACE_PERIOD_SECONDS; si no, SIGKILL antes de drenar
    networks:
      - tele-net

  couchdb:
    image: couchdb:3.3
    # Sin `ports:`: 5984 no se publica nunca; solo se alcanza por tele-net.
    environment:
      - COUCHDB_USER=${COUCHDB_USER}
      - COUCHDB_PASSWORD=${COUCHDB_PASSWORD}
    volumes:
      - couchdb_data:/opt/couchdb/data
    healthcheck:
      test: ["CMD-SHELL", "curl -fsS http://localhost:5984/_up || exit 1"]
      interval: 10s
      timeout: 5s
      retries: 5
    networks:
      - tele-net

  valkey:
    image: valkey/valkey:7.2
    # La contraseña por entorno del contenedor, no por argv: `--requirepass` en el command
    # quedaría visible en `docker inspect` y en `ps`.
    environment:
      - VALKEY_PASSWORD=${VALKEY_PASSWORD}
    command:
      - sh
      - -c
      - exec valkey-server --requirepass "$$VALKEY_PASSWORD" --maxmemory 256mb
        --maxmemory-policy allkeys-lru
    volumes:
      - valkey_data:/data
    healthcheck:
      test: ["CMD-SHELL", 'valkey-cli -a "$$VALKEY_PASSWORD" --no-auth-warning ping | grep -q PONG']
      interval: 10s
      timeout: 5s
      retries: 5
    networks:
      - tele-net

volumes:
  couchdb_data:
  valkey_data:

networks:
  tele-net:
    driver: bridge
```

Tres cosas de este bloque son requisitos de seguridad, no de estilo: el `127.0.0.1:` delante del
puerto de control (la API `/api/v1/` escribe configs y rota `crypt_id`; expuesta al público es un
endpoint de administración sin otra protección que el Bearer), la ausencia de `ports:` en CouchDB, y
la contraseña de Valkey referenciada con `$$VALKEY_PASSWORD` dentro del `command` — `$$` es el
escape de Compose para que la interpolación la haga el shell del contenedor y no el intérprete de
Compose, que es lo que evitaría que el secreto aparezca en el `argv` inspeccionable.

**Requisitos del despliegue**:

- **Linux**: la imagen se construye y ejecuta en Linux; Pingora no funciona en Windows. El `HEALTHCHECK` del `Dockerfile` usa `curl -fsS http://localhost:8080/health`, así que `/health` **debe** responder dentro de `request_filter` (ver Fase 3): el listener 8080 es el de Pingora, no un router de Axum, y si la ruta se tratara como petición de proxy el healthcheck siempre fallaría.
- **El binario se construye con el feature `proxy`** (`cargo build --release --locked --features proxy`). Sin él, `pingora-core`/`pingora-proxy` son dependencias `optional` que no se compilan y el binario no trae motor de proxy: `docker build` que no lo pase produce una imagen que arranca pero no escucha `/aq/`.
- **TLS del upstream por rustls** (default): `pingora-core` y `pingora-proxy` declaran `default = []` en sus manifiestos, de modo que **ningún** backend TLS viene activado por defecto. Hay que pedir explícitamente `features = ["rustls"]` (`pingora-proxy/Cargo.toml` del rev: `rustls = ["pingora-core/rustls", "pingora-cache/rustls", "any_tls"]`). Elegir `rustls` en lugar de `openssl` mantiene el runtime `debian:bookworm-slim` sin `libssl` y coincide con el `ring` que ya trae `hickory-resolver` (`tls-ring`). Existe una variante hermana excluyente **`proxy-openssl`** (reqwest sobre OpenSSL vendored) para orígenes que fingerprintan el JA3 de rustls — operativa documentada en `docs/DEPLOYMENT.md`; la decisión de default no cambia.
- **La base de datos `tele_proxy_configs` se crea a mano** tras el primer arranque (el servicio no la autogenera): `docker exec <couchdb> curl -s -X PUT http://localhost:5984/tele_proxy_configs -u "$COUCHDB_USER:$COUCHDB_PASSWORD"`.
- La red por defecto de Docker Desktop **no tiene ruta IPv6**: los resolvers DoT devuelven AAAA para sitios tras CDN. El resolver debe preferir IPv4 y el `HttpPeer` debe construirse con `SocketAddr` IPv4 salvo que el host tenga salida v6.
- `docker build`/`cargo build --release` deben usar `--locked` con `Cargo.lock` commiteado.

---

## 🤖 6. Instrucciones Estrictas para el Agente de Código

1. **Repositorio**: `stringnetlab/tele-proxy`. Cada paso debe ser un commit atómico.
2. **Formato de Commit**: `feat(phase-X): descripción corta`, `fix(security): …` o `test(phase-X): añadir pruebas para Y`.
3. **TDD/BDD**: Antes de escribir la lógica de negocio de una fase, escribe las pruebas (`#[cfg(test)]`) que cubran los escenarios Gherkin de `specs/*.feature` (generado desde `docs/BDD.md`, un archivo por Feature).
4. **Manejo de Errores**: `thiserror` para errores del dominio. Nunca `.unwrap()` en código de producción (se permite en tests y en el arranque de `main`, y aun ahí se prefiere `?` + salida limpia); usa `?` o manejo explícito. Los callbacks de Pingora devuelven `pingora_core::Result<T>` (= `Result<T, Box<pingora_core::Error>>`), así que el mapeo `ProxyError -> BError` debe ser explícito (`impl From<ProxyError> for BError` en `application/proxy_service.rs`, con `ErrorType::new_code`) y conservando el `error_code`. `domain/` no importa `pingora`.
5. **Verificación**: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings` y `cargo test` en **Linux** (Docker `rust:bookworm`) cuando el feature `proxy` esté activo. Un `exit 0` del envoltorio no es prueba: hay que leer el log real del comando.
6. **Seguridad Primero**: si la Fase 3 necesita datos de la Fase 2, asume que existe y mapea sus traits. **No comprometas la validación de IPs privadas por conveniencia**: ninguna ruta de salida (proxy, webhook Lua, fallback remoto) puede resolver por fuera de `SecureDnsResolver`.
7. **Dokploy / Imagen**: `Dockerfile` multi-etapa (`rust:bookworm` -> `debian:bookworm-slim`), binario despojado (`strip = true`, `lto = "fat"`). Con Pingora el objetivo realista de tamaño es **< 80 MB**; el límite histórico de 50 MB era de la etapa Axum+reqwest y queda sustituido por esta cifra hasta que se mida la imagen final.

---

### 🟢 Instrucción de Inicio para el Agente

> "Agente, las Fases 1 y 2 están completas y el proxy aún corre sobre Axum+`reqwest`. Ejecuta la **Fase 3**: sustituye el handler por `impl ProxyHttp for ProxyService` en `src/application/proxy_service.rs` (incluyendo el arranque con `Server` + `http_proxy_service` y la API de control como `BackgroundService`), verifica en Linux con `--features proxy`, y deja las pruebas de los escenarios Gherkin correspondientes en verde antes de commitear."
