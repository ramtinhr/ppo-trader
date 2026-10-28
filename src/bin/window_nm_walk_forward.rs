#![recursion_limit = "256"]

use std::{
    cmp::Ordering,
    fs::{self, File},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use dynamic_trader::{
    backtest::{Fill, Run, run_backtest},
    config::AppConfig,
    exits::{ExitProfile, Exits},
    filters::{Filter, FilteredCross},
    grid::{exit_settings, parse_groups},
    okx_history::{self, Candle},
    rsi::Rsi,
    rsi_cross::RsiCrossParams,
    scan::{self, ClosedTrade, Costs, Market, Outcome, Record, STEP_BTC},
    strategy::BarSignals,
    tape::{CloseRanges, Tape},
    tpsl_grid::{Entries, Frame, Grid, GridTrade},
    price_model::{
        PriceModel, PriceSample, PRICE_FEATURE_NAMES, aggregate_candles, candle_features,
        samples_from_candles, samples_from_candles_horizon,
    },
    window_score::{
        MAX_SCORE_REL_ERR, MIN_DROP, MIN_FIT_SAMPLES, MIN_TRADES, RECORD, SCORE_BUDGET,
        ScoreModel, Tally,
        WindowStats,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};

const USAGE: &str = "\
Window in window: trade windows N/M (IS/OOS) inside outer calendar windows from window_scan.

1. At the start of every outer window (often 1 day), every combination (RSI pair x timeframe x
   take-profit x stop-loss) is ranked by its score in the window before, where every combination
   traded the same days. The best ones are the window's finalists: as many as --max-finalists,
   fewer if their trades would not fit in --memory-gb.
   The scan's first outer window is ranking-only (no prior day to rank on) — OOS / picks never
   start there. For September OOS with a real prior ranking day, window_scan --start must include
   warmup days (e.g. 2026-08-27), then use --first-choice 2026-09-01.
2. Every window pair N/M (N from --train-trades, M from N/3 down to 1) runs its own
   walk-forward: at each pick, every finalist is scored on its last N trades closed within
   --lookback-days (default 30; independent of the outer day-to-day window). At least 5 trades
   and IS profit > 0.1 USDT (losing / flat / tiny windows are skipped — no trade). The best
   qualifying one is traded for its next M trades entered at or after the pick, and when the
   last one closes it picks again. Tape is loaded from scan start minus lookback so those closes
   exist when archives do.
3. From --first-choice, the window pairs themselves are chosen the same way, on their own last
   N trades within that lookback (again only if IS profit > 0.1 USDT), again and again. Two
   Step-2 policies are written:
   stop (end if the best N/M cannot fill M) and allow_short (fold still runs with remaining
   trades, or empty / 0 PnL, then continues).
   --first-choice cannot invent outer windows missing from the scan: it must be on or after the
   first pickable window (second scan window). Warmup days belong in window_scan --start.
4. The chosen trades chained together are what a bot doing this would have made, compared with
   buy-and-hold. Combinations traded are replayed again by the trade-by-trade scanner, and the
   last one through NautilusTrader, to confirm every trade and fill. Verify uses the same tape
   window as the scanner (scan start minus --lookback-days through scan end).
   Reports: nm_walk_forward(.html|_fa.html|.json) and nm_walk_forward_allow_short(...).

Learned score (user formula): S = w·wr + w·gain + w·(R/R) + w·until + w·drawdown + w·n
(fᵢ∈[0,1], wᵢ≥0, Σwᵢ=10). Score TRAIN walks near→far (day before TRADE reserve, then
further back) until rel_err < 1%; LOCK; then fills N/M chain history forward for Step-2
TRADE. Scan combos are NOT rebuilt — only finalist paths are replayed from the tape.

Usage: cargo run --release --bin window_nm_walk_forward -- [options]

  --scan <dir>                window_scan results (default: data/windowscan/2026-09-01_2026-10-01_1d)
  --lookback-days <n>         Max age (calendar days) of closes when scoring last N trades
                              (default: 30). Independent of outer window size (day-to-day = 1).
                              Tape loads from scan start minus this many days.
  --train-trades <list>       Training window sizes in trades (default: 100,90,...,10)
  --trade-days <n>            After score locks, cap Step-2 TRADE to N calendar days.
                              Default 0 = trade until scan end (all remaining data).
  --min-fit-pairs <n>         Do not LOCK until at least this many fit samples / points
                              (default: 2000). Calendar days do not gate the score equation.
  --reserve-trade-days <n>    Keep the last N calendar days of the scan for TRADE only
                              (default: 30). TRAIN stops when it reaches that boundary.
  --first-choice <YYYY-MM-DD> Optional earliest TRADE start (floor). Default: start trading
                              as soon as the score locks. Must be >= first pickable window.
  --require-fit <0|1>         Refuse TRADE unless score locked under --max-rel-err (default: 1).
  --max-rel-err <frac>        LOCK gate on mean(|S−S*|)/10 (default: 0.01 = 1%; e.g. 0.05 = 5%).
  --max-finalists <n>         Finalists per window at most (default: 200000)
  --memory-gb <gb>            Cap on finalists' kept trades in RAM (default: 4). Estimate
                              scales with --lookback-days so a 30d lookback does not OOM.
  --max-fit-pairs <n>         Cap on kept IS→OOS train points (default: 0 = keep all).
  --threads <n>               Parallel workers for finalist build / chain advance (default: 2)
  --verify <n>                1 to rerun the last combination traded through NautilusTrader
                              over the same tape window as the scanner (default: 1, 0 to skip)
  --refine-exits <0|1>        Try TP ladders on shortlist tops and at each Step-2 cycle; keep the
                              best IS exit (default: 0 = single scan TP/SL only).
  --ladders <list>            TP ladders in R (× stop), for refine (default: 1,2,1/2,1/2/3)
  --shares <list>             Position shares % matching ladder length (default: 100,50/50,50/25/25)
  --breakeven <yes|no|both>   Move stop to entry after first of several TPs (default: both)
  --exit-alts <n>             Runner-up exits kept in the report per cycle (default: 5)
  --balance-usdt <usdt>       Starting balance of the chained account (default: 500)
  --config <path>             Fee config (default: config/testnet.toml)
  --data <dir>                Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --path score|price|both     Which pipeline to run (default: score).
                              score = RSI budget-10 walk-forward (nm_walk_forward*).
                              price = candle OHLC linear return model; OOS buy/sell (nm_price_walk_forward*).
                              both  = run both, separate reports.
  --price-bars <list>         Candle sizes for the price path grid (default: 5m,15m,1h,4h).
                              Each bar is fit separately; best locked becomes nm_price_walk_forward.*.
  --price-horizon <n>         Candles ahead to forecast (default: 20). One OLS per horizon h=1..n
                              from the last finished close (h=1 next bar, h=20 = twenty ahead).
  --price-strategy <0|1>      Optional TP/SL×lev grid (default: 0 = off).
  --price-stops <list>        Stops if --price-strategy 1 (default: 0.25,0.5,1,1.5,2).
  --price-leverage <list>     Leverage if --price-strategy 1 (default: 1,2,3,5).
  --html-only                 Rebuild HTML reports from existing nm_walk_forward*.json (no rescan)
  --rerank                    Force day-by-day shortlist again (ignore nm_finalists_cache.bin).
                              Default: reuse cache when lookback/max-finalists/memory-gb/scan match";

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathMode {
    Score,
    Price,
    Both,
}

impl PathMode {
    fn wants_score(self) -> bool {
        matches!(self, PathMode::Score | PathMode::Both)
    }
    fn wants_price(self) -> bool {
        matches!(self, PathMode::Price | PathMode::Both)
    }
}

/// Balance of the whole-period replays, as in window_scan.
const REPLAY_BALANCE: f64 = 1e9;
/// Combinations shown per window in the report.
const SHOWN: usize = 20;
/// Minimum IS (training) profit in USDT required before a pick/choice is traded.
const MIN_IS_PROFIT: f64 = 0.1;

struct Args {
    scan: PathBuf,
    /// Max age of closes for last-N scoring (not the outer day-to-day window).
    lookback_days: u64,
    train_trades: Vec<usize>,
    /// Calendar days of Step-2 TRADE after the score locks.
    trade_days: u64,
    /// Minimum fit_pairs (equation points) before LOCK is allowed.
    min_fit_pairs: usize,
    /// Last N calendar days of the scan reserved for TRADE (TRAIN stops before).
    reserve_trade_days: u64,
    /// Optional earliest TRADE start (floor); default = lock time.
    first_choice: Option<NaiveDate>,
    /// Refuse Step-2 unless the trade-fitted score model passes --max-rel-err.
    require_fit: bool,
    /// LOCK when score_rel_err < this (fraction of SCORE_BUDGET).
    max_rel_err: f64,
    max_finalists: usize,
    memory_gb: f64,
    /// 0 = keep every fit pair (no sliding cap).
    max_fit_pairs: usize,
    threads: usize,
    verify: usize,
    /// Try TP ladders at Step-2 pick time and keep the best IS exit.
    refine_exits: bool,
    ladders: Vec<Vec<f64>>,
    shares: Vec<Vec<f64>>,
    breakeven: Vec<bool>,
    exit_alts: usize,
    balance_usdt: f64,
    config: PathBuf,
    data: PathBuf,
    /// Rebuild HTML from existing JSON only (skip walk-forward computation).
    html_only: bool,
    /// Ignore nm_finalists_cache.bin and re-rank every outer window.
    rerank: bool,
    /// Which pipeline(s) to run.
    path: PathMode,
    /// Candle bar sizes for the price path (label, bar_ms).
    price_bars: Vec<(String, i64)>,
    /// Forecast horizons 1..=price_horizon (candles ahead).
    price_horizon: usize,
    /// Optional TP/SL×lev strategy on price signals (off by default).
    price_strategy: bool,
    /// Stop-loss % values for the price-path TP/SL strategy grid.
    price_stops: Vec<f64>,
    /// Leverage multipliers for the price-path strategy grid.
    price_leverage: Vec<u32>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        scan: "data/windowscan/2026-09-01_2026-10-01_1d".into(),
        lookback_days: 30,
        train_trades: (1..=10).rev().map(|k| k * 10).collect(),
        trade_days: 0,
        min_fit_pairs: 2000,
        reserve_trade_days: 30,
        first_choice: None,
        require_fit: true,
        max_rel_err: MAX_SCORE_REL_ERR,
        max_finalists: 200_000,
        memory_gb: 4.0,
        max_fit_pairs: 0,
        threads: 2,
        verify: 1,
        refine_exits: false,
        ladders: parse_groups("1,2,1/2,1/2/3")?,
        shares: parse_groups("100,50/50,50/25/25")?,
        breakeven: vec![false, true],
        exit_alts: 5,
        balance_usdt: 500.0,
        config: "config/testnet.toml".into(),
        data: "data/okx/BTC-USDT-SWAP/archives".into(),
        html_only: false,
        rerank: false,
        path: PathMode::Score,
        price_bars: parse_price_bars("5m,15m,1h,4h").expect("default price bars"),
        price_horizon: 20,
        price_strategy: false,
        price_stops: vec![0.25, 0.5, 1.0, 1.5, 2.0],
        price_leverage: vec![1, 2, 3, 5],
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        if flag == "--html-only" {
            args.html_only = true;
            continue;
        }
        if flag == "--rerank" {
            args.rerank = true;
            continue;
        }
        if flag == "--path" {
            let value = raw
                .next()
                .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
            args.path = match value.as_str() {
                "score" => PathMode::Score,
                "price" => PathMode::Price,
                "both" => PathMode::Both,
                _ => bail!("--path must be score, price, or both"),
            };
            continue;
        }
        let value = raw
            .next()
            .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
        let bad = || format!("bad value for {flag}: {value}");
        match flag.as_str() {
            "--scan" => args.scan = value.into(),
            "--lookback-days" => {
                args.lookback_days = value.parse().with_context(bad)?;
                ensure!(args.lookback_days > 0, "--lookback-days must be at least 1");
            }
            "--train-trades" => {
                args.train_trades = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?
            }
            "--trade-days" => {
                args.trade_days = value.parse().with_context(bad)?;
            }
            "--min-fit-pairs" => {
                args.min_fit_pairs = value.parse().with_context(bad)?;
                ensure!(args.min_fit_pairs > 0, "--min-fit-pairs must be at least 1");
            }
            "--reserve-trade-days" => {
                args.reserve_trade_days = value.parse().with_context(bad)?;
                ensure!(
                    args.reserve_trade_days > 0,
                    "--reserve-trade-days must be at least 1"
                );
            }
            "--first-choice" => args.first_choice = Some(value.parse().with_context(bad)?),
            "--require-fit" => {
                let n: u8 = value.parse().with_context(bad)?;
                ensure!(n <= 1, "--require-fit must be 0 or 1");
                args.require_fit = n == 1;
            }
            "--max-rel-err" => {
                args.max_rel_err = value.parse().with_context(bad)?;
                ensure!(
                    args.max_rel_err > 0.0 && args.max_rel_err <= 1.0,
                    "--max-rel-err must be in (0, 1], e.g. 0.05 for 5%"
                );
            }
            "--max-finalists" => args.max_finalists = value.parse().with_context(bad)?,
            "--memory-gb" => args.memory_gb = value.parse().with_context(bad)?,
            "--max-fit-pairs" => args.max_fit_pairs = value.parse().with_context(bad)?,
            "--threads" => {
                args.threads = value.parse().with_context(bad)?;
                ensure!(args.threads > 0, "--threads must be at least 1");
            }
            "--verify" => args.verify = value.parse().with_context(bad)?,
            "--refine-exits" => {
                let n: u8 = value.parse().with_context(bad)?;
                ensure!(n <= 1, "--refine-exits must be 0 or 1");
                args.refine_exits = n == 1;
            }
            "--ladders" => args.ladders = parse_groups(&value).with_context(bad)?,
            "--shares" => args.shares = parse_groups(&value).with_context(bad)?,
            "--breakeven" => {
                args.breakeven = match value.as_str() {
                    "yes" => vec![true],
                    "no" => vec![false],
                    "both" => vec![false, true],
                    _ => bail!("--breakeven must be yes, no, or both"),
                }
            }
            "--exit-alts" => args.exit_alts = value.parse().with_context(bad)?,
            "--balance-usdt" => args.balance_usdt = value.parse().with_context(bad)?,
            "--config" => args.config = value.into(),
            "--data" => args.data = value.into(),
            "--price-bars" => {
                args.price_bars = parse_price_bars(&value).with_context(bad)?;
            }
            "--price-horizon" => {
                args.price_horizon = value.parse().with_context(bad)?;
                ensure!(
                    (1..=200).contains(&args.price_horizon),
                    "--price-horizon must be 1..200"
                );
            }
            "--price-strategy" => {
                let n: u8 = value.parse().with_context(bad)?;
                ensure!(n <= 1, "--price-strategy must be 0 or 1");
                args.price_strategy = n == 1;
            }
            "--price-stops" => {
                args.price_stops = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?;
                ensure!(
                    !args.price_stops.is_empty()
                        && args.price_stops.iter().all(|s| *s > 0.0 && *s < 50.0),
                    "--price-stops must be in (0, 50)"
                );
            }
            "--price-leverage" => {
                args.price_leverage = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?;
                ensure!(
                    !args.price_leverage.is_empty()
                        && args.price_leverage.iter().all(|l| (1..=10).contains(l)),
                    "--price-leverage must be whole numbers 1..10"
                );
            }
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(
        !args.price_bars.is_empty(),
        "--price-bars needs at least one bar size (e.g. 15m,1h)"
    );
    ensure!(
        !args.train_trades.is_empty() && args.train_trades.iter().all(|n| *n >= 3),
        "--train-trades must be at least 3"
    );
    ensure!(
        args.max_finalists > 0 && args.memory_gb > 0.0,
        "--max-finalists and --memory-gb must be positive"
    );
    ensure!(
        !args.ladders.is_empty() && !args.shares.is_empty() && !args.breakeven.is_empty(),
        "--ladders, --shares, and --breakeven need values"
    );
    Ok(args)
}

const FINALISTS_CACHE_FILE: &str = "nm_finalists_cache.bin";
const FINALISTS_CACHE_MAGIC: &[u8; 4] = b"NF01";

fn finalists_cache_key(
    meta: &Meta,
    n_combos: usize,
    lookback_days: u64,
    max_finalists: usize,
    memory_gb: f64,
    day_scale: usize,
    est_pad: usize,
    max_m: usize,
) -> Value {
    json!({
        "scan_start": meta.start,
        "scan_end": meta.end,
        "n_windows": meta.windows.len(),
        "n_combos": n_combos,
        "lookback_days": lookback_days,
        "max_finalists": max_finalists,
        "memory_gb": memory_gb,
        "day_scale": day_scale,
        "est_pad": est_pad,
        "max_m": max_m,
    })
}

fn load_finalists_cache(
    scan: &std::path::Path,
    want: &Value,
    n_windows: usize,
) -> Result<Option<(Vec<Vec<u32>>, Vec<Value>)>> {
    let path = scan.join(FINALISTS_CACHE_FILE);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(None);
    };
    if bytes.len() < 8 || &bytes[0..4] != FINALISTS_CACHE_MAGIC {
        return Ok(None);
    }
    let mut i = 4usize;
    let key_len = u32::from_le_bytes(bytes[i..i + 4].try_into()?) as usize;
    i += 4;
    ensure!(i + key_len <= bytes.len(), "truncated finalists cache key in {}", path.display());
    let key: Value = serde_json::from_slice(&bytes[i..i + key_len])?;
    i += key_len;
    if key != *want {
        println!(
            "finalists cache key mismatch ({}); re-ranking",
            path.display()
        );
        return Ok(None);
    }
    ensure!(i + 4 <= bytes.len(), "truncated finalists cache header");
    let nw = u32::from_le_bytes(bytes[i..i + 4].try_into()?) as usize;
    i += 4;
    ensure!(nw == n_windows, "finalists cache n_windows {nw} != {n_windows}");
    let mut finalist_ids = Vec::with_capacity(nw);
    for _ in 0..nw {
        ensure!(i + 4 <= bytes.len(), "truncated finalists cache lens");
        let n = u32::from_le_bytes(bytes[i..i + 4].try_into()?) as usize;
        i += 4;
        let nbytes = n.checked_mul(4).context("finalists cache overflow")?;
        ensure!(i + nbytes <= bytes.len(), "truncated finalists cache ids");
        let mut ids = Vec::with_capacity(n);
        for _ in 0..n {
            ids.push(u32::from_le_bytes(bytes[i..i + 4].try_into()?));
            i += 4;
        }
        finalist_ids.push(ids);
    }
    ensure!(i + 4 <= bytes.len(), "truncated finalists cache windows_json len");
    let wlen = u32::from_le_bytes(bytes[i..i + 4].try_into()?) as usize;
    i += 4;
    ensure!(i + wlen <= bytes.len(), "truncated finalists cache windows_json");
    let windows_json: Vec<Value> = serde_json::from_slice(&bytes[i..i + wlen])?;
    ensure!(
        windows_json.len() + 1 == n_windows || windows_json.len() == n_windows.saturating_sub(1),
        "finalists cache windows_json length mismatch"
    );
    Ok(Some((finalist_ids, windows_json)))
}

fn save_finalists_cache(
    scan: &std::path::Path,
    key: &Value,
    finalist_ids: &[Vec<u32>],
    windows_json: &[Value],
) -> Result<()> {
    let path = scan.join(FINALISTS_CACHE_FILE);
    let key_bytes = serde_json::to_vec(key)?;
    let windows_bytes = serde_json::to_vec(windows_json)?;
    let mut out = Vec::with_capacity(
        16 + key_bytes.len()
            + windows_bytes.len()
            + finalist_ids.iter().map(|v| 4 + 4 * v.len()).sum::<usize>(),
    );
    out.extend_from_slice(FINALISTS_CACHE_MAGIC);
    out.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&key_bytes);
    out.extend_from_slice(&(finalist_ids.len() as u32).to_le_bytes());
    for ids in finalist_ids {
        out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
        for &id in ids {
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
    out.extend_from_slice(&(windows_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&windows_bytes);
    let mb = out.len() as f64 / 1e6;
    fs::write(&path, out)?;
    println!(
        "wrote finalists cache {} ({mb:.1} MB) — next run skips ranking unless --rerank",
        path.display()
    );
    Ok(())
}

/// Embed report JSON into the bilingual HTML template (EN + FA variants).
fn write_report_html(scan: &std::path::Path, stem: &str, report: &Value) -> Result<()> {
    let payload = serde_json::to_string(report)?.replace("</", "<\\/");
    let fill = |lang: &str| {
        let html_tag = if lang == "fa" {
            r#"<html lang="fa" dir="rtl">"#
        } else {
            r#"<html lang="en" dir="ltr">"#
        };
        REPORT_HTML
            .replace(r#"<html lang="en" dir="ltr">"#, html_tag)
            .replace("/*__TABLE_SORT__*/", TABLE_SORT)
            .replace("/*__REPORT__*/null", &payload)
            .replace("/*__LANG0__*/'en'", &format!("'{lang}'"))
    };
    let base = scan.join(stem);
    fs::write(base.with_extension("html"), fill("en"))?;
    fs::write(scan.join(format!("{stem}_fa.html")), fill("fa"))?;
    Ok(())
}

/// Rebuild HTML reports from JSON already on disk (no walk-forward recompute).
fn regenerate_html_from_json(scan: &std::path::Path) -> Result<()> {
    let mut wrote = 0usize;
    for stem in ["nm_walk_forward", "nm_walk_forward_allow_short"] {
        let json_path = scan.join(format!("{stem}.json"));
        if !json_path.is_file() {
            println!("skip (no JSON): {}", json_path.display());
            continue;
        }
        let report: Value = serde_json::from_str(&fs::read_to_string(&json_path)?)?;
        write_report_html(scan, stem, &report)?;
        println!("wrote {} / {stem}_fa.html", scan.join(format!("{stem}.html")).display());
        wrote += 1;
    }
    for entry in fs::read_dir(scan)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        if stem != "nm_price_walk_forward" && !stem.starts_with("nm_price_walk_forward_") {
            continue;
        }
        let report: Value = serde_json::from_str(&fs::read_to_string(entry.path())?)?;
        write_price_report_html(scan, stem, &report)?;
        println!(
            "wrote {} / {stem}_fa.html",
            scan.join(format!("{stem}.html")).display()
        );
        wrote += 1;
    }
    ensure!(
        wrote > 0,
        "no nm_walk_forward*.json / nm_price_walk_forward*.json found in {}",
        scan.display()
    );
    Ok(())
}

fn write_price_report_html(scan: &std::path::Path, stem: &str, report: &Value) -> Result<()> {
    let payload = serde_json::to_string(report)?.replace("</", "<\\/");
    let fill = |lang: &str| {
        let html_tag = if lang == "fa" {
            r#"<html lang="fa" dir="rtl">"#
        } else {
            r#"<html lang="en" dir="ltr">"#
        };
        PRICE_REPORT_HTML
            .replace(r#"<html lang="en" dir="ltr">"#, html_tag)
            .replace("/*__TABLE_SORT__*/", TABLE_SORT)
            .replace("/*__REPORT__*/null", &payload)
            .replace("/*__LANG0__*/'en'", &format!("'{lang}'"))
    };
    fs::write(scan.join(format!("{stem}.html")), fill("en"))?;
    fs::write(scan.join(format!("{stem}_fa.html")), fill("fa"))?;
    Ok(())
}

#[derive(Deserialize)]
struct Window {
    /// `YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS` (UTC).
    start: String,
    end: String,
    file: String,
}

/// window_scan's meta.json.
#[derive(Deserialize)]
struct Meta {
    start: NaiveDate,
    end: NaiveDate,
    days: u64,
    /// Outer window length in hours (from window_scan --hours / --days). Default = days×24.
    #[serde(default)]
    hours: Option<u64>,
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    tp: Vec<f64>,
    sl: Vec<f64>,
    /// Missing in older scans: treated as `[1]` (no leverage axis).
    #[serde(default = "default_leverage")]
    leverage: Vec<u32>,
    combinations: usize,
    every_pair: usize,
    windows: Vec<Window>,
    size_btc: f64,
    taker_fee_bps: f64,
    maker_fee_bps: f64,
    /// Staged scans (scan_version 2+): file group index → original timeframe×pair group id.
    #[serde(default)]
    survivor_groups: Option<Vec<usize>>,
    /// scan_version 3: scale-out templates on the TP axis (index = profile).
    #[serde(default)]
    exit_profiles: Option<Vec<ExitProfile>>,
    #[serde(default)]
    scan_version: Option<u32>,
}

fn default_leverage() -> Vec<u32> {
    vec![1]
}

impl Meta {
    fn window_hours(&self) -> u64 {
        self.hours.unwrap_or(self.days.saturating_mul(24)).max(1)
    }
}

fn ms_day(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn parse_window_ms(s: &str) -> Result<i64> {
    if let Ok(d) = s.parse::<NaiveDate>() {
        return Ok(ms_day(d));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt.and_utc().timestamp_millis());
        }
    }
    bail!("bad window time {s:?} (want YYYY-MM-DD or YYYY-MM-DD HH:MM:SS)")
}

fn ms(day: NaiveDate) -> i64 {
    ms_day(day)
}

fn utc(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Test window size for a training window of `n` trades.
fn test_size(n: usize) -> usize {
    n / 3
}

/// Every combination window_scan replayed: id = file_group * settings + setting.
/// For staged scans, `survivor_groups[file_group]` is the original timeframe×pair group.
struct Space {
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    pairs: Vec<(usize, usize)>,
    grid: Grid,
    survivor_groups: Option<Vec<usize>>,
    /// When set (scan_version 3), TP index selects an [`ExitProfile`] instead of a fixed %.
    exit_profiles: Option<Vec<ExitProfile>>,
}

impl Space {
    /// (timeframe index, pair index, take-profit/profile index, stop-loss index, leverage index).
    fn decode(&self, id: u32) -> (usize, usize, usize, usize, usize) {
        let id = id as usize;
        let (file_group, s) = (id / self.grid.len(), id % self.grid.len());
        let group = self
            .survivor_groups
            .as_ref()
            .map(|sg| sg[file_group])
            .unwrap_or(file_group);
        let (tp, sl, lev) = self.grid.split(s);
        (
            group / self.pairs.len(),
            group % self.pairs.len(),
            tp,
            sl,
            lev,
        )
    }

    fn group(&self, id: u32) -> usize {
        let file_group = id as usize / self.grid.len();
        self.survivor_groups
            .as_ref()
            .map(|sg| sg[file_group])
            .unwrap_or(file_group)
    }

    fn leverage(&self, id: u32) -> u32 {
        let (_, _, _, _, li) = self.decode(id);
        self.grid.lev[li]
    }

    fn label(&self, id: u32) -> String {
        self.label_with_exits(id, &self.exits(id))
    }

    fn exits(&self, id: u32) -> Exits {
        let (_, _, tp, sl, _) = self.decode(id);
        let stop = self.grid.sl[sl];
        if let Some(profiles) = &self.exit_profiles {
            profiles[tp].to_exits(stop)
        } else {
            Exits {
                stop_pct: stop,
                targets: vec![(self.grid.tp[tp], 1.0)],
                breakeven: false,
            }
        }
    }

    /// RSI/TF/lev prefix plus a full [`Exits`] label (supports multi-TP ladders).
    fn label_with_exits(&self, id: u32, exits: &Exits) -> String {
        let (f, p, tp, _, li) = self.decode(id);
        let (i, j) = self.pairs[p];
        let profile = self
            .exit_profiles
            .as_ref()
            .map(|ps| format!("{} · ", ps[tp].label))
            .unwrap_or_default();
        format!(
            "{}m RSI {}/{} · {profile}{} · {}x",
            self.timeframes[f],
            self.lengths[i],
            self.lengths[j],
            exits.label(),
            self.grid.lev[li]
        )
    }
}

fn exits_to_json(exits: &Exits) -> Value {
    json!({
        "stop_pct": exits.stop_pct,
        "targets": exits.targets.iter().map(|(p, s)| json!([p, s])).collect::<Vec<_>>(),
        "breakeven": exits.breakeven,
        "label": exits.label(),
    })
}

/// Exit catalog for one combo: scan single-TP plus R-ladders on that SL.
fn exit_catalog(
    space: &Space,
    id: u32,
    ladders: &[Vec<f64>],
    shares: &[Vec<f64>],
    breakeven: &[bool],
    size_steps: i64,
) -> Result<Vec<Exits>> {
    let single = space.exits(id);
    let stop = single.stop_pct;
    let steps = size_steps * i64::from(space.leverage(id));
    let mut out = vec![single.clone()];
    let mut seen = vec![single.label()];
    for e in exit_settings(&[stop], ladders, shares, breakeven, steps)?
        .into_iter()
        .flatten()
    {
        let lab = e.label();
        if seen.iter().any(|s| s == &lab) {
            continue;
        }
        seen.push(lab);
        out.push(e);
    }
    Ok(out)
}

fn stats_json(s: &WindowStats) -> Value {
    stats_json_scored(s, None)
}

fn stats_json_scored(s: &WindowStats, model: Option<&ScoreModel>) -> Value {
    let learned = model.and_then(|m| m.predict(s));
    json!({
        "profit": round2(f64::from(s.profit)),
        "drop": round2(f64::from(s.drop)),
        "worst": round2(f64::from(s.worst)),
        "hold_hours": round2(f64::from(s.hold_hours)),
        "trades": s.trades,
        "wins": s.wins,
        "win_rate": round2(100.0 * s.win_rate()),
        "trusted_win_rate": round2(100.0 * s.trusted_win_rate()),
        "score": s.score().map(|x| (x * 1000.0).round() / 1000.0),
        "learned_score": learned.map(|x| (x * 1000.0).round() / 1000.0),
    })
}

fn score_model_json(m: &ScoreModel, max_rel_err: f64) -> Value {
    let ok = !m.cold_start && m.score_rel_err < max_rel_err;
    json!({
        "weights": m.weights.iter().map(|w| round2(*w * 1000.0) / 1000.0).collect::<Vec<_>>(),
        "weight_sum": round2(m.weights.iter().sum::<f64>() * 1000.0) / 1000.0,
        "budget": SCORE_BUDGET,
        "intercept": 0.0,
        "score_mae": round2(m.score_mae * 1000.0) / 1000.0,
        "score_rel_err": round2(m.score_rel_err * 10_000.0) / 10_000.0,
        "samples": m.samples,
        "cold_start": m.cold_start,
        "score_ok": ok,
        "max_rel_err": max_rel_err,
    })
}

/// When the best N/M cannot fill a full M-trade OOS block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShortOos {
    /// End the path (original).
    Stop,
    /// Still “run the fold”: take remaining trades (or empty / 0 PnL) and continue.
    AllowShort,
}

impl ShortOos {
    fn label(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::AllowShort => "allow_short",
        }
    }
}

struct Step2Out {
    windows: Vec<Value>,
    traded: Vec<u32>,
    stop_reason: String,
    balance: f64,
    short_folds: usize,
    empty_folds: usize,
    /// Last combo + exits actually traded (for Nautilus verify).
    last_exits: Option<(u32, Exits)>,
}

/// Ladder refine settings for Step 2.
struct ExitRefine<'a> {
    ladders: &'a [Vec<f64>],
    shares: &'a [Vec<f64>],
    breakeven: &'a [bool],
    exit_alts: usize,
}

fn next_exit_after(chains: &[Chain], at: i64) -> Option<i64> {
    chains
        .iter()
        .filter_map(|c| c.trades.iter().find(|t| t.exit_ms > at))
        .map(|t| t.exit_ms)
        .min()
}

/// Step 2: pick best N/M on last N trades, take next M (or remaining / empty if AllowShort).
/// When `refine` is set, the chosen combo's exit is re-scored with TP ladders and the best IS
/// exit is used for OOS (scan single-TP remains one candidate).
fn run_step2(
    chains: &[Chain],
    space: &Space,
    mk: &Market1m<'_>,
    size_btc: f64,
    first_choice_ms: i64,
    end_ms: i64,
    lookback_ms: i64,
    lookback_days: u64,
    start_balance: f64,
    policy: ShortOos,
    refine: Option<&ExitRefine<'_>>,
    model: &ScoreModel,
) -> Result<Step2Out> {
    let mut last_check: Option<(u32, Exits)> = None;
    let mut at = first_choice_ms;
    let mut balance = start_balance;
    let mut windows = Vec::new();
    let mut traded: Vec<u32> = Vec::new();
    let mut short_folds = 0usize;
    let mut empty_folds = 0usize;
    let stop_reason: String;
    loop {
        if at >= end_ms {
            stop_reason = "the data ended".into();
            break;
        }
        let mut scored: Vec<(usize, WindowStats, f64)> = Vec::new();
        for (k, c) in chains.iter().enumerate() {
            let range = training(
                |i| c.trades[i].exit_ms,
                c.trades.len(),
                at,
                c.n,
                lookback_ms,
            );
            if range.len() < MIN_TRADES as usize {
                continue;
            }
            let stats = chain_stats(&c.trades[range]).with_score_clock(at, lookback_ms);
            // Skip weak/red IS: only N/M modes with training profit above the floor may be chosen.
            if f64::from(stats.profit) <= MIN_IS_PROFIT {
                continue;
            }
            let Some(score) = model.predict(&stats) else { continue };
            scored.push((k, stats, score));
        }
        if scored.is_empty() {
            match next_exit_after(chains, at) {
                Some(next) => {
                    at = next;
                    continue;
                }
                None => {
                    stop_reason = format!(
                        "no window pair with IS profit > {MIN_IS_PROFIT} USDT (≥{MIN_TRADES} trades in lookback {lookback_days}d) after {}",
                        utc(at)
                    );
                    break;
                }
            }
        }
        scored.sort_by(|a, b| {
            b.2.total_cmp(&a.2)
                .then(b.1.profit.total_cmp(&a.1.profit))
                .then(a.0.cmp(&b.0))
        });
        let eligible = scored.len();
        let (k, train, train_score) = scored[0];
        let c = &chains[k];
        let first = c.trades.partition_point(|t| t.entry_ms < at);
        let available = c.trades.len().saturating_sub(first);
        if available < c.m {
            if policy == ShortOos::Stop {
                stop_reason = format!(
                    "the chosen window pair {}/{} has fewer than {} trades left after {}",
                    c.n,
                    c.m,
                    c.m,
                    utc(at)
                );
                break;
            }
            // AllowShort: fold still runs with whatever is left (maybe empty).
            if available == 0 {
                empty_folds += 1;
                short_folds += 1;
                let Some(next) = next_exit_after(chains, at) else {
                    stop_reason = format!(
                        "empty fold for {}/{} at {} and no later trades in any window pair",
                        c.n,
                        c.m,
                        utc(at)
                    );
                    break;
                };
                println!(
                    "  [{}] cycle {} at {}: {}/{} empty fold (wanted M={}, 0 left); advance to {}",
                    policy.label(),
                    windows.len() + 1,
                    utc(at),
                    c.n,
                    c.m,
                    c.m,
                    utc(next)
                );
                windows.push(json!({
                    "pick_at": utc(at),
                    "train_trades": c.n,
                    "test_trades": c.m,
                    "oos_trades_taken": 0,
                    "short_oos": true,
                    "empty_fold": true,
                    "label": "",
                    "train": stats_json(&train),
                    "test_from": utc(at),
                    "test_to": utc(next),
                    "test": stats_json(&WindowStats::default()),
                    "test_net": 0.0,
                    "balance": round2(balance),
                    "buy_and_hold": 0.0,
                    "eligible": eligible,
                }));
                at = next;
                continue;
            }
            short_folds += 1;
        }
        let combo = c.trades[first].combo;
        let mut exits = space.exits(combo);
        let mut exits_train = train;
        let mut exits_score = train_score;
        let mut exit_alts = Vec::new();
        let mut oos_kept: Option<Vec<(i64, i64, f64)>> = None;

        if let Some(r) = refine {
            if let Some(best) = refine_exit_for_combo(
                mk,
                combo,
                at,
                c.n,
                lookback_ms,
                r.ladders,
                r.shares,
                r.breakeven,
                r.exit_alts,
                model,
            )? {
                exits = best.exits;
                exits_train = best.train;
                exits_score = best.train_score;
                exit_alts = best.alts;
                let oos_first = best.trades.partition_point(|t| t.entry_ms < at);
                let mut rows = Vec::new();
                for k in oos_first..best.trades.len() {
                    let net = best.trades[k].cum
                        - if k == 0 {
                            0.0
                        } else {
                            best.trades[k - 1].cum
                        };
                    rows.push((best.trades[k].entry_ms, best.trades[k].exit_ms, net));
                }
                oos_kept = Some(rows);
            }
        }

        let (take, test_from, test_to, result, net, label) = if let Some(ref rows) = oos_kept {
            let take = rows.len().min(c.m);
            if take == 0 {
                if policy == ShortOos::Stop {
                    stop_reason = format!(
                        "refined exits for {} have no OOS trades after {}",
                        space.label_with_exits(combo, &exits),
                        utc(at)
                    );
                    break;
                }
                // Fall through to short/empty via next exit on the chain.
                let Some(next) = next_exit_after(chains, at) else {
                    stop_reason = format!(
                        "refined exits empty OOS at {} and no later chain trades",
                        utc(at)
                    );
                    break;
                };
                empty_folds += 1;
                short_folds += 1;
                windows.push(json!({
                    "pick_at": utc(at),
                    "train_trades": c.n,
                    "test_trades": c.m,
                    "oos_trades_taken": 0,
                    "short_oos": true,
                    "empty_fold": true,
                    "label": space.label_with_exits(combo, &exits),
                    "exits": exits_to_json(&exits),
                    "exit_alts": exit_alts,
                    "train": stats_json(&train),
                    "exits_train": stats_json(&exits_train),
                    "test_from": utc(at),
                    "test_to": utc(next),
                    "test": stats_json(&WindowStats::default()),
                    "test_net": 0.0,
                    "balance": round2(balance),
                    "buy_and_hold": 0.0,
                    "eligible": eligible,
                }));
                at = next;
                continue;
            }
            let slice = &rows[..take];
            let mut tally = Tally::default();
            let mut net = 0.0;
            for &(entry_ms, exit_ms, n) in slice {
                tally.add(n, exit_ms - entry_ms);
                net += n;
            }
            (
                take,
                slice[0].0,
                slice[take - 1].1,
                tally.stats(),
                net,
                space.label_with_exits(combo, &exits),
            )
        } else {
            let take = available.min(c.m);
            let test = &c.trades[first..first + take];
            let result = chain_stats(test);
            let net: f64 = test.iter().map(|t| t.net).sum();
            let mut used: Vec<u32> = test.iter().map(|t| t.combo).collect();
            used.dedup();
            let label = used
                .iter()
                .map(|&id| space.label(id))
                .collect::<Vec<_>>()
                .join(" then ");
            (
                take,
                test[0].entry_ms,
                test[take - 1].exit_ms,
                result,
                net,
                label,
            )
        };

        let short = take < c.m;
        if short && oos_kept.is_some() {
            short_folds += 1;
        }
        let bh = mk.buy_and_hold(size_btc, at, test_to);
        balance += net;
        traded.push(combo);
        last_check = Some((combo, exits.clone()));
        let train_show = if refine.is_some() && oos_kept.is_some() {
            exits_train
        } else {
            train
        };
        let score_show = if refine.is_some() && oos_kept.is_some() {
            exits_score
        } else {
            train_score
        };
        println!(
            "  [{}] cycle {} at {}: {}/{} (last {} trades {:+.2}, score {:.3}; {eligible} scored{}) → {:+.2} USDT ({} of {} won{}), balance {:.2}; BH {:+.2} · {}",
            policy.label(),
            windows.len() + 1,
            utc(at),
            c.n,
            c.m,
            train_show.trades,
            train_show.profit,
            score_show,
            if short {
                format!(", short OOS took {take}/{}", c.m)
            } else {
                String::new()
            },
            net,
            result.wins,
            take,
            if short { " — fold ran short" } else { "" },
            balance,
            bh,
            label
        );
        windows.push(json!({
            "pick_at": utc(at),
            "train_trades": c.n,
            "test_trades": c.m,
            "oos_trades_taken": take,
            "short_oos": short,
            "empty_fold": false,
            "label": label,
            "exits": exits_to_json(&exits),
            "exit_alts": exit_alts,
            "train": stats_json(&train),
            "exits_train": stats_json(&train_show),
            "test_from": utc(test_from),
            "test_to": utc(test_to),
            "test": stats_json(&result),
            "test_net": round2(net),
            "balance": round2(balance),
            "buy_and_hold": round2(bh),
            "eligible": eligible,
        }));
        if balance <= 0.0 {
            stop_reason = "the account ran out of money".into();
            break;
        }
        at = test_to;
    }
    Ok(Step2Out {
        windows,
        traded,
        stop_reason,
        balance,
        short_folds,
        empty_folds,
        last_exits: last_check,
    })
}

/// One combination over one window, from window_scan.
#[derive(Debug, Clone, Copy)]
struct Scored {
    id: u32,
    stats: WindowStats,
    score: f64,
}

/// Highest score, then highest profit, then the lowest id first.
fn better(a: &Scored, b: &Scored) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then(b.stats.profit.total_cmp(&a.stats.profit))
        .then(a.id.cmp(&b.id))
}

/// The `keep` best combinations of one of window_scan's files, best first, with the number of
/// combinations that traded, that had a score and that made money.
fn rank(
    path: &std::path::Path,
    n_combos: usize,
    keep: usize,
    model: &ScoreModel,
) -> Result<(Vec<Scored>, usize, usize, usize)> {
    let file = File::open(path).with_context(|| path.display().to_string())?;
    ensure!(
        file.metadata()?.len() == (n_combos * RECORD) as u64,
        "{} has the wrong size",
        path.display()
    );
    let chunk = 1 << 20;
    let mut buf = vec![0u8; chunk * RECORD];
    let mut kept: Vec<Scored> = Vec::with_capacity(2 * keep.min(1 << 24));
    let mut worst: Option<Scored> = None;
    let (mut traded, mut eligible, mut profitable) = (0, 0, 0);
    for first in (0..n_combos).step_by(chunk) {
        let n = chunk.min(n_combos - first);
        file.read_exact_at(&mut buf[..n * RECORD], (first * RECORD) as u64)?;
        for k in 0..n {
            let stats = WindowStats::from_bytes(&buf[k * RECORD..(k + 1) * RECORD]);
            if stats.trades == 0 {
                continue;
            }
            traded += 1;
            profitable += usize::from(stats.profit > 0.0);
            let Some(score) = model.predict(&stats) else { continue };
            eligible += 1;
            let s = Scored {
                id: (first + k) as u32,
                stats,
                score,
            };
            if worst.is_some_and(|w| better(&s, &w) != Ordering::Less) {
                continue;
            }
            kept.push(s);
            if kept.len() >= 2 * keep {
                kept.select_nth_unstable_by(keep - 1, better);
                kept.truncate(keep);
                worst = Some(kept[keep - 1]);
            }
        }
    }
    kept.sort_by(better);
    kept.truncate(keep);
    Ok((kept, traded, eligible, profitable))
}

/// One trade of a finalist, with running totals since the first trade kept.
#[derive(Debug, Clone, Copy)]
struct Kept {
    entry_ms: i64,
    exit_ms: i64,
    /// Profit after fees of every kept trade up to and including this one.
    cum: f64,
}

/// A finalist's trades around one window: every trade closing from the window before it on,
/// and up to the longest test window entered after it.
struct Finalist {
    id: u32,
    trades: Vec<Kept>,
}

impl Finalist {
    fn net(&self, k: usize) -> f64 {
        self.trades[k].cum - if k == 0 { 0.0 } else { self.trades[k - 1].cum }
    }
}

/// The trades scored at time `at`: the last `n` closed by then, but none closed before
/// `at - lookback_ms`.
fn training(
    exits: impl Fn(usize) -> i64,
    len: usize,
    at: i64,
    n: usize,
    lookback_ms: i64,
) -> std::ops::Range<usize> {
    let count_until = |t: i64| {
        let (mut lo, mut hi) = (0, len);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if exits(mid) <= t {
                lo = mid + 1
            } else {
                hi = mid
            }
        }
        lo
    };
    let known = count_until(at);
    let first = count_until(at - lookback_ms).max(known.saturating_sub(n));
    first..known
}

