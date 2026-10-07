# SPEC — Interfaz web de administración (tele-proxy)

> Borrador para implementación por agente. El backend (tele-proxy) ya implementa todo lo que esta
> spec consume: ver "Prerequisitos en el API". Contrato canónico de endpoints:
> `docs/api_contract.yaml` (sección `/api/v1/admin/*`); configuración: `docs/ENVIRONMENT.md`.

## 1. Objetivo

Panel de administración de tele-proxy como **proyecto web independiente** que consume la API de
control. Permite gestionar clientes (configuración, tokens, crypt_id, wildcard) y administradores
(login, whitelist) sin tocar CouchDB a mano.

Arquitectura de dominios (todos servidos por Dokploy/Traefik con TLS):

| Dominio | Servicio | Rol |
| --- | --- | --- |
| `https://teleproxy.velone.ai` | tele-proxy (puerto proxy) | Proxy público `/aq/{crypt_id}/` — **no tocar** |
| `https://apiteleproxy.velone.ai` | tele-proxy (puerto de control) | API de control y administración |
| `https://admteleproxy.velone.ai` | **este proyecto** | UI de administración (SPA) |

## 2. Stack obligatorio

- **SvelteKit** con **Svelte 5 (runes: `$state`, `$derived`, `$effect`)** y **TypeScript** estricto.
- `@sveltejs/adapter-node` (servir con Node en Dokploy) o `adapter-static` en modo SPA
  (`fallback: 'index.html'`) si se prefiere estático puro. Ambos aceptables; elegir uno y justificar.
- Sin backend propio: la UI es 100% cliente que llama a la API. **Prohibido** crear endpoints de
  servidor que reenvíen a la API (SvelteKit `+server.ts` proxy): rompería el modelo de sesión.
- CSS: la que el proyecto traiga por defecto (scoped por componente); sin requisito de framework CSS.
- Estado: runes + módulos simples; no imponer stores externas salvo necesidad.

## 3. Variables de entorno (build/runtime)

| Variable | Descripción |
| --- | --- |
| `PUBLIC_API_URL` | Origen de la API, **obligatoria**: `https://apiteleproxy.velone.ai` (sin slash final) |

En desarrollo local: `PUBLIC_API_URL=http://localhost:8785` apuntando a un tele-proxy local con
`ADMIN_UI_URL=http://localhost:5173` y `CORS_ALLOWED_ORIGINS=http://localhost:5173`.

## 4. Prerequisitos en el API (YA IMPLEMENTADOS — no reimplementar)

Configuración del servicio tele-proxy en Dokploy para soportar esta UI:

| Variable | Valor |
| --- | --- |
| `ADMIN_UI_URL` | `https://admteleproxy.velone.ai` |
| `CORS_ALLOWED_ORIGINS` | `https://admteleproxy.velone.ai` |
| `GOOGLE_REDIRECT_URI` | `https://apiteleproxy.velone.ai/api/v1/admin/auth/google/callback` (+ misma URI en Google Console) |

Comportamiento del API con esa configuración (release ≥ 5):

- **CORS**: allowlist exacta, `Access-Control-Allow-Credentials: true`, métodos
  GET/POST/PUT/PATCH/DELETE/OPTIONS, headers permitidos `content-type`, `authorization`,
  `x-admin-ui`. Sin estas variables el API no emite cabeceras CORS.
- **Sesión**: cookie `teleproxy_admin`, `HttpOnly; Secure; SameSite=None; Path=/` (SameSite=None
  habilitado precisamente por la UI cross-origin). TTL `ADMIN_SESSION_TTL_SECONDS` (default 3600).
- **CSRF**: con CORS activo, TODA mutación (POST/PUT/PATCH/DELETE) en `/api/v1/admin/*` exige el
  header `X-Admin-UI` (cualquier valor). Sin él → `403`. Las rutas de auth (`/auth/google/url`,
  `/auth/google/callback`) son GET y no lo exigen.
- **Post-login**: el callback de Google hace 302 a `ADMIN_UI_URL` (ya no a `/admin/`).

## 5. Cliente API (wrapper de fetch)

Un único módulo (`src/lib/api.ts`) con:

```ts
api(path: string, options?: { method?: string; body?: unknown }): Promise<T>
```

- Base: `PUBLIC_API_URL` + path. **Siempre** `credentials: 'include'` (la sesión vive en la cookie
  cross-site) y header `X-Admin-UI: 1` en todos los métodos (GET incluido: simplifica y no estorba).
