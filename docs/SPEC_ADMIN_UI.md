# SPEC — Panel de administración web (tele-proxy)

> Documento de especificación del panel `admin-teleproxy`. El backend (tele-proxy) ya implementa
> todo lo que consume esta UI: contrato canónico en `docs/api_contract.yaml` (sección
> `/api/v1/admin/*`), configuración del servicio en `docs/ENVIRONMENT.md`.
>
> **Cambio de arquitectura respecto a versiones anteriores de esta spec**: el panel ya no es una
> SPA que llama directo a la API. Es una aplicación **Nette server-rendered que actúa como BFF**.
> El navegador habla **solo** con el panel; el panel reenvía server-side al API de control.
> La autenticación es **exclusivamente con Google OAuth**. No existe login con master token en la
> interfaz.

## 1. Objetivo

Panel de administración de tele-proxy para gestionar clientes y administradores sin tocar CouchDB
a mano:

- Alta, edición, clonado, exportación/importación y borrado de clientes.
- Rotación de bearer token y de `crypt_id`; purga de caché por cliente.
- Configuración completa del cliente: whitelist, rate limit, scripting Lua, manejo de errores con
  fallbacks, reglas de headers y preview (`/mt/` y `/pv/`).
- Gestión de administradores (alta, activación/desactivación, borrado).
- Ajustes globales del proxy recargables en caliente (`RuntimeSettings`).

Fuera del panel: servir tráfico de proxy. El dominio público `teleproxy.velone.ai` no se toca.

## 2. Arquitectura de dominios

| Dominio                            | Servicio                          | Rol                                                    |
| ---------------------------------- | --------------------------------- | ------------------------------------------------------ |
| `https://teleproxy.velone.ai`      | tele-proxy (puerto proxy)         | Proxy público `/aq/{crypt_id}/`, `/mt/`, `/pv/`         |
| `https://apiteleproxy.velone.ai`   | tele-proxy (puerto de control)    | API de control y administración                        |
| `https://admteleproxy.velone.ai`   | **admin-teleproxy** (este panel)  | UI de administración (Nette + FrankenPHP/Caddy)        |

> Nombres canónicos tomados de `admin-teleproxy/AGENTS.md`. Documentación antigua usa
> `teleproxy-api.velone.ai` y `teleproxy-adm.velone.ai`; son alias del mismo par API/panel y hay
> que unificar en los canónicos al desplegar.

### Modelo BFF (obligatorio)

```
Navegador ──HTTPS──> Nette (admteleproxy) ──HTTPS server-side──> API (apiteleproxy) ──> CouchDB/Valkey
   │                        │
   │ cookie __Host-teleproxy_admin_ui (sesión Nette)
   └── nunca recibe la cookie teleproxy_admin del API
```

Reglas de oro:

1. El navegador **nunca** llama a `apiteleproxy.velone.ai`; no hay CORS de navegador.
2. La cookie de sesión del API (`teleproxy_admin`) vive **solo en el servidor Nette**, asociada a
   la sesión del operador. Jamás se expone en HTML, JS, URL, logs ni cookies del navegador.
3. **No hay autenticación alternativa**: la única vía de entrada es Google OAuth.
4. Ninguna mutación viaja por GET.
5. Los mensajes de error del API se muestran tal cual (escapados); no se inventan mensajes
   genéricos cuando el servidor ya explica el problema.

## 3. Stack obligatorio

- **PHP 8.3+** con **Nette 3.2+** (`nette/application`, `nette/forms`, `nette/routing`,
  `nette/di`), **Latte 3**.
- **FrankenPHP en modo clásico** (sin worker) + **Caddy** en el mismo contenedor.
- **Guzzle 7** para el cliente HTTP server-side; **Predis 2** para Valkey.
- **CouchDB 3** para datos y **Valkey 7** para single-session y rate limiting.
- Frontend: Latte + HTML/CSS/JS propios (sin frameworks de tabla, notificaciones ni navegación
  asíncrona), servidos desde el propio dominio (`www/assets/`), incluido `netteForms` para la
  validación cliente. Prohibido cargar CDNs externos: rompe la CSP.
- Arquitectura **hexagonal + vertical slicing** (`ClientManagement`, `AdminManagement`,
  `Authentication`, `Shared`) con puertos/adaptadores; los handlers de un solo método usan
  `__invoke`.
- PHPStan nivel 8 sin errores; PHP-CS-Fixer; tests con Nette Tester.

## 4. Variables de entorno

### 4.1 Panel (`admin-teleproxy`)

| Variable         | Obligatoria | Descripción                                                                 |
| ---------------- | ----------- | --------------------------------------------------------------------------- |
| `PUBLIC_API_URL` | Sí          | Origen del API de control, sin slash final: `https://apiteleproxy.velone.ai` |
| `COUCHDB_URL`    | Sí          | URL de CouchDB (datos y, si se opta, sesiones del panel)                     |
| `VALKEY_URL`     | Sí          | URL de Valkey en formato Redis (single-session y rate limiting)              |
| `SESSION_SECRET` | Sí          | Secreto de firma de la sesión Nette (≥ 32 bytes aleatorios)                  |
| `APP_ENV`        | Sí          | `production` en despliegue; `development` en local                           |
| `CADDY_DOMAIN`   | Sí          | `admteleproxy.velone.ai` (TLS automático de Caddy)                           |

Configuración adicional exigida:

- **IP real del cliente**: el rate limit de login del panel debe contar por IP del operador. Al
  estar detrás de Cloudflare/Dokploy, leer la IP de los headers del proxy **solo si** la petición
  viene de una IP de proxy de confianza (`TRUSTED_PROXIES` o equivalente). Nunca confiar en
  `X-Forwarded-For` de un origen arbitrario.
