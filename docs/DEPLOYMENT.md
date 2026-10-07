# Guía de Despliegue

Motor de proxy: **Pingora** (Cloudflare). Toda esta guía asume el binario construido con el feature
`proxy`; las decisiones de API están en `docs/spec.md` §1-§5 y las firmas verificadas contra el rev
pineado están en `docs/ARCHITECTURE.md` y `docs/ERROR_DICTIONARY.md`.

## 0. Condición de plataforma (leer antes de desplegar)

Pingora es **Linux/Unix-only** (`epoll`, señales POSIX, `daemon()`):

1. `docker build` y cualquier `cargo build/check/clippy/test` **con** `--features proxy` corren en
   Linux. En Windows solo se trabaja con `default = []` (código independiente de plataforma).
2. El `Dockerfile` compila con `cargo build --release --locked --features proxy`. Sin el feature,
   `pingora-core`/`pingora-proxy` son dependencias `optional` y el binario resultante **no trae motor
   de proxy**: arranca pero no sirve `/aq/`.
3. TLS del upstream por el feature **`rustls`** (variante default). Los manifiestos de `pingora-core` y
   `pingora-proxy` declaran `default = []`, así que **ningún** backend TLS viene activado por defecto; sin
   `rustls` (o `openssl`/`boringssl`/`s2n`) las peticiones `https://` al origen fallan. `rustls` es la opción
   elegida: deja el runtime `debian:bookworm-slim` sin `libssl` y reutiliza el backend `ring` que ya
   trae `hickory-resolver` (`tls-ring`). Existe una variante opt-in `proxy-openssl` — ver la sección
   dedicada más abajo.
4. MSRV: `pingora-proxy` del rev pide **Rust 1.85**; `rust:bookworm` la cumple.

## Variante `proxy-openssl` (orígenes con JA3-blocking)

El backend TLS **default del binario es rustls**. Algunos orígenes (las properties de Meta:
Instagram, Facebook…) fingerprintan el JA3 de rustls como cliente no-navegador y bloquean:
responden `302` a `facebook.com/unsupportedbrowser`. Con OpenSSL esos mismos orígenes
devuelven `200` (verificado: el perfil de Instagram con el HTML real y sus metadatos OG).
Cloudflare con bot score estricto puede seguir bloqueando ambos fingerprints — ese límite
no lo resuelve ninguna de las dos variantes.

Para esos despliegues existe la feature hermana **`proxy-openssl`**: mismo binario y mismas
funciones, con reqwest sobre OpenSSL **vendored** (compilado estático desde `openssl-src`;
el runtime sigue sin `libssl`). Reglas:

- **Excluyentes**: `proxy` y `proxy-openssl` son features hermanas — no las actives a la vez.
  `--all-features` queda cubierto: `http_client::pinned_client` fuerza `use_native_tls()`
  cuando `proxy-openssl` está activa.
- **Default intacto**: la imagen del `Dockerfile` sigue construyéndose con `proxy` (rustls);
  nada del flujo documentado cambia si no usas la variante.
- **Coste de la variante**: build más lento (openssl-src compila OpenSSL en C; perl ya viene
  en la base `rust:bookworm`), dos pilas TLS en el binario (`ring` para hickory/pingora +
  OpenSSL para reqwest) y doble verificación al tocar `http_client`.

Construcción:

```bash
# Docker directo
docker build --build-arg PROXY_FEATURES=proxy-openssl -t tele-proxy:openssl .

# Compose (override con tag de imagen propio; mismos puertos, una variante a la vez)
docker compose -f docker-compose.yml -f docker-compose.openssl.yml up -d --build
```

En Dokploy: mismo repo y Dockerfile, añadiendo el build arg `PROXY_FEATURES=proxy-openssl`
(sección Build → Build Args de la app).

## Requisitos Previos

