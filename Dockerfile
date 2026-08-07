# syntax=docker/dockerfile:1

# ---------- Stage 1: build zen-proxy-rs ----------
FROM rust:1.88-bookworm AS builder
WORKDIR /build

COPY free-model-client-rs ./free-model-client-rs
COPY zen-proxy-rs ./zen-proxy-rs

WORKDIR /build/zen-proxy-rs
RUN cargo build --release

# ---------- Stage 2: runtime (debian-slim + mihomo + zen-proxy-rs) ----------
# debian-slim (glibc) matches the builder, so the zen-proxy-rs binary's dynamic
# linker resolves. mihomo "compatible" build is statically linked and runs fine.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl bash tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && update-ca-certificates

# mihomo core (compatible = static build, runs on alpine without glibc)
ARG MIHOMO_VERSION=v1.19.29
RUN curl -fsSL -o /tmp/mihomo.gz \
      "https://github.com/MetaCubeX/mihomo/releases/download/${MIHOMO_VERSION}/mihomo-linux-amd64-compatible-${MIHOMO_VERSION}.gz" \
    && gunzip -c /tmp/mihomo.gz > /usr/local/bin/mihomo \
    && chmod +x /usr/local/bin/mihomo \
    && rm -f /tmp/mihomo.gz \
    && mihomo -v

# zen-proxy-rs binary
COPY --from=builder /build/zen-proxy-rs/target/release/zen-proxy-rs /usr/local/bin/zen-proxy-rs

# entrypoint (generates mihomo config from SUBSCRIBE_URL, starts both daemons)
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENV PORT=4000 \
    BIND_ADDRESS=0.0.0.0 \
    MIHOMO_CONFIG=/etc/mihomo/config.yaml

EXPOSE 4000 7890 7891 9090

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fs http://127.0.0.1:${PORT}/health || exit 1

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