- `Content-Type: application/json` cuando hay body.
- **Master token fallback**: si existe `localStorage.teleproxy_master`, enviar además
  `Authorization: Bearer <token>`. El API acepta cookie de sesión O master token.
- Errores normalizados: el cuerpo de error es `{ error: string, message: string, details?: ... }`
  (ver `ErrorResponse` en el contrato). Lanzar `ApiError { status, code, message }`.
- 401 → redirigir a la pantalla de login (sesión expirada o sin autenticar). 429 → mostrar
  "demasiados intentos, espera" con el mensaje del API.

## 6. Autenticación y flujos

### 6.1 Login con Google (flujo feliz)

1. `GET /api/v1/admin/auth/google/url` → `{ url }`; redirigir el navegador (`window.location`).
2. Google → callback en el **API** (`apiteleproxy…/callback`) → valida, crea sesión (Set-Cookie)
   → 302 a `ADMIN_UI_URL` (= la raíz de esta UI).
3. La UI arranca, llama a `GET /api/v1/admin/me`: 200 → panel; 401 → pantalla de login.
4. Errores del callback: el API los renderiza como HTML propio (pantalla de error con link al
   panel) — la UI **no** necesita manejarlos.

Casos de error de login que la UI sí muestra (después de `/me` o del botón):
`403` email no admin → "El email X no es un administrador registrado".
`503` login Google no configurado → mostrar el fallback de master token como opción principal.

### 6.2 Master token (bootstrap y emergencia)

- Input en la pantalla de login, siempre visible (es la vía de creación del primer admin).
- Guardar en `localStorage.teleproxy_master` al pulsar "Usar master token"; `GET /me` con bearer
  debe devolver `via: "master"`.
- Sección "Administradores" accesible con master: añadir el primer email.
- Botón "Olvidar master token" que limpia el localStorage.

### 6.3 Sesión

- `GET /me` al arrancar decide si hay sesión: cachear en estado global (runes) y revalidar ante
  401. Email del admin se muestra en la cabecera con botón "Cerrar sesión"
  (`POST /auth/logout` → limpiar estado → login).

## 7. Pantallas y rutas

| Ruta | Pantalla |
| --- | --- |
| `/login` | Login (botón Google + input master token). Si ya hay sesión, redirect a `/`. |
| `/` | Clientes: tabla paginada + crear + acciones por fila. |
| `/clients/[id]` | Detalle/editor de un cliente (ver §8). |
| `/admins` | Administradores: lista, añadir, activar/desactivar, eliminar. |

Layout común: cabecera con email del admin, nav (Clientes / Administradores), botón logout.
Toda mutación confirma antes con diálogo nativo (`confirm`) salvo edición de formulario.

### 7.1 Clientes (`/`)

- `GET /api/v1/admin/clients?limit=50&skip=N` → `{ clients: AdminClientView[], total }`.
- Columnas: `crypt_id`, `kind`, `wildcard` (badge), `whitelist` (resumen, p.ej. primeros 3 dominios
  + "+N"), `rate_limit` (`max_requests/window_seconds`), `config_version`.
- Paginación por `skip/limit` con total (botones anterior/siguiente + info de página).
- "Crear cliente": checkbox "salida libre (wildcard)" + botón → `POST /clients` (body
  `{ wildcard }` opcional). El `token` de la respuesta se muestra **una sola vez** en un bloque
  destacado con botón copiar al portapapeles y aviso de que no se volverá a mostrar.
- Acciones por fila → navegar a `/clients/[id]`.

### 7.2 Detalle de cliente (`/clients/[id]`)

- `GET /api/v1/admin/clients/{id}` → config completa (incluye `scripting.code`; **nunca**
  `bearer_token_hash` — el API no lo devuelve).
- Formulario (guardar → `PUT /clients/{id}`):
  - `whitelist`: textarea, un dominio por línea (el API valida: máx 100 entradas, dominio/IP
    pública válidos, sin comodines).
  - `rate_limit`: `max_requests` (1..10000) y `window_seconds` (1..3600).
  - `wildcard`: checkbox (solo admin; es el flag de salida libre).
  - `scripting.enabled`: checkbox; `scripting.code`: textarea (editor simple; el resaltado Lua es
    **fase 2**); `scripting.expression`: input opcional.
  - `max_scripting_body_bytes`: number (1048576..52428800).
  - `error_handling.fallback_urls`: JSON textarea (`{ "<mime>": { "url", "hash" } }`) validado en
    cliente antes de enviar.
  - `header_rules`: **fase 2** (hoy puede editarse como JSON crudo si se incluye el campo en el
    payload, documentarlo como avanzado).
