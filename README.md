# TeleProxy

Proxy inverso seguro multi-cliente construido en Rust con arquitectura DDD (Domain-Driven Design).
Expone un endpoint público sin autenticación para recuperar recursos remotos (`/aq/{crypt_id}/`)
y una API de control autenticada para gestionar la configuración de cada cliente. Incluye
protección anti-SSRF de cinco pasos, rate limiting, caché de respuestas con fallback de 3 niveles
y scripting con Lua sandboxed por cliente.

**Dominio de referencia de despliegue**: `https://teleproxy.velone.ai`

> **Estado del proyecto**: las Fases 1 y 2 (cimientos DDD y seguridad DNS anti-SSRF) están
> completas. El motor de proxy se está portando de Axum+`reqwest` a Pingora (Fase 3, en curso);
> hoy `/aq/` lo sirve el handler Axum con el mismo contrato de seguridad. Ver
> `docs/spec.md`, "Estado de la migración Axum/reqwest -> Pingora".

---

## Tabla de Contenidos

1. [Características clave](#características-clave)
2. [Stack tecnológico](#stack-tecnológico)
3. [Arquitectura](#arquitectura)
4. [Prerrequisitos](#prerrequisitos)
5. [Puesta en marcha local](#puesta-en-marcha-local)
6. [Uso](#uso)
7. [Configuración de un cliente](#configuración-de-un-cliente)
8. [API de control](#api-de-control)
9. [Scripting Lua](#scripting-lua)
10. [Variables de entorno](#variables-de-entorno)
11. [Despliegue en producción](#despliegue-en-producción)
12. [Testing y calidad](#testing-y-calidad)
13. [Solución de problemas](#solución-de-problemas)
14. [Documentación](#documentación)

---

## Características clave

- **Endpoint público sin auth por diseño**: la seguridad no depende de un token en la URL
  pública, sino de una whitelist de dominios, rate limiting y validación estricta anti-SSRF
  aplicadas al vuelo.
- **Anti-SSRF en 5 pasos**: parseo estricto de URL (sin credenciales, sin fragmentos,
  solo http/https), whitelist por sufijo, resolución DNS manual con DoT + DNSSEC (Quad9 ECS →
  Cloudflare → AdGuard → CleanBrowsing → Google), pinning de la IP validada en la conexión TCP
  (anti DNS rebinding) y revalidación de redirects 3xx.
- **Multi-cliente**: cada cliente tiene su propio documento en CouchDB (`tele_proxy_configs`)
  con dominios permitidos, límites, scripting y fallbacks. Rotación de `crypt_id` con un
  endpoint.
- **Rate limiting distribuido**: contador atómico (`INCR` + `PEXPIRE` vía `EVAL`) en Valkey,
  con limiter local in-process como fallback degradado. Nunca se omite el límite.
- **Caché de respuestas**: capa propia sobre Valkey (serialización `postcard`) con TTL dinámico
  por MIME, copia `:stale` para servir en fallo y cabecera `X-Cache: HIT/MISS/FALLBACK/BYPASS`.
- **Fallbacks de 3 niveles**: caché stale por cliente → fallback global por MIME → recurso
  embebido, más `fallback_urls` configurables por el cliente con las mismas garantías anti-SSRF.
- **Lua sandboxed por cliente**: transformación de cuerpos con deadline real de 200 ms,
  límite de memoria, `os`/`io`/`debug` deshabilitados y API `proxy` segura
  (`log`, `regex_replace`, `http_request`).
- **Fail-safe por defecto**: si CouchDB es inaccesible, el proxy bloquea todo el tráfico con una
  configuración ultra-restrictiva (`default_degraded`) en lugar de degradarse a open relay.
- **Observabilidad**: logs estructurados JSON con `tracing`; el log de seguridad emite códigos
  de error del diccionario (`docs/ERROR_DICTIONARY.md`).

## Stack tecnológico

| Componente | Tecnología |
|---|---|
| Lenguaje | Rust (Edition 2021, `rust-version = "1.85"`) |
| Motor de proxy | [Pingora](https://github.com/cloudflare/pingora) 0.9 (rev pineado `4487f7b2`, feature `proxy`, TLS via rustls) |
| API de control | `axum` 0.8 (mismo binario, como servicio de fondo) |
| Runtime async | `tokio` 1.x |
| Base de datos | CouchDB 3.3 (configuración y metadatos) |
| Caché distribuida | Valkey 7.2 (compatible con Redis, serialización `postcard`) |
| Caché local | `moka` (configs, TTL 5 min) + `lru` (IPs DNS, in-process) |
| DNS | `hickory-resolver` con DoT + DNSSEC (`config/dns_resolvers.json` embebido) |
| Scripting | `mlua` (Lua 5.4 vendored, sandbox estricto) |
| IDs | `nanoid` (12 caracteres, URL-safe) |
| Despliegue | Docker Compose vía Dokploy |

**Restricción de plataforma**: Pingora es Linux/Unix-only (`epoll`/`io-uring`, señales POSIX).
El desarrollo en Windows trabaja el código independiente de plataforma
(`cargo check/test/clippy` con `default = []`); todo build que active `--features proxy`
se ejecuta en Linux (el Dockerfile construye siempre con `--features proxy`).

## Arquitectura

Arquitectura DDD en cuatro capas:

```
src/
├── domain/          # Lógica pura, sin dependencias de framework ni de red
│   ├── models.rs        # Entidades y value objects (ClientConfig, ProxyError, ...)
│   ├── validators.rs    # Validación estricta de URLs, IPs privadas, configs
│   ├── errors.rs        # ProxyError (thiserror) con códigos del diccionario
│   └── services.rs      # Servicios de dominio
├── application/     # Casos de uso y orquestación
│   ├── proxy_service.rs     # impl ProxyHttp para Pingora (objetivo Fase 3)
│   ├── proxy_handler.rs     # Handler Axum actual (hasta terminar la portación)
│   ├── lua_engine.rs        # Sandbox mlua con hook de deadline por instrucciones
│   ├── webhook_service.rs   # Puente Lua -> cliente HTTP validado
│   ├── fallback.rs          # Cadena de fallbacks de 3 niveles
│   ├── cache_key.rs         # Claves Valkey px:{internal_id}:{ver}:{sha256(url)}
│   └── changes_feed.rs      # Escucha del feed _changes de CouchDB (invalidación)
├── infrastructure/  # Adaptadores externos
│   ├── dns_resolver.rs      # SecureDnsResolver: DoT + DNSSEC + rechazo de IPs privadas
│   ├── http_client.rs       # Cliente HTTP con pinning (única salida de red permitida)
│   ├── couchdb_repo.rs      # Persistencia y views (by_token_hash, ...)
│   ├── valkey_cache.rs      # EVAL atómico de rate limit + caché postcard
│   └── local_rate_limiter.rs# Fallback in-process del rate limiter
└── interfaces/      # Entrada/salida HTTP
    ├── proxy_handler.rs     # Endpoint público /aq/{crypt_id}/
    └── control_api.rs       # API de control /api/v1/ (auth Bearer)
```

### Modelo de seguridad de dos niveles

| Nivel | Ruta | Auth | Protección |
|---|---|---|---|
| Público | `GET /aq/<crypt_id>/?url=...` | No | Whitelist + rate limit + anti-SSRF. `crypt_id` es un nanoid de 12 caracteres, rotable. |
| Control | `/api/v1/*` | Bearer (hash SHA-256 contra CouchDB) | Solo loopback en el compose (`127.0.0.1:8081`). Un token solo ve su propio cliente. |

### Flujo de una petición `/aq/`

```
Cliente → GET /aq/{crypt_id}/?url=https://ejemplo.com/img.jpg
   │
   ├─ 1. Validar crypt_id (regex ^[A-Za-z0-9_-]{12}$)
   ├─ 2. Parsear URL estricto (anti-SSRF paso 1: sin credenciales, sin fragmento, http/https)
   ├─ 3. Cargar ClientConfig desde CouchDB (caché Moka, TTL 5 min)
   ├─ 4. Comprobar whitelist del cliente (sufijo, case-insensitive)
   ├─ 5. Rate limit por crypt_id (Valkey EVAL atómico; fallback local degradado)
   ├─ 6. Resolver DNS con DoT+DNSSEC; rechazar IPs privadas (anti-SSRF paso 3)
   ├─ 7. Conectar TCP pinneado a la IP validada; SNI con el hostname (anti-SSRF paso 4)
   ├─ 8. Reenviar request-line/Host reescritos; añadir X-Forwarded-For/Proto
   ├─ 9. Revalidar Location en 3xx (anti-SSRF paso 5)
   ├─ 10. Caché Valkey: HIT → servir; MISS → fetch + guardar (tope 5 MB cacheable)
   ├─ 11. Scripting Lua del cliente (si está habilitado y el body cabe en el límite)
   ├─ 12. Fallback 3 niveles si el upstream falla; headers X-Cache, X-Content-Type-Options
   └─→ Respuesta al cliente
```

### Cadena de fallback ante error del upstream

1. **Caché stale** del cliente: `px:{internal_id}:{config_version}:{sha256(url)}:stale` (TTL 24 h) → `X-Cache: FALLBACK`
2. **Fallback global por MIME**: `px:defaults:{mime}` en Valkey (sin TTL)
3. **Recurso embebido**: compilado en el binario con `include_bytes!` según MIME
4. **`error_handling.fallback_urls`** del cliente: URL remota validada con el mismo pipeline anti-SSRF

## Prerrequisitos

- **Docker + Docker Compose v2** (recomendado; es el camino de desarrollo y producción)
- O, para desarrollo nativo: **Rust ≥ 1.85** (`rustup`), **Linux** si vas a activar `--features proxy`
- Acceso a CouchDB y Valkey (el compose los levanta; en el despliegue real de Velone viven
  detrás de `velone-servicios`, un hostname de Tailscale MagicDNS)

## Puesta en marcha local

```bash
# 1. Clonar el repositorio
git clone https://github.com/stringnetlab/tele-proxy.git
cd tele-proxy

# 2. Preparar el entorno
cp .env.example .env
# Editar .env: sustituir las tres credenciales (COUCHDB_USER, COUCHDB_PASSWORD, VALKEY_PASSWORD)

# 3. Levantar los servicios
docker compose up -d --build

# 4. Crear la base de datos en CouchDB (no se autogenera)
docker exec <contenedor_couchdb> \
  curl -s -X PUT http://localhost:5984/tele_proxy_configs \
  -u "$COUCHDB_USER:$COUCHDB_PASSWORD"

# 5. Verificar salud
curl http://localhost:8080/health
# → OK
```

En el primer arranque con la base vacía, el servicio siembra automáticamente un **cliente demo**:

```
{"level":"INFO","fields":{"message":"Demo client seeded","crypt_id":"V1StGXR8_Z5j",...}}
```

- Token Bearer del demo: `demo-token`
- Whitelist: `["example.com"]` · Rate limit: 5 req / 60 s · Sin scripting

### Primera petición de prueba

```bash
# Sustituir <crypt_id> por el valor del log de seed
curl "http://localhost:8080/aq/<crypt_id>/?url=https%3A%2F%2Fexample.com%2Fimage.jpg"
```

## Uso

### Endpoint público: `GET /aq/<crypt_id>/`

```
https://teleproxy.velone.ai/aq/{crypt_id}/?url={url_codificada}[&mime={mime}]
```

| Parámetro | Descripción |
|---|---|
| `crypt_id` | Nanoid de 12 caracteres (`A-Za-z0-9_-`) que identifica al cliente en la URL pública. Rotable sin cambiar el resto de la configuración. |
| `url` | **Requerida**, URL-encoded. Solo `http`/`https`; sin credenciales, sin fragmentos. El host debe estar en la whitelist del cliente (o ser IP pública permitida). |
| `mime` | Opcional. Hint de MIME para la decisión de caché/fallback. |

**Ejemplos con el dominio de producción:**

```bash
# Recurso básico
curl "https://teleproxy.velone.ai/aq/V1StGXR8_Z5j/?url=https%3A%2F%2Fshutterstock.com%2Fimg%2Fphoto.jpg"

# Con hint de MIME
curl "https://teleproxy.velone.ai/aq/V1StGXR8_Z5j/?url=https%3A%2F%2Fcdn.ejemplo.com%2Flogo.png&mime=image%2Fpng"

# Respuesta con caché HIT
curl -i "https://teleproxy.velone.ai/aq/V1StGXR8_Z5j/?url=https%3A%2F%2Fexample.com%2Fa.css"
```

Headers relevantes de la respuesta:

| Header | Valores | Significado |
|---|---|---|
| `X-Cache` | `HIT` / `MISS` / `FALLBACK` / `BYPASS` | Estado de la caché para esta respuesta |
| `X-Content-Type-Options` | `nosniff` | Protección MIME fija |
| `X-Proxy-By` | `tele.velone.ai` | Identificación del proxy |
| `X-Degraded-Mode` | `true` | El modo degradado está activo (CouchDB o Valkey caído; rate limit por proceso) |
| `Retry-After` | segundos | Con `429 Too Many Requests`: espera antes de reintentar |

**Reglas del endpoint:**
- Solo `GET`. Las únicas rutas del puerto del proxy son `/aq/{crypt_id}/`, `/health` y `/`; cualquier otra responde `404` sin tocar el origen.
- `GET /` responde el nombre del proyecto (`TELE - PROXY`); con `MODO=desarrollo` añade la lista de endpoints definidos.
- `GET /health` en el puerto del proxy responde `200 OK` antes de cualquier validación
  (es el endpoint del `HEALTHCHECK` del Dockerfile).
- El proxy **no sigue redirects**: reenvía el 3xx al cliente, pero valida `Location` con el
  anti-SSRF y lo **reescribe como URL del proxy** (`/aq/{crypt_id}/?url=<destino>`), para que el
  cliente pueda seguirlo (un `Location` relativo del origen resuelto contra `/aq/` rompería).
  El destino de un redirect inválido (esquema no HTTP, IP privada, …) nunca se reenvía: la
  petición se bloquea con `ssrf_blocked`. La petición de seguimiento reaplica whitelist, rate
  limit y pinning DNS; los 3xx no se cachean.
- Tope de respuesta del origen: 100 MB (aborta el stream con `payload_too_large`, sin bufferizar).

### Errores

Las respuestas de error son JSON con códigos del diccionario (`docs/ERROR_DICTIONARY.md`):

```json
{"error":"domain_not_whitelisted","message":"Domain not whitelisted: evil.com","url":"https://evil.com/x"}
```

Con `MODO=desarrollo` los cuerpos de error son diagnósticos: los 5xx exponen el motivo interno
completo y todos los errores incluyen un objeto `details` con los campos estructurados
(`reason`, `domain`, `retry_after_secs`, …):

```json
{
  "error": "rate_limit_exceeded",
  "message": "Rate limit exceeded",
  "details": {
    "current_count": "51",
    "max_requests": "50",
    "retry_after_secs": "60"
  }
}
```

En cualquier otro modo los 5xx devuelven el mensaje escueto `Internal server error` y sin
`details`. Los fallos del **origen** se distinguen de los del propio proxy sin filtrar detalles
internos: `upstream_error` → `Bad gateway`, `upstream_timeout` → `Gateway timeout`:

| Código | HTTP | Causa típica |
|---|---|---|
| `invalid_url_format` | 400 | URL malformada, con credenciales, fragmento o esquema no permitido |
| `invalid_crypt_id` | 400 | `crypt_id` con formato inválido |
| `domain_not_whitelisted` | 403 | Host fuera de la whitelist del cliente |
| `client_not_found` | 404 | `crypt_id` desconocido (o rotado) |
| `ssrf_blocked` | 403 | IP privada resuelta, redirect a IP privada, DNS rebinding |
| `rate_limit_exceeded` | 429 | Límite del cliente superado; incluye `Retry-After` |
| `payload_too_large` | 413 | Respuesta del origen > 100 MB |
| `upstream_error` | 502 | Fallo de transporte hacia el origen (conexión/TLS rechazados) |
| `upstream_timeout` | 504 | El origen no respondió antes de 30 s |
| `script_timeout` | — | Script Lua abortado por deadline (se degrada al body original) |
| `integrity_check_failed` | — | `code_hash` del script no coincide (se degrada al body original) |

## Configuración de un cliente

Cada cliente es un documento JSON en la base `tele_proxy_configs` de CouchDB, gestionado
íntegramente vía la API de control. Referencia completa en `docs/CLIENT_CONFIG.md`.

### Documento completo de ejemplo

```json
{
  "internal_id": "client_int_9f8e7d",
  "crypt_id": "V1StGXR8_Z5j",
  "config_version": 3,
  "whitelist": ["shutterstock.com", "gettyimages.com", "8.8.8.8"],
  "rate_limit": { "max_requests": 500, "window_seconds": 60 },
  "max_scripting_body_bytes": 5242880,
  "scripting": {
    "enabled": true,
    "code": "function handle(req, res) ... end",
    "code_hash": "sha256:..."
  },
  "error_handling": {
    "mode": "wrapped",
    "fallback_urls": {
      "image/*": { "url": "https://cdn.example.com/error.png", "hash": "sha256:..." }
    }
  },
  "header_rules": [
    {
      "expression": "http.response.status eq 200 and starts_with(http.response.content_type, \"application/json\")",
      "action": "set",
      "action_parameters": {
        "headers": [
          { "name": "x-algo", "value": "asi", "operation": "set" }
        ]
      }
    }
  ]
}
```

### Opciones disponibles

#### Identificadores

| Campo | Tipo | Descripción |
|---|---|---|
| `internal_id` | string | Identificador interno, inmutable. Aísla los datos del cliente. |
| `crypt_id` | string | Nanoid de 12 caracteres que viaja en la URL pública. Se rota con `POST /api/v1/clients/rotate-id`; al rotarlo cambia la URL pero no el resto de la config. |
| `bearer_token_hash` | `sha256:<64 hex>` | Hash SHA-256 del token Bearer. El token en claro nunca se almacena. |
| `config_version` | u64 | Se incrementa automáticamente en cada `PUT` o rotación; forma parte de la clave de caché. |

#### `whitelist` — dominios permitidos

```json
"whitelist": ["shutterstock.com", "8.8.8.8"]
```

| Restricción | Valor |
|---|---|
| Máximo | 100 entradas |
| Formato | Hostname desnudo (sin esquema, puerto, path, credenciales ni fragmento) o IP **pública** |
| Matching | Sufijo, case-insensitive: `cdn.shutterstock.com` matchea `shutterstock.com` |
| Rechazadas | `localhost`, IPs privadas (RFC 1918, loopback, link-local, CGNAT), `dominio:8080`, `https://dominio`, `user:pass@dominio` |

#### `rate_limit` — límite de peticiones

```json
"rate_limit": { "max_requests": 500, "window_seconds": 60 }
```

| Campo | Rango | Default demo | Descripción |
|---|---|---|---|
| `max_requests` | 1 – 10 000 | 5 | Máximo de peticiones por ventana |
| `window_seconds` | 1 – 3 600 | 60 | Duración de la ventana deslizante (segundos) |

- Se aplica **por `crypt_id`**, no por IP.
- Almacenado en Valkey (contador atómico con TTL); si Valkey cae, limiter local in-process
  con la misma cuota y `X-Degraded-Mode: true`.
- Al excederlo: `429` con `Retry-After`.

#### `scripting` — transformación con Lua

```json
"scripting": { "enabled": true, "code": "...", "code_hash": "sha256:..." }
```

| Campo | Descripción |
|---|---|
| `enabled` | Activa/desactiva la ejecución del script para este cliente |
| `code` | Código fuente Lua 5.4, ejecutado en sandbox mlua con deadline estricto |
| `code_hash` | SHA-256 del código; obligatorio si `code` no está vacío. Si no coincide, se sirve el body sin transformar |
| `max_scripting_body_bytes` | 1 MB – 50 MB. Cuerpos mayores entran a Lua en bypass (se sirven sin transformar) |

Límites del sandbox (variables de entorno): `LUA_TIMEOUT_MS` (default 200), `LUA_MEMORY_LIMIT_MB`
(50), `WEBHOOK_TIMEOUT_MS` (5 000). Ver [Scripting Lua](#scripting-lua).

#### `error_handling` — errores y fallbacks

```json
"error_handling": {
  "mode": "wrapped",
  "fallback_urls": {
    "image/*": { "url": "https://cdn.example.com/error.png", "hash": "sha256:..." },
    "text/html": { "url": "https://cdn.example.com/error.html", "hash": "sha256:..." }
  }
}
```

| Modo | Comportamiento |
|---|---|
| `transparent` | Reenvía el error del upstream tal cual (status, headers, body). No se usan `fallback_urls`. |
| `wrapped` | Si el upstream falla y existe un fallback para el MIME de la respuesta, se sirve el fallback en su lugar. |

`fallback_urls` usa como clave un patrón de MIME (`type/subtype` o `type/*`). La `url` se valida
con el mismo pipeline anti-SSRF (http/https, sin credenciales, sin IPs privadas). El `hash`
garantiza la integridad del contenido servido.

#### `header_rules` — modificación de headers de respuesta

Reglas estilo [Cloudflare Ruleset Engine](https://developers.cloudflare.com/ruleset-engine/about/rules/)
para inyectar, modificar o eliminar headers de la respuesta según condiciones sobre la petición
y el upstream:

```json
"header_rules": [
  {
    "expression": "http.response.status eq 200 and starts_with(http.response.content_type, \"application/json\")",
    "action": "set",
    "action_parameters": {
      "headers": [
        { "name": "x-algo", "value": "asi", "operation": "set" }
      ]
    }
  },
  {
    "expression": "http.request.headers[\"x-client\"] in {\"ios\" \"android\"}",
    "action": "set",
    "action_parameters": {
      "headers": [
        { "name": "x-platform", "value": "${http.request.headers[\"x-client\"]}", "operation": "set" }
      ]
    }
  }
]
```

| Campo | Descripción |
|---|---|
| `expression` | Condición evaluada por petición. Máx. 4.096 caracteres. |
| `action` | `"set"` (única por ahora; como las reglas de headers de Cloudflare). |
| `action_parameters.headers` | Operaciones `set` (reemplaza), `add` (acumula valores) o `remove`. Máx. 10 por regla. El `value` admite placeholders `${campo}`. |

- Sintaxis de expresiones: `eq`/`ne` (o `==`/`!=`), conjuntos `in { ... }`, `and`/`or`/`not`,
  paréntesis; funciones `starts_with`, `ends_with`, `contains`, `matches` (regex),
  `lower`, `upper`, `len`, `concat`.
- Campos: `http.response.status`, `http.response.content_type`,
  `http.response.headers["nombre"]`, `http.request.method`,
  `http.request.headers["nombre"]`, `url.scheme`, `url.host`, `url.path`, `url.query`.
- Se aplican en orden sobre cualquier respuesta servida (upstream, caché HIT o fallback); los
  headers que fija el proxy siempre ganan y no pueden tocarse desde una regla.
- Cotas: 50 reglas por cliente; `[]` las desactiva. Referencia completa del lenguaje con
  ejemplos por caso de uso: **`docs/HEADER_RULES.md`**.

### Cliente demo (seed)

Al arrancar con CouchDB vacío se crea automáticamente un cliente de prueba con permisos
mínimos (ver la tabla en `docs/CLIENT_CONFIG.md`). Su `crypt_id` se imprime en los logs.

### Modo degradado (CouchDB caído)

Si CouchDB no responde, el proxy bloquea todo el tráfico con una config ultra-restrictiva:
whitelist vacía, 3 req/60 s, sin scripting. Nunca actúa como open relay ante un fallo de
configuración.

## API de control

Autenticación: `Authorization: Bearer <token>` en todos los endpoints. El token se hashea
con SHA-256 y se busca en CouchDB (view `by_token_hash`); cada token solo accede a su propio
`internal_id`. En el compose, el puerto 8081 se publica **solo en loopback** — es un endpoint
de administración.

Base URL en producción: `http://127.0.0.1:8081` (o túnel SSH al servidor).

| Método | Ruta | Descripción |
|---|---|---|
| `GET` | `/api/v1/clients/config` | Configuración actual del cliente (sin secretos) |
| `PUT` | `/api/v1/clients/config` | Actualización parcial: solo cambian los campos enviados; valida cotas y devuelve `400 invalid_config` si algo está fuera de rango |
| `POST` | `/api/v1/clients/rotate-id` | Rota el `crypt_id` (genera uno nuevo y devuelve el anterior inmediatamente útil para migración) |
| `GET` | `/health` | Healthcheck: `200 OK` |

> No existe `POST /api/v1/clients`: el aprovisionamiento de clientes nuevos se hace fuera de
> este servicio (directamente en CouchDB).

### Ejemplos

```bash
TOKEN="mi-token-secreto"
API="http://127.0.0.1:8081/api/v1"

# 1. Ver la configuración actual
curl -H "Authorization: Bearer $TOKEN" "$API/clients/config"

# 2. Actualizar whitelist y rate limit (los demás campos se conservan)
curl -X PUT "$API/clients/config" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "whitelist": ["shutterstock.com", "gettyimages.com"],
    "rate_limit": { "max_requests": 500, "window_seconds": 60 }
  }'

# 3. Rotar el crypt_id (la URL pública cambia; la config no)
curl -X POST -H "Authorization: Bearer $TOKEN" "$API/clients/rotate-id"

# 4. Activar scripting con verificación de integridad
SCRIPT=$(cat mi_script.lua)
HASH="sha256:$(printf '%s' "$SCRIPT" | sha256sum | cut -d' ' -f1)"
curl -X PUT "$API/clients/config" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{
    \"scripting\": { \"enabled\": true, \"code\": $(printf '%s' "$SCRIPT" | jq -Rs .), \"code_hash\": \"$HASH\" }
  }"
```

## Scripting Lua

El script de un cliente recibe la respuesta del upstream y puede transformar su cuerpo antes
de servirla al cliente final.

```lua
-- Ejemplo: reescribir URLs en el HTML del origen
function handle(req, res)
  proxy.log("info", "procesando " .. req.url)

  if res.status == 200 and res.headers["content-type"]:find("text/html") then
    res.body = proxy.regex_replace(res.body, "http://", "https://", 1)
  end

  -- Webhook: sale por el mismo pipeline anti-SSRF (whitelist + DoT + pinning)
  local r = proxy.http_request("https://api.example.com/hook", "POST", res.body, 3000)
  proxy.log("info", "webhook status: " .. tostring(r.status))

  return res
end
```

### API disponible en el sandbox

| Función | Descripción |
|---|---|
| `proxy.log(level, msg)` | Log estructurado (`info`, `warn`, `error`) hacia `tracing` |
| `proxy.regex_replace(text, pattern, replacement, limit?)` | Sustitución regex con límite anti-ReDoS de 100 ms |
| `proxy.http_request(url, method, body, timeout_ms)` | Webhook HTTP validado (mismo pipeline anti-SSRF que el proxy; sin redirects). Sticky: `SsrfBlocked`/`DomainNotWhitelisted` abortan la petición aunque se envuelva en `pcall` |

### Garantías del sandbox

- `os`, `io`, `package`, `debug`, `dofile`, `loadfile`, `load` están deshabilitados.
- Deadline **real** de ejecución: hook de interrupción por contador de instrucciones (un
  `while true do end` se aborta en <1 s con `script_timeout`), no una comprobación a posteriori.
- Memoria limitada (`LUA_MEMORY_LIMIT_MB`, default 50 MB).
- Los cuerpos que superan `max_scripting_body_bytes` no entran a la VM: se sirven sin transformar.
- El socket nunca se abre en el hilo de la VM: `proxy.http_request` puente a tokio vía
  `Handle::spawn` con su propio timeout.

## Variables de entorno

Lista canónica en `docs/ENVIRONMENT.md`. Resumen:

### Aplicación

| Variable | Descripción | Default |
|---|---|---|
| `RUST_LOG` | Filtro de logs (`EnvFilter`) | `warn,tele_proxy=info` |
| `RUST_BACKTRACE` | Backtrace en pánico | `1` |
| `MODO` | `desarrollo` → errores HTTP verbosos (motivo interno + `details`) y `GET /` lista los endpoints; cualquier otro valor o ausente → respuestas escuetas | escueto |

### Escucha HTTP

| Variable | Descripción | Default |
|---|---|---|
| `HTTP_HOST` | Host de escucha | `0.0.0.0` |
| `HTTP_PROXY_PORT` | Puerto del endpoint público `/aq/` | `8080` |
| `HTTP_CONTROL_PORT` | Puerto de la API de control | `8081` |

### Runtime de Pingora (ServerConf)

| Variable | Default | Nota |
|---|---|---|
| `PROXY_WORKER_THREADS` | nº de CPUs del host | **Trampa**: el default de Pingora es 1 thread; sin fijarlo el proxy es mono-thread |
| `PROXY_WORK_STEALING` | `true` | |
| `PROXY_GRACE_PERIOD_SECONDS` | `10` | Debe ser menor que `stop_grace_period` del compose (15 s) |
| `UPSTREAM_KEEPALIVE_POOL_SIZE` | `128` | Campo marcado inestable por Pingora: revisar al subir el rev |

### CouchDB

| Variable | Default | Requerida |
|---|---|---|
| `COUCHDB_URL` | `http://couchdb:5984` | Sí |
| `COUCHDB_USER` | — | **Sí, sin default** |
| `COUCHDB_PASSWORD` | — | **Sí, sin default** |
| `COUCHDB_DB_NAME` | `tele_proxy_configs` | La base se crea a mano |

### Valkey

| Variable | Descripción |
|---|---|
| `VALKEY_URL` | `redis://:<password>@valkey:6379` — se **compone** desde `VALKEY_PASSWORD`, nunca con la contraseña literal en un documento |
| `VALKEY_PASSWORD` | Solo referenciada por compose/`${}` |

### Límites del sistema

| Variable | Default | Rango válido |
|---|---|---|
| `LUA_TIMEOUT_MS` | `200` | 1 – 60 000 (fuera de rango: el proceso no arranca) |
| `LUA_MEMORY_LIMIT_MB` | `50` | — |
| `WEBHOOK_TIMEOUT_MS` | `5000` | 1 – 60 000 |
| `MAX_RESPONSE_SIZE_BYTES` | `104857600` (100 MB) | — |
| `CONFIG_CACHE_TTL_SECONDS` | `300` | — |
| `CONFIG_CACHE_MAX_CAPACITY` | `10000` | — |

### DNS (DoT + DNSSEC)

La lista primaria de resolvedores vive en `config/dns_resolvers.json`, compilada en el binario:
Quad9 ECS → Cloudflare Security → AdGuard → CleanBrowsing (`security-dns.nl`) → Google, todos
puerto 853 con DNSSEC. `DNS_DOT_SERVERS` es solo la cadena de reserva configurable por el operador.

## Despliegue en producción

El despliegue objetivo es **Docker Compose vía Dokploy**, con TLS terminado por Dokploy
(`teleproxy.velone.ai` apunta al puerto 8080; el 8081 queda solo en loopback del host).

```bash
# Build de la imagen (multi-etapa: rust:bookworm → debian:bookworm-slim, binario strip + LTO fat)
docker compose build

# Levantar
docker compose up -d

# Crear la base de datos (primer arranque únicamente)
docker exec <couchdb> curl -s -X PUT \
  http://localhost:5984/tele_proxy_configs -u "$COUCHDB_USER:$COUCHDB_PASSWORD"
```

Detalles operativos importantes (`docs/DEPLOYMENT.md` tiene el procedimiento completo):

- **El binario se construye con `--features proxy`**: sin ese feature no hay motor de proxy.
- **Build con `--locked`**: el `Cargo.lock` está commiteado y fija el rev de Pingora
  (`4487f7b2`); regenerar el lock sin `--locked` podría desplazar el commit.
- **CouchDB sin `ports:` públicos**: solo se alcanza por la red interna (en Velone, vía
  `velone-servicios`, hostname de Tailscale MagicDNS mapeado en `extra_hosts`).
- **Contraseña de Valkey por entorno del contenedor**, no por `argv` (no visible en
  `docker inspect`).
- **IPv6**: la red por defecto de Docker no tiene ruta v6; el resolver prefiere IPv4 y el
  peer se construye con `SocketAddr` IPv4 salvo salida v6 explícita.
- **Apagado ordenado**: `stop_grace_period: 15s` > `PROXY_GRACE_PERIOD_SECONDS: 10`; el
  proceso nunca se daemoniza (`daemon: false`).

## Testing y calidad

```bash
# En Linux (con el motor Pingora)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --features proxy

# En Windows (código multiplataforma, sin feature proxy)
cargo check
cargo test
```

Las pruebas cubren los escenarios Gherkin de `specs/*.feature` (generados desde `docs/BDD.md`):
proxy público, anti-SSRF/DNS, caché y streaming, sandbox Lua y manejo de errores/MIME.

> Un `exit 0` del envoltorio no es prueba: hay que leer el log real del comando
> (`docs/spec.md`, Instrucciones para el Agente).

## Solución de problemas

**El proxy responde 403 `domain_not_whitelisted` para un dominio válido.**
El matching es por sufijo: `cdn.shutterstock.com` matchea `shutterstock.com`, pero
`shutterstock.com.cdn.net` no. Verifica la entrada exacta con `GET /api/v1/clients/config`.

**Todas las peticiones responden 404 `client_not_found` tras un redepliegue.**
El `crypt_id` se rota vía `POST /api/v1/clients/rotate-id`; si se perdió CouchDB o el
volumen, el cliente demo se siembra de nuevo con un `crypt_id` nuevo — recógalo del log
(`Demo client seeded`).

**`429` constante aunque haga pocas peticiones.**
El rate limit es por `crypt_id`, no por IP: todos los consumidores de la misma URL comparten
la cuota. Sube `rate_limit.max_requests` o baja `window_seconds` con `PUT /config`.

**Las respuestas llevan `X-Degraded-Mode: true`.**
Valkey o CouchDB están inaccesibles desde el contenedor. En el despliegue de Velone, revisa
que `extra_hosts` resuelva `velone-servicios` y que Tailscale esté activo en el host.

**El healthcheck de Docker falla.**
`/health` se responde dentro del `request_filter` del puerto 8080 antes de cualquier
validación; si el feature `proxy` no se compiló, el listener no existe y el contenedor
entra en ciclo de reinicio.

**El proceso no arranca con `Configuration error, aborting startup`.**
Alguna variable numérica está fuera de rango (p. ej. `LUA_TIMEOUT_MS=99999`; rango 1–60000).
El error sale por `tracing` en JSON con el motivo exacto.

**Scripts Lua no se ejecutan.**
Comprueba `scripting.enabled`, que `code_hash` sea `sha256:` del código exacto (sin trailing
newline extra), que el cuerpo no supere `max_scripting_body_bytes` (si lo supera, el bypass
es el comportamiento correcto) y los logs: `script_timeout` e `integrity_check_failed`
degradan al body original en lugar de romper la respuesta.

## Documentación

| Archivo | Contenido |
|---|---|
| `docs/spec.md` | Especificación técnica completa (arquitectura, fases, estado de la migración a Pingora) |
| `docs/CLIENT_CONFIG.md` | Referencia de cada campo del documento de cliente |
| `docs/HEADER_RULES.md` | Lenguaje de expresiones de `header_rules` (sintaxis Cloudflare, funciones, ejemplos) |
| `docs/ENVIRONMENT.md` | Lista canónica de variables de entorno y validaciones |
| `docs/ERROR_DICTIONARY.md` | Diccionario completo de códigos de error |
| `docs/DEPLOYMENT.md` | Procedimiento de despliegue con Dokploy, incluido "Subir el Rev de Pingora" |
| `docs/ARCHITECTURE.md` / `docs/DIAGRAMS.md` | Diseño detallado y diagramas de flujo |
| `docs/BDD.md` + `specs/*.feature` | Escenarios de comportamiento (Gherkin) |
| `docs/RUST_STYLE_GUIDE.md` | Convenciones de código Rust del proyecto |
| `docs/api_contract.yaml` | Contrato OpenAPI de la API de control |
| `config/dns_resolvers.json` | Cadena de resolvedores DoT compilada en el binario |

---

**Licencia**: MIT
