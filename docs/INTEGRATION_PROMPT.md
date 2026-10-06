# Prompt de Integración para Agentes de Codificación

Este documento contiene un **prompt listo para copiar y pegar** en un agente de codificación
(Copilot, Claude Code, Kimi Code, Cursor, …) para que integre TeleProxy como capa de salida de
media/contenido en un proyecto existente.

**Cómo usarlo**:

1. Copia el bloque de la sección [El prompt](#el-prompt).
2. Sustituye los valores entre `<ANGULARES>` (dominios del proyecto, framework, rutas).
3. Pégalo en el agente con acceso al repositorio del proyecto.

**Qué necesitas antes de empezar** (lo proporciona el operador de TeleProxy, no el agente):

- Un `crypt_id` y su Bearer token (cliente provisionado en TeleProxy).
- Los dominios de origen que el proyecto va a consumir (para la whitelist).
- La URL del proxy (`https://teleproxy.velone.ai` en producción; `http://localhost:8080` en
  desarrollo local contra el contenedor del demo client).

---

## El prompt

```text
INTEGRA TELEPROXY COMO PROXY DE SALIDA DE MEDIA/CONTENIDO EN ESTE PROYECTO

## Contexto

TeleProxy es un proxy inverso multi-cliente que usamos para servir recursos remotos
(imágenes, CSS, JS, HTML) sin exponger nuestra infraestructura y con control por cliente
(whitelist de dominios, rate limit, transformación de headers). Documentación de referencia:
https://github.com/stringnetlab/teleproxy (docs/CLIENT_CONFIG.md y docs/HEADER_RULES.md).

Formato del endpoint público (solo GET):

    <BASE>/aq/<CRYPT_ID>/?url=<URL_ORIGEN_PERCENT_ENCODED>[&mime=<mime_opcional>]

Ejemplo:
    https://teleproxy.velone.ai/aq/V1StGXR8_Z5j/?url=https%3A%2F%2Fcdn.ejemplo.com%2Flogo.png

## Tarea

Integra el acceso a través de TeleProxy en <PROYECTO/STACK: p. ej. "una app Next.js 14">,
con estos pasos concretos:

### 1. Configuración del lado de TeleProxy (vía API de control)

Usa curl (o el HTTP client que prefieras) contra la API de control — pide a mi equipo la URL
del túnel/loopback y el Bearer token del cliente. Endpoints:

    GET  /api/v1/clients/config           # leer config actual
    PUT  /api/v1/clients/config           # actualización parcial (JSON)
    POST /api/v1/clients/rotate-id        # rotar crypt_id (no lo uses sin avisar)

Configura como mínimo:
- whitelist: añade SOLO los dominios de origen que el proyecto consume:
  <DOMINIOS: p. ej. ["cdn.misrecursos.com", "img.shutterstock.com"]>. Formato: host desnudo,
  sin esquema ni path. Máx. 100.
- rate_limit acorde al tráfico esperado (default prudente: 500 req / 60 s).
- header_rules: aplica esta receta de CORS para que los assets sean consumibles desde el
  navegador (adapta el origen al dominio del proyecto):

    {
      "header_rules": [{
        "expression": "http.response.status eq 200 and (starts_with(http.response.content_type, \"image/\") or starts_with(http.response.content_type, \"font/\"))",
        "action": "set",
        "action_parameters": { "headers": [
          { "name": "access-control-allow-origin", "value": "https://<DOMINIO_DE_MI_APP>", "operation": "set" },
          { "name": "cache-control", "value": "public, max-age=86400, immutable", "operation": "set" }
        ]}
      }]
    }

### 2. Variables de entorno del proyecto

Añade a .env.example (sin valores reales en el repo):

    TELEPROXY_BASE_URL=https://teleproxy.velone.ai
    TELEPROXY_CRYPT_ID=<CRYPT_ID>

En desarrollo local contra el contenedor: TELEPROXY_BASE_URL=http://localhost:8080 y el
crypt_id del demo client (se obtiene con: GET /api/v1/clients config usando Bearer demo-token
contra http://localhost:8081).

### 3. Helper central de construcción de URLs (OBLIGATORIO, no disperses la lógica)

Crea <RUTA: p. ej. "lib/teleproxy.ts" o "app/Services/TeleProxy.php"> con una función única
que reciba una URL de origen y devuelva la URL proxied:

    function teleproxyUrl(origen: string): string {
      const base = process.env.TELEPROXY_BASE_URL;
      const cryptId = process.env.TELEPROXY_CRYPT_ID;
      if (!base || !cryptId) throw new Error("TeleProxy no configurado");
      return `${base}/aq/${cryptId}/?url=${encodeURIComponent(origen)}`;
    }

Regla de uso: TODO recurso externo servido al cliente pasa por este helper. Nunca construyas
la URL a mano en los componentes.

### 4. Sustitución de URLs en el proyecto

Localiza donde el proyecto referencia assets externos <DOMINIOS> (templates, componentes,
imports de CSS/JS, JSON de configuración) y pásalos por el helper. Mantén los atributos
alt/dimensiones; el <img>/<link>/<script> sigue siendo HTML estándar.

### 5. Manejo de errores (el proxy devuelve JSON de error; comportamiento por status)

- 403 domain_not_whitelisted: el dominio no está en la whitelist del cliente → corrige la
  config (paso 1), no es un fallo transitorio.
- 429 rate_limit_exceeded: respeta el header Retry-After (segundos) antes de reintentar.
- 502 Bad gateway / 504 Gateway timeout: el ORIGEN falló (no el proxy). Reintenta con
  backoff; si persiste, el origen está caído o nos bloquea.
- 3xx: el proxy NO sigue redirects, pero reescribe Location a una URL del proxy → tu cliente
  HTTP debe seguir redirects normalmente y la cadena funcionará.
- Modo desarrollo de TeleProxy (MODO=desarrollo): los cuerpos de error incluyen "details"
  con el motivo exacto; en producción son escuetos. Loguea siempre el status + error code.

### 6. Headers de respuesta útiles

- X-Cache: HIT/MISS/BYPASS/FALLBACK (estado de la caché del proxy).
- X-Proxy-By: identifica que la respuesta pasó por TeleProxy.
- Los que inyectes vía header_rules (CORS, cache-control) llegan tal cual.

## Restricciones (no violar)

- Solo GET. No se puede usar para APIs con POST/cookies de sesión.
- Tope de respuesta: 100 MB. No sirvas descargas grandes por aquí.
- La URL de origen no puede llevar credenciales ni fragmentos (#).
- No expongas el Bearer token del cliente en el frontend (solo se usa en paso 1, backend o
  administración); el crypt_id en la URL pública no es secreto pero es rotable.
- Si el agente detecta que necesita un dominio que no está en la whitelist, debe DEJARLO
  anotado en el resumen (no desactivar la validación).

## Criterios de aceptación (verifica antes de terminar)

1. Ninguna URL externa <DOMINIOS> queda sin pasar por teleproxyUrl() (grep de auditoría).
2. Las variables de entorno están en .env.example con placeholders y documentadas en el README.
3. Una imagen de prueba de <DOMINIOS> carga en el navegador con status 200 y
   access-control-allow-origin presente (pestaña Network).
4. El JSON de error del proxy se maneja: simula un 429 y verifica que el reintento espera
   Retry-After.
5. README del proyecto: sección "Media vía TeleProxy" de 5-10 líneas con el formato de URL
   y a quién pedir el crypt_id.

Entrega un resumen final con: archivos creados/modificados, dominios añadidos a la whitelist,
y cualquier dominio detectado que falte por autorizar.
```

---

## Notas para el operador de TeleProxy

- El prompt asume que el **provisionamiento del cliente es externo** a este servicio (no hay
  `POST /api/v1/clients`): el agente solo toca la config del cliente existente vía `PUT`.
- Si el proyecto necesita **modificar el cuerpo** de las respuestas (no solo headers), eso va
  por scripting Lua del cliente (`scripting`), no por este prompt — evalúalo aparte.
- Para proyectos que consumen muchos subdominios de un mismo dominio, recuerda que la
  whitelist matchea por sufijo: basta `ejemplo.com` para `cdn1.ejemplo.com`, etc.
- Rotación de `crypt_id` (`POST /api/v1/clients/rotate-id`) invalida las URLs públicas:
  coordínala con un despliegue del proyecto que actualice `TELEPROXY_CRYPT_ID`.