- **Duración de sesión del panel**: 15 minutos de inactividad (configuración de sesión Nette),
  independiente del TTL del API.
- **Sin master token en el panel**: no existe variable para inyectarlo ni ruta que lo acepte.

### 4.2 API de control (prerequisitos, ya implementados)

| Variable                        | Valor para este despliegue                                          |
| ------------------------------- | ------------------------------------------------------------------- |
| `GOOGLE_CLIENT_ID`              | Client ID OAuth de Google                                           |
| `GOOGLE_CLIENT_SECRET`          | Client Secret OAuth de Google                                       |
| `GOOGLE_REDIRECT_URI`           | `https://admteleproxy.velone.ai/login/google/callback`              |
| `ADMIN_UI_URL`                  | `https://admteleproxy.velone.ai`                                    |
| `ADMIN_ALLOWED_DOMAINS`         | Dominios de email autorizados (p. ej. `gmail.com,stringnet.pe`)     |
| `ADMIN_SESSION_TTL_SECONDS`     | `3600` (default)                                                    |
| `ADMIN_OAUTH_STATE_TTL_SECONDS` | `300` (default)                                                     |
| `ADMIN_LOGIN_RATE_LIMIT`        | `5/60` (default)                                                    |
| `ADMIN_RATE_LIMIT`              | `120/60` (default)                                                  |
| `CORS_ALLOWED_ORIGINS`          | **Vacío**: en modo BFF no hay llamadas de navegador al API          |

Puntos críticos:

- **`GOOGLE_REDIRECT_URI` apunta al callback de Nette**, no al del API. El API genera la URL de
  Google con esa URI y canjea el `code` con la misma; Google redirige al panel, que reenvía
  `code`+`state` al API server-side. La misma URI exacta debe estar registrada en Google Console.
- `ADMIN_UI_URL` solo se usa como destino del 302 del callback del API; Nette **no sigue** ese
  redirect (ver §5.2).
- El rate limit del API se aplica por IP de origen y en modo BFF la IP es la del panel: es un tope
  **global** para todos los operadores. El panel debe aplicar su propio rate limit por IP real.

## 5. Autenticación (solo Google)

### 5.1 Requisitos de identidad

La validación de identidad ocurre **íntegramente en el API**:

1. `state` OAuth de un solo uso y TTL corto (generado por el API).
2. Canje del `code` contra Google.
3. Verificación del `id_token` (RS256 vía JWKS; `iss`, `aud`, `exp`, `email_verified`).
4. Dominio del email en `ADMIN_ALLOWED_DOMAINS`.
5. Admin existente y `active=true` en CouchDB.

El panel no valida tokens ni emails por su cuenta: tras el callback confirma la identidad con
`GET /api/v1/admin/me` y usa el `email`/`role` que devuelve el API.

### 5.2 Flujo completo

1. El operador abre `https://admteleproxy.velone.ai/`. Sin sesión → `/login`.
2. Pulsa **“Entrar con Google”** → el navegador va a `GET /login/google` del panel.
3. Nette llama server-side a `GET /api/v1/admin/auth/google/url` y obtiene `{ url }`; responde 302
   del navegador a esa `url` de Google.
4. Google autentica y redirige a
   `GET https://admteleproxy.velone.ai/login/google/callback?code=...&state=...`.
5. Nette reenvía server-side a
   `GET {PUBLIC_API_URL}/api/v1/admin/auth/google/callback?code=...&state=...` con
   `allow_redirects=false` y `http_errors=false`.
6. El API valida, crea la sesión y responde **302** con
   `Set-Cookie: teleproxy_admin=<id>; ...`. Nette:
   - exige status 302 y presencia de `teleproxy_admin`; si no, va al paso 9;
   - extrae **solo el valor** de la cookie (hasta el primer `;`) y lo guarda en la sesión
     server-side;
   - guarda `expires_at` a partir de `Max-Age` si está presente.
7. Nette llama `GET /api/v1/admin/me` con esa cookie. Debe devolver
   `{ email, role: "admin", via: "session" }`.
8. Nette registra la sesión única en Valkey (`admin_ui_session:{email}` → id de sesión Nette;
   TTL alineado con el del API), **regenera el id de sesión** (anti-fixation) y redirige a
   `/clients`.
9. Cualquier fallo → página de error del panel (§5.5) y sesión limpia. Nunca se muestra el HTML
   crudo del API.

### 5.3 Sesión del panel

- Cookie de navegador: `__Host-teleproxy_admin_ui` — `Secure`, `HttpOnly`, `SameSite=Lax`,
  `Path=/`, sin `Domain`. La cookie del API **no** se replica en el navegador.
- Almacén server-side: sesión Nette (filesystem del contenedor o CouchDB). Contiene únicamente:
  email, rol, valor de la cookie del API y `expires_at`. Nunca se serializa a JS.
- Toda petición autenticada del panel usa la cookie del API guardada; si el API responde
  **401**, se destruye la sesión Nette, se borra la entrada de Valkey y se redirige a `/login`.
- **Single session**: al completar un login se invalida la sesión anterior del mismo email
  (Valkey + destrucción de la sesión Nette previa si es localizable). Un operador, una sesión.
- Inactividad: 15 minutos → `/login`.
- `GET /api/v1/admin/me` se consulta al arrancar y ante cualquier 401 para decidir el estado.

### 5.4 Logout

`POST /logout` (solo POST, con CSRF de Nette):

1. `POST /api/v1/admin/auth/logout` con la cookie del API.
2. Destruir sesión Nette y borrar `admin_ui_session:{email}` de Valkey.
3. Redirigir a `/login`.

El botón “Cerrar sesión” está siempre en la cabecera y muestra el email del operador.

### 5.5 Errores de login (los muestra el panel)

