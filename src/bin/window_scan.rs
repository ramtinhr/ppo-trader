use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration as StdDuration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use dynamic_trader::{
    config::AppConfig,
    exits::{ExitProfile, Exits, default_scaleout_profiles},
    grid::{parse_lengths, rsi_series},
    okx_history,
    scan::{self, ClosedTrade, Costs, Market, Record, STEP_BTC},
    tape::{CloseRanges, Tape},
    tpsl_grid::{Entries, Frame, Grid, SIGNAL_BLOCK, parse_leverage, parse_range},
    window_score::{
        GroupBest, RECORD, Tally, WindowStats, top_groups, union_survivor_groups,
    },
};
use rayon::prelude::*;
use serde_json::json;

const USAGE: &str = "\
Every RSI pair, on every timeframe, with take-profit × stop-loss × leverage, replayed on every
OKX BTC-USDT-SWAP trade in equal outer windows (days or hours).

Default is --staged (two passes). Stage 1 scores every RSI pair × TF on a coarse TP/SL grid at
leverage 1. Survivors (top groups per window, union capped) go to stage 2 on SL × exit-profile ×
leverage (scale-out multi-TP templates, not a fine TP×SL grid). Stage 1 still uses Entries at
base size with leverage scaled; stage 2 replays each profile with the trade-by-trade scanner.

Stage 1 writes stage1/window_*.bin (coarse, all groups). Stage 2 writes window_*.bin (profiles,
survivors only). meta.json is scan_version 3 when staged; older dense full-grid scans stay
version 1. Use --full-grid for the previous single-pass fine TP×SL×lev grid over every pair.

In each window every combination starts flat at the window's first minute, trades every signal
inside it, and a trade still open at the end is closed at the window's last price with the taker
fee. Leverage scales position size (qty = size_btc × leverage). window_walk_forward /
browse_window_scan / window_nm_walk_forward consume the results.

Examples:
  cargo run --release --bin window_scan -- \\
    --start 2026-09-01 --end 2026-10-01 --hours 12
  cargo run --release --bin window_scan -- \\
    --start 2026-09-01 --end 2026-10-01 --hours 12 \\
    --coarse-tp 0.1-1/0.1 --coarse-sl 0.1-1/0.1 \\
    --tp 0.01-1/0.01 --sl 0.01-1/0.01 --leverage 1-10/1 \\
    --finalists 40 --survivor-cap 120
  cargo run --release --bin window_scan -- --full-grid --hours 12 --start ... --end ...

Usage: cargo run --release --bin window_scan -- [options]

  --start <YYYY-MM-DD>      First UTC day, inclusive (default: 2025-10-01).
                            For window_nm_walk_forward, the first outer window is ranking-only;
                            include warmup days before the OOS month (e.g. 2026-08-27 for Sept).
  --end <YYYY-MM-DD>        Last UTC day, exclusive (default: 2026-10-01)
  --days <n>                Days per outer window (default: 10). Ignored if --hours is set.
                            Use 1 for day-by-day (24h) finalist refresh.
  --hours <n>               Hours per outer window (e.g. 12, 24, 48, 72). Overrides --days.
                            Windows start at UTC midnight of --start and step by n hours.
  --timeframes <list>       Candle sizes in minutes (default: 1,3,5,15,60)
  --lengths <list>          RSI lengths (default: 1-50,60-400/10)
  --staged                  Two-pass scan (default). Output dir ends with _staged.
  --full-grid               Single-pass fine TP×SL×lev on every pair (old behaviour).
  --coarse-tp <from-to/step> Stage-1 take-profit % (default: 0.1-1/0.1; staged only)
  --coarse-sl <from-to/step> Stage-1 stop-loss % (default: 0.1-1/0.1; staged only)
  --tp <from-to/step>       Fine take-profit % (default: 0.01-1/0.01); full-grid only
  --sl <from-to/step>       Fine stop-loss % (default: 0.01-1/0.01); stage 2 or full-grid
  --leverage <from-to/step> Leverage multipliers (default: 1-10/1); stage 2 or full-grid
                            Stage 1 always uses leverage 1 only.
                            Staged stage 2 axis: exit_profile × SL × leverage (scan_version 3).
  --finalists <n>           Top RSI pair×TF groups kept per window after stage 1 (default: 40)
  --survivor-cap <n>        Max unique survivor groups after union across windows (default: 120)
  --min-profit <usdt>       Stage-1 best-setting profit must be >= this to survive (optional)
  --every-pair <n>          Only every n-th RSI pair, to measure the time (default: 1 = all)
  --check <n>               Combinations per timeframe replayed again by the trade-by-trade
                            scanner in a random window, which must agree (default: 20)
  --size-btc <btc>          Base position size in BTC before leverage (default: 0.005)
  --config <path>           Fee config (default: config/testnet.toml)
  --data <dir>              Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --out <dir>               Where to write the results (default: data/windowscan)";

/// Balance of the replays: large enough that no order is ever refused, so every trade's result
/// depends only on the prices.
const REPLAY_BALANCE: f64 = 1e9;

struct Args {
    start: NaiveDate,
    end: NaiveDate,
    /// Whole-day outer windows when `hours` is None.
    days: u64,
    /// If set, overrides `days` (outer window length in hours).
    hours: Option<u64>,
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    staged: bool,
    coarse_tp: Vec<f64>,
    coarse_sl: Vec<f64>,
    tp: Vec<f64>,
    sl: Vec<f64>,
    leverage: Vec<u32>,
    finalists: usize,
    survivor_cap: usize,
    min_profit: Option<f32>,
    every_pair: usize,
    check: usize,
    size_btc: f64,
    config: PathBuf,
    data: PathBuf,
    out: PathBuf,
}

impl Args {
    fn window_hours(&self) -> u64 {
        self.hours.unwrap_or(self.days.saturating_mul(24))
    }

