# Arquitectura del Sistema

**Motor de proxy**: Pingora (Cloudflare) v0.9, `rustls` para TLS de upstream.
**API de control**: Axum 0.8, en el mismo binario como `BackgroundService` de Pingora.

> **Estado de la migración**: las Fases 1-2 (dominio, DNS anti-SSRF) están implementadas. El ciclo de vida del proxy todavía corre sobre Axum + `reqwest` (`src/interfaces/proxy_handler.rs`). Este documento describe el **objetivo Pingora** y marca con ⚠️ lo que aún no está portado. El detalle fiel al código de hoy está en `docs/DIAGRAMS.md`, sección "Estado actual vs objetivo".

---

## Diagrama de Alto Nivel

```
┌─────────────────────────────────────────────────────────────┐
│                        CLIENTES                              │
│  (Apps, Websites, Emails con URLs /aq/{crypt_id}/?url=...)  │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│                     Cloudflare (CDN/WAF)                     │
│              - TLS Termination (edge)                        │
│              - DDoS Protection                               │
│              - Rate Limiting (capa adicional, no sustitutiva)│
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│                      Dokploy (Orquestador)                   │
│              - Docker Compose Management                     │
│              - Auto-restart on failure (Linux-only)          │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────┐
│            tele-proxy (binario único, Rust)                  │
│                                                              │
│  pingora_core::Server (run_forever, threads = ServerConf)     │
│  ├── http_proxy_service  → add_tcp(0.0.0.0:8080)  ProxyHttp  │
│  ├── background_service("control-api") → axum :8081          │
│  └── background_service("changes-feed") → polling CouchDB    │
│                                                              │
│  Estado en proceso: caché Moka de configs, LRU de DNS        │
└──────┬────────────────────────┬──────────────────┬───────────┘
       │                        │                  │
       ▼                        ▼                  ▼
┌─────────────┐        ┌──────────────┐    ┌──────────────────┐
│  CouchDB    │        │    Valkey    │    │   DNS over TLS   │
│  configs de │        │  respuestas  │    │  Quad9/Cloudflare│
│  cliente +  │        │  (postcard), │    │  /AdGuard/Clean  │
│  _changes   │        │  rate limit, │    │  Browsing/Google │
│  feed       │        │  fallbacks   │    │  :853 + DNSSEC   │
└─────────────┘        └──────────────┘    └──────────────────┘
                               │
                               ▼
                        ┌──────────────┐
                        │   UPSTREAMS  │
                        │  (whitelist) │
                        │  conexión fijada a la IP validada
                        └──────────────┘
```

Un solo proceso sirve ambos puertos. Pingora supervisa los servicios, maneja señales (`SIGTERM`/`SIGHUP`) y reparte el tráfico proxy sobre el runtime del servicio (`ServerConf.threads` hilos, con *work stealing*); el listener Axum y el polling de `_changes` viven como *background services* que reciben el `ShutdownWatch`.

## Flujo de Datos: Solicitud Pública (ciclo `ProxyHttp`)