| Origen                            | Mensaje al operador                                                        |
| --------------------------------- | -------------------------------------------------------------------------- |
| API 400 (state inválido/expirado) | “La sesión de login expiró. Inténtalo de nuevo.”                         |
| API 401 (Google rechazó)          | “Google no pudo verificar la cuenta. Inténtalo de nuevo.”                  |
| API 403 (email no autorizado)     | “Esta cuenta de Google no está autorizada como administrador. Pide a un administrador que te añada.” |
| API 429                           | “Demasiados intentos de login. Espera unos minutos.” + `Retry-After` si existe |
| API 503                           | “El login con Google no está configurado en el servidor.”                   |
| Red/timeout/5xx                   | “No se pudo contactar con el servicio de administración.”                   |

Nunca se interpola HTML del API sin escapar; si se muestra el `message`, se escapa con Latte.

### 5.6 Bootstrap del primer administrador

No hay master token en la UI. El primer admin se crea **fuera de banda** contra el API:

```bash
curl -X POST https://apiteleproxy.velone.ai/api/v1/admin/admins \
  -H "Authorization: Bearer $TELEPROXY_MASTER" \
  -H "X-Admin-UI: 1" \
  -H "Content-Type: application/json" \
  -d '{"email":"operador@gmail.com"}'
```

Después, ese email entra por Google como cualquier otro. El master token sigue existiendo como
acceso de emergencia **del API**, pero el panel no lo acepta, no lo pide y no lo guarda.

## 6. Cliente del API (`ApiClient`, server-side)

Un único adaptador en `src/Shared/Infrastructure/ApiClient.php`:

```php
final class ApiClient
{
    public function withSessionCookie(string $cookieValue): self;
    public function get(string $path, array $query = []): ResponseInterface;
    public function post(string $path, array $body = []): ResponseInterface;
    public function put(string $path, array $body = []): ResponseInterface;
    public function patch(string $path, array $body = []): ResponseInterface;
    public function delete(string $path): ResponseInterface;
}
```

Contrato:

- Base URI: `PUBLIC_API_URL` (sin slash final). Timeout total 30 s, connect 5 s.
- Cabeceras en **todas** las peticiones: `X-Admin-UI: 1`, `Accept: application/json`; con body,
  `Content-Type: application/json`.
- Sesión: `Cookie: teleproxy_admin=<valor>` desde la sesión Nette. No se acepta master token.
- `allow_redirects=false` **obligatorio** en el callback de Google.
- Errores normalizados: el API devuelve
  `{ "error": string, "message": string, "details"?: ... }`. Lanzar `ApiError`
  (`status`, `code`, `message`, `details`) y mapear en la UI:
  - 401 → cerrar sesión local y `/login`.
  - 403/404/409/429 → mostrar `message` del API en el contexto de la acción.
  - 400 `invalid_config` → mostrar `message`; si `details` identifica el campo, resaltarlo.
  - 5xx → mensaje genérico + log server-side con el cuerpo.
- **Nunca** registrar en logs la cookie, el master token ni cuerpos con `scripting_code`.
- El callback de Google no usa `ApiClient::withSessionCookie` (aún no hay cookie): se invoca sin
  cookie y con redirecciones desactivadas.

## 7. Rutas y pantallas

| Ruta                          | Método | Pantalla / acción                                 |
| ----------------------------- | ------ | ------------------------------------------------- |
| `/`                           | GET    | Redirige a `/clients`                             |
| `/login`                      | GET    | Login (solo botón Google)                         |
| `/login/google`               | GET    | Inicia OAuth (302 a Google)                       |
| `/login/google/callback`      | GET    | Recibe `code`+`state`, completa la sesión         |
| `/logout`                     | POST   | Cierra sesión local y del API                     |
| `/clients`                    | GET    | Lista paginada de clientes                        |
| `/clients/create`             | POST   | Crea cliente; muestra token una sola vez          |
| `/clients/export`             | GET    | Descarga JSON de configuración (sin tokens)       |
| `/clients/import`             | POST   | Importa JSON (crea y configura clientes)          |
| `/clients/{id}`               | GET    | Detalle del cliente                               |
| `/clients/{id}/edit`          | POST   | Guarda configuración (`PUT`)                      |
| `/clients/{id}/clone`         | POST   | Clona configuración en un cliente nuevo           |
| `/clients/{id}/rotate-token`  | POST   | Rota bearer token                                 |
| `/clients/{id}/rotate-id`     | POST   | Rota `crypt_id`                                   |
| `/clients/{id}/purge-cache`   | POST   | Purga la caché del cliente                        |
| `/clients/{id}/delete`        | POST   | Elimina cliente                                   |
| `/admins`                     | GET    | Lista de administradores                          |
| `/admins/create`              | POST   | Añade administrador                               |
| `/admins/{email}`             | GET    | Detalle del administrador                         |
| `/admins/{email}/toggle`      | POST   | Activa/desactiva                                  |
| `/admins/{email}/delete`      | POST   | Elimina administrador                             |
| `/settings`                   | GET    | Ajustes globales                                  |
| `/settings`                   | POST   | Guarda patch de ajustes                           |
| `/_ok`                        | GET    | Healthcheck (200)                                 |

Reglas transversales:

- **Todas las mutaciones son POST** con token CSRF de Nette. No hay borrado/rotación/logout por
  GET.
- Confirmación previa con diálogo nativo (`confirm`) para borrar, rotar, clonar, importar y purgar.
- `id` de cliente en rutas = **`internal_id`**. El `crypt_id` es solo un dato mostrado/rotado.
- Layout común: cabecera con email del admin, nav (Clientes / Administradores / Ajustes) y botón
  de logout.

### 7.1 Login (`/login`)

- Botón único “Entrar con Google”. Sin inputs de token ni formularios alternativos.
- Si ya hay sesión válida, redirige a `/clients`.
- Muestra los errores de §5.5.

