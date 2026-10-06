# 📋 Especificación de Comportamiento (Gherkin) y Criterios de Aceptación

Este documento define el comportamiento esperado del sistema. El agente de código debe usar estos escenarios como base para las pruebas TDD/BDD (`cargo test` y crate `cucumber`).
`specs/*.feature` (un archivo por Feature, como exige Gherkin) es la versión ejecutable de este
documento: mismos escenarios, mismo orden, con etiquetas `@domain` (corre en cualquier plataforma),
`@linux` (requiere `--features proxy` en Linux) y `@pending` (requisito aún no implementado).
Si cambia uno, cambia el otro.

> **Motor**: el ciclo de vida del request corre sobre **Pingora** (`ProxyHttp`). Los escenarios que
> involucran filtros, streaming del cuerpo o el pool de conexiones hacia el origen solo son
> verificables en **Linux** con `--features proxy` (Docker `rust:bookworm`). Los escenarios de
> dominio (validaciones, MIME, claves de caché, sandbox) corren en cualquier plataforma.

## Feature 1: Endpoint Público de Proxy (`/aq/<crypt_id>`)

```gherkin
Feature: Proxy Público Sin Autenticación
  Como usuario de una aplicación
  Quiero solicitar recursos externos a través de una URL pública corta
  Para que el proxy aplique las reglas de mi cuenta sin exponer mi identidad real

  Scenario: Solicitud válida con crypt_id de 12 caracteres
    Given el servicio está en ejecución
    And existe un cliente con crypt_id "V1StGXR8_Z5j" y whitelist que incluye "shutterstock.com"
    When envío una solicitud GET a "/aq/V1StGXR8_Z5j/?url=https://shutterstock.com/img.jpg"
    Then el servicio debe procesar la solicitud sin pedir token de autorización
    And debe validar que el dominio está en la whitelist
    And debe devolver el recurso con HTTP 200 OK

  Scenario: Crypt_id inválido o inexistente
    When envío una solicitud GET a "/aq/invalid_id_123/?url=https://shutterstock.com/img.jpg"
    Then el servicio debe responder con HTTP 404 Not Found
    And debe registrar un log de error con "event": "invalid_crypt_id"

  Scenario: Dominio fuera de la whitelist
    Given el cliente con crypt_id "V1StGXR8_Z5j" solo tiene "shutterstock.com" en su whitelist
    When envío una solicitud GET a "/aq/V1StGXR8_Z5j/?url=https://malicious.com/payload.exe"
    Then el servicio debe responder con HTTP 403 Forbidden
    And debe registrar un log de error con "event": "domain_not_whitelisted"

  Scenario: Rate limit excedido por crypt_id
    Given el cliente tiene un límite de 50 peticiones por minuto
    And ya ha realizado 50 peticiones en el último minuto
    When envío la petición 51
    Then el servicio debe responder con HTTP 429 Too Many Requests
    And debe incluir el header "Retry-After: 60"
    And debe cerrar el keep-alive de la conexión downstream
    And debe registrar un log de error con "event": "rate_limit_exceeded"

  Scenario: Contador de rate limit atómico bajo concurrencia
    Given el cliente tiene un límite de 50 peticiones por minuto
    And varios workers de Pingora reciben peticiones concurrentes del mismo "crypt_id"
    When se procesan las peticiones
    Then el incremento en Valkey debe ser un único comando atómico (INCR con PEXPIRE solo en el primer incremento)
    And la cantidad total aceptada no puede exceder 50 por efecto de carrera

  Scenario: Valkey indisponible al verificar el rate limit
    Given Valkey no responde
    When llega una petición
    Then el límite debe resolverse con el contador en-proceso (LocalRateLimiter)
    And en modo degradado el tope por worker debe ser como máximo el configurado (nunca allow ilimitado)
    And debe registrar un log WARN con "event": "rate_limit_degraded"
    And la respuesta debe llevar la cabecera "X-Degraded-Mode: true"
```

## Feature 2: Seguridad de Red y Anti-SSRF