```
1. Cliente → GET /aq/V1StGXR8_Z5j/?url=https://shutterstock.com/img.jpg

2. new_ctx()
   └─ Se crea el RequestContext de la solicitud

3. request_filter() — parte A: identidad   (los pasos 3 y 4 son EL MISMO callback;
   ├─ Extrae crypt_id y url de la ruta/query          early_request_filter no se
   │                                                    implementa, ver spec.md Fase 3)
   ├─ Valida formato: /^[A-Za-z0-9_-]{12}$/  (8..=24 configurable)
   └─ error 400 invalid_crypt_id  (WARN, event=invalid_crypt_id)

4. request_filter() — parte B: config y control
   ├─ ConfigFetcher: caché Moka → MISS → CouchDB view by_crypt_id
   ├─ Valkey: INCR + PEXPIRE atómicos sobre rl:{crypt_id}
   │    ├─ count > max_requests → 429 con Retry-After, X-Cache: BYPASS
   │    └─ Valkey caído → limiter local LocalRateLimiter (WARN rate_limit_degraded)
   ├─ validate_url_strict: scheme http/https, sin credenciales, sin fragmento
   ├─ Whitelist: sufijo de dominio contra client.whitelist
   └─ Consulta px:{internal_id}:{config_version}:{sha256(url)}
        ├─ HIT  → servir de caché, response_filter añade X-Cache: HIT, Ok(true)
        └─ MISS → continuar a upstream

5. upstream_peer()
   ├─ SecureDnsResolver::resolve_and_validate(hostname)
   │    ├─ LRU in-process → HIT directo
   │    └─ MISS → DoT :853 + DNSSEC, cadena Quad9 → Cloudflare → AdGuard
   │              → CleanBrowsing → Google; descarta IPs privadas
   ├─ HttpPeer::new(SocketAddr::from((ip_validada, 443)), tls=true, sni=hostname)
   └─ peer.options.verify_cert = verify_hostname = true
      (el nombre NUNCA se resuelve por Pingora: eso es el anti DNS-rebinding)

6. upstream_request_filter()
   ├─ Reescribe la request-line al path real (/img.jpg)
   ├─ insert_header("Host", "shutterstock.com")
   ├─ X-Forwarded-For / X-Forwarded-Proto
   └─ NO borra framing/hop-by-hop a mano: PeerOptions.http_upstream_request_policy
      (default standard(), pingora-core/src/upstreams/peer.rs:462/:545) ya quita
      connection, keep-alive, upgrade, proxy-authenticate, proxy-authorization, te,
      trailer, transfer-encoding y HTTP2-Settings; el servicio fija deny_upgrades().
      content-length/content-encoding son end-to-end y se conservan.

7. upstream_response_filter()  (SOLO cabeceras del origen, antes de caché; async)
   ├─ Si 3xx: valida Location con los mismos pasos 4-5; si apunta a IP
   │  privada o dominio fuera de whitelist → 403 ssrf_blocked
   │  reason=redirect_to_private_ip y Location se elimina
   ├─ Content-Type efectivo: 1)?mime 2)extensión 3)content-type del origen 4)octet-stream
   └─ Declara la cacheabilidad de cabeceras (status 200/…, sin Set-Cookie);
      el tope de tamaño se decide en el filtro de cuerpo, no aquí

8. upstream_response_body_filter()  (streaming por chunks; async, puede .await)
   ├─ Acumula contador de bytes en el CTX (el tope duro lo mide
   │  Session::upstream_body_bytes_received(), pingora-proxy/src/lib.rs:921)
   ├─ > max_response_size_bytes → PayloadTooLarge (aborte, sin buffer total)
   ├─ Scripting Lua si enabled y body <= max_scripting_body_bytes
   │  (spawn_blocking + deadline real dentro del sandbox, ver RUST_STYLE_GUIDE §3)
   └─ Solo retiene copia para caché si el total <= CACHEABLE_SIZE_LIMIT (5 MB);
      si lo supera, X-Cache: BYPASS y el resto del cuerpo sigue de largo

9. response_filter()  (después de caché, para todas las respuestas)
   ├─ X-Content-Type-Options: nosniff
   ├─ X-Proxy-By: tele.velone.ai
   └─ X-Cache: MISS | HIT | FALLBACK | BYPASS

10. Persistencia
    ├─ px:{internal_id}:{ver}:{sha256(url)}   ← postcard, TTL por MIME
    ├─ px:…{sha256(url)}:stale               ← copia para Nivel 1 de fallback
    └─ px:defaults:{mime}                    ← fallback global (solo al bootstrap)

11. fail_to_proxy() / error_while_proxy()  → cadena de 3 niveles
    ├─ 1) px:…:stale (Valkey)          → X-Cache: FALLBACK
    ├─ 2) px:defaults:{mime} (Valkey)  → X-Cache: FALLBACK
    └─ 3) recurso embebido include_bytes! según MIME

12. logging()
    └─ Solo errores (WARN/ERROR) en fase 1, JSON con event= y campos
       del diccionario de errores
```

## Flujo de Datos: API de Control

