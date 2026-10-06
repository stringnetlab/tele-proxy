# Versión ejecutable de docs/BDD.md -> "Feature 2".
# Etiquetas: @domain = verificable en cualquier plataforma; @linux = requiere --features proxy en Linux.

Feature: Protección Anti-SSRF y DNS Seguro
  Como administrador del sistema
  Quiero que todas las resoluciones de red sean validadas estrictamente
  Para prevenir ataques de Server-Side Request Forgery (SSRF) y DNS Rebinding

  @domain
  Scenario: Resolución DNS segura con cadena de fallback
    Given el servicio está configurado con Quad9 ECS como primario
    When el servicio necesita resolver "shutterstock.com"
    Then debe usar DNS-over-TLS con validación DNSSEC
    And si Quad9 falla, debe hacer failover a Cloudflare Security, luego AdGuard, etc.

  @domain
  Scenario: Bloqueo de resolución a IP privada (SSRF)
    When el servicio intenta resolver "http://169.254.169.254/latest/meta-data/"
    Then debe detectar que la IP es privada o de metadatos
    And debe responder con HTTP 403 Forbidden
    And debe registrar un log de error con "event": "ssrf_blocked", "reason": "private_ip"

  @linux
  Scenario: Prevención de DNS Rebinding
    Given un dominio "attacker.com" resuelve a una IP pública en la primera consulta
    But intenta cambiar a una IP privada en la segunda consulta
    When el servicio procesa la solicitud
    Then upstream_peer debe construir el HttpPeer con la SocketAddr de la IP pública ya validada
    And el TLS debe dirigirse con sni = "attacker.com" y el header Host real debe conservarse
    And la conexión nunca debe re-resolver el nombre por el DNS del sistema

  @linux
  Scenario: El proxy no sigue redirects por sí mismo
    Given el origen responde 302 con "Location: https://cdn.valido.com/recurso.jpg"
    And el destino resuelve a una IP pública
    When el servicio procesa la respuesta
    Then debe reenviar el 302 y su Location al cliente sin perseguirlo
    And no debe emitir una segunda solicitud hacia el destino del redirect

  @linux
  Scenario: Bloqueo de redirect a IP privada
    Given el origen "https://attacker.com/redirect" devuelve un HTTP 302
    And el header "Location" apunta a "http://127.0.0.1/admin"
    When el servicio procesa el redirect
    Then debe validar la nueva URL contra las reglas anti-SSRF
    And debe abortar la solicitud con HTTP 403 Forbidden
    And debe eliminar el header Location de la respuesta al cliente
    And debe registrar un log de error con "event": "ssrf_blocked", "reason": "redirect_to_private_ip"

  @domain
  Scenario: URL con credenciales o esquemas no permitidos
    When envío una solicitud con "?url=ftp://user:pass@169.254.169.254/"
    Then el servicio debe responder con HTTP 400 Bad Request
    And debe registrar un log de error con "event": "invalid_url_format"
