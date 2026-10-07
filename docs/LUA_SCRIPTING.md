# Scripting Lua: procesamiento de respuestas

Cada cliente puede transformar el **cuerpo** de las respuestas que sirve con un script Lua 5.4
sandboxed. Este documento es la referencia práctica: contrato del script, API disponible,
ejecución condicional por expresión (mismo motor que `header_rules`) y recetas de mundo real.
Para la config del campo `scripting` (límites, hash, sandbox) ver `docs/CLIENT_CONFIG.md`.

---

## 1. Contrato del script

El script debe definir una función que recibe **el cuerpo de la respuesta como único argumento
string** y **devuelve el cuerpo transformado**:

```lua
function(body)
  -- transformar body (string) y devolver el resultado (string)
  return body
end
```

No hay objeto `req`/`res`: el script solo ve el cuerpo. Las condiciones sobre status,
content-type, URL o headers se expresan en `scripting.expression` (sección 3), no dentro del
código. Si el script no devuelve un string, o falla, o supera el deadline, el proxy **sirve el
cuerpo original sin transformar** (degradación segura) y deja el motivo en el log.

---

## 2. API del sandbox

| Función | Descripción |
|---|---|
| `proxy.log(level, msg)` | Log estructurado (`info`, `warn`, `error`). |
| `proxy.regex_replace(texto, patron, reemplazo, limite?)` | Sustitución regex (límite anti-ReDoS de 100 ms). |
| `proxy.json_parse(cadena)` | JSON → tabla Lua. Object ⇄ tabla de claves string, array ⇄ tabla secuencial. |
| `proxy.json_stringify(valor)` | Tabla/valor Lua → JSON. Tabla con claves enteras ≥1 ⇄ array; resto ⇄ object. |
| `proxy.http_request(url, metodo?, body?, timeout_ms?)` | Webhook HTTP validado (misma whitelist + anti-SSRF que el proxy). Devuelve `{status, body}`. |

Desactivados: `os`, `io`, `package`, `debug`, `dofile`, `loadfile`, `load`. No hay `require`:
todo lo que el script usa viene de la tabla `proxy` o de la stdlib pura (`string`, `table`,
`math` básico…).

**Gotchas del JSON**:
- `null` JSON → `nil` Lua: **un `null` no sobrevive al round-trip** (queda ausente). Si el
  contrato downstream distingue `null` de ausente, no uses el round-trip sobre ese campo.
- Para quitar elementos de un array usa `table.remove(t, i)` (hacia atrás), no `t[i] = nil`:
  los huecos convierten la tabla en object al serializar.
- `json_parse` sobre un cuerpo inválido lanza error → el script falla → se sirve el original.
- Cuerpos mayores que `max_scripting_body_bytes` no entran a la VM: pasan sin transformar.

---

## 3. Ejecución condicional: `scripting.expression`

El script solo se ejecuta cuando la respuesta cumple una expresión del **mismo motor que
`header_rules`** (documentado en `docs/HEADER_RULES.md`): mismos campos
(`http.response.status`, `http.response.content_type`, `http.response.headers["x"]`,
`http.request.headers["x"]`, `url.host`, `url.path`, …), mismos operadores (`eq`/`ne`,
`in { }`, `and`/`or`/`not`) y mismas funciones (`starts_with`, `contains`, `matches`…).

- **Vacía o ausente** = el script corre siempre (compatibilidad con configs anteriores).
- Se valida en el `PUT /config` (`400 invalid_config` si no parsea, campo
  `scripting.expression`).
- La expresión evalúa **antes** de levantar la VM: si no cumple, no se paga el costo del
  sandbox.

```json
"scripting": {
  "enabled": true,
  "expression": "http.response.status eq 200 and starts_with(http.response.content_type, \"application/json\")",
  "code": "function(body) ... end",
  "code_hash": "sha256:..."
}
```

---

## 4. Recetas de mundo real

### Mapa de capacidades por tipo de contenido

Antes de las recetas, qué es viable con el sandbox actual:

