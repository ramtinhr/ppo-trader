# Dynamic Trader — Architecture Guide

This document explains **every major part** of the codebase after the Rust-engine / Go-product split.
Audience: fullstack developers who are not Rust specialists.

Related: [DT.md](DT.md) (CLI cheat sheet), [README.md](../README.md) (research behaviour).

---

## 1. Big picture

```
OKX daily trade zips
        │
        ▼
┌─────────────────── Rust engine ───────────────────┐
│  download  →  verified archives under data/okx/…  │
│  scan      →  combo × window .bin + meta.json     │
│  simulate  →  batch JSONL replay (fast path)      │
│  verify    →  one combo through NautilusTrader    │
│  backtest  →  Pine RSI strategy via Nautilus      │
│  tv_check  →  compare vs TradingView              │
│  window_nm_walk_forward --path price (price path) │
└───────────────────────┬───────────────────────────┘
                        │ bins / JSONL / JSON
                        ▼
┌─────────────────── Go product (`dt`) ─────────────┐
│  browse        local HTTP over scan .bin files    │
│  walk-forward  N/M score path + ScoreModel        │
│  report        HTML from saved JSON               │
│  price-walk    thin wrap of Rust price path       │
│  download|scan|simulate|verify|…  exec Rust bins  │
└───────────────────────────────────────────────────┘
```

**Rule of thumb**

| Concern | Language | Why |
|--------|----------|-----|
| Tick tape, fill rules, million-combo grids | Rust | Must match Nautilus; needs speed |
| Walk-forward policy, ranking, HTML, HTTP | Go | You can change rules without Nautilus compiles |
| Fill-by-fill truth | Nautilus (Rust `verify` / `backtest`) | Slow; only for checks |

`data/` is gitignored. Archives and scan outputs stay on disk locally.

---

## 2. Repository layout

```
ppo-trader/
├── Cargo.toml              # Rust crate + optional feature `verify`
├── go.mod / cmd/dt/        # Go product CLI
├── internal/
│   ├── engine/             # find & exec Rust binaries; simulate JSONL client
│   ├── browse/             # HTTP browser for scan results
│   ├── walkforward/        # N/M score-path walk-forward
│   ├── score/              # budget-10 ScoreModel (port of window_score.rs)
│   └── report/             # embed HTML templates
├── src/                    # Rust library (shared by all engine bins)
├── src/bin/                # Rust binaries (engine + price-path WF)
├── config/testnet.toml     # fees / paper_only / max leverage
├── scripts/build_dt.sh     # macOS-safe Go build
└── docs/                   # this guide, DT.md, PDF
```

---

## 3. Rust library modules (`src/`)

| Module | Role |
|--------|------|
| `okx_history.rs` | Download/verify OKX daily zips; parse trades; build candles; known gaps/voids |
| `tape.rs` | Every trade as Nautilus L1 matching prices + `first_hit` tree for TP/SL |
| `scan.rs` | Fast RSI-cross + exits simulator (mirrors Nautilus rules) |
| `tpsl_grid.rs` | Vectorized single-TP×SL×leverage grid over a tape (`Entries`) |
| `exits.rs` | Stop / take-profit ladder math shared by scanner and Nautilus |
| `rsi.rs` / `rsi_cross.rs` / `pine_rsi.rs` | Indicators and cross / Pine RSI signals |
| `filters.rs` | EMA trend + ATR filters wrapping crosses |
| `grid.rs` | CLI-style parsers + RSI series helpers for live grids |
| `fills.rs` | `Fill` / `FillKind` (no Nautilus dependency) |
| `signals.rs` | `Signal` / `BarSignals` (no Nautilus dependency) |
| `window_score.rs` | `WindowStats` (20-byte records), learned `ScoreModel` |
| `price_model.rs` | OLS candle-return model (price-path research) |
| `config.rs` | TOML load + safety validation (`paper_only`, leverage) |
| `verify_cmp.rs` | Compare scanner vs Nautilus fills |
| `strategy.rs` | Nautilus `ReversalStrategy` (**feature `verify`**) |
| `backtest.rs` | Nautilus engine wrapper (**feature `verify`**) |
| `tradingview.rs` | TradingView WebSocket client for `tv_check` |

### Fast path vs Nautilus

1. **Fast path:** `Tape::load` → `scan::simulate` / `tpsl_grid::Entries`  
   Used for millions of combinations.
2. **Slow path:** `run_backtest` + `ReversalStrategy`  
   Used to confirm a handful of winners fill-by-fill.

Feature `verify` (default **on**) pulls in `nautilus-*`. Engine work without Nautilus:

```bash
cargo build --profile relfast --no-default-features --bin download --bin scan --bin simulate
```

---

## 4. Rust binaries (`src/bin/`)

