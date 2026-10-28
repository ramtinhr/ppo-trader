use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Months, NaiveDate, Utc};
use dynamic_trader::{
    backtest::{Fill, Run, run_backtest},
    config::AppConfig,
    exits::Exits,
    filters::{Filter, FilteredCross},
    grid::{Indicators, exit_settings, exits_label, parse_groups, parse_lengths, parse_numbers},
    okx_history::{self, Candle},
    rsi_cross::RsiCrossParams,
    scan::{self, ClosedTrade, Costs, Market, Record, STEP_BTC},
    strategy::BarSignals,
    tape::{CloseRanges, Tape},
};
use serde_json::{Value, json};

const USAGE: &str = "\
Walk-forward test of the RSI cross strategy with windows counted in trades, on every OKX
BTC-USDT-SWAP trade.

A trade is one position from its entry until it is fully closed (by take-profits, the stop or
the next signal), with every fee. A window pair is a training window of N trades and a test
window of M trades, for every N in --train-trades and every M from N/3 (rounded down) to 1:
100/33, 100/32, ..., 100/1, 90/30, ..., 10/1.

1. Every combination (RSI pair x take-profit / stop-loss setting) is replayed once over the whole
   period, from the first candle, so its RSI is always warmed up.
2. Every window pair runs its own walk-forward from --first-pick: each combination's last N
   trades closed by then are scored (profit after fees divided by the largest drop of that
   profit, counted trade by trade, at least 1% of the balance); the highest score is traded for
   its next M trades entered at or after the pick; when the last one closes, it picks again.
   Combinations with fewer than N closed trades are skipped.
3. From --first-choice, the window pairs themselves are chosen the same way: each one's own
   last N trades are scored, the best pair is followed for its next M trades (whatever its own
   walk-forward trades then), and the choice is made again when they close, until the data runs
   out. Nothing after a pick or a choice is used to make it.
4. The chosen trades chained together are what a bot doing this would have made. The account
   starts with --balance-usdt and stops if it falls to zero. Every choice is compared with
   buying the same BTC when it starts and selling when it ends, both with the taker fee. The
   last combination traded is rerun through NautilusTrader over the whole period to confirm
   every fill.

Usage: cargo run --release --bin walk_forward_trades -- [options]

  --start <YYYY-MM-DD>       First UTC day, inclusive (default: 2025-10-01)
  --end <YYYY-MM-DD>         Last UTC day, exclusive (default: 2026-10-01)
  --timeframe <min>          Bar size in minutes (default: 60)
  --train-trades <list>      Training window sizes in trades; each is tested with every test
                             size from a third of it down to 1 (default: 100,90,...,10)
  --first-pick <YYYY-MM-DD>  First pick of every window pair (default: 3 months after --start)
  --first-choice <YYYY-MM-DD> First choice of a window pair (default: 6 months after --start)
  --lengths <list>           RSI lengths (default: 1-50,60-400/10)
  --stops <list>             Stop-loss distances in % (default: 0.5,1,1.5,2,3,5; `none` for no TP/SL)
  --ladders <list>           Take-profit ladders in R (default: 1,2,3,1/2,1/2/3,2/4/6)
  --shares <list>            Share per take-profit in % (default: 100,50/50,50/25/25)
  --breakeven <mode>         yes, no or both (default: both)
  --verify <n>               1 to rerun the last combination traded through NautilusTrader
                             (default: 1, 0 to skip)
  --size-btc <btc>           Position size in BTC (default: 0.005)
  --balance-usdt <usdt>      Starting balance of the chained account (default: 500)
  --config <path>            Fee config (default: config/testnet.toml)
  --data <dir>               Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --out <dir>                Where to write the report (default: data/walkforward)";

/// Balance of the whole-period replays: large enough that no order is ever refused, so every
/// trade's result depends only on the prices. The chained account checks the real balance.
const REPLAY_BALANCE: f64 = 1e9;

