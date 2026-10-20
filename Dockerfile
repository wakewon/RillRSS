# syntax=docker/dockerfile:1
#
# RillRSS container image (linux/amd64 and linux/arm64).
#   docker compose up -d --build
# Multi-arch build (cross-compiles on the build machine, no emulation needed for Rust):
#   docker buildx build --platform linux/amd64,linux/arm64 -t rillrss:latest .

ARG RUST_IMAGE=rust:1.99-slim-trixie
ARG RUNTIME_IMAGE=debian:trixie-slim

FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS builder
ARG TARGETARCH
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates pkg-config cmake perl make gcc g++; \
    native="$(dpkg --print-architecture)"; \
    if [ "$TARGETARCH" != "$native" ]; then \
        case "$TARGETARCH" in \
            amd64) apt-get install -y --no-install-recommends gcc-x86-64-linux-gnu g++-x86-64-linux-gnu libc6-dev-amd64-cross ;; \
            arm64) apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross ;; \
            *) echo "unsupported TARGETARCH $TARGETARCH"; exit 1 ;; \
        esac; \
    fi; \
    rm -rf /var/lib/apt/lists/*; \
    case "$TARGETARCH" in \
        amd64) echo x86_64-unknown-linux-gnu > /rust-target ;; \
        arm64) echo aarch64-unknown-linux-gnu > /rust-target ;; \
    esac; \
    rustup target add "$(cat /rust-target)"
# Triplet-prefixed compilers exist natively and as cross compilers on Debian.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CXX_x86_64_unknown_linux_gnu=x86_64-linux-gnu-g++ \
    AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
WORKDIR /src

# Build dependencies first (cached unless Cargo.toml/Cargo.lock change).
COPY Cargo.toml Cargo.lock ./
RUN set -eux; mkdir -p src; echo 'fn main() {}' > src/main.rs; touch src/lib.rs; \
    cargo build --release --locked --target "$(cat /rust-target)"; \
    rm -rf src; find target -name 'rillrss*' -prune -exec rm -rf {} + ; find target -name 'librillrss*' -exec rm -rf {} +

COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
COPY src ./src
RUN set -eux; cargo build --release --locked --target "$(cat /rust-target)"; \
    cp "target/$(cat /rust-target)/release/rillrss" /rillrss

FROM ${RUNTIME_IMAGE}
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/*
# No RUN steps after the package install: a cross-built image then needs no
# emulation beyond that cached layer (emulated chmod has failed with EIO).
COPY --from=builder --chmod=0755 /rillrss /usr/local/bin/rillrss
COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/entrypoint.sh
# WORKDIR creates /data without running a process.
WORKDIR /data
WORKDIR /

ENV RILL_DATA_DIR=/data \
    RILL_LISTEN=0.0.0.0:8080 \
    PUID=1000 \
    PGID=1000
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 CMD ["rillrss", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