```
1. Cliente → GET /api/v1/clients/config
   Headers: Authorization: Bearer sk_live_abc123

2. Middleware axum: hash_sha256(token) → CouchDB view by_token_hash
   ├─ inválido → 401 unauthorized
   └─ válido → inyecta internal_id del cliente (aislamiento)

3. Handler
   ├─ Lee ClientConfig
   ├─ Excluye campos internos (_id, internal_id, bearer_token_hash)
   └─ 200 JSON con crypt_id vigente

4. PUT /api/v1/clients/config
   ├─ Valida bounds del contrato (max_requests 1..=10000, window 1..=3600,
   │  max_scripting_body_bytes 1 MiB..=50 MiB, mode ∈ {transparent,wrapped})
   ├─ Valida scripting.code_hash si scripting.enabled
   ├─ increments config_version → invalida claves px: automáticamente
   └─ 400 invalid_url_format | internal_error en caso de rechazo

5. POST /api/v1/clients/rotate-id
   ├─ nanoid!(12) nuevo, escribe CouchDB, invalida Moka
   └─ INFO event=crypt_id_rotated {internal_id, old_crypt_id, new_crypt_id}
```

## Componentes Internos

### 1. Proxy Handler (Pingora)

```rust
// src/application/proxy_service.rs
pub struct ProxyService { /* Arc<dyn ConfigFetcher>, CacheStore, DnsResolver, LuaExecutor */ }

#[async_trait]
// El trait es `ProxyHttp<DS = ()>` (proxy_trait.rs:48). Con DS = () —el caso de este proyecto—
// `Session<DS>` se resuelve a la tipo `Session` concreta, así que el impl escribe `&mut Session`.
impl ProxyHttp for ProxyService {
    type CTX = RequestContext;
    fn new_ctx(&self) -> Self::CTX;                          // :56, no tiene default
    // early_request_filter EXISTE (:121) pero no se implementa: corre antes de los módulos
    // internos y toda la decisión de acceso ya vive en request_filter. Ver spec.md Fase 3.
    async fn request_filter(&self, s: &mut Session, ctx: &mut Self::CTX) -> Result<bool>;   // :105
    async fn upstream_peer(&self, s: &mut Session, ctx: &mut Self::CTX) -> Result<Box<HttpPeer>>;  // :62
    async fn upstream_request_filter(&self, s: &mut Session, req: &mut RequestHeader,
                                     ctx: &mut Self::CTX) -> Result<()>;
    async fn upstream_response_filter(&self, s: &mut Session, resp: &mut ResponseHeader,
                                      ctx: &mut Self::CTX) -> Result<()>;
    // Filtros de cuerpo: en este rev son `async fn` y devuelven el delay de reenvío.
    async fn upstream_response_body_filter(&self, s: &mut Session, body: &mut Option<Bytes>,
                                           eos: bool, ctx: &mut Self::CTX)
        -> Result<Option<Duration>>;                          // :461
    async fn response_filter(&self, s: &mut Session, resp: &mut ResponseHeader,
                             ctx: &mut Self::CTX) -> Result<()>;
    async fn response_body_filter(&self, s: &mut Session, body: &mut Option<Bytes>,
                                  eos: bool, ctx: &mut Self::CTX) -> Result<Option<Duration>>;  // :494
    async fn fail_to_proxy(&self, s: &mut Session, e: &Error, ctx: &mut Self::CTX)
        -> FailToProxy;                                       // :653 -> { error_code, can_reuse_downstream } (:775)
    fn error_while_proxy(&self, peer: &HttpPeer, s: &mut Session, e: Box<Error>,
                         ctx: &mut Self::CTX, reused: bool) -> Box<Error>;   // :609, es `fn` síncrono
    fn suppress_error_log(&self, s: &mut Session, ctx: &Self::CTX, e: &Error) -> bool;  // :571
    async fn logging(&self, s: &mut Session, e: Option<&Error>, ctx: &mut Self::CTX);   // :529
}
```

Todos los hooks anteriores van con `where Self::CTX: Send + Sync`, igual que el trait; el `impl` debe repetirlo.

### 2. Arranque y API de Control (Axum sobre Pingora)

