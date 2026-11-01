# `dt` — Go product CLI

Rust is the research engine (`download`, `scan`, `simulate`, `verify`, `backtest`, `tv_check`).
Go owns the product CLI: score walk-forward, **price walk-forward**, HTML reports, and browse.

## Build

```bash
# Rust engine. This crate has its own [workspace] so a parent Cargo.toml is ignored.
cargo build --profile relfast --no-default-features --bin download --bin scan --bin simulate
# optional Nautilus:
cargo build --profile relfast --features verify --bin verify --bin backtest

# Go CLI — on macOS use the script (plain `go build` can abort with missing LC_UUID)
./scripts/build_dt.sh
# or: CGO_ENABLED=1 go build -ldflags="-linkmode=external" -o dt ./cmd/dt
```

Download only (no Go):

```bash
# SOCKS5 when OKX is blocked (Clash/V2Ray local port — change if yours differs)
cargo run --release --bin download -- 2024-10-30 2026-10-30 \
  --proxy socks5://127.0.0.1:1080
# or: export OKX_PROXY=socks5://127.0.0.1:1080
```

Engine discovery order: `$DT_ENGINE_DIR`, `./target/relfast`, `./target/release`, `./target/debug`, then `PATH`.

## Commands

| Command | Behavior |
|--------|----------|
| `dt download [args…]` | exec Rust `download` (alias `download_okx_trades`) |
| `dt scan [args…]` | exec Rust `scan` (pass through, including `--bucket`) |
| `dt simulate [args…]` | exec Rust `simulate` (batch JSONL stdin/stdout) |
| `dt verify [args…]` | exec Rust `verify` |
| `dt backtest [args…]` | exec Rust `backtest` |
| `dt tv-check [args…]` | exec Rust `tv_check` |
| `dt browse --scan DIR [--port N] [--layout window\|month\|auto]` | merged browse HTTP API |
| `dt walk-forward --scan DIR [flags…]` | N/M score-path walk-forward (Go + Rust `simulate`) |
| `dt price-walk --scan DIR [flags…]` | candle OLS + oracle exits (pure Go; M1C1 cache) |
| `dt report --json PATH` | regenerate EN/FA HTML from saved JSON (score or price) |

## Browse

Auto-detects layout from `meta.json` (`windows` / `scan_version` → 20-byte window records;
`months` → 12-byte month records). Replaces the old Rust `browse_*` binaries (removed).

## Walk-forward (score)

Loads `meta.json` + ranks `window_*.bin` finalists, batch-calls Rust `simulate` for closed trades,
runs the N/M pick loop with a learned `ScoreModel` (near→far TRAIN → LOCK → TRADE), and writes
`nm_walk_forward.json` plus embedded HTML.

## Price walk-forward

Pure Go (`internal/pricewalk`). Aggregates 1m candles (M1C1 cache under `scan/.cache/`), fits
OLS return features per bar size, optionally runs per-trade TP/SL:

- `--price-strategy 1` / `oracle` — peek actual future closes (research ceiling → `nm_price_walk_forward.html`)
- `--price-strategy 2` / `predicted` — **live TRADE-year bot**: walk the full TRADE
  window (e.g. one year). Raw OLS only — sit out if peak &lt; ~0.20%; h=1 must agree;
  re-test TP×SL×lev (SL≥err_band, lev≤3). FORWARD chart is post-period demo only.
  (`nm_price_walk_forward_predicted.html`)

### Train 2024 → trade 2025 (one year)

Use your existing **2024→2025** archives (no 2023). Staged scan output:
`data/windowscan/2024-01-01_2026-01-01_24h_staged`. Last 365 days = TRADE (~2025).

```bash
# 0) build dt + Rust engine once
./scripts/build_dt.sh
cargo build --profile relfast --no-default-features --bin download --bin scan

# 1) only fill gaps for 2024–2025 if needed (END exclusive; skip if archives already there)
#    use --proxy socks5://127.0.0.1:1080 if OKX is blocked
./dt download 2024-01-01 2026-01-01

# 2) windowscan — creates data/windowscan/2024-01-01_2026-01-01_24h_staged/
./dt scan --bucket window --start 2024-01-01 --end 2026-01-01 --hours 24

# 3) live predicted bot: TRAIN≈2024, TRADE≈2025
./dt price-walk \
  --scan data/windowscan/2024-01-01_2026-01-01_24h_staged \
  --data data/okx/BTC-USDT-SWAP/archives \
  --price-bars 15m \
  --price-strategy 2 \
  --reserve-trade-days 365 \
  --trade-days 365 \
  --max-rel-err 0.01 \
  --verify 0
```

Or with the helper (same scan path):

```bash
SCAN=data/windowscan/2024-01-01_2026-01-01_24h_staged \
  RESERVE=365 TRADE_DAYS=365 STRATEGY=2 ./scripts/price_iter.sh
```

`ls data/windowscan` / `ls data/okx/BTC-USDT-SWAP/archives | head` to confirm
what you already have. Soft-fails OKX when the network is blocked. Writes
`nm_price_walk_forward.json` / `.html` (selected bar) plus per-bar grid reports.

## Tests

```bash
go test ./...
cargo check --no-default-features
```
