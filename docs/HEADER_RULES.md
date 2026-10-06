# Reglas de Modificación de Headers (`header_rules`)

Cada cliente de TeleProxy puede definir **reglas de respuesta** que inyectan, modifican o
eliminan headers HTTP, con una sintaxis inspirada en el [Ruleset Engine de
Cloudflare](https://developers.cloudflare.com/ruleset-engine/about/rules/) y su
[Rules language](https://developers.cloudflare.com/ruleset-engine/rules-language/expressions/).
Cada regla tiene dos partes:

- Una **expresión** (`expression`): se evalúa contra la petición del cliente y la respuesta del
  upstream. Si evalúa a `true`, la regla se aplica.
- Una o más **operaciones de header** (`action_parameters.headers`): qué hacer (crear,
  reemplazar, añadir o eliminar) y con qué valor.

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
  }
]
```

Las reglas se aplican **en orden**, sobre la respuesta que se sirve (tanto si viene del upstream
en vivo como de la caché o de un fallback). Entre regla y regla, cada expresión ve los headers
que las reglas anteriores ya modificaron — igual que las fases de Cloudflare se ejecutan en
orden. Los headers de seguridad que fija el proxy (`X-Cache`, `X-Proxy-By`,
`X-Content-Type-Options`, `X-Degraded-Mode`) se escriben después y siempre ganan.

---

## 1. Estructura de una expresión

El lenguaje admite dos tipos de expresiones, igual que Cloudflare:

### Expresiones simples

Comparan el valor de un **campo** con un **valor**, usando un operador de comparación:

```
<campo> <operador_de_comparación> <valor>
```

```js
http.response.status eq 200
http.request.headers["x-client"] eq "ios"
```

### Expresiones compuestas

Combinan dos o más expresiones con operadores lógicos (`and`, `or`, `not`) y paréntesis:

```
<expresión> <operador_lógico> <expresión>
```

```js
http.response.status eq 200 and starts_with(http.response.content_type, "application/json")
url.host eq "cdn.example.com" and not (http.response.status in {301 302})
```

**Límite de longitud**: una expresión puede tener como máximo **4.096 caracteres** (el mismo
límite que Cloudflare).

---

## 2. Campos disponibles

Un campo representa una propiedad de la petición del cliente (lo que este envió al proxy) o de
la respuesta del upstream (lo que el origen devolvió, antes de aplicar ninguna regla).

### Campos de la respuesta (`http.response`)

| Campo | Tipo | Descripción |
|---|---|---|
| `http.response.status` | Entero | Status code de la respuesta del upstream (p. ej. `200`). |
| `http.response.content_type` | Cadena | Valor completo del header `Content-Type` (p. ej. `application/json; charset=utf-8`). |
| `http.response.headers["<nombre>"]` | Cadena | Cualquier header de la respuesta del upstream. El nombre es case-insensitive. |

### Campos de la petición (`http.request`)

| Campo | Tipo | Descripción |
|---|---|---|
| `http.request.method` | Cadena | Método HTTP. En el endpoint público siempre es `GET`. |
| `http.request.headers["<nombre>"]` | Cadena | Cualquier header que el cliente envió al proxy (p. ej. `x-client`, `user-agent`). |

### Campos de la URL destino (`url`)

El campo `url` describe la URL del parámetro `?url=`, no la URL del proxy.

| Campo | Tipo | Descripción |
|---|---|---|
| `url.scheme` | Cadena | Esquema: `http` o `https`. |
| `url.host` | Cadena | Host, en minúsculas (p. ej. `cdn.shutterstock.com`). |
| `url.path` | Cadena | Path de la URL (p. ej. `/img/photo.jpg`). |
| `url.query` | Cadena | Query string sin el `?` (p. ej. `v=2&size=large`). Ausente si la URL no tiene query. |

### Campos ausentes

Si un header o la query no existen, el campo se considera **ausente**: cualquier comparación
contra él es `false` (nunca un error). Esto copia la semántica de Cloudflare/wirefilter:

```js
http.response.headers["x-missing"] eq "y"   // false, no error
contains(http.response.headers["x-missing"], "y")  // false, no error
```

---

## 3. Operadores de comparación

| Operador | Alias | Descripción |
|---|---|---|
| `eq` | `==` | Igualdad. Los tipos de ambos lados tienen que coincidir. |
| `ne` | `!=` | Desigualdad. Un campo ausente tampoco cumple `ne`. |
| `in { ... }` | — | Pertenencia a un conjunto. Los miembros van separados por espacios, sin comas. |

```js
http.response.status eq 200
http.response.status in {200 201 204}
http.request.headers["x-client"] in {"ios" "android" "web"}
```

Los tipos se comprueban al escribir la regla (`PUT /config` devuelve `400 invalid_config`), no
en la primera petición: `http.response.status eq "200"` o `http.response.status in {200 "x"}`
se rechazan de inmediato.

---

## 4. Operadores lógicos

| Operador | Descripción |
|---|---|
| `not <expresión>` | Niega la expresión. |
| `<expresión> and <expresión>` | `true` si ambas lo son. |
| `<expresión> or <expresión>` | `true` si alguna lo es. |
| `( <expresión> )` | Agrupa para controlar la precedencia. |

Precedencia (de mayor a menor): `not` → `and` → `or`. Con paréntesis se puede forzar cualquier
otro orden:

```js
http.response.status eq 200 and (starts_with(url.path, "/api/") or ends_with(url.path, ".json"))
```

---

## 5. Funciones

Las funciones manipulan valores dentro de la expresión. Todas las funciones booleanas
(`starts_with`, `ends_with`, `contains`, `matches`) devuelven `false` — en lugar de fallar —
cuando algún argumento es un campo ausente.

| Función | Devuelve | Descripción |
|---|---|---|
| `starts_with(cadena, prefijo)` | Booleano | `true` si la cadena empieza por el prefijo. |
| `ends_with(cadena, sufijo)` | Booleano | `true` si la cadena termina en el sufijo. |
| `contains(cadena, subcadena)` | Booleano | `true` si la cadena contiene la subcadena. |
| `matches(cadena, patron)` | Booleano | `true` si la cadena coincide con la expresión regular (sintaxis Rust/`regex`). El patrón se compila en la validación: un regex inválido se rechaza en el `PUT`. |
| `lower(cadena)` | Cadena | La cadena en minúsculas. |
| `upper(cadena)` | Cadena | La cadena en mayúsculas. |
| `len(cadena)` | Entero | Longitud de la cadena en caracteres. |
| `concat(cadena, ...)` | Cadena | Concatena dos o más cadenas. |

```js
starts_with(http.response.content_type, "image/")
ends_with(url.path, ".jpg") or ends_with(url.path, ".webp")
matches(url.path, "^/v[0-9]+/")
lower(url.host) eq "cdn.example.com"
len(http.request.headers["x-token"]) gt 0   // ⚠ `gt` no existe: usar not (len(...) eq 0)
```

> El motor de regex es el crate `regex` de Rust: **no backtrackea**, por lo que no hay riesgo de
> ReDoS en las expresiones de las reglas.

---

## 6. Valores y literales

| Tipo | Literal | Ejemplo |
|---|---|---|
| Cadena | Comillas dobles o simples: `"..."` o `'...'` | `"application/json"`, `'ios'` |
| Entero | Dígitos, sin signo | `200` |
| Booleano | `true`, `false` | `true` |

Las cadenas admiten escapes: `\"`, `\'`, `\\`, `\n`, `\t`.

