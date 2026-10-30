# PLAN_PREVIEW — API de preview de URLs para TeleProxy

Documento de diseño y **estado as-built** de las capacidades de preview del proxy. Nació como
plan inspirado en `microlink.io`, `link-preview-js`, `php-embed/Embed`, `metascraper`, `html-get`,
`browserless`, `oembed-spec`, `is-antibot`; esta versión se actualizó el **2026-10-30** para
reflejar lo efectivamente entregado y separar lo pendiente.

> Este documento describe el **diseño vigente**. Donde el plan original proponía algo distinto de
> lo implementado, se marca explícitamente `[DESCARTADO]` / `[REVISADO]` con el motivo, para que
> nadie implemente a ciegas una decisión ya descartada.

## Estado general (2026-10-30)

| Capacidad | Endpoint | Estado |
|---|---|---|
| Metadatos (title, description, author, date, lang, publisher, site_name) | `GET /mt/{crypt_id}` | **Implementado** |
| Media (image, logo, video, audio, favicons) | `GET /mt/{crypt_id}` | **Implementado** |
| OpenGraph / Twitter Cards / JSON-LD básico | `GET /mt/{crypt_id}` | **Implementado** |
| Contenido (texto principal + markdown) | `GET /mt/{crypt_id}?content=1` | **Implementado** |
| Detección antibot declarativa | `GET /mt/{crypt_id}` | **Implementado** |
| Screenshot PNG | `GET /pv/{crypt_id}` | **Cliente + artefactos** (fail-soft; verificación en Linux) |
| PDF | `GET /pv/{crypt_id}?format=pdf` | **Cliente + artefactos** (fail-soft; verificación en Linux) |
| oEmbed | — | **DESCARTADO en esta entrega** (Fase 2) |
| Registry por dominio | — | **Pendiente** |
| Panel admin (Nette) + cuotas/observabilidad | — | **Pendiente** |
| Paridad Pingora | — | **Pendiente** (sincroniza con la migración en curso) |

Gate verde sobre `default features` (Windows): `cargo fmt --check`, `cargo clippy --all-targets
-- -D warnings` y `cargo test` (236 lib + 2 bin). El sidecar de render no se puede ejercitar en
este puesto (sin motor Linux/Docker): se entrega cliente + compose y lo valida el operador.

---

## D1. Dos endpoints, no uno [REVISADO]

El plan original (`GET /pv/?url=...&meta=1&screenshot=1&...`) se separó en **dos endpoints con
contratos de respuesta distintos**, porque devuelven cosas distintas y cachean distinto:

- `GET /mt/{crypt_id}?url=<url>[&content=1][&ttl=<s>][&force=1]` → **JSON JSend** con `PreviewData`.
- `GET /pv/{crypt_id}?url=<url>[&format=screenshot|pdf][&fullPage=1][&timeout=<ms>]` → **binario**
  (PNG o PDF) servido tal cual.

Motivo: mezclar JSON de metadatos y bytes de imagen en un solo endpoint obliga a un sobre (y a un
endpoint de assets) para casos que no lo necesitan. Separados, el front pide metadatos por `/mt/` y
la imagen por `/pv/`; `/pv/` no necesita envoltorio ni almacenamiento intermedio.

Ambos aceptan la variante con y sin slash final (`/mt/{crypt_id}` y `/mt/{crypt_id}/`) porque el
`crypt_id` es un segmento fijo y un slash de más no debe dar 404.

`/aq/` sigue siendo el proxy transparente; `/mt/` y `/pv/` son compañeros de `/aq/`, nunca devuelven
el body crudo del origen.

---

## D2. Contrato de respuesta [REVISADO]

### `/mt/` — JSON JSend

```json
{
  "status": "success",
  "statusCode": 200,
  "data": {
    "url": "https://final.example/articulo",
    "title": "...", "description": "...", "author": "...", "date": "...",
    "lang": "es", "publisher": "...", "siteName": "...",
    "image":  { "url": "https://final.example/img.png", "type": "image/png", "width": 1200, "height": 630 },
    "logo":   { "url": "..." },
    "video":  { "url": "...", "type": "video/mp4" },
    "audio":  { "url": "...", "type": "audio/mpeg" },
    "favicons": [ { "url": "https://final.example/favicon.ico" } ],
    "openGraph": { "title": "...", "description": "...", "site_name": "...", "image": "..." },
    "twitter":   { "card": "summary_large_image", "title": "..." },
    "jsonLd": [ { "@type": "Article", "headline": "..." } ],
    "content": { "text": "...", "markdown": "..." },
    "antibot": { "detected": false }
  }
}
```