/// A pick: the finalist and its training trades' results and score.
#[derive(Debug, Clone, Copy)]
struct Pick {
    finalist: usize,
    stats: WindowStats,
    score: f64,
}

/// The finalist with the best learned score on its last `n` trades closed at or before `at` and
/// after `at - lookback_ms`, with at least `MIN_TRADES` of them and IS profit > `MIN_IS_PROFIT`.
/// Losing / flat / tiny training windows are ignored (no trade).
fn pick(
    finalists: &[Finalist],
    at: i64,
    n: usize,
    lookback_ms: i64,
    model: &ScoreModel,
) -> Option<Pick> {
    let mut best: Option<(Pick, u32)> = None;
    for (k, f) in finalists.iter().enumerate() {
        let range = training(|i| f.trades[i].exit_ms, f.trades.len(), at, n, lookback_ms);
        if range.len() < MIN_TRADES as usize {
            continue;
        }
        let before = if range.start > 0 {
            f.trades[range.start - 1].cum
        } else {
            0.0
        };
        let profit = f.trades[range.end - 1].cum - before;
        // Only trade combinations that cleared the IS profit floor.
        if profit <= MIN_IS_PROFIT {
            continue;
        }
        let mut tally = Tally::default();
        for i in range {
            tally.add(f.net(i), f.trades[i].exit_ms - f.trades[i].entry_ms);
            tally.note_exit(f.trades[i].exit_ms);
        }
        let stats = tally.stats().with_score_clock(at, lookback_ms);
        let Some(score) = model.predict(&stats) else { continue };
        let wins = match best {
            None => true,
            Some((b, id)) => {
                score
                    .total_cmp(&b.score)
                    .then(stats.profit.total_cmp(&b.stats.profit))
                    .then(id.cmp(&f.id))
                    == Ordering::Greater
            }
        };
        if wins {
            best = Some((
                Pick {
                    finalist: k,
                    stats,
                    score,
                },
                f.id,
            ));
        }
    }
    best.map(|(p, _)| p)
}