### Valores dinámicos en los headers modificados

El `value` de una operación `set`/`add` puede interpolar campos con la sintaxis
`${<campo>}`, además de texto literal. Un campo ausente expande a cadena vacía:

```json
{ "name": "x-served", "value": "host=${url.host};status=${http.response.status}" }
{ "name": "x-client", "value": "${http.request.headers[\"x-client\"]}" }
```

Los placeholders se validan en el `PUT`: `${url.bogus}` o un `${` sin cerrar devuelven
`400 invalid_config`.

---

## 7. Operaciones de header

Cada entrada de `action_parameters.headers` tiene esta forma (igual que las reglas de
"Modify HTTP Response Headers" de Cloudflare):

| Campo | Valores | Descripción |
|---|---|---|
| `name` | string | Nombre del header (case-insensitive; se normaliza a minúsculas). |
| `operation` | `set` (default) / `add` / `remove` | `set`: reemplaza el valor (o crea el header, eliminando valores previos). `add`: añade un valor; el header puede quedar con varios (p. ej. varios `Set-Cookie`). `remove`: elimina el header. |
| `value` | string | Valor con placeholders `${...}` opcionales. Obligatorio para `set`/`add`; se ignora en `remove`. |

### Headers reservados del proxy

`PUT /config` rechaza con `400 invalid_config` cualquier regla que toque un header que el proxy
gestiona por su cuenta:

`content-length`, `content-encoding`, `transfer-encoding`, `connection`, `keep-alive`,
`upgrade`, `te`, `trailer`, `host`, `proxy-authenticate`, `proxy-authorization`, `x-cache`,
`x-proxy-by`, `x-degraded-mode`, `x-content-type-options`, `x-fallback-source`.

---

## 8. Límites

| Límite | Valor |
|---|---|
| Reglas por cliente | 50 |
| Operaciones de header por regla | 10 |
| Longitud de una expresión | 4.096 caracteres |
| Nombre de header | 128 bytes |
| Valor de header | 4.096 bytes |

