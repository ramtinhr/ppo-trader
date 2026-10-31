#!/usr/bin/env bash
# Fast price-strategy iteration loop (avoid --release LTO + avoid re-reading archives).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
SCAN="${SCAN:-data/windowscan/2025-10-01_2026-10-10_24h_staged}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

echo "==> relfast build (no LTO)"
cargo build --profile relfast --bin window_nm_walk_forward

echo "==> run --path price (1m candle cache under $SCAN/.cache/)"
exec "$CARGO_TARGET_DIR/relfast/window_nm_walk_forward" \
  --path price \
  --price-bars 15m \
  --price-strategy 1 \
  --scan "$SCAN" \
  --verify 0 \
  "$@"