/// A trade a window pair made, and the combination that made it.
#[derive(Debug, Clone, Copy)]
struct ChainTrade {
    entry_ms: i64,
    exit_ms: i64,
    net: f64,
    combo: u32,
}

/// Results of `trades` as one window (caller should `.with_score_clock(at, lookback)` for until).
fn chain_stats(trades: &[ChainTrade]) -> WindowStats {
    let mut tally = Tally::default();
    for t in trades {
        tally.add(t.net, t.exit_ms - t.entry_ms);
        tally.note_exit(t.exit_ms);
    }
    tally.stats()
}

/// One window pair's own walk-forward.
struct Chain {
    n: usize,
    m: usize,
    at: i64,
    done: bool,
    windows: Vec<f64>,
    trades: Vec<ChainTrade>,
    stop_reason: String,
}

/// Picks and trades of `chain` from its current time until `window_end`, among `finalists`.
/// Without any finalist to pick, it waits for the next window's finalists.
/// Returns IS→OOS pairs: features(last N) → budget-10 S*(next M) for score fitting.
fn advance(
    chain: &mut Chain,
    finalists: &[Finalist],
    window_end: i64,
    lookback_ms: i64,
    space: &Space,
    model: &ScoreModel,
) -> Vec<(WindowStats, f64)> {
    let mut fit_pairs = Vec::new();
    while !chain.done && chain.at < window_end {
        let (n, m, at) = (chain.n, chain.m, chain.at);
        let Some(p) = pick(finalists, at, n, lookback_ms, model) else {
            chain.at = window_end;
            break;
        };
        let f = &finalists[p.finalist];
        let first = f.trades.partition_point(|t| t.entry_ms < at);
        if first + m > f.trades.len() {
            chain.stop_reason = format!(
                "the pick {} has fewer than {m} trades left after {}",
                space.label(f.id),
                utc(at)
            );
            chain.done = true;
            break;
        }
        let mut net = 0.0;
        let mut oos = Tally::default();
        for k in first..first + m {
            let t = &f.trades[k];
            let trade_net = f.net(k);
            chain.trades.push(ChainTrade {
                entry_ms: t.entry_ms,
                exit_ms: t.exit_ms,
                net: trade_net,
                combo: f.id,
            });
            net += trade_net;
            oos.add(trade_net, t.exit_ms - t.entry_ms);
            oos.note_exit(t.exit_ms);
        }
        // Signal sample: look back on IS (N) to forecast OOS (M) budget-10 score.
        if u32::from(p.stats.trades) >= MIN_TRADES {
            let oos_at = f.trades[first + m - 1].exit_ms;
            let oos_stats = oos.stats().with_score_clock(oos_at, lookback_ms);
            if u32::from(oos_stats.trades) >= MIN_TRADES {
                fit_pairs.push((p.stats, oos_stats.target_quality()));
            }
        }
        chain.windows.push(net);
        chain.at = f.trades[first + m - 1].exit_ms;
    }
    fit_pairs
}

/// Parse comma-separated candle sizes: `5m`, `15m`, `1h`, `4h`, or raw minutes (`15`).
fn parse_price_bars(s: &str) -> Result<Vec<(String, i64)>> {
    let mut out = Vec::new();
    for raw in s.split(',') {
        let t = raw.trim().to_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        let (label, minutes) = if let Some(rest) = t.strip_suffix('h') {
            let h: i64 = rest.parse().with_context(|| format!("bad bar '{raw}'"))?;
            ensure!(h > 0, "bar hours must be > 0 ({raw})");
            (format!("{h}h"), h * 60)
        } else if let Some(rest) = t.strip_suffix('m') {
            let m: i64 = rest.parse().with_context(|| format!("bad bar '{raw}'"))?;
            ensure!(m > 0, "bar minutes must be > 0 ({raw})");
            let label = if m % 60 == 0 {
                format!("{}h", m / 60)
            } else {
                format!("{m}m")
            };
            (label, m)
        } else {
            let m: i64 = t.parse().with_context(|| format!("bad bar '{raw}' (use 15m or 1h)"))?;
            ensure!(m > 0, "bar minutes must be > 0 ({raw})");
            let label = if m % 60 == 0 {
                format!("{}h", m / 60)
            } else {
                format!("{m}m")
            };
            (label, m)
        };
        ensure!(
            minutes >= 1 && minutes <= 24 * 60,
            "bar '{raw}' out of range (1m … 24h)"
        );
        let bar_ms = minutes * 60_000;
        if out.iter().any(|(_, ms)| *ms == bar_ms) {
            continue;
        }
        out.push((label, bar_ms));
    }
    ensure!(!out.is_empty(), "no bar sizes in '{s}'");
    Ok(out)
}

