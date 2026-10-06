# Versión ejecutable de docs/BDD.md -> "Feature 1".
# Si cambia uno de los dos, cambia el otro.
# Etiquetas: @domain = verificable en cualquier plataforma; @linux = requiere --features proxy en Linux.

Feature: Proxy Público Sin Autenticación
  Como usuario de una aplicación
  Quiero solicitar recursos externos a través de una URL pública corta
  Para que el proxy aplique las reglas de mi cuenta sin exponer mi identidad real

  @domain
  Scenario: Solicitud válida con crypt_id de 12 caracteres
    Given el servicio está en ejecución
    And existe un cliente con crypt_id "V1StGXR8_Z5j" y whitelist que incluye "shutterstock.com"
    When envío una solicitud GET a "/aq/V1StGXR8_Z5j/?url=https://shutterstock.com/img.jpg"
    Then el servicio debe procesar la solicitud sin pedir token de autorización
    And debe validar que el dominio está en la whitelist
    And debe devolver el recurso con HTTP 200 OK

  @domain
  Scenario: Crypt_id inválido o inexistente
    When envío una solicitud GET a "/aq/invalid_id_123/?url=https://shutterstock.com/img.jpg"
    Then el servicio debe responder con HTTP 404 Not Found
    And debe registrar un log de error con "event": "invalid_crypt_id"

  @domain
  Scenario: Dominio fuera de la whitelist
    Given el cliente con crypt_id "V1StGXR8_Z5j" solo tiene "shutterstock.com" en su whitelist
    When envío una solicitud GET a "/aq/V1StGXR8_Z5j/?url=https://malicious.com/payload.exe"
    Then el servicio debe responder con HTTP 403 Forbidden
    And debe registrar un log de error con "event": "domain_not_whitelisted"

  @linux
  Scenario: Rate limit excedido por crypt_id
    Given el cliente tiene un límite de 50 peticiones por minuto
    And ya ha realizado 50 peticiones en el último minuto
    When envío la petición 51
    Then el servicio debe responder con HTTP 429 Too Many Requests
    And debe incluir el header "Retry-After: 60"
    And debe cerrar el keep-alive de la conexión downstream
    And debe registrar un log de error con "event": "rate_limit_exceeded"

  @linux
  Scenario: Contador de rate limit atómico bajo concurrencia
    Given el cliente tiene un límite de 50 peticiones por minuto
    And varios workers de Pingora reciben peticiones concurrentes del mismo "crypt_id"
    When se procesan las peticiones
    Then el incremento en Valkey debe ser un único comando atómico (INCR con PEXPIRE solo en el primer incremento)
    And la cantidad total aceptada no puede exceder 50 por efecto de carrera

  @linux
  Scenario: Valkey indisponible al verificar el rate limit
    Given Valkey no responde
    When llega una petición
    Then el límite debe resolverse con el contador en-proceso (pingora-limits)
    And en modo degradado el tope por worker debe ser como máximo el configurado (nunca allow ilimitado)
    And debe registrar un log WARN con "event": "rate_limit_degraded"
    And la respuesta debe llevar la cabecera "X-Degraded-Mode: true"