Todos se validan en `PUT /api/v1/clients/config` antes de escribir en CouchDB: un documento
fuera de cota nunca llega a la base de datos, y `GET /api/v1/clients/config` nunca devuelve una
configuración que el propio contrato rechazaría. Enviar `header_rules: []` desactiva las
reglas (sustituye la lista completa, igual que `whitelist`).

---

## 9. Ejemplos de mundo real

Recetas listas para `PUT /api/v1/clients/config` con `header_rules`, agrupadas por el problema
que resuelven. Todas usan únicamente la sintaxis documentada en las secciones anteriores.

### 9.1 CORS: que los assets se puedan consumir desde el navegador

**Escenario**: sirves imágenes/CSS/JS a una web que corre en otro dominio. El navegador bloquea
la respuesta al hacer `fetch()` o al pintarla en un `<canvas>` (tainted canvas) si el origen no
devuelve `Access-Control-Allow-Origin`. Es el caso de uso más frecuente con este proxy.

```json
{
  "expression": "http.response.status eq 200 and (starts_with(http.response.content_type, \"image/\") or starts_with(http.response.content_type, \"font/\") or ends_with(url.path, \".css\") or ends_with(url.path, \".js\"))",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "access-control-allow-origin", "value": "https://mi-app.example.com", "operation": "set" },
      { "name": "access-control-allow-methods", "value": "GET", "operation": "set" },
      { "name": "timing-allow-origin", "value": "https://mi-app.example.com", "operation": "set" }
    ]
  }
}
```

Notas:
- Fíjate en el `or` entre condiciones; si prefieres abrirlo a cualquier consumidor, usa `*` en
  lugar del dominio (pierdes credenciales/cookies, que aquí no existen).
- `timing-allow-origin` habilita métricas de Resource Timing en el navegador del consumidor.

### 9.2 Cache-Control por tipo de recurso

**Escenario**: el upstream no manda `Cache-Control` (o manda uno malo) y el navegador/CDN de tu
app revalida todo. Imágenes versionadas por URL pueden cachearse días; HTML nunca.

```json
{
  "expression": "starts_with(http.response.content_type, \"image/\")",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "cache-control", "value": "public, max-age=86400, immutable", "operation": "set" }
    ]
  }
}
```

```json
{
  "expression": "starts_with(http.response.content_type, \"text/html\")",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "cache-control", "value": "no-store, must-revalidate", "operation": "set" }
    ]
  }
}
```

### 9.3 Cabeceras de seguridad en páginas HTML proxied

**Escenario**: embebes páginas HTML del upstream en tu producto. Les añades la protección que el
origen no tiene (clicjacking, MIME sniffing ya lo pone el proxy).

```json
{
  "expression": "starts_with(http.response.content_type, \"text/html\")",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "x-frame-options", "value": "SAMEORIGIN", "operation": "set" },
      { "name": "referrer-policy", "value": "strict-origin-when-cross-origin", "operation": "set" },
      { "name": "permissions-policy", "value": "camera=(), microphone=(), geolocation=()", "operation": "set" }
    ]
  }
}
```

### 9.4 Limpiar la huella del upstream y las cookies de los assets

**Escenario**: el origen anuncia su stack (`server: nginx/1.18`, `x-powered-by: PHP/8.1`) y
planta `Set-Cookie` hasta en las imágenes. Las cookies en recursos públicos rompen la caché
compartida (cada usuario cachea su propia copia) y son un vector de tracking. Se quitan en todo
lo que se sirve:

```json
{
  "expression": "true eq true",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "server", "operation": "remove" },
      { "name": "x-powered-by", "operation": "remove" },
      { "name": "x-aspnet-version", "operation": "remove" },
      { "name": "x-generator", "operation": "remove" }
    ]
  }
}
```

```json
{
  "expression": "starts_with(http.response.content_type, \"image/\") or ends_with(url.path, \".css\") or ends_with(url.path, \".js\")",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "set-cookie", "operation": "remove" }
    ]
  }
}
```

### 9.5 Corregir el Content-Type de un origen mal configurado

**Escenario**: un CDN sirve los SVG como `application/octet-stream` y el navegador los descarga
en vez de pintarlos en `<img>`. Se corrige mirando la extensión del path (el upstream mintió,
así que la respuesta no sirve):

```json
{
  "expression": "ends_with(lower(url.path), \".svg\") and starts_with(http.response.content_type, \"application/octet-stream\")",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "content-type", "value": "image/svg+xml", "operation": "set" }
    ]
  }
}
```

### 9.6 Trazabilidad: saber qué origen respondió y si hubo fallo

**Escenario**: tu app consume varios orígenes a través del mismo `crypt_id` y en soporte no
sabes a cuál peticionar. Dos reglas: traza siempre, y marca explícitamente los errores (en modo
`transparent`, donde el status del origen viaja tal cual al cliente):