/// Everything needed to replay combinations again.
struct Market1m<'a> {
    space: &'a Space,
    tape: &'a Tape,
    minutes: &'a [Candle],
    frames: &'a [Frame],
    costs: Costs,
}

impl Market1m<'_> {
    /// Signals of RSI pair `p` on timeframe `f`.
    fn signals(&self, f: usize, p: usize) -> Vec<(u32, i8)> {
        let (i, j) = self.space.pairs[p];
        let series = |length: usize| {
            let mut rsi = Rsi::new(length);
            self.frames[f]
                .candles
                .iter()
                .map(|c| rsi.update(c.close).unwrap_or(f64::NAN))
                .collect::<Vec<_>>()
        };
        scan::signals(
            &series(self.space.lengths[i]),
            &series(self.space.lengths[j]),
        )
    }

    /// Price of the last OKX trade at or before `at`.
    fn price_at(&self, at: i64) -> f64 {
        let bar = self.minutes.partition_point(|c| c.time_ms <= at).max(1) - 1;
        let tape = self.tape;
        let (mut lo, mut hi) = (
            tape.bar_start[bar] as usize,
            tape.bar_start
                .get(bar + 1)
                .map_or(tape.price.len(), |s| *s as usize),
        );
        while lo < hi {
            let mid = (lo + hi) / 2;
            if tape.time_ms(mid) <= at {
                lo = mid + 1
            } else {
                hi = mid
            }
        }
        f64::from(tape.price[lo.max(1) - 1]) / 10.0
    }

    fn buy_and_hold(&self, size_btc: f64, from: i64, to: i64) -> f64 {
        let (a, b) = (self.price_at(from), self.price_at(to));
        size_btc * (b - a) - size_btc * (a + b) * self.costs.taker
    }

    /// The trade-by-trade scanner's replay of combination `id` over the whole period.
    fn replay(&self, id: u32, record: Option<&mut Record>, log: &mut Vec<ClosedTrade>) {
        let exits = self.space.exits(id);
        self.replay_exits(id, &exits, record, log);
    }

    fn replay_exits(
        &self,
        id: u32,
        exits: &Exits,
        record: Option<&mut Record>,
        log: &mut Vec<ClosedTrade>,
    ) {
        let (f, p, ..) = self.space.decode(id);
        let mut costs = self.costs;
        costs.steps *= i64::from(self.space.leverage(id));
        let frame = &self.frames[f];
        let closes = CloseRanges::new(&frame.candles.iter().map(|c| c.close).collect::<Vec<_>>());
        let market = Market {
            candles: &frame.candles,
            tape: self.tape,
            bar_start: &frame.bar_start,
            closes: &closes,
        };
        let signals = self.signals(f, p);
        match record {
            Some(r) => {
                scan::simulate(
                    (0, 0, 0),
                    &signals,
                    Some(exits),
                    &market,
                    0..frame.candles.len(),
                    costs,
                    Some(r),
                );
            }
            None => {
                scan::simulate_logged(
                    (0, 0, 0),
                    &signals,
                    Some(exits),
                    &market,
                    0..frame.candles.len(),
                    costs,
                    log,
                );
            }
        }
    }

    /// Closed trades as [`Kept`] with cumulative nets (leverage already in costs.steps).
    fn kept_trades(&self, id: u32, exits: &Exits) -> Vec<Kept> {
        let (f, ..) = self.space.decode(id);
        let mut log = Vec::new();
        self.replay_exits(id, exits, None, &mut log);
        let frame = &self.frames[f];
        let mut cum = 0.0;
        log.into_iter()
            .map(|t| {
                cum += f64::from(t.net);
                Kept {
                    entry_ms: frame.candles[t.entry_bar as usize].time_ms,
                    exit_ms: t.exit_ms,
                    cum,
                }
            })
            .collect()
    }
}

/// Score `exits` on the last `n` closes within lookback; returns stats, score, and all kept trades.
fn score_exits_is(
    mk: &Market1m<'_>,
    id: u32,
    exits: &Exits,
    at: i64,
    n: usize,
    lookback_ms: i64,
    model: &ScoreModel,
) -> Option<(WindowStats, f64, Vec<Kept>)> {
    let trades = mk.kept_trades(id, exits);
    let range = training(|i| trades[i].exit_ms, trades.len(), at, n, lookback_ms);
    if range.len() < MIN_TRADES as usize {
        return None;
    }
    let before = if range.start > 0 {
        trades[range.start - 1].cum
    } else {
        0.0
    };
    let profit = trades[range.end - 1].cum - before;
    if profit <= MIN_IS_PROFIT {
        return None;
    }
    let mut tally = Tally::default();
    for i in range.clone() {
        let net = trades[i].cum - if i == 0 { 0.0 } else { trades[i - 1].cum };
        tally.add(net, trades[i].exit_ms - trades[i].entry_ms);
        tally.note_exit(trades[i].exit_ms);
    }
    let stats = tally.stats().with_score_clock(at, lookback_ms);
    let score = model.predict(&stats)?;
    Some((stats, score, trades))
}

/// Best exit for `id` at pick time, plus runner-up alts for the report.
struct RefinedExit {
    exits: Exits,
    train: WindowStats,
    train_score: f64,
    /// All closed trades with this exit (for OOS slice).
    trades: Vec<Kept>,
    alts: Vec<Value>,
}

fn refine_exit_for_combo(
    mk: &Market1m<'_>,
    id: u32,
    at: i64,
    n: usize,
    lookback_ms: i64,
    ladders: &[Vec<f64>],
    shares: &[Vec<f64>],
    breakeven: &[bool],
    exit_alts: usize,
    model: &ScoreModel,
) -> Result<Option<RefinedExit>> {
    let catalog = exit_catalog(
        mk.space,
        id,
        ladders,
        shares,
        breakeven,
        mk.costs.steps,
    )?;
    let mut ranked: Vec<(Exits, WindowStats, f64, Vec<Kept>)> = Vec::new();
    for exits in catalog {
        if let Some((stats, score, trades)) =
            score_exits_is(mk, id, &exits, at, n, lookback_ms, model)
        {
            ranked.push((exits, stats, score, trades));
        }
    }
    if ranked.is_empty() {
        return Ok(None);
    }
    ranked.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then(b.1.profit.total_cmp(&a.1.profit))
            .then(a.0.label().cmp(&b.0.label()))
    });
    let (exits, train, train_score, trades) = ranked.remove(0);
    let alts: Vec<Value> = ranked
        .into_iter()
        .take(exit_alts)
        .map(|(e, s, sc, _)| {
            json!({
                "exits": exits_to_json(&e),
                "train": stats_json(&s),
                "score": (sc * 1000.0).round() / 1000.0,
                "label": mk.space.label_with_exits(id, &e),
            })
        })
        .collect();
    Ok(Some(RefinedExit {
        exits,
        train,
        train_score,
        trades,
        alts,
    }))
}

/// How many shortlist tops per day get exit refine (full SHOWN would be too slow).
const SHORTLIST_EXIT_REFINE: usize = 5;

/// Attach best exit + alts to shortlist tops (same catalog as Step-2 refine).
fn enrich_shortlist_exits(
    windows_json: &mut [Value],
    mk: &Market1m<'_>,
    lookback_ms: i64,
    n: usize,
    refine: &ExitRefine<'_>,
    model: &ScoreModel,
) -> Result<()> {
    for w in windows_json.iter_mut() {
        let traded_in = w["traded_in"]
            .as_str()
            .context("shortlist window missing traded_in")?;
        let at_s = traded_in
            .split(" → ")
            .next()
            .context("bad traded_in")?;
        let at = parse_window_ms(at_s)?;
        let Some(tops) = w["top"].as_array_mut() else {
            continue;
        };
        for row in tops.iter_mut().take(SHORTLIST_EXIT_REFINE) {
            let id = row["id"].as_u64().context("shortlist top missing id")? as u32;
            let Some(best) = refine_exit_for_combo(
                mk,
                id,
                at,
                n,
                lookback_ms,
                refine.ladders,
                refine.shares,
                refine.breakeven,
                refine.exit_alts,
                model,
            )?
            else {
                continue;
            };
            row["label"] = json!(mk.space.label_with_exits(id, &best.exits));
            row["exits"] = exits_to_json(&best.exits);
            row["exit_alts"] = Value::Array(best.alts);
            row["exits_train"] = stats_json(&best.train);
        }
    }
    Ok(())
}

/// The trades of `ids` (one window's finalists): every one closing at or after tape step
/// `from_step`, and up to `after` trades entered at or after `end_step`. Built once per RSI
/// pair and timeframe, over the whole period from the first candle.
fn build_finalists(
    mk: &Market1m,
    ids: &[u32],
    from_step: u32,
    end_step: u32,
    after: usize,
    threads: usize,
) -> Vec<Finalist> {
    let space = mk.space;
    let mut order: Vec<usize> = (0..ids.len()).collect();
    order.sort_by_key(|&k| ids[k]);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (pos, &k) in order.iter().enumerate() {
        match runs.last_mut() {
            Some((_, end)) if space.group(ids[order[*end - 1]]) == space.group(ids[k]) => {
                *end = pos + 1
            }
            _ => runs.push((pos, pos + 1)),
        }
    }
    let next = AtomicUsize::new(0);
    let built: Vec<(usize, Finalist)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    let mut all: Vec<GridTrade> = Vec::new();
                    loop {
                        let r = next.fetch_add(1, AtomicOrdering::Relaxed);
                        let Some(&(from, to)) = runs.get(r) else {
                            break out;
                        };
                        let (f, p, ..) = space.decode(ids[order[from]]);
                        let entries = Entries::new(
                            &mk.signals(f, p),
                            &mk.frames[f],
                            mk.tape,
                            &space.grid,
                            mk.costs,
                        );
                        for &k in &order[from..to] {
                            let (_, _, tp, sl, li) = space.decode(ids[k]);
                            let lev = f64::from(space.grid.lev[li]);
                            all.clear();
                            entries.trades(tp, sl, |t| all.push(t));
                            let lo = all.partition_point(|t| t.exit_step < from_step);
                            let first_after =
                                all.partition_point(|t| entries.step[t.signal as usize] < end_step);
                            let hi = (first_after + after).min(all.len());
                            let mut cum = 0.0;
                            let trades = all[lo..hi.max(lo)]
                                .iter()
                                .map(|t| {
                                    cum += f64::from(t.net) * lev;
                                    Kept {
                                        entry_ms: mk
                                            .tape
                                            .time_ms(entries.step[t.signal as usize] as usize),
                                        exit_ms: mk.tape.time_ms(t.exit_step as usize),
                                        cum,
                                    }
                                })
                                .collect();
                            out.push((k, Finalist { id: ids[k], trades }));
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    let mut finalists: Vec<Option<Finalist>> = (0..ids.len()).map(|_| None).collect();
    for (k, f) in built {
        finalists[k] = Some(f);
    }
    finalists
        .into_iter()
        .map(|f| f.expect("every finalist built"))
        .collect()
}

struct PriceBarRun {
    label: String,
    bar_ms: i64,
    locked: bool,
    fit_pairs: usize,
    return_mae: f64,
    price_rel_err: f64,
    baseline_rel_err: f64,
    total_pnl: f64,
    n_signals: usize,
    report: Value,
}

/// Candle OHLC price path: grid of bar sizes → fit each → pick best locked (lowest return-MAE).
fn run_price_path(
    args: &Args,
    meta: &Meta,
    mk: &Market1m<'_>,
    train_ws: &[usize],
    train_cutoff_ms: i64,
    trade_start_ms: i64,
    trade_end_ms: i64,
    lookback_days: u64,
) -> Result<()> {
    let bar_list = args
        .price_bars
        .iter()
        .map(|(l, _)| l.as_str())
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "\n=== PRICE PATH (candle grid) ===\n\
         Equation: (close_t/close_{{t-1}})-1 = a·prev_ret + b·range + c·body + d·typical_dev\n\
         pred_close = close_{{t-1}}·(1+Σαf)   |  OHLC from market tape\n\
         Bars: [{bar_list}]  ·  gate return-MAE < {:.0}% + beat pred=ref · ≥{} points\n\
         TRADE {} → {}",
        args.max_rel_err * 100.0,
        args.min_fit_pairs,
        utc(trade_start_ms),
        utc(trade_end_ms)
    );

    let mut runs: Vec<PriceBarRun> = Vec::new();
    for (label, bar_ms) in &args.price_bars {
        match run_one_price_bar(
            args,
            meta,
            mk,
            train_ws,
            train_cutoff_ms,
            trade_start_ms,
            trade_end_ms,
            lookback_days,
            label,
            *bar_ms,
        ) {
            Ok(run) => {
                println!(
                    "  [{label}] done: locked={} return_mae={:.3}% pnl={:+.2} signals={}",
                    run.locked,
                    run.return_mae * 100.0,
                    run.total_pnl,
                    run.n_signals
                );
                runs.push(run);
            }
            Err(e) => {
                println!("  [{label}] skipped: {e:#}");
            }
        }
    }
    ensure!(!runs.is_empty(), "price bar grid produced no usable runs");

    let grid_summary: Vec<Value> = runs
        .iter()
        .map(|r| {
            json!({
                "bar": r.label,
                "bar_ms": r.bar_ms,
                "locked": r.locked,
                "fit_pairs": r.fit_pairs,
                "return_mae": r.return_mae,
                "price_rel_err": r.price_rel_err,
                "baseline_rel_err": r.baseline_rel_err,
                "total_pnl": r.total_pnl,
                "signals": r.n_signals,
            })
        })
        .collect();

        // Prefer locked bars with the best OOS strategy PnL (after fees); tie-break on return_mae.
    let best_idx = {
        let locked: Vec<usize> = runs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.locked)
            .map(|(i, _)| i)
            .collect();
        let pick = |idxs: Vec<usize>| {
            idxs.into_iter()
                .max_by(|&a, &b| {
                    runs[a]
                        .total_pnl
                        .partial_cmp(&runs[b].total_pnl)
                        .unwrap_or(Ordering::Equal)
                        .then_with(|| {
                            runs[b]
                                .return_mae
                                .partial_cmp(&runs[a].return_mae)
                                .unwrap_or(Ordering::Equal)
                        })
                })
                .unwrap()
        };
        if !locked.is_empty() {
            pick(locked)
        } else if args.require_fit {
            bail!(
                "price TRAIN never LOCKed on any bar in [{bar_list}] (need return_mae < {:.0}% and ≥{} points)",
                args.max_rel_err * 100.0,
                args.min_fit_pairs
            );
        } else {
            println!("  warning: no bar LOCKed; --require-fit 0 → pick best OOS strategy PnL anyway");
            pick((0..runs.len()).collect())
        }
    };
let selected_label = runs[best_idx].label.clone();
    for (i, run) in runs.iter_mut().enumerate() {
        run.report["meta"]["bar_grid"] = json!(grid_summary);
        run.report["meta"]["selected_bar"] = json!(&selected_label);
        run.report["meta"]["is_selected"] = json!(i == best_idx);
        let stem = if i == best_idx {
            "nm_price_walk_forward".to_string()
        } else {
            format!("nm_price_walk_forward_{}", run.label)
        };
        let base = args.scan.join(&stem);
        fs::write(
            base.with_extension("json"),
            serde_json::to_string_pretty(&run.report)?,
        )?;
        write_price_report_html(&args.scan, &stem, &run.report)?;
        println!(
            "  wrote {} ({})",
            args.scan.join(format!("{stem}.html")).display(),
            if i == best_idx { "SELECTED" } else { "grid" }
        );
    }

    let best = &runs[best_idx];
    println!(
        "PRICE PATH done: selected {} → {:+.2} USDT · return_mae={:.3}% · {} signals",
        best.label,
        best.total_pnl,
        best.return_mae * 100.0,
        best.n_signals
    );
    Ok(())
}

