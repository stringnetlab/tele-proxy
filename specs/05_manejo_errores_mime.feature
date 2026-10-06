# Versión ejecutable de docs/BDD.md -> "Feature 5".
# Etiquetas: @domain = verificable en cualquier plataforma; @linux = requiere --features proxy en Linux;
# @pending = requisito aún no implementado (falla hoy, debe pasar antes de cerrar la fase).

Feature: Respuestas de Error Consistentes
  Como cliente
  Quiero recibir un recurso válido incluso si el origen falla
  Para evitar que se rompa el layout de mi aplicación (ej. etiqueta <img> rota)

  @linux
  Scenario: Fallback de imagen cuando el origen falla (modo wrapped)
    Given el cliente tiene "error_handling.mode" = "wrapped"
    And la URL solicitada termina en ".jpg" o el query param es "?mime=image/png"
    When el origen responde con HTTP 500 Internal Server Error
    Then el servicio debe interceptar el error
    And debe buscar el fallback en Valkey ("px:defaults:image/png") o usar el embebido
    # 200 y no 404: el objetivo declarado es que la etiqueta <img> no se rompa, y el navegador
    # descarta el cuerpo si el status no es exitoso. El estado real del origen queda en los logs.
    And debe responder con HTTP 200 y un Content-Type de imagen utilizable por <img>
    And debe marcar la respuesta con "X-Cache: FALLBACK"
    # Nivel 3 (embebido): para cualquier image/* el recurso embebido es un SVG, asi que el
    # Content-Type del placeholder es image/svg+xml cuando no hay entrada global en Valkey.

  @domain
  Scenario: Orden de prioridad para inferir el MIME del fallback
    Given el cliente solicita una URL
    Then el MIME del fallback se determina en este orden: ?mime= explícito, extensión de la URL, header Accept
    And si ninguna fuente aporta un MIME se usa application/octet-stream

  @domain
  Scenario: Prioridad de inferencia de MIME para fallbacks
    Given la URL es "https://origen.com/api/getAsset" (sin extensión)
    And la solicitud incluye "?mime=application/pdf"
    And el header "Accept" pide "image/*"
    When el origen responde con HTTP 500
    Then el servicio debe priorizar el parámetro "?mime=" sobre el header "Accept"
    And debe servir el fallback de PDF

  @linux @pending
  Scenario: Discrepancia entre MIME forzado y real
    Given el cliente solicita "?mime=text/html"
    But el origen responde exitosamente con "Content-Type: image/jpeg"
    When el servicio procesa la respuesta
    Then debe respetar el "Content-Type: image/jpeg" del origen
    And debe inyectar "X-Content-Type-Options: nosniff"
    And debe registrar un log WARN con "event": "mime_discrepancy"