### 7.2 Clientes (`/clients`)

- `GET /api/v1/admin/clients?limit=50&skip=N` → `{ clients: AdminClientView[], total }`
  (máximo `limit=200`).
- Tabla: `crypt_id`, `kind`, `wildcard` (badge), resumen de `whitelist` (3 primeros + “+N”),
  `rate_limit` (`max_requests/window_seconds`), `config_version`, estado de preview
  (`/pv/` on/off, `/mt/` on/off).
- Paginación por `skip/limit` con total y botones anterior/siguiente.
- Botones: “Nuevo cliente”, “Exportar”, “Importar”.
- La lista **no** incluye `scripting_code` (solo el detalle).

### 7.3 Crear cliente

- Formulario mínimo: checkbox “salida libre (wildcard)” + botón crear.
- `POST /api/v1/admin/clients` con `{ "wildcard": bool }`.
- Respuesta `{ token, client }`: mostrar el token **una sola vez** en un bloque destacado con:
  - botón “Copiar” (Clipboard API con fallback a selección manual);
  - aviso explícito de que no se volverá a mostrar;
  - bloqueo del resto de acciones hasta confirmar que se copió/guardó.
- El token nunca se guarda en localStorage, sesión, logs ni URL.

### 7.4 Detalle / edición (`/clients/{id}`)

`GET /api/v1/admin/clients/{id}` devuelve la vista plana con `scripting_code`. Secciones del
formulario (guardar → `PUT /clients/{id}` con la configuración completa):

1. **Identidad (solo lectura)**: `internal_id`, `crypt_id`, `kind`, `config_version`.
2. **Salida**: `wildcard` (checkbox, solo admin). Aviso: salta la whitelist; el anti-SSRF de IP
   sigue aplicando.
3. **Whitelist**: textarea, una entrada por línea. Formato `host` o `host:port`; sin comodines;
   dominios o IP públicas; máximo **100** entradas.
4. **Rate limit**: `max_requests` 1..10000, `window_seconds` 1..3600.
5. **Scripting Lua**:
   - `scripting.enabled` (checkbox);
   - `scripting.code` (textarea; resaltado = fase 2);
   - `scripting.expression` (opcional; vacío = siempre).
   - Al guardar con código, el panel calcula `SHA-256` **server-side** y envía
     `scripting.code_hash = "sha256:<hex>"` (el API lo exige si hay `code`).
6. **Manejo de errores**:
   - `mode`: `transparent` | `wrapped`;
   - `fallback_urls`: editor JSON `{ "<mime>": { "url": "...", "hash": "sha256:..." } }`.
     Validar JSON y presencia de `url`+`hash` antes de enviar.
7. **Reglas de headers (`header_rules`)**: editor JSON avanzado (fase 1). Máximo 50 reglas;
   cada una `{ expression, action: "set", action_parameters: { headers: [...] } }`, máximo 10
   headers por regla, `expression` ≤ 4096 bytes.
8. **Preview** (misma forma en lectura y escritura):
   - `enabled` (`/pv/`), `meta_enabled` (`/mt/`);
   - `capabilities`: `meta`, `content`, `screenshot`, `pdf`;
   - `rate_limit`: `max_requests`, `window_seconds`;
   - `ttl_default_seconds`, `ttl_max_seconds` (≥ default, ≤ 2678400);
   - `max_render_timeout_ms` (≤ 120000), `max_body_bytes` (≤ 10485760);
   - `screenshot.max_full_page`, `screenshot.default_viewport` `[ancho, alto]`
     (cada lado 1..4096).
   - Aviso: `enabled` es fail-safe (si falta la sección nace `false`); `meta_enabled` nace `true`.

Comportamiento del guardado:

- Se envía el formulario completo (todos los campos editables) en un único `PUT`.
- El `PUT` devuelve `{ config_version, updated_at }`; mostrar la versión nueva.
- **No** se envía `config_version` (lo gestiona el API) ni `kind`.
- Errores 400: mostrar `message`; si viene `details`, marcar el campo (`whitelist`, `rate_limit`,
  `scripting.code_hash`, `header_rules[0].expression`, `preview.*`, etc.).

Payload de referencia:

```json
{
  "wildcard": false,
  "whitelist": ["example.com", "cdn.example.com:8443"],
  "rate_limit": { "max_requests": 1000, "window_seconds": 60 },
  "max_scripting_body_bytes": 5242880,
  "scripting": {
    "enabled": true,
    "code": "function transform(ctx) ... end",
    "code_hash": "sha256:<hex>",
    "expression": ""
  },
  "error_handling": {
    "mode": "wrapped",
    "fallback_urls": {
      "image/jpeg": { "url": "https://cdn.example.com/fallback.jpg", "hash": "sha256:<hex>" }
    }
  },
  "header_rules": [],
  "preview": {
    "enabled": false,
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

### 7.5 Acciones del detalle

| Acción           | Endpoint del API                      | UI                                                              |
| ---------------- | ------------------------------------- | --------------------------------------------------------------- |
| Rotar token      | `POST /clients/{id}/rotate-token`     | Confirmar; mostrar token nuevo una sola vez con copiar; bloquear hasta confirmar. |
| Rotar `crypt_id` | `POST /clients/{id}/rotate-id`        | Confirmar; mostrar `new_crypt_id` y `rotated_at`; avisar de que las URLs `/aq/{crypt_id}/` cambian. |
| Purgar caché     | `POST /clients/{id}/purge-cache`      | Confirmar; mostrar `purged_keys`, `pattern` y `purged_at`. No sube `config_version`. |
| Clonar           | `POST /clients` + `PUT /clients/{id}` | Confirmar; copiar toda la configuración; token nuevo una sola vez. Si el `PUT` falla, borrar el cliente creado (compensación) y reportar. |
| Eliminar         | `DELETE /clients/{id}`                | Doble confirmación (diálogo + confirmación escrita del `crypt_id`); volver a `/clients`. |

Todas las acciones comprueban 401/403/404/429 y muestran el `message` del API.

### 7.6 Exportar / importar

No existen endpoints de export/import en el API: el panel los compone con las rutas existentes.

**Exportar** (`GET /clients/export`):

- Recorre `GET /clients?limit=200&skip=...` y `GET /clients/{internal_id}` para cada cliente.
- Genera `clients-export.json`:

```json
{
  "format": "teleproxy-clients/v1",
  "exported_at": "2026-11-02T12:00:00Z",
  "clients": [
    {
      "crypt_id": "abc123def456",
      "kind": "client",
      "wildcard": false,
      "whitelist": ["example.com"],
      "rate_limit": { "max_requests": 1000, "window_seconds": 60 },
      "max_scripting_body_bytes": 5242880,
      "scripting": { "enabled": false, "code": "", "expression": "" },
      "error_handling": { "mode": "transparent", "fallback_urls": {} },
      "header_rules": [],
      "preview": { "...": "..." }
    }
  ]
}
```

- **Sin tokens** (el API nunca devuelve el token ni su hash), sin `internal_id`, sin
  `config_version` ni timestamps internos.

**Importar** (`POST /clients/import`):

- Acepta el mismo sobre (`format`, `clients`), máximo 100 clientes y 10 MB.
- Valida el sobre antes de escribir; si el JSON está malformado → 400 sin efectos.
- Por cada cliente: `POST /clients` con `{ "wildcard": ... }` y después
  `PUT /clients/{internal_id}` con la configuración.
- Los tokens nuevos se muestran **una sola vez** al final, junto con un resumen por cliente
  (creado / fallido + `message`). No se puede recuperar un token perdido: reimportar no lo
  restaura.
- Si un `PUT` falla, borrar el cliente recién creado y marcar la entrada como fallida.

### 7.7 Administradores (`/admins`)

- `GET /api/v1/admin/admins` → `{ "admins": AdminUser[] }` (ojo: objeto con clave `admins`, no un
  array raíz).
- Tabla: `email`, `role`, `active`, `created_at`, `created_by`.
- Añadir: input email → `POST /admins` `{ "email": "..." }`. El dominio debe estar en
  `ADMIN_ALLOWED_DOMAINS`; mostrar el `message` del API en 400.
- Activar/desactivar: `PATCH /admins/{email}` `{ "active": bool }`. El API rechaza la
  autodesactivación (403); mostrar el mensaje.
- Eliminar: `DELETE /admins/{email}`. El API rechaza autoborrado y borrar el último admin activo
  (403); mostrar el mensaje.
- El panel **no** crea ni usa master tokens; esta pantalla es la vía normal para dar acceso.

### 7.8 Ajustes globales (`/settings`)

- `GET /api/v1/admin/settings` → `RuntimeSettings` efectivos (campos ausentes resueltos a su
  default).
- Formulario con valor actual y rango visible por campo. Guardar envía **solo los campos
  cambiados** (`PATCH`); un patch vacío es 400 y no se envía.
- `PATCH /settings` responde con los `RuntimeSettings` efectivos; refrescar el formulario con esa
  respuesta.
- Errores: 400 fuera de rango (cita el campo), 409 conflicto de `_rev` → recargar y reintentar,
  429 cuota; siempre con el `message` del servidor.
- `verbose_errors`: interruptor destacado con aviso explícito (“expone el motivo interno en los
  errores de `/aq/` y de las APIs”); el API emite un WARN al activarlo.

| Campo                    | Tipo    | Rango / default                                          |
| ------------------------ | ------- | -------------------------------------------------------- |
| `upstream_user_agent`    | string  | ≤ 256; default `""`                                      |
| `upstream_timeout_secs`  | int     | 1..300; default 30                                       |
| `max_response_bytes`     | int     | 1 MiB..512 MiB; default 104857600                        |
| `cacheable_size_bytes`   | int     | 1 KiB..512 MiB; default 5242880; ≤ `max_response_bytes`  |
| `stale_cache_ttl_secs`   | int     | 60..604800; default 86400                                |
| `cache_ttl_images_secs`  | int     | 1..604800; default 3600                                  |
| `cache_ttl_assets_secs`  | int     | 1..604800; default 1800                                  |
| `cache_ttl_default_secs` | int     | 1..604800; default 300                                   |
| `lua_timeout_ms`         | int     | 1..60000; default 200                                    |
| `lua_memory_limit_mb`    | int     | 1..1024; default 50                                      |
| `webhook_timeout_ms`     | int     | 1..60000; default 5000                                   |
| `fallback_timeout_ms`    | int     | 1..60000; default 5000                                   |
| `verbose_errors`         | boolean | default `false`                                          |

### 7.9 Healthcheck

`GET /_ok` → 200 sin autenticación, para el healthcheck del contenedor. No consulta dependencias.

## 8. Contratos de datos

### 8.1 Endpoints del API usados

| Endpoint                                          | Uso                                       |
| ------------------------------------------------- | ----------------------------------------- |
| `GET /api/v1/admin/auth/google/url`               | Iniciar login                             |
| `GET /api/v1/admin/auth/google/callback`          | Completar login (server-side, sin seguir redirects) |
| `POST /api/v1/admin/auth/logout`                  | Logout                                    |
| `GET /api/v1/admin/me`                            | Identidad y validación de sesión          |
| `GET /api/v1/admin/admins`                        | Lista (`{ admins: [...] }`)               |
| `POST /api/v1/admin/admins`                       | Alta                                      |
| `PATCH /api/v1/admin/admins/{email}`              | Activar/desactivar                        |
| `DELETE /api/v1/admin/admins/{email}`             | Baja                                      |
| `GET /api/v1/admin/clients`                       | Lista paginada                            |
| `POST /api/v1/admin/clients`                      | Alta (token de un solo uso)               |
| `GET /api/v1/admin/clients/{id}`                  | Detalle (incluye `scripting_code`)        |
| `PUT /api/v1/admin/clients/{id}`                  | Configuración + `wildcard`                |
| `DELETE /api/v1/admin/clients/{id}`               | Baja                                      |
| `POST /api/v1/admin/clients/{id}/rotate-token`    | Rotar token                               |
| `POST /api/v1/admin/clients/{id}/rotate-id`       | Rotar `crypt_id`                          |
| `POST /api/v1/admin/clients/{id}/purge-cache`     | Purgar caché                              |
| `GET /api/v1/admin/settings`                      | Ajustes efectivos                         |
| `PATCH /api/v1/admin/settings`                    | Patch de ajustes                          |

### 8.2 Cliente: vista de lectura (`AdminClientView`)

Campos **planos** (no anidados) en la respuesta:

```ts
interface AdminClientView {
  internal_id: string;          // id de ruta/API
  crypt_id: string;
  kind: "client" | "admin";
  wildcard: boolean;
  config_version: number;
  whitelist: string[];
  rate_limit: { max_requests: number; window_seconds: number };
  max_scripting_body_bytes: number;
  scripting_enabled: boolean;
  scripting_code_hash: string;
  scripting_expression: string;
  scripting_code?: string;      // solo en GET individual
  error_handling: {
    mode: "transparent" | "wrapped";
    fallback_urls: Record<string, { url: string; hash: string }>;
  };
  header_rules: HeaderRule[];
  preview: PreviewConfig;
}
```

No existen `name`, `domain`, timestamps de cliente ni `bearer_token_hash` en la respuesta.

### 8.3 Cliente: escritura (`PUT /clients/{id}`)

```ts
interface AdminClientUpdate {
  wildcard?: boolean;                       // solo admin
  whitelist?: string[];
  rate_limit?: { max_requests: number; window_seconds: number };
  max_scripting_body_bytes?: number;
  scripting?: {
    enabled?: boolean;
    code?: string;
    code_hash?: string;                     // sha256:<hex>, obligatorio si hay code
    expression?: string;
  };
  error_handling?: {
    mode?: "transparent" | "wrapped";
    fallback_urls?: Record<string, { url: string; hash: string }>;
  };
  header_rules?: HeaderRule[];
  preview?: PreviewConfig;
}
```

### 8.4 `PreviewConfig`

```ts
interface PreviewConfig {
  enabled: boolean;                         // /pv/; fail-safe false si la sección falta
  meta_enabled: boolean;                    // /mt/
  capabilities: { meta: boolean; content: boolean; screenshot: boolean; pdf: boolean };
  rate_limit: { max_requests: number; window_seconds: number };
  ttl_default_seconds: number;
  ttl_max_seconds: number;                  // ≥ ttl_default_seconds
  max_render_timeout_ms: number;
  max_body_bytes: number;
  screenshot: { max_full_page: boolean; default_viewport: [number, number] };
}
```

### 8.5 `HeaderRule`

```ts
interface HeaderRule {
  expression: string;                       // ≤ 4096 bytes
  action: "set";
  action_parameters: {
    headers: Array<{
      name: string;                         // ≤ 128; rechaza headers gestionados por el proxy
      operation?: "set" | "add" | "remove";
      value?: string;                       // ≤ 4096; obligatorio en set/add
    }>;                                     // máximo 10
  };
}
```

### 8.6 Administrador

```ts
interface AdminUser {
  email: string;
  role: string;
  active: boolean;
  created_at: string;
  created_by: string;
}
```

### 8.7 Ajustes

`RuntimeSettings` y `RuntimeSettingsPatch` = los campos de la tabla §7.8; el patch es
`Partial<RuntimeSettings>` y un objeto vacío es inválido.

### 8.8 Error

```ts
interface ErrorResponse {
  error: string;
  message: string;
  details?: unknown;
}
```

## 9. Validación y manejo de errores

- El API es la autoridad de validación; el panel valida solo lo necesario para dar feedback
  inmediato (rangos numéricos, JSON bien formado, whitelist con formato `host[:port]`).
- Límites obligatorios que el panel debe reflejar en la UI:

| Campo                                 | Límite                                                     |
| ------------------------------------- | ---------------------------------------------------------- |
| `whitelist`                           | 100 entradas; `host` o `host:port`; sin comodines; IP pública |
| `rate_limit`                          | 1..10000 / 1..3600                                         |
| `max_scripting_body_bytes`            | 1048576..52428800                                          |
| `header_rules`                        | 50 reglas; 10 headers/regla; expresión ≤ 4096; valor ≤ 4096 |
| `preview.ttl_max_seconds`             | ≥ `ttl_default_seconds`; ≤ 2678400                         |
| `preview.max_render_timeout_ms`       | 1..120000                                                  |
| `preview.max_body_bytes`              | 1..10485760                                                |
| `preview.screenshot.default_viewport` | cada lado 1..4096                                          |
| `fallback_urls`                       | cada entrada requiere `url` y `hash` (`sha256:<hex>`)      |

- Mapa de errores HTTP: 400 → mensaje + campo; 401 → `/login`; 403/404 → mensaje del API; 409 →
  recargar y reintentar; 429 → mensaje + espera; 5xx → mensaje genérico con log server-side.
- No se muestran trazas, cuerpos crudos ni detalles internos salvo que `verbose_errors` esté
  activo en el API (y aun así se escapan).
- Toda mutación fallida deja el formulario con los datos del usuario y el error visible; nunca se
  recarga en blanco.

## 10. Seguridad

- **BFF**: el navegador solo conoce `admteleproxy.velone.ai`; la API y su cookie son
  server-side.
- **Sesión**: cookie `__Host-` (Secure, HttpOnly, SameSite=Lax), regeneración de id al login,
  expiración por inactividad, single-session en Valkey.
- **CSRF**: formularios Nette con token; mutaciones solo POST; `SameSite=Lax` como segunda capa.
- **CSP** (Caddy): `default-src 'self'; base-uri 'none'; form-action 'self';
  frame-ancestors 'none'; img-src 'self' data:; style-src 'self'; script-src 'self';
  object-src 'none'`. Sin CDNs, sin `unsafe-inline` ni `unsafe-eval`.
- **Cabeceras**: `Strict-Transport-Security`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`.
- **Secretos**: `SESSION_SECRET` y la cookie del API nunca se registran ni se renderizan; la
  cookie se guarda cifrada (AES-256-GCM) en la sesión del panel. El master token operativo no
  entra al panel.