| Contenido | ¿Procesable? | Cómo |
|---|---|---|
| JSON | ✅ Pleno | `proxy.json_parse` / `proxy.json_stringify` (nativo) |
| HTML | ✅ Sí | `proxy.regex_replace` (el cuerpo entra como string) |
| CSS | ✅ Sí | `proxy.regex_replace` |
| Markdown / TXT / CSV | ✅ Sí | `proxy.regex_replace` + stdlib de strings |
| Imágenes, PDF, video, audio | ❌ No | El cuerpo binario se degrada a UTF-8 *lossy* al entrar a la VM y el sandbox no tiene librerías de decode. Ver 4.10/4.11 para las alternativas (reescribir URLs hacia un resizer, o servicio externo por URL) |

### Notas de sintaxis regex

`proxy.regex_replace` usa el motor de regex de Rust: los grupos de captura se referencian
en el reemplazo como **`$1`**, `$2`, … (no `%1` como en Lua patterns), y los flags van inline:
`(?i)` case-insensitive, `(?s)` que `.` cruce saltos de línea, combinables `(?si)`.

Dos trampas del motor, ambas **sin error visible** (el script corre y devuelve un body
corrupto; el proxy degrada solo cuando el regex no compila):

- **`$1` seguido de texto alfanumérico se come el texto**: el crate parsea `$1https` como
  referencia a un grupo llamado `1https`; al no existir, lo sustituye por vacío en silencio.
  Usa llaves cuando el backreference toque letras o dígitos: **`${1}https://…`**.
- **No hay look-around**: `(?<=…)` y `(?=…)` no están soportados y el `Regex::new` falla
  (el script degrada al cuerpo original). Reformula con grupos de captura en su lugar.

### 4.1 Redactar claves internas de un JSON de API

**Escenario**: el origen devuelve campos que el frontend no debe ver (`internal_id`,
costes, tokens). Se quitan antes de que la respuesta se cachee y se sirva — un solo punto de
redacción para todos los consumidores.

```json
{
  "scripting": {
    "enabled": true,
    "expression": "http.response.status eq 200 and starts_with(http.response.content_type, \"application/json\")",
    "code": "function(body)\n  local data = proxy.json_parse(body)\n  data.internal_id = nil\n  data.costo_interno = nil\n  data.token = nil\n  return proxy.json_stringify(data)\nend",
    "code_hash": "sha256:<digest del code>"
  }
}
```

### 4.2 Renombrar claves (compatibilidad de API)

**Escenario**: el origen usa `camelCase` y tu frontend migró a `snake_case` (o al revés).
Renombras en el proxy sin tocar origen ni clientes:

```lua
function(body)
  local data = proxy.json_parse(body)
  data.user_name = data.userName
  data.userName = nil
  data.created_at = data.createdAt
  data.createdAt = nil
  return proxy.json_stringify(data)
end
```

### 4.3 Filtrar elementos de un array

**Escenario**: la lista del origen mezcla elementos activos e inactivos y el consumidor solo
debería ver los activos:

```lua
function(body)
  local data = proxy.json_parse(body)
  for i = #data.items, 1, -1 do
    if not data.items[i].activo then
      table.remove(data.items, i)
    end
  end
  return proxy.json_stringify(data)
end
```

### 4.4 Enriquecer un JSON con un webhook

**Escenario**: añades a cada respuesta un campo que vive en otro servicio interno. El webhook
sale por el **mismo pipeline anti-SSRF** del proxy (whitelist del cliente incluida) y si falla
o excede timeout, el script recibe error y puedes decidir degradar:

```lua
function(body)
  local data = proxy.json_parse(body)
  local ok, extra = pcall(function()
    return proxy.http_request("https://api-interna.ejemplo.com/enriquecer/" .. data.id, "GET", nil, 2000)
  end)
  if ok and extra.status == 200 then
    local info = proxy.json_parse(extra.body)
    data.score = info.score
  else
    proxy.log("warn", "enriquecimiento no disponible; se sirve sin score")
  end
  return proxy.json_stringify(data)
end
```

### 4.5 HTML: censar datos sensibles (correos, teléfonos)

**Escenario**: sirves páginas HTML que en algunos contextos muestran PII (perfiles, tickets,
confirmaciones). Las censuras en el proxy, antes de que salgan a la app:

```lua
function(body)
  body = proxy.regex_replace(body, "(?i)[a-z0-9._%+-]+@[a-z0-9.-]+\\.[a-z]{2,}", "[correo oculto]", 0)
  body = proxy.regex_replace(body, "\\+?\\d[\\d -]{7,}\\d", "[telefono oculto]", 0)
  return body
end
```

