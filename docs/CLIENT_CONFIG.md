# Configuración por Cliente

Cada cliente del proxy tiene un documento en CouchDB (base `tele_proxy_configs`) que controla
todos los aspectos de su comportamiento: identificadores, dominios permitidos, rate limiting,
scripting Lua, manejo de errores, reglas de headers y endpoints de preview. Este documento es la
referencia completa: cada campo, su tipo, su valor por defecto, sus cotas y cómo se recarga.

Documentos relacionados:

- `docs/api_contract.yaml` — contrato HTTP (incluye `ClientConfigUpdate`, `PreviewConfig` y las
  rutas admin/cliente).
- `docs/HEADER_RULES.md` — lenguaje completo de `header_rules`.
- `docs/LUA_SCRIPTING.md` — sandbox Lua y recetas de scripting.
- `docs/ENVIRONMENT.md` — variables de entorno (caché de configs, sidecar de render, etc.).
- `docs/ERROR_DICTIONARY.md` — significados de los códigos de error.

---

## Ciclo de vida del documento

1. **Indexación**: el proxy busca al cliente por `crypt_id` en la vista `by_crypt_id`. La vista solo
   emite documentos con `type == "client_config"` y `crypt_id` no vacío; un documento sin `type`
   es invisible para la vista (el endpoint responde `404 config_not_found`). La autenticación de la
   API de cliente usa `by_token_hash`.
2. **Caché local**: la config se guarda en una caché Moka en memoria, indexada por `crypt_id` y por
   `bearer_token_hash`, con TTL `CONFIG_CACHE_TTL_SECONDS` (default **300 s**) y capacidad
   `CONFIG_CACHE_MAX_CAPACITY` (default **10000**). La caché es una red de seguridad, no el
   mecanismo principal de actualización.
3. **Recarga**: el feed `_changes` consulta CouchDB cada **5 s** (backoff exponencial hasta 60 s si
   falla). Al detectar un documento `client_config` nuevo o modificado invalida la entrada de caché
   de ese `internal_id`; al detectar un borrado la invalida por `_id`. Por eso un cambio guardado
   en CouchDB tarda como máximo ~5 s en ser efectivo, sin reiniciar el proxy.
4. **Fallo de CouchDB**: si la conexión falla en el momento de resolver el cliente, se devuelve la
   config degradada (ver § Modo degradado). Si el documento existe pero **no parsea**, la petición
   responde `500` con el detalle de serde en el log (campo que falta o tipo equivocado).
5. **Claves ausentes**: el documento se deserializa con `#[serde(default)]` a nivel de contenedor.
   Cualquier clave ausente toma **su valor por defecto** (tabla siguiente); las claves desconocidas
   se ignoran. Esto permite documentos escritos a mano o migrados desde versiones anteriores.
   `null` explícito **no** equivale a ausente: `"_rev": null` en `_rev` es correcto (`Option`),
   pero `"whitelist": null` es un error de tipo.

### Valores por defecto si la clave falta

| Campo | Default |
|---|---|
| `_id` | `""` |
| `_rev` | ausente (`null`) |
| `type` | `"client_config"` |
| `internal_id` | `""` → al leer se adopta el `_id` del documento |
| `crypt_id` | `""` |
| `bearer_token_hash` | `""` |
| `config_version` | `0` |
| `kind` | `"client"` |
| `wildcard` | `false` |
| `whitelist` | `[]` |
| `rate_limit` | `max_requests: 50`, `window_seconds: 60` |
| `max_scripting_body_bytes` | `5242880` (5 MiB) |
| `scripting` | `enabled: false`, `code: ""`, `code_hash: ""`, `expression: ""` |
| `error_handling` | `mode: "transparent"`, `fallback_urls: {}` |
| `header_rules` | `[]` |
| `preview` | `enabled: false`, `meta_enabled: true`, capacidades `meta`/`content` activas, `screenshot`/`pdf` apagadas, rate `60/60`, TTL `86400`/`2678400`, render `45000`, cuerpo `2097152`, viewport `[1920, 1080]` |

> **Importante**: que una clave falte no exime de cumplir el contrato cuando se escribe por la API.
> Los `PUT` validan antes de guardar; un valor fuera de cota devuelve `400 invalid_config` con el
> campo concreto. La edición directa en CouchDB **no** pasa por esa validación: el documento se
> acepta tal cual y el error aparece al usarlo (o nunca, si el proxy puede seguir con defaults).

---

## Documento completo de referencia