```gherkin
Feature: Protección Anti-SSRF y DNS Seguro
  Como administrador del sistema
  Quiero que todas las resoluciones de red sean validadas estrictamente
  Para prevenir ataques de Server-Side Request Forgery (SSRF) y DNS Rebinding

  Scenario: Resolución DNS segura con cadena de fallback
    Given el servicio está configurado con Quad9 ECS como primario
    When el servicio necesita resolver "shutterstock.com"
    Then debe usar DNS-over-TLS con validación DNSSEC
    And si Quad9 falla, debe hacer failover a Cloudflare Security, luego AdGuard, etc.

  Scenario: Bloqueo de resolución a IP privada (SSRF)
    When el servicio intenta resolver "http://169.254.169.254/latest/meta-data/"
    Then debe detectar que la IP es privada o de metadatos
    And debe responder con HTTP 403 Forbidden
    And debe registrar un log de error con "event": "ssrf_blocked", "reason": "private_ip"

  Scenario: Prevención de DNS Rebinding
    Given un dominio "attacker.com" resuelve a una IP pública en la primera consulta
    But intenta cambiar a una IP privada en la segunda consulta
    When el servicio procesa la solicitud
    Then `upstream_peer` debe construir el `HttpPeer` con la `SocketAddr` de la IP pública ya validada
    And el TLS debe dirigirse con `sni = "attacker.com"` y el header `Host` real debe conservarse
    And la conexión nunca debe re-resolver el nombre por el DNS del sistema

  Scenario: El proxy no sigue redirects por sí mismo
    Given el origen responde 302 con "Location: https://cdn.valido.com/recurso.jpg"
    And el destino resuelve a una IP pública
    When el servicio procesa la respuesta
    Then debe reenviar el 302 y su `Location` al cliente sin perseguirlo
    And no debe emitir una segunda solicitud hacia el destino del redirect

  Scenario: Bloqueo de redirect a IP privada
    Given el origen "https://attacker.com/redirect" devuelve un HTTP 302
    And el header "Location" apunta a "http://127.0.0.1/admin"
    When el servicio procesa el redirect
    Then debe validar la nueva URL contra las reglas anti-SSRF
    And debe abortar la solicitud con HTTP 403 Forbidden
    And debe registrar un log de error con "event": "ssrf_blocked", "reason": "redirect_to_private_ip"

  Scenario: URL con credenciales o esquemas no permitidos
    When envío una solicitud con "?url=ftp://user:pass@169.254.169.254/"
    Then el servicio debe responder con HTTP 400 Bad Request
    And debe registrar un log de error con "event": "invalid_url_format"
```

## Feature 3: Caché, Streaming y Límites de Recursos

```gherkin
Feature: Gestión de Caché y Memoria
  Como sistema
  Quiero cachear respuestas y limitar el consumo de memoria
  Para garantizar el rendimiento y prevenir ataques de agotamiento de recursos (OOM)

  Scenario: Cache Miss y almacenamiento en Valkey
    Given el recurso no está en caché
    When el servicio obtiene una respuesta exitosa del origen
    Then debe serializar la respuesta (status, headers, body) con postcard
    And debe guardarla en Valkey con la clave "px:{internal_id}:{config_version}:{url_hash}"
    And debe aplicar el TTL definido en la estrategia de MIME
    And debe responder con "X-Cache: MISS"

  Scenario: Cache HIT sin tocar el origen
    Given la clave "px:{internal_id}:{config_version}:{url_hash}" existe en Valkey
    When se solicita el mismo recurso
    Then debe responder 200 con el cuerpo cacheado
    And debe responder con "X-Cache: HIT"
    And no debe abrir conexión con el origen

  Scenario: Invalidación de caché por versión de configuración
    Given la configuración del cliente cambia y su "config_version" incrementa
    When se realiza una nueva solicitud
    Then el servicio debe generar una nueva clave de caché en Valkey
    And NO debe ejecutar comandos de borrado masivo (SCAN/DEL) en Valkey

  Scenario: Bypass de scripting por límite de tamaño de cuerpo
    Given el cliente tiene "max_scripting_body_bytes" configurado a 5 MB
    And el origen responde con un archivo de 15 MB
    When el servicio procesa la respuesta
    Then debe omitir la ejecución del script Lua
    And debe transmitir el cuerpo chunk a chunk desde `upstream_response_body_filter`
    And el primer byte debe llegar al cliente antes de que termine la descarga del origen
    And no debe acumular los 15 MB en memoria RAM del proceso
    And debe responder con "X-Cache: BYPASS" porque 15 MB supera el límite cacheable de 5 MB

  Scenario: Origen que envía más de 100 MB
    Given el origen anuncia o envía un cuerpo que supera 100 MB
    When el contador de bytes del contexto excede el límite
    Then el servicio debe abortar la transmisión
    And debe responder con el error "payload_too_large"
    And no debe guardar nada en Valkey para esa URL
```

## Feature 4: Scripting Lua Sandboxed y Webhooks