- Respuesta del PUT: `{ config_version, updated_at }` — mostrar versión nueva.
- Botones: "Rotar token" (`POST …/rotate-token` → token nuevo una sola vez, bloqueo de UI hasta
  copiar), "Rotar crypt_id" (`POST …/rotate-id` → muestra nuevo `new_crypt_id`), "Eliminar cliente"
  (confirm doble, `DELETE` → volver a `/`).
- Errores de validación del PUT (400): mostrar `message` del API campo a campo si es posible, o
  global.

### 7.3 Administradores (`/admins`)

- `GET /api/v1/admin/admins` → lista `{ email, role, active, created_at, created_by }`.
- Añadir: input email → `POST /admins` (dominio restringido a `ADMIN_ALLOWED_DOMAINS` del API;
  errores 400 con mensaje del API).
- Activar/desactivar: `PATCH /admins/{email}` `{ active }` (el API rechaza autodesactivación 403).
- Eliminar: `DELETE /admins/{email}` (el API rechaza autoborrado y último admin activo, 403 —
  mostrar el mensaje).

## 8. Modelos TypeScript (mínimos)

```ts
interface Me { email: string; role: string; via: 'session' | 'master' }
interface AdminUser { email: string; role: string; active: boolean; created_at: string; created_by: string }
interface RateLimit { max_requests: number; window_seconds: number }
interface ClientView {
  internal_id: string; crypt_id: string; kind: 'client' | 'admin'; wildcard: boolean;
  config_version: number; whitelist: string[]; rate_limit: RateLimit;
  max_scripting_body_bytes: number;
  scripting: { enabled: boolean; code: string | null; code_hash: string | null; expression: string | null };
  error_handling: { mode: string; fallback_urls: Record<string, { url: string; hash: string }> };
  // header_rules: ver contrato; GET individual incluye scripting.code
}
interface ClientsPage { clients: ClientView[]; total: number }
```

Generar tipos desde `docs/api_contract.yaml` con `openapi-typescript` si se prefiere; mantener
estos como referencia mínima. **Los nombres de campo son snake_case** (el API no usa camelCase).

## 9. Seguridad (obligatorio)

- Svelte escapa por defecto: **no** usar `{@html}` con datos del API.
- CSP estricta en el hosting; todo el JS es propio del build (sin CDNs).
- El master token solo en `localStorage` (nunca en URL, logs ni estado compartido).
- Manejar la expiración de sesión: ante 401 en cualquier llamada → `/login`.
- No enviar ningún dato a terceros; la única red es `PUBLIC_API_URL`.

## 10. Despliegue (Dokploy)

- Nuevo servicio tipo **Dockerfile** o **Nixpacks** en Dokploy, repo propio (sugerido:
  `stringnetlab/tele-proxy-admin`).
- Build: `npm ci && npm run build`; runtime: `node build` (adapter-node) o estático servido por
  Traefik (adapter-static).
- Dominio: `admteleproxy.velone.ai` con HTTPS (letsencrypt), sin path (raíz).
- Env del servicio: `PUBLIC_API_URL=https://apiteleproxy.velone.ai`.
- Healthcheck: cualquier 200 en `/`.

## 11. Criterios de aceptación

1. Login con Google end-to-end: botón → Google → callback → cookie → panel (con un email admin en
   `gmail.com`/`stringnet.pe` y el API configurado por §4).
2. Login con master token: accede, crea un admin nuevo, y ese admin entra por Google.
3. CRUD completo de clientes con las reglas de §7 (token mostrado una sola vez con copiar).
4. Mutaciones siempre envían `X-Admin-UI`; sin sesión → 401 → `/login`.
5. Errores del API (400/403/429) visibles con el `message` del servidor, no genéricos.
6. `npm run build` limpio, TypeScript estricto sin `any` implícitos.

## 12. Fuera de alcance (fases futuras)

- Editor Lua con resaltado/validación (CodeMirror/Monaco).
- Constructor visual de `header_rules`.
- Visor de auditoría (el API aún no exp endpoint de logs; existe `event=admin_audit` en logs).
- SSR: innecesario, la sesión es cookie + API.