- Serialización `camelCase`; todo campo opcional se omite (`skip_serializing_if`), nunca viaja como
  `null` de relleno. `antibot` **siempre** está presente.
- `data.url` es la **URL final post-redirects** (las imágenes relativas se resuelven contra ella).
- `MediaInfo` es un objeto único reutilizable (`url, type, size, sizePretty, width, height,
  duration`); el MIME viaja como `type` (palabra reservada evitada en serde).
- `MetaMap` (`openGraph`/`twitter`) es un mapa plano con el prefijo retirado, para preservar claves
  abiertas (`og:image:width`, `article:section`, ...) sin un struct rígido.
- `jsonLd` son los objetos del `<script type="application/ld+json">` ya aplanados (los `@graph` se
  expanden).
- **No** se exponen `redirects`, `headers` de la respuesta, `palette`/`color` ni `screenshot` dentro
  del JSON: el binario va por `/pv/` [DESCARTADO el sobre de assets del plan original].

### `/pv/` — binario

- `200` con `Content-Type` real (`image/png` o `application/pdf`) y los bytes del render.
- Fail-soft: sidecar no configurado o caído → `503 render_unavailable` con `Retry-After`, **nunca
  un 500 interno**.

### Cabeceras

| Endpoint | `Content-Type` | `Cache-Control` | Otras |
|---|---|---|---|
| `/mt/` | `application/json; charset=utf-8` | `private, max-age=0, no-store` | `X-Cache: HIT\|MISS`, `X-Degraded-Mode: true` si degradado |
| `/pv/` | del render (`image/png` / `application/pdf`) | `private, max-age=0, no-store` | — |

---

## D3. Pipeline sin navegador primero, render después

1. `/mt/` hace **fetch validado** (mismo anti-SSRF que `/aq/`) + extracción pura en Rust.
2. La detección antibot se evalúa **sobre la respuesta** (declarativa, `is-antibot`) antes de
   decidir nada; si hay desafío, se devuelven igualmente los metadatos que se pudieron extraer con
   `antibot.detected=true` — un shell vacío sería peor que el diagnóstico honesto.
3. `/pv/` entrega al sidecar **después** de validar el destino (whitelist + anti-SSRF de IP), para
   que Chromium no pueda apuntar a redes internas.

No hay escalado automático `prerender=auto` en esta entrega: `/mt/` es siempre fetch-only y `/pv/`
es siempre render (vía sidecar). [PENDIENTE] la puerta de usabilidad html-get (fetch → render solo
si el render es estrictamente mejor).

---

## D4. Cascada de detectores por campo [IMPLEMENTADO]

Cada campo se resuelve de forma independiente y cortocircuitada; si una fuente no aporta, se cae a
la siguiente. Orden general **OpenGraph/Twitter → JSON-LD → `<meta>` genéricos → DOM → content-type**,
con las excepciones por campo que se detallan:

| Campo | Precedencia |
|---|---|
| `title` | `og:title` → `twitter:title` → JSON-LD `headline`/`name` → `<meta title>` → `<title>` |
| `description` | `og:description` → `twitter:description` → JSON-LD `description` → `<meta description>` |
| `siteName` | `og:site_name` → `<meta application-name>` |
| `author` | `<meta author/article:author/og:article:author>` → JSON-LD `author` (objeto → `name`) |
| `date` | `<meta article:published_time/date/pubdate/publishdate/og:updated_time>` → JSON-LD `datePublished`/`dateCreated` |
| `publisher` | `<meta publisher/article:publisher>` → JSON-LD `publisher` (objeto → `name`) |
| `lang` | atributo `lang` de `<html>` |
| `image` | `og:image` → `og:image:secure_url` → `twitter:image` → JSON-LD `image`/`thumbnailUrl` → `<meta image/thumbnail>` → primer `<img src>` |
| `logo` | `<link rel=logo>` → JSON-LD `logo` |
| `video` | `og:video` → `og:video:secure_url` → `twitter:player` |
| `audio` | `og:audio` → `og:audio:secure_url` |
| `favicons` | `<link rel=icon\|shortcut icon\|apple-touch-icon\|mask-icon>`; fallback `/favicon.ico` |

