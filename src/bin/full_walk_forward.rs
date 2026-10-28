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
use chrono::{DateTime, Months, NaiveDate, Utc};
use dynamic_trader::{
    backtest::{Fill, Run, run_backtest},
    config::AppConfig,
    exits::Exits,
    filters::{Filter, FilteredCross},
    okx_history::{self, Candle},
    rsi::Rsi,
    rsi_cross::RsiCrossParams,
    scan::{self, ClosedTrade, Costs, Market, Record, STEP_BTC},
    strategy::BarSignals,
    tape::{CloseRanges, Tape},
    tpsl_grid::{Entries, Frame, Grid, GridTrade},
};
use serde::Deserialize;
use serde_json::{Value, json};

const USAGE: &str = "\
Stage 2 of the full search: a walk-forward with windows counted in trades, choosing among the
best combinations of the month before, after full_scan has scored every combination.

1. Every month, every combination (RSI pair x timeframe x take-profit x stop-loss) is ranked by
   its score in the month before: profit after fees divided by the largest drop of that profit,
   counted trade by trade (at least 1% of the balance). The best ones are the month's
   finalists: as many as --max-finalists, fewer if their trades would not fit in --memory-gb.
2. Every window pair N/M (N from --train-trades, M from N/3 down to 1) runs its own
   walk-forward from --first-pick: each finalist's last N trades closed by then are scored, the
   best is traded for its next M trades entered at or after the pick, and when the last one
   closes it picks again, among the finalists of the month it is then in.
3. From --first-choice, the window pairs themselves are chosen the same way, on their own last
   N trades, again and again. Nothing after a pick or a choice is used to make it.
4. The chosen trades chained together are what a bot doing this would have made, compared with
   buy-and-hold. Combinations traded are replayed again by the trade-by-trade scanner, and the
   last one through NautilusTrader, to confirm every trade and fill.

Usage: cargo run --release --bin full_walk_forward -- [options]

  --scan <dir>                full_scan results (default: data/fullscan/2025-10-01_2026-10-01)
  --train-trades <list>       Training window sizes in trades (default: 100,90,...,10)
  --first-pick <YYYY-MM-DD>   First pick of every window pair, the first day of a month
                              (default: the second month)
  --first-choice <YYYY-MM-DD> First choice of a window pair (default: 6 months after the start)
  --max-finalists <n>         Finalists per month at most (default: 1000000)
  --memory-gb <gb>            Memory for the finalists' trades (default: 2)
  --verify <n>                1 to rerun the last combination traded through NautilusTrader
                              (default: 1, 0 to skip)
  --balance-usdt <usdt>       Starting balance of the chained account (default: 500)
  --config <path>             Fee config (default: config/testnet.toml)
  --data <dir>                Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --out <dir>                 Where to write the report (default: data/walkforward)";

/// Balance of the whole-period replays, as in full_scan.
const REPLAY_BALANCE: f64 = 1e9;
/// Bytes per combination and month in full_scan's files.
const RECORD: usize = 12;
/// Combinations shown per month in the report.
const SHOWN: usize = 20;

struct Args {
    scan: PathBuf,
    train_trades: Vec<usize>,
    first_pick: Option<NaiveDate>,
    first_choice: Option<NaiveDate>,
    max_finalists: usize,
    memory_gb: f64,
    verify: usize,
    balance_usdt: f64,
    config: PathBuf,
    data: PathBuf,
    out: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        scan: "data/fullscan/2025-10-01_2026-10-01".into(),
        train_trades: (1..=10).rev().map(|k| k * 10).collect(),
        first_pick: None,
        first_choice: None,
        max_finalists: 1_000_000,
        memory_gb: 2.0,
        verify: 1,
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
            "--scan" => args.scan = value.into(),
            "--train-trades" => {
                args.train_trades = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?
            }
            "--first-pick" => args.first_pick = Some(value.parse().with_context(bad)?),
            "--first-choice" => args.first_choice = Some(value.parse().with_context(bad)?),
            "--max-finalists" => args.max_finalists = value.parse().with_context(bad)?,
            "--memory-gb" => args.memory_gb = value.parse().with_context(bad)?,
            "--verify" => args.verify = value.parse().with_context(bad)?,
            "--balance-usdt" => args.balance_usdt = value.parse().with_context(bad)?,
            "--config" => args.config = value.into(),
            "--data" => args.data = value.into(),
            "--out" => args.out = value.into(),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(
        !args.train_trades.is_empty() && args.train_trades.iter().all(|n| *n >= 3),
        "--train-trades must be at least 3"
    );
    ensure!(
        args.max_finalists > 0 && args.memory_gb > 0.0,
        "--max-finalists and --memory-gb must be positive"
    );
    Ok(args)
}

