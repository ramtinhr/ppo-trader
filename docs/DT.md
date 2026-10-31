# `dt` — Go product CLI

Rust remains the engine (`download`, `scan`, `simulate`, `verify`, `backtest`, `tv_check`,
`window_nm_walk_forward` for the price path). Go owns the product CLI: walk-forward score path,
HTML reports, and the browse HTTP server.

## Build

```bash
# Rust engine. This crate has its own [workspace] so a parent Cargo.toml is ignored.
cargo build --profile relfast --no-default-features --bin download --bin scan --bin simulate
# optional: cargo build --profile relfast --features verify --bin verify --bin window_nm_walk_forward

# Go CLI — on macOS use the script (plain `go build` can abort with missing LC_UUID)
./scripts/build_dt.sh
# or: CGO_ENABLED=1 go build -ldflags="-linkmode=external" -o dt ./cmd/dt
```

Download only (no Go):

```bash
cargo run --release --bin download -- 2024-10-31 2026-10-31
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
| `dt report --json PATH` | regenerate EN/FA HTML from saved JSON |
| `dt price-walk [args…]` | exec `window_nm_walk_forward --path price …` |

## Browse

Auto-detects layout from `meta.json` (`windows` / `scan_version` → 20-byte window records;
`months` → 12-byte month records). Same query/CSV/combo APIs as the former Rust
`browse_window_scan` / `browse_full_scan` binaries.

## Walk-forward (score)

Loads `meta.json` + ranks `window_*.bin` finalists, batch-calls Rust `simulate` for closed trades,
runs the N/M pick loop with a learned `ScoreModel` (near→far TRAIN → LOCK → TRADE), and writes
`nm_walk_forward.json` plus embedded HTML.

## Tests

```bash
go test ./...
```
