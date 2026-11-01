#!/usr/bin/env bash
# Build the macOS/Linux `dt` CLI with the external system linker.
# Plain `go build` can produce a binary that aborts on modern macOS with:
#   dyld: missing LC_UUID load command
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
rm -f "$ROOT/dt"
export CGO_ENABLED=1
go build -ldflags="-linkmode=external" -o "$ROOT/dt" ./cmd/dt
chmod +x "$ROOT/dt"
echo "built $ROOT/dt"
file "$ROOT/dt" 2>/dev/null || true
