#!/usr/bin/env bash
# Zen Proxy RS + mihomo one-shot entrypoint.
# The ONLY required input is SUBSCRIBE_URL (a Clash-compatible subscription).
# It generates a mihomo config with TWO local mixed ports, each bound to its
# own Selector group, then starts mihomo and zen-proxy-rs in clash mode.
#
# SUBSCRIBE_URL supports ONE or TWO subscription links separated by a comma:
#   - 1 subscription -> both instances use it (each with its own Selector group)
#   - 2 subscriptions -> instance A uses the first, instance B the second

set -euo pipefail

MIHOMO_BIN="${MIHOMO_BIN:-mihomo}"
MIHOMO_CONFIG="${MIHOMO_CONFIG:-/etc/mihomo/config.yaml}"
MIHOMO_DIR="$(dirname "${MIHOMO_CONFIG}")"

# --- mihomo side ---------------------------------------------------------
MIHOMO_PORT_1="${MIHOMO_PORT_1:-32000}"
MIHOMO_PORT_2="${MIHOMO_PORT_2:-32001}"
MIHOMO_API_PORT="${MIHOMO_API_PORT:-33000}"
MIHOMO_SECRET="${MIHOMO_SECRET:-zen-proxy-secret}"
GROUP_A="${GROUP_A:-Group-A}"
GROUP_B="${GROUP_B:-Group-B}"
# Health-check URL for the subscription provider. Used by url-test auto
# selection only; select groups (what zen-proxy-rs drives) do not depend on it.
HEALTH_CHECK_URL="${HEALTH_CHECK_URL:-https://www.gstatic.com/generate_204}"
# Provider filter (Go RE2 regex): only proxies whose NAME matches are kept.
# Default keeps emoji/flag-prefixed nodes and drops airport "info" entries
# like 剩余流量/距离下次重置/套餐到期 (plain-text fake nodes).
# Override with PROVIDER_FILTER if your subscription uses a different scheme.
PROVIDER_FILTER="${PROVIDER_FILTER:-^\p{So}}"

# --- zen-proxy side ------------------------------------------------------
PORT="${PORT:-4000}"
BIND_ADDRESS="${BIND_ADDRESS:-0.0.0.0}"
UPSTREAM_BASE="${UPSTREAM_BASE:-https://opencode.ai/zen}"
UPSTREAM_API_KEY="${UPSTREAM_API_KEY:-public}"

fail() { echo "[entrypoint] ERROR: $*" >&2; exit 1; }

[ -n "${SUBSCRIBE_URL:-}" ] || fail "SUBSCRIBE_URL is required (your Clash subscription link)"
[ -x "$(command -v "${MIHOMO_BIN}")" ] || fail "mihomo binary not found: ${MIHOMO_BIN}"

# --- split subscriptions (up to two, comma-separated) --------------------
# 1 subscription  -> both instances use it
# 2 subscriptions -> instance A (port 1) uses SUB_1, instance B (port 2) uses SUB_2
SUB_1="$(printf '%s' "${SUBSCRIBE_URL}" | cut -d',' -f1 | xargs)"
# awk: missing second field prints empty (unlike `cut -f2` which echoes the whole line)
SUB_2="$(printf '%s' "${SUBSCRIBE_URL}" | awk -F',' '{print $2}' | xargs)"
[ -n "${SUB_1}" ] || fail "SUBSCRIBE_URL is empty after splitting"
if [ -n "${SUB_2}" ]; then
  echo "[entrypoint] two subscriptions detected: instance A <- sub1, instance B <- sub2"
  GROUP_B_USE="sub1"
else
  echo "[entrypoint] one subscription detected: both instances use it"
  GROUP_B_USE="main"
fi

mkdir -p "${MIHOMO_DIR}"

# --- generate mihomo config ----------------------------------------------
# Two independent Selector groups. With one subscription both groups use the
# same provider; with two, each group is pinned to its own provider so the
# two instances never share a node pool. Switching one port never affects
# the other (and in_use dedup keeps them on different nodes).
cat > "${MIHOMO_CONFIG}" <<EOF
allow-lan: false
mode: rule
log-level: info
ipv6: false
external-controller: 0.0.0.0:${MIHOMO_API_PORT}
secret: ${MIHOMO_SECRET}
unified-delay: true
tcp-concurrent: true
# Remember manual node selection inside Selector groups across restarts
store-selected: true

