#!/usr/bin/env bash
# Fast price-strategy iteration loop (Go price-walk; 1m candle cache under scan/.cache/).
# STRATEGY=1 oracle (default) | STRATEGY=2 rolling predicted-path bot (trade→refresh)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
SCAN="${SCAN:-data/windowscan/2025-10-01_2026-10-10_24h_staged}"
STRATEGY="${STRATEGY:-1}"

if [[ -x "$ROOT/dt" ]]; then
  DT=( "$ROOT/dt" )
else
  DT=( go run ./cmd/dt )
fi

echo "==> price-walk (Go) strategy=${STRATEGY} via ${DT[*]}"
exec "${DT[@]}" price-walk \
  --price-bars 15m \
  --price-strategy "$STRATEGY" \
  --scan "$SCAN" \
  --verify 0 \
  "$@"