```rust
// src/main.rs
// El conf base no viene de un .yaml: se construye en código porque `threads`
// sale de ENVIRONMENT.md. ServerConf::default() deja threads = 1 (:236).
let opt = Opt::default();                                    // configuration/mod.rs:270
let mut conf = ServerConf::new_with_opt_override(&opt).expect("ServerConf base");  // :339
conf.threads = std::env::var("PROXY_WORKER_THREADS")            // :74 — trampa en ENVIRONMENT.md:38
    .ok().and_then(|v| v.parse().ok())
    .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
conf.work_stealing = true;                                   // :78

// new_with_opt_and_conf es INFALIBLE (devuelve Server, sin Result) y es el único
// constructor que respeta el `conf` propio: Server::new (:492) lee `-c` o genera
// un conf por defecto y perdería los threads de arriba.
let mut server = Server::new_with_opt_and_conf(opt, conf);   // server/mod.rs:455
server.bootstrap();                                          // :604

let mut proxy = pingora_proxy::http_proxy_service(&server.configuration, ProxyService::new(deps));
// `configuration` es Arc<ServerConf> (server/mod.rs:224) y add_tcp recibe &str (listeners/mod.rs:383):
// no existe ninguna variable HTTP_PROXY_ADDR, host y puerto son variables separadas.
proxy.add_tcp(&format!("{}:{}", http_host, http_proxy_port));   // p.ej. 0.0.0.0:8080
server.add_service(proxy);                                       // :546 -> ServiceHandle

server.add_service(background_service("control-api", ControlApi::new(app)));      // background.rs:118
server.add_service(background_service("changes-feed", ChangesFeedListener::new(repo)));

server.run_forever();   // :626 — consume el Server, nunca devuelve; última línea del main
```

`ControlApi` implementa `BackgroundService::start(&self, mut shutdown: ShutdownWatch)`: hace `tokio::join!` del `axum::serve` y de `shutdown.changed()` para cerrar ordenadamente.

### 3. DNS Resolver (`src/infrastructure/dns_resolver.rs`)

```rust
pub async fn resolve_and_validate(&self, hostname: &str) -> Result<IpAddr> {
    // 1. LRU in-process (capacidad cache_size, TTL ip_cache_ttl_seconds)
    // 2. DoT :853 con DNSSEC (opts.validate) en cadena de prioridad
    // 3. is_private_ip() sobre cada dirección; preferir IPv4 (sin ruta v6 en Docker)
    // 4. SNI del resolver = server_name del proveedor
    // 5. Inserta en LRU y devuelve la IP ya validada
}
```

No usa Valkey: ver la decisión en "¿Por qué el caché DNS es in-process?".

### 4. HTTP Client validado (`src/infrastructure/http_client.rs`) ⚠️ pendiente

Única puerta de salida que no es el proxy (webhooks Lua, `fallback_urls`). Recibe URL + whitelist + resolver, aplica el ciclo anti-SSRF completo y devuelve la respuesta sin seguir redirects. Ningún otro módulo construye un `reqwest::Client`.

### 5. Lua Engine (`src/application/lua_engine.rs`)

```rust
pub async fn execute_script(&self, ctx: &mut RequestContext) -> Result<Body> {
    // 1. Estado sandboxed (os/io/package/debug/dofile/loadfile/load = nil)
    // 2. Objeto global `proxy`: log, regex_replace (timeout anti-ReDoS), http_request
    //    -> http_request delega en webhook_service (validación anti-SSRF obligatoria)
    //       y es async: un webhook no puede secuestrar el hilo donde corre la VM
    // 3. spawn_blocking: los threads de trabajo del proxy salen de ServerConf.threads
    //    (default 1 — trampa en ENVIRONMENT.md:38), así que bloquear uno de ellos
    //    con una VM de Lua deja al proxy sin capacidad de atender peticiones
    // 4. Deadline REAL dentro de la VM, no comprobado después: con mlua 0.12 + lua54
    //    el mecanismo es Lua::set_hook(HookTriggers::new().every_nth_instruction(N), cb)
    //    y el cb devuelve Err(mlua::Error::RuntimeError(..)) al vencer. set_interrupt
    //    está tras el feature luau, y un tokio::time::timeout alrededor de
    //    spawn_blocking cancela la espera pero NO cancela el task bloqueado.
    //    Ver spec.md Fase 4 y RUST_STYLE_GUIDE §3.
    // 5. Límite de memoria reportado con el consumo medido, no con el límite
}
```

