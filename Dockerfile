# Abacus — Multi-stage build
# Usage:
#   docker build -t abacus .
#   docker run -p 8080:8080 -e ABACUS_API_KEY=sk-xxx -e ABACUS_SERVER_TOKEN=mysecret abacus

# ─── Build stage ─────────────────────────────────────────────────
FROM rust:1.82-slim AS builder

WORKDIR /build
COPY pkg/Cargo.toml pkg/Cargo.lock ./
COPY pkg/crates/ crates/

# Build release binary (server)
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev protobuf-compiler && \
    rm -rf /var/lib/apt/lists/* && \
    cargo build --release --bin abacus -p abacus-cli && \
    cargo build --release -p abacus-server

# ─── Runtime stage ───────────────────────────────────────────────
FROM debian:bookworm-slim

# curl 是 HEALTHCHECK 必需（也供运维诊断用）；缺它会让容器永久 unhealthy
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy binaries
COPY --from=builder /build/target/release/abacus /usr/local/bin/abacus
COPY --from=builder /build/target/release/abacus-server /usr/local/bin/abacus-server

# Data directory
RUN mkdir -p /app/data /app/logs

# Default configuration
ENV ABACUS_LOG_DIR=/app/logs
ENV ABACUS_DATA_DIR=/app/data
ENV RUST_LOG=abacus_engine=info,abacus_server=info

EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
    CMD curl -f http://localhost:8080/api/v1/health || exit 1

ENTRYPOINT ["abacus-server"]
