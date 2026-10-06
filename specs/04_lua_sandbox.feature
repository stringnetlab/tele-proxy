# Versión ejecutable de docs/BDD.md -> "Feature 4".
# Etiquetas: @domain = verificable en cualquier plataforma; @linux = requiere --features proxy en Linux.

Feature: Ejecución Segura de Scripts Lua
  Como cliente avanzado
  Quiero ejecutar lógica personalizada en las respuestas
  Para transformar imágenes o censurar datos, sin comprometer la seguridad del servidor

  @domain
  Scenario: Ejecución exitosa de script Lua válido
    Given el cliente tiene un script que llama a proxy.regex_replace
    When el servicio procesa una respuesta HTML
    Then debe ejecutar el script en un entorno mlua aislado
    And debe devolver el cuerpo modificado al cliente

  @domain
  Scenario: Bloqueo de acceso al sistema desde Lua (Sandbox)
    Given el script del cliente contiene os.execute("rm -rf /")
    When el servicio intenta ejecutar el script
    Then el sandbox de mlua debe lanzar un error de "attempt to call a nil value"
    And el servicio debe abortar el script y devolver el contenido original
    And debe registrar un log de error con "event": "lua_sandbox_violation"

  @domain
  Scenario: Protección contra ReDoS en regex
    Given el script intenta compilar un patrón regex catastrófico "(a+)+b"
    When el servicio compila el regex
    And la compilación tarda más de 100ms
    Then el servicio debe abortar la operación regex
    And debe devolver el contenido original sin transformar
    And debe registrar un log de error con "event": "redos_blocked"

  @domain
  Scenario: Timeout en webhook desde Lua
    Given el script llama a proxy.http_request con un servicio externo lento
    And el timeout configurado es de 2000ms
    When el servicio externo tarda 5 segundos en responder
    Then el servicio debe abortar la petición al webhook
    And debe continuar con el flujo normal o fallback según la configuración

  @domain
  Scenario: Webhook desde Lua no puede alcanzar la red interna
    Given el script llama a proxy.http_request("http://169.254.169.254/latest/meta-data/")
    When se ejecuta la llamada
    Then la URL debe pasar por las mismas validaciones anti-SSRF que el proxy
    And la resolución debe hacerse con SecureDnsResolver
    And el servicio debe responder 403 con "event": "ssrf_blocked"
    And nunca debe abrir un socket directo desde el sandbox Lua

  @domain
  Scenario: Timeout real de ejecución del script
    Given el script contiene un bucle infinito
    And LUA_TIMEOUT_MS está en 200
    When el servicio ejecuta el script
    Then la ejecución debe interrumpirse dentro de una tolerancia pequeña del límite
    And debe devolver el cuerpo original sin transformar
    And debe registrar un log WARN con "event": "script_timeout"
