# Versión ejecutable de docs/BDD.md -> "Feature 3".
# Etiquetas: @domain = verificable en cualquier plataforma; @linux = requiere --features proxy en Linux.

Feature: Gestión de Caché y Memoria
  Como sistema
  Quiero cachear respuestas y limitar el consumo de memoria
  Para garantizar el rendimiento y prevenir ataques de agotamiento de recursos (OOM)

  @domain
  Scenario: Cache Miss y almacenamiento en Valkey
    Given el recurso no está en caché
    When el servicio obtiene una respuesta exitosa del origen
    Then debe serializar la respuesta (status, headers, body) con postcard
    And debe guardarla en Valkey con la clave "px:{internal_id}:{config_version}:{url_hash}"
    And debe aplicar el TTL definido en la estrategia de MIME
    And debe responder con "X-Cache: MISS"

  @linux
  Scenario: Cache HIT sin tocar el origen
    Given la clave "px:{internal_id}:{config_version}:{url_hash}" existe en Valkey
    When se solicita el mismo recurso
    Then debe responder 200 con el cuerpo cacheado
    And debe responder con "X-Cache: HIT"
    And no debe abrir conexión con el origen

  @domain
  Scenario: Invalidación de caché por versión de configuración
    Given la configuración del cliente cambia y su "config_version" incrementa
    When se realiza una nueva solicitud
    Then el servicio debe generar una nueva clave de caché en Valkey
    And NO debe ejecutar comandos de borrado masivo (SCAN/DEL) en Valkey

  @linux
  Scenario: Bypass de scripting por límite de tamaño de cuerpo
    Given el cliente tiene "max_scripting_body_bytes" configurado a 5 MB
    And el origen responde con un archivo de 15 MB
    When el servicio procesa la respuesta
    Then debe omitir la ejecución del script Lua
    And debe transmitir el cuerpo chunk a chunk desde upstream_response_body_filter
    And el primer byte debe llegar al cliente antes de que termine la descarga del origen
    And no debe acumular los 15 MB en memoria RAM del proceso
    And debe responder con "X-Cache: BYPASS" porque 15 MB supera el límite cacheable de 5 MB

  @linux
  Scenario: Origen que envía más de 100 MB
    Given el origen anuncia o envía un cuerpo que supera 100 MB
    When el contador de bytes del contexto excede el límite
    Then el servicio debe abortar la transmisión
    And debe responder con el error "payload_too_large"
    And no debe guardar nada en Valkey para esa URL
