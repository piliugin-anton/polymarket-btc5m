# syntax=docker/dockerfile:1
# Multi-stage: release binary copied into a minimal runtime image.
# Do not COPY your real .env into the image — use docker-compose `env_file` (or `-e` / secrets).

# `rust:bookworm` tracks latest stable — lockfile deps may require a recent rustc (e.g. 1.88+).
ARG RUST_IMAGE=rust:bookworm

FROM ${RUST_IMAGE} AS builder
WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/polymarket-crypto /usr/local/bin/polymarket-crypto
COPY docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod +x /usr/local/bin/polymarket-crypto /docker-entrypoint.sh

WORKDIR /data
ENV RUST_BACKTRACE=1
ENTRYPOINT ["/docker-entrypoint.sh"]
