# syntax=docker/dockerfile:1

FROM rust:1.85-bookworm AS builder
WORKDIR /build

COPY free-model-client-rs ./free-model-client-rs
COPY zen-proxy-rs ./zen-proxy-rs

WORKDIR /build/zen-proxy-rs
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/zen-proxy-rs/target/release/zen-proxy-rs /usr/local/bin/zen-proxy-rs

ENV PORT=4000
ENV BIND_ADDRESS=0.0.0.0

EXPOSE 4000
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://127.0.0.1:${PORT}/health || exit 1

ENTRYPOINT ["/usr/local/bin/zen-proxy-rs"]