struct Args {
    start: NaiveDate,
    end: NaiveDate,
    minutes: usize,
    train_trades: Vec<usize>,
    first_pick: Option<NaiveDate>,
    first_choice: Option<NaiveDate>,
    lengths: Vec<usize>,
    stops: Vec<f64>,
    ladders: Vec<Vec<f64>>,
    shares: Vec<Vec<f64>>,
    breakeven: Vec<bool>,
    verify: usize,
    size_btc: f64,
    balance_usdt: f64,
    config: PathBuf,
    data: PathBuf,
    out: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        start: "2025-10-01".parse()?,
        end: "2026-10-01".parse()?,
        minutes: 60,
        train_trades: (1..=10).rev().map(|k| k * 10).collect(),
        first_pick: None,
        first_choice: None,
        lengths: parse_lengths("1-50,60-400/10")?,
        stops: parse_numbers("0.5,1,1.5,2,3,5")?,
        ladders: parse_groups("1,2,3,1/2,1/2/3,2/4/6")?,
        shares: parse_groups("100,50/50,50/25/25")?,
        breakeven: vec![false, true],
        verify: 1,
        size_btc: 0.005,
        balance_usdt: 500.0,
        config: "config/testnet.toml".into(),
        data: "data/okx/BTC-USDT-SWAP/archives".into(),
        out: "data/walkforward".into(),
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
            "--timeframe" => args.minutes = value.parse().with_context(bad)?,
            "--train-trades" => {
                args.train_trades = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?
            }
            "--first-pick" => args.first_pick = Some(value.parse().with_context(bad)?),
            "--first-choice" => args.first_choice = Some(value.parse().with_context(bad)?),
            "--lengths" => args.lengths = parse_lengths(&value).with_context(bad)?,
            "--stops" => args.stops = parse_numbers(&value).with_context(bad)?,
            "--ladders" => args.ladders = parse_groups(&value).with_context(bad)?,
            "--shares" => args.shares = parse_groups(&value).with_context(bad)?,
            "--breakeven" => {
                args.breakeven = match value.as_str() {
                    "yes" => vec![true],
                    "no" => vec![false],
                    "both" => vec![false, true],
                    _ => bail!(bad()),
                }
            }
            "--verify" => args.verify = value.parse().with_context(bad)?,
            "--size-btc" => args.size_btc = value.parse().with_context(bad)?,
            "--balance-usdt" => args.balance_usdt = value.parse().with_context(bad)?,
            "--config" => args.config = value.into(),
            "--data" => args.data = value.into(),
            "--out" => args.out = value.into(),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(args.start < args.end, "--start must be before --end");
    ensure!(
        args.minutes > 0 && 1440 % args.minutes == 0,
        "--timeframe must divide a day into whole bars"
    );
    ensure!(
        args.lengths.len() >= 2,
        "--lengths needs at least two lengths"
    );
    ensure!(
        !args.train_trades.is_empty() && args.train_trades.iter().all(|n| *n >= 3),
        "--train-trades must be at least 3"
    );
    let first = *args.first_pick.get_or_insert(args.start + Months::new(3));
    ensure!(
        args.start < first && first < args.end,
        "--first-pick must be inside the period"
    );
    let choice = *args.first_choice.get_or_insert(args.start + Months::new(6));
    ensure!(
        first <= choice && choice < args.end,
        "--first-choice must be inside the period, on or after --first-pick"
    );
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

/// Test window size for a training window of `n` trades.
fn test_size(n: usize) -> usize {
    n / 3
}

/// A combination's training window at the moment `at`: its last `n` trades closed at or before
/// `at`, as (profit after fees, largest drop of the running profit, score). `None` with fewer
/// than `n` such trades.
fn training(trades: &[ClosedTrade], at: i64, n: usize, min_drop: f64) -> Option<(f64, f64, f64)> {
    let known = trades.partition_point(|t| t.exit_ms <= at);
    let window = trades.get(known.checked_sub(n)?..known)?;
    let (mut sum, mut peak, mut drop) = (0.0f64, 0.0f64, 0.0f64);
    for t in window {
        sum += f64::from(t.net);
        peak = peak.max(sum);
        drop = drop.max(peak - sum);
    }
    Some((sum, drop, sum / drop.max(min_drop)))
}

/// A combination's test window from the moment `at`: its first `m` trades entered at or after
/// `at`, given each trade's entry time. `None` if the data ends before `m` of them close.
fn test_window(
    trades: &[ClosedTrade],
    entry_ms: impl Fn(&ClosedTrade) -> i64,
    at: i64,
    m: usize,
) -> Option<&[ClosedTrade]> {
    let first = trades.partition_point(|t| entry_ms(t) < at);
    trades.get(first..first + m)
}

/// One window pair's own walk-forward: its picks and the trades it made.
struct Chain {
    windows: Vec<ChainWindow>,
    /// Every test window's trades, in order, and the combination that made each.
    trades: Vec<ClosedTrade>,
    combos: Vec<u32>,
    stop_reason: String,
}

struct ChainWindow {
    net: f64,
}

/// Walk-forward of the window pair `n` / `m` from `first_pick`: the combination with the best
/// score on its last `n` closed trades, traded for its next `m` trades, again and again.
fn run_chain(
    ctx: &Ctx,
    history: &History,
    n: usize,
    m: usize,
    first_pick: i64,
    min_drop: f64,
) -> Chain {
    let entry = |t: &ClosedTrade| ctx.entry_ms(t);
    let mut chain = Chain {
        windows: Vec::new(),
        trades: Vec::new(),
        combos: Vec::new(),
        stop_reason: String::new(),
    };
    let mut at = first_pick;
    loop {
        // Highest score, then highest profit, then the first combination: the same pick every run.
        let best = (0..history.trades.len())
            .filter_map(|c| {
                training(&history.trades[c], at, n, min_drop).map(|(p, _, s)| (c, p, s))
            })
            .min_by(|a, b| {
                b.2.total_cmp(&a.2)
                    .then(b.1.total_cmp(&a.1))
                    .then(a.0.cmp(&b.0))
            });
        let Some((pick, ..)) = best else {
            chain.stop_reason = format!("no combination had {n} closed trades by {}", utc(at));
            return chain;
        };
        let Some(test) = test_window(&history.trades[pick], entry, at, m) else {
            chain.stop_reason = format!(
                "the pick {} has fewer than {m} trades left after {}",
                ctx.label(pick),
                utc(at)
            );
            return chain;
        };
        chain.windows.push(ChainWindow {
            net: test.iter().map(|t| f64::from(t.net)).sum(),
        });
        chain.trades.extend_from_slice(test);
        chain.combos.extend(std::iter::repeat_n(pick as u32, m));
        at = test[m - 1].exit_ms;
    }
}

/// Every combination's trades over the whole period, `[pair * n_settings + setting]`.
struct History {
    trades: Vec<Vec<ClosedTrade>>,
}

struct Ctx<'a> {
    args: &'a Args,
    candles: &'a [Candle],
    tape: &'a Tape,
    market: Market<'a>,
    ind: &'a Indicators,
    exits: &'a [Option<Exits>],
    pairs: &'a [(usize, usize)],
    costs: Costs,
}

