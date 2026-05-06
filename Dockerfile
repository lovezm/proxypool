# syntax=docker/dockerfile:1.7

# ── builder ────────────────────────────────────────────────────────────────
FROM rust:1.82-bookworm AS builder

RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools pkg-config \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-unknown-linux-musl

WORKDIR /src

# Cache dependencies first.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
 && cargo build --release --target x86_64-unknown-linux-musl \
 && rm -rf src target/x86_64-unknown-linux-musl/release/deps/proxy_gateway* \
            target/x86_64-unknown-linux-musl/release/proxy-gateway*

# Real build.
COPY src   ./src
COPY assets ./assets
RUN cargo build --release --target x86_64-unknown-linux-musl \
 && strip target/x86_64-unknown-linux-musl/release/proxy-gateway || true

# ── runtime ────────────────────────────────────────────────────────────────
FROM alpine:3.20

RUN apk add --no-cache ca-certificates tini \
 && addgroup -S app && adduser -S app -G app \
 && mkdir -p /app/data \
 && chown -R app:app /app

USER app
WORKDIR /app

COPY --from=builder --chown=app:app \
     /src/target/x86_64-unknown-linux-musl/release/proxy-gateway /app/proxy-gateway

EXPOSE 11077 11078
VOLUME ["/app/data"]

ENTRYPOINT ["/sbin/tini", "--", "/app/proxy-gateway"]
