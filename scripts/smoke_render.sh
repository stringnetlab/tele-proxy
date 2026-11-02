#!/bin/sh
# Smoke test del sidecar de render (browserless) que consumen /pv/ y el escalado de /mt/.
#
# Verifica el contrato de `src/infrastructure/preview/renderer.rs` contra un sidecar real:
#   GET  /pressure    -> 200 + JSON con "pressure" (lo usa `healthy()`)
#   POST /content     -> 200 + HTML renderizado (escalado de /mt/)
#   POST /screenshot  -> 200 + PNG (magic bytes)
#   POST /pdf         -> 200 + PDF (magic bytes)
#   y, si BROWSERLESS_TOKEN está definido, que /pressure sin token sea rechazado.
#
# Uso desde el host (sidecar con puerto publicado):
#   BROWSERLESS_URL=http://localhost:3000 BROWSERLESS_TOKEN=... sh scripts/smoke_render.sh
#
# Uso dentro del compose (el script se copia a /app/scripts en la imagen de tele-proxy):
#   docker compose exec tele-proxy sh /app/scripts/smoke_render.sh
#
# Variables:
#   BROWSERLESS_URL    obligatoria; el slash final se tolera
#   BROWSERLESS_TOKEN  opcional; debe coincidir con el TOKEN del sidecar
#   SMOKE_URL          página pública de prueba (default https://example.com/)
#   SMOKE_MARKER       texto que /content debe contener (default "Example Domain")
#   SMOKE_TIMEOUT_MS   deadline de cada render (default 15000)
#   SMOKE_ATTEMPTS     reintentos de espera inicial (default 10, 1 s entre intentos)
set -eu

BASE="${BROWSERLESS_URL:?Falta BROWSERLESS_URL (p. ej. http://browserless:3000)}"
BASE="${BASE%/}"
TOKEN="${BROWSERLESS_TOKEN:-}"
SMOKE_URL="${SMOKE_URL:-https://example.com/}"
SMOKE_MARKER="${SMOKE_MARKER:-Example Domain}"
SMOKE_TIMEOUT_MS="${SMOKE_TIMEOUT_MS:-15000}"
SMOKE_ATTEMPTS="${SMOKE_ATTEMPTS:-10}"

if [ -n "$TOKEN" ]; then QS="?token=$TOKEN"; else QS=""; fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT INT TERM

FAILED=0
pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1" >&2; FAILED=1; }

# Imprime el status HTTP; curl emite "000" si el transporte falla (conexión, DNS, timeout).
request() { # metodo ruta archivo_salida [payload]
    if [ -n "${4:-}" ]; then
        curl -sS -o "$3" -w '%{http_code}' -X "$1" \
            -H 'Content-Type: application/json' --data "$4" "$BASE$2$QS" 2>/dev/null || true
    else
        curl -sS -o "$3" -w '%{http_code}' -X "$1" "$BASE$2$QS" 2>/dev/null || true
    fi
}

printf 'Sidecar: %s\n' "$BASE"

# 1) /pressure: además de verificar el endpoint, sirve de espera de arranque.
attempt=1
while :; do
    code="$(request GET /pressure "$TMPDIR/pressure")"
    [ "$code" = "200" ] && break
    [ "$attempt" -ge "$SMOKE_ATTEMPTS" ] && break
    attempt=$((attempt + 1))
    sleep 1
done
if [ "$code" = "200" ] && grep -q '"pressure"' "$TMPDIR/pressure"; then
    pass "GET /pressure (intento $attempt)"
else
    fail "GET /pressure: status=$code (¿sidecar caído o TOKEN incorrecto?)"
fi

# 2) El token debe proteger el sidecar: sin él, /pressure no puede ser 200.
if [ -n "$TOKEN" ]; then
    plain="$(curl -sS -o "$TMPDIR/pressure-plain" -w '%{http_code}' "$BASE/pressure" 2>/dev/null || true)"
    case "${plain:-000}" in
        401 | 403) pass "GET /pressure sin token rechazado ($plain)" ;;
        *) fail "GET /pressure sin token devolvió ${plain:-000} (se esperaba 401/403)" ;;
    esac
fi

# 3) /content: la misma llamada que hace el escalado de /mt/.
payload_content="{\"url\":\"$SMOKE_URL\",\"timeout\":$SMOKE_TIMEOUT_MS,\"gotoOptions\":{\"waitUntil\":\"networkidle2\"},\"viewport\":{\"width\":800,\"height\":600}}"
code="$(request POST /content "$TMPDIR/content.html" "$payload_content")"
if [ "$code" = "200" ] && grep -qF "$SMOKE_MARKER" "$TMPDIR/content.html"; then
    pass "POST /content devuelve HTML con \"$SMOKE_MARKER\""
else
    fail "POST /content: status=$code o marcador ausente (SMOKE_URL=$SMOKE_URL)"
fi

# 4) /screenshot: PNG crudo, sin cabecera Content-Type fiable por parte del sidecar.
payload_shot="{\"url\":\"$SMOKE_URL\",\"timeout\":$SMOKE_TIMEOUT_MS,\"gotoOptions\":{\"waitUntil\":\"networkidle2\"},\"viewport\":{\"width\":800,\"height\":600},\"options\":{\"type\":\"png\",\"fullPage\":false}}"
code="$(request POST /screenshot "$TMPDIR/shot.png" "$payload_shot")"
magic=$(head -c 8 "$TMPDIR/shot.png" 2>/dev/null | od -An -tx1 | tr -d ' \n' || true)
if [ "$code" = "200" ] && [ "$magic" = "89504e470d0a1a0a" ]; then
    pass "POST /screenshot devuelve PNG ($(wc -c <"$TMPDIR/shot.png" | tr -d ' ') bytes)"
else
    fail "POST /screenshot: status=$code magic=${magic:-vacio} (se esperaba 89504e470d0a1a0a)"
fi

# 5) /pdf: el body debe empezar por la firma %PDF-.
payload_pdf="{\"url\":\"$SMOKE_URL\",\"timeout\":$SMOKE_TIMEOUT_MS,\"gotoOptions\":{\"waitUntil\":\"networkidle2\"},\"options\":{\"format\":\"A4\",\"printBackground\":true,\"landscape\":false}}"
code="$(request POST /pdf "$TMPDIR/render.pdf" "$payload_pdf")"
magic=$(head -c 5 "$TMPDIR/render.pdf" 2>/dev/null || true)
if [ "$code" = "200" ] && [ "$magic" = "%PDF-" ]; then
    pass "POST /pdf devuelve PDF ($(wc -c <"$TMPDIR/render.pdf" | tr -d ' ') bytes)"
else
    fail "POST /pdf: status=$code magic=${magic:-vacio} (se esperaba %PDF-)"
fi

if [ "$FAILED" -ne 0 ]; then
    printf '\nSmoke test FALLIDO\n' >&2
    exit 1
fi
printf '\nSmoke test OK: /pressure /content /screenshot /pdf\n'