| Binary | Command | Needs Nautilus? | What it does |
|--------|---------|-----------------|--------------|
| `download` | `cargo run --bin download -- START END` | No | Fetch + verify OKX archives |
| `scan` | `… --bin scan -- --bucket window\|month …` | No | Precompute combo×window (or month) bins |
| `simulate` | JSONL on stdin | No | Batch replay for Go walk-forward |
| `verify` | `… --features verify --bin verify` | Yes | One combo vs Nautilus |
| `backtest` | Pine RSI through Nautilus | Yes | Classic RSI strategy table |
| `tv_check` | Needs TV cookies in `.env` | No engine | Candle / RSI / trade parity vs TradingView |
| `window_nm_walk_forward` | `--path score\|price\|both` | Yes (default feature) | Legacy score path + **active price path** |

`dt price-walk` execs `window_nm_walk_forward --path price`.

### Scan on-disk format

**Window bucket** (`--bucket window`, default)

- `meta.json` — timeframes, lengths, TP/SL/leverage, window list, fees, `scan_version`
- `window_*.bin` — fixed **20-byte** little-endian `WindowStats` per combo  
  (`profit f32, drop f32, worst f32, hold_hours f32, trades u16, wins u16`)

**Month bucket** (`--bucket month`)

- `month_YYYY-MM.bin` / `all.bin` — **12-byte** records (`sum f32, drop f32, trades u32`)

Go `dt browse` auto-detects layout from `meta.json`.

### `simulate` JSONL contract

Request line:

```json
{"id":"1","minutes":60,"fast":14,"slow":50,"exits":{"stop_pct":1.0,"targets":[[1.0,1.0]],"breakeven":false},"leverage":1,"logged":true}
```

Response line: `outcome`, optional `closed` trades, optional `fills`.  
Load tape once; never spawn one process per combo.

---

## 5. Go product (`cmd/dt`, `internal/`)

Build on macOS:

```bash
./scripts/build_dt.sh
```

| Command | Behaviour |
|---------|-----------|
| `dt download\|scan\|simulate\|verify\|backtest\|tv-check` | Exec matching Rust binary (`$DT_ENGINE_DIR` → `target/relfast` → …) |
| `dt browse --scan DIR` | HTTP UI over `.bin` results |
| `dt walk-forward --scan DIR` | Score-path N/M: rank bins → `simulate` trades → ScoreModel → JSON/HTML |
| `dt report --json PATH` | Rebuild HTML from saved walk-forward JSON |
| `dt price-walk …` | Rust price-path WF |

### Score model (`internal/score`)

Port of `src/window_score.rs`:

- Features in `[0,1]`: winrate, gain, R/R, until, drawdown, n_trades  
- Weights ≥ 0, sum = **10** (`SCORE_BUDGET`)  
- Fit on IS→OOS pairs; LOCK when relative error &lt; 1%

### Walk-forward score path (`internal/walkforward`)

1. Read `meta.json` + window bins → finalists  
2. Batch-call Rust `simulate` for closed trades  
3. For each outer window: score last N trades in lookback, trade next M (= N/3)  
4. Near→far TRAIN of ScoreModel, then LOCK, then TRADE  
5. Write `nm_walk_forward.json` + HTML  

Unit tests live under `internal/score` and `internal/walkforward`.

---

## 6. Typical research workflow

```bash
# 1. Archives (2 years, end exclusive)
cargo run --release --bin download -- 2024-10-31 2026-10-31

# 2. Window scan (staged TP/SL grid → survivors)
cargo run --release --bin scan -- --bucket window \
  --start 2025-10-01 --end 2026-10-01 --hours 24

# 3. Browse results
./scripts/build_dt.sh
./dt browse --scan data/windowscan/<dir> --port 8091

# 4. Score walk-forward (Go)
./dt walk-forward --scan data/windowscan/<dir>

# 5. Price-path walk-forward (still Rust)
./dt price-walk --scan data/windowscan/<dir> --verify 0
# or: scripts/price_iter.sh
```

---

## 7. Config & safety

`config/testnet.toml`:

- `risk.paper_only` must be `true`  
- `risk.max_leverage` capped by validation  
- OKX maker/taker fees in bps  

Secrets only in `.env` (gitignored). Never commit keys.

---

## 8. What was removed in the cleanup

Deleted as unused after the split (Go owns product UX; `scan` replaced dual scanners):

- `browse_full_scan` / `browse_window_scan` (+ HTML)  
- Thin `full_scan` / `window_scan` wrappers  
- Legacy orchestration bins: `rsi_combos`, `walk_forward`, `walk_forward_trades`, `full_walk_forward`, `window_walk_forward` (+ their HTML reports)

Still present for engine / price path: `download`, `scan`, `simulate`, `verify`, `backtest`, `tv_check`, `window_nm_walk_forward`.

---

## 9. Mental model for reading code

1. **Data in:** `okx_history` + `tape`  
2. **Signal:** `rsi*` + `filters` + `signals`  
3. **Replay:** `scan` / `tpsl_grid` (fast) or `strategy`+`backtest` (Nautilus)  
4. **Score:** `window_score` (Rust) ↔ `internal/score` (Go)  
5. **Product loop:** Go ranks → calls `simulate` → writes reports  

When changing fill rules, change **Rust** `scan` + `strategy` together and confirm with `verify`.  
When changing how winners are picked, change **Go** `walkforward` / `score`.