```gherkin
Feature: Ejecución Segura de Scripts Lua
  Como cliente avanzado
  Quiero ejecutar lógica personalizada en las respuestas
  Para transformar imágenes o censurar datos, sin comprometer la seguridad del servidor

  Scenario: Ejecución exitosa de script Lua válido
    Given el cliente tiene un script que llama a `proxy.regex_replace`
    When el servicio procesa una respuesta HTML
    Then debe ejecutar el script en un entorno `mlua` aislado
    And debe devolver el cuerpo modificado al cliente

  Scenario: Bloqueo de acceso al sistema desde Lua (Sandbox)
    Given el script del cliente contiene `os.execute("rm -rf /")`
    When el servicio intenta ejecutar el script
    Then el sandbox de `mlua` debe lanzar un error de "attempt to call a nil value"
    And el servicio debe abortar el script y devolver el contenido original
    And debe registrar un log de error con "event": "lua_sandbox_violation"

  Scenario: Protección contra ReDoS en regex
    Given el script intenta compilar un patrón regex catastrófico "(a+)+b"
    When el servicio compila el regex
    And la compilación tarda más de 100ms
    Then el servicio debe abortar la operación regex
    And debe devolver el contenido original sin transformar
    And debe registrar un log de error con "event": "redos_blocked"

  Scenario: Timeout en webhook desde Lua
    Given el script llama a `proxy.http_request` con un servicio externo lento
    And el timeout configurado es de 2000ms
    When el servicio externo tarda 5 segundos en responder
    Then el servicio debe abortar la petición al webhook
    And debe continuar con el flujo normal o fallback según la configuración

  Scenario: Webhook desde Lua no puede alcanzar la red interna
    Given el script llama a `proxy.http_request("http://169.254.169.254/latest/meta-data/")`
    When se ejecuta la llamada
    Then la URL debe pasar por las mismas validaciones anti-SSRF que el proxy
    And la resolución debe hacerse con `SecureDnsResolver`
    And el servicio debe responder 403 con "event": "ssrf_blocked"
    And nunca debe abrir un socket directo desde el sandbox Lua

  Scenario: Timeout real de ejecución del script
    Given el script contiene un bucle infinito
    And `LUA_TIMEOUT_MS` está en 200
    When el servicio ejecuta el script
    Then la ejecución debe interrumpirse dentro de una tolerancia pequeña del límite
    And debe devolver el cuerpo original sin transformar
    And debe registrar un log WARN con "event": "script_timeout"
```

## Feature 5: Manejo de Errores y Fallbacks por MIME

```gherkin
Feature: Respuestas de Error Consistentes
  Como cliente
  Quiero recibir un recurso válido incluso si el origen falla
  Para evitar que se rompa el layout de mi aplicación (ej. etiqueta <img> rota)

  Scenario: Fallback de imagen cuando el origen falla (modo wrapped)
    Given el cliente tiene "error_handling.mode" = "wrapped"
    And la URL solicitada termina en ".jpg" o el query param es "?mime=image/png"
    When el origen responde con HTTP 500 Internal Server Error
    Then el servicio debe interceptar el error
    And debe buscar el fallback en Valkey ("px:defaults:image/png") o usar el embebido
    And debe responder con HTTP 200 y un "Content-Type" de imagen utilizable por `<img>`
    And debe marcar la respuesta con "X-Cache: FALLBACK"
    # 200 y no 404: el objetivo declarado es que la etiqueta <img> no se rompa, y el navegador
    # descarta el cuerpo si el status no es exitoso. El estado del origen queda en los logs.
    # Nivel 3 (embebido): para cualquier image/* el recurso embebido es un SVG, asi que el
    # Content-Type del placeholder es image/svg+xml cuando no hay entrada global en Valkey.

  Scenario: Orden de prioridad para inferir el MIME del fallback
    Given el cliente solicita una URL
    Then el MIME del fallback se determina en este orden: ?mime= explícito, extensión de la URL, header Accept
    And si ninguna fuente aporta un MIME se usa application/octet-stream

  Scenario: Prioridad de inferencia de MIME para fallbacks
    Given la URL es "https://origen.com/api/getAsset" (sin extensión)
    And la solicitud incluye "?mime=application/pdf"
    And el header "Accept" pide "image/*"
    When el origen responde con HTTP 500
    Then el servicio debe priorizar el parámetro "?mime=" sobre el header "Accept"
    And debe servir el fallback de PDF

  Scenario: Discrepancia entre MIME forzado y real
    Given el cliente solicita "?mime=text/html"
    But el origen responde exitosamente con "Content-Type: image/jpeg"
    When el servicio procesa la respuesta
    Then debe respetar el "Content-Type: image/jpeg" del origen
    And debe inyectar "X-Content-Type-Options: nosniff"
    And debe registrar un log WARN con "event": "mime_discrepancy"
```