## Decisiones Arquitectónicas

### ¿Por qué Pingora en lugar de Hyper puro?

- **Ventaja**: Pingora ya implementa el ciclo de vida de proxy (`upstream_peer`, filtros de request/response/body, reintentos) en lugar de reconstruirlo a mano sobre `reqwest`.
- **Ventaja**: Streaming nativo por chunks, imprescindible para el escenario BDD de 15 MB sin cargar el cuerpo en RAM.
- **Ventaja**: Pool de conexiones reutilizadas por peer; hoy se construye un `reqwest::Client` por solicitud.
- **Ventaja**: Runtime Tokio multi-thread configurable en `ServerConf.threads`, `daemon()`, recarga suave y métricas internas ya probadas en producción por Cloudflare.
- **Ventaja**: El pinning anti-rebinding se expresa de forma natural: `HttpPeer` se construye con el `SocketAddr` ya validado y el hostname solo viaja en SNI/`Host`.
- **Coste**: solo Linux/Unix, dependencias por git/crates.io más pesadas y binario mayor. Se asume deliberadamente.
- **Coste**: la API real es la del rev pineado en `Cargo.lock`, no la de crates.io; los filtros de cuerpo son `async fn` y `ProxyHttp` es genérico en `DS`. Toda esta documentación cita ese rev.

### ¿Por qué Axum solo para la API de control?

La API `/api/v1/*` es CRUD autenticado, no un proxy: le benefician los extractors, la validación y el enrutado de Axum. Convive en el mismo proceso como `BackgroundService` para conservar un único binario, un único supervisor y un único apagado ordenado.

### ¿Por qué CouchDB en lugar de PostgreSQL?

- **Ventaja**: API REST nativa (API First) y sin driver extra.
- **Ventaja**: Feed `_changes` para invalidación de caché casi en tiempo real.
- **Ventaja**: Documentos JSON flexibles (whitelist, scripting, error_handling).
- **Desventaja**: Menos rendimiento en consultas complejas (no es nuestro caso).
- **Nota operativa**: la base `tele_proxy_configs` debe crearse manualmente tras el primer despliegue; el diseño `_design/proxy_lookup` sí se autogenera.

### ¿Por qué Valkey en lugar de Redis?

- **Ventaja**: Fork de Redis con mejor rendimiento y 100% compatible con el protocolo.
- **Ventaja**: Comunidad activa y desarrollo continuo.

### ¿Por qué el caché de respuestas es Valkey propio y no `HttpCache` de Pingora?

Porque el modelo de caché de tele-proxy no es el de un CDN genérico: la TTL depende del MIME, la clave codifica `config_version`, existe un nivel `FALLBACK` con `hash` verificado y una entrada `:stale` escrita por el propio servicio. Todo eso vive en `CacheStore` (Valkey + `postcard`) y no se expresa con las directivas de `HttpCache`.

En consecuencia **el módulo de caché de Pingora no se habilita**, y esto no es una omisión inocente: `HttpCache::new()` arranca en `CachePhase::Disabled(NoCacheReason::NeverEnabled)` (`pingora-cache/src/lib.rs:382`) y habilitarlo exige `HttpCache::enable(&'static (dyn storage::Storage + Sync), ..)` (`:571`), es decir un adaptador de vida `'static` sobre Valkey. Mientras la fase siga en `NeverEnabled`, `session.cache.set_max_file_size_bytes(..)` hace `panic!("wrong phase")` (`:776-784`), y `track_body_bytes_for_max_file_size(..)` (`:795`) y `exceeded_max_file_size()` (`:814`) fallan con `assert!`. Por eso el tope de 5 MB cacheable se mide con el contador propio del `CTX` dentro de `upstream_response_body_filter`, y `pingora-cache` **no** se declara en `Cargo.toml`.

La alternativa queda registrada pero no planificada: un `storage::Storage` sobre Valkey permitiría reutilizar el *cache fill* y el *stale-while-revalidate* de Pingora, a cambio de atar el esquema de claves a `CacheKey` (`pingora-cache/src/key.rs:108`) y perder el control sobre `:stale` y `px:defaults:{mime}`.