Con expresión: `starts_with(http.response.content_type, "text/html")`. Los límites son los de
todo regex sobre HTML: páginas generadas por JS en el cliente no pasan por aquí (solo el HTML
inicial), y correos partidos en varios nodos HTML pueden colarse — valida contra tus plantillas.

### 4.6 HTML: inyectar un script JS

**Escenario**: añades analytics o un widget a páginas proxied sin tocar el origen. Se inserta
antes de `</body>`; si no existe, se anexa al final:

```lua
function(body)
  local snippet = "<script src=\"https://mi-app.example.com/widget.js\" defer></script>"
  if body:find("</body>", 1, true) then
    body = proxy.regex_replace(body, "</body>", snippet .. "</body>", 1)
  else
    body = body .. snippet
  end
  return body
end
```

Ojo: el script inyectado se ejecuta con el origen de la página proxied; si necesita CORS o
cookies del dominio real, configúralo en consecuencia.

### 4.7 HTML: eliminar scripts del origen

**Escenario**: el origen embebe trackers o scripts de terceros que no quieres servir a tu
cliente. Se eliminan los bloques `<script>` (incluidos los inline):

```lua
function(body)
  body = proxy.regex_replace(body, "(?si)<script[^>]*>.*?</script>", "", 0)
  body = proxy.regex_replace(body, "(?i)<script[^>]*/>", "", 0)
  return body
end
```

`(?s)` es imprescindible: sin él `.` no cruza saltos de línea y los bloques multilínea
sobrevivirían. Advertencia: esto rompe las páginas cuyo JS es necesario para renderizar
(SPAs); úsalo cuando sirvas contenido estático o de lectura.

### 4.8 HTML: reescribir URLs

**Escenario**: el HTML del origen referencia `http://` y el navegador marca contenido mixto,
o quieres apuntar los assets a otro host:

```lua
function(body)
  body = proxy.regex_replace(body, "http://cdn\\.ejemplo\\.com/", "https://cdn.ejemplo.com/", 0)
  return body
end
```

### 4.9 CSS: editar estilos

**Escenario**: el CSS del origen trae colores de marca que no quieres mostrar, o un
`@import` a un tracker. Se edita como texto:

```lua
function(body)
  body = proxy.regex_replace(body, "#ff0000", "#0a2540", 0)
  body = proxy.regex_replace(body, "(?i)@import\\s+url\\(\"https?://tracker[^\"]*\"\\);?", "", 0)
  return body
end
```

Con expresión: `starts_with(http.response.content_type, "text/css")`. Para reescrituras
estructurales de CSS (añadir prefijos a selectores) el regex se queda corto: hazlo solo con
CSS de origen controlado.

### 4.10 Imágenes: redimensionar (alternativa real)

**Escenario**: sirves imágenes que el cliente consume con `width`/`height` fijos y quieres
ahorrar ancho de banda. **Dentro del sandbox no es viable**: el cuerpo binario se corrompe
al entrar a la VM (UTF-8 lossy) y no hay librería de imágenes. La solución real es no
procesar píxeles aquí, sino **reescribir las referencias** a las imágenes hacia un servicio de
resize (imgix, Cloudinary, images.weserv.nl propio…) y dejar que ese servicio sirva el
binario:

```lua
-- En una página HTML o JSON que referencia imágenes de tu CDN:
function(body)
  body = proxy.regex_replace(
    body,
    "https://cdn\\.ejemplo\\.com/([^\"')\\s]+\\.(?:jpg|jpeg|png|webp))",
    "https://images-weserv.ejemplo.com/?url=https://cdn.ejemplo.com/$1&w=400&fit=cover",
    0
  )
  return body
end
```

El resizer tiene que estar en la whitelist del cliente para que el navegador lo alcance, y
puedes protegerlo con una regla de `header_rules` (`starts_with(url.host, ...)` no aplica a
peticiones del navegador — protege el resizer por su cuenta, p. ej. firmando URLs).

### 4.11 Imágenes: procesar con un servicio externo

**Escenario**: OCR, moderación de contenido, generación de thumbnails firmados, etc. El
patrón es **pasar la URL de la imagen** (nunca los bytes: el bridge del sandbox es
string/UTF-8) a un servicio externo vía `proxy.http_request`, y usar su respuesta JSON para
modificar el cuerpo que sirves:

