use std::{
    fs::{self, File},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use dynamic_trader::{
    config::AppConfig,
    exits::Exits,
    grid::{parse_lengths, rsi_series},
    okx_history,
    scan::{self, ClosedTrade, Costs, Market},
    tape::{CloseRanges, Tape},
    tpsl_grid::{Entries, Frame, Grid, SIGNAL_BLOCK, parse_leverage, parse_range},
};
use rayon::prelude::*;
use serde_json::json;

const USAGE: &str = "\
Stage 1 of the full search: every RSI pair, on every timeframe, with every take-profit ×
stop-loss × leverage, replayed on every OKX BTC-USDT-SWAP trade, and scored month by month.

A trade is one position from its entry until it is closed by its take-profit, its stop-loss or
the next opposite signal, with every fee. Leverage scales position size (qty = size_btc ×
leverage). Each combination is replayed once over the whole period from the first candle, and
its trades are grouped by the calendar month (UTC) they close in. For each month the file
month_YYYY-MM.bin gets, per combination, the profit after fees, the largest drop of that profit
counted trade by trade, and the number of trades; all.bin the same over the whole period.
rank_full_scan then ranks them.

Usage: cargo run --release --bin full_scan -- [options]

  --start <YYYY-MM-DD>      First UTC day, inclusive (default: 2025-10-01)
  --end <YYYY-MM-DD>        Last UTC day, exclusive (default: 2026-10-01)
  --timeframes <list>       Candle sizes in minutes (default: 1,3,5,15,60)
  --lengths <list>          RSI lengths (default: 1-50,60-400/10)
  --tp <from-to/step>       Take-profit distances in % (default: 0.01-1/0.01)
  --sl <from-to/step>       Stop-loss distances in % (default: 0.01-1/0.01)
  --leverage <from-to/step> Leverage multipliers (default: 1-10/1)
  --every-pair <n>          Only every n-th RSI pair, to measure the time (default: 1 = all)
  --check <n>               Combinations per timeframe replayed again by the trade-by-trade
                            scanner, which must give the same trades (default: 20)
  --size-btc <btc>          Base position size in BTC before leverage (default: 0.005)
  --config <path>           Fee config (default: config/testnet.toml)
  --data <dir>              Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --out <dir>               Where to write the results (default: data/fullscan)";

/// Balance of the replays: large enough that no order is ever refused, so every trade's result
/// depends only on the prices.
const REPLAY_BALANCE: f64 = 1e9;

/// One combination over one month: profit after fees, largest drop, trades.
const RECORD: usize = 12;

struct Args {
    start: NaiveDate,
    end: NaiveDate,
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    tp: Vec<f64>,
    sl: Vec<f64>,
    leverage: Vec<u32>,
    every_pair: usize,
    check: usize,
    size_btc: f64,
    config: PathBuf,
    data: PathBuf,
    out: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        start: "2025-10-01".parse()?,
        end: "2026-10-01".parse()?,
        timeframes: vec![1, 3, 5, 15, 60],
        lengths: parse_lengths("1-50,60-400/10")?,
        tp: parse_range("0.01-1/0.01")?,
        sl: parse_range("0.01-1/0.01")?,
        leverage: parse_leverage("1-10/1")?,
        every_pair: 1,
        check: 20,
        size_btc: 0.005,
        config: "config/testnet.toml".into(),
        data: "data/okx/BTC-USDT-SWAP/archives".into(),
        out: "data/fullscan".into(),
    };
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        let value = raw
            .next()
            .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
        let bad = || format!("bad value for {flag}: {value}");
        match flag.as_str() {
            "--start" => args.start = value.parse().with_context(bad)?,
            "--end" => args.end = value.parse().with_context(bad)?,
            "--timeframes" => {
                args.timeframes = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?
            }
            "--lengths" => args.lengths = parse_lengths(&value).with_context(bad)?,
            "--tp" => args.tp = parse_range(&value).with_context(bad)?,
            "--sl" => args.sl = parse_range(&value).with_context(bad)?,
            "--leverage" => args.leverage = parse_leverage(&value).with_context(bad)?,
            "--every-pair" => args.every_pair = value.parse().with_context(bad)?,
            "--check" => args.check = value.parse().with_context(bad)?,
            "--size-btc" => args.size_btc = value.parse().with_context(bad)?,
            "--config" => args.config = value.into(),
            "--data" => args.data = value.into(),
            "--out" => args.out = value.into(),
            "--bucket" => {} // handled by bin/scan; ignored here
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(args.start < args.end, "--start must be before --end");
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
    let steps = args.size_btc * 10_000.0;
    ensure!(
        args.size_btc >= 0.0001 && (steps.round() - steps).abs() < 1e-6,
        "--size-btc must use OKX's 0.0001 BTC step"
    );
    Ok(args)
}

fn ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn utc(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
    )
}

