#!/usr/bin/env bash
# Fast price-strategy iteration loop (Go price-walk; 1m candle cache under scan/.cache/).
# STRATEGY=1 oracle | STRATEGY=2 live TRADE-year predicted bot
# Year example: SCAN=data/windowscan/<2024_to_2026> RESERVE=365 TRADE_DAYS=365 STRATEGY=2
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
SCAN="${SCAN:-data/windowscan/2025-10-01_2026-10-10_24h_staged}"
STRATEGY="${STRATEGY:-1}"
RESERVE="${RESERVE:-}"
TRADE_DAYS="${TRADE_DAYS:-}"

if [[ -x "$ROOT/dt" ]]; then
  DT=( "$ROOT/dt" )
else
  DT=( go run ./cmd/dt )
fi

EXTRA=()
if [[ -n "$RESERVE" ]]; then
  EXTRA+=( --reserve-trade-days "$RESERVE" )
fi
if [[ -n "$TRADE_DAYS" ]]; then
  EXTRA+=( --trade-days "$TRADE_DAYS" )
fi

echo "==> price-walk (Go) strategy=${STRATEGY} scan=${SCAN} via ${DT[*]} ${EXTRA[*]:-}"
exec "${DT[@]}" price-walk \
  --price-bars 15m \
  --price-strategy "$STRATEGY" \
  --scan "$SCAN" \
  --verify 0 \
  "${EXTRA[@]}" \
  "$@"