- Todas las URLs se resuelven a **absoluto** contra la URL final; las que no quedan `http(s)` se
  descartan y la cascada continúa.
- Un campo sin ninguna fuente válida se omite (`None`), nunca produce error de extracción.
- El **registry por dominio / overrides** (YouTube, X, etc.) del plan original **no** se implementó
  [PENDIENTE]: hoy todo pasa por el extractor genérico.

`content` (capacidad aparte, `content=1`): `readability` extrae el texto principal y `htmd` lo
convierte a Markdown; si la extracción sale vacía se omite `content`.

---

## D5. Caché, límites y modo degradado [REVISADO]

- **Clave** (`application/cache_key.rs`):
  `pv:{capability}:{internal_id}:{config_version}:{sha256(url|params)}`, con `params` canónico
  (hoy `content=0|1`). Se hashea `url|params` para que la clave no filtre la URL en Valkey.
- **Serialización**: JSON del `PreviewData` vía `CacheStore::{get_preview,set_preview}`. `postcard`
  queda para `/aq/`.
- **Se cachea `/mt/`** (`mt`); **`/pv/` no cachea el binario** en esta entrega: el render siempre
  va al sidecar [DESCARTADO el almacenamiento Valkey/volumen + URL firmada HMAC del plan original].
  Si más adelante el render necesita caché, se reutiliza `preview_cache_key("pv", ...)`.
- TTL default **24 h** (`86_400 s`), techo **31 días** (`2_678_400 s`); el `ttl` de la petición se
  acota al techo. `force=1` salta la lectura de caché.
- **Tope de cuerpo a parsear**: `preview.max_body_bytes` (default **2 MB**); `read_body_capped`
  corta y el error se traduce a `preview_payload_too_large` (413). Los binarios (imagen/video/audio
  servidos directos) no se parsean: se reportan como `MediaInfo` con URL y MIME.
- **Modo degradado**: CouchDB caído → config degradada (`ClientConfig::default_degraded`) → `/pv/`
  responde 403 `preview_disabled` (`enabled=false`) y `/mt/` sigue sirviendo si el proceso tiene
  config. Valkey caído → preview funciona sin caché (`X-Degraded-Mode: true`,
  `X-Cache` según corresponda); `set_preview` ya es no-op si Valkey no responde.
- **Rate limit**: bucket propio por capacidad `rlp:{crypt_id}:{capability}` (más estricto que `/aq/`;
  el render es caro). Default `60 req / 60 s` por capacidad.

---

## D6. Sección `preview` de `ClientConfig` [IMPLEMENTADO]

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

- `#[serde(default)]` a nivel de struct y campos: documentos antiguos de CouchDB parsean con
  `enabled=false` (fail-safe para `/pv/`) y `meta_enabled=true` (`/mt/` disponible).
- Interruptores separados: `meta_enabled` gobierna `/mt/` (metadatos) y `enabled` gobierna
  `/pv/` (render). Apagar el render no apaga los metadatos.
- Defaults por capacidad: `meta` y `content` **activas** (baratas, fetch + parseo); `screenshot` y
  `pdf` **apagadas** hasta que el operador las encienda.
- Cotas en `validators.rs` (`validate_preview`): `ttl_default ≤ ttl_max ≤ 31 d`,
  `max_render_timeout_ms ≤ 120 000`, `max_body_bytes ≤ 10 MB`, viewport en `1..=4096` por lado,
  `rate_limit.max_requests ≤ 10 000`, `window_seconds ≤ 3600`. Espejo en `ClientConfigResponse`,
  `ClientConfigUpdate` y `default_degraded`.

---

## D7. Sidecar de render: browserless en red aislada [REVISADO → opción B]

- Servicio `browserless` (imagen oficial `ghcr.io/browserless/chromium`) en `docker-compose.yml`,
  **sin publicar puerto**, unido **solo** a la red `red-preview`.
