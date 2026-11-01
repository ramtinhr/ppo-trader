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
- `--price-strategy 2` / `predicted` — **master loop** for one OOS month:
  at each free bar **predict@entry** (~160 under 1%) → best TP×SL×lev (wins on path,
  SL≥err_band, no liquid) → **one trade** → after exit **re-predict + re-pick** until
  `trade_end`. The FORWARD table from the last tape candle is **demo only** — not a trade
  origin. (`nm_price_walk_forward_predicted.html`)

### Master loop: TRAIN → predict@entry → trade → refresh (1 OOS month)

| Phase | Rule |
|-------|------|
| **TRAIN** | All dates **before** `trade_start` (fit only) |
| **Each decision** | Predict from **that bar** (~`path_under_1pct` candles) → pick best combo → trade once |
| **After exit** | New origin → predict again → new combo → trade again … until `trade_end` |
| **FORWARD demo** | Path from last tape candle (e.g. 2026-10-10) — **ignore for trade dates** |

Defaults: `--reserve-trade-days 30 --trade-days 30`. Short scan → shrink TRAIN, keep TRADE.

```bash
# Use a scan you already have (example)
./dt price-walk \
  --scan data/windowscan/2025-10-01_2026-10-10_24h_staged \
  --data data/okx/BTC-USDT-SWAP/archives \
  --price-bars 15m \
  --price-strategy 2 \
  --reserve-trade-days 30 \
  --trade-days 30 \
  --max-rel-err 0.01 \
  --verify 0
```

Or:

```bash
STRATEGY=2 SCAN=data/windowscan/2025-10-01_2026-10-10_24h_staged \
  RESERVE=30 TRADE_DAYS=30 ./scripts/price_iter.sh
```

Writes `nm_price_walk_forward_predicted.json` / `.html`. Soft-fails OKX when blocked.

## Tests

```bash
go test ./...
cargo check --no-default-features
```