fn run_one_price_bar(
    args: &Args,
    meta: &Meta,
    mk: &Market1m<'_>,
    train_ws: &[usize],
    train_cutoff_ms: i64,
    trade_start_ms: i64,
    trade_end_ms: i64,
    lookback_days: u64,
    label: &str,
    bar_ms: i64,
) -> Result<PriceBarRun> {
    let typical = mk.price_at(train_cutoff_ms);
    let mut price_model = PriceModel::cold_start(typical);
    let mut fit_pairs: Vec<PriceSample> = Vec::new();
    let mut score_models_json = Vec::new();
    let mut locked = false;
    let mut lock_ms = train_cutoff_ms;

    let bars = aggregate_candles(mk.minutes, bar_ms);
    let all_samples = samples_from_candles(&bars);
    ensure!(
        all_samples.len() >= MIN_FIT_SAMPLES,
        "not enough {label} candle samples on tape ({})",
        all_samples.len()
    );

    println!(
        "\n--- price bar {label} ({} bars · {} samples) ---",
        bars.len(),
        all_samples.len()
    );
    println!(
        "  fit near→far ({} train days, newest first = {})…",
        train_ws.len(),
        meta.windows[*train_ws.last().unwrap()].start
    );

    for &w in train_ws.iter().rev() {
        let started = Instant::now();
        let window = &meta.windows[w];
        let (start, end) = (parse_window_ms(&window.start)?, parse_window_ms(&window.end)?);
        if end > train_cutoff_ms {
            continue;
        }
        let batch: Vec<PriceSample> = all_samples
            .iter()
            .copied()
            .filter(|s| s.at_ms >= start && s.at_ms < end)
            .collect();
        let added = batch.len();
        if added == 0 {
            continue;
        }
        fit_pairs.extend(batch);
        if args.max_fit_pairs > 0 && fit_pairs.len() > args.max_fit_pairs {
            fit_pairs.drain(args.max_fit_pairs..);
        }

        let pairs_ok = fit_pairs.len() >= args.min_fit_pairs;
        if fit_pairs.len() < MIN_FIT_SAMPLES {
            println!(
                "  {}: {label} +{added} → points={} (need ≥{MIN_FIT_SAMPLES}) in {:.0}s",
                window.start,
                fit_pairs.len(),
                started.elapsed().as_secs_f64()
            );
            continue;
        }
        match PriceModel::fit(&fit_pairs, Some(&price_model), typical) {
            Ok(m) => {
                let prev = price_model.rel_err;
                price_model = m;
                println!(
                    "  {}: {label} +{added} → points={}/{} | return_mae={:.3}% (was {:.3}%) price_err={:.3}% baseline={:.3}% in {:.0}s",
                    window.start,
                    fit_pairs.len(),
                    args.min_fit_pairs,
                    price_model.rel_err * 100.0,
                    prev * 100.0,
                    price_model.price_rel_err * 100.0,
                    price_model.baseline_rel_err * 100.0,
                    started.elapsed().as_secs_f64()
                );
                score_models_json.push(json!({
                    "phase": "price_candle_near_to_far",
                    "bar": label,
                    "label": window.start.clone(),
                    "alphas": price_model.alphas,
                    "return_mae": price_model.rel_err,
                    "price_rel_err": price_model.price_rel_err,
                    "baseline_rel_err": price_model.baseline_rel_err,
                    "fit_pairs": fit_pairs.len(),
                    "locked": false,
                }));
                if price_model.ok(args.max_rel_err) {
                    if pairs_ok {
                        locked = true;
                        lock_ms = train_cutoff_ms;
                        println!(
                            "  {label} LOCKED (back to {}) — return_mae={:.3}% < {:.0}% beats baseline {:.3}% (n={})",
                            window.start,
                            price_model.rel_err * 100.0,
                            args.max_rel_err * 100.0,
                            price_model.baseline_rel_err * 100.0,
                            fit_pairs.len()
                        );
                        if let Some(last) = score_models_json.last_mut() {
                            last["locked"] = json!(true);
                        }
                        break;
                    }
                    println!(
                        "  … gate ok but need more points ({}/{}) — go further back",
                        fit_pairs.len(),
                        args.min_fit_pairs
                    );
                } else if price_model.rel_err < args.max_rel_err {
                    println!(
                        "  … return_mae under gate but does not beat baseline ({:.3}% ≥ {:.3}%) — go further back",
                        price_model.price_rel_err * 100.0,
                        price_model.baseline_rel_err * 100.0
                    );
                } else {
                    println!(
                        "  … return_mae still above {:.0}% — go further back",
                        args.max_rel_err * 100.0
                    );
                }
            }
            Err(e) => println!("  {}: {label} fit failed ({e}); keep going back", window.start),
        }
    }

    if !locked {
        println!(
            "  {label}: not LOCKed (cold={} return_mae={:.3}% n={})",
            price_model.cold_start,
            price_model.rel_err * 100.0,
            fit_pairs.len()
        );
    }

    let horizon_n = args.price_horizon.max(1);
    let horizons = eval_price_horizons(
        &price_model,
        &bars,
        typical,
        horizon_n,
        bar_ms,
        train_cutoff_ms,
        trade_start_ms,
        trade_end_ms,
        args.max_rel_err,
    )?;
    println!(
        "  {label} horizons 1..{horizon_n}: OOS h=1 price_err={:.3}% (base {:.3}%) · h={horizon_n} price_err={:.3}% (base {:.3}%)",
        horizons.oos_h1_price_err * 100.0,
        horizons.oos_h1_base_err * 100.0,
        horizons.oos_hmax_price_err * 100.0,
        horizons.oos_hmax_base_err * 100.0
    );
    if let Some(fwd) = horizons.path_example.as_object() {
        println!(
            "  {label} FORWARD from last candle {} (close {}) → next {horizon_n} bars:",
            fwd.get("origin").and_then(|v| v.as_str()).unwrap_or("?"),
            fwd.get("origin_close").and_then(|v| v.as_f64()).unwrap_or(f64::NAN)
        );
        if let Some(steps) = fwd.get("steps").and_then(|v| v.as_array()) {
            let show = |s: &Value| {
                let spot = s.get("spot");
                let spot_s = match spot {
                    Some(v) if v.is_null() => "n/a (future)".to_string(),
                    Some(v) => format!("{}", v.as_f64().unwrap_or(f64::NAN)),
                    None => "n/a (future)".to_string(),
                };
                println!(
                    "    +{:<2} {}  pred={}  spot={}",
                    s.get("h").and_then(|v| v.as_u64()).unwrap_or(0),
                    s.get("at").and_then(|v| v.as_str()).unwrap_or("?"),
                    s.get("predicted").and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
                    spot_s
                );
            };
            for s in steps.iter().take(3) {
                show(s);
            }
            if steps.len() > 3 {
                println!("    …");
                show(&steps[steps.len() - 1]);
            }
        }
    }

    // Chart: h=1 pred vs spot over OOS (visual only).
    let mut chart_signals = Vec::new();
    for s in all_samples
        .iter()
        .filter(|s| s.at_ms >= trade_start_ms && s.at_ms < trade_end_ms)
    {
        let pred = price_model.predict_close(s.ref_px, &s.features);
        chart_signals.push(json!({
            "at": utc(s.at_ms),
            "spot": round2(s.spot),
            "ref": round2(s.ref_px),
            "predicted": round2(pred),
            "side": "h1",
            "pnl": 0.0,
            "label": label,
            "rel_err": (pred - s.spot).abs() / s.spot.max(1e-12),
        }));
    }

    let strategy = if args.price_strategy {
        Some(run_price_strategy_grid(
            args,
            meta,
            mk,
            &price_model,
            label,
            bar_ms,
            trade_start_ms,
            trade_end_ms,
            lookback_days,
        )?)
    } else {
        None
    };
    let balance_pnl = strategy
        .as_ref()
        .and_then(|s| s.get("oos"))
        .and_then(|o| o.get("total_pnl"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let report = json!({
        "meta": {
            "path": "price",
            "instrument": okx_history::INSTRUMENT,
            "feature_names": PRICE_FEATURE_NAMES,
            "equation": "(close_{t+h}/close_t)-1 = Σαf(t); one OLS per horizon h",
            "bar": label,
            "bar_ms": bar_ms,
            "price_horizon": horizon_n,
            "signal_rule": format!(
                "at close_t forecast close for t+1..t+{horizon_n}; lock on h=1 vs last-close baseline"
            ),
            "lookback_days": lookback_days,
            "max_rel_err": args.max_rel_err,
            "min_fit_pairs": args.min_fit_pairs,
            "require_fit": args.require_fit,
            "fit_pairs": fit_pairs.len(),
            "locked": locked,
            "locked_at": utc(lock_ms),
            "trade_start": utc(trade_start_ms),
            "trade_end": utc(trade_end_ms),
            "size_btc": meta.size_btc,
            "taker_fee_bps": mk.costs.taker * 10_000.0,
            "maker_fee_bps": mk.costs.maker * 10_000.0,
            "price_model": {
                "alphas": price_model.alphas.iter().map(|a| round2(*a * 1e6) / 1e6).collect::<Vec<_>>(),
                "rel_err": price_model.rel_err,
                "return_mae": price_model.rel_err,
                "price_rel_err": price_model.price_rel_err,
                "baseline_rel_err": price_model.baseline_rel_err,
                "mae": price_model.mae,
                "samples": price_model.samples,
                "cold_start": price_model.cold_start,
                "form": "candle_ohlc_multi_horizon",
                "horizon": 1,
            },
            "horizons": horizons.table,
            "path_example": horizons.path_example,
            "strategy": strategy.as_ref().and_then(|s| s.get("selected")).cloned().unwrap_or(Value::Null),
            "fit_trace": score_models_json,
            "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
        },
        "signals": chart_signals,
        "strategy": strategy.unwrap_or(Value::Null),
        "total_pnl": round2(balance_pnl),
    });

    Ok(PriceBarRun {
        label: label.to_string(),
        bar_ms,
        locked,
        fit_pairs: fit_pairs.len(),
        return_mae: price_model.rel_err,
        price_rel_err: price_model.price_rel_err,
        baseline_rel_err: price_model.baseline_rel_err,
        total_pnl: balance_pnl,
        n_signals: chart_signals.len(),
        report,
    })
}

struct HorizonEval {
    table: Vec<Value>,
    path_example: Value,
    oos_h1_price_err: f64,
    oos_h1_base_err: f64,
    oos_hmax_price_err: f64,
    oos_hmax_base_err: f64,
}

/// Fit one model per horizon h=1..H on pre-TRADE data; score OOS price error vs last-close baseline.
fn eval_price_horizons(
    h1_model: &PriceModel,
    bars: &[Candle],
    typical: f64,
    horizon_n: usize,
    bar_ms: i64,
    train_cutoff_ms: i64,
    trade_start_ms: i64,
    trade_end_ms: i64,
    max_rel_err: f64,
) -> Result<HorizonEval> {
    let mut table = Vec::with_capacity(horizon_n);
    let mut oos_h1_price_err = f64::NAN;
    let mut oos_h1_base_err = f64::NAN;
    let mut oos_hmax_price_err = f64::NAN;
    let mut oos_hmax_base_err = f64::NAN;
    let mut models: Vec<PriceModel> = Vec::with_capacity(horizon_n + 1);
    models.push(PriceModel::cold_start(typical)); // unused index 0

    for h in 1..=horizon_n {
        let all_h = samples_from_candles_horizon(bars, h);
        let train: Vec<PriceSample> = all_h
            .iter()
            .copied()
            .filter(|s| s.origin_ms < train_cutoff_ms && s.at_ms < trade_start_ms)
            .collect();
        let model = if h == 1 && !h1_model.cold_start {
            h1_model.clone()
        } else {
            PriceModel::fit(&train, None, typical).unwrap_or_else(|_| PriceModel::cold_start(typical))
        };
        let oos: Vec<PriceSample> = all_h
            .iter()
            .copied()
            .filter(|s| s.origin_ms >= trade_start_ms && s.at_ms < trade_end_ms)
            .collect();
        let (ret_mae, price_err, base_err) = oos_horizon_errors(&model, &oos);
        let under_1pct = price_err.is_finite() && price_err < 0.01;
        let beats_base = price_err.is_finite()
            && base_err.is_finite()
            && price_err + 1e-12 < base_err;
        table.push(json!({
            "h": h,
            "train_n": train.len(),
            "oos_n": oos.len(),
            "alphas": model.alphas.iter().map(|a| round2(*a * 1e6) / 1e6).collect::<Vec<_>>(),
            "oos_return_mae": ret_mae,
            "oos_price_err": price_err,
            "oos_baseline_err": base_err,
            "under_1pct": under_1pct,
            "beats_baseline": beats_base,
            "gate_ok": !model.cold_start && price_err.is_finite() && price_err < max_rel_err && beats_base,
        }));
        if h == 1 {
            oos_h1_price_err = price_err;
            oos_h1_base_err = base_err;
        }
        if h == horizon_n {
            oos_hmax_price_err = price_err;
            oos_hmax_base_err = base_err;
        }
        models.push(model);
    }

    // Live-style: last finished candle on the tape → forecast the next H future bars.
    let path_example = {
        let mut origin_t = None;
        for t in 1..bars.len() {
            // Prefer the last bar we actually "know" at TRADE end (same idea as last signal row).
            if bars[t].time_ms < trade_end_ms {
                origin_t = Some(t);
            }
        }
        match origin_t.and_then(|t| {
            let feat = candle_features(&bars[t], bars[t - 1].close)?;
            let ref_px = bars[t].close;
            let origin_ms = bars[t].time_ms;
            let mut steps = Vec::new();
            for h in 1..=horizon_n {
                let pred = models[h].predict_close(ref_px, &feat);
                let future_ms = origin_ms + h as i64 * bar_ms;
                let (spot, err_pct, base_err_pct) = if let Some(bar) = bars.get(t + h) {
                    let spot = bar.close;
                    (
                        Some(round2(spot)),
                        Some(round2(100.0 * (pred - spot).abs() / spot.max(1e-12))),
                        Some(round2(100.0 * (ref_px - spot).abs() / spot.max(1e-12))),
                    )
                } else {
                    (None, None, None)
                };
                steps.push(json!({
                    "h": h,
                    "at": utc(future_ms),
                    "spot": spot,
                    "predicted": round2(pred),
                    "ref": round2(ref_px),
                    "err_pct": err_pct,
                    "base_err_pct": base_err_pct,
                    "beyond_tape": spot.is_none(),
                }));
            }
            Some(json!({
                "origin": utc(origin_ms),
                "origin_close": round2(ref_px),
                "kind": "forward_from_last_candle",
                "steps": steps,
            }))
        }) {
            Some(v) => v,
            None => Value::Null,
        }
    };

    Ok(HorizonEval {
        table,
        path_example,
        oos_h1_price_err,
        oos_h1_base_err,
        oos_hmax_price_err,
        oos_hmax_base_err,
    })
}

fn oos_horizon_errors(model: &PriceModel, oos: &[PriceSample]) -> (f64, f64, f64) {
    if oos.is_empty() {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let mut ret_sum = 0.0;
    let mut price_sum = 0.0;
    let mut base_sum = 0.0;
    let mut n = 0usize;
    for s in oos {
        if !(s.spot.is_finite() && s.spot > 0.0 && s.ref_px.is_finite() && s.ref_px > 0.0) {
            continue;
        }
        let rhat = model.predict_return(&s.features);
        let y = s.spot / s.ref_px - 1.0;
        let pred = model.predict_close(s.ref_px, &s.features);
        ret_sum += (y - rhat).abs();
        price_sum += (pred - s.spot).abs() / s.spot;
        base_sum += (s.ref_px - s.spot).abs() / s.spot;
        n += 1;
    }
    if n == 0 {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let n = n as f64;
    (ret_sum / n, price_sum / n, base_sum / n)
}

/// Predicted-return sign → bar index signal (fills at that bar's open in `scan::simulate`).
fn price_dir_signals(model: &PriceModel, candles: &[Candle]) -> Vec<(u32, i8)> {
    let mut out = Vec::new();
    let mut last_dir: i8 = 0;
    for i in 2..candles.len() {
        let Some(feat) = candle_features(&candles[i - 1], candles[i - 2].close) else {
            continue;
        };
        let r = model.predict_return(&feat);
        if !r.is_finite() || r == 0.0 {
            continue;
        }
        let dir: i8 = if r > 0.0 { 1 } else { -1 };
        if dir == last_dir {
            continue;
        }
        last_dir = dir;
        out.push((i as u32, dir));
    }
    out
}

fn frame_bar_range(candles: &[Candle], start_ms: i64, end_ms: i64) -> std::ops::Range<usize> {
    let lo = candles.partition_point(|c| c.time_ms < start_ms);
    let hi = candles.partition_point(|c| c.time_ms < end_ms);
    lo..hi.max(lo)
}

fn outcome_json(o: &Outcome) -> Value {
    json!({
        "total_pnl": round2(o.total()),
        "realized": round2(o.realized),
        "fees": round2(o.fees),
        "open_pnl": round2(o.open_pnl),
        "closed_trades": o.closed_trades,
        "wins": o.wins,
        "win_rate": if o.closed_trades > 0 {
            round2(100.0 * o.wins as f64 / o.closed_trades as f64)
        } else {
            0.0
        },
        "target_fills": o.target_fills,
        "stop_fills": o.stop_fills,
        "orders": o.orders,
        "refused": o.refused,
        "max_drawdown": round2(o.max_drawdown),
    })
}

/// Pick TP/SL × leverage on IS (lookback before TRADE), then score OOS with taker/maker fees.
fn run_price_strategy_grid(
    args: &Args,
    meta: &Meta,
    mk: &Market1m<'_>,
    model: &PriceModel,
    label: &str,
    bar_ms: i64,
    trade_start_ms: i64,
    trade_end_ms: i64,
    lookback_days: u64,
) -> Result<Value> {
    let bar_minutes = (bar_ms / 60_000) as usize;
    ensure!(bar_minutes >= 1, "bar must be at least 1m");
    let frame = Frame::from_minutes(mk.minutes, &mk.tape.bar_start, bar_minutes);
    ensure!(
        frame.candles.len() >= 3,
        "{label}: not enough frame candles for strategy"
    );
    let signals = price_dir_signals(model, &frame.candles);
    ensure!(
        !signals.is_empty(),
        "{label}: price model produced no direction signals"
    );

    let is_start_ms = trade_start_ms.saturating_sub(lookback_days as i64 * 86_400_000);
    let is_range = frame_bar_range(&frame.candles, is_start_ms, trade_start_ms);
    let oos_range = frame_bar_range(&frame.candles, trade_start_ms, trade_end_ms);
    ensure!(!oos_range.is_empty(), "{label}: empty OOS bar range");

    let base_steps = mk.costs.steps;
    let exit_opts = exit_settings(
        &args.price_stops,
        &args.ladders,
        &args.shares,
        &args.breakeven,
        base_steps,
    )?;
    let exits: Vec<Exits> = exit_opts.into_iter().flatten().collect();
    ensure!(!exits.is_empty(), "{label}: no TP/SL exit settings");

    let closes = CloseRanges::new(
        &frame
            .candles
            .iter()
            .map(|c| c.close)
            .collect::<Vec<_>>(),
    );
    let market = Market {
        candles: &frame.candles,
        tape: mk.tape,
        bar_start: &frame.bar_start,
        closes: &closes,
    };

    println!(
        "  {label} strategy grid: {} exits × {} lev = {} · signals={} · IS bars {}..{} · OOS {}..{}",
        exits.len(),
        args.price_leverage.len(),
        exits.len() * args.price_leverage.len(),
        signals.len(),
        is_range.start,
        is_range.end,
        oos_range.start,
        oos_range.end
    );
    println!(
        "  fees: taker {:.1} bps · maker {:.1} bps · base size {} BTC",
        mk.costs.taker * 10_000.0,
        mk.costs.maker * 10_000.0,
        meta.size_btc
    );

    struct Cand {
        exits: Exits,
        lev: u32,
        is: Outcome,
    }
    let mut ranked: Vec<Cand> = Vec::new();
    for exits in &exits {
        for &lev in &args.price_leverage {
            let mut costs = mk.costs;
            costs.steps = base_steps.saturating_mul(i64::from(lev));
            costs.balance = REPLAY_BALANCE;
            if exits.validate(costs.steps).is_err() {
                continue;
            }
            let is = scan::simulate(
                (0, 0, 0),
                &signals,
                Some(exits),
                &market,
                is_range.clone(),
                costs,
                None,
            );
            if is.closed_trades < MIN_TRADES || is.refused > 0 {
                continue;
            }
            ranked.push(Cand {
                exits: exits.clone(),
                lev,
                is,
            });
        }
    }
    ensure!(
        !ranked.is_empty(),
        "{label}: no price strategy combo passed IS (need ≥{MIN_TRADES} closed trades, fees applied)"
    );
    ranked.sort_by(|a, b| {
        b.is.total()
            .partial_cmp(&a.is.total())
            .unwrap_or(Ordering::Equal)
    });

    let top_n = ranked.len().min(15);
    let top_json: Vec<Value> = ranked[..top_n]
        .iter()
        .map(|c| {
            json!({
                "exits": exits_to_json(&c.exits),
                "leverage": c.lev,
                "size_btc": round2(meta.size_btc * f64::from(c.lev)),
                "is": outcome_json(&c.is),
            })
        })
        .collect();

    let best = &ranked[0];
    let mut costs = mk.costs;
    costs.steps = base_steps.saturating_mul(i64::from(best.lev));
    costs.balance = REPLAY_BALANCE;
    let mut record = Record::default();
    let oos = scan::simulate(
        (0, 0, 0),
        &signals,
        Some(&best.exits),
        &market,
        oos_range.clone(),
        costs,
        Some(&mut record),
    );

    println!(
        "  {label} SELECTED {} · {}x · IS {:+.2} (fees {:.2}, {} trades) → OOS {:+.2} (fees {:.2}, {} trades)",
        best.exits.label(),
        best.lev,
        best.is.total(),
        best.is.fees,
        best.is.closed_trades,
        oos.total(),
        oos.fees,
        oos.closed_trades
    );

    let trades_json: Vec<Value> = record
        .trades
        .iter()
        .map(|t| {
            let exit_fees: f64 = t.exits.iter().map(|e| e.fee).sum();
            let exit_pnl: f64 = t
                .exits
                .iter()
                .map(|e| {
                    let qty = e.steps as f64 * STEP_BTC;
                    let sign = if t.long { 1.0 } else { -1.0 };
                    sign * (e.price - t.entry_price) * qty - e.fee
                })
                .sum();
            json!({
                "side": if t.long { "buy" } else { "sell" },
                "entry_at": utc(t.entry_ms),
                "entry_price": round2(t.entry_price),
                "size_btc": round2(t.steps as f64 * STEP_BTC),
                "entry_fee": round2(t.entry_fee),
                "exit_fee": round2(exit_fees),
                "pnl": round2(exit_pnl - t.entry_fee),
                "open": t.open,
                "exits": t.exits.iter().map(|e| json!({
                    "at": utc(e.time_ms),
                    "price": round2(e.price),
                    "size_btc": round2(e.steps as f64 * STEP_BTC),
                    "fee": round2(e.fee),
                    "kind": e.kind,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(json!({
        "selected": {
            "exits": exits_to_json(&best.exits),
            "leverage": best.lev,
            "size_btc": round2(meta.size_btc * f64::from(best.lev)),
            "taker_fee_bps": mk.costs.taker * 10_000.0,
            "maker_fee_bps": mk.costs.maker * 10_000.0,
        },
        "is": outcome_json(&best.is),
        "oos": outcome_json(&oos),
        "top": top_json,
        "combos_tested": ranked.len(),
        "trades": trades_json,
        "signal_count": signals.len(),
        "is_bars": [is_range.start, is_range.end],
        "oos_bars": [oos_range.start, oos_range.end],
    }))
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if args.html_only {
        return regenerate_html_from_json(&args.scan);
    }
    let config = AppConfig::load(&args.config)?;
    let meta: Meta = serde_json::from_str(&fs::read_to_string(args.scan.join("meta.json")).with_context(|| {
        format!("no finished scan in {}: window_scan writes meta.json when it is done (see data/windowscan/run.log)", args.scan.display())
    })?)?;
    ensure!(
        meta.every_pair == 1,
        "window_scan ran on every {}th RSI pair only; run it on all of them",
        meta.every_pair
    );
    ensure!(
        meta.taker_fee_bps == config.venues.okx.taker_fee_bps
            && meta.maker_fee_bps == config.venues.okx.maker_fee_bps,
        "window_scan used other fees than {}",
        args.config.display()
    );
    let pairs: Vec<(usize, usize)> = (0..meta.lengths.len())
        .flat_map(|i| (i + 1..meta.lengths.len()).map(move |j| (i, j)))
        .collect();
    if let Some(ref profiles) = meta.exit_profiles {
        ensure!(
            profiles.len() == meta.tp.len(),
            "meta.exit_profiles length {} != meta.tp length {}",
            profiles.len(),
            meta.tp.len()
        );
        for p in profiles {
            p.validate()?;
        }
    }
    let space = Space {
        timeframes: meta.timeframes.clone(),
        lengths: meta.lengths.clone(),
        pairs,
        grid: Grid {
            tp: meta.tp.clone(),
            sl: meta.sl.clone(),
            lev: meta.leverage.clone(),
        },
        survivor_groups: meta.survivor_groups.clone(),
        exit_profiles: meta.exit_profiles.clone(),
    };
    let n_groups = space
        .survivor_groups
        .as_ref()
        .map(|s| s.len())
        .unwrap_or(space.timeframes.len() * space.pairs.len());
    ensure!(
        n_groups * space.grid.len() == meta.combinations,
        "meta.json does not add up"
    );
    let n_combos = meta.combinations;
    let n_windows = meta.windows.len();
    ensure!(n_windows >= 2, "window_scan covered less than two windows");
    ensure!(meta.days > 0, "meta.days must be at least 1");
    let outer_hours = meta.window_hours();
    // Recency cap for last-N scoring; outer span is meta.hours (or days×24).
    let lookback_days = args.lookback_days;
    let lookback_ms = lookback_days as i64 * 86_400_000;
    // windows[0] is ranking-only (no prior outer window). Picks / OOS start at windows[1].
    let ranking_warmup = meta.windows[0].start.clone();
    let first_pick = meta.windows[1].start.clone();
    let first_pick_ms = parse_window_ms(&first_pick)?;
    let trade_floor_ms = args.first_choice.map(ms_day);
    if let Some(floor) = trade_floor_ms {
        ensure!(
            floor < ms_day(meta.end),
            "--first-choice must be before scan end {}",
            meta.end
        );
        ensure!(
            first_pick_ms <= floor,
            "--first-choice is before the first pickable outer window ({first_pick}). \
             The scan's first window ({ranking_warmup}) is ranking-only."
        );
    }
    println!(
        "Warmup: scan window {ranking_warmup} is ranking-only. Step-1 picks from {first_pick}. \
         Score TRAIN until rel_err < {:.0}% (no fixed end date); then TRADE {}{}. \
         Lookback {lookback_days}d — tape loads from scan start minus lookback when archives exist.",
        args.max_rel_err * 100.0,
        if args.trade_days == 0 {
            "until scan end (all remaining data)".to_string()
        } else {
            format!("{} calendar day(s)", args.trade_days)
        },
        trade_floor_ms.map_or_else(String::new, |t| format!(" (not before {})", utc(t)))
    );
    let max_m = args
        .train_trades
        .iter()
        .map(|&n| test_size(n))
        .max()
        .unwrap();
    let configs: Vec<(usize, usize)> = args
        .train_trades
        .iter()
        .flat_map(|&n| (1..=test_size(n)).rev().map(move |m| (n, m)))
        .collect();
    let budget = (args.memory_gb * 1e9) as usize;
    // Kept trades span lookback + the outer window (+ a little OOS). Scale by hours.
    let lookback_hours = lookback_days.saturating_mul(24).max(1);
    let day_scale = lookback_hours.div_ceil(outer_hours).max(1) as usize;
    // Safety vs under-count (high-frequency combos, estimate error).
    let est_pad = 2usize;

    // Shortlist from scan bins (cached). Score fit happens later on trade N→M folds.
    let cache_key = finalists_cache_key(
        &meta,
        n_combos,
        lookback_days,
        args.max_finalists,
        args.memory_gb,
        day_scale,
        est_pad,
        max_m,
    );
    let mut score_model = ScoreModel::cold_start();
    let mut score_models_json = Vec::new();
    let (finalist_ids, windows_json) = if !args.rerank {
        match load_finalists_cache(&args.scan, &cache_key, n_windows)? {
            Some((ids, wj)) => {
                let n_kept: usize = ids.iter().map(|v| v.len()).sum();
                println!(
                    "Reusing finalists cache ({} windows, {n_kept} shortlist ids) — skip day ranking. Pass --rerank to rebuild.",
                    ids.len()
                );
                (ids, wj)
            }
            None => (Vec::new(), Vec::new()),
        }
    } else {
        (Vec::new(), Vec::new())
    };
    let (finalist_ids, mut windows_json) = if !finalist_ids.is_empty() {
        (finalist_ids, windows_json)
    } else {
        println!(
            "Ranking all {n_combos} combinations window by window (shortlist only; score TRAIN is trade N→M until --first-choice, gate < {:.0}%); finalist RAM budget {:.2} GB (lookback {lookback_days}d × outer {outer_hours}h → scale {day_scale}×, pad {est_pad}×, max {})",
            args.max_rel_err * 100.0,
            args.memory_gb,
            args.max_finalists
        );
        let mut finalist_ids: Vec<Vec<u32>> = vec![Vec::new(); n_windows];
        let mut windows_json = Vec::new();
        let rank_model = ScoreModel::cold_start();
        for w in 1..n_windows {
            let started = Instant::now();
            let (ranked, traded_in) = (&meta.windows[w - 1], &meta.windows[w]);
            let (top, traded, eligible, profitable) =
                rank(&args.scan.join(&ranked.file), n_combos, args.max_finalists, &rank_model)?;
            let mut used = 0;
            let kept = top.iter().take_while(|s| {
                let est_trades =
                    usize::from(s.stats.trades).saturating_mul(day_scale).saturating_mul(est_pad)
                        + max_m;
                used += est_trades * std::mem::size_of::<Kept>() + 64;
                used <= budget
            });
            let ids: Vec<u32> = kept.map(|s| s.id).collect();
            let next = File::open(args.scan.join(&traded_in.file))?;
            let mut buf = [0u8; RECORD];
            let mut next_of = |id: u32| -> Result<WindowStats> {
                next.read_exact_at(&mut buf, id as u64 * RECORD as u64)?;
                Ok(WindowStats::from_bytes(&buf))
            };
            let (mut next_total, mut next_positive) = (0.0f64, 0);
            for &id in &ids {
                let s = next_of(id)?;
                next_total += f64::from(s.profit);
                next_positive += usize::from(s.profit > 0.0);
            }
            let shown = top
                .iter()
                .take(SHOWN)
                .enumerate()
                .map(|(r, s)| {
                    Ok(json!({
                        "rank": r + 1,
                        "id": s.id,
                        "label": space.label(s.id),
                        "scored": stats_json_scored(&s.stats, Some(&rank_model)),
                        "next": stats_json_scored(&next_of(s.id)?, Some(&rank_model)),
                    }))
                })
                .collect::<Result<Vec<_>>>()?;
            println!(
                "  {}: {traded} combinations traded, {eligible} with a score, {profitable} made money; best {} (score {:.3}); {} finalists for {} ({:.1}s), who then made {:+.0} USDT together, {} of them positive",
                ranked.start,
                top.first().map_or_else(String::new, |s| space.label(s.id)),
                top.first().map_or(0.0, |s| s.score),
                ids.len(),
                traded_in.start,
                started.elapsed().as_secs_f64(),
                next_total,
                next_positive
            );
            let cold_rank_source = w == 1;
            if cold_rank_source {
                println!(
                    "  note: {} has no prior outer window in this scan — finalists for {} are ranked on a cold first day. \
                     Re-scan with an earlier --start if that day should itself have prior IS.",
                    ranked.start, traded_in.start
                );
            }
            windows_json.push(json!({
                "ranked": format!("{} → {}", ranked.start, ranked.end),
                "traded_in": format!("{} → {}", traded_in.start, traded_in.end),
                "cold_rank_source": cold_rank_source,
                "combinations_traded": traded,
                "eligible": eligible,
                "profitable": profitable,
                "finalists": ids.len(),
                "finalists_next_total": round2(next_total),
                "finalists_next_positive": next_positive,
                "top": shown,
            }));
            finalist_ids[w] = ids;
        }
        save_finalists_cache(&args.scan, &cache_key, &finalist_ids, &windows_json)?;
        (finalist_ids, windows_json)
    };

    let started = Instant::now();
    let mut shown = 0;
    // Prefer scan_start − lookback, but never ask for days before the scan (use only our data).
    let wanted_from_ms = ms(meta.start).saturating_sub(lookback_ms);
    let tape_from_ms = wanted_from_ms.max(ms(meta.start));
    if tape_from_ms > wanted_from_ms {
        println!(
            "note: --lookback-days {lookback_days} would need tape from {}; clamping to scan start {} (no download)",
            utc(wanted_from_ms),
            meta.start
        );
    }
    println!(
        "Loading OKX tape from {} through {} (scan {} → {}, lookback {lookback_days}d)",
        utc(tape_from_ms),
        utc(ms(meta.end)),
        meta.start,
        meta.end
    );
    let (minutes, tape) = Tape::load(
        &args.data,
        60_000,
        tape_from_ms,
        ms(meta.end),
        |done, total| {
            if done * 10 / total.max(1) > shown || done == total {
                shown = done * 10 / total.max(1);
                println!("Reading every OKX trade: archive {done} of {total}");
            }
        },
    )?;
    println!(
        "{} one-minute candles, {} price changes ({:.0}s)",
        minutes.len(),
        tape.price.len(),
        started.elapsed().as_secs_f64()
    );
    let frames: Vec<Frame> = space
        .timeframes
        .iter()
        .map(|&m| Frame::from_minutes(&minutes, &tape.bar_start, m))
        .collect();
    let costs = Costs {
        steps: (meta.size_btc * 10_000.0).round() as i64,
        taker: config.venues.okx.taker_fee_bps / 10_000.0,
        maker: config.venues.okx.maker_fee_bps / 10_000.0,
        balance: REPLAY_BALANCE,
    };
    let mk = Market1m {
        space: &space,
        tape: &tape,
        minutes: &minutes,
        frames: &frames,
        costs,
    };
    if args.refine_exits {
        let refine = ExitRefine {
            ladders: &args.ladders,
            shares: &args.shares,
            breakeven: &args.breakeven,
            exit_alts: args.exit_alts,
        };
        let n_is = args.train_trades.iter().copied().min().unwrap_or(10).max(MIN_TRADES as usize);
        println!(
            "Enriching shortlist tops with exit refine (IS last {n_is} trades, lookback {lookback_days}d)…"
        );
        let t0 = Instant::now();
        enrich_shortlist_exits(&mut windows_json, &mk, lookback_ms, n_is, &refine, &score_model)?;
        println!(
            "  shortlist exit refine done in {:.0}s",
            t0.elapsed().as_secs_f64()
        );
    }
    let step_at = |at: i64| {
        let i = minutes.partition_point(|c| c.time_ms < at);
        tape.bar_start
            .get(i)
            .copied()
            .unwrap_or(tape.price.len() as u32)
    };
    let threads = args.threads;

    let scan_end_ms = ms_day(meta.end);
    let train_cutoff_ms =
        scan_end_ms.saturating_sub(args.reserve_trade_days as i64 * 86_400_000);
    ensure!(
        train_cutoff_ms > first_pick_ms,
        "--reserve-trade-days leaves no TRAIN window before TRADE (scan too short or reserve too large)"
    );
    let mut train_ws: Vec<usize> = Vec::new();
    for w in 1..n_windows {
        let end = parse_window_ms(&meta.windows[w].end)?;
        if end > train_cutoff_ms {
            break;
        }
        train_ws.push(w);
    }
    ensure!(
        !train_ws.is_empty(),
        "no TRAIN windows before TRADE cutoff {}",
        utc(train_cutoff_ms)
    );
    let price_trade_start_ms = match trade_floor_ms {
        Some(floor) => train_cutoff_ms.max(floor),
        None => train_cutoff_ms,
    };
    let price_trade_end_ms = if args.trade_days == 0 {
        scan_end_ms
    } else {
        (price_trade_start_ms + args.trade_days as i64 * 86_400_000).min(scan_end_ms)
    };

    if args.path.wants_price() {
        run_price_path(
            &args,
            &meta,
            &mk,
            &train_ws,
            train_cutoff_ms,
            price_trade_start_ms,
            price_trade_end_ms,
            lookback_days,
        )?;
    }
    if !args.path.wants_score() {
        return Ok(());
    }

    // Step 1: near→far score TRAIN (day just before TRADE reserve, then further back).
    println!(
        "\nStep 1: {} window pairs (IS: last N of {} trades within lookback {} day(s); OOS: M = N/3 … 1).\n\
         Score TRAIN near→far until rel_err < {:.0}% AND ≥{} fit_pairs; LOCK.\n\
         Reserve last {}d for TRADE (cutoff {}). Scan bins reused — not rebuilding combo grid.",
        configs.len(),
        args.train_trades
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" / "),
        lookback_days,
        args.max_rel_err * 100.0,
        args.min_fit_pairs,
        args.reserve_trade_days,
        utc(train_cutoff_ms)
    );
    println!(
        "Score fit near→far ({} train days, newest first = {})…",
        train_ws.len(),
        meta.windows[*train_ws.last().unwrap()].start
    );

    let harvest_model = ScoreModel::cold_start();
    let mut fit_pairs: Vec<(WindowStats, f64)> = Vec::new();
    let mut score_locked = false;
    let mut lock_ms: Option<i64> = None;
    for &w in train_ws.iter().rev() {
        let started = Instant::now();
        let window = &meta.windows[w];
        let (start, end) = (parse_window_ms(&window.start)?, parse_window_ms(&window.end)?);
        let day_start = start.max(first_pick_ms);
        let finalists = build_finalists(
            &mk,
            &finalist_ids[w],
            step_at(start - lookback_ms),
            step_at(end),
            max_m,
            threads,
        );
        let kept: usize = finalists.iter().map(|f| f.trades.len()).sum();
        let kept_gb = (kept * std::mem::size_of::<Kept>()) as f64 / 1e9;
        let built = started.elapsed().as_secs_f64();
        if kept_gb > args.memory_gb * 1.25 {
            eprintln!(
                "  warning: kept {kept_gb:.2} GB > budget {:.2} GB; lower --memory-gb or --max-finalists",
                args.memory_gb
            );
        }
        // Fresh chains for this day only — score fit samples, not continuous WF history.
        let day_chains: Vec<Mutex<Chain>> = configs
            .iter()
            .map(|&(n, m)| {
                Mutex::new(Chain {
                    n,
                    m,
                    at: day_start,
                    done: false,
                    windows: Vec::new(),
                    trades: Vec::new(),
                    stop_reason: "the data ended".into(),
                })
            })
            .collect();
        let new_pairs = Mutex::new(Vec::<(WindowStats, f64)>::new());
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let k = next.fetch_add(1, AtomicOrdering::Relaxed);
                        let Some(chain) = day_chains.get(k) else { break };
                        let pairs = advance(
                            &mut chain.lock().unwrap(),
                            &finalists,
                            end,
                            lookback_ms,
                            &space,
                            &harvest_model,
                        );
                        if !pairs.is_empty() {
                            new_pairs.lock().unwrap().extend(pairs);
                        }
                    }
                });
            }
        });
        let batch = std::mem::take(&mut *new_pairs.lock().unwrap());
        let added = batch.len();
        fit_pairs.extend(batch);
        if args.max_fit_pairs > 0 && fit_pairs.len() > args.max_fit_pairs {
            // Prefer newest (front): we prepended? We extend so order is newest-first batches.
            // Drop from the end (older points added later in near→far).
            let keep = args.max_fit_pairs;
            fit_pairs.drain(keep..);
        }
        drop(finalists);

        let pairs_ok = fit_pairs.len() >= args.min_fit_pairs;
        if fit_pairs.len() < MIN_FIT_SAMPLES {
            println!(
                "  {}: near→far +{added} → fit_pairs={} (need ≥{MIN_FIT_SAMPLES}) | {kept_gb:.2}/{:.2} GB in {built:.0}s+{:.0}s",
                window.start,
                fit_pairs.len(),
                args.memory_gb,
                started.elapsed().as_secs_f64() - built,
            );
            continue;
        }
        match ScoreModel::fit(&fit_pairs, Some(&score_model)) {
            Ok(m) => {
                let prev_err = score_model.score_rel_err;
                score_model = m;
                println!(
                    "  {}: near→far +{added} → fit_pairs={}/{} cold={} rel_err={:.3}% (was {:.3}%) | {kept_gb:.2}/{:.2} GB in {built:.0}s+{:.0}s",
                    window.start,
                    fit_pairs.len(),
                    args.min_fit_pairs,
                    score_model.cold_start,
                    score_model.score_rel_err * 100.0,
                    prev_err * 100.0,
                    args.memory_gb,
                    started.elapsed().as_secs_f64() - built,
                );
                score_models_json.push(json!({
                    "phase": "train_near_to_far",
                    "window_end": utc(end),
                    "label": window.start.clone(),
                    "model": score_model_json(&score_model, args.max_rel_err),
                    "fit_pairs": fit_pairs.len(),
                    "locked": false,
                }));
                if !score_model.cold_start && score_model.score_rel_err < args.max_rel_err {
                    if pairs_ok {
                        score_locked = true;
                        lock_ms = Some(train_cutoff_ms);
                        println!(
                            "  score LOCKED (went back to {}) — rel_err={:.3}% fit_pairs={} (< {:.0}% gate). TRADE from {}.",
                            window.start,
                            score_model.score_rel_err * 100.0,
                            fit_pairs.len(),
                            args.max_rel_err * 100.0,
                            utc(train_cutoff_ms)
                        );
                        if let Some(last) = score_models_json.last_mut() {
                            last["locked"] = json!(true);
                        }
                        break;
                    }
                    println!(
                        "  … gate ok (rel_err={:.3}%) but need more points ({}/{}) — go further back",
                        score_model.score_rel_err * 100.0,
                        fit_pairs.len(),
                        args.min_fit_pairs
                    );
                } else {
                    println!(
                        "  … still above {:.0}% — go further back in time",
                        args.max_rel_err * 100.0
                    );
                }
            }
            Err(e) => {
                println!(
                    "  {}: near→far fit failed ({e}); keep going back | {kept_gb:.2} GB",
                    window.start
                );
            }
        }
    }
    if !score_locked {
        if args.require_fit {
            bail!(
                "score TRAIN never LOCKed near→far (need rel_err < {:.0}% and ≥{} fit_pairs; \
                 got cold={} rel_err={:.3}% pairs={}). Reserved last {}d for TRADE.",
                args.max_rel_err * 100.0,
                args.min_fit_pairs,
                score_model.cold_start,
                score_model.score_rel_err * 100.0,
                fit_pairs.len(),
                args.reserve_trade_days
            );
        }
        lock_ms = Some(train_cutoff_ms);
        println!(
            "  warning: score never LOCKed (cold={} rel_err={:.3}% pairs={}); --require-fit 0 → TRADE from {}",
            score_model.cold_start,
            score_model.score_rel_err * 100.0,
            fit_pairs.len(),
            utc(train_cutoff_ms)
        );
    }

    // Continuous N/M history for Step-2 mode picks: only the lookback before TRADE (not full TRAIN).
    let chain_fill_from_ms = train_cutoff_ms.saturating_sub(lookback_ms).max(first_pick_ms);
    println!(
        "\nFilling N/M chain history {} → {} (lookback {lookback_days}d before TRADE)…",
        utc(chain_fill_from_ms),
        utc(train_cutoff_ms)
    );
    let chains: Vec<Mutex<Chain>> = configs
        .iter()
        .map(|&(n, m)| {
            Mutex::new(Chain {
                n,
                m,
                at: chain_fill_from_ms,
                done: false,
                windows: Vec::new(),
                trades: Vec::new(),
                stop_reason: "the data ended".into(),
            })
        })
        .collect();
    for &w in &train_ws {
        let window = &meta.windows[w];
        let (start, end) = (parse_window_ms(&window.start)?, parse_window_ms(&window.end)?);
        if end <= chain_fill_from_ms {
            continue;
        }
        let started = Instant::now();
        let finalists = build_finalists(
            &mk,
            &finalist_ids[w],
            step_at(start - lookback_ms),
            step_at(end),
            max_m,
            threads,
        );
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let k = next.fetch_add(1, AtomicOrdering::Relaxed);
                        let Some(chain) = chains.get(k) else { break };
                        let _ = advance(
                            &mut chain.lock().unwrap(),
                            &finalists,
                            end,
                            lookback_ms,
                            &space,
                            &score_model,
                        );
                    }
                });
            }
        });
        let picks: usize = chains.iter().map(|c| c.lock().unwrap().windows.len()).sum();
        println!(
            "  {}: chain fill | {} finalists in {:.0}s; picks {picks}",
            window.start,
            finalists.len(),
            started.elapsed().as_secs_f64(),
        );
        drop(finalists);
    }
    let trade_start_ms = {
        let lock = lock_ms.expect("lock_ms set");
        match trade_floor_ms {
            Some(floor) => lock.max(floor),
            None => lock,
        }
    };
    let trade_end_ms = if args.trade_days == 0 {
        scan_end_ms
    } else {
        (trade_start_ms + args.trade_days as i64 * 86_400_000).min(scan_end_ms)
    };
    ensure!(
        trade_start_ms < trade_end_ms,
        "no TRADE period left after score lock (lock/floor {}, scan end {})",
        utc(trade_start_ms),
        utc(scan_end_ms)
    );
    let first_choice = utc(trade_start_ms);
    score_models_json.push(json!({
        "phase": "locked",
        "model": score_model_json(&score_model, args.max_rel_err),
        "fit_pairs": fit_pairs.len(),
        "require_fit": args.require_fit,
        "locked_at": utc(lock_ms.unwrap_or(trade_start_ms)),
        "trade_start": first_choice.clone(),
        "trade_end": utc(trade_end_ms),
        "trade_days": args.trade_days,
    }));
    let chains: Vec<Chain> = chains
        .into_iter()
        .map(|c| c.into_inner().unwrap())
        .collect();
    let mut config_json = Vec::new();
    for c in &chains {
        let total: f64 = c.trades.iter().map(|t| t.net).sum();
        let mut tf_trades = vec![0usize; space.timeframes.len()];
        for t in &c.trades {
            tf_trades[space.decode(t.combo).0] += 1;
        }
        config_json.push(json!({
            "train_trades": c.n,
            "test_trades": c.m,
            "windows": c.windows.len(),
            "positive": c.windows.iter().filter(|w| **w > 0.0).count(),
            "trades": c.trades.len(),
            "total": round2(total),
            "timeframe_trades": tf_trades,
            "until": c.trades.last().map_or_else(String::new, |t| utc(t.exit_ms)),
            "stop_reason": c.stop_reason,
        }));
    }
    let mut ranked: Vec<&Value> = config_json.iter().collect();
    ranked.sort_by(|a, b| {
        b["total"]
            .as_f64()
            .unwrap()
            .total_cmp(&a["total"].as_f64().unwrap())
    });
    println!("Window pairs on their own, best and worst (USDT after fees from {first_pick}):");
    for c in ranked
        .iter()
        .take(10)
        .chain(ranked.iter().rev().take(5).rev())
    {
        println!(
            "  {:>3} / {:>2} trades: {:+9.2} over {} test windows ({} positive), {} trades",
            c["train_trades"],
            c["test_trades"],
            c["total"].as_f64().unwrap(),
            c["windows"],
            c["positive"],
            c["trades"]
        );
    }
    let profitable = config_json
        .iter()
        .filter(|c| c["total"].as_f64().unwrap() > 0.0)
        .count();
    println!(
        "  {profitable} of {} window pairs made money on their own",
        configs.len()
    );

    // Step 2 TRADE: locked score, from lock (or --first-choice floor) until trade_end.
    println!(
        "\nStep 2 TRADE: {} → {} ({}, lookback {lookback_days}d). Locked score rel_err={:.3}%. Writing stop and allow_short reports.",
        utc(trade_start_ms),
        utc(trade_end_ms),
        if args.trade_days == 0 {
            "until scan end".to_string()
        } else {
            format!("{} day cap", args.trade_days)
        },
        score_model.score_rel_err * 100.0
    );
    let refine = args.refine_exits.then_some(ExitRefine {
        ladders: &args.ladders,
        shares: &args.shares,
        breakeven: &args.breakeven,
        exit_alts: args.exit_alts,
    });
    if args.refine_exits {
        println!(
            "  exit refine ON: ladders={:?} shares={:?} breakeven={:?} exit_alts={}",
            args.ladders, args.shares, args.breakeven, args.exit_alts
        );
    }
    let step2_stop = run_step2(
        &chains,
        &space,
        &mk,
        meta.size_btc,
        trade_start_ms,
        trade_end_ms,
        lookback_ms,
        lookback_days,
        args.balance_usdt,
        ShortOos::Stop,
        refine.as_ref(),
        &score_model,
    )?;
    let step2_short = run_step2(
        &chains,
        &space,
        &mk,
        meta.size_btc,
        trade_start_ms,
        trade_end_ms,
        lookback_ms,
        lookback_days,
        args.balance_usdt,
        ShortOos::AllowShort,
        refine.as_ref(),
        &score_model,
    )?;

    let summarize = |label: &str, out: &Step2Out| {
        let total = out.balance - args.balance_usdt;
        let positive = out
            .windows
            .iter()
            .filter(|w| w["test_net"].as_f64().unwrap() > 0.0)
            .count();
        let bh_total: f64 = out
            .windows
            .iter()
            .map(|w| w["buy_and_hold"].as_f64().unwrap())
            .sum();
        println!(
            "\n[{label}] {} cycles, {:+.2} USDT, {positive} profitable; BH {:+.2}; short folds {}; empty folds {}; stopped because {}",
            out.windows.len(),
            total,
            bh_total,
            out.short_folds,
            out.empty_folds,
            out.stop_reason
        );
        (total, positive, bh_total)
    };
    let (total_stop, positive_stop, bh_stop) = summarize("stop", &step2_stop);
    let (total_short, positive_short, bh_short) = summarize("allow_short", &step2_short);

    let mut distinct: Vec<u32> = step2_stop
        .traded
        .iter()
        .chain(step2_short.traded.iter())
        .copied()
        .collect();
    distinct.sort_unstable();
    distinct.dedup();
    if !args.refine_exits && space.exit_profiles.is_none() {
        let mut log = Vec::new();
        let mut all = Vec::new();
        for &id in &distinct {
            let (f, p, tp, sl, _) = space.decode(id);
            let lev = f64::from(space.leverage(id));
            log.clear();
            mk.replay(id, None, &mut log);
            all.clear();
            Entries::new(&mk.signals(f, p), &frames[f], &tape, &space.grid, costs).trades(
                tp,
                sl,
                |t| all.push(t),
            );
            ensure!(
                all.len() == log.len(),
                "{}: {} trades, the scanner {}",
                space.label(id),
                all.len(),
                log.len()
            );
            for (a, b) in all.iter().zip(&log) {
                ensure!(
                    tape.time_ms(a.exit_step as usize) == b.exit_ms
                        && (f64::from(a.net) * lev - f64::from(b.net)).abs() < 1e-3 * lev.max(1.0),
                    "{}: trade closed at {} differs from the scanner's",
                    space.label(id),
                    utc(b.exit_ms)
                );
            }
        }
        println!(
            "{} combinations from both Step-2 paths, replayed trade by trade: identical",
            distinct.len()
        );
    } else if space.exit_profiles.is_some() {
        println!(
            "Skipping Entries↔scanner check (scan uses scale-out exit profiles; Nautilus verify still runs)"
        );
    } else {
        println!(
            "{} combinations from both Step-2 paths (exit refine on — skip Entries↔single-TP check)",
            distinct.len()
        );
    }

    let falling = |windows: &[Value]| -> f64 {
        windows
            .iter()
            .filter(|w| w["buy_and_hold"].as_f64().unwrap() < 0.0)
            .map(|w| w["test_net"].as_f64().unwrap())
            .sum()
    };
    let build_report = |policy: ShortOos, out: &Step2Out, total: f64, positive: usize, bh_total: f64| -> Value {
        json!({
            "meta": {
                "instrument": okx_history::INSTRUMENT,
                "start": meta.start.to_string(),
                "end": meta.end.to_string(),
                "days": meta.days,
                "hours": outer_hours,
                "lookback_days": lookback_days,
                "tape_from": utc(tape_from_ms),
                "timeframes": space.timeframes.clone(),
                "lengths": space.lengths.len(),
                "pairs": space.pairs.len(),
                "tp": {
                    "min": space.grid.tp.first(),
                    "max": space.grid.tp.last(),
                    "n": space.grid.tp.len(),
                    "values": space.grid.tp.clone(),
                },
                "sl": {
                    "min": space.grid.sl.first(),
                    "max": space.grid.sl.last(),
                    "n": space.grid.sl.len(),
                    "values": space.grid.sl.clone(),
                },
                "leverage": {
                    "min": space.grid.lev.first(),
                    "max": space.grid.lev.last(),
                    "n": space.grid.lev.len(),
                    "values": space.grid.lev.clone(),
                },
                "combinations": n_combos,
                "ranking_warmup": ranking_warmup.to_string(),
                "first_pick": first_pick.to_string(),
                "first_choice": first_choice.to_string(),
                "window_pairs": configs.len(),
                "max_finalists": args.max_finalists,
                "memory_gb": args.memory_gb,
                "threads": args.threads,
                "min_trades": MIN_TRADES,
                "min_drop": MIN_DROP,
                "size_btc": meta.size_btc,
                "balance_usdt": args.balance_usdt,
                "taker_fee_bps": config.venues.okx.taker_fee_bps,
                "maker_fee_bps": config.venues.okx.maker_fee_bps,
                "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
                "short_oos_policy": policy.label(),
                "refine_exits": args.refine_exits,
                "ladders": args.ladders,
                "shares": args.shares,
                "breakeven": args.breakeven,
                "exit_alts": args.exit_alts,
                "scan_version": meta.scan_version,
                "exit_profiles": meta.exit_profiles.clone(),
                "score_model": score_model_json(&score_model, args.max_rel_err),
                "score_models": score_models_json.clone(),
                "max_score_rel_err": args.max_rel_err,
                "require_fit": args.require_fit,
                "score_fit_pairs": fit_pairs.len(),
                "score_locked_at": utc(lock_ms.unwrap_or(trade_start_ms)),
                "trade_days": args.trade_days,
                "trade_start": utc(trade_start_ms),
                "trade_end": utc(trade_end_ms),
            },
            "windows": windows_json.clone(),
            "window_pairs": config_json.clone(),
            "chosen": {
                "windows": out.windows,
                "total": round2(total),
                "positive": positive,
                "buy_and_hold_total": round2(bh_total),
                "in_falling_windows": round2(falling(&out.windows)),
                "stop_reason": out.stop_reason,
                "combinations_checked": distinct.len(),
                "short_folds": out.short_folds,
                "empty_folds": out.empty_folds,
                "short_oos_policy": policy.label(),
            },
            "verify": [],
        })
    };
    let mut report_stop = build_report(
        ShortOos::Stop,
        &step2_stop,
        total_stop,
        positive_stop,
        bh_stop,
    );
    let mut report_short = build_report(
        ShortOos::AllowShort,
        &step2_short,
        total_short,
        positive_short,
        bh_short,
    );

    let save = |stem: &str, report: &Value| -> Result<()> {
        let base = args.scan.join(stem);
        fs::write(
            base.with_extension("json"),
            serde_json::to_string_pretty(report)?,
        )?;
        write_report_html(&args.scan, stem, report)?;
        Ok(())
    };
    save("nm_walk_forward", &report_stop)?;
    save("nm_walk_forward_allow_short", &report_short)?;
    println!(
        "\nStop (original): {}",
        args.scan.join("nm_walk_forward.html").display()
    );
    println!(
        "Allow short/empty folds: {}",
        args.scan
            .join("nm_walk_forward_allow_short.html")
            .display()
    );

    let make_check = |id: u32, exits: Exits| -> Check {
        let mut record = Record::default();
        mk.replay_exits(id, &exits, Some(&mut record), &mut Vec::new());
        let (f, p, ..) = space.decode(id);
        let (i, j) = space.pairs[p];
        Check {
            label: space.label_with_exits(id, &exits),
            minutes: space.timeframes[f],
            fast: space.lengths[i],
            slow: space.lengths[j],
            exits,
            size_btc: meta.size_btc * f64::from(space.leverage(id)),
            fills: record.fills,
        }
    };
    let checks: Vec<(ShortOos, Check)> = [
        (ShortOos::Stop, step2_stop.last_exits.clone()),
        (ShortOos::AllowShort, step2_short.last_exits.clone()),
    ]
    .into_iter()
    .filter_map(|(p, last)| {
        (args.verify > 0)
            .then_some(last)
            .flatten()
            .map(|(id, exits)| (p, make_check(id, exits)))
    })
    .collect();

    drop(mk);
    drop(frames);
    drop(tape);

    // Same UTC window as Tape::load / scanner replay (clamped lookback → end).
    let verify_start = DateTime::<Utc>::from_timestamp_millis(tape_from_ms)
        .context("bad tape_from")?
        .date_naive();
    ensure!(
        ms(verify_start) == tape_from_ms,
        "verify start {verify_start} must match tape_from {}",
        utc(tape_from_ms)
    );

    for (policy, check) in checks {
        let v = verify(&meta, &args, &config, check, verify_start)?;
        let ok = v["ok"] == true;
        match policy {
            ShortOos::Stop => {
                report_stop["verify"] = Value::Array(vec![v]);
                save("nm_walk_forward", &report_stop)?;
            }
            ShortOos::AllowShort => {
                report_short["verify"] = Value::Array(vec![v]);
                save("nm_walk_forward_allow_short", &report_short)?;
            }
        }
        println!("NautilusTrader check ({}) saved", policy.label());
        if !ok {
            bail!(
                "NautilusTrader and the scanner disagree ({})",
                policy.label()
            );
        }
    }
    Ok(())
}