- Aislamiento duro: la red `red-preview` **no** está conectada a `red-velone`/`dokploy-network`/
  `default`, así Chromium no alcanza CouchDB/Valkey/servicios internos. El sidecar no se publica.
- **Opción B adoptada; opción A (reescritura de subrecursos hacia `/aq/`) NO implementada**
  [DESCARTADO]. El sidecar navega la URL destino directamente, pero TeleProxy la valida
  (whitelist + anti-SSRF de IP) **antes** de entregársela y la red aislada bloquea el acceso a lo
  interno. Limitación conocida y documentada en compose: un bridge de Docker sigue permitiendo
  egreso a Internet (no filtra la navegación a hosts públicos, solo protege la red interna).
- Resiliencia del cliente: `GET /pressure` como salud; `is_configured()==false` o sidecar caído →
  `render_unavailable` (503) con `Retry-After`. Tope de salida `max_output_bytes`
  (`PREVIEW_ASSET_MAX_BYTES`) contra un render desbocado.

---

## D8. Antibot declarativo [IMPLEMENTADO]

- Tabla `const` embebida (subconjunto de los 38 proveedores de `is-antibot`: los que se ven en la
  práctica), sin runtime JS. Señales por proveedor: `header` (+`header_value`), `cookie`, `status`
  característico y `html` (substrings del desafío).
- Técnicas: `javascript | captcha | waf | cookie`.
- `unless_open_graph`: si la respuesta trae OG completos (título + imagen o descripción), el
  proveedor marcado no la reporta como desafío (el contenido real está presente).
- Salida `AntibotInfo { detected, provider?, technique? }`, siempre presente en el `data`.

---

## D9. oEmbed [DESCARTADO en esta entrega]

La Fase 2 del plan original (discovery por `<link rel=alternate type=application/*+oembed>` +
lista estática de ~398 proveedores con índice por tokens) **no se implementó**. Queda como trabajo
futuro: si se retoma, el endpoint oEmbed debe pasar por el **mismo pipeline anti-SSRF** del cliente
(lo declara un tercero → SSRF de segundo grado real) y respetar `cache_age` como TTL máximo.
La variante de error `oembed_endpoint_blocked` del plan original tampoco existe todavía.

---

## D10. Lo que deliberadamente NO se copia

- `data`/MQL libre ni `function` (evaluación JS en servidor de terceros).
- `insights`/lighthouse, `search`, `inspector`: fuera de alcance.
- Proxy residencial/rotación de UA anti-bloqueo: TeleProxy es un proxy con whitelist; `is-antibot`
  se usa para **diagnosticar**, no para evadir.
- Dual free/pro ni API key pública: la identidad sigue siendo `crypt_id` + cuota por cliente.
- Límite de bytes infinito de `link-preview-js`.
- URL firmada HMAC + endpoint `/asset/{id}` para screenshots/PDF (se entregan los bytes directos).

---

## Estructura as-built (layer-first, hexagonal)

Contra el plan original (slice vertical `src/preview/`), se usa **layer-first** como el resto del
crate:

```
src/
├── domain/preview/
│   ├── config.rs       # PreviewConfig, PreviewCapabilities, ScreenshotConfig + cotas
│   ├── media.rs        # MediaInfo reutilizable + pretty_bytes
│   ├── result.rs       # PreviewData, PreviewResponse (JSend), MetaMap, ContentInfo
│   └── antibot.rs      # tabla is-antibot + detect()
├── application/preview/
│   ├── cascade.rs      # detectores por campo (D4)
│   ├── content.rs      # readability + htmd
│   ├── extract.rs      # build_preview: ContentKind → cascada → antibot
│   └── service.rs      # PreviewService: preamble (config + rate limit), caché, render
├── infrastructure/preview/
│   ├── fetch.rs        # anti-SSRF + redirects revalidados + tope de cuerpo
│   ├── parse_html.rs   # decode_html (charset real con encoding_rs)
│   └── renderer.rs     # cliente browserless (screenshot/pdf, /pressure, fail-soft)
├── infrastructure/valkey_cache.rs  # métodos `*_preview` del trait CacheStore (JSON + rate limit)
└── interfaces/preview_handler.rs   # handlers Axum /mt/ y /pv/
```

