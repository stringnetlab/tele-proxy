# Configuración por Cliente

Cada cliente del proxy tiene un documento en CouchDB (`tele_proxy_configs`) que controla todos los
aspectos de su comportamiento: dominios permitidos, rate limiting, scripting Lua, manejo de errores
y fallbacks. Este documento describe cada campo, sus rangos válidos y cómo afectan al proxy.

---

## Identificadores

| Campo | Tipo | Descripción |
|---|---|---|
| `_id` | `string` | ID del documento CouchDB. Igual a `internal_id`. |
| `internal_id` | `string` | Identificador interno del cliente. Inmutable tras rotación de `crypt_id`. |
| `crypt_id` | `string` | Nanoid de 12 caracteres (`A-Za-z0-9_-`). Viaja en la URL pública: `/aq/{crypt_id}/`. Rotable vía `POST /api/v1/clients/rotate-id`. |
| `bearer_token_hash` | `string` | Hash SHA-256 del token Bearer del cliente. Formato: `sha256:<64 hex>`. Se usa para autenticar en la API de control. |
| `config_version` | `u64` | Versión de la configuración. Se incrementa automáticamente en cada `PUT /api/v1/clients/config` o rotación de `crypt_id`. |
| `type` | `string` | Siempre `"client_config"`. Usado por las views de CouchDB para indexar documentos. |

---

## Whitelist de Dominios

```json
"whitelist": ["shutterstock.com", "gettyimages.com", "8.8.8.8"]
```

| Restricción | Valor |
|---|---|
| Máximo de entradas | 100 |
| Formato | Hostname desnudo (sin esquema, puerto, path, credenciales ni fragmentos) |
| IPs privadas | Rechazadas (RFC 1918, loopback, link-local, CGNAT, etc.) |
| IPs públicas | Permitidas como entradas |
| Matching | Sufijo, case-insensitive: `cdn.shutterstock.com` matchea `shutterstock.com` |

**Validación**: cada entrada debe ser un hostname válido (máx 253 bytes, etiquetas de máx 63 bytes,
TLD alfabético) o una IP pública. Se rechazan: `localhost`, `shutterstock.com:8080`,
`https://shutterstock.com`, `user:pass@shutterstock.com`, IPs privadas (`127.0.0.1`, `10.x`, etc.).

---

## Rate Limit

```json
"rate_limit": {
  "max_requests": 50,
  "window_seconds": 60
}
```

| Campo | Rango | Default demo | Descripción |
|---|---|---|---|
| `max_requests` | 1..=10,000 | 5 | Máximo de peticiones permitidas en la ventana |
| `window_seconds` | 1..=3,600 | 60 | Duración de la ventana deslizante en segundos |

**Comportamiento**:
- El rate limit se aplica por `crypt_id` (no por IP ni por token).
- Se almacena en Valkey (contador atómico con TTL = `window_seconds`).
- Si Valkey no está disponible, el limiter local (in-process) actúa como fallback con el mismo
  límite, pero por proceso. El cliente recibe `X-Degraded-Mode: true` en la respuesta.
- Al exceder el límite, el proxy responde `429 Too Many Requests` con `Retry-After: <segundos>`.

---

## Scripting (Sandbox Lua)

```json
"scripting": {
  "enabled": true,
  "code": "function handle(req, res) ... end",
  "code_hash": "sha256:abc123..."
},
"max_scripting_body_bytes": 5242880
```

| Campo | Rango / Tipo | Descripción |
|---|---|---|
| `enabled` | `bool` | Activa/desactiva la ejecución del script Lua para este cliente |
| `code` | `string` | Código fuente Lua. Se ejecuta en un sandbox mlua (Lua 5.4) con deadline estricto. Contrato: `function(body) ... return body_transformado end` — recibe el cuerpo como string y devuelve el transformado. Referencia y recetas: `docs/LUA_SCRIPTING.md` |
| `expression` | `string` | Opcional, máx. 4.096 caracteres. Expresión del motor de reglas (la misma que `header_rules`): el script solo se ejecuta si la respuesta la cumple (p. ej. `http.response.status eq 200 and starts_with(http.response.content_type, "application/json")`). Vacía = siempre |
| `code_hash` | `sha256:<64 hex>` | Hash SHA-256 del código. Obligatorio cuando `code` no está vacío |
| `max_scripting_body_bytes` | 1,048,576..=52,428,800 | Límite de bytes del body que el script puede leer/escribir (1 MB – 50 MB) |

**Límites del sandbox** (variables de entorno globales):

| Variable | Default | Rango | Descripción |
|---|---|---|---|
| `LUA_TIMEOUT_MS` | 200 | 1..=60,000 | Deadline total de ejecución del script |
| `LUA_MEMORY_LIMIT_MB` | 50 | — | Límite de memoria del sandbox en MB |
| `WEBHOOK_TIMEOUT_MS` | 5,000 | 1..=60,000 | Timeout para `proxy.http_request` desde Lua |