/// A combination to rerun through NautilusTrader, with the scanner's fills.
struct Check {
    label: String,
    minutes: usize,
    fast: usize,
    slow: usize,
    exits: Exits,
    size_btc: f64,
    fills: Vec<Fill>,
}

/// Reruns `check` through NautilusTrader over `verify_start`..`meta.end` (the same window as the
/// scanner tape / replay, including lookback before scan start) and compares every fill.
fn verify(
    meta: &Meta,
    args: &Args,
    config: &AppConfig,
    check: Check,
    verify_start: NaiveDate,
) -> Result<Value> {
    let name = format!("{} ({} to {})", check.label, verify_start, meta.end);
    let run = Run {
        config,
        data: &args.data,
        start: verify_start,
        end: meta.end,
        bar_minutes: check.minutes as u64,
        size_btc: check.size_btc,
        balance_usdt: REPLAY_BALANCE,
    };
    let signals = FilteredCross::new(
        RsiCrossParams {
            fast: check.fast,
            slow: check.slow,
        },
        Filter::default(),
    );
    let started = Instant::now();
    println!("NautilusTrader check: {name}");
    let mut shown = 0;
    let (_, mut theirs) = run_backtest(
        &run,
        Box::new(signals) as Box<dyn BarSignals>,
        Some(check.exits),
        |done, total| {
            if done * 10 / total.max(1) > shown {
                shown = done * 10 / total.max(1);
                println!("  archive {done} of {total}");
            }
        },
    )?;
    let mut ours = check.fills;
    let key = |f: &Fill| {
        (
            f.time_ms,
            f.kind,
            f.buy,
            (f.price * 10.0).round() as i64,
            (f.size_btc / STEP_BTC).round() as i64,
        )
    };
    ours.sort_by_key(key);
    theirs.sort_by_key(key);
    let same = |a: &Fill, b: &Fill| {
        a.time_ms == b.time_ms
            && a.kind == b.kind
            && a.buy == b.buy
            && (a.size_btc - b.size_btc).abs() < 1e-9
            && (a.price - b.price).abs() < 1e-6
            && (a.fee_usdt - b.fee_usdt).abs() < 1e-6
    };
    let bad: Vec<usize> = (0..ours.len().max(theirs.len()))
        .filter(|&k| !matches!((ours.get(k), theirs.get(k)), (Some(a), Some(b)) if same(a, b)))
        .collect();
    for &k in bad.iter().take(3) {
        println!(
            "    fill {}: scanner {:?} | Nautilus {:?}",
            k + 1,
            ours.get(k),
            theirs.get(k)
        );
    }
    println!(
        "  {} fills, {} differ (time, kind, side, size, price, fee) ({:.0}s)",
        theirs.len(),
        bad.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(
        json!({ "pair": name, "fills": theirs.len(), "differ": bad.len(), "ok": bad.is_empty(), "seconds": started.elapsed().as_secs_f64() }),
    )
}

const REPORT_HTML: &str = include_str!("window_nm_walk_forward_report.html");
const PRICE_REPORT_HTML: &str = include_str!("window_nm_price_walk_forward_report.html");
const TABLE_SORT: &str = include_str!("table_sort.js");

#[cfg(test)]
mod tests {
    use super::*;

    /// Trades entered at 0, 10, 20, ... and closed 5 later.
    fn finalist(id: u32, nets: &[f64]) -> Finalist {
        let mut cum = 0.0;
        let trades = nets
            .iter()
            .enumerate()
            .map(|(k, n)| {
                cum += n;
                Kept {
                    entry_ms: k as i64 * 10,
                    exit_ms: k as i64 * 10 + 5,
                    cum,
                }
            })
            .collect();
        Finalist { id, trades }
    }

    fn brute_force(
        finalists: &[Finalist],
        at: i64,
        n: usize,
        lookback: i64,
        model: &ScoreModel,
    ) -> Option<usize> {
        finalists
            .iter()
            .enumerate()
            .filter_map(|(k, f)| {
                let closed: Vec<usize> = (0..f.trades.len())
                    .filter(|&i| f.trades[i].exit_ms <= at && f.trades[i].exit_ms > at - lookback)
                    .collect();
                let last: Vec<usize> = closed.iter().rev().take(n).rev().copied().collect();
                (last.len() >= MIN_TRADES as usize).then(|| {
                    let mut t = Tally::default();
                    for &i in &last {
                        t.add(f.net(i), 5);
                    }
                    let s = t.stats();
                    (k, model.predict(&s).unwrap(), s.profit)
                })
            })
            .filter(|(_, _, profit)| f64::from(*profit) > MIN_IS_PROFIT)
            .min_by(|a, b| {
                b.1.total_cmp(&a.1)
                    .then(b.2.total_cmp(&a.2))
                    .then(finalists[a.0].id.cmp(&finalists[b.0].id))
            })
            .map(|x| x.0)
    }

    /// Skipping finalists that cannot beat the best so far picks the same as scoring them all,
    /// and trades closed before the lookback are never scored.
    #[test]
    fn picks_like_scoring_every_finalist_in_the_lookback() {
        let mut x: u64 = 5;
        let mut rand = || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as f64 / (1u64 << 31) as f64
        };
        let mut picked = 0;
        for round in 0..400 {
            let finalists: Vec<Finalist> = (0..40)
                .map(|id| {
                    let len = 3 + (rand() * 30.0) as usize;
                    let drift = rand() - 0.5;
                    let nets: Vec<f64> = (0..len)
                        .map(|_| ((rand() - 0.5 + drift) * 6.0).round())
                        .collect();
                    finalist((id * 7 % 40) as u32, &nets)
                })
                .collect();
            let (at, n, lookback) = (
                (rand() * 350.0) as i64,
                3 + (rand() * 12.0) as usize,
                40 + (rand() * 200.0) as i64,
            );
            let model = ScoreModel::cold_start();
            let got = pick(&finalists, at, n, lookback, &model).map(|p| p.finalist);
            assert_eq!(
                got,
                brute_force(&finalists, at, n, lookback, &model),
                "round {round}"
            );
            picked += usize::from(got.is_some());
        }
        assert!(picked > 100, "too few picks tested: {picked}");
    }

    #[test]
    fn trades_before_the_lookback_do_not_count() {
        // Exits at 5, 15, ..., 95: at 95 with a lookback of 50 only the trades closed at 55..95.
        assert_eq!(training(|i| i as i64 * 10 + 5, 10, 95, 100, 50), 5..10);
        assert_eq!(training(|i| i as i64 * 10 + 5, 10, 95, 3, 50), 7..10);
        // Fewer than MIN_TRADES in the lookback: no pick, even with many trades before it.
        let f = finalist(1, &[5.0; 10]);
        assert!(pick(&[f], 95, 100, 40, &ScoreModel::cold_start()).is_none());
    }

    #[test]
    fn exit_catalog_includes_single_tp_and_ladders() {
        let space = Space {
            timeframes: vec![1],
            lengths: vec![1, 2],
            pairs: vec![(0, 1)],
            grid: Grid {
                tp: vec![0.5],
                sl: vec![0.5],
                lev: vec![1],
            },
            survivor_groups: None,
            exit_profiles: None,
        };
        let ladders = parse_groups("1,1/2").unwrap();
        let shares = parse_groups("100,50/50").unwrap();
        let cat = exit_catalog(&space, 0, &ladders, &shares, &[false, true], 50).unwrap();
        assert!(cat.len() >= 2, "expected single + ladders, got {}", cat.len());
        assert_eq!(cat[0].targets.len(), 1);
        assert!(cat.iter().any(|e| e.targets.len() > 1));
    }

    #[test]
    fn a_chain_trades_the_next_m_trades_entered_at_or_after_the_pick() {
        let space = Space {
            timeframes: vec![1],
            lengths: vec![1, 2],
            pairs: vec![(0, 1)],
            grid: Grid {
                tp: vec![1.0],
                sl: vec![1.0],
                lev: vec![1],
            },
            survivor_groups: None,
            exit_profiles: None,
        };
        let f = finalist(0, &[1.0, 1.0, 1.0, -1.0, 2.0, 2.0, 2.0, 1.0, 1.0]);
        let mut chain = Chain {
            n: 5,
            m: 2,
            at: 46,
            done: false,
            windows: Vec::new(),
            trades: Vec::new(),
            stop_reason: String::new(),
        };
        // At 46 five trades are known; the 6th was entered at 50, the 7th at 60.
        let pairs = advance(&mut chain, &[f], 60, 1000, &space, &ScoreModel::cold_start());
        assert_eq!(pairs.len(), 1, "one N→M train pair from the pick");
        assert_eq!(chain.windows, vec![4.0]);
        assert_eq!(chain.at, 65);
        assert_eq!(
            (chain.trades[0].entry_ms, chain.trades[1].exit_ms),
            (50, 65)
        );
    }
}