> **Estado de `mime_discrepancy`**: es un requisito de especificación todavía no implementado en el
> handler. El comportamiento de las dos primeras líneas (respetar el `Content-Type` real del origen e
> inyectar `nosniff`) sí está vigente; falta el log WARN. Hay que cerrar las dos cosas en la misma
> fase para que este escenario pase.

## Feature 6: API de Control y Auditoría (`/api/v1/`)

```gherkin
Feature: API de Control Segura
  Como administrador de la cuenta
  Quiero gestionar mi configuración a través de una API autenticada
  Para mantener el aislamiento total de mis datos

  Scenario: Acceso sin token Bearer
    When envío una solicitud GET a "/api/v1/clients/config" sin header Authorization
    Then el servicio debe responder con HTTP 401 Unauthorized

  Scenario: Acceso con token Bearer válido
    Given tengo un token Bearer válido asociado a mi "internal_id"
    When envío una solicitud GET a "/api/v1/clients/config" con "Authorization: Bearer <token>"
    Then el servicio debe responder con HTTP 200 OK
    And debe devolver SOLO mi configuración (incluyendo mi "crypt_id" actual)

  Scenario: PUT de configuración con valores fuera de cota
    Given tengo un token Bearer válido
    When envío `PUT /api/v1/clients/config` con "rate_limit.max_requests = 0"
    Or con "rate_limit.window_seconds = 0"
    Or con un "whitelist" de más de 100 dominios
    Then el servicio debe responder 400 Bad Request
    And debe denegar el dominio de las cabeceras `Host`/`Authorization` y las IP privadas
    And no debe escribir el documento en CouchDB
    And debe registrar un log WARN con "event": "invalid_config"

  Scenario: No existe alta de clientes por API
    When envío una solicitud POST a "/api/v1/clients"
    Then el servicio debe responder 404 (ruta inexistente)
    # el provisionamiento de clientes es fuera de banda: se crea el documento en CouchDB

  Scenario: Rotación de crypt_id
    Given mi "crypt_id" actual es "V1StGXR8_Z5j"
    When envío una solicitud POST a "/api/v1/clients/rotate-id" con mi token
    Then el servicio debe generar un nuevo nanoid de 12 caracteres
    And debe invalidar el "crypt_id" anterior inmediatamente
    And debe registrar un log de auditoría con "event": "crypt_id_rotated"
```

---

## ✅ Criterios de Aceptación del Proyecto (Definition of Done)

Para que una fase o el proyecto completo se considere "Terminado", debe cumplir:

1. **Compilación y Linting**: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`
   y, en Linux (Docker `rust:bookworm`), `cargo clippy --all-targets --features proxy -- -D warnings`.
   Todo comando de verificación con `--locked`.
2. **Pruebas**: `cargo test` pasa al 100%. Las pruebas unitarias cubren la lógica de dominio
   (validación de URLs, IPs privadas, parsing de MIME, claves de caché, sandbox de Lua) y los
   escenarios de `specs/*.feature` están trazados por nombre. Los filtros de Pingora se
   verifican en Linux.
3. **Seguridad**: No hay uso de `.unwrap()` en rutas de producción. Los errores se manejan con
   `thiserror` y se registran vía `tracing`. Ningún socket saliente se abre por fuera de
   `infrastructure/http_client.rs` / `dns_resolver.rs`, incluyendo el webhook `proxy.http_request`.
4. **Docker**: El `Dockerfile` es multi-etapa (`rust:bookworm` -> `debian:bookworm-slim`) y compila
   con `--release --locked --features proxy`; la construcción es **solo en Linux**. La imagen final
   es **< 80 MB** (con Pingora; el límite histórico de 50 MB pertenecía a la etapa Axum+reqwest) y se
   levanta con el `docker-compose.yml` proporcionado, tras crear a mano la base `tele_proxy_configs`.
5. **Logs**: En configuración de producción, solo se emiten logs de nivel `WARN` o `ERROR` en formato
   JSON estructurado. Dos excepciones en `INFO`, ambas del diccionario de `docs/ERROR_DICTIONARY.md`:
   `event: request_completed` (logging de acceso) y `event: crypt_id_rotated` (pista de auditoría de
   la rotación; si se filtrara por nivel, el escenario «Rotación de crypt_id» no dejaría evidencia).
   Cada entrada usa el nombre del diccionario en `event` y sus campos — incluido `internal_id` en la
   rotación, que es el único identificador que sobrevive al cambio de `crypt_id`.
6. **Commits**: Cada funcionalidad tiene su propio commit atómico con mensajes convencionales
   (`feat:`, `test:`, `fix:`).
7. **Evidencia**: Un `exit 0` del envoltorio no cierra el criterio: se debe pegar/leer el log real de
   `cargo` (warnings incluidos) antes de declarar la fase terminada.