/// Profit, largest drop and trades of one combination over one month, built trade by trade.
#[derive(Clone, Copy, Default)]
struct Tally {
    sum: f64,
    peak: f64,
    drop: f64,
    trades: u32,
}

impl Tally {
    fn add(&mut self, net: f64) {
        self.sum += net;
        self.peak = self.peak.max(self.sum);
        self.drop = self.drop.max(self.peak - self.sum);
        self.trades += 1;
    }

    fn scale_leverage(self, leverage: u32) -> Tally {
        let l = f64::from(leverage);
        Tally {
            sum: self.sum * l,
            peak: self.peak * l,
            drop: self.drop * l,
            trades: self.trades,
        }
    }

    fn bytes(&self) -> [u8; RECORD] {
        let mut b = [0; RECORD];
        b[0..4].copy_from_slice(&(self.sum as f32).to_le_bytes());
        b[4..8].copy_from_slice(&(self.drop as f32).to_le_bytes());
        b[8..12].copy_from_slice(&self.trades.to_le_bytes());
        b
    }
}

pub fn run() -> Result<()> {
    let args = parse_args()?;
    let config = AppConfig::load(&args.config)?;
    let costs = Costs {
        steps: (args.size_btc * 10_000.0).round() as i64,
        taker: config.venues.okx.taker_fee_bps / 10_000.0,
        maker: config.venues.okx.maker_fee_bps / 10_000.0,
        balance: REPLAY_BALANCE,
    };
    let grid = Grid {
        tp: args.tp.clone(),
        sl: args.sl.clone(),
        lev: args.leverage.clone(),
    };
    let pairs: Vec<(usize, usize)> = (0..args.lengths.len())
        .flat_map(|i| (i + 1..args.lengths.len()).map(move |j| (i, j)))
        .collect();
    let n_settings = grid.len();
    let n_combos = args.timeframes.len() * pairs.len() * n_settings;
    ensure!(
        n_combos <= u32::MAX as usize,
        "too many combinations for 32-bit ids"
    );

    let started = Instant::now();
    let mut shown = 0;
    let (minutes, tape) = Tape::load(
        &args.data,
        60_000,
        ms(args.start),
        ms(args.end),
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
    let frames: Vec<Frame> = args
        .timeframes
        .iter()
        .map(|&m| Frame::from_minutes(&minutes, &tape.bar_start, m))
        .collect();
    let rsi: Vec<Vec<Vec<f64>>> = frames
        .iter()
        .map(|f| rsi_series(&args.lengths, &f.candles))
        .collect();

    // Calendar months, as the first tape step at or after each one's start.
    let mut month_days = vec![args.start.with_day0(0).unwrap()];
    while let Some(next) = month_days
        .last()
        .and_then(|d| d.checked_add_months(Months::new(1)))
        .filter(|d| *d < args.end)
    {
        month_days.push(next);
    }
    let month_step: Vec<u32> = month_days
        .iter()
        .map(|d| minutes.partition_point(|c| c.time_ms < ms(*d)))
        .map(|i| {
            tape.bar_start
                .get(i)
                .copied()
                .unwrap_or(tape.price.len() as u32)
        })
        .collect();
    let n_months = month_days.len();
    let month_name = |m: usize| month_days[m].format("%Y-%m").to_string();

    let dir = args.out.join(format!("{}_{}", args.start, args.end));
    fs::create_dir_all(&dir)?;
    let files: Vec<File> = (0..n_months)
        .map(|m| dir.join(format!("month_{}.bin", month_name(m))))
        .chain(std::iter::once(dir.join("all.bin")))
        .map(|path| {
            let file = File::create(&path).with_context(|| path.display().to_string())?;
            file.set_len((n_combos * RECORD) as u64)?;
            Ok(file)
        })
        .collect::<Result<_>>()?;

    let tasks: Vec<(usize, usize)> = (0..frames.len())
        .flat_map(|f| {
            (0..pairs.len())
                .step_by(args.every_pair)
                .map(move |p| (f, p))
        })
        .collect();
    println!(
        "{n_combos} combinations ({} RSI pairs × {} timeframes × {} TP × {} SL × {} lev); replaying {} of the {} RSI pair × timeframe groups, {} months",
        pairs.len(),
        frames.len(),
        grid.tp.len(),
        grid.sl.len(),
        grid.lev.len(),
        tasks.len(),
        pairs.len() * frames.len(),
        n_months
    );

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    // Rayon pool: Entries::new steals idle workers; tasks pulled one-at-a-time for balance.
    let counts: Vec<(Vec<u64>, Vec<u64>)> = (0..threads)
        .into_par_iter()
        .map(|_| -> Result<(Vec<u64>, Vec<u64>)> {
            let mut signals_seen = vec![0u64; frames.len()];
            let mut trades_seen = vec![0u64; frames.len()];
            let n_tpsl = grid.tpsl_len();
            let n_lev = grid.lev.len();
            let n_sl = grid.sl.len();
            let mut out = vec![Tally::default(); n_tpsl * (n_months + 1)];
            let mut position = vec![(0usize, 0usize); n_tpsl];
            let mut bytes = vec![0u8; n_settings * RECORD];
            loop {
                let t = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(f, p)) = tasks.get(t) else { break };
                let (i, j) = pairs[p];
                let signals = scan::signals(&rsi[f][i], &rsi[f][j]);
                let entries = Entries::new(&signals, &frames[f], &tape, &grid, costs);
                signals_seen[f] += signals.len() as u64;
                out.fill(Tally::default());
                // Every TP/SL through one block of signals at a time: (next signal, month).
                position.fill((0, 0));
                for first in (0..entries.len()).step_by(SIGNAL_BLOCK) {
                    for (tpsl, (k, m)) in position.iter_mut().enumerate() {
                        let (tp, sl) = (tpsl / n_sl, tpsl % n_sl);
                        let tallies =
                            &mut out[tpsl * (n_months + 1)..(tpsl + 1) * (n_months + 1)];
                        entries.trades_from(tp, sl, k, first + SIGNAL_BLOCK, |trade| {
                            while *m + 1 < n_months && trade.exit_step >= month_step[*m + 1] {
                                *m += 1;
                            }
                            let net = f64::from(trade.net);
                            tallies[*m].add(net);
                            tallies[n_months].add(net);
                        });
                    }
                }
                trades_seen[f] += (0..n_tpsl)
                    .map(|tpsl| {
                        u64::from(out[tpsl * (n_months + 1) + n_months].trades) * n_lev as u64
                    })
                    .sum::<u64>();
                let first = ((f * pairs.len() + p) * n_settings * RECORD) as u64;
                for (m, file) in files.iter().enumerate() {
                    for tpsl in 0..n_tpsl {
                        let base = out[tpsl * (n_months + 1) + m];
                        for (li, &lev) in grid.lev.iter().enumerate() {
                            let s = tpsl * n_lev + li;
                            bytes[s * RECORD..(s + 1) * RECORD]
                                .copy_from_slice(&base.scale_leverage(lev).bytes());
                        }
                    }
                    file.write_all_at(&bytes, first)?;
                }
                let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                if d % 50 == 0 || d == tasks.len() {
                    let secs = started.elapsed().as_secs_f64();
                    println!(
                        "Replayed group {d} of {} ({}m RSI {}/{}), {:.0}s so far, about {:.0} min left",
                        tasks.len(),
                        frames[f].minutes,
                        args.lengths[i],
                        args.lengths[j],
                        secs,
                        secs / d as f64 * (tasks.len() - d) as f64 / 60.0
                    );
                }
            }
            Ok((signals_seen, trades_seen))
        })
        .collect::<Result<_>>()?;
    let secs = started.elapsed().as_secs_f64();
    let all_groups = pairs.len() * frames.len();
    println!(
        "Replayed {} groups in {:.0}s; for all {all_groups} groups that is about {:.1} hours",
        tasks.len(),
        secs,
        secs * all_groups as f64 / tasks.len() as f64 / 3600.0
    );
    for (f, frame) in frames.iter().enumerate() {
        let (signals, trades): (u64, u64) = counts
            .iter()
            .map(|(s, t)| (s[f], t[f]))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        let groups = tasks.iter().filter(|t| t.0 == f).count().max(1) as u64;
        println!(
            "  {:>2}m: {} candles, {} signals per RSI pair, {} trades per combination on average",
            frame.minutes,
            frame.candles.len(),
            signals / groups,
            trades / groups / n_settings as u64
        );
    }

    // The trade-by-trade scanner must give exactly the same trades for a sample.
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
            tape: &tape,
            bar_start: &frame.bar_start,
            closes: &closes,
        };
        for _ in 0..args.check {
            let p = tasks
                .iter()
                .filter(|t| t.0 == f)
                .nth(rand(tasks.len() / frames.len()))
                .map_or(0, |t| t.1);
            let s = rand(n_settings);
            let (i, j) = pairs[p];
            let (tp, sl, li) = grid.split(s);
            let lev = grid.lev[li];
            let mut costs_lev = costs;
            costs_lev.steps *= i64::from(lev);
            let signals = scan::signals(&rsi[f][i], &rsi[f][j]);
            let entries = Entries::new(&signals, frame, &tape, &grid, costs_lev);
            let mut ours = Vec::new();
            entries.trades(tp, sl, |t| ours.push(t));
            let exits = Exits {
                stop_pct: grid.sl[sl],
                targets: vec![(grid.tp[tp], 1.0)],
                breakeven: false,
            };
            let mut theirs: Vec<ClosedTrade> = Vec::new();
            scan::simulate_logged(
                (0, 0, 0),
                &signals,
                Some(&exits),
                &market,
                0..frame.candles.len(),
                costs_lev,
                &mut theirs,
            );
            let name = format!(
                "{}m RSI {}/{} · {} · {lev}x",
                frame.minutes,
                args.lengths[i],
                args.lengths[j],
                exits.label()
            );
            ensure!(
                ours.len() == theirs.len(),
                "{name}: {} trades, the scanner {}",
                ours.len(),
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
            checked += 1;
        }
    }
    println!("{checked} combinations replayed again trade by trade: identical");

    let meta = json!({
        "instrument": okx_history::INSTRUMENT,
        "start": args.start.to_string(),
        "end": args.end.to_string(),
        "timeframes": args.timeframes,
        "lengths": args.lengths,
        "pairs": pairs.len(),
        "tp": grid.tp,
        "sl": grid.sl,
        "leverage": grid.lev,
        "settings": n_settings,
        "combinations": n_combos,
        "every_pair": args.every_pair,
        "months": (0..n_months).map(month_name).collect::<Vec<_>>(),
        "record": "per combination, little-endian: profit after fees (f32 USDT), largest drop (f32 USDT), trades (u32); combination = (timeframe * pairs + pair) * settings + ((tp * sl_count + sl) * lev_count + lev); pair = (i, j) with i < j over lengths; size = size_btc × leverage",
        "size_btc": args.size_btc,
        "taker_fee_bps": config.venues.okx.taker_fee_bps,
        "maker_fee_bps": config.venues.okx.maker_fee_bps,
        "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
    });
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    println!("Results: {}", dir.display());
    Ok(())
}