```json
{
  "_id": "b116a9228725658d0d14f328c70350c1",
  "_rev": "12-8d1f0a2b3c4d5e6f7a8b9c0d1e2f3a4b",
  "type": "client_config",
  "internal_id": "pixelphone",
  "crypt_id": "pHnNeVj8fR83",
  "bearer_token_hash": "sha256:9f2c1d7a4b6e8f0a1c3e5d7b9a2f4c6e8d0b1a3c5e7f9d2b4a6c8e0f1d3b5a7c",
  "config_version": 7,
  "kind": "client",
  "wildcard": true,
  "whitelist": ["celebmafia.com", "example.com"],
  "rate_limit": { "max_requests": 1000, "window_seconds": 60 },
  "max_scripting_body_bytes": 5242880,
  "scripting": {
    "enabled": false,
    "code": "",
    "code_hash": "",
    "expression": ""
  },
  "error_handling": {
    "mode": "transparent",
    "fallback_urls": {}
  },
  "header_rules": [
    {
      "expression": "true eq true",
      "action": "set",
      "action_parameters": {
        "headers": [
          { "name": "server", "operation": "remove" },
          { "name": "x-powered-by", "operation": "remove" }
        ]
      }
    }
  ],
  "preview": {
    "enabled": true,
    "meta_enabled": true,
    "capabilities": { "meta": true, "content": true, "screenshot": false, "pdf": false },
    "rate_limit": { "max_requests": 60, "window_seconds": 60 },
    "ttl_default_seconds": 86400,
    "ttl_max_seconds": 2678400,
    "max_render_timeout_ms": 45000,
    "max_body_bytes": 2097152,
    "screenshot": { "max_full_page": true, "default_viewport": [1920, 1080] }
  }
}
```

`_rev` lo asigna CouchDB; el valor del ejemplo es ficticio. `bearer_token_hash` contiene el hash
del token, nunca el token en claro.

---

## Identificadores y metadatos

| Campo | Tipo | Descripción |
|---|---|---|
| `_id` | `string` | ID del documento CouchDB. Los clientes creados por la API usan `_id == internal_id`. En documentos importados a mano puede ser un UUID distinto; se recomienda mantenerlos iguales (ver § Edición directa en CouchDB). |
| `_rev` | `string` | Revisión de CouchDB. Obligatoria para actualizar el documento; puede faltar al leer (se toma `null`). |
| `type` | `string` | Debe ser `"client_config"`. La vista `by_crypt_id` solo indexa documentos con este `type`; sin él el cliente es invisible al proxy. |
| `internal_id` | `string` | Identificador interno estable. Clave de la caché de config y de las claves de respuesta. Si el documento no lo trae, al leer se adopta el `_id`. No lo cambies: cambia la identidad del cliente y sus cachés. |
| `crypt_id` | `string` | Nanoid de 12 caracteres (`A-Za-z0-9_-`). Viaja en la URL pública: `/aq/{crypt_id}/`, `/mt/{crypt_id}`, `/pv/{crypt_id}`. Rotable con `POST /api/v1/clients/rotate-id` o `POST /api/v1/admin/clients/{id}/rotate-id`. Debe ser único. |
| `bearer_token_hash` | `string` | `sha256:<64 hex>` del bearer token. Autentica en la API de cliente (`Authorization: Bearer <token>`). El token en claro solo existe en la respuesta de creación/rotación. |
| `config_version` | `u64` | Versión de la configuración. Los `PUT` (API de cliente y admin), `rotate-token` y `rotate-id` la incrementan en 1. Forma parte de las claves de caché de respuestas (`px:{internal_id}:{config_version}:...`) y de preview (`pv:{capability}:{internal_id}:{config_version}:...`), así que subirla invalida lo cacheado. Una edición directa en CouchDB **no** la incrementa automáticamente. |
| `kind` | `"client"` \| `"admin"` | Naturaleza del documento. `admin` identifica operadores del panel: no pueden usar el proxy ni la API de cliente (403) y la API admin los ignora. No es modificable por ningún `PUT`; por defecto `"client"`. |
| `wildcard` | `bool` | Cliente de confianza con salida libre a cualquier dominio (ver § Whitelist). Solo la API admin puede fijarlo. Por defecto `false`. |

---

## Whitelist de Dominios

```json
"whitelist": ["shutterstock.com", "gettyimages.com", "8.8.8.8"]
```

| Restricción | Valor |
|---|---|
| Máximo de entradas | 100 |
| Formato | Hostname desnudo (sin esquema, puerto, path, credenciales ni fragmentos) |
| IPs privadas | Rechazadas (RFC 1918, loopback, link-local, CGNAT, documentación, etc.) |
| IPs públicas | Permitidas como entradas |
| Matching | Sufijo, case-insensitive: `cdn.shutterstock.com` matchea `shutterstock.com` |

**Validación**: cada entrada debe ser un hostname válido (máx. 253 bytes, etiquetas de máx. 63 bytes,
TLD alfabético) o una IP pública. Se rechazan: `localhost`, `shutterstock.com:8080`,
`https://shutterstock.com`, `user:pass@shutterstock.com`, IPs privadas (`127.0.0.1`, `10.x`, etc.).

Notas:

- La lista se **sustituye completa** en cada `PUT`; `[]` deja al cliente sin destinos permitidos.
- `"*"` **no** es un comodín. No es un hostname válido y la API lo rechaza; si aparece en un
  documento escrito a mano, es una entrada inerte (`domain == "*"` nunca matchea). La salida libre
  se consigue exclusivamente con `wildcard: true`.

### Salida libre (`wildcard`)

Campo **solo-admin** del documento (`"wildcard": true`): el cliente se convierte en un cliente
especial de confianza que puede salir a **cualquier dominio** — la whitelist se ignora por
completo (puede incluso quedar vacía).

- **Quién puede activarlo**: únicamente la API admin (`POST /api/v1/admin/clients` con
  `{ "wildcard": true }` o `PUT /api/v1/admin/clients/{id}` con `{ "wildcard": true|false }`). El
  cliente jamás: `PUT /api/v1/clients/config` ni siquiera recibe el campo (`ClientConfigUpdate` no
  lo tiene, es imposible por construcción). Cada toggle queda auditado (`client_wildcard_toggle`).
- **Lo que NO salta**: el anti-SSRF de IP sigue aplicando exactamente igual — IPs privadas
  literales en la URL, pinning DNS y resolución validada. Un cliente wildcard puede salir a
  dominios ajenos, pero no a redes internas.
- **En scripting**: `proxy.http_request` hereda el flag, así que el webhook del sandbox salta
  la whitelist del mismo modo (con las mismas garantías de IP).
- **Edición directa en CouchDB**: cualquiera con credenciales de la base puede escribir el campo.
  La restricción "solo admin" es una garantía de las APIs, no de CouchDB.

---

## Rate Limit

```json
"rate_limit": {
  "max_requests": 50,
  "window_seconds": 60
}
```

| Campo | Rango | Default (cliente nuevo / clave ausente) | Descripción |
|---|---|---|---|
| `max_requests` | 1..=10,000 | 50 | Máximo de peticiones permitidas en la ventana |
| `window_seconds` | 1..=3,600 | 60 | Duración de la ventana deslizante en segundos |

**Comportamiento**:

- El rate limit de `/aq/` se aplica por `crypt_id` (no por IP ni por token).
- Se almacena en Valkey (contador atómico con TTL = `window_seconds`).
- Si Valkey no está disponible, el limiter local (in-process) actúa como fallback con el mismo
  límite, pero por proceso. El cliente recibe `X-Degraded-Mode: true` en la respuesta.
- Al exceder el límite, el proxy responde `429 Too Many Requests` con `Retry-After: <segundos>`.
- El bucket de los endpoints de preview es **independiente** (ver § Preview). Bajar
  `max_requests` no reinicia el contador ya existente: la ventana en curso sigue contando.

---

## Scripting (Sandbox Lua)

```json
"scripting": {
  "enabled": true,
  "code": "function handle(req, res) ... end",
  "code_hash": "sha256:abc123...",
  "expression": "http.response.status eq 200"
},
"max_scripting_body_bytes": 5242880
```

| Campo | Rango / Tipo | Default | Descripción |
|---|---|---|---|
| `scripting.enabled` | `bool` | `false` | Activa/desactiva la ejecución del script Lua para este cliente |
| `scripting.code` | `string` | `""` | Código fuente Lua. Se ejecuta en un sandbox mlua (Lua 5.4) con deadline estricto. Contrato: `function(body) ... return body_transformado end`. Referencia y recetas: `docs/LUA_SCRIPTING.md` |
| `scripting.expression` | `string` | `""` | Opcional, máx. 4.096 caracteres. Expresión del motor de reglas (la misma que `header_rules`): el script solo se ejecuta si la respuesta la cumple (p. ej. `http.response.status eq 200 and starts_with(http.response.content_type, "application/json")`). Vacía = siempre |
| `scripting.code_hash` | `sha256:<64 hex>` | `""` | Hash SHA-256 del código. Obligatorio cuando `code` no está vacío |
| `max_scripting_body_bytes` | `u64` | `5242880` (5 MiB) | Límite de bytes del body que el script puede leer/escribir. Rango aceptado por la API: 1,048,576..=52,428,800 (1–50 MiB) |

Notas:

- El `PUT` **fusiona campo a campo**: enviar solo `{ "scripting": { "enabled": false } }` no borra
  el código guardado; para borrarlo hay que enviar `"code": ""` (y no se exige `code_hash`).
- El `GET` de cliente devuelve solo `scripting.enabled` y `scripting.code_hash`; el código
  (`scripting_code`) solo aparece en `GET /api/v1/admin/clients/{id}`.
- `max_scripting_body_bytes` solo se consulta cuando el scripting está activo. El seed demo y la
  config degradada usan `0` internamente (no es un valor escribible por la API, que exige ≥ 1 MiB).

**Límites del sandbox** (variables de entorno globales, no por cliente):