- **Rate limiting**: el login se limita por IP real del operador en Valkey; los límites de las
  mutaciones los aplica el API de control.
- **XSS**: Latte escapa por defecto; prohibido `|noescape` con datos del API. El editor JSON se
  muestra como texto, no como HTML.
- **Auditoría**: el API registra `admin_audit` (login, mutaciones, `settings_update`,
  `client_purge_cache`). El panel no necesita endpoint de logs (fase futura).
- **Tokens de cliente**: se muestran una vez, nunca se persisten en cliente ni en logs; el
  portapapeles se limpia visualmente tras confirmar.

## 11. Despliegue (Dokploy)

- Servicio tipo **Dockerfile** (FrankenPHP + Caddy) con healthcheck `GET /_ok`.
- Build: `composer install --no-dev --optimize-autoloader`; los assets son estáticos y no
  requieren paso de compilación. Runtime: FrankenPHP.
- Dominio `admteleproxy.velone.ai` con TLS (Caddy/Let's Encrypt o Cloudflare Full Strict), sin
  path: la app usa rutas absolutas.
- Variables del servicio: las de §4.1. `PUBLIC_API_URL` apunta al API de control.
- El API debe configurarse con `GOOGLE_REDIRECT_URI` = callback del panel (§4.2) y esa URI debe
  estar registrada en Google Console.
- `CORS_ALLOWED_ORIGINS` del API vacío: no hay navegador llamando al API.
- Proxy de confianza configurado para obtener la IP real (rate limiting) sin aceptar headers
  falsificables.

## 12. Criterios de aceptación

1. **Solo Google**: no existe ningún input, ruta ni flujo de master token en el panel; el login
   con Google funciona end-to-end con un email de `ADMIN_ALLOWED_DOMAINS` dado de alta y activo.
2. **BFF**: en DevTools no aparece ninguna petición del navegador a `apiteleproxy.velone.ai` ni la
   cookie `teleproxy_admin`; el panel funciona con CORS del API desactivado.
3. **Sesión**: login crea sesión, `/me` confirma identidad, logout la destruye, un segundo login
   invalida el primero, la inactividad expira y un 401 del API redirige a `/login`.
4. **Clientes**: lista paginada con total; alta con token mostrado una sola vez y copiable;
   detalle/edición de whitelist, rate limit, scripting, fallbacks, header rules y preview con las
   validaciones de §9.
5. **Acciones**: rotar token (token nuevo una vez), rotar `crypt_id`, purgar caché (muestra
   `purged_keys`/`pattern`/`purged_at`), clonar con compensación ante fallo y borrar con doble
   confirmación.
6. **Export/import**: export sin tokens; import crea y configura, reporta resultado por cliente y
   muestra los tokens nuevos una sola vez.
7. **Admins**: lista leída de `{ admins: [...] }`; alta con dominio permitido; toggle y borrado
   muestran los 403 del API (autodesactivación, autoborrado, último admin).
8. **Ajustes**: `GET` muestra valores efectivos; `PATCH` con solo cambios; un valor fuera de rango
   muestra el 400 con el campo; un 409 invita a recargar; `verbose_errors` muestra su aviso.
9. **Errores**: 400/403/404/409/429 muestran el `message` del API, no textos genéricos.
10. **Seguridad/calidad**: CSP sin CDNs, activos self-hosted, PHPStan 8 limpio, tests verdes,
    `composer audit` sin vulnerabilidades, `/_ok` responde 200.

## 13. Fuera de alcance (fases futuras)

- Editor Lua con resaltado y validación (CodeMirror/Monaco).
- Constructor visual de `header_rules` (fase 1: JSON avanzado).
- Visor de auditoría en el panel (existe `admin_audit` en logs del API).
- Importación con dry-run y resolución de conflictos por `crypt_id`.
- Login con proveedores distintos de Google (no previsto).
- Acceso directo del navegador al API (descartado por diseño BFF).

## Anexo A — Decisión de arquitectura: Nette BFF frente a SvelteKit SPA

**Estado**: aceptada · **Fecha**: 2026-11-02 · **Sustituye**: el modelo SPA de la versión
anterior de esta spec (`SvelteKit` + `adapter-node`/`adapter-static` llamando directo al API).

### A.1 Contexto

La versión anterior especificaba una SPA que llamaba al API con `credentials: 'include'` y
prohibía expresamente los endpoints server-side de SvelteKit. Esa combinación obliga a que el
navegador sea el portador de la sesión del API: cookie `teleproxy_admin` cross-site, CORS con
credenciales, `SameSite=None` y `X-Admin-UI` en cada mutación.

En el workspace ya existe `admin-teleproxy`, una aplicación **Nette 3.2 + FrankenPHP** con
arquitectura hexagonal, `ApiClient` server-side, sesión `__Host-`, single-session en Valkey y
despliegue propio. El desajuste no era solo de stack: eran **dos modelos de sesión incompatibles**.

### A.2 Decisión

El panel es una aplicación **Nette server-rendered que actúa como BFF**. El navegador habla solo
con `admteleproxy.velone.ai`; la cookie del API vive exclusivamente en el servidor Nette. La
autenticación es **solo Google OAuth**, con el callback en Nette y reenvío server-side al API
(§5.2).

### A.3 Alternativas consideradas

| Alternativa | Descripción | Motivo del descarte |
| --- | --- | --- |
| **A. SPA directa al API** | SvelteKit en el navegador con `credentials: 'include'` | Depende de cookies de terceros, CORS y `SameSite=None`; frágil frente a bloqueo/particionado de cookies y con más superficie CSRF |
| **B. SvelteKit como BFF** | SvelteKit `+server.ts` reenvía server-side | Viable, pero reescribe en Node el BFF que Nette ya implementa; duplica sesión, rate limit y despliegue |
| **C. Nette BFF** (elegida) | Nette renderiza y reenvía server-side | Reutiliza el proyecto existente y elimina la dependencia de cookies cross-site |

La alternativa A era la única compatible con la prohibición de server routes de la spec anterior.
Levantar esa prohibición conduce a B, que es un BFF nuevo. Es decir: **evitar un BFF implicaba
aceptar cookies de terceros**.

### A.4 Evidencia en el código

1. **La sesión del API está pensada para UI externa o BFF, no para una SPA**
   - `src/application/admin_service.rs:52-77`: `cookie_same_site()` devuelve `None` con
     `ADMIN_UI_URL`; `cors_enabled()` activa CORS cuando hay allowlist.
   - `src/interfaces/admin_api.rs:95-113`: con CORS activo, toda mutación exige `X-Admin-UI`
     (CSRF por header).
   - `src/interfaces/admin_api.rs:159-171`: la cookie se emite `HttpOnly` con `SameSite` y
     `Secure` según la configuración.
   - En BFF esa cookie **no la recibe el navegador**: Nette la captura del `Set-Cookie` y la
     reenvía como header. `SameSite` deja de ser un factor de riesgo.

2. **El callback de Google es una ruta de navegación, no una API JSON**
   - `src/interfaces/admin_api.rs:243-283`: responde `302` + `Set-Cookie`, y los errores se
     renderizan como HTML.
   - Una SPA depende de que el navegador acepte esa cookie cross-site y de que el `302` la
     devuelva al panel. Nette recibe `code`+`state` en su dominio, reenvía con
     `allow_redirects=false`, captura la cookie y confirma con `/me` antes de renderizar.

3. **El BFF ya existe y está conectado**
   - `admin-teleproxy/src/Shared/Infrastructure/ApiClient.php:84-96`:
     `withSessionCookie()` y `Cookie: teleproxy_admin=...`.
   - `admin-teleproxy/config/common.neon:12-18`: cookie `__Host-`, `Secure`, `HttpOnly`,
     `SameSite=Lax`, 15 minutos.
   - `admin-teleproxy/app/Core/RouterFactory.php:19-23`: rutas de login y callback.
   - `admin-teleproxy/Dockerfile`, `Caddyfile` y `docker-compose.yml`: despliegue y healthcheck.

4. **Los huecos actuales son tareas, no motivos de reescritura**
   - `LoginPresenter.php:49`: el callback solo recibe `code` (falta `state`).
   - `LoginWithGoogleHandler.php:20-25`: ignora `code`/`state` y llama a `/me` sin cookie.
   - `@layout.latte:9,43-45`: Tabulator/Notiflix/Naja desde CDN (debe pasar a self-hosted).
   - `ApiAdminRepository.php`: asume un array raíz en `GET /admins` (el API devuelve
     `{ admins: [...] }`).
   - Falta la ruta `purge-cache` y la acción `import`.

### A.5 Consecuencias

**Positivas**

- Sin cookies de terceros ni CORS de navegador: el login no depende de políticas de cookies.
- Superficie CSRF menor: mutaciones solo por POST con token de Nette; `X-Admin-UI` siempre
  presente en el adaptador.
- Secretos fuera del alcance de JS: la cookie del API y el master token no entran al navegador.
- Manejo centralizado de 401/errores del API en un único adaptador.
- Un solo origen, TLS, runtime y healthcheck; se reutilizan slices, sesiones y despliegue.

**Negativas y mitigaciones**

- **Stack PHP/Nette**: menos común que Svelte; se mitiga con la arquitectura ya existente y
  PHPStan 8.
- **BFF con estado**: la sesión Nette requiere una réplica o sesiones compartidas (CouchDB) al
  escalar; el repositorio de sesiones ya existe.
- **Menos reactividad**: interacciones sin recarga vía Naja/AJAX; suficiente para formularios y
  tablas.
- **Un salto más**: Nette → API; irrelevante en la misma red, pero el panel pasa a ser un punto
  de disponibilidad con su propio `/_ok`.

### A.6 Condición de reversibilidad

El límite del BFF es `ApiClient`/`ApiAuthAdapter`. Un futuro frontend podría sustituir la capa de
presentación (por ejemplo, una SPA servida por Nette) manteniendo la sesión server-side. Lo que no
debe hacerse es mover la cookie del API al navegador: eso reintroduce el modelo A y sus riesgos.

### A.7 Cuándo revisar esta decisión

Reevaluar si (a) el panel necesita funcionar offline o como app nativa, (b) el API expone
autenticación por token para clientes que no son navegador, o (c) el equipo abandona PHP. En los
tres casos, el BFF puede mantenerse como capa de sesión y cambiar solo la presentación.
