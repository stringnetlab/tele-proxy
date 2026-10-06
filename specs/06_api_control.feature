# Versión ejecutable de docs/BDD.md -> "Feature 6".
# Etiquetas: @domain = verificable en cualquier plataforma; @linux = requiere --features proxy en Linux;
# @pending = requisito aún no implementado (falla hoy, debe pasar antes de cerrar la fase).

Feature: API de Control Segura
  Como administrador de la cuenta
  Quiero gestionar mi configuración a través de una API autenticada
  Para mantener el aislamiento total de mis datos

  @domain
  Scenario: Acceso sin token Bearer
    When envío una solicitud GET a "/api/v1/clients/config" sin header Authorization
    Then el servicio debe responder con HTTP 401 Unauthorized

  @domain
  Scenario: Acceso con token Bearer válido
    Given tengo un token Bearer válido asociado a mi "internal_id"
    When envío una solicitud GET a "/api/v1/clients/config" con "Authorization: Bearer <token>"
    Then el servicio debe responder con HTTP 200 OK
    And debe devolver SOLO mi configuración (incluyendo mi "crypt_id" actual)

  @domain
  Scenario: PUT de configuración con valores fuera de cota
    # `domain::validators::validate_config_update`, invocado desde `PUT /config` antes de tocar
    # CouchDB; el WARN con `event: invalid_config` lo emite `log_domain_error` al materializar la
    # respuesta. Cubierto por los tests de `validate_config_update` en `src/domain/validators.rs`.
    Given tengo un token Bearer válido
    When envío PUT /api/v1/clients/config con "rate_limit.max_requests = 0"
    Or con "rate_limit.window_seconds = 0"
    Or con un whitelist de más de 100 dominios
    Then el servicio debe responder 400 Bad Request
    And debe denegar el dominio de las cabeceras Host/Authorization y las IP privadas
    And no debe escribir el documento en CouchDB
    And debe registrar un log WARN con "event": "invalid_config"

  @domain
  Scenario: No existe alta de clientes por API
    # el provisionamiento de clientes es fuera de banda: se crea el documento en CouchDB
    When envío una solicitud POST a "/api/v1/clients"
    Then el servicio debe responder 404 (ruta inexistente)

  @domain
  Scenario: Rotación de crypt_id
    Given mi "crypt_id" actual es "V1StGXR8_Z5j"
    When envío una solicitud POST a "/api/v1/clients/rotate-id" con mi token
    Then el servicio debe generar un nuevo nanoid de 12 caracteres
    And debe invalidar el "crypt_id" anterior inmediatamente
    And debe registrar un log de auditoría con "event": "crypt_id_rotated"