```lua
function(body)
  local data = proxy.json_parse(body)
  local res = proxy.http_request(
    "https://api-procesador.ejemplo.com/thumbnail?url=" .. data.image_url, "GET", nil, 5000
  )
  if res.status == 200 then
    local out = proxy.json_parse(res.body)
    data.thumbnail_url = out.url
    data.moderado = true
  else
    proxy.log("warn", "procesador no disponible")
  end
  return proxy.json_stringify(data)
end
```

Si lo que necesitas es transformar el **binario** de la imagen que el proxy sirve, la
arquitectura correcta es un servicio de procesado propio como origen (la whitelist y el
anti-SSRF de TeleProxy apuntan a él como a cualquier upstream), no el sandbox.

### 4.12 Markdown / TXT: editar contenido

**Escenario**: sirves documentación o exportaciones en markdown/texto y quieres normalizarlas
en el proxy: quitar front-matter, sustituir dominios viejos, censurar datos:

```lua
function(body)
  -- quitar front-matter YAML (multilínea: el flag (?s) es obligatorio)
  body = proxy.regex_replace(body, "(?s)\\A---\\n.*?\\n---\\n", "", 1)
  -- enlaces al dominio legacy al nuevo
  body = proxy.regex_replace(body, "https://docs-vieja\\.ejemplo\\.com", "https://docs.ejemplo.com", 0)
  -- censurar correos incluso en texto plano
  body = proxy.regex_replace(body, "(?i)[a-z0-9._%+-]+@[a-z0-9.-]+\\.[a-z]{2,}", "[oculto]", 0)
  return body
end
```

Con expresión: `starts_with(http.response.content_type, "text/") or ends_with(url.path, ".md")`.

### 4.13 Marcar respuestas de error (modo transparent)

**Escenario**: en modo `transparent` el status del origen viaja tal cual; quieres una marca
visible para tus logs de cliente cuando el origen falla (los 5xx pasan por aquí):

```json
{
  "scripting": {
    "enabled": true,
    "expression": "http.response.status in {500 502 503 504}",
    "code": "function(body)\n  proxy.log(\"warn\", \"origen caido, sirviendo body de error\")\n  return body\nend",
    "code_hash": "sha256:<digest del code>"
  }
}
```

---

## 5. Cuerpo completo de ejemplo (PUT /config)

```json
{
  "max_scripting_body_bytes": 1048576,
  "scripting": {
    "enabled": true,
    "expression": "http.response.status eq 200 and starts_with(http.response.content_type, \"application/json\")",
    "code": "function(body)\n  local data = proxy.json_parse(body)\n  data.internal_id = nil\n  data.user_name = data.userName\n  data.userName = nil\n  return proxy.json_stringify(data)\nend",
    "code_hash": "sha256:15c29a08734fb6adacb9220934e7ebd334f381317c0de717ab74cfd7d4465fb4"
  }
}
```

El hash se calcula sobre el string exacto del campo `code` (decodificado):

```bash
printf '%s' "$code" | sha256sum
```

Solo se actualizan los campos enviados; para quitar la ejecución condicional manda
`"expression": ""` (vuelve a "siempre"), y para desactivar el scripting `"enabled": false`.

---

## 6. Límites y comportamiento bajo presión

| Límite | Valor | Al superarlo |
|---|---|---|
| Deadline de ejecución (`LUA_TIMEOUT_MS`) | 200 ms | `script_timeout`; cuerpo original servido (WARN en log) |
| Memoria del sandbox (`LUA_MEMORY_LIMIT_MB`) | 50 MB | `script_memory_limit`; cuerpo original servido |
| Cuerpo máximo que entra a la VM | `max_scripting_body_bytes` (1–50 MB) | bypass: se sirve sin transformar |
| Webhook (`WEBHOOK_TIMEOUT_MS`) | 5 000 ms | el script recibe error (decide vía `pcall`) |
| Integridad | `code_hash` obligatorio con `code` | mismatch → cuerpo original servido (`integrity_check_failed` en log) |

Los errores del script **nunca rompen la respuesta al cliente**: siempre se degrada al cuerpo
original y la causa queda en el log estructurado. Las violaciones de seguridad del webhook
(`ssrf_blocked`, `domain_not_whitelisted`) sí abortan la petición con 403 — son *sticky* y no
las captura `pcall`.