impl Ctx<'_> {
    fn combo(&self, c: usize) -> (usize, usize, usize) {
        let (i, j) = self.pairs[c / self.exits.len()];
        (
            self.args.lengths[i],
            self.args.lengths[j],
            c % self.exits.len(),
        )
    }

    fn label(&self, c: usize) -> String {
        let (fast, slow, s) = self.combo(c);
        format!("RSI {fast}/{slow} · {}", exits_label(&self.exits[s]))
    }

    fn entry_ms(&self, t: &ClosedTrade) -> i64 {
        self.tape
            .time_ms(self.tape.bar_start[t.entry_bar as usize] as usize)
    }

    /// Price of the last OKX trade at or before `at`.
    fn price_at(&self, at: i64) -> f64 {
        let bar = self.candles.partition_point(|c| c.time_ms <= at).max(1) - 1;
        let (mut lo, mut hi) = (
            self.tape.bar_start[bar] as usize,
            self.tape
                .bar_start
                .get(bar + 1)
                .map_or(self.tape.price.len(), |s| *s as usize),
        );
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.tape.time_ms(mid) <= at {
                lo = mid + 1
            } else {
                hi = mid
            }
        }
        f64::from(self.tape.price[lo.max(1) - 1]) / 10.0
    }

    fn buy_and_hold(&self, from: i64, to: i64) -> f64 {
        let (a, b) = (self.price_at(from), self.price_at(to));
        self.args.size_btc * (b - a) - self.args.size_btc * (a + b) * self.costs.taker
    }

    /// One combination over the whole period, with every fill and trade.
    fn replay(&self, c: usize) -> Record {
        let (fast, slow, s) = self.combo(c);
        let (i, j) = self.pairs[c / self.exits.len()];
        let mut record = Record::default();
        scan::simulate(
            (fast as u16, slow as u16, s as u16),
            &self.ind.signals(i, j),
            self.exits[s].as_ref(),
            &self.market,
            0..self.candles.len(),
            self.costs,
            Some(&mut record),
        );
        record
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let config = AppConfig::load(&args.config)?;
    let taker = config.venues.okx.taker_fee_bps / 10_000.0;
    let maker = config.venues.okx.maker_fee_bps / 10_000.0;
    let bar_ms = args.minutes as i64 * 60_000;
    let first_pick = ms(args.first_pick.unwrap());

    let started = Instant::now();
    let mut shown = 0;
    let (candles, tape) = Tape::load(
        &args.data,
        bar_ms,
        ms(args.start),
        ms(args.end),
        |done, total| {
            if done * 10 / total.max(1) > shown || done == total {
                shown = done * 10 / total.max(1);
                println!("Reading every OKX trade: archive {done} of {total}");
            }
        },
    )?;
    let gaps = candles
        .windows(2)
        .filter(|w| w[1].time_ms - w[0].time_ms != bar_ms)
        .count();
    println!(
        "{} {}-minute candles from {} to {} (exclusive) in {:.0}s; {gaps} gaps without trades; {} price changes for TP/SL",
        candles.len(),
        args.minutes,
        args.start,
        args.end,
        started.elapsed().as_secs_f64(),
        tape.price.len()
    );

    let exits = exit_settings(
        &args.stops,
        &args.ladders,
        &args.shares,
        &args.breakeven,
        (args.size_btc * 10_000.0).round() as i64,
    )?;
    let n_settings = exits.len();
    ensure!(n_settings <= usize::from(u16::MAX), "too many settings");
    let pairs: Vec<(usize, usize)> = (0..args.lengths.len())
        .flat_map(|i| (i + 1..args.lengths.len()).map(move |j| (i, j)))
        .collect();
    let n_combos = pairs.len() * n_settings;
    let ind = Indicators::new(&candles, &args.lengths, &[Filter::default()]);
    let closes = CloseRanges::new(&candles.iter().map(|c| c.close).collect::<Vec<_>>());
    let costs = Costs {
        steps: (args.size_btc * 10_000.0).round() as i64,
        taker,
        maker,
        balance: REPLAY_BALANCE,
    };
    let ctx = Ctx {
        args: &args,
        candles: &candles,
        tape: &tape,
        market: Market {
            candles: &candles,
            tape: &tape,
            bar_start: &tape.bar_start,
            closes: &closes,
        },
        ind: &ind,
        exits: &exits,
        pairs: &pairs,
        costs,
    };
    println!(
        "{n_combos} combinations ({} RSI pairs × {n_settings} TP/SL settings incl. none), each replayed over the whole period",
        pairs.len()
    );

    // Every combination's trades over the whole period.
    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut per_pair: Vec<(usize, Vec<Vec<ClosedTrade>>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    let mut log = Vec::new();
                    loop {
                        let p = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(i, j)) = pairs.get(p) else {
                            break out;
                        };
                        let signals = ind.signals(i, j);
                        let ids =
                            |s: usize| (args.lengths[i] as u16, args.lengths[j] as u16, s as u16);
                        let per_setting = exits
                            .iter()
                            .enumerate()
                            .map(|(s, exit)| {
                                log.clear();
                                scan::simulate_logged(
                                    ids(s),
                                    &signals,
                                    exit.as_ref(),
                                    &ctx.market,
                                    0..candles.len(),
                                    costs,
                                    &mut log,
                                );
                                log.as_slice().into()
                            })
                            .collect();
                        out.push((p, per_setting));
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if d % 200 == 0 {
                            let secs = started.elapsed().as_secs_f64();
                            println!(
                                "Replayed pair {d} of {}, about {:.0}s left",
                                pairs.len(),
                                secs / d as f64 * (pairs.len() - d) as f64
                            );
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
    per_pair.sort_by_key(|(p, _)| *p);
    let history = History {
        trades: per_pair.into_iter().flat_map(|(_, v)| v).collect(),
    };
    let n_trades: usize = history.trades.iter().map(Vec::len).sum();
    println!(
        "Replayed every combination in {:.0}s: {n_trades} closed trades ({:.0} MB)",
        started.elapsed().as_secs_f64(),
        (n_trades * std::mem::size_of::<ClosedTrade>()) as f64 / 1e6
    );

    let min_drop = args.balance_usdt * 0.01;
    let end_ms = candles.last().map_or(0, |c| c.time_ms + bar_ms);
    let first_choice = ms(args.first_choice.unwrap());
    let configs: Vec<(usize, usize)> = args
        .train_trades
        .iter()
        .flat_map(|&n| (1..=test_size(n)).rev().map(move |m| (n, m)))
        .collect();
    println!(
        "\nStep 1: {} window pairs (training {} trades, each tested on every size from a third down to 1), each run on its own from {}",
        configs.len(),
        args.train_trades
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" / "),
        utc(first_pick)
    );

    // Step 1: every window pair's own walk-forward.
    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let mut chains: Vec<(usize, Chain)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(n, m)) = configs.get(k) else {
                            break out;
                        };
                        out.push((k, run_chain(&ctx, &history, n, m, first_pick, min_drop)));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    chains.sort_by_key(|(k, _)| *k);
    let chains: Vec<Chain> = chains.into_iter().map(|(_, c)| c).collect();
    println!(
        "Ran every window pair in {:.0}s",
        started.elapsed().as_secs_f64()
    );
    let mut config_json = Vec::new();
    for (&(n, m), chain) in configs.iter().zip(&chains) {
        let total: f64 = chain.trades.iter().map(|t| f64::from(t.net)).sum();
        let positive = chain.windows.iter().filter(|w| w.net > 0.0).count();
        config_json.push(json!({
            "train_trades": n,
            "test_trades": m,
            "windows": chain.windows.len(),
            "positive": positive,
            "trades": chain.trades.len(),
            "total": round2(total),
            "until": chain.trades.last().map_or_else(String::new, |t| utc(t.exit_ms)),
            "stop_reason": chain.stop_reason,
        }));
    }
    let mut ranked: Vec<&Value> = config_json.iter().collect();
    ranked.sort_by(|a, b| {
        b["total"]
            .as_f64()
            .unwrap()
            .total_cmp(&a["total"].as_f64().unwrap())
    });
    println!(
        "Window pairs on their own, best and worst (USDT after fees from {}):",
        utc(first_pick)
    );
    for c in ranked
        .iter()
        .take(10)
        .chain(ranked.iter().rev().take(5).rev())
    {
        println!(
            "  {:>3} / {:>2} trades: {:+8.2} over {} test windows ({} positive), {} trades",
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

    // Step 2: again and again, the window pair with the best score on its own last N trades,
    // traded for its next M trades.
    println!(
        "\nStep 2: from {}, choose the window pair with the best score on its own last N trades, trade its next M trades, repeat",
        utc(first_choice)
    );
    let entry = |t: &ClosedTrade| ctx.entry_ms(t);
    let mut at = first_choice;
    let mut balance = args.balance_usdt;
    let mut windows = Vec::new();
    let mut last_combo = None;
    let stop_reason: String;
    loop {
        let mut scored: Vec<(usize, f64, f64, f64)> = (0..configs.len())
            .filter_map(|k| {
                training(&chains[k].trades, at, configs[k].0, min_drop)
                    .map(|(p, d, s)| (k, p, d, s))
            })
            .collect();
        if scored.is_empty() {
            stop_reason = format!("no window pair had N trades of its own by {}", utc(at));
            break;
        }
        let eligible = scored.len();
        // Highest score, then highest profit, then the first window pair (largest sizes first).
        scored.sort_by(|a, b| {
            b.3.total_cmp(&a.3)
                .then(b.1.total_cmp(&a.1))
                .then(a.0.cmp(&b.0))
        });
        let (k, train_profit, train_drop, train_score) = scored[0];
        let ((n, m), chain) = (configs[k], &chains[k]);
        let first = chain.trades.partition_point(|t| entry(t) < at);
        let Some(test) = chain.trades.get(first..first + m) else {
            stop_reason = format!(
                "the chosen window pair {n}/{m} has fewer than {m} trades left after {}",
                utc(at)
            );
            break;
        };
        let combos = &chain.combos[first..first + m];
        let test_from = entry(&test[0]);
        let test_to = test[m - 1].exit_ms;
        ensure!(
            test_from >= at && test_to >= test_from,
            "test window out of order"
        );
        let net: f64 = test.iter().map(|t| f64::from(t.net)).sum();
        let wins = test.iter().filter(|t| t.net > 0.0).count();
        let bh = ctx.buy_and_hold(at, test_to);
        balance += net;
        let mut used: Vec<String> = combos.iter().map(|&c| ctx.label(c as usize)).collect();
        used.dedup();
        println!(
            "Choice {} at {}: window pair {n}/{m} (its last {n} trades {:+.2} USDT, drop {:.2}, score {:.2}; {eligible} pairs eligible) traded {} with {}: {:+.2} USDT ({wins} of {m} won), balance {:.2}; buy-and-hold {:+.2}",
            windows.len() + 1,
            utc(at),
            train_profit,
            train_drop,
            train_score,
            utc(test_from),
            used.join(" then "),
            net,
            balance,
            bh
        );
        windows.push(json!({
            "pick_at": utc(at),
            "train_trades": n,
            "test_trades": m,
            "label": used.join(" then "),
            "train_profit": round2(train_profit),
            "train_drop": round2(train_drop),
            "train_score": round2(train_score),
            "test_from": utc(test_from),
            "test_to": utc(test_to),
            "test_net": round2(net),
            "test_wins": wins,
            "balance": round2(balance),
            "buy_and_hold": round2(bh),
            "eligible": eligible,
        }));
        last_combo = combos.last().map(|&c| c as usize);
        if balance <= 0.0 {
            stop_reason = "the account ran out of money".into();
            break;
        }
        at = test_to;
    }
    let total = balance - args.balance_usdt;
    let positive = windows
        .iter()
        .filter(|w| w["test_net"].as_f64().unwrap() > 0.0)
        .count();
    let bh_total: f64 = windows
        .iter()
        .map(|w| w["buy_and_hold"].as_f64().unwrap())
        .sum();
    let falling: f64 = windows
        .iter()
        .filter(|w| w["buy_and_hold"].as_f64().unwrap() < 0.0)
        .map(|w| w["test_net"].as_f64().unwrap())
        .sum();
    println!(
        "\nResult: {} choices, {:+.2} USDT after fees, {positive} positive; buy-and-hold over the same windows {:+.2}; in windows where BTC fell {:+.2}. Stopped because {stop_reason}.",
        windows.len(),
        total,
        bh_total,
        falling
    );
    let chosen = json!({
        "windows": windows,
        "total": round2(total),
        "positive": positive,
        "buy_and_hold_total": round2(bh_total),
        "in_falling_windows": round2(falling),
        "stop_reason": stop_reason,
    });
    let to_verify: Vec<usize> = last_combo.into_iter().take(args.verify).collect();
    // The compact trades the windows used must be exactly the full replay's closed trades.
    let mut checks = Vec::new();
    for &c in &to_verify {
        let record = ctx.replay(c);
        let closed: Vec<&scan::Trade> = record.trades.iter().filter(|t| !t.open).collect();
        let log = &history.trades[c];
        ensure!(
            closed.len() == log.len(),
            "{}: {} closed trades in the full replay, {} in the history",
            ctx.label(c),
            closed.len(),
            log.len()
        );
        for (t, l) in closed.iter().zip(log) {
            let net = t
                .exits
                .iter()
                .map(|e| {
                    (if t.long { 1.0 } else { -1.0 })
                        * (e.price - t.entry_price)
                        * e.steps as f64
                        * STEP_BTC
                        - e.fee
                })
                .sum::<f64>()
                - t.entry_fee;
            ensure!(
                t.exits.last().map(|e| e.time_ms) == Some(l.exit_ms)
                    && t.entry_ms == ctx.entry_ms(l)
                    && (net - f64::from(l.net)).abs() < 1e-3,
                "{}: trade at {} differs between the full replay and the history",
                ctx.label(c),
                utc(t.entry_ms)
            );
        }
        let (fast, slow, s) = ctx.combo(c);
        checks.push(Check {
            label: ctx.label(c),
            fast,
            slow,
            exits: exits[s].clone(),
            fills: record.fills,
        });
    }

    let mut report = json!({
        "meta": {
            "instrument": okx_history::INSTRUMENT,
            "start": args.start.to_string(),
            "end": args.end.to_string(),
            "end_ms": end_ms,
            "first_pick": utc(first_pick),
            "first_choice": utc(first_choice),
            "timeframe_minutes": args.minutes,
            "combinations": n_combos,
            "pairs": pairs.len(),
            "exit_settings": n_settings,
            "window_pairs": configs.len(),
            "size_btc": args.size_btc,
            "balance_usdt": args.balance_usdt,
            "taker_fee_bps": config.venues.okx.taker_fee_bps,
            "maker_fee_bps": config.venues.okx.maker_fee_bps,
            "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
        },
        "window_pairs": config_json,
        "chosen": chosen,
        "verify": [],
    });
    fs::create_dir_all(&args.out)?;
    let base = args.out.join(format!(
        "walk_forward_trades_{}m_{}_{}",
        args.minutes, args.start, args.end
    ));
    let save = |report: &Value| -> Result<()> {
        fs::write(
            base.with_extension("json"),
            serde_json::to_string_pretty(report)?,
        )?;
        let html = REPORT_HTML
            .replace("/*__TABLE_SORT__*/", TABLE_SORT)
            .replace(
                "/*__REPORT__*/null",
                &serde_json::to_string(report)?.replace("</", "<\\/"),
            );
        fs::write(base.with_extension("html"), html)?;
        Ok(())
    };
    save(&report)?;
    println!(
        "\nReport (open in a browser): {}",
        base.with_extension("html").display()
    );

    // NautilusTrader reads every trade again: free the replays' memory first.
    drop(history);
    drop(ctx);
    drop(tape);
    let mut verify_json = Vec::new();
    for check in checks {
        verify_json.push(verify(&args, &config, check)?);
        report["verify"] = Value::Array(verify_json.clone());
        save(&report)?;
    }
    if !verify_json.is_empty() {
        println!(
            "Report updated with the NautilusTrader check: {}",
            base.with_extension("html").display()
        );
    }
    if verify_json.iter().any(|v| v["ok"] == false) {
        bail!("NautilusTrader and the scanner disagree, see above");
    }
    Ok(())
}

/// A pick to rerun through NautilusTrader, with the scanner's fills over the whole period.
struct Check {
    label: String,
    fast: usize,
    slow: usize,
    exits: Option<Exits>,
    fills: Vec<Fill>,
}

/// Reruns `check` over the whole period through NautilusTrader with the same balance as the
/// replays, and compares every fill with the scanner's.
fn verify(args: &Args, config: &AppConfig, check: Check) -> Result<Value> {
    let name = format!("{} ({} to {})", check.label, args.start, args.end);
    let run = Run {
        config,
        data: &args.data,
        start: args.start,
        end: args.end,
        bar_minutes: args.minutes as u64,
        size_btc: args.size_btc,
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
        check.exits,
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

const REPORT_HTML: &str = include_str!("walk_forward_trades_report.html");
const TABLE_SORT: &str = include_str!("table_sort.js");

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(entry_bar: u32, exit_ms: i64, net: f32) -> ClosedTrade {
        ClosedTrade {
            exit_ms,
            entry_bar,
            net,
        }
    }

    #[test]
    fn test_window_is_a_third_rounded_down() {
        assert_eq!((test_size(100), test_size(90), test_size(80)), (33, 30, 26));
    }

    #[test]
    fn training_uses_only_the_last_trades_closed_by_the_pick() {
        // Exits at 10, 20, ..., 60 with nets 1, -2, 3, -4, 5, -6.
        let trades: Vec<_> = (1..=6)
            .map(|k| {
                trade(
                    k,
                    k as i64 * 10,
                    if k % 2 == 1 { k as f32 } else { -(k as f32) },
                )
            })
            .collect();
        // At 45 the last 3 known trades are the 2nd to the 4th: -2 + 3 - 4 = -3, drop from 1 to -3 = 4.
        let (profit, drop, score) = training(&trades, 45, 3, 1.0).unwrap();
        assert_eq!((profit, drop), (-3.0, 4.0));
        assert_eq!(score, -0.75);
        // A trade closing exactly at the pick is known.
        assert_eq!(training(&trades, 40, 4, 1.0).unwrap().0, -2.0);
        // Not enough closed trades yet.
        assert!(training(&trades, 25, 3, 1.0).is_none());
    }

    #[test]
    fn small_drops_count_as_the_minimum() {
        let trades: Vec<_> = (1..=3).map(|k| trade(k, k as i64, 1.0)).collect();
        assert_eq!(training(&trades, 3, 3, 5.0).unwrap(), (3.0, 0.0, 0.6));
    }

    #[test]
    fn test_window_starts_with_the_first_entry_at_or_after_the_pick() {
        // Entry time = entry bar × 10.
        let trades: Vec<_> = (1..=6).map(|k| trade(k, k as i64 * 10 + 5, 0.0)).collect();
        let entry = |t: &ClosedTrade| i64::from(t.entry_bar) * 10;
        // A trade entered before the pick and still open at it is skipped.
        assert_eq!(test_window(&trades, entry, 21, 2).unwrap()[0].entry_bar, 3);
        assert_eq!(test_window(&trades, entry, 30, 2).unwrap()[0].entry_bar, 3);
        assert!(test_window(&trades, entry, 41, 3).is_none());
    }
}