```json
{
  "expression": "true eq true",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "x-served-from", "value": "${url.host}", "operation": "set" }
    ]
  }
}
```

```json
{
  "expression": "http.response.status in {500 502 503 504}",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "x-upstream-degraded", "value": "true;status=${http.response.status};url=${url.host}", "operation": "set" }
    ]
  }
}
```

### 9.7 Segmentar la respuesta según el dispositivo del cliente

**Escenario**: tu app móvil y tu web comparten el `crypt_id` y quieres métricas (o comportamiento
de caché) por plataforma. El valor del header de salida se interpola del header de entrada:

```json
{
  "expression": "http.request.headers[\"x-app\"] in {\"ios\" \"android\"}",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "x-platform", "value": "${http.request.headers[\"x-app\"]}", "operation": "set" },
      { "name": "vary", "value": "x-app", "operation": "set" }
    ]
  }
}
```

El `vary: x-app` evita que una caché intermedia sirva la respuesta de iOS a un cliente Android.

### 9.8 Forzar descarga en rutas concretas

**Escenario**: las facturas/PDF de `/documentos/` deben descargarse, no abrirse en pestaña:

```json
{
  "expression": "starts_with(url.path, \"/documentos/\") and http.response.status eq 200",
  "action": "set",
  "action_parameters": {
    "headers": [
      { "name": "content-disposition", "value": "attachment", "operation": "set" }
    ]
  }
}
```

### 9.9 Configuración completa de ejemplo

Un cliente real combina varias de las recetas anteriores. `PUT /api/v1/clients/config` con todo
el bloque (sustituye la lista completa de `header_rules`):

```json
{
  "header_rules": [
    {
      "expression": "true eq true",
      "action": "set",
      "action_parameters": {
        "headers": [
          { "name": "server", "operation": "remove" },
          { "name": "x-powered-by", "operation": "remove" },
          { "name": "x-served-from", "value": "${url.host}", "operation": "set" }
        ]
      }
    },
    {
      "expression": "http.response.status eq 200 and (starts_with(http.response.content_type, \"image/\") or starts_with(http.response.content_type, \"font/\"))",
      "action": "set",
      "action_parameters": {
        "headers": [
          { "name": "access-control-allow-origin", "value": "https://mi-app.example.com", "operation": "set" },
          { "name": "cache-control", "value": "public, max-age=86400, immutable", "operation": "set" },
          { "name": "set-cookie", "operation": "remove" }
        ]
      }
    },
    {
      "expression": "starts_with(http.response.content_type, \"text/html\")",
      "action": "set",
      "action_parameters": {
        "headers": [
          { "name": "cache-control", "value": "no-store, must-revalidate", "operation": "set" },
          { "name": "x-frame-options", "value": "SAMEORIGIN", "operation": "set" }
        ]
      }
    },
    {
      "expression": "ends_with(lower(url.path), \".svg\") and starts_with(http.response.content_type, \"application/octet-stream\")",
      "action": "set",
      "action_parameters": {
        "headers": [
          { "name": "content-type", "value": "image/svg+xml", "operation": "set" }
        ]
      }
    },
    {
      "expression": "http.response.status in {500 502 503 504}",
      "action": "set",
      "action_parameters": {
        "headers": [
          { "name": "x-upstream-degraded", "value": "true;status=${http.response.status}", "operation": "set" }
        ]
      }
    }
  ]
}
```

El orden importa: las reglas se evalúan en secuencia y cada una ve las modificaciones de las
anteriores — por eso la limpieza general va primero y las correcciones condicionales después.

---

## 10. Referencia de compatibilidad con Cloudflare

| Característica | Cloudflare | TeleProxy |
|---|---|---|
| Expresiones simples `campo eq valor` | ✅ | ✅ |
| Expresiones compuestas `and`/`or`/`not`/paréntesis | ✅ | ✅ |
| Conjuntos `in { ... }` | ✅ | ✅ |
| Acceso a headers `headers["nombre"]` | ✅ | ✅ |
| Funciones `starts_with`/`ends_with`/`contains`/`matches` | ✅ | ✅ |
| Funciones `lower`/`upper`/`len`/`concat` | ✅ | ✅ |
| Límite de expresión (4.096 caracteres) | ✅ | ✅ |
| Campos `cf.*` (colo, country, threat...) | ✅ | ❌ (no aplica: no hay red de Cloudflare) |
| Campos de la petición original del visitante (`http.request.uri.*`) | ✅ | ⚠️ Parcial: `url.*` describe la URL destino (`?url=`), no la URL del proxy. |
| Operadores `gt`/`ge`/`lt`/`le` | ✅ | ❗ No implementados: comparar con `in { ... }` o reformular con `not ... eq`. |
| Regex en `matches()` | RE2 | Sintaxis del crate `regex` de Rust (similar; sin look-around). |
