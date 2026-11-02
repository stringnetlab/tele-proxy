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

  @linux
  Scenario: Discrepancia entre MIME forzado y real
    Given el cliente solicita "?mime=text/html"
    But el origen responde exitosamente con "Content-Type: image/jpeg"
    When el servicio procesa la respuesta
    Then debe respetar el "Content-Type: image/jpeg" del origen
    And debe inyectar "X-Content-Type-Options: nosniff"
    And debe registrar un log WARN con "event": "mime_discrepancy"

  @domain
  Scenario: Los magic bytes corrigen el TTL de una respuesta que miente
    Given el origen responde con "Content-Type: image/jpeg"
    But el cuerpo empieza con "<!DOCTYPE html"
    When el servicio almacena la respuesta en caché
    Then el TTL se calcula con el MIME detectado ("text/html"), no con el declarado
    And debe registrar un log WARN con "event": "mime_sniff_mismatch"

  @domain
  Scenario: HTML disfrazado de imagen en modo wrapped
    Given el cliente tiene "error_handling.mode" = "wrapped"
    And espera un recurso binario por "?mime=image/jpeg" o por la extensión ".jpg"
    When el origen responde 200 con un cuerpo HTML
    Then el servicio debe descartar la respuesta del origen
    And debe servir el fallback de imagen con "X-Cache: FALLBACK"
    And no debe reutilizar un fallback stale o global cuyo cuerpo contradiga el MIME esperado

  @domain
  Scenario: Fallback configurado válido se sirve y se cachea por cliente
    Given el cliente tiene "error_handling.fallback_urls" con la entrada "image/png"
    And la entrada declara un "hash" sha256 que coincide con el cuerpo remoto
    When el origen falla y no hay stale utilizable
    Then el servicio obtiene la URL del fallback con el mismo pipeline anti-SSRF (sin redirects)
    And valida que el status sea 2xx, el cuerpo no esté vacío y los magic bytes coincidan con "image/png"
    And sirve la respuesta con "X-Cache: FALLBACK" y "X-Fallback-Source: configured"
    And la cachea bajo "px:{internal_id}:{config_version}:fallback:{sha256 del mime}"
    And registra un log INFO con "event": "fallback_served" y "source": "configured"

  @domain
  Scenario: Fallback configurado que no supera las puertas se descarta
    Given el cliente tiene "error_handling.fallback_urls" con la entrada "image/png"
    When la URL configurada responde 500, un cuerpo vacío, un MIME contradictorio o un hash distinto
    Then el servicio descarta esa respuesta y registra un log WARN con "event": "fallback_configured_rejected"
    And continúa con el siguiente nivel de la cascada ("px:defaults:{mime}" o el embebido)

  @domain
  Scenario: Prioridad de patrones en "fallback_urls"
    Given el cliente declara entradas para "image/png", "image/*" y "*/*"
    When se busca el fallback de un recurso "image/png"
    Then se elige la entrada de "image/png"
    And si no existe, se elige la de "image/*"
    And si tampoco existe, se elige la de "*/*"
    And sin MIME esperado solo es aplicable "*/*"

  @domain
  Scenario: Fallback remoto rechazado por el validador de URL
    Given una entrada de "fallback_urls" apunta a una IP privada, a un esquema no http(s) o incluye credenciales
    When el servicio intenta obtenerla
    Then no se realiza la petición
    And se registra "fallback_configured_rejected" con el motivo
    And se continúa con el siguiente nivel de la cascada