    /// Output folder suffix: `12h` when --hours was used, else `2d` for --days.
    fn dir_suffix(&self) -> String {
        match self.hours {
            Some(h) => format!("{h}h"),
            None => format!("{}d", self.days),
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        start: "2025-10-01".parse()?,
        end: "2026-10-01".parse()?,
        days: 10,
        hours: None,
        timeframes: vec![1, 3, 5, 15, 60],
        lengths: parse_lengths("1-50,60-400/10")?,
        staged: true,
        coarse_tp: parse_range("0.1-1/0.1")?,
        coarse_sl: parse_range("0.1-1/0.1")?,
        tp: parse_range("0.01-1/0.01")?,
        sl: parse_range("0.01-1/0.01")?,
        leverage: parse_leverage("1-10/1")?,
        finalists: 40,
        survivor_cap: 120,
        min_profit: None,
        every_pair: 1,
        check: 20,
        size_btc: 0.005,
        config: "config/testnet.toml".into(),
        data: "data/okx/BTC-USDT-SWAP/archives".into(),
        out: "data/windowscan".into(),
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        if flag == "--staged" {
            args.staged = true;
            continue;
        }
        if flag == "--full-grid" {
            args.staged = false;
            continue;
        }
        let value = raw
            .next()
            .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
        let bad = || format!("bad value for {flag}: {value}");
        match flag.as_str() {
            "--start" => args.start = value.parse().with_context(bad)?,
            "--end" => args.end = value.parse().with_context(bad)?,
            "--days" => args.days = value.parse().with_context(bad)?,
            "--hours" => args.hours = Some(value.parse().with_context(bad)?),
            "--timeframes" => {
                args.timeframes = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?
            }
            "--lengths" => args.lengths = parse_lengths(&value).with_context(bad)?,
            "--coarse-tp" => args.coarse_tp = parse_range(&value).with_context(bad)?,
            "--coarse-sl" => args.coarse_sl = parse_range(&value).with_context(bad)?,
            "--tp" => args.tp = parse_range(&value).with_context(bad)?,
            "--sl" => args.sl = parse_range(&value).with_context(bad)?,
            "--leverage" => args.leverage = parse_leverage(&value).with_context(bad)?,
            "--finalists" => args.finalists = value.parse().with_context(bad)?,
            "--survivor-cap" => args.survivor_cap = value.parse().with_context(bad)?,
            "--min-profit" => args.min_profit = Some(value.parse().with_context(bad)?),
            "--every-pair" => args.every_pair = value.parse().with_context(bad)?,
            "--check" => args.check = value.parse().with_context(bad)?,
            "--size-btc" => args.size_btc = value.parse().with_context(bad)?,
            "--config" => args.config = value.into(),
            "--data" => args.data = value.into(),
            "--out" => args.out = value.into(),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(args.start < args.end, "--start must be before --end");
    ensure!(args.days > 0, "--days must be at least 1");
    if let Some(h) = args.hours {
        ensure!(h > 0, "--hours must be at least 1");
    }
    ensure!(
        args.lengths.len() >= 2,
        "--lengths needs at least two lengths"
    );
    ensure!(
        !args.timeframes.is_empty() && args.timeframes.iter().all(|m| *m > 0 && 1440 % m == 0),
        "every timeframe must divide a day into whole candles"
    );
    args.timeframes.sort_unstable();
    args.timeframes.dedup();
    ensure!(args.every_pair > 0, "--every-pair must be at least 1");
    ensure!(!args.leverage.is_empty(), "--leverage needs at least one value");
    ensure!(!args.tp.is_empty() && !args.sl.is_empty(), "--tp/--sl need values");
    if args.staged {
        ensure!(
            !args.coarse_tp.is_empty() && !args.coarse_sl.is_empty(),
            "--coarse-tp/--coarse-sl need values"
        );
        ensure!(args.finalists > 0, "--finalists must be at least 1");
        ensure!(args.survivor_cap > 0, "--survivor-cap must be at least 1");
    }
    let steps = args.size_btc * 10_000.0;
    ensure!(
        args.size_btc >= 0.0001 && (steps.round() - steps).abs() < 1e-6,
        "--size-btc must use OKX's 0.0001 BTC step"
    );
    Ok(args)
}

fn ms_day(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn ms_dt(dt: NaiveDateTime) -> i64 {
    dt.and_utc().timestamp_millis()
}

/// Label for files / display: date-only at midnight when using day windows; else `YYYY-MM-DD_HH`.
fn window_label(dt: NaiveDateTime, hour_mode: bool) -> String {
    if !hour_mode && dt.time() == chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
        dt.date().to_string()
    } else {
        format!("{}_{:02}", dt.date(), dt.hour())
    }
}

fn window_start_str(dt: NaiveDateTime, hour_mode: bool) -> String {
    if !hour_mode && dt.time() == chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
        dt.date().to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    }
}

fn utc(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

/// File layout index for one RSI pair × timeframe task.
struct GroupMap {
    /// `file_index = f * n_pairs + p` (full grid / stage 1).
    n_pairs: usize,
    /// If set, only these original group ids are written; file index is position in this list.
    survivors: Option<Vec<usize>>,
}

impl GroupMap {
    fn full(n_pairs: usize) -> Self {
        Self {
            n_pairs,
            survivors: None,
        }
    }

    fn survivors(n_pairs: usize, survivors: Vec<usize>) -> Self {
        Self {
            n_pairs,
            survivors: Some(survivors),
        }
    }

    fn file_index(&self, f: usize, p: usize) -> usize {
        let group = f * self.n_pairs + p;
        match &self.survivors {
            None => group,
            Some(ids) => ids.binary_search(&group).expect("survivor group"),
        }
    }

    fn group_id(&self, f: usize, p: usize) -> usize {
        f * self.n_pairs + p
    }
}

/// Replay every task into dense window files. Builds Entries once per group (signals + TP/SL
/// hits at base size); scales tallies by leverage. `stage` labels progress lines.
fn replay_groups(
    stage: &str,
    tasks: &[(usize, usize)],
    finished: &HashSet<usize>,
    pairs: &[(usize, usize)],
    lengths: &[usize],
    frames: &[Frame],
    rsi: &[Vec<Vec<f64>>],
    tape: &Tape,
    grid: &Grid,
    costs: Costs,
    window_step: &[u32],
    window_close: &[f64],
    files: &[File],
    map: &GroupMap,
    n_file_groups: usize,
    done_path: &Path,
) -> Result<Vec<Vec<u64>>> {
    let n_settings = grid.len();
    let n_windows = files.len();
    let pending: Vec<(usize, usize)> = tasks
        .iter()
        .copied()
        .filter(|(f, p)| !finished.contains(&map.group_id(*f, *p)))
        .collect();
    if !finished.is_empty() {
        println!(
            "{stage}: resuming — {} groups already done, {} left",
            tasks.len() - pending.len(),
            pending.len()
        );
    }
    println!(
        "{stage}: {} combinations ({} groups × {} TP × {} SL × {} lev) in {n_windows} windows; replaying {} of {} groups",
        n_file_groups * n_settings,
        n_file_groups,
        grid.tp.len(),
        grid.sl.len(),
        grid.lev.len(),
        pending.len(),
        tasks.len(),
    );

    let done_log = Mutex::new((
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(done_path)?,
        Vec::<usize>::new(),
    ));
    let flush_batch = |batch: Vec<usize>| -> Result<()> {
        for file in files {
            file.sync_data()?;
        }
        let text: String = batch.into_iter().map(|g| format!("{g}\n")).collect();
        let mut log = done_log.lock().unwrap();
        log.0.write_all(text.as_bytes())?;
        log.0.sync_data()?;
        Ok(())
    };

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let last_report_secs = Mutex::new(-1.0f64);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    println!(
        "{stage}: starting on {threads} workers (progress every group at first, then every 10s or 10 groups; heartbeat every 15s)"
    );
    let _ = io::stdout().flush();
    let alive = AtomicBool::new(true);
    let trades_seen = thread::scope(|scope| -> Result<Vec<Vec<u64>>> {
        scope.spawn(|| {
            let mut ticks = 0u32;
            while alive.load(Ordering::Relaxed) {
                thread::sleep(StdDuration::from_secs(1));
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                ticks += 1;
                if ticks % 15 != 0 {
                    continue;
                }
                let d = done.load(Ordering::Relaxed);
                let secs = started.elapsed().as_secs_f64();
                let rate = if secs > 0.0 { d as f64 / secs } else { 0.0 };
                let left = if rate > 1e-9 {
                    (pending.len().saturating_sub(d)) as f64 / rate / 60.0
                } else {
                    f64::NAN
                };
                if left.is_finite() {
                    println!(
                        "{stage}: ... still working: {d} of {} groups done ({:.2} groups/s), {:.0}s elapsed, about {:.0} min left",
                        pending.len(),
                        rate,
                        secs,
                        left
                    );
                } else {
                    println!(
                        "{stage}: ... still working: {d} of {} groups done, {:.0}s elapsed (first group in progress)",
                        pending.len(),
                        secs
                    );
                }
                let _ = io::stdout().flush();
            }
        });
        let result = (0..threads)
            .into_par_iter()
            .map(|_| -> Result<Vec<u64>> {
                let mut trades_seen = vec![0u64; frames.len()];
                let mut bytes = vec![0u8; n_settings * RECORD];
                let n_tpsl = grid.tpsl_len();
                let n_lev = grid.lev.len();
                let n_sl = grid.sl.len();
                let mut position = vec![0usize; n_tpsl];
                let mut tallies = vec![Tally::default(); n_tpsl];
                loop {
                    let t = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(f, p)) = pending.get(t) else { break };
                    let (i, j) = pairs[p];
                    let signals = scan::signals(&rsi[f][i], &rsi[f][j]);
                    // One Entries build per group: all TP/SL hits at base size; leverage scales.
                    let entries = Entries::new(&signals, &frames[f], tape, grid, costs);
                    let file_g = map.file_index(f, p);
                    let first = (file_g * n_settings * RECORD) as u64;
                    for (w, file) in files.iter().enumerate() {
                        let end = (window_step[w + 1], window_close[w]);
                        let from = entries.first_at(window_step[w]);
                        let limit = entries.first_at(end.0);
                        position.fill(from);
                        tallies.fill(Tally::default());
                        for block_start in (from..limit).step_by(SIGNAL_BLOCK) {
                            let block_end = (block_start + SIGNAL_BLOCK).min(limit);
                            for (tpsl, (k, tally)) in
                                position.iter_mut().zip(tallies.iter_mut()).enumerate()
                            {
                                let (tp, sl) = (tpsl / n_sl, tpsl % n_sl);
                                entries.trades_until(
                                    tp,
                                    sl,
                                    k,
                                    block_end,
                                    Some(end),
                                    |trade| {
                                        let held = tape.time_ms(trade.exit_step as usize)
                                            - entries.entry_ms[trade.signal as usize];
                                        tally.add(f64::from(trade.net), held);
                                    },
                                );
                            }
                        }
                        for tpsl in 0..n_tpsl {
                            let base = tallies[tpsl].stats();
                            for (li, &lev) in grid.lev.iter().enumerate() {
                                let s = tpsl * n_lev + li;
                                let stats = base.scale_leverage(lev);
                                trades_seen[f] += u64::from(stats.trades);
                                bytes[s * RECORD..(s + 1) * RECORD]
                                    .copy_from_slice(&stats.bytes());
                            }
                        }
                        file.write_all_at(&bytes, first)?;
                    }
                    let group_id = map.group_id(f, p);
                    let batch = {
                        let mut log = done_log.lock().unwrap();
                        log.1.push(group_id);
                        if log.1.len() >= 100 {
                            Some(std::mem::take(&mut log.1))
                        } else {
                            None
                        }
                    };
                    if let Some(batch) = batch {
                        flush_batch(batch)?;
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let secs = started.elapsed().as_secs_f64();
                    let report = {
                        let mut last = last_report_secs.lock().unwrap();
                        let due = d <= 5
                            || d == pending.len()
                            || d % 10 == 0
                            || *last < 0.0
                            || secs - *last >= 10.0;
                        if due {
                            *last = secs;
                            true
                        } else {
                            false
                        }
                    };
                    if report {
                        let rate = d as f64 / secs.max(1e-9);
                        let left_min = (pending.len() - d) as f64 / rate.max(1e-9) / 60.0;
                        println!(
                            "{stage}: replayed group {d} of {} ({:.2} groups/s, {}m RSI {}/{}), {:.0}s so far, about {:.0} min left",
                            pending.len(),
                            rate,
                            frames[f].minutes,
                            lengths[i],
                            lengths[j],
                            secs,
                            left_min
                        );
                        let _ = io::stdout().flush();
                    }
                }
                Ok(trades_seen)
            })
            .collect::<Result<Vec<Vec<u64>>>>();
        alive.store(false, Ordering::Relaxed);
        result
    })?;
    {
        let batch = std::mem::take(&mut done_log.lock().unwrap().1);
        if !batch.is_empty() {
            flush_batch(batch)?;
        }
    }
    println!(
        "{stage}: replayed {} groups in {:.0}s",
        pending.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(trades_seen)
}

/// Stage-2 scale-out: SL × exit profile × leverage via trade-by-trade [`scan::simulate`].
/// Layout matches [`Grid`]: `((profile * sl_count + sl) * lev_count + lev)`.
fn replay_groups_profiles(
    stage: &str,
    tasks: &[(usize, usize)],
    finished: &HashSet<usize>,
    pairs: &[(usize, usize)],
    lengths: &[usize],
    frames: &[Frame],
    rsi: &[Vec<Vec<f64>>],
    tape: &Tape,
    profiles: &[ExitProfile],
    sl: &[f64],
    lev: &[u32],
    costs: Costs,
    window_step: &[u32],
    window_close: &[f64],
    files: &[File],
    map: &GroupMap,
    n_file_groups: usize,
    done_path: &Path,
) -> Result<Vec<Vec<u64>>> {
    let n_settings = profiles.len() * sl.len() * lev.len();
    let n_windows = files.len();
    let pending: Vec<(usize, usize)> = tasks
        .iter()
        .copied()
        .filter(|(f, p)| !finished.contains(&map.group_id(*f, *p)))
        .collect();
    if !finished.is_empty() {
        println!(
            "{stage}: resuming — {} groups already done, {} left",
            tasks.len() - pending.len(),
            pending.len()
        );
    }
    println!(
        "{stage}: {} combinations ({} groups × {} profiles × {} SL × {} lev) in {n_windows} windows; simulating {} of {} groups",
        n_file_groups * n_settings,
        n_file_groups,
        profiles.len(),
        sl.len(),
        lev.len(),
        pending.len(),
        tasks.len(),
    );

    let done_log = Mutex::new((
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(done_path)?,
        Vec::<usize>::new(),
    ));
    let flush_batch = |batch: Vec<usize>| -> Result<()> {
        for file in files {
            file.sync_data()?;
        }
        let text: String = batch.into_iter().map(|g| format!("{g}\n")).collect();
        let mut log = done_log.lock().unwrap();
        log.0.write_all(text.as_bytes())?;
        log.0.sync_data()?;
        Ok(())
    };

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let last_report_secs = Mutex::new(-1.0f64);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    println!(
        "{stage}: starting on {threads} workers (progress every group at first, then every 10s or 10 groups; heartbeat every 15s)"
    );
    let _ = io::stdout().flush();
    let alive = AtomicBool::new(true);
    let trades_seen = thread::scope(|scope| -> Result<Vec<Vec<u64>>> {
        scope.spawn(|| {
            let mut ticks = 0u32;
            while alive.load(Ordering::Relaxed) {
                thread::sleep(StdDuration::from_secs(1));
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                ticks += 1;
                if ticks % 15 != 0 {
                    continue;
                }
                let d = done.load(Ordering::Relaxed);
                let secs = started.elapsed().as_secs_f64();
                let rate = if secs > 0.0 { d as f64 / secs } else { 0.0 };
                let left = if rate > 1e-9 {
                    (pending.len().saturating_sub(d)) as f64 / rate / 60.0
                } else {
                    f64::NAN
                };
                if left.is_finite() {
                    println!(
                        "{stage}: ... still working: {d} of {} groups done ({:.2} groups/s), {:.0}s elapsed, about {:.0} min left",
                        pending.len(),
                        rate,
                        secs,
                        left
                    );
                } else {
                    println!(
                        "{stage}: ... still working: {d} of {} groups done, {:.0}s elapsed (first group in progress)",
                        pending.len(),
                        secs
                    );
                }
                let _ = io::stdout().flush();
            }
        });
        let result = (0..threads)
            .into_par_iter()
            .map(|_| -> Result<Vec<u64>> {
                let mut trades_seen = vec![0u64; frames.len()];
                let mut bytes = vec![0u8; n_settings * RECORD];
                let n_sl = sl.len();
                let n_lev = lev.len();
                loop {
                    let t = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(f, p)) = pending.get(t) else { break };
                    let (i, j) = pairs[p];
                    let signals = scan::signals(&rsi[f][i], &rsi[f][j]);
                    let frame = &frames[f];
                    let closes =
                        CloseRanges::new(&frame.candles.iter().map(|c| c.close).collect::<Vec<_>>());
                    let market = Market {
                        candles: &frame.candles,
                        tape,
                        bar_start: &frame.bar_start,
                        closes: &closes,
                    };
                    let file_g = map.file_index(f, p);
                    let first = (file_g * n_settings * RECORD) as u64;
                    for (w, file) in files.iter().enumerate() {
                        let end_step = window_step[w + 1];
                        let end_close = window_close[w];
                        let bar_first =
                            frame.bar_start.partition_point(|&b| b < window_step[w]);
                        let bar_last = frame.bar_start.partition_point(|&b| b < end_step);
                        let bars = bar_first.saturating_sub(1)..bar_last;
                        let end_ms = tape.time_ms(end_step.saturating_sub(1) as usize);
                        for (pi, profile) in profiles.iter().enumerate() {
                            for (si, &stop) in sl.iter().enumerate() {
                                let exits = profile.to_exits(stop);
                                for (li, &leverage) in lev.iter().enumerate() {
                                    let s = (pi * n_sl + si) * n_lev + li;
                                    let mut costs_lev = costs;
                                    costs_lev.steps *= i64::from(leverage);
                                    let mut record = Record::default();
                                    scan::simulate(
                                        (0, 0, 0),
                                        &signals,
                                        Some(&exits),
                                        &market,
                                        bars.clone(),
                                        costs_lev,
                                        Some(&mut record),
                                    );
                                    let mut tally = Tally::default();
                                    for t in &record.trades {
                                        let sign = if t.long { 1.0 } else { -1.0 };
                                        let mut net = -t.entry_fee;
                                        let mut closed_steps = 0i64;
                                        for e in &t.exits {
                                            if e.kind == "open" {
                                                continue;
                                            }
                                            net += sign
                                                * (e.price - t.entry_price)
                                                * e.steps as f64
                                                * STEP_BTC
                                                - e.fee;
                                            closed_steps += e.steps;
                                        }
                                        let (exit_ms, net) = if t.open {
                                            let left = (t.steps - closed_steps).max(0);
                                            let qty = left as f64 * STEP_BTC;
                                            // Match Entries forced close: mark + taker on remainder.
                                            let net = net
                                                + sign * (end_close - t.entry_price) * qty
                                                - costs_lev.taker * qty * end_close;
                                            (end_ms, net)
                                        } else {
                                            let exit_ms =
                                                t.exits.last().map(|e| e.time_ms).unwrap_or(end_ms);
                                            (exit_ms, net)
                                        };
                                        tally.add(net, exit_ms - t.entry_ms);
                                    }
                                    let stats = tally.stats();
                                    trades_seen[f] += u64::from(stats.trades);
                                    bytes[s * RECORD..(s + 1) * RECORD]
                                        .copy_from_slice(&stats.bytes());
                                }
                            }
                        }
                        file.write_all_at(&bytes, first)?;
                    }
                    let group_id = map.group_id(f, p);
                    let batch = {
                        let mut log = done_log.lock().unwrap();
                        log.1.push(group_id);
                        if log.1.len() >= 100 {
                            Some(std::mem::take(&mut log.1))
                        } else {
                            None
                        }
                    };
                    if let Some(batch) = batch {
                        flush_batch(batch)?;
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let secs = started.elapsed().as_secs_f64();
                    let report = {
                        let mut last = last_report_secs.lock().unwrap();
                        let due = d <= 5
                            || d == pending.len()
                            || d % 10 == 0
                            || *last < 0.0
                            || secs - *last >= 10.0;
                        if due {
                            *last = secs;
                            true
                        } else {
                            false
                        }
                    };
                    if report {
                        let rate = d as f64 / secs.max(1e-9);
                        let left_min = (pending.len() - d) as f64 / rate.max(1e-9) / 60.0;
                        println!(
                            "{stage}: simulated group {d} of {} ({:.2} groups/s, {}m RSI {}/{}), {:.0}s so far, about {:.0} min left",
                            pending.len(),
                            rate,
                            frames[f].minutes,
                            lengths[i],
                            lengths[j],
                            secs,
                            left_min
                        );
                        let _ = io::stdout().flush();
                    }
                }
                Ok(trades_seen)
            })
            .collect::<Result<Vec<Vec<u64>>>>();
        alive.store(false, Ordering::Relaxed);
        result
    })?;
    {
        let batch = std::mem::take(&mut done_log.lock().unwrap().1);
        if !batch.is_empty() {
            flush_batch(batch)?;
        }
    }
    println!(
        "{stage}: simulated {} groups in {:.0}s",
        pending.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(trades_seen)
}

fn open_window_files(
    paths: &[PathBuf],
    n_combos: usize,
    resume: bool,
) -> Result<Vec<File>> {
    paths
        .iter()
        .map(|path| {
            if resume {
                return OpenOptions::new()
                    .write(true)
                    .open(path)
                    .with_context(|| path.display().to_string());
            }
            let file = File::create(path).with_context(|| path.display().to_string())?;
            file.set_len((n_combos * RECORD) as u64)?;
            Ok(file)
        })
        .collect()
}

fn read_done(path: &Path) -> HashSet<usize> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Best scored coarse setting per group in one stage-1 window file.
fn group_bests_in_window(
    path: &Path,
    n_groups: usize,
    n_settings: usize,
) -> Result<Vec<GroupBest>> {
    let bytes = fs::read(path).with_context(|| path.display().to_string())?;
    ensure!(
        bytes.len() == n_groups * n_settings * RECORD,
        "{} has the wrong size for stage-1 ranking",
        path.display()
    );
    let mut out = Vec::with_capacity(n_groups);
    for g in 0..n_groups {
        let mut best: Option<(f64, f32)> = None;
        for s in 0..n_settings {
            let off = (g * n_settings + s) * RECORD;
            let stats = WindowStats::from_bytes(&bytes[off..off + RECORD]);
            let Some(score) = stats.score() else { continue };
            let profit = stats.profit;
            let replace = match best {
                None => true,
                Some((bs, bp)) => score > bs || (score == bs && profit > bp),
            };
            if replace {
                best = Some((score, profit));
            }
        }
        if let Some((score, profit)) = best {
            out.push(GroupBest {
                group: g,
                score,
                profit,
            });
        }
    }
    Ok(out)
}

fn select_survivors(
    stage1_paths: &[PathBuf],
    n_groups: usize,
    n_settings: usize,
    finalists: usize,
    survivor_cap: usize,
    min_profit: Option<f32>,
) -> Result<Vec<usize>> {
    let mut per_window = Vec::with_capacity(stage1_paths.len());
    for path in stage1_paths {
        let ranked = group_bests_in_window(path, n_groups, n_settings)?;
        per_window.push(top_groups(ranked, finalists, min_profit));
    }
    let survivors = union_survivor_groups(&per_window, survivor_cap);
    ensure!(
        !survivors.is_empty(),
        "no stage-1 survivors; loosen --min-profit or raise --finalists"
    );
    Ok(survivors)
}

fn check_against_scanner(
    args: &Args,
    frames: &[Frame],
    rsi: &[Vec<Vec<f64>>],
    pairs: &[(usize, usize)],
    tape: &Tape,
    grid: &Grid,
    costs: Costs,
    window_step: &[u32],
    window_close: &[f64],
    labels: &[String],
    tasks: &[(usize, usize)],
    n_windows: usize,
) -> Result<usize> {
    let n_settings = grid.len();
    let mut x: u64 = 0x9e3779b97f4a7c15;
    let mut rand = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    let mut checked = 0;
    for (f, frame) in frames.iter().enumerate() {
        let closes = CloseRanges::new(&frame.candles.iter().map(|c| c.close).collect::<Vec<_>>());
        let market = Market {
            candles: &frame.candles,
            tape,
            bar_start: &frame.bar_start,
            closes: &closes,
        };
        let groups: Vec<usize> = tasks.iter().filter(|t| t.0 == f).map(|t| t.1).collect();
        if groups.is_empty() || n_settings == 0 || n_windows == 0 {
            continue;
        }
        for _ in 0..args.check {
            let p = groups[rand(groups.len())];
            let s = rand(n_settings);
            let w = rand(n_windows);
            let (i, j) = pairs[p];
            let (tp, sl, li) = grid.split(s);
            let lev = grid.lev[li];
            let mut costs_lev = costs;
            costs_lev.steps *= i64::from(lev);
            let qty_lev = costs_lev.steps as f64 * scan::STEP_BTC;
            let signals = scan::signals(&rsi[f][i], &rsi[f][j]);
            let entries = Entries::new(&signals, frame, tape, grid, costs_lev);
            let end = (window_step[w + 1], window_close[w]);
            let mut ours = Vec::new();
            let mut k = entries.first_at(window_step[w]);
            entries.trades_until(tp, sl, &mut k, entries.first_at(end.0), Some(end), |t| {
                ours.push(t)
            });
            let first = frame.bar_start.partition_point(|&b| b < window_step[w]);
            let last = frame.bar_start.partition_point(|&b| b < end.0);
            let exits = Exits {
                stop_pct: grid.sl[sl],
                targets: vec![(grid.tp[tp], 1.0)],
                breakeven: false,
            };
            let mut theirs: Vec<ClosedTrade> = Vec::new();
            let o = scan::simulate_logged(
                (0, 0, 0),
                &signals,
                Some(&exits),
                &market,
                first.saturating_sub(1)..last,
                costs_lev,
                &mut theirs,
            );
            let name = format!(
                "{}m RSI {}/{} · {} · {lev}x · window {}",
                frame.minutes,
                args.lengths[i],
                args.lengths[j],
                exits.label(),
                labels[w]
            );
            let closed = if o.position != 0 {
                ours.len().saturating_sub(1)
            } else {
                ours.len()
            };
            ensure!(
                closed == theirs.len(),
                "{name}: {} trades closed, the scanner {}",
                closed,
                theirs.len()
            );
            for (a, b) in ours.iter().zip(&theirs) {
                ensure!(
                    tape.time_ms(a.exit_step as usize) == b.exit_ms
                        && signals[a.signal as usize].0 == b.entry_bar
                        && (a.net - b.net).abs() < 1e-3,
                    "{name}: trade closed at {} differs from the scanner's",
                    utc(b.exit_ms)
                );
            }
            let total: f64 = ours.iter().map(|t| f64::from(t.net)).sum();
            let fee = if o.position != 0 {
                costs_lev.taker * qty_lev * window_close[w]
            } else {
                0.0
            };
            ensure!(
                (total - (o.total() - fee)).abs() < 1e-2,
                "{name}: total {total:.4}, the scanner {:.4}",
                o.total() - fee
            );
            checked += 1;
        }
    }
    Ok(checked)
}

/// Spot-check single-TP scale-out profiles against the Entries engine (multi-TP is scanner-only).
fn check_profiles_vs_entries(
    args: &Args,
    profiles: &[ExitProfile],
    frames: &[Frame],
    rsi: &[Vec<Vec<f64>>],
    pairs: &[(usize, usize)],
    tape: &Tape,
    grid: &Grid,
    costs: Costs,
    window_step: &[u32],
    window_close: &[f64],
    labels: &[String],
    tasks: &[(usize, usize)],
    n_windows: usize,
) -> Result<usize> {
    let singles: Vec<(usize, &ExitProfile)> = profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| p.ladder_r.len() == 1)
        .collect();
    ensure!(
        !singles.is_empty(),
        "need at least one single-TP exit profile for the Entries check"
    );
    let n_sl = grid.sl.len();
    let n_lev = grid.lev.len();
    let mut x: u64 = 0x9e3779b97f4a7c15;
    let mut rand = |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % n as u64) as usize
    };
    let mut checked = 0;
    for (f, frame) in frames.iter().enumerate() {
        let closes = CloseRanges::new(&frame.candles.iter().map(|c| c.close).collect::<Vec<_>>());
        let market = Market {
            candles: &frame.candles,
            tape,
            bar_start: &frame.bar_start,
            closes: &closes,
        };
        let groups: Vec<usize> = tasks.iter().filter(|t| t.0 == f).map(|t| t.1).collect();
        if groups.is_empty() || n_windows == 0 {
            continue;
        }
        for _ in 0..args.check {
            let p = groups[rand(groups.len())];
            let (pi, profile) = singles[rand(singles.len())];
            let si = rand(n_sl);
            let li = rand(n_lev);
            let w = rand(n_windows);
            let (i, j) = pairs[p];
            let lev = grid.lev[li];
            let stop = grid.sl[si];
            let tp_pct = profile.ladder_r[0] * stop;
            let mut costs_lev = costs;
            costs_lev.steps *= i64::from(lev);
            let qty_lev = costs_lev.steps as f64 * STEP_BTC;
            let signals = scan::signals(&rsi[f][i], &rsi[f][j]);
            let check_grid = Grid {
                tp: vec![tp_pct],
                sl: vec![stop],
                lev: vec![1],
            };
            let entries = Entries::new(&signals, frame, tape, &check_grid, costs_lev);
            let end = (window_step[w + 1], window_close[w]);
            let mut ours = Vec::new();
            let mut k = entries.first_at(window_step[w]);
            entries.trades_until(0, 0, &mut k, entries.first_at(end.0), Some(end), |t| {
                ours.push(t)
            });
            let first = frame.bar_start.partition_point(|&b| b < window_step[w]);
            let last = frame.bar_start.partition_point(|&b| b < end.0);
            let exits = profile.to_exits(stop);
            let mut theirs: Vec<ClosedTrade> = Vec::new();
            let o = scan::simulate_logged(
                (0, 0, 0),
                &signals,
                Some(&exits),
                &market,
                first.saturating_sub(1)..last,
                costs_lev,
                &mut theirs,
            );
            let name = format!(
                "{}m RSI {}/{} · {} · {lev}x · window {} (profile {pi})",
                frame.minutes,
                args.lengths[i],
                args.lengths[j],
                exits.label(),
                labels[w],
            );
            let closed = if o.position != 0 {
                ours.len().saturating_sub(1)
            } else {
                ours.len()
            };
            ensure!(
                closed == theirs.len(),
                "{name}: {} trades closed, the scanner {}",
                closed,
                theirs.len()
            );
            for (a, b) in ours.iter().zip(&theirs) {
                ensure!(
                    tape.time_ms(a.exit_step as usize) == b.exit_ms
                        && signals[a.signal as usize].0 == b.entry_bar
                        && (a.net - b.net).abs() < 1e-3,
                    "{name}: trade closed at {} differs from the scanner's",
                    utc(b.exit_ms)
                );
            }
            let total: f64 = ours.iter().map(|t| f64::from(t.net)).sum();
            let fee = if o.position != 0 {
                costs_lev.taker * qty_lev * window_close[w]
            } else {
                0.0
            };
            ensure!(
                (total - (o.total() - fee)).abs() < 1e-2,
                "{name}: total {total:.4}, the scanner {:.4}",
                o.total() - fee
            );
            checked += 1;
        }
    }
    Ok(checked)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let config = AppConfig::load(&args.config)?;
    let costs = Costs {
        steps: (args.size_btc * 10_000.0).round() as i64,
        taker: config.venues.okx.taker_fee_bps / 10_000.0,
        maker: config.venues.okx.maker_fee_bps / 10_000.0,
        balance: REPLAY_BALANCE,
    };
    let exit_profiles: Option<Vec<ExitProfile>> = if args.staged {
        let profiles = default_scaleout_profiles();
        for p in &profiles {
            p.validate()?;
        }
        Some(profiles)
    } else {
        None
    };
    // Staged stage 2: profile index × SL × lev (tp slot holds 1-based profile ids for Grid layout).
    let fine_grid = if let Some(ref profiles) = exit_profiles {
        Grid {
            tp: (1..=profiles.len()).map(|i| i as f64).collect(),
            sl: args.sl.clone(),
            lev: args.leverage.clone(),
        }
    } else {
        Grid {
            tp: args.tp.clone(),
            sl: args.sl.clone(),
            lev: args.leverage.clone(),
        }
    };
    let coarse_grid = Grid {
        tp: args.coarse_tp.clone(),
        sl: args.coarse_sl.clone(),
        lev: vec![1],
    };
    let pairs: Vec<(usize, usize)> = (0..args.lengths.len())
        .flat_map(|i| (i + 1..args.lengths.len()).map(move |j| (i, j)))
        .collect();
    let n_groups_all = args.timeframes.len() * pairs.len();

    let started = Instant::now();
    let mut shown = 0;
    let (minutes, tape) = Tape::load(
        &args.data,
        60_000,
        ms_day(args.start),
        ms_day(args.end),
        |done, total| {
            if done * 10 / total.max(1) > shown || done == total {
                shown = done * 10 / total.max(1);
                println!("Reading every OKX trade: archive {done} of {total}");
            }
        },
    )?;
    println!(
        "{} one-minute candles from {} to {} (exclusive) in {:.0}s; {} price changes for TP/SL",
        minutes.len(),
        args.start,
        args.end,
        started.elapsed().as_secs_f64(),
        tape.price.len()
    );
    ensure!(!tape.price.is_empty(), "no trades in the period");
    let frames: Vec<Frame> = args
        .timeframes
        .iter()
        .map(|&m| Frame::from_minutes(&minutes, &tape.bar_start, m))
        .collect();
    let rsi: Vec<Vec<Vec<f64>>> = frames
        .iter()
        .map(|f| rsi_series(&args.lengths, &f.candles))
        .collect();

    let window_hours = args.window_hours();
    let hour_mode = args.hours.is_some();
    let period_end = args.end.and_hms_opt(0, 0, 0).unwrap();
    let step = Duration::hours(window_hours as i64);
    let mut window_starts: Vec<NaiveDateTime> = Vec::new();
    let mut cursor = args.start.and_hms_opt(0, 0, 0).unwrap();
    while cursor < period_end {
        window_starts.push(cursor);
        let Some(next) = cursor.checked_add_signed(step) else {
            break;
        };
        if next >= period_end {
            break;
        }
        if next <= cursor {
            bail!("--hours/{window_hours} did not advance the window clock");
        }
        cursor = next;
    }
    ensure!(!window_starts.is_empty(), "no outer windows in the period");
    let window_step: Vec<u32> = window_starts
        .iter()
        .map(|d| minutes.partition_point(|c| c.time_ms < ms_dt(*d)))
        .map(|i| {
            tape.bar_start
                .get(i)
                .copied()
                .unwrap_or(tape.price.len() as u32)
        })
        .chain(std::iter::once(tape.price.len() as u32))
        .collect();
    let n_windows = window_starts.len();
    for w in 0..n_windows {
        ensure!(
            window_step[w] < window_step[w + 1],
            "window {} has no trades",
            window_label(window_starts[w], hour_mode)
        );
    }
    let price_at = |step: u32| f64::from(tape.price[step as usize]) / 10.0;
    let window_close: Vec<f64> = (0..n_windows)
        .map(|w| price_at(window_step[w + 1] - 1))
        .collect();
    let window_end_dt = |w: usize| window_starts.get(w + 1).copied().unwrap_or(period_end);
    let labels: Vec<String> = window_starts
        .iter()
        .map(|d| window_label(*d, hour_mode))
        .collect();

    let dir_name = if args.staged {
        format!("{}_{}_{}_staged", args.start, args.end, args.dir_suffix())
    } else {
        format!("{}_{}_{}", args.start, args.end, args.dir_suffix())
    };
    let dir = args.out.join(dir_name);
    fs::create_dir_all(&dir)?;
    let _ = fs::remove_file(dir.join("meta.json"));

    let all_tasks: Vec<(usize, usize)> = (0..frames.len())
        .flat_map(|f| {
            (0..pairs.len())
                .step_by(args.every_pair)
                .map(move |p| (f, p))
        })
        .collect();

    let days_meta = match args.hours {
        None => args.days,
        Some(h) => h.div_ceil(24).max(1),
    };

    let (survivor_groups, fine_n_combos, trades_seen, check_tasks) = if args.staged {
        let profiles = exit_profiles.as_ref().expect("staged has exit profiles");
        let profile_labels: Vec<&str> = profiles.iter().map(|p| p.label.as_str()).collect();
        let settings_text = format!(
            "staged=3 scaleout=1 {} {} hours={} days={} {:?} {:?} coarse_tp={:?} coarse_sl={:?} profiles={:?} sl={:?} lev={:?} finalists={} survivor_cap={} min_profit={:?} {} {} {} {} {}",
            args.start,
            args.end,
            window_hours,
            args.days,
            args.timeframes,
            args.lengths,
            coarse_grid.tp,
            coarse_grid.sl,
            profile_labels,
            fine_grid.sl,
            fine_grid.lev,
            args.finalists,
            args.survivor_cap,
            args.min_profit,
            args.every_pair,
            args.size_btc,
            costs.taker,
            costs.maker,
            tape.price.len()
        );
        let settings_path = dir.join("scan_settings.txt");
        let stage1_dir = dir.join("stage1");
        fs::create_dir_all(&stage1_dir)?;
        let stage1_paths: Vec<PathBuf> = labels
            .iter()
            .map(|d| stage1_dir.join(format!("window_{d}.bin")))
            .collect();
        let stage2_paths: Vec<PathBuf> = labels
            .iter()
            .map(|d| dir.join(format!("window_{d}.bin")))
            .collect();
        let done1_path = dir.join("done_groups_stage1.txt");
        let done2_path = dir.join("done_groups_stage2.txt");
        let survivors_path = dir.join("survivors.json");

        let n_coarse = coarse_grid.len();
        let n_combos1 = n_groups_all * n_coarse;
        ensure!(n_combos1 <= u32::MAX as usize, "too many stage-1 combinations");

        let resume1 = fs::read_to_string(&settings_path).is_ok_and(|s| s == settings_text)
            && stage1_paths
                .iter()
                .all(|p| fs::metadata(p).is_ok_and(|m| m.len() == (n_combos1 * RECORD) as u64));
        if !resume1 {
            let _ = fs::remove_file(&done1_path);
            let _ = fs::remove_file(&done2_path);
            let _ = fs::remove_file(&survivors_path);
            for p in &stage2_paths {
                let _ = fs::remove_file(p);
            }
            fs::write(&settings_path, &settings_text)?;
        }
        let finished1 = if resume1 {
            read_done(&done1_path)
        } else {
            HashSet::new()
        };

        println!(
            "Staged scan: stage 1 coarse {}×{} TP/SL @ 1x, then top {} groups/window (cap {}), stage 2 {} profiles × {} SL × {} lev",
            coarse_grid.tp.len(),
            coarse_grid.sl.len(),
            args.finalists,
            args.survivor_cap,
            profiles.len(),
            fine_grid.sl.len(),
            fine_grid.lev.len(),
        );
        println!(
            "Stage 2 axis: exit profiles {:?} on ≤{} survivors (scan_version 3)",
            profile_labels,
            args.survivor_cap,
        );

        let files1 = open_window_files(&stage1_paths, n_combos1, resume1)?;
        let map1 = GroupMap::full(pairs.len());
        let trades1 = replay_groups(
            "Stage 1 (coarse)",
            &all_tasks,
            &finished1,
            &pairs,
            &args.lengths,
            &frames,
            &rsi,
            &tape,
            &coarse_grid,
            costs,
            &window_step,
            &window_close,
            &files1,
            &map1,
            n_groups_all,
            &done1_path,
        )?;
        drop(files1);

        let survivors = if resume1 && survivors_path.is_file() {
            let v: Vec<usize> = serde_json::from_str(&fs::read_to_string(&survivors_path)?)?;
            println!(
                "Stage 1: loaded {} survivors from {}",
                v.len(),
                survivors_path.display()
            );
            v
        } else {
            let v = select_survivors(
                &stage1_paths,
                n_groups_all,
                n_coarse,
                args.finalists,
                args.survivor_cap,
                args.min_profit,
            )?;
            fs::write(&survivors_path, serde_json::to_string_pretty(&v)?)?;
            println!(
                "Stage 1: kept {} survivor groups (finalists={}, cap={}) → {}",
                v.len(),
                args.finalists,
                args.survivor_cap,
                survivors_path.display()
            );
            v
        };

        let n_fine = fine_grid.len();
        let n_combos2 = survivors.len() * n_fine;
        ensure!(n_combos2 <= u32::MAX as usize, "too many stage-2 combinations");
        let stage2_tasks: Vec<(usize, usize)> = survivors
            .iter()
            .map(|&g| (g / pairs.len(), g % pairs.len()))
            .collect();

        let resume2 = resume1
            && survivors_path.is_file()
            && stage2_paths
                .iter()
                .all(|p| fs::metadata(p).is_ok_and(|m| m.len() == (n_combos2 * RECORD) as u64));
        if !resume2 {
            let _ = fs::remove_file(&done2_path);
        }
        let finished2 = if resume2 {
            read_done(&done2_path)
        } else {
            HashSet::new()
        };

        let files2 = open_window_files(&stage2_paths, n_combos2, resume2)?;
        let map2 = GroupMap::survivors(pairs.len(), survivors.clone());
        let trades2 = replay_groups_profiles(
            "Stage 2 (scale-out)",
            &stage2_tasks,
            &finished2,
            &pairs,
            &args.lengths,
            &frames,
            &rsi,
            &tape,
            profiles,
            &fine_grid.sl,
            &fine_grid.lev,
            costs,
            &window_step,
            &window_close,
            &files2,
            &map2,
            survivors.len(),
            &done2_path,
        )?;
        drop(files2);

        // Prefer stage-2 trade counts for the summary.
        let _ = trades1;
        (
            Some(survivors),
            n_combos2,
            trades2,
            stage2_tasks,
        )
    } else {
        let grid = &fine_grid;
        let n_settings = grid.len();
        let n_combos = n_groups_all * n_settings;
        ensure!(n_combos <= u32::MAX as usize, "too many combinations for 32-bit ids");
        let settings_text = format!(
            "{} {} hours={} days={} {:?} {:?} {:?} {:?} {:?} {} {} {} {} {}",
            args.start,
            args.end,
            window_hours,
            args.days,
            args.timeframes,
            args.lengths,
            grid.tp,
            grid.sl,
            grid.lev,
            args.every_pair,
            args.size_btc,
            costs.taker,
            costs.maker,
            tape.price.len()
        );
        let (settings_path, done_path) = (dir.join("scan_settings.txt"), dir.join("done_groups.txt"));
        let paths: Vec<PathBuf> = labels
            .iter()
            .map(|d| dir.join(format!("window_{d}.bin")))
            .collect();
        let resume = fs::read_to_string(&settings_path).is_ok_and(|s| s == settings_text)
            && paths
                .iter()
                .all(|p| fs::metadata(p).is_ok_and(|m| m.len() == (n_combos * RECORD) as u64));
        let finished = if resume {
            read_done(&done_path)
        } else {
            let _ = fs::remove_file(&done_path);
            fs::write(&settings_path, &settings_text)?;
            HashSet::new()
        };
        let files = open_window_files(&paths, n_combos, resume)?;
        let map = GroupMap::full(pairs.len());
        let trades = replay_groups(
            "Full grid",
            &all_tasks,
            &finished,
            &pairs,
            &args.lengths,
            &frames,
            &rsi,
            &tape,
            grid,
            costs,
            &window_step,
            &window_close,
            &files,
            &map,
            n_groups_all,
            &done_path,
        )?;
        (None, n_combos, trades, all_tasks.clone())
    };

    let n_settings_out = fine_grid.len();
    let n_file_groups = survivor_groups
        .as_ref()
        .map(|s| s.len())
        .unwrap_or(n_groups_all);
    for (f, frame) in frames.iter().enumerate() {
        let trades: u64 = trades_seen.iter().map(|t| t[f]).sum();
        let groups = check_tasks.iter().filter(|t| t.0 == f).count().max(1) as u64;
        println!(
            "  {:>2}m: {:.1} trades per combination per window on average",
            frame.minutes,
            trades as f64 / (groups * n_settings_out as u64 * n_windows as u64) as f64
        );
    }

    let checked = if let Some(ref profiles) = exit_profiles {
        // Stage-2 already uses the trade-by-trade scanner; spot-check single-TP profiles vs Entries.
        check_profiles_vs_entries(
            &args,
            profiles,
            &frames,
            &rsi,
            &pairs,
            &tape,
            &fine_grid,
            costs,
            &window_step,
            &window_close,
            &labels,
            &check_tasks,
            n_windows,
        )?
    } else {
        check_against_scanner(
            &args,
            &frames,
            &rsi,
            &pairs,
            &tape,
            &fine_grid,
            costs,
            &window_step,
            &window_close,
            &labels,
            &check_tasks,
            n_windows,
        )?
    };
    println!("{checked} combinations replayed again trade by trade in random windows: identical");

    let scan_version = if args.staged { 3 } else { 1 };
    let mut meta = json!({
        "scan_version": scan_version,
        "staged": args.staged,
        "instrument": okx_history::INSTRUMENT,
        "start": args.start.to_string(),
        "end": args.end.to_string(),
        "days": days_meta,
        "hours": window_hours,
        "timeframes": args.timeframes,
        "lengths": args.lengths,
        "pairs": pairs.len(),
        "tp": fine_grid.tp,
        "sl": fine_grid.sl,
        "leverage": fine_grid.lev,
        "settings": n_settings_out,
        "combinations": fine_n_combos,
        "every_pair": args.every_pair,
        "windows": (0..n_windows).map(|w| {
            let mut wj = json!({
                "start": window_start_str(window_starts[w], hour_mode),
                "end": window_start_str(window_end_dt(w), hour_mode),
                "file": format!("window_{}.bin", labels[w]),
                "first_price": price_at(window_step[w]),
                "last_price": window_close[w],
            });
            if args.staged {
                wj["stage1_file"] = json!(format!("stage1/window_{}.bin", labels[w]));
            }
            wj
        }).collect::<Vec<_>>(),
        "record": if args.staged {
            "scan_version 3 (staged scale-out): window_*.bin holds profile×SL×lev results for survivor groups only; combination = survivor_index * settings + ((profile * sl_count + sl) * lev_count + lev); exit_profiles[profile] is the scale-out template; survivor_groups[survivor_index] = timeframe * pairs + pair. stage1/window_*.bin is the coarse TP×SL grid at leverage 1. Same 20-byte record as v1."
        } else {
            "per combination, little-endian: profit after fees (f32 USDT), largest drop (f32 USDT), biggest losing trade (f32 USDT, <= 0), average hours held (f32), trades (u16), winning trades (u16); combination = (timeframe * pairs + pair) * settings + ((tp * sl_count + sl) * lev_count + lev); pair = (i, j) with i < j over lengths; size = size_btc × leverage"
        },
        "size_btc": args.size_btc,
        "taker_fee_bps": config.venues.okx.taker_fee_bps,
        "maker_fee_bps": config.venues.okx.maker_fee_bps,
        "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
    });
    if args.staged {
        meta["coarse_tp"] = json!(coarse_grid.tp);
        meta["coarse_sl"] = json!(coarse_grid.sl);
        meta["coarse_leverage"] = json!([1]);
        meta["coarse_settings"] = json!(coarse_grid.len());
        meta["finalists_per_window"] = json!(args.finalists);
        meta["survivor_cap"] = json!(args.survivor_cap);
        meta["min_profit"] = json!(args.min_profit);
        meta["survivor_groups"] = json!(survivor_groups.as_ref().unwrap());
        meta["survivor_count"] = json!(n_file_groups);
        meta["groups_scanned_stage1"] = json!(n_groups_all);
        meta["exit_profiles"] = json!(exit_profiles.as_ref().unwrap());
        meta["stage2_axis"] = json!("profile_sl_lev");
    }
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    println!("Results: {}", dir.display());
    Ok(())
}