- Servidor **Linux** con Docker Engine y Docker Compose v2
- Dokploy configurado y funcionando
- Dominio apuntando al servidor (ej. `tele.velone.ai`)
- Certificado SSL (gestionado por Cloudflare o Let's Encrypt)
- `Cargo.lock` commiteado (la reproducibilidad del rev de Pingora la da el lock, no la rama `main`)

## Estructura del Repositorio

```
tele-proxy/
├── src/
│   ├── domain/                   # modelos, errores, validadores (sin pingora)
│   ├── application/              # ProxyService (impl ProxyHttp), lua_engine, cache_key, fallback
│   ├── infrastructure/           # couchdb_repo, valkey_cache, dns_resolver
│   ├── interfaces/               # control_api (axum), proxy_handler (deuda de migración)
│   ├── lib.rs
│   └── main.rs                   # arranca pingora_core::Server
├── config/
│   └── dns_resolvers.json        # cadena DoT+DNSSEC, embebida con include_str!
├── docs/                         # spec, ARCHITECTURE, DIAGRAMS, ENVIRONMENT, ERROR_DICTIONARY,
│                                 # RUST_STYLE_GUIDE, BDD, api_contract.yaml
├── specs/                        # 01_proxy_publico … 06_api_control (.feature, uno por Feature)
├── .dockerignore
├── .env.example                  # variables de entorno ejemplo (sin secretos reales)
├── .env                          # variables reales (NO commitear)
├── Cargo.toml / Cargo.lock
├── Dockerfile                    # multi-etapa rust:bookworm -> debian:bookworm-slim
├── docker-compose.yml            # tele-proxy + couchdb + valkey
└── README.md
```

No existe `specs/acceptance.feature` (hay un archivo `.feature` por Feature, generado desde
`docs/BDD.md`) ni directorio `tests/`: las pruebas unitarias van en `#[cfg(test)]` dentro de cada
módulo y los escenarios BDD viven en `specs/`.

## Configuración Inicial

### 1. Clonar el Repositorio

```bash
git clone https://github.com/stringnetlab/tele-proxy.git
cd tele-proxy
```

### 2. Configurar Variables de Entorno

```bash
cp .env.example .env
nano .env
```

La lista **canónica** de variables está en `docs/ENVIRONMENT.md`; ahí se marca cuáles están activas y
cuáles designadas. Las tres que **no tienen default** y hacen fallar el arranque si faltan:

```env
COUCHDB_USER=<usuario>
COUCHDB_PASSWORD=<generar_password_seguro>
VALKEY_PASSWORD=<generar_password_seguro>
```

No existe `RUST_ENV`: el entorno se distingue solo por `RUST_LOG`.

### 3. Generar Contraseñas Seguras

```bash
openssl rand -base64 32   # COUCHDB_PASSWORD
openssl rand -base64 32   # VALKEY_PASSWORD
```

Los valores reales **nunca** se escriben en este repositorio ni en la documentación: viven en el
`.env` del host (ignorado por git) y en el secret store de Dokploy. Aquí todo ejemplo los referencia
como `${COUCHDB_PASSWORD}` / `${VALKEY_PASSWORD}` o los lee desde el entorno del contenedor.

### 4. Threads y apagado ordenado (Pingora)

Cuatro variables mapean a campos de `ServerConf` (`docs/ENVIRONMENT.md`, *Runtime de Pingora*):

| Variable | Efecto |
| --- | --- |
| `PROXY_WORKER_THREADS` | `ServerConf.threads` — **default de Pingora: 1**. Si no se fija, el proxy es mono-thread aunque la máquina tenga 8 núcleos |
| `PROXY_WORK_STEALING` | `ServerConf.work_stealing` — un solo runtime compartido (`true`) o N runtimes mono-hilo |
| `PROXY_GRACE_PERIOD_SECONDS` | `ServerConf.grace_period_seconds` — ventana de `SIGTERM` para drenar conexiones |
| `UPSTREAM_KEEPALIVE_POOL_SIZE` | pool de conexiones reutilizadas al origen (campo declarado **inestable** por Pingora) |

> El proceso **no** se daemoniza: no se pasa `-d/--daemon` ni se pone `daemon: true`. Docker y
> Dokploy supervisan el proceso en primer plano; un proceso daemonizado rompe el `HEALTHCHECK` y el
> `stop_grace_period`. Tampoco se usa el YAML de `-c/--conf`: `Server::new_with_opt_and_conf` ignora
> esa ruta y la configuración se construye en código desde las variables anteriores.

Si se ajusta `PROXY_GRACE_PERIOD_SECONDS`, el compose debe darle margen: `stop_grace_period: 15s` por
encima del periodo de gracia, o Docker manda `SIGKILL` antes de que Pingora drene.

## Construcción de la Imagen

```bash
docker build -t tele-proxy .
docker images tele-proxy --format '{{.Size}}'
```

Objetivo de tamaño: **< 80 MB** con Pingora (el límite histórico de 50 MB pertenecía a la etapa
Axum+reqwest). La imagen de runtime solo instala `ca-certificates` y `curl`: **no** hay `nc`,
`bash`-tools ni clientes DNS, lo que importa en *Troubleshooting*.

## Despliegue con Docker Compose (Local)

```bash
docker compose up -d --build
docker compose logs -f tele-proxy
docker compose ps
```

## Creación de la Base de Datos de CouchDB (manual, obligatoria)

El servicio **no** autogenera la base: `ensure_design_doc()` (`src/main.rs`) crea el diseño
`_design/proxy_lookup`, pero ese `PUT` falla si la base no existe. Tras el primer arranque:

```bash
docker compose exec couchdb \
  sh -c 'curl -s -X PUT -u "$COUCHDB_USER:$COUCHDB_PASSWORD" \
         http://localhost:5984/tele_proxy_configs'
# -> {"ok":true}

docker compose exec couchdb \
  sh -c 'curl -s -u "$COUCHDB_USER:$COUCHDB_PASSWORD" http://localhost:5984/tele_proxy_configs'
# -> {"db_name":"tele_proxy_configs", ...}
```

Las credenciales se leen del entorno del propio contenedor (`COUCHDB_USER`/`COUCHDB_PASSWORD` las
inyecta el compose), así que ningún secreto queda escrito en la línea de comandos ni en el historial.

Si se cambia `COUCHDB_DB_NAME`, el `PUT` usa ese nombre.

## Despliegue con Dokploy

### 1. Conectar Repositorio

- Dokploy Dashboard → proyecto `tele-proxy` → repositorio GitHub `stringnetlab/tele-proxy` → rama `main`

### 2. Configurar Docker Compose

Dokploy detecta el `docker-compose.yml`. Verificar que **no** añade un segundo mapeo de puertos: el
`couchdb` del compose no publica `5984` en el host (ver *Seguridad Post-Despliegue*).

### 3. Variables de Entorno

Añadir en Dokploy las variables del `.env`, incluidas las tres contraseñas. Los valores quedan como
secrets del servicio, no en el repositorio.

### 4. Configurar Dominio

- Dominio del proxy: `tele.velone.ai` → Puerto: `HTTP_PROXY_PORT` (proxy público, el listener de Pingora).
- Dominio del panel: `admin.teleproxy.velone.ai` → Puerto: `HTTP_CONTROL_PORT`. **Subdominio, no path**:
  la app usa rutas absolutas (`/admin/`, `/api/v1/admin/...`) y Dokploy no strippea prefijos, así que
  un path tipo `/adm` rompe la UI aunque el puerto responda.
- Los dos puertos se publican en `0.0.0.0` (no loopback): el Traefik de Dokploy vive en otro
  contenedor y `127.0.0.1` del host no le sirve. El acceso directo `http://<ip-del-servidor>:8785`
  queda expuesto: protégelo en firewall (solo loopback + redes Docker) o acepta que la API admin
  (OAuth + rate limit) es el único filtro. El panel nunca se sirve por el puerto del proxy.
- HTTPS: habilitado (Cloudflare o Let's Encrypt). Fijar `GOOGLE_REDIRECT_URI` al callback con el
  esquema https del subdominio del panel: la cookie de sesión solo es `Secure` si el esquema es https.
- DNS: `A` de `admin.` apuntando al servidor (puede ir proxied por Cloudflare).

### 5. Desplegar

Deploy → Dokploy construye la imagen y levanta los servicios.

## Verificación Post-Despliegue

### 1. Verificar Servicios

```bash
docker compose ps
# tele-proxy   Up (healthy)
# couchdb      Up (healthy)
# valkey       Up (healthy)
```

### 2. Verificar Health (ambos puertos)

```bash
curl -f http://localhost:8080/health   # listener de Pingora  -> OK
curl -f http://localhost:8081/health   # API de control (axum) -> OK
```

`/health` vive en los dos puertos y no requiere Bearer. En el 8080 lo resuelve `request_filter`
**antes** de validar `crypt_id`: es el endpoint que usa el `HEALTHCHECK` del `Dockerfile`, y si se
tratara como petición de proxy siempre devolvería `400`/`404` y el contenedor se marcaría unhealthy.
Solo informa que el proceso atiende: no comprueba CouchDB ni Valkey, porque un `503` aquí derribaría
el listener de un proxy que degradó a la config default y sigue sirviendo MISS.

### 3. Verificar CouchDB y Valkey

```bash
curl http://localhost:5984/_up            # {"status":"ok"} — solo desde el host o la red interna
docker compose exec valkey sh -c 'valkey-cli -a "$VALKEY_PASSWORD" ping'   # PONG
```

### 4. Aprovisionar un Cliente de Prueba

**No existe `POST /api/v1/clients`**: los clientes se provisionan escribiendo su documento en CouchDB
(contrato en `docs/spec.md` §3 y endpoints en `docs/api_contract.yaml`).

```bash
# 1) generar crypt_id (12 chars URL-safe) y token bearer en el shell, no en el repositorio
#    (requiere coreutils + openssl; jq solo hace falta para el paso 5)
CRYPT_ID=$(LC_ALL=C tr -dc 'A-Za-z0-9_-' </dev/urandom | head -c 12)
TOKEN="sk_live_$(openssl rand -hex 20)"
HASH="sha256:$(printf '%s' "$TOKEN" | sha256sum | cut -d' ' -f1)"

# 2) escribir el documento
docker compose exec -T couchdb sh -c \
  'curl -s -X PUT -u "$COUCHDB_USER:$COUCHDB_PASSWORD" \
     -H "Content-Type: application/json" \
     "http://localhost:5984/tele_proxy_configs/client_int_test" \
     -d "{\"_id\":\"client_int_test\",\"type\":\"client_config\",\
\"internal_id\":\"client_int_test\",\"crypt_id\":\"'"$CRYPT_ID"'\",\
\"bearer_token_hash\":\"'"$HASH"'\",\"config_version\":1,\
\"whitelist\":[\"example.com\"],\
\"rate_limit\":{\"max_requests\":50,\"window_seconds\":60}}"'
```

Comprobar que el feed de invalidación lo recoge: en los logs debe aparecer el bump de
`config_version` tras el `PUT` (`GET /api/v1/clients/config` devuelve la config leída de CouchDB).

### 5. Verificar el Proxy End-to-End

```bash
URL=$(printf 'https://example.com/logo.png' | jq -sRr @uri)
curl -i "https://tele.velone.ai/aq/${CRYPT_ID}/?url=${URL}"
```

Cabeceras esperadas en un MISS correcto:

- `200` del origen reenviado
- `X-Cache: MISS`, y `HIT` en la segunda petición de la misma URL
- `X-Content-Type-Options: nosniff` y `X-Proxy-By: tele.velone.ai` (las añade `response_filter`,
  **después** de la decisión de caché)

```bash
curl -i "https://tele.velone.ai/aq/${CRYPT_ID}/?url=${URL}" | grep -i '^x-cache'   # HIT
```

### 6. Verificar Bloqueos y Rate Limit

```bash
# dominio fuera de whitelist -> 403 con cuerpo JSON {error: ...}
curl -i "https://tele.velone.ai/aq/${CRYPT_ID}/?url=$(printf 'https://no-permitido.test/a.jpg' | jq -sRr @uri)"

# crypt_id malformado (11 caracteres) -> 400
curl -i "https://tele.velone.ai/aq/CORTESINO11/?url=${URL}"

# URL a IP privada / metadatos -> 403 ssrf_blocked, nunca una conexión al origen
curl -i "https://tele.velone.ai/aq/${CRYPT_ID}/?url=$(printf 'http://169.254.169.254/latest/meta-data/' | jq -sRr @uri)"

# superar el límite -> 429 con Retry-After
for i in $(seq 1 60); do curl -s -o /dev/null -w '%{http_code} ' \
  "https://tele.velone.ai/aq/${CRYPT_ID}/?url=${URL}"; done; echo
curl -si "https://tele.velone.ai/aq/${CRYPT_ID}/?url=${URL}" | grep -i '^retry-after'
```

## Monitoreo

### Logs

El suscriptor emite **JSON** (`main.rs` → `fmt().json()`), con `with_target(false)` y sin
file/line. Por tanto el filtro es por el campo `"level"`, **no** por `level=ERROR` (ese formato es
del capas de texto, no del nuestro):

```bash
docker compose logs -f tele-proxy

# solo errores
docker compose logs --no-log-prefix tele-proxy | grep '"level":"ERROR"'

# igual con jq, agrupando el código del diccionario
docker compose logs --no-log-prefix tele-proxy \
  | jq -r 'select(.level=="ERROR" or .level=="WARN") | [.level, .event, .crypt_id] | @tsv' \
  | sort | uniq -c | sort -rn
```

Los campos de cada evento están definidos en `docs/ERROR_DICTIONARY.md`; `crypt_id`, `internal_id`,
`config_version` e `ip_address` vienen del span de la petición y los adjunta `request_filter`.

### Métricas (Futuro)

- Implementar Prometheus exporter. El rev pineado incluye la crate `pingora-prometheus` en el
  workspace de Pingora; **no** está declarada en `Cargo.toml`, así que hoy no se exporta nada.
- Métricas clave: request rate por `crypt_id`, cache hit/miss/BYPASS/FALLBACK, error rate por
  `event`, latencia p50/p95/p99, uso del pool keepalive del upstream.

## Actualizaciones

### Actualizar Código

```bash
git pull origin main
docker compose up -d --build
```

Dokploy hace esto automáticamente si se configura CI/CD.

### Actualizar Configuración DNS

```bash
nano config/dns_resolvers.json     # prioridad, IP, server_name, dnssec, ecs
docker compose up -d --build       # el JSON está embebido con include_str!: hay que re-construir
```

### Subir el Rev de Pingora

`Cargo.toml` declara el commit con `rev = "4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19"`, así que subir
el rev es editar esa línea **cuatro veces** (`pingora-core`, `pingora-proxy`, `pingora-http`,
`pingora-limits`) — tienen que coincidir, porque los crates del workspace se referencian entre sí y
dos revs distintos en el mismo grafo producen dos copias de `pingora-core` con tipos incompatibles.
Al moverlo:

1. Volver a resolver el lock **en Linux y en línea**, y leer el diff. Sirve cualquiera de las dos
   formas, ambas verificadas:
   ```bash
   cargo update -p pingora-core -p pingora-proxy    # sólo el subgrafo Pingora
   cargo metadata --format-version 1 > /dev/null     # re-resuelve el grafo completo
   ```
   **No** usar `--offline`: la resolución de las crates del workspace Pingora necesita el índice de
   crates.io para los crates que el lock aún no conoce y aborta con
   `error: no matching package named `ouroboros` found` (verificado con `cargo metadata --offline`,
   RC=101). El `git db` local sí basta para el commit, pero no para el índice.
2. Re-verificar contra el nuevo checkout las firmas citadas en `spec.md`/`ARCHITECTURE.md`/
   `ERROR_DICTIONARY.md` (números de línea de `proxy_trait.rs`, `FailToProxy`, los filtros `async`).
3. Revisar los campos declarados inestables (`upstream_keepalive_pool_size`).
4. No mezclar: los hechos documentados están verificados contra el rev pineado, **no** contra
   crates.io 0.9.0. En crates.io los filtros de cuerpo son síncronos y `Session` no es genérica;
   cambiar de fuente exige reescribir esos bloques.

Con `rev` explícito **no** hay forma de que un `cargo update` sin argumentos mueva Pingora por
accidente: `branch = "main"` sí lo permitía, y ese era el riesgo que esta forma de pinear cierra.

Estado actual del lock (regenerado 2026-10-02): las 12 entradas `pingora-*` usan
`?rev=4487f7b2ab50f159e4a2cf4f6a6b813f61bb6e19`, y ya aparecen `pingora-limits` (declarado) y
`pingora-rustls` (lo trae `features = ["rustls"]`). Efecto colateral que hay que saber leer: la
regeneración **unificó `zstd` en 0.13.3** y eliminó la copia 0.14.0 que había antes. No es una
regresión —`pingora-core/Cargo.toml:70` pide `zstd = "0"` (cualquier 0.x) y
`pingora-header-serde/Cargo.toml:25` pide `"0.13.1"`, así que 0.13.3 satisface ambas con un solo
`zstd-sys` compilado donde antes se compilaban dos. Si un diff de lock muestra ese salto de `zstd`,
es correcto; lo que debe hacer saltar la alarma es un cambio en el hash `4487f7b2…`.

## Backup

### CouchDB

```bash
# Backup manual (credenciales desde el entorno del contenedor)
docker compose exec couchdb sh -c \
  'curl -s -u "$COUCHDB_USER:$COUCHDB_PASSWORD" \
     "http://localhost:5984/tele_proxy_configs/_all_docs?include_docs=true"' > backup.json

# Restaurar en una base recién creada
docker compose exec couchdb \
  sh -c 'curl -s -X PUT -u "$COUCHDB_USER:$COUCHDB_PASSWORD" http://localhost:5984/tele_proxy_configs'
jq '{docs: [.rows[] | .doc]}' backup.json > bulk.json
docker compose exec -T couchdb sh -c \
  'curl -s -X POST -u "$COUCHDB_USER:$COUCHDB_PASSWORD" -H "Content-Type: application/json" \
     -d @- http://localhost:5984/tele_proxy_configs/_bulk_docs' < bulk.json
```

`_bulk_docs` reemplaza documentos con el mismo `_id` conservando sus ` _rev` solo si el backup los
incluye; si la restauración devuelve `conflict`, usar la replicación (`_replicator`) en su lugar.

### Valkey

```bash
# Las claves px:* son caché y se reconstruyen solas: no requieren backup.
# PERO px:defaults:{mime} (Nivel 2 de la cadena de fallback) NO tiene TTL y es dato operativo:
# si se pierde, el fallback global por MIME desaparece hasta que se vuelva a cargar.
docker compose exec valkey sh -c 'valkey-cli -a "$VALKEY_PASSWORD" --no-auth-warning BGSAVE'
docker compose cp valkey:/data/dump.rdb ./backups/valkey-$(date +%F).rdb
```

## Troubleshooting

### Problema: CouchDB no inicia

```bash
docker compose logs couchdb
# Solución común: permisos del volumen
docker compose down
sudo chown -R 5984:5984 couchdb_data
docker compose up -d
```

### Problema: el diseño `_design/proxy_lookup` no existe

El arranque lo intenta y si falla solo avisa (`No se pudo crear el design doc de CouchDB (se reintentará en
la primera petición)`). Causa casi siempre: la base no está creada todavía → ver
*Creación de la Base de Datos*.

### Problema: Valkey no acepta conexiones

```bash
grep '^VALKEY_PASSWORD=' .env          # comprobar que la variable existe
docker compose exec valkey sh -c 'valkey-cli -a "$VALKEY_PASSWORD" --no-auth-warning ping'
```

Si Valkey está caído el proxy **degrada**: la caché se salta y el rate limit pasa al limiter local
`pingora-limits` con un `event: rate_limit_degraded` en WARN. No es un fallo del proxy.

### Problema: Proxy devuelve 502

```bash
docker compose ps                                   # couchdb/valkey healthy?
docker compose logs --no-log-prefix tele-proxy | grep '"level":"ERROR"' | tail -20
```

Causas: CouchDB o Valkey inaccesibles, config inválida, o el origen no respondió. En Pingora hay que
distinguir el `502` **propio** del reenviado desde el origen:

- El `502` generado por `fail_to_proxy` sale del default de Pingora cuando el servicio no escribe
  cuerpo: **sin** `Content-Type`, `content-length: 0`, `cache-control: private, no-store` y
  `Connection: close` forzado (la conexión del cliente no se reutiliza). El cuerpo JSON del
  diccionario solo aparece cuando el servicio lo escribe explícitamente.
- Si la cabecera `X-Proxy-By` está presente, la respuesta pasó por nuestros filtros.

### Problema: 400/403/404 en peticiones que "deberían" funcionar

Todo el control de acceso vive en `request_filter` (`early_request_filter` queda sin implementar):

- `400` — `crypt_id` con formato inválido o `url` malformada (credenciales, fragmento, scheme no http/https)
- `403` — dominio fuera de whitelist o `ssrf_blocked` (IP privada/loopback/enlace-local tras la
  resolución DoT+DNSSEC)
- `404` — `crypt_id` válido pero inexistente en CouchDB, o ruta que no es `/aq/{crypt_id}/` ni `/health`
- `429` — límite superado; `Retry-After` indica la ventana real

### Problema: un núcleo al 100 % y el resto parado

`ServerConf.threads` vale **1** por defecto en Pingora. Fijar `PROXY_WORKER_THREADS` (≈ número de CPU
asignadas) en el `.env`/compose y recrear el contenedor. Es la trampa más frecuente tras el despliegue.

### Problema: el contenedor tarda en morir o corta conexiones

`SIGTERM` inicia el drenado de Pingora durante `grace_period_seconds`. Si Docker manda `SIGKILL`
antes, subir `stop_grace_period` del compose por encima de `PROXY_GRACE_PERIOD_SECONDS`. Y verificar
que nadie pasó `-d/--daemon`: un daemon dentro del contenedor deja el `HEALTHCHECK` en falso.

### Problema: DNS over TLS falló

La imagen de runtime **no** trae `nc` (solo `ca-certificates` y `curl`), así que
`docker compose exec tele-proxy nc -zv …` no funciona y no hay que intentarlo. Dos alternativas:

```bash
# a) contenedor desechable en la red del compose (el nombre es <proyecto>_tele-net;
#    <proyecto> = el directorio del compose, o el valor de -p / COMPOSE_PROJECT_NAME)
RED=$(basename "$(pwd)")_tele-net
docker run --rm --network "$RED" alpine sh -c \
  'for h in 9.9.9.11 1.1.1.3 94.140.14.140 185.228.168.9 8.8.8.8; do nc -zv -w 3 "$h" 853; done'

# b) desde el host
nc -zv -w 3 9.9.9.11 853 && nc -zv -w 3 1.1.1.3 853
```

Si falla: firewall del servidor (salida TCP 853), o el host no tiene ruta IPv6 — los resolvers DoT
devuelven `AAAA` para sitios tras CDN y el `HttpPeer` debe construirse con `SocketAddr` IPv4 salvo
que haya salida v6 real (`docs/spec.md` §5).

### Problema: `panic: wrong phase` en el arranque o bajo carga

Alguien tocó una API del módulo de caché de Pingora (`HttpCache::set_max_file_size_bytes`,
`track_body_bytes_for_max_file_size`, `exceeded_max_file_size`) sin haber llamado a
`HttpCache::enable(..)`. La decisión registrada (`docs/spec.md` Fase 3, `docs/ARCHITECTURE.md`) es
**no** habilitar `HttpCache`: la persistencia es la capa propia sobre Valkey. Con la fase en
`Disabled`, esos métodos hacen `panic!`/`assert!` por diseño.

### Problema: la compilación falla en Windows con el feature `proxy`

Esperado: Pingora no compila en Windows. Trabajar en Windows con `default = []` y ejecutar
`fmt`/`clippy`/`test`/`build` del feature en `rust:bookworm`.

## Seguridad Post-Despliegue

### Checklist

- [ ] `COUCHDB_USER`, `COUCHDB_PASSWORD` y `VALKEY_PASSWORD` generados (no los del `.env.example`)
- [ ] `5984` (CouchDB) y `6379` (Valkey) **sin** mapeo de puertos público
- [ ] `HTTP_CONTROL_PORT` del panel alcanzable por el subdominio de Dokploy y, si el host tiene IP
  pública, restringido en firewall al loopback + redes Docker (el puerto queda publicado en
  `0.0.0.0` porque Traefik no puede usar el `127.0.0.1` del host); `HTTP_PROXY_PORT` detrás del
  TLS de Dokploy
- [ ] Base `tele_proxy_configs` creada y `_design/proxy_lookup` presente
- [ ] `PROXY_WORKER_THREADS` fijado explícitamente
- [ ] Proceso **no** daemonizado; `stop_grace_period` >= `PROXY_GRACE_PERIOD_SECONDS`
- [ ] Firewall (solo puertos 80, 443, 22) y fail2ban
- [ ] Actualizaciones automáticas de seguridad habilitadas
- [ ] Backup automático de CouchDB y de `px:defaults:{mime}`
- [ ] Tokens Bearer rotados periódicamente; `POST /api/v1/clients/rotate-id` si se filtró un `crypt_id`
- [ ] Logs revisados semanalmente (`"level":"ERROR"` / `"level":"WARN"`)

### Hardening del Servidor

```bash
# Deshabilitar root login SSH
sudo nano /etc/ssh/sshd_config      # PermitRootLogin no

sudo apt install fail2ban
sudo systemctl enable fail2ban

sudo ufw allow 22/tcp
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw enable
```

## Soporte

- Documentación: `docs/` (`spec.md` es la especificación; `ENVIRONMENT.md` la lista de variables)
- Divergencias código ↔ documentación: tabla 0 de `docs/DIAGRAMS.md`
- Issues: https://github.com/stringnetlab/tele-proxy/issues
- Logs: `docker compose logs tele-proxy`
