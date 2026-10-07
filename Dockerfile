FROM rust:bookworm AS builder

WORKDIR /app

# cmake/pkg-config: los que pide el build de dependencias C (mlua vendored compila Lua 5.4;
# la variante proxy-openssl añade openssl-src, que además necesita perl — incluido en la base
# buildpack-deps de rust:bookworm). libssl-dev NO lo necesita ninguna variante (rustls es
# puro Rust; native-tls-vendored compila OpenSSL estático), pero se mantiene en la etapa de
# build porque no altera el tamaño de la imagen final y evita romper el caché del builder.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    cmake \
    && rm -rf /var/lib/apt/lists/*

# Variante del binario: "proxy" (default, rustls) o "proxy-openssl" (OpenSSL vendored, para
# orígenes que fingerprintan el JA3 de rustls — docs/DEPLOYMENT.md, "Variante proxy-openssl").
ARG PROXY_FEATURES=proxy

COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs
# --locked: Cargo.lock tiene que casar con el `rev = "4487f7b2…"` que declara Cargo.toml
# (RUST_STYLE_GUIDE.md §8). Si al regenerar el lock ese hash cambia, se subió Pingora sin
# re-verificar las firmas citadas en docs/. El builder necesita red: la dependencia es git.
# Sin --features: sin el feature las crates de pingora son `optional` y el binario
# saldría sin motor de proxy (escucharía 8080/8081 pero no serviría /aq/).
RUN cargo build --release --locked --features ${PROXY_FEATURES}
RUN rm -rf src

COPY src/ ./src/
COPY config/ ./config/
RUN touch src/main.rs src/lib.rs
RUN cargo build --release --locked --features ${PROXY_FEATURES}

FROM debian:bookworm-slim AS runtime

# Solo ca-certificates + curl: curl lo usa el HEALTHCHECK. A cambio la imagen final no trae
# `nc` ni clientes DNS: las pruebas de conectividad DoT se hacen desde el host o con un
# contenedor desechable (docs/DEPLOYMENT.md, Troubleshooting).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN useradd -r -s /bin/false teleproxy

WORKDIR /app

COPY --from=builder /app/target/release/tele-proxy /app/tele-proxy
COPY --from=builder /app/config /app/config

RUN chown -R teleproxy:teleproxy /app

USER teleproxy

# 8080 = proxy /aq/ (objetivo: add_tcp de Pingora; hoy Axum — docs/spec.md, Fase 3),
# 8081 = API de control (axum; objetivo: BackgroundService).
EXPOSE 8080 8081

# El proceso va en PRIMER PLANO: nunca -d/--daemon ni ServerConf.daemon = true, porque un
# proceso daemonizado deja este HEALTHCHECK en falso y rompe el stop_grace_period del compose.
# /health en 8080 lo resuelve request_filter antes de validar crypt_id (docs/spec.md Fase 3).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:8080/health || exit 1

ENTRYPOINT ["/app/tele-proxy"]