**Webhooks desde Lua**: el script puede hacer peticiones HTTP externas vía `proxy.http_request(url, method, body, timeout_ms)`.
Estas peticiones pasan por el mismo pipeline anti-SSRF que el proxy principal (whitelist del cliente,
resolución DNS segura, rechazo de IPs privadas). Si el webhook excede el timeout, el script recibe
un error y el body original del upstream se degrada transparentemente.

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
| `transparent` | El proxy reenvía el error del upstream tal cual (status code, headers, body). No se usan fallbacks. |
| `wrapped` | Si el upstream falla y hay un fallback para el MIME de la respuesta, el proxy sirve el fallback en su lugar. |

### Fallback URLs

| Campo | Formato | Descripción |
|---|---|---|
| key (MIME pattern) | `type/subtype` o `type/*` | Patrón de MIME que activa este fallback |
| `url` | URL válida (http/https, sin credenciales, sin fragmentos, sin IPs privadas) | URL del recurso de fallback |
| `hash` | `sha256:<64 hex>` | Hash SHA-256 del contenido del fallback (integridad) |

**Validación de URLs de fallback**: mismas reglas anti-SSRF que la whitelist — solo http/https,
sin credenciales, sin fragmentos, sin IPs privadas en el host.

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
  `X-Degraded-Mode`) se fijan después y siempre ganan; `PUT /config` rechaza operaciones sobre
  ellos y sobre los headers de framing (`content-length`, `transfer-encoding`, `connection`,
  `host`, ...).
- `PUT` con `header_rules` sustituye la lista completa (como `whitelist`); `[]` las desactiva.
- El campo es opcional: los documentos de CouchDB anteriores a él se cargan con la lista vacía.
- Cotas: 50 reglas por cliente, 10 operaciones por regla, 4.096 caracteres por expresión,
  128 bytes por nombre de header, 4.096 bytes por valor. Fuera de cota → `400 invalid_config`.

**Campos disponibles en las expresiones**: `http.response.status`,
`http.response.content_type`, `http.response.headers["nombre"]`, `http.request.method`,
`http.request.headers["nombre"]`, `url.scheme`, `url.host`, `url.path`, `url.query`.
Operadores: `eq`/`ne` (o `==`/`!=`), `in { ... }`, `and`/`or`/`not`, paréntesis. Funciones:
`starts_with`, `ends_with`, `contains`, `matches` (regex), `lower`, `upper`, `len`, `concat`.

---

## Configuración del Cliente Demo (Seed)

Cuando la base de datos CouchDB está vacía (primer despliegue), el proxy crea automáticamente un
cliente demo con permisos mínimos para pruebas:

| Campo | Valor demo |
|---|---|
| `internal_id` | `demo_client` |
| `crypt_id` | Nanoid de 12 caracteres (generado) |
| `bearer_token` | `demo-token` (el hash SHA-256 se calcula en runtime) |
| `whitelist` | `["example.com"]` |
| `rate_limit` | 5 requests / 60 segundos |
| `scripting` | Deshabilitado |
| `max_scripting_body_bytes` | 0 |
| `error_handling.mode` | `transparent` |
| `fallback_urls` | Vacío |

**Propósito**: permitir pruebas inmediatas tras el despliegue sin configuración manual. Los permisos
son intencionalmente restrictivos: solo un dominio, rate limit bajo, sin scripting.

**Uso**:
```bash
# Obtener la configuración del cliente demo
curl -H "Authorization: Bearer demo-token" http://localhost:8081/api/v1/clients/config

# Hacer una petición proxy (reemplazar <crypt_id> con el valor del log de seed)
curl "http://localhost:8080/aq/<crypt_id>/?url=https://example.com/image.jpg"
```

El `crypt_id` generado se imprime en los logs al arrancar:
```
{"level":"INFO","fields":{"message":"Demo client seeded","crypt_id":"V1StGXR8_Z5j",...}}
```

---

## API de Control

La configuración se gestiona vía la API de control (`HTTP_CONTROL_PORT`, default 8081, solo loopback):

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

---

## Degraded Mode (CouchDB inaccesible)

Si CouchDB no está disponible en el momento de una petición, el proxy devuelve una configuración
degraded con permisos ultra-restrictivos:

| Campo | Valor degraded |
|---|---|
| `internal_id` | `default_degraded` |
| `whitelist` | Vacío (todas las peticiones serán bloqueadas) |
| `rate_limit` | 3 requests / 60 segundos |
| `scripting` | Deshabilitado |
| `error_handling.mode` | `transparent` |

Esto garantiza que un fallo de CouchDB no convierta el proxy en un open relay, sino que bloquee
todo el tráfico de forma segura.