/// full_scan's meta.json.
#[derive(Deserialize)]
struct Meta {
    start: NaiveDate,
    end: NaiveDate,
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    tp: Vec<f64>,
    sl: Vec<f64>,
    /// Missing in older scans: treated as `[1]` (no leverage axis).
    #[serde(default = "default_leverage")]
    leverage: Vec<u32>,
    combinations: usize,
    every_pair: usize,
    months: Vec<String>,
    size_btc: f64,
    taker_fee_bps: f64,
    maker_fee_bps: f64,
}

fn default_leverage() -> Vec<u32> {
    vec![1]
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

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Test window size for a training window of `n` trades.
fn test_size(n: usize) -> usize {
    n / 3
}

/// Every combination full_scan replayed: id = (timeframe * pairs + pair) * settings + setting.
struct Space {
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    pairs: Vec<(usize, usize)>,
    grid: Grid,
}

impl Space {
    /// (timeframe index, pair index, take-profit index, stop-loss index, leverage index).
    fn decode(&self, id: u32) -> (usize, usize, usize, usize, usize) {
        let id = id as usize;
        let (group, s) = (id / self.grid.len(), id % self.grid.len());
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
        id as usize / self.grid.len()
    }

    fn leverage(&self, id: u32) -> u32 {
        let (_, _, _, _, li) = self.decode(id);
        self.grid.lev[li]
    }

    fn label(&self, id: u32) -> String {
        let (f, p, tp, sl, li) = self.decode(id);
        let (i, j) = self.pairs[p];
        format!(
            "{}m RSI {}/{} · SL {}% · TP {}% · {}x",
            self.timeframes[f],
            self.lengths[i],
            self.lengths[j],
            self.grid.sl[sl],
            self.grid.tp[tp],
            self.grid.lev[li]
        )
    }

    fn exits(&self, id: u32) -> Exits {
        let (_, _, tp, sl, _) = self.decode(id);
        Exits {
            stop_pct: self.grid.sl[sl],
            targets: vec![(self.grid.tp[tp], 1.0)],
            breakeven: false,
        }
    }
}

/// One combination over one month, from full_scan.
#[derive(Debug, Clone, Copy)]
struct Scored {
    id: u32,
    profit: f32,
    drop: f32,
    trades: u32,
    score: f32,
}

/// Highest score, then highest profit, then the lowest id first.
fn better(a: &Scored, b: &Scored) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then(b.profit.total_cmp(&a.profit))
        .then(a.id.cmp(&b.id))
}

fn record(bytes: &[u8]) -> (f32, f32, u32) {
    let f = |k: usize| f32::from_le_bytes(bytes[k..k + 4].try_into().unwrap());
    (
        f(0),
        f(4),
        u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
    )
}