| Variable | Default | Rango | Descripción |
|---|---|---|---|
| `LUA_TIMEOUT_MS` | 200 | 1..=60,000 | Deadline total de ejecución del script |
| `LUA_MEMORY_LIMIT_MB` | 50 | — | Límite de memoria del sandbox en MB |
| `WEBHOOK_TIMEOUT_MS` | 5,000 | 1..=60,000 | Timeout para `proxy.http_request` desde Lua |

**API del sandbox**: `proxy.log`, `proxy.regex_replace` (regex estilo Rust: grupos `$1`,
flags `(?i)`/`(?s)`), `proxy.json_parse`/`proxy.json_stringify` (JSON ⇄ tabla Lua) y
`proxy.http_request(url, method, body, timeout_ms)`. Recetas por tipo de contenido (JSON,
HTML, CSS, Markdown, imágenes) en `docs/LUA_SCRIPTING.md`.

**Webhooks desde Lua**: el script puede hacer peticiones HTTP externas vía
`proxy.http_request(url, method, body, timeout_ms)`. Estas peticiones pasan por el mismo pipeline
anti-SSRF que el proxy principal (whitelist del cliente —o `wildcard`—, resolución DNS segura,
rechazo de IPs privadas). Si el webhook excede el timeout, el script recibe un error y el body
original del upstream se degrada transparentemente.

---

## Manejo de Errores y Fallbacks

```json
"error_handling": {
  "mode": "wrapped",
  "fallback_urls": {
    "image/*": {
      "url": "https://cdn.example.com/error.png",
      "hash": "sha256:abc123..."
    },
    "text/html": {
      "url": "https://cdn.example.com/error.html",
      "hash": "sha256:def456..."
    }
  }
}
```

### Modos

| Modo | Comportamiento |
|---|---|
| `transparent` (default) | El proxy reenvía el error del upstream tal cual (status code, headers, body). No se usan fallbacks. |
| `wrapped` | Si el upstream falla y hay un fallback para el MIME de la respuesta, el proxy sirve el fallback en su lugar. |

### Fallback URLs

| Campo | Formato | Descripción |
|---|---|---|
| key (MIME pattern) | `type/subtype` o `type/*` | Patrón de MIME que activa este fallback |
| `url` | URL válida (http/https, sin credenciales, sin fragmentos, sin IPs privadas) | URL del recurso de fallback |
| `hash` | `sha256:<64 hex>` | Hash SHA-256 del contenido del fallback (integridad) |

**Validación de URLs de fallback**: mismas reglas anti-SSRF que la whitelist — solo http/https,
sin credenciales, sin fragmentos, sin IPs privadas en el host.

Notas:

- El bloque `error_handling` se **sustituye completo** en cada `PUT`; para conservar los fallbacks
  hay que reenviarlos junto al modo.
- Las copias servidas desde fallback se cachean y se pueden purgar con
  `POST /api/v1/admin/clients/{id}/purge-cache`.

---

## Reglas de Modificación de Headers (`header_rules`)

