#!/usr/bin/env bash
# Fast price-strategy iteration loop (Go price-walk; 1m candle cache under scan/.cache/).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
SCAN="${SCAN:-data/windowscan/2025-10-01_2026-10-10_24h_staged}"

if [[ -x "$ROOT/dt" ]]; then
  DT=( "$ROOT/dt" )
else
  DT=( go run ./cmd/dt )
fi

echo "==> price-walk (Go) via ${DT[*]}"
exec "${DT[@]}" price-walk \
  --price-bars 15m \
  --price-strategy 1 \
  --scan "$SCAN" \
  --verify 0 \
  "$@"
