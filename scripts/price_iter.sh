#!/usr/bin/env bash
# Fast price-strategy iteration loop (Go price-walk; 1m candle cache under scan/.cache/).
# STRATEGY=1 oracle | STRATEGY=2 IS/OOS predicted bot
# Year: TRAIN 2024→2025 / TRADE 2025→2026
#   STRATEGY=2 RESERVE=365 TRADE_DAYS=365 SCAN=data/windowscan/2024-01-01_2026-01-01_24h_staged
# Month smoke: RESERVE=30 TRADE_DAYS=30 SCAN=data/windowscan/<short_scan>
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
SCAN="${SCAN:-data/windowscan/2024-01-01_2026-01-01_24h_staged}"
STRATEGY="${STRATEGY:-1}"
RESERVE="${RESERVE:-365}"
TRADE_DAYS="${TRADE_DAYS:-365}"

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