Puntos de conexión fuera del módulo: rutas en `main.rs`; `preview` en `ClientConfig`/response/
update/`default_degraded`; `validate_preview` en `validators.rs`; `preview_cache_key`/
`preview_rate_limit_key` en `application/cache_key.rs`; las 5 variantes de error en
`domain/errors.rs`; variables en `AppConfig::from_env`.

Dependencias añadidas (`Cargo.toml`), todas 100 % Rust puro (compilan en Windows sin sidecar):
`scraper 0.29`, `encoding_rs 0.8`, `readability 0.3`, `htmd 0.5`, `mime 0.3`.

---

## Errores de preview (diccionario)

| Variante | HTTP | Código | Disparador |
|---|---|---|---|
| `PreviewDisabled` | 403 | `preview_disabled` | `preview.meta_enabled=false` (`/mt/`), `preview.enabled=false` (`/pv/`) o capacidad concreta apagada |
| `InvalidPreviewParams` | 400 | `invalid_preview_params` | falta `url`, `format` desconocido, etc. |
| `PreviewPayloadTooLarge` | 413 | `preview_payload_too_large` | cuerpo del origen supera `max_body_bytes` |
| `RenderUnavailable` | 503 | `render_unavailable` | sidecar no configurado/caído |
| `RenderTimeout` | 504 | `render_timeout` | el render excede el deadline acotado |

Todos se registran una sola vez por petición en `ApiError::into_response` (span `preview` con
`crypt_id`); el handler **no** llama a `log_domain_error` para evitar doble evento.

---

## Variables de entorno (`docs/ENVIRONMENT.md`)

| Variable | Default | Rango | Uso |
|---|---|---|---|
| `BROWSERLESS_URL` | vacío | — | URL del sidecar; **vacío = render desactivado** (`/pv/` → 503) |
| `BROWSERLESS_TOKEN` | vacío | — | Token del sidecar (nunca por query) |
| `RENDER_TIMEOUT_MS` | `30000` | `1..=120000` | Deadline del render; la petición lo acota contra `preview.max_render_timeout_ms` |
| `PREVIEW_ASSET_MAX_BYTES` | `10485760` | `1..=52428800` | Tope del binario aceptado del sidecar |

---

## Fases — estado

- **Fase 0 (contrato/ADRs)**: hecho. `docs/api_contract.yaml` (paths + schemas), diccionario,
  `docs/CLIENT_CONFIG.md`, `docs/ENVIRONMENT.md` actualizados.
- **Fase 1 (núcleo puro)**: hecho (metadatos + media + OG/Twitter/JSON-LD).
- **Fase 2 (oEmbed + registry)**: **no** en esta entrega.
- **Fase 3 (content + antibot)**: hecho.
- **Fase 4 (sidecar render)**: **cliente + artefactos compose/env**, fail-soft; verificación de
  render real pendiente en Linux (sin Docker/Chromium en este puesto).
- **Fase 5 (panel admin + cuotas/observabilidad)**: pendiente.
- **Fase 6 (paridad Pingora)**: pendiente; el handler es delgado y la lógica vive en
  `application/preview/`, así que el enrutado se portará al `request_filter` en la migración.

---

## Riesgos latentes

| Riesgo | Mitigación |
|---|---|
| Reescritura/render frágil en SPAs | Opción B (red aislada) + validación previa del destino; opción A documentada como futura |
| `readability` en bodies grandes | Tope `max_body_bytes` (2 MB), solo `text/html`, extracción gated por `content` |
| Screenshots desbocados | `PREVIEW_ASSET_MAX_BYTES` acota el binario aceptado del sidecar |
| Sidecar caído tumba `/pv/` | Fail-soft → 503 `render_unavailable` con `Retry-After`, nunca 500 |
| Proveedores antibot desactualizados | Tabla `const` versionada en el binario; subconjunto curado |

## Fuentes

- `html-get`, `browserless`, `metascraper`, `link-preview-js`, `php-embed`, `oembed-spec`,
  `is-antibot`, `microlink/sdk` y `microlink.io/docs`. Dependencias suaves con
  `docs/PLAN_MICROLINK.md` (F-D8, F-B4, E-11, F-13, G-14).
