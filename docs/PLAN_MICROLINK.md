# PLAN_MICROLINK — Aprendizajes de microlinkhq aplicables a TeleProxy

Plan derivado del análisis a fondo de cuatro repositorios de microlinkhq:
[`html-get`](https://github.com/microlinkhq/html-get),
[`browserless`](https://github.com/microlinkhq/browserless),
[`metascraper`](https://github.com/microlinkhq/metascraper) y
[`skills`](https://github.com/microlinkhq/skills).

El patrón común que los une — y que a TeleProxy le falta — es una filosofía:
**cascadas de reglas con validador post-candidato, presupuesto de tiempos fraccionado y
degradación elegante sin romper nunca la respuesta**.

## Qué aporta cada repo

| Repo | Idea central extraída |
|---|---|
| **html-get** | Estrategia escalonada fetch→prerender con *puerta de usabilidad* (solo se escala si la alternativa es realmente válida); formato decidido por content-type con candidatos de la cadena de redirects; saneado de subrecursos locales en el body; fail-soft de capacidades opcionales |
| **browserless** | Detección de recursos zombis con deadline corto propio (no heredar el timeout largo del driver); circuit breaker por retryabilidad tipada de errores; respawn single-flight con verificación de identidad; timeouts jerárquicos por fracciones; precompilación del programa con solo un snippet inyectado |
| **metascraper** | Pipeline `findRule`: primera regla cuyo candidato **pasa un validador** gana; precedencia explícita global→cliente→dominio; gate por dominio memoizado; caché negativa; verificación de MIME con sniff de 1 byte; corpus de fixtures HTML por dominio como regresión |
| **skills** | Conversión de docs/prompt de integración en skills para agentes con frontmatter y disparadores; progressive disclosure; diccionario de errores en formato "código → qué hacer"; CI que valida el skill contra `docs/api_contract.yaml` |

## Plan por fases

### Fase A — Data plane: resiliencia hacia el origen

Integrar en la portación a Pingora en curso (Fase 3 de `docs/spec.md`) para no duplicar trabajo.

1. **Presupuesto de timeout fraccionado** (`src/application/proxy_service.rs`): deadline
   global de 30 s repartido por fases (DNS, conexión, TTFB, stream), como `goto.timeouts` de
   browserless (base 2/3, acciones 1/11).
2. **Circuit breaker por origen con errores tipados retryables/permanentes**
   (`src/infrastructure/http_client.rs`): normalizar fallos de transporte como
   `@browserless/errors` y abrir circuito por host tras N fallos transitorios; hoy todo 5xx de
   origen va igual al fallback.
3. **Deadline corto en la adquisición del keepalive pool**: detectar conexiones zombis del pool
   (`UPSTREAM_KEEPALIVE_POOL_SIZE`) antes de heredar timeouts largos — patrón
   `CREATE_CONTEXT_TIMEOUT` de browserless.

### Fase B — Rendimiento: whitelist, gates y caché

4. **Whitelist compilada y memoizada por `(host, cliente)`** (`src/domain/validators.rs` +
   caché Moka existente): evaluar el matching sufijo una vez por host y cachear, con **caché
   negativa** (centinela) para hosts no permitidos — patrón `rules.test` memoizado de
   metascraper.
5. **Sniff de content-type con lectura de 1 byte / magic bytes** antes de fijar TTL por MIME y
   elegir fallback (`src/application/fallback.rs`): hoy se depende del `Content-Type` declarado
   y del hint `mime`; validar contra los primeros bytes (metascraper descarta "HTML disfrazado
   de imagen"; html-get hace lo mismo con PDFs).

### Fase C — Lua: coste de arranque y reciclaje

6. **Precompilar el chunk del cliente y reutilizar la VM** (`src/application/lua_engine.rs`):
   en `mlua`, compilar una vez por `(config_version, code_hash)` e inyectar solo el cuerpo del
   script en un patrón envoltorio (como el `SLOT` de `isolated-function` de browserless);
   reciclar la VM solo bajo fault, con watchdog — evita recompilar y respawn por petición.
7. **Fail-soft de capacidades**: si el init de la VM o una capacidad opcional falla al arrancar,
   probe memoizado y servir en bypass por petición en vez de propagar 500.

### Fase D — Seguridad: fallbacks y saneado de bodies

8. **Puerta de usabilidad en la cadena de fallback** (`src/application/fallback.rs`): un
   fallback solo reemplaza la respuesta si es *realmente válida* (status < 400, content-type
   coherente, hash); evitar servir "cáscaras de bloqueo" como html-get evita el shell vacío de
   Chromium.
9. **Saneado de subrecursos privados en bodies HTML reescritos**: el anti-SSRF valida la
   resolución DNS, pero un body servido puede contener `src/href/url()` apuntando a
   `localhost`/IPs internas — añadir el rewriter equivalente a `isLocalAddress → remove` de
   html-get en el pipeline post-respuesta (encaja natural en `header_rules`/Lua como regla
   global).
10. **Egress allowlist con `host:port` explícito** (puertos por defecto, IPv6) en el validador
    de `fallback_urls` y webhooks: browserless resuelve SSRF a nivel de puerto, no solo de host.

### Fase E — Observabilidad y decisión

11. **`stats` estructurados por petición**: latencia por fase y origen de la decisión al log
    `tracing` (el `X-Cache` HIT/MISS/FALLBACK/BYPASS ya cubre el modo servido) + **percentiles
    p95/p99 por cliente** en Valkey, expuestos en el panel admin PHP (slice `ClientManagement`
    de admin-teleproxy).
12. **Precedencia de reglas global→cliente→dominio** con merge explícito (metascraper
    `mergeRules`): aplicar a `header_rules` y `scripting.expression` para permitir *overrides*
    por dominio dentro del mismo cliente, no solo reemplazo. Extensión de modelo en
    `src/domain/header_rules.rs` — la más ambiciosa; tratar como fase independiente con diseño
    previo.

### Fase F — DX para agentes (barato y de alto impacto)

13. Convertir `docs/INTEGRATION_PROMPT.md` en `.agents/skills/teleproxy/SKILL.md` con
    frontmatter y disparadores ("header_rules, lua script, crypt_id, teleproxy…") y progressive
    disclosure hacia `HEADER_RULES.md`, `LUA_SCRIPTING.md` y `ERROR_DICTIONARY.md` como
    referencias enlazadas; reformatear el diccionario de errores como tabla
    "código → qué hacer"; recetas copiables por workflow (rotar token, alta de cliente) en
    `common-workflows/`; check de CI que valide el skill contra `docs/api_contract.yaml`.

### Fase G — Regresión

14. **Corpus de fixtures HTML reales por dominio** (estilo los ~90 fixtures de metascraper)
    como golden tests de `header_rules`, reescritura de redirects y detección de MIME —
    especialmente valioso para Shutterstock/Getty, los dominios reales de los clientes.

## Orden de ejecución sugerido

- **Primero** (rápido/seguro, no interfiere con la portación a Pingora): F-13 y B-4.
- **Integrar en la Fase 3 actual** (Pingora): A-1, A-2, A-3.
- **Después**: C (6–7), D (8–10), E-11. E-12 merece diseño independiente.
- **G-14** acompaña a cada fase conforme se estabiliza cada comportamento.

## Referencias del análisis

- html-get: `src/index.js` (escalada con puerta de usabilidad), `src/html.js`
  (`isLocalAddress`), `src/office.js` (resolución de formato en 3 niveles), `src/util.js`
  (caché memoizada de content-type), `scripts/postinstall` (`auto-domains.json` ordenado por
  tráfico).
- browserless: `packages/browserless/src/index.js` (zombie detection, respawn single-flight),
  `driver.js` (close escalonado, kill por grupo), `goto.timeouts` (presupuesto fraccionado),
  `@browserless/errors` (retryabilidad tipada), `isolated-function` (programa precompilado con
  `SLOT`), `function.js` (egress allowlist host:port).
- metascraper: `packages/metascraper/src/get-data.js` + `src/index.js` (pipeline `findRule`,
  merge con precedencia), `packages/metascraper-helpers/src/index.js` (gate memoizado,
  validadores por `propName`), `metascraper-logo-favicon` (verificación Content-Type con
  `maxBody: 1`, caché negativa).
- skills: catálogo plano `SKILL.md` con frontmatter/disparadores, progressive disclosure,
  `.agents/skills/` como ubicación canónica, tabla de errores con columna "qué hacer",
  feedback loop `.well-known/agent-feedback.json`.