### ¿Por qué postcard en lugar de bincode?

`bincode` no se mantiene: la versión 3.0.0 de crates.io es un `compile_error!` deliberado y la 1.3 está en fin de vida. `postcard` es `serde`-based, `no_std`-friendly, mantenido y con codificación determinista, que es lo que necesita una clave de caché compartida entre instancias.

### ¿Por qué el caché DNS es in-process y no en Valkey?

Porque la IP validada debe viajarse unida al `SocketAddr` con el que se abre el TCP: si otra instancia escribiera `ip_cache:{hostname}` con una respuesta distinta, se rompería la garantía anti-rebinding y se añadiría un viaje de red a cada petición. El LRU del proceso es la única fuente de verdad; la clave `ip_cache:{hostname}` queda reservada pero sin escritor.

### ¿Por qué nanoid en lugar de UUID?

- **Ventaja**: URLs ~72% más cortas (12 caracteres frente a 36).
- **Ventaja**: URL-safe por defecto.
- **Ventaja**: 72 bits de entropía y rotable vía `/rotate-id`.

## Escalabilidad

### Horizontal

- El proceso es stateless salvo las cachés locales (Moka de configs, LRU de DNS), que son idempotentes y de corto TTL.
- N instancias detrás de un load balancer; la invalidación se propaga por `_changes` y por `config_version` dentro de la clave de caché.
- El rate limit vive en Valkey, así que es global entre instancias; el limiter `infrastructure::local_rate_limiter::LocalRateLimiter` solo actúa como red de seguridad por instancia cuando Valkey no responde.

### Vertical

- `pingora-runtime` monta el runtime Tokio del servicio: con `work_stealing = true` es **un** runtime
  con `worker_threads = ServerConf.threads` (`pingora-runtime/src/lib.rs:471`); con `false` son
  `threads` runtimes de un hilo y cada tarea queda fijada a su hilo. De ahí que este proyecto fije
  `threads` en el arranque (`ServerConf::new_with_opt_and_conf`, ver "Arranque") y no dependa de un
  `-c <conf.yaml>`: el default de `threads` es **1** (`configuration/mod.rs:236`).
- El pool de conexiones de upstream (`keepalive`) reduce el coste de TLS por solicitud.
- Límites configurables por cliente para adaptar al hardware disponible.

## Seguridad en Profundidad

| # | Nivel | Fase de Pingora |
|---|-------|-----------------|
| 1 | Cloudflare (TLS, DDoS, WAF) | borde |
| 2 | Validación de `crypt_id` (nanoid rotable) | `request_filter` (parte A) |
| 3 | Whitelist de dominios | `request_filter` (parte B) |
| 4 | Rate limiting por `crypt_id` (atómico, con `Retry-After`) | `request_filter` (parte B) |
| 5 | Validación estricta de URLs (anti-SSRF) | `request_filter` (parte B) |
| 6 | DNS seguro (DoT + DNSSEC, cadena de proveedores) | `upstream_peer` |
| 7 | Validación de IPs (no privadas) + pinning del peer | `upstream_peer` |
| 8 | Validación de `Location` en 3xx | `upstream_response_filter` |
| 9 | Sanado de cabeceras de framing/hop-by-hop | nativo de Pingora: `PeerOptions.http_upstream_request_policy` + `deny_upgrades()` en `upstream_peer` |
| 10 | Sandbox de Lua (sin acceso al sistema, deadline real) | `upstream_response_body_filter` (VM en `spawn_blocking` con `set_hook`) |
| 11 | Límites de recursos (memoria, tiempo, tamaño) | `upstream_response_body_filter` (contador propio en CTX + `Session::upstream_body_bytes_received()`) |
| 12 | Integridad de scripts y respuestas (`code_hash`, `hash` de fallback) | `upstream_response_body_filter` (hash por chunks) y carga de configuración |
| 13 | Logs de seguridad (solo errores, JSON, con `event=`) | `logging` (con `suppress_error_log` para no duplicar) |

Cada capa es obligatoria: ninguna se omite por conveniencia ni se delega en la anterior.