/// The `keep` best combinations of one of full_scan's files, best first, with the number of
/// combinations that traded and of those that made money.
fn rank(
    path: &std::path::Path,
    n_combos: usize,
    keep: usize,
    min_drop: f64,
) -> Result<(Vec<Scored>, usize, usize)> {
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
    let (mut traded, mut profitable) = (0, 0);
    for first in (0..n_combos).step_by(chunk) {
        let n = chunk.min(n_combos - first);
        file.read_exact_at(&mut buf[..n * RECORD], (first * RECORD) as u64)?;
        for k in 0..n {
            let (profit, drop, trades) = record(&buf[k * RECORD..(k + 1) * RECORD]);
            if trades == 0 {
                continue;
            }
            traded += 1;
            profitable += usize::from(profit > 0.0);
            let s = Scored {
                id: (first + k) as u32,
                profit,
                drop,
                trades,
                score: (f64::from(profit) / f64::from(drop).max(min_drop)) as f32,
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
    Ok((kept, traded, profitable))
}

/// One trade of a finalist, with its running profit since the first trade kept.
#[derive(Debug, Clone, Copy)]
struct Kept {
    entry_ms: i64,
    exit_ms: i64,
    /// Profit after fees of every kept trade up to and including this one.
    cum: f64,
}

/// A finalist's trades around one month: up to the longest training window before it, every
/// trade closing in it, and up to the longest test window entered after it.
struct Finalist {
    id: u32,
    trades: Vec<Kept>,
}

impl Finalist {
    fn net(&self, k: usize) -> f64 {
        self.trades[k].cum - if k == 0 { 0.0 } else { self.trades[k - 1].cum }
    }
}

/// A pick: the finalist and its training window's profit and score.
#[derive(Debug, Clone, Copy)]
struct Pick {
    finalist: usize,
    profit: f64,
    score: f64,
}

/// The finalist with the best score on its last `n` trades closed at or before `at`.
fn pick(finalists: &[Finalist], at: i64, n: usize, min_drop: f64) -> Option<Pick> {
    let mut best: Option<(Pick, u32)> = None;
    for (k, f) in finalists.iter().enumerate() {
        let known = f.trades.partition_point(|t| t.exit_ms <= at);
        if known < n {
            continue;
        }
        let before = if known > n {
            f.trades[known - n - 1].cum
        } else {
            0.0
        };
        let profit = f.trades[known - 1].cum - before;
        // The score can be at most profit / min_drop (or 0 without a profit).
        let ceiling = if profit > 0.0 { profit / min_drop } else { 0.0 };
        if best.is_some_and(|(b, _)| ceiling < b.score) {
            continue;
        }
        let (mut peak, mut drop) = (0.0f64, 0.0f64);
        for t in &f.trades[known - n..known] {
            let sum = t.cum - before;
            peak = peak.max(sum);
            drop = drop.max(peak - sum);
        }
        let score = profit / drop.max(min_drop);
        let wins = match best {
            None => true,
            // Greater: this finalist is better than the best so far.
            Some((b, id)) => {
                score
                    .total_cmp(&b.score)
                    .then(profit.total_cmp(&b.profit))
                    .then(id.cmp(&f.id))
                    == Ordering::Greater
            }
        };
        if wins {
            best = Some((
                Pick {
                    finalist: k,
                    profit,
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

/// Picks and trades of `chain` from its current time until the end of the month, among
/// `finalists`.
fn advance(
    chain: &mut Chain,
    finalists: &[Finalist],
    month_end: i64,
    min_drop: f64,
    space: &Space,
) {
    while !chain.done && chain.at < month_end {
        let (n, m, at) = (chain.n, chain.m, chain.at);
        let Some(p) = pick(finalists, at, n, min_drop) else {
            chain.stop_reason = format!("no finalist had {n} closed trades by {}", utc(at));
            chain.done = true;
            return;
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
            return;
        }
        let mut net = 0.0;
        for k in first..first + m {
            let t = &f.trades[k];
            chain.trades.push(ChainTrade {
                entry_ms: t.entry_ms,
                exit_ms: t.exit_ms,
                net: f.net(k),
                combo: f.id,
            });
            net += f.net(k);
        }
        chain.windows.push(net);
        chain.at = f.trades[first + m - 1].exit_ms;
    }
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
    /// Signals of RSI pair `p` on timeframe `f`; the RSIs are computed again each time rather
    /// than kept for every length, to leave the memory to the finalists.
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
        let exits = self.space.exits(id);
        let signals = self.signals(f, p);
        match record {
            Some(r) => {
                scan::simulate(
                    (0, 0, 0),
                    &signals,
                    Some(&exits),
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
                    Some(&exits),
                    &market,
                    0..frame.candles.len(),
                    costs,
                    log,
                );
            }
        }
    }
}

/// The trades of `ids` (finalists of one month) kept around month `q`, built once per RSI pair
/// and timeframe.
fn build_finalists(
    mk: &Market1m,
    ids: &[u32],
    month_step: &[u32],
    q: usize,
    before: usize,
    after: usize,
    threads: usize,
) -> Vec<Finalist> {
    let space = mk.space;
    let mut order: Vec<usize> = (0..ids.len()).collect();
    order.sort_by_key(|&k| ids[k]);
    // Runs of finalists in the same group.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (pos, &k) in order.iter().enumerate() {
        match runs.last_mut() {
            Some((_, end)) if space.group(ids[order[*end - 1]]) == space.group(ids[k]) => {
                *end = pos + 1
            }
            _ => runs.push((pos, pos + 1)),
        }
    }
    let start_step = month_step[q];
    let end_step = month_step.get(q + 1).copied().unwrap_or(u32::MAX);
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
                            let first_in = all.partition_point(|t| t.exit_step < start_step);
                            let first_after =
                                all.partition_point(|t| entries.step[t.signal as usize] < end_step);
                            let (lo, hi) = (
                                first_in.saturating_sub(before),
                                (first_after + after).min(all.len()),
                            );
                            let mut cum = 0.0;
                            let trades = all[lo..hi]
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

fn main() -> Result<()> {
    let args = parse_args()?;
    let config = AppConfig::load(&args.config)?;
    let meta: Meta = serde_json::from_str(
        &fs::read_to_string(args.scan.join("meta.json")).context("full_scan's meta.json")?,
    )?;
    ensure!(
        meta.every_pair == 1,
        "full_scan ran on every {}th RSI pair only; run it on all of them",
        meta.every_pair
    );
    ensure!(
        meta.taker_fee_bps == config.venues.okx.taker_fee_bps
            && meta.maker_fee_bps == config.venues.okx.maker_fee_bps,
        "full_scan used other fees than {}",
        args.config.display()
    );
    let pairs: Vec<(usize, usize)> = (0..meta.lengths.len())
        .flat_map(|i| (i + 1..meta.lengths.len()).map(move |j| (i, j)))
        .collect();
    let space = Space {
        timeframes: meta.timeframes.clone(),
        lengths: meta.lengths.clone(),
        pairs,
        grid: Grid {
            tp: meta.tp.clone(),
            sl: meta.sl.clone(),
            lev: meta.leverage.clone(),
        },
    };
    ensure!(
        space.timeframes.len() * space.pairs.len() * space.grid.len() == meta.combinations,
        "meta.json does not add up"
    );
    let n_combos = meta.combinations;
    let min_drop = args.balance_usdt * 0.01;
    let month_days: Vec<NaiveDate> = meta
        .months
        .iter()
        .map(|m| NaiveDate::parse_from_str(&format!("{m}-01"), "%Y-%m-%d"))
        .collect::<Result<_, _>>()?;
    let n_months = month_days.len();
    let first_pick = args.first_pick.unwrap_or(
        month_days
            .get(1)
            .copied()
            .context("full_scan covered less than two months")?,
    );
    let q0 = month_days
        .iter()
        .position(|d| *d == first_pick)
        .context("--first-pick must be the first day of a month after the first")?;
    ensure!(q0 >= 1, "--first-pick needs a month before it to rank");
    let first_choice = args.first_choice.unwrap_or(meta.start + Months::new(6));
    ensure!(
        first_pick <= first_choice && first_choice < meta.end,
        "--first-choice must be inside the period, on or after --first-pick"
    );
    let max_n = *args.train_trades.iter().max().unwrap();
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

    // Monthly rankings: month q's finalists are the best of month q - 1.
    println!(
        "Ranking all {n_combos} combinations month by month (score = profit after fees / largest drop, at least {min_drop} USDT)"
    );
    let mut finalist_ids: Vec<Vec<u32>> = vec![Vec::new(); n_months];
    let mut months_json = Vec::new();
    for q in q0..n_months {
        let started = Instant::now();
        let ranked_month = q - 1;
        let path = args
            .scan
            .join(format!("month_{}.bin", meta.months[ranked_month]));
        let (top, traded, profitable) = rank(&path, n_combos, args.max_finalists, min_drop)?;
        // As many as fit in memory, estimating each one's trades from the month ranked.
        let mut used = 0;
        let kept = top.iter().take_while(|s| {
            used +=
                ((s.trades as usize * 6 / 5 + max_n + max_m) * std::mem::size_of::<Kept>()) + 64;
            used <= budget
        });
        let ids: Vec<u32> = kept.map(|s| s.id).collect();
        // How the finalists did in the month they were chosen for.
        let next = File::open(args.scan.join(format!("month_{}.bin", meta.months[q])))?;
        let mut buf = [0u8; RECORD];
        let mut next_of = |id: u32| -> Result<(f32, u32)> {
            next.read_exact_at(&mut buf, id as u64 * RECORD as u64)?;
            let (p, _, t) = record(&buf);
            Ok((p, t))
        };
        let (mut next_total, mut next_positive) = (0.0f64, 0);
        for &id in &ids {
            let (p, _) = next_of(id)?;
            next_total += f64::from(p);
            next_positive += usize::from(p > 0.0);
        }
        let shown = top
            .iter()
            .take(SHOWN)
            .enumerate()
            .map(|(r, s)| {
                let (p, t) = next_of(s.id)?;
                Ok(json!({ "rank": r + 1, "label": space.label(s.id), "score": round2(f64::from(s.score)), "profit": round2(f64::from(s.profit)), "drop": round2(f64::from(s.drop)), "trades": s.trades, "next_profit": round2(f64::from(p)), "next_trades": t }))
            })
            .collect::<Result<Vec<_>>>()?;
        println!(
            "  {}: {traded} combinations traded, {profitable} made money; best {} (score {:.2}, {:+.2} USDT); {} finalists for {} ({:.1}s), who then made {:+.0} USDT together, {} of them positive",
            meta.months[ranked_month],
            top.first().map_or_else(String::new, |s| space.label(s.id)),
            top.first().map_or(0.0, |s| s.score),
            top.first().map_or(0.0, |s| s.profit),
            ids.len(),
            meta.months[q],
            started.elapsed().as_secs_f64(),
            next_total,
            next_positive
        );
        months_json.push(json!({
            "ranked": meta.months[ranked_month],
            "traded_in": meta.months[q],
            "combinations_traded": traded,
            "profitable": profitable,
            "finalists": ids.len(),
            "finalists_next_total": round2(next_total),
            "finalists_next_positive": next_positive,
            "top": shown,
        }));
        finalist_ids[q] = ids;
    }
    // The whole period at once, with hindsight: only to show what was possible.
    let (year_top, year_traded, year_profitable) =
        rank(&args.scan.join("all.bin"), n_combos, 50, min_drop)?;
    let year_json: Vec<Value> = year_top
        .iter()
        .map(|s| json!({ "label": space.label(s.id), "score": round2(f64::from(s.score)), "profit": round2(f64::from(s.profit)), "drop": round2(f64::from(s.drop)), "trades": s.trades }))
        .collect();
    println!(
        "Whole period with hindsight: {year_traded} combinations traded, {year_profitable} made money; best {}",
        year_top
            .first()
            .map_or_else(String::new, |s| space.label(s.id))
    );

    // The trades, as full_scan replayed them.
    let started = Instant::now();
    let mut shown = 0;
    let (minutes, tape) = Tape::load(
        &args.data,
        60_000,
        ms(meta.start),
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
    let end_ms = ms(meta.end);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());

    // Step 1: every window pair's own walk-forward, month by month.
    println!(
        "\nStep 1: {} window pairs (training {} trades, each tested on every size from a third down to 1), each run on its own from {first_pick}",
        configs.len(),
        args.train_trades
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" / ")
    );
    let chains: Vec<Mutex<Chain>> = configs
        .iter()
        .map(|&(n, m)| {
            Mutex::new(Chain {
                n,
                m,
                at: ms(first_pick),
                done: false,
                windows: Vec::new(),
                trades: Vec::new(),
                stop_reason: "the data ended".into(),
            })
        })
        .collect();
    for q in q0..n_months {
        let started = Instant::now();
        let finalists =
            build_finalists(&mk, &finalist_ids[q], &month_step, q, max_n, max_m, threads);
        let kept: usize = finalists.iter().map(|f| f.trades.len()).sum();
        let built = started.elapsed().as_secs_f64();
        let month_end = month_days.get(q + 1).map_or(end_ms, |d| ms(*d));
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let k = next.fetch_add(1, AtomicOrdering::Relaxed);
                        let Some(chain) = chains.get(k) else { break };
                        advance(
                            &mut chain.lock().unwrap(),
                            &finalists,
                            month_end,
                            min_drop,
                            &space,
                        );
                    }
                });
            }
        });
        let picks: usize = chains.iter().map(|c| c.lock().unwrap().windows.len()).sum();
        println!(
            "  {}: {} finalists, {kept} trades kept ({:.1} GB) in {built:.0}s; window pairs ran in {:.0}s; {picks} picks so far",
            meta.months[q],
            finalists.len(),
            (kept * std::mem::size_of::<Kept>()) as f64 / 1e9,
            started.elapsed().as_secs_f64() - built
        );
    }
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

    // Step 2: again and again, the window pair with the best score on its own last N trades.
    println!(
        "\nStep 2: from {first_choice}, choose the window pair with the best score on its own last N trades, trade its next M trades, repeat"
    );
    let mut at = ms(first_choice);
    let mut balance = args.balance_usdt;
    let mut windows = Vec::new();
    let mut traded: Vec<u32> = Vec::new();
    let stop_reason: String;
    loop {
        let mut scored: Vec<(usize, f64, f64, f64)> = Vec::new();
        for (k, c) in chains.iter().enumerate() {
            let known = c.trades.partition_point(|t| t.exit_ms <= at);
            if known < c.n {
                continue;
            }
            let (mut sum, mut peak, mut drop) = (0.0f64, 0.0f64, 0.0f64);
            for t in &c.trades[known - c.n..known] {
                sum += t.net;
                peak = peak.max(sum);
                drop = drop.max(peak - sum);
            }
            scored.push((k, sum, drop, sum / drop.max(min_drop)));
        }
        if scored.is_empty() {
            stop_reason = format!("no window pair had N trades of its own by {}", utc(at));
            break;
        }
        let eligible = scored.len();
        scored.sort_by(|a, b| {
            b.3.total_cmp(&a.3)
                .then(b.1.total_cmp(&a.1))
                .then(a.0.cmp(&b.0))
        });
        let (k, train_profit, train_drop, train_score) = scored[0];
        let c = &chains[k];
        let first = c.trades.partition_point(|t| t.entry_ms < at);
        let Some(test) = c.trades.get(first..first + c.m) else {
            stop_reason = format!(
                "the chosen window pair {}/{} has fewer than {} trades left after {}",
                c.n,
                c.m,
                c.m,
                utc(at)
            );
            break;
        };
        let (test_from, test_to) = (test[0].entry_ms, test[c.m - 1].exit_ms);
        ensure!(
            test_from >= at && test_to >= test_from,
            "test window out of order"
        );
        let net: f64 = test.iter().map(|t| t.net).sum();
        let wins = test.iter().filter(|t| t.net > 0.0).count();
        let bh = mk.buy_and_hold(meta.size_btc, at, test_to);
        balance += net;
        let mut used: Vec<u32> = test.iter().map(|t| t.combo).collect();
        used.dedup();
        traded.extend(&used);
        let label = used
            .iter()
            .map(|&id| space.label(id))
            .collect::<Vec<_>>()
            .join(" then ");
        println!(
            "Choice {} at {}: window pair {}/{} (its last {} trades {:+.2} USDT, drop {:.2}, score {:.2}; {eligible} pairs eligible) traded {} with {label}: {:+.2} USDT ({wins} of {} won), balance {:.2}; buy-and-hold {:+.2}",
            windows.len() + 1,
            utc(at),
            c.n,
            c.m,
            c.n,
            train_profit,
            train_drop,
            train_score,
            utc(test_from),
            net,
            c.m,
            balance,
            bh
        );
        windows.push(json!({
            "pick_at": utc(at),
            "train_trades": c.n,
            "test_trades": c.m,
            "label": label,
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

    // Every combination the chosen trades came from, replayed again trade by trade.
    let mut distinct = traded.clone();
    distinct.sort_unstable();
    distinct.dedup();
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
        "{} combinations traded by the chosen window pairs, replayed again trade by trade: identical",
        distinct.len()
    );

    let chosen = json!({
        "windows": windows,
        "total": round2(total),
        "positive": positive,
        "buy_and_hold_total": round2(bh_total),
        "in_falling_windows": round2(falling),
        "stop_reason": stop_reason,
        "combinations_checked": distinct.len(),
    });
    let mut report = json!({
        "meta": {
            "instrument": okx_history::INSTRUMENT,
            "start": meta.start.to_string(),
            "end": meta.end.to_string(),
            "timeframes": space.timeframes,
            "lengths": space.lengths.len(),
            "pairs": space.pairs.len(),
            "tp": [space.grid.tp.first(), space.grid.tp.last(), space.grid.tp.len()],
            "sl": [space.grid.sl.first(), space.grid.sl.last(), space.grid.sl.len()],
            "leverage": [space.grid.lev.first(), space.grid.lev.last(), space.grid.lev.len()],
            "combinations": n_combos,
            "first_pick": first_pick.to_string(),
            "first_choice": first_choice.to_string(),
            "window_pairs": configs.len(),
            "max_finalists": args.max_finalists,
            "memory_gb": args.memory_gb,
            "size_btc": meta.size_btc,
            "balance_usdt": args.balance_usdt,
            "taker_fee_bps": config.venues.okx.taker_fee_bps,
            "maker_fee_bps": config.venues.okx.maker_fee_bps,
            "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
        },
        "months": months_json,
        "year": { "traded": year_traded, "profitable": year_profitable, "top": year_json },
        "window_pairs": config_json,
        "chosen": chosen,
        "verify": [],
    });
    fs::create_dir_all(&args.out)?;
    let base = args
        .out
        .join(format!("full_walk_forward_{}_{}", meta.start, meta.end));
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

    // The last combination traded, through NautilusTrader over the whole period.
    let check = traded
        .last()
        .copied()
        .filter(|_| args.verify > 0)
        .map(|id| {
            let mut record = Record::default();
            mk.replay(id, Some(&mut record), &mut Vec::new());
            let (f, p, ..) = space.decode(id);
            let (i, j) = space.pairs[p];
            Check {
                label: space.label(id),
                minutes: space.timeframes[f],
                fast: space.lengths[i],
                slow: space.lengths[j],
                exits: space.exits(id),
                size_btc: meta.size_btc * f64::from(space.leverage(id)),
                fills: record.fills,
            }
        });
    // NautilusTrader reads every trade again: free the tape first.
    drop(mk);
    drop(frames);
    drop(tape);
    if let Some(check) = check {
        let v = verify(&meta, &args, &config, check)?;
        let ok = v["ok"] == true;
        report["verify"] = Value::Array(vec![v]);
        save(&report)?;
        println!(
            "Report updated with the NautilusTrader check: {}",
            base.with_extension("html").display()
        );
        if !ok {
            bail!("NautilusTrader and the scanner disagree, see above");
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

/// Reruns `check` over the whole period through NautilusTrader with the same balance as the
/// replays, and compares every fill with the scanner's.
fn verify(meta: &Meta, args: &Args, config: &AppConfig, check: Check) -> Result<Value> {
    let name = format!("{} ({} to {})", check.label, meta.start, meta.end);
    let run = Run {
        config,
        data: &args.data,
        start: meta.start,
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

const REPORT_HTML: &str = include_str!("full_walk_forward_report.html");
const TABLE_SORT: &str = include_str!("table_sort.js");

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn picks_like_scoring_every_finalist() {
        // Exits at 5, 15, 25, ...; at 35 the last 3 known trades are the 2nd to the 4th.
        let a = finalist(7, &[1.0, -2.0, 3.0, -4.0, 9.0]);
        let b = finalist(3, &[0.0, 2.0, 2.0, 1.0, -9.0]);
        let c = finalist(5, &[5.0, 1.0, 1.0]);
        let p = pick(&[a, b, c], 35, 3, 1.0).unwrap();
        // a: -3 (drop 4); b: +5, no drop -> 5; c: only 3 trades known by 35, +7 -> 7.
        assert_eq!((p.finalist, p.profit), (2, 7.0));
        assert_eq!(p.score, 7.0);
    }

    #[test]
    fn ties_go_to_the_higher_profit_then_the_lower_id() {
        // Same score 1: profit 2 / drop 2 versus profit 1 / minimum drop 1.
        let a = finalist(9, &[0.0, -2.0, 4.0]);
        let p = pick(
            &[finalist(4, &[0.0, 1.0]), a, finalist(4, &[0.5, 0.5])],
            100,
            2,
            1.0,
        )
        .unwrap();
        assert_eq!(p.score, 1.0);
        assert_eq!(p.finalist, 1, "{p:?}");
        let p = pick(
            &[finalist(8, &[0.5, 0.5]), finalist(4, &[0.5, 0.5])],
            100,
            2,
            1.0,
        )
        .unwrap();
        assert_eq!(p.finalist, 1);
    }

    /// Skipping finalists that cannot beat the best so far picks the same as scoring them all.
    #[test]
    fn skipping_picks_the_same_as_scoring_all() {
        let mut x: u64 = 5;
        let mut rand = || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as f64 / (1u64 << 31) as f64
        };
        for round in 0..300 {
            let finalists: Vec<Finalist> = (0..40)
                .map(|id| {
                    let len = 3 + (rand() * 20.0) as usize;
                    // Whole numbers often tie; a drift makes some finalists profitable.
                    let drift = rand() - 0.5;
                    let nets: Vec<f64> = (0..len)
                        .map(|_| ((rand() - 0.5 + drift) * 6.0).round())
                        .collect();
                    finalist((id * 7 % 40) as u32, &nets)
                })
                .collect();
            let (at, n) = ((rand() * 250.0) as i64, 1 + (rand() * 8.0) as usize);
            let all: Vec<(usize, f64, f64)> = finalists
                .iter()
                .enumerate()
                .filter_map(|(k, f)| {
                    let known = f.trades.partition_point(|t| t.exit_ms <= at);
                    (known >= n).then(|| {
                        let nets: Vec<f64> = (known - n..known).map(|i| f.net(i)).collect();
                        let (mut sum, mut peak, mut drop) = (0.0f64, 0.0f64, 0.0f64);
                        for v in nets {
                            sum += v;
                            peak = peak.max(sum);
                            drop = drop.max(peak - sum);
                        }
                        (k, sum, sum / drop.max(2.0))
                    })
                })
                .collect();
            let expected = all.iter().min_by(|a, b| {
                b.2.total_cmp(&a.2)
                    .then(b.1.total_cmp(&a.1))
                    .then(finalists[a.0].id.cmp(&finalists[b.0].id))
            });
            let got = pick(&finalists, at, n, 2.0);
            assert_eq!(
                got.map(|p| p.finalist),
                expected.map(|e| e.0),
                "round {round}"
            );
        }
    }

    #[test]
    fn losing_finalists_are_still_compared() {
        let p = pick(
            &[finalist(1, &[-3.0, -3.0]), finalist(2, &[-1.0, -1.0])],
            100,
            2,
            1.0,
        )
        .unwrap();
        // -6 / 6 = -1 and -2 / 2 = -1: same score, higher profit wins.
        assert_eq!(p.finalist, 1);
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
        };
        let f = finalist(0, &[1.0, 1.0, 1.0, -1.0, 2.0, 2.0, 2.0]);
        let mut chain = Chain {
            n: 3,
            m: 2,
            at: 26,
            done: false,
            windows: Vec::new(),
            trades: Vec::new(),
            stop_reason: String::new(),
        };
        // At 26 three trades are known; the 4th was entered at 30, the 5th at 40.
        advance(&mut chain, &[f], 40, 1.0, &space);
        assert_eq!(chain.windows, vec![1.0]);
        assert_eq!(chain.at, 45);
        assert_eq!(
            (chain.trades[0].entry_ms, chain.trades[1].exit_ms),
            (30, 45)
        );
    }
}