# Two independent inbound ports, each pinned to its own Selector group.
# zen-proxy-rs treats each port as one node; switching one group never
# affects the other (and in_use dedup keeps them on different nodes).
listeners:
  - name: zen-proxy-${MIHOMO_PORT_1}
    type: mixed
    port: ${MIHOMO_PORT_1}
    listen: 0.0.0.0
    udp: true
    proxy: "${GROUP_A}"
  - name: zen-proxy-${MIHOMO_PORT_2}
    type: mixed
    port: ${MIHOMO_PORT_2}
    listen: 0.0.0.0
    udp: true
    proxy: "${GROUP_B}"

proxy-providers:
  main:
    type: http
    url: "${SUB_1}"
    interval: 3600
    path: ${MIHOMO_DIR}/provider-main.yaml
    filter: '${PROVIDER_FILTER}'
    health-check:
      enable: true
      url: "${HEALTH_CHECK_URL}"
      interval: 300
EOF

# With a second subscription, add provider sub1 (used by Group-B / port 2).
if [ -n "${SUB_2}" ]; then
  cat >> "${MIHOMO_CONFIG}" <<EOF
  sub1:
    type: http
    url: "${SUB_2}"
    interval: 3600
    path: ${MIHOMO_DIR}/provider-sub1.yaml
    filter: '${PROVIDER_FILTER}'
    health-check:
      enable: true
      url: "${HEALTH_CHECK_URL}"
      interval: 300
EOF
fi

cat >> "${MIHOMO_CONFIG}" <<EOF
proxy-groups:
  - name: "${GROUP_A}"
    type: select
    use:
      - main
    disable-udp: false
  - name: "${GROUP_B}"
    type: select
    use:
      - ${GROUP_B_USE}
    disable-udp: false

rules:
  - MATCH,DIRECT
EOF

echo "[entrypoint] mihomo config written to ${MIHOMO_CONFIG}"

# --- start mihomo --------------------------------------------------------
"${MIHOMO_BIN}" -d "${MIHOMO_DIR}" -f "${MIHOMO_CONFIG}" &
MIHOMO_PID=$!
echo "[entrypoint] mihomo started (pid ${MIHOMO_PID}, api 127.0.0.1:${MIHOMO_API_PORT})"

# Wait for the external controller to come up
API_OK=0
for i in $(seq 1 30); do
  if curl -fs -H "Authorization: Bearer ${MIHOMO_SECRET}" \
      "http://127.0.0.1:${MIHOMO_API_PORT}/version" >/dev/null 2>&1; then
    API_OK=1
    break
  fi
  sleep 1
done
[ "${API_OK}" = "1" ] || fail "mihomo API did not become ready within 30s"

# Let the provider finish its first health check (graceful start)
sleep 5

# --- start zen-proxy-rs (clash mode) ------------------------------------
echo "[entrypoint] starting zen-proxy-rs on ${BIND_ADDRESS}:${PORT}"

cleanup() {
  echo "[entrypoint] shutting down (pid zen=${ZEN_PID:-?} mihomo=${MIHOMO_PID})"
  [ -n "${ZEN_PID:-}" ] && kill "${ZEN_PID}" 2>/dev/null || true
  kill "${MIHOMO_PID}" 2>/dev/null || true
}
trap cleanup EXIT TERM INT

env \
  NODE_PROVIDER_MODE=clash \
  CLASH_API_URLS="http://127.0.0.1:${MIHOMO_API_PORT}" \
  CLASH_API_SECRETS="${MIHOMO_SECRET}" \
  CLASH_PROXY_URLS="socks5://127.0.0.1:${MIHOMO_PORT_1},socks5://127.0.0.1:${MIHOMO_PORT_2}" \
  CLASH_CONFIG_FILE="${MIHOMO_CONFIG}" \
  CLASH_SWITCH_MAX_ATTEMPTS="${CLASH_SWITCH_MAX_ATTEMPTS:-15}" \
  CLASH_INVALID_TTL_SECS="${CLASH_INVALID_TTL_SECS:-86400}" \
  PORT="${PORT}" \
  BIND_ADDRESS="${BIND_ADDRESS}" \
  UPSTREAM_BASE="${UPSTREAM_BASE}" \
  UPSTREAM_API_KEY="${UPSTREAM_API_KEY}" \
  PROXY_API_KEY="${PROXY_API_KEY:-}" \
  ADMIN_API_KEY="${ADMIN_API_KEY:-}" \
  zen-proxy-rs &
ZEN_PID=$!
wait "${ZEN_PID}"
ZEN_EXIT=$?
echo "[entrypoint] zen-proxy-rs exited with code ${ZEN_EXIT}"
exit "${ZEN_EXIT}"
