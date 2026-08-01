#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust from https://rustup.rs" >&2
  exit 1
fi

if [[ ! -f .env ]]; then
  cp .env.example .env
  echo "Created .env from .env.example — please edit it before production use."
fi

echo "==> Building zen-proxy-rs (release)..."
cd zen-proxy-rs
cargo build --release

echo ""
echo "Build complete: zen-proxy-rs/target/release/zen-proxy-rs"
echo ""
echo "Run:"
echo "  set -a && source ../.env && set +a && ./target/release/zen-proxy-rs"
