# syntax=docker/dockerfile:1
#
# RillRSS container image (linux/amd64 and linux/arm64).
#   docker compose up -d --build
# Multi-arch build:
#   docker buildx build --platform linux/amd64,linux/arm64 -t rillrss:latest .

ARG RUST_IMAGE=rust:1.99-slim-trixie
ARG RUNTIME_IMAGE=debian:trixie-slim

FROM ${RUST_IMAGE} AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates pkg-config cmake perl make g++ \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src

# Build dependencies first (cached unless Cargo.toml/Cargo.lock change).
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src target/release/rillrss target/release/deps/rillrss-* target/release/deps/librillrss-* target/release/.fingerprint/rillrss-*

COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
COPY src ./src
RUN cargo build --release --locked && strip target/release/rillrss || true

FROM ${RUNTIME_IMAGE}
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/rillrss /usr/local/bin/rillrss
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh /usr/local/bin/rillrss && mkdir -p /data

ENV RILL_DATA_DIR=/data \
    RILL_LISTEN=0.0.0.0:8080 \
    PUID=1000 \
    PGID=1000
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 CMD ["rillrss", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