Cada cliente puede definir reglas que inyectan, modifican o eliminan headers de la respuesta,
con expresiones estilo [Cloudflare Ruleset Engine](https://developers.cloudflare.com/ruleset-engine/about/rules/).
Referencia completa del lenguaje (campos, operadores, funciones, límites y ejemplos) en
**`docs/HEADER_RULES.md`**.

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
| `expression` | Se evalúa por petición contra la request del cliente y la respuesta del upstream. Si es `true`, se aplican las operaciones. Máx. 4.096 caracteres. |
| `action` | `"set"` (única acción por ahora, igual que las reglas de headers de Cloudflare). |
| `action_parameters.headers` | Lista de operaciones (máx. 10 por regla): `name` + `operation` (`set`/`add`/`remove`) + `value` (obligatorio para `set`/`add`, admite placeholders `${campo}`). |

**Comportamiento**:

- Las reglas se aplican en orden sobre la respuesta servida (upstream en vivo, caché HIT y
  fallback). Cada regla ve las modificaciones de las anteriores.
- Los headers del proxy (`X-Cache`, `X-Proxy-By`, `X-Content-Type-Options`,
  `X-Degraded-Mode`, `X-Fallback-Source`) se fijan después y siempre ganan; el `PUT` rechaza
  operaciones sobre ellos y sobre los headers de framing (`content-length`,
  `transfer-encoding`, `connection`, `host`, ...).
- El `PUT` con `header_rules` sustituye la lista completa (como `whitelist`); `[]` las desactiva.
- El campo es opcional: los documentos de CouchDB anteriores a él se cargan con la lista vacía.
- Cotas: 50 reglas por cliente, 10 operaciones por regla, 4.096 caracteres por expresión,
  128 bytes por nombre de header, 4.096 bytes por valor. Fuera de cota → `400 invalid_config`.

**Campos disponibles en las expresiones**: `http.response.status`,
`http.response.content_type`, `http.response.headers["nombre"]`, `http.request.method`,
`http.request.headers["nombre"]`, `url.scheme`, `url.host`, `url.path`, `url.query`.
Operadores: `eq`/`ne` (o `==`/`!=`), `in { ... }`, `and`/`or`/`not`, paréntesis. Funciones:
`starts_with`, `ends_with`, `contains`, `matches` (regex), `lower`, `upper`, `len`, `concat`.

---

## Preview (`/mt/` y `/pv/`)

La sección `preview` habilita los endpoints públicos de preview del puerto del proxy:
`GET /mt/{crypt_id}?url=...` (metadatos enriquecidos, JSend) y `GET /pv/{crypt_id}?url=...`
(render de screenshot PNG o PDF vía el sidecar browserless). Cada endpoint tiene su interruptor:
`meta_enabled` para `/mt/` y `enabled` para `/pv/`. Ambos reutilizan la cadena anti-SSRF
de `/aq/` (validación estricta, whitelist del cliente, resolución DNS segura y pinning de IP) y
respetan la whitelist y el `wildcard` del cliente. Contrato completo en `docs/api_contract.yaml`;
variables del sidecar en `docs/ENVIRONMENT.md` § Preview.

```json
"preview": {
  "enabled": true,
  "meta_enabled": true,
  "capabilities": { "meta": true, "content": true, "screenshot": false, "pdf": false },
  "rate_limit": { "max_requests": 60, "window_seconds": 60 },
  "ttl_default_seconds": 86400,
  "ttl_max_seconds": 2678400,
  "max_render_timeout_ms": 45000,
  "max_body_bytes": 2097152,
  "screenshot": { "max_full_page": true, "default_viewport": [1920, 1080] }
}
```

| Campo | Tipo | Default | Descripción |
|---|---|---|---|
| `enabled` | `bool` | `false` | Interruptor de `/pv/` (render). **Fail-safe**: si el documento no trae la sección, nace deshabilitado y `/pv/` responde `403 preview_disabled`. No afecta a `/mt/`. |
| `meta_enabled` | `bool` | `true` | Interruptor de `/mt/` (metadatos). Separado de `enabled`: los metadatos son fetch + parseo barato y no dependen del sidecar, así que apagar el render no los apaga. |
| `capabilities.meta` | `bool` | `true` | Segunda comprobación de `/mt/`, junto a `meta_enabled`. |
| `capabilities.content` | `bool` | `true` | Permite la extracción de contenido principal (`content=1` en `/mt/`); solo aplica a respuestas HTML. |
| `capabilities.screenshot` | `bool` | `false` | Permite `/pv/?format=screenshot`. |
| `capabilities.pdf` | `bool` | `false` | Permite `/pv/?format=pdf`. |
| `rate_limit` | objeto | `60 / 60` | Límite propio de preview, independiente del de `/aq/`. Contador distribuido en Valkey por `crypt_id` + capacidad (`rlp:{crypt_id}:{capability}`). Cotas: `max_requests ≤ 10000`, `window_seconds ≤ 3600`. |
| `ttl_default_seconds` | `int` | `86400` | TTL de caché cuando la petición no pide `ttl`. |
| `ttl_max_seconds` | `int` | `2678400` | Techo del `ttl` pedido por query (31 días); debe ser ≥ `ttl_default_seconds`. |
| `max_render_timeout_ms` | `int` | `45000` | Techo del timeout de render por cliente; gana el menor frente a `RENDER_TIMEOUT_MS` y al `timeout` de la query. Cota: ≤ 120000. |
| `max_body_bytes` | `int` | `2097152` | Tope del cuerpo HTML a **parsear** en `/mt/`. Si el origen lo supera → `413 preview_payload_too_large`. Cota: ≤ 10485760. |
| `screenshot.max_full_page` | `bool` | `true` | Valor por omisión de `fullPage` cuando la query no lo fija. |
| `screenshot.default_viewport` | `[int; 2]` | `[1920, 1080]` | `[ancho, alto]` del viewport; cada lado en 1..=4096. |

**Comportamiento**:

- **Quién la edita**: la sección es **del operador**. Se fija con
  `PUT /api/v1/admin/clients/{id}` (sustituye la sección completa y la valida contra las cotas
  `MAX_PREVIEW_*` de `domain/validators.rs`). El `PUT /api/v1/clients/config` del cliente la
  valida pero **no la aplica**: un cliente no puede encenderse el render ni subirse los topes.
- **Sección parcial**: como `PreviewConfig` usa defaults por campo, un `PUT` admin con
  `{"preview": {"enabled": true}}` deja `meta_enabled: true`, las capacidades por defecto y los
  topes por defecto. Para conservar valores previos hay que reenviar la sección completa.
- Los documentos anteriores al campo se cargan con `PreviewConfig::default()` (`/pv/`
  deshabilitado, `/mt/` disponible si `capabilities.meta` está activa).
- `/pv/` es **fail-soft**: si el sidecar no está configurado (`BROWSERLESS_URL` vacío), no está
  sano o falla, responde `503 render_unavailable` con `Retry-After: 30`; si supera el deadline,
  `504 render_timeout`. Nunca devuelve 500 por ausencia del sidecar, y `/mt/` no depende de él.
- Las respuestas de `/mt/` se cachean en Valkey (`X-Cache: HIT/MISS`); `force=1` fuerza la
  reobtención y `ttl` acota la vida de la entrada contra `ttl_max_seconds`.
- `/mt/` sin `content=1` devuelve metadatos (título, descripción, OpenGraph, Twitter Cards,
  JSON-LD, favicons, media). Las imágenes servidas directas incluyen `width`/`height` sondeados de
  la cabecera (PNG, GIF, JPEG, WebP) sin decodificar el fichero.

### Ejemplo: habilitar `/mt/` (y opcionalmente `/pv/`) para un cliente

Vía API admin (recomendado; el cambio es efectivo en ≤ 5 s por el feed `_changes`):

```bash
curl -X PUT https://apiteleproxy.velone.ai/api/v1/admin/clients/pixelphone \
  -H "X-Admin-UI: 1" \
  -H "Cookie: __Host-teleproxy_admin=..." \
  -H "Content-Type: application/json" \
  -d '{"preview": {"enabled": true, "meta_enabled": true}}'
```

Vía edición directa del documento (CouchDB):

```bash
doc=$(curl -su "$COUCHDB_USER:$COUCHDB_PASSWORD" \
  "$COUCHDB_URL/tele_proxy_configs/b116a9228725658d0d14f328c70350c1")
echo "$doc" \
  | jq '.preview = ((.preview // {}) + {enabled: true, meta_enabled: true}) | .config_version += 1' \
  > cliente.json
curl -su "$COUCHDB_USER:$COUCHDB_PASSWORD" -X PUT \
  "$COUCHDB_URL/tele_proxy_configs/b116a9228725658d0d14f328c70350c1" \
  -H "Content-Type: application/json" -d @cliente.json
```

---

## Cómo editar la configuración

### 1. API admin (`/api/v1/admin/clients/*`)

Autenticada por cookie de sesión del panel o master token. Las mutaciones exigen el header
`X-Admin-UI: 1` cuando CORS está activo (defensa CSRF). Todas las mutaciones dejan auditoría.

| Método | Ruta | Descripción |
|---|---|---|
| `GET` | `/api/v1/admin/clients?limit=50&skip=0` | Lista paginada (`limit` máx. 200). Incluye `scripting_code_hash` pero no el código Lua ni `bearer_token_hash`. |
| `POST` | `/api/v1/admin/clients` | Crea el cliente con defaults (whitelist vacía, 50/60, sin scripting, preview por defecto). Body opcional `{"wildcard": true}`. Devuelve `{ "token": "...", "client": {...} }`; el token en claro se muestra **una sola vez**. |
| `GET` | `/api/v1/admin/clients/{internal_id}` | Detalle, incluido `scripting_code`. |
| `PUT` | `/api/v1/admin/clients/{internal_id}` | Aplica un `ClientConfigUpdate` parcial (mismas cotas que el `PUT` de cliente) **más** `wildcard`. Incrementa `config_version`. Devuelve `{ "config_version", "updated_at" }`. Es la vía para `preview`. |
| `DELETE` | `/api/v1/admin/clients/{internal_id}` | Borra el cliente. El feed invalida su caché. |
| `POST` | `/api/v1/admin/clients/{internal_id}/rotate-token` | Nuevo bearer token (`{ "token", "client" }`); invalida el anterior e incrementa `config_version`. |
| `POST` | `/api/v1/admin/clients/{internal_id}/rotate-id` | Nuevo `crypt_id` (`{ "new_crypt_id", "rotated_at" }`); las URLs públicas cambian e incrementa `config_version`. |
| `POST` | `/api/v1/admin/clients/{internal_id}/purge-cache` | Borra `px:{internal_id}:*` (respuestas de `/aq/` y copias stale del fallback) sin tocar `config_version`. Devuelve `{ "purged_keys", "pattern", "purged_at" }`. |

> **`preview` en la vista del panel**: `GET /api/v1/admin/clients/{id}` devuelve la sección
> `preview` completa (interruptores, capacidades, TTLs, topes y rate limit), igual que el
> documento almacenado. El código Lua solo aparece en el detalle individual, nunca en el listado.

### 2. API de cliente (`/api/v1/clients/*`)

Autenticada con `Authorization: Bearer <token>`. Solo permite tocar lo que es del cliente:

- `GET /api/v1/clients/config` → `ClientConfigResponse`: `crypt_id`, `config_version`,
  `whitelist`, `rate_limit`, `max_scripting_body_bytes`, `scripting` (`enabled` + `code_hash`),
  `error_handling`, `header_rules` y `preview` efectivo. Nunca expone `bearer_token_hash`,
  `internal_id` ni el código Lua.
- `PUT /api/v1/clients/config` → actualización parcial: `whitelist`, `rate_limit`,
  `max_scripting_body_bytes`, `scripting`, `error_handling`, `header_rules`. Incrementa
  `config_version`.
- `POST /api/v1/clients/rotate-id` → nuevo `crypt_id`.

Semántica de fusión del `PUT`:

| Campo | Semántica |
|---|---|
| `whitelist`, `header_rules`, `error_handling`, `preview` (admin) | Sustitución completa del valor enviado |
| `rate_limit` | Sustitución del objeto |
| `scripting` | Fusión campo a campo (solo los subcampos enviados cambian) |
| `preview` (cliente) | Se valida pero no se aplica: es del operador |

### 3. Edición directa en CouchDB

Útil para migraciones o para corregir documentos importados a mano. Reglas:

1. **Conserva `_id` y `_rev`**. Un `PUT` sin el `_rev` vigente devuelve `409 Conflict`.
2. **Mantén `type: "client_config"`**. Sin él, la vista `by_crypt_id` no indexa el documento y el
   cliente deja de encontrarse (`404 config_not_found`).
3. **No cambies `internal_id`** salvo que quieras una identidad nueva: es la clave de la caché de
   config y de las claves de respuesta. Si falta, al leer se adopta el `_id`; si `_id` e
   `internal_id` difieren, un borrado del documento no invalida la caché (el feed solo tiene el
   `_id` en los borrados) y el cliente se sigue sirviendo hasta que expire el TTL de config
   (300 s por defecto). Lo recomendable es `_id == internal_id`.
4. **Sube `config_version` en +1** si el cambio debe invalidar lo cacheado de `/aq/` y de preview.
   El feed `_changes` invalida la **config** igualmente (≤ 5 s), pero las respuestas ya cacheadas
   viven bajo la versión anterior; sin el incremento siguen sirviéndose hasta su TTL.
5. **Las claves ausentes toman su default**, pero un tipo o enum equivocado rompe el parseo: el
   log muestra `No se pudo parsear el documento de CouchDB '<id>': ...` y las peticiones de ese
   cliente responden `500` hasta corregirlo.
6. La edición directa **no valida** cotas ni `wildcard`: úsala con cuidado y replica el esquema de
   este documento.

Después de guardar, verifica (≤ 5 s) con el `GET` de cliente usando su token, o con
`GET /api/v1/admin/clients/{internal_id}` para el resto de campos.

### Propagación y cachés

| Caché | Clave | Cómo se invalida |
|---|---|---|
| Config (Moka, in-process) | `crypt_id` y `bearer_token_hash`; índice por `internal_id` | Feed `_changes` (≤ 5 s), TTL 300 s, o `PUT`/rotaciones que llaman al repositorio |
| Respuestas `/aq/` (Valkey) | `px:{internal_id}:{config_version}:{sha256(url)}` | Subir `config_version`, `purge-cache`, o expirar por TTL (`cache_ttl_images_secs` 3600, `cache_ttl_assets_secs` 1800, `cache_ttl_default_secs` 300; ajustes globales) |
| Preview `/mt/` (Valkey) | `pv:mt:{internal_id}:{config_version}:{sha256(url\|params)}` | Subir `config_version`, `force=1`, o TTL de la entrada |
| Rate limit | `rl:{crypt_id}` / `rlp:{crypt_id}:{capability}` | TTL de la ventana; no se reinicia al cambiar la config |

Con Valkey caído no hay caché de respuestas ni de preview: todo va al origen y se responde con
`X-Degraded-Mode: true`.

---

## Configuración del Cliente Demo (Seed)

Cuando la base de datos CouchDB está vacía (primer despliegue), el proxy crea automáticamente un
cliente demo con permisos mínimos para pruebas:

| Campo | Valor demo |
|---|---|
| `_id` / `internal_id` | `demo_client` |
| `crypt_id` | Nanoid de 12 caracteres (generado) |
| `bearer_token` | `demo-token` (el hash SHA-256 se calcula en runtime) |
| `config_version` | 1 |
| `kind` / `wildcard` | `client` / `false` |
| `whitelist` | `["example.com"]` |
| `rate_limit` | 5 requests / 60 segundos |
| `scripting` | Deshabilitado |
| `max_scripting_body_bytes` | 0 |
| `error_handling.mode` | `transparent` |
| `fallback_urls` | Vacío |
| `header_rules` | Vacío |
| `preview` | `enabled: true`, `meta_enabled: true`, capacidades `meta`+`content` activas (`screenshot`/`pdf` off), rate limit y topes por default |

**Propósito**: permitir pruebas inmediatas tras el despliegue sin configuración manual. Los permisos
son intencionalmente restrictivos: solo un dominio, rate limit bajo, sin scripting. El preview nace
habilitado en sus capacidades sin sidecar (`meta`/`content`) para que `/mt/` se pueda probar de
inmediato; `/pv/` exige además el sidecar browserless (ver `docs/ENVIRONMENT.md` § Preview).

> **Solo en `MODO=desarrollo`**: en cualquier otro modo el seed no se ejecuta (un cliente con
> token conocido en producción sería una puerta trasera). El log de seed solo imprime el
> `crypt_id` (público por diseño): el bearer token en claro jamás se registra.

**Uso**:
```bash
# Obtener la configuración del cliente demo
curl -H "Authorization: Bearer demo-token" http://localhost:8081/api/v1/clients/config

# Hacer una petición proxy (reemplazar <crypt_id> con el valor del log de seed)
curl "http://localhost:8080/aq/<crypt_id>/?url=https://example.com/image.jpg"
```

El `crypt_id` generado se imprime en los logs al arrancar:
```
{"level":"INFO","fields":{"message":"Cliente demo sembrado (whitelist: example.com, 5 req/60s); el bearer token no se registra","crypt_id":"V1StGXR8_Z5j",...}}
```

---

## Modo degradado (CouchDB inaccesible)

Si CouchDB no está disponible en el momento de una petición, el proxy devuelve una configuración
degradada con permisos ultra-restrictivos:

| Campo | Valor degraded |
|---|---|
| `internal_id` | `default_degraded` |
| `whitelist` | Vacío (todas las peticiones serán bloqueadas) |
| `rate_limit` | 3 requests / 60 segundos |
| `scripting` | Deshabilitado |
| `max_scripting_body_bytes` | 0 |
| `error_handling.mode` | `transparent` |
| `header_rules` | Vacío |
| `preview` | Deshabilitado (`PreviewConfig::default()`, `enabled: false`) → `/mt/` y `/pv/` responden `403 preview_disabled` |

Esto garantiza que un fallo de CouchDB no convierta el proxy en un open relay, sino que bloquee
todo el tráfico de forma segura. Los errores de red con CouchDB se reconocen como conexión; un
documento que existe pero no parsea **no** cae en degradado: es un `500` con detalle.

---

## Errores frecuentes de configuración

| Síntoma | Causa habitual | Solución |
|---|---|---|
| `403 preview_disabled` en `/mt/` | `preview.meta_enabled` o `preview.capabilities.meta` en `false`, o el documento no trae `preview` y el default aplica sobre un cliente que esperabas habilitado | Habilitar la sección por API admin o CouchDB (ver § Preview) |
| `403 preview_disabled` en `/pv/` | `preview.enabled` en `false` (fail-safe por omisión) | `PUT` admin con `{"preview": {"enabled": true}}` |
| `403` "el cliente es un administrador" | `kind: "admin"` en un documento usado como cliente | Corregir `kind` o usar el documento de cliente correcto |
| `404 config_not_found` | `type` distinto de `client_config`, `crypt_id` incorrecto o documento no indexado aún | Revisar `type`/`crypt_id`; esperar a que la vista se actualice |
| `500` "No se pudo parsear el documento de CouchDB" | Tipo o enum equivocado (p. ej. `whitelist` como string, `mode: "aggressive"`, `kind` desconocido) | Corregir el JSON; el default solo cubre claves **ausentes**, no valores inválidos |
| Cambios en CouchDB sin efecto inmediato | El feed tarda hasta 5 s; con Valkey, las respuestas cacheadas viven bajo la versión anterior | Esperar 5 s, subir `config_version` o llamar a `purge-cache` |
| `409 Conflict` al editar en CouchDB | `_rev` desactualizado | Releer el documento y reintentar con el `_rev` vigente |
| `429` con `Retry-After` | Rate limit de `/aq/` o del bucket de preview agotado | Revisar `rate_limit` y `preview.rate_limit` |
| `400 invalid_config` al hacer `PUT` | Valor fuera de cota o expresión/URL/hash inválidos; el mensaje cita el campo | Corregir el campo indicado |

---

## API de Control (cliente)

La configuración de **cliente** se gestiona vía la API de control (`HTTP_CONTROL_PORT`, default
8081, solo loopback):

| Método | Ruta | Descripción |
|---|---|---|
| `GET` | `/api/v1/clients/config` | Obtener la configuración actual (sin secretos) |
| `PUT` | `/api/v1/clients/config` | Actualización parcial (solo los campos enviados) |
| `POST` | `/api/v1/clients/rotate-id` | Rotar el `crypt_id` (genera uno nuevo) |

Todos los endpoints requieren `Authorization: Bearer <token>`. El token se hasha con SHA-256 y se
busca en CouchDB vía la view `by_token_hash`.

### Ejemplo de actualización parcial

```bash
curl -X PUT http://localhost:8081/api/v1/clients/config \
  -H "Authorization: Bearer mi-token-secreto" \
  -H "Content-Type: application/json" \
  -d '{
    "whitelist": ["shutterstock.com", "gettyimages.com"],
    "rate_limit": {
      "max_requests": 500,
      "window_seconds": 60
    }
  }'
```

Solo se actualizan los campos enviados; el resto se mantiene sin cambios. El `config_version` se
incrementa automáticamente.
