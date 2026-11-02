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
- `--price-strategy 2` / `predicted` — **confirmed IS/OOS loop**:
  each entry origin → **re-find** lookback **N** + under-1% **H** from past → **re-predict** H closes →
  **score** TP×SL×lev on TRAIN lookback (most train PnL) → trade after origin at bar open → **append actuals** →
  next entry **re-finds N** and **predicts again** until `trade_end`. (`nm_price_walk_forward_predicted.html`)

### Master loop: find N → predict → trade → append → re-find N → predict again

| Phase | Rule |
|-------|------|
| **IS (TRAIN)** | All dates **before** `trade_start` (fit OLS) |
| **N / H** | At **each entry** origin: walk back lookback **N** + forward **H** with mean err &lt; `--max-rel-err` (from past data only — includes appended actuals). Not frozen after trade 1 |
| **Predict** | Fresh every origin from `bars[i-1]` features; enter at `bars[i].Open` (never reuse the prior path) |
| **Combo** | Enter on predicted **side** (fee-floor peak). Exits: score ONLY analyze band TP 0.4–1.2 / SL 0.7–1.5 for **most train PnL**; if train thin → fallback TP 0.7 / SL 1.0 / lev≈5 (do not starve the book) |
| **OOS trade** | Predict **H** → enter **after** → realize on actuals |
| **After exit** | **Append** entry→exit actual candles → **re-find N/H** → **re-predict** → re-score exits; continue until `trade_end` |

Defaults for a **1-month** smoke test: `--reserve-trade-days 30 --trade-days 30`.
Year run: `--reserve-trade-days 365 --trade-days 365`. Short scan → shrink TRAIN, keep TRADE.

### TRAIN 2024→2025, TRADE 2025→2026 (one OOS year)

| Phase | From | Until |
|-------|------|-------|
| **IS / TRAIN** | 2024-01-01 | 2025-01-01 |
| **OOS / TRADE** | 2025-01-01 | 2026-01-01 |

Need archives + a windowscan spanning both years, then reserve the last 365 days:

```bash
# 0) build
./scripts/build_dt.sh
cargo build --profile relfast --no-default-features --bin download --bin scan

# 1) archives (END exclusive). Skips days already verified.
#    use --proxy socks5://127.0.0.1:PORT if OKX is blocked
./dt download 2024-01-01 2026-01-01 --proxy socks5://127.0.0.1:12334

# 2) windowscan → data/windowscan/2024-01-01_2026-01-01_24h_staged/
./dt scan --bucket window --start 2024-01-01 --end 2026-01-01 --hours 24

# 3) IS = 2024→2025, OOS trade year = 2025→2026, under-1% H
./dt price-walk \
  --scan data/windowscan/2024-01-01_2026-01-01_24h_staged \
  --data data/okx/BTC-USDT-SWAP/archives \
  --price-bars 15m \
  --price-strategy 2 \
  --reserve-trade-days 365 \
  --trade-days 365 \
  --max-rel-err 0.01 \
  --verify 0

# 4) browse
./dt browse --scan data/windowscan/2024-01-01_2026-01-01_24h_staged --port 8091
# → http://127.0.0.1:8091/report/nm_price_walk_forward_predicted.html
```

Or helper:

```bash
STRATEGY=2 SCAN=data/windowscan/2024-01-01_2026-01-01_24h_staged \
  RESERVE=365 TRADE_DAYS=365 ./scripts/price_iter.sh
```

Writes `nm_price_walk_forward_predicted.json` / `.html`. Soft-fails OKX when blocked.

## Tests

```bash
go test ./...
cargo check --no-default-features
```
