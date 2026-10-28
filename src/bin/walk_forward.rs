use std::{
    fs,
    ops::Range,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Months, NaiveDate, Utc};
use dynamic_trader::{
    backtest::{Fill, Run, run_backtest},
    config::AppConfig,
    exits::Exits,
    filters::{Filter, FilteredCross},
    grid::{
        Indicators, exit_settings, exits_label, filters, parse_groups, parse_lengths,
        parse_numbers, parse_options,
    },
    okx_history::{self, Candle},
    rsi_cross::RsiCrossParams,
    scan::{self, Costs, Market, Outcome, Record, STEP_BTC},
    strategy::BarSignals,
    tape::{CloseRanges, Tape},
};
use serde_json::{Value, json};

const USAGE: &str = "\
Walk-forward test of the RSI cross strategy on every OKX BTC-USDT-SWAP trade.

The period is cut into calendar months. For every month after the first few, the strategy is
chosen using only the months before it (the training window), then traded on that month, which
played no part in the choice. Chaining these unseen months gives the profit a trader would have
made by re-choosing the settings every month, without knowing the future.

On each training window every combination (RSI pair × filter × take-profit / stop-loss setting,
as in rsi_combos) is scored by its profit after fees. Combinations with fewer than --min-trades
closed trades, or with orders refused for lack of cash, are skipped. The best one, and the
average of the --top-k best, are then traded on the next month, starting flat with the full
balance, with every indicator warmed up on all earlier candles. A position still open at the end
of a month is counted as closed at the last price with the taker fee.

Fixed candidates are also traded on every month, and every month is compared with simply buying
the same amount of BTC at the month's first price and selling at its last (both with the taker
fee). The last window's pick is rerun through NautilusTrader to confirm every fill.

Usage: cargo run --release --bin walk_forward -- [options]

  --start <YYYY-MM-DD>     First day, the first of a month (default: 2025-10-01)
  --end <YYYY-MM-DD>       Last day, exclusive, the first of a month (default: 2026-10-01)
  --timeframe <min>        Bar size in minutes (default: 240)
  --train-months <list>    Training window lengths in months, each tested on its own
                           (default: 2,4,6)
  --min-trades <n>         Closed trades a combination needs in training (default: 10)
  --top-k <n>              How many of the best in training to also average (default: 10)
  --lengths <list>         RSI lengths (default: 1-50,60-400/10)
  --trend <list>           Trend filter EMA lengths (default: none, e.g. none,50,100,200)
  --atr <list>             Volatility filter minimum ATR(14) in % (default: none, e.g. none,0.2,0.4,0.8)
  --stops <list>           Stop-loss distances in % (default: 0.5,1,1.5,2,3,5; `none` for no TP/SL)
  --ladders <list>         Take-profit ladders in R (default: 1,2,3,1/2,1/2/3,2/4/6)
  --shares <list>          Share per take-profit in % (default: 100,50/50,50/25/25)
  --breakeven <mode>       yes, no or both (default: both)
  --candidates <list>      Fixed settings to trade on every month, as fast/slow/trend/atr, e.g.
                           16/60/50/0.8 or 290/400/none/none (default: the earlier candidates
                           for 4h and 1h)
  --verify <n>             Last windows whose pick is rerun through NautilusTrader, on its
                           test month and its training window (default: 1, 0 to skip)
  --size-btc <btc>         Position size in BTC (default: 0.005)
  --balance-usdt <usdt>    Starting balance of every run (default: 500)
  --config <path>          Fee config (default: config/testnet.toml)
  --data <dir>             Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --out <dir>              Where to write the report (default: data/walkforward)";

/// The first day of the archives used before the walk-forward data, for marking months that no
/// earlier test saw.
const EARLIER_DATA_START: &str = "2026-03-16";

struct Args {
    start: NaiveDate,
    end: NaiveDate,
    minutes: usize,
    train_months: Vec<u32>,
    min_trades: u32,
    top_k: usize,
    lengths: Vec<usize>,
    trends: Vec<Option<usize>>,
    atrs: Vec<Option<f64>>,
    stops: Vec<f64>,
    ladders: Vec<Vec<f64>>,
    shares: Vec<Vec<f64>>,
    breakeven: Vec<bool>,
    candidates: Option<Vec<Candidate>>,
    verify: usize,
    size_btc: f64,
    balance_usdt: f64,
    config: PathBuf,
    data: PathBuf,
    out: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    fast: usize,
    slow: usize,
    filter: Filter,
}

impl Candidate {
    fn label(&self) -> String {
        format!(
            "RSI {}/{} · {} · No TP/SL",
            self.fast,
            self.slow,
            self.filter.label()
        )
    }
}

/// `16/60/50/0.8,290/400/none/none`
fn parse_candidates(text: &str) -> Result<Vec<Candidate>> {
    if text == "none" {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(|c| {
            let parts: Vec<&str> = c.trim().split('/').collect();
            ensure!(
                parts.len() == 4,
                "a candidate is fast/slow/trend/atr, got {c:?}"
            );
            let fast: usize = parts[0].parse()?;
            let slow: usize = parts[1].parse()?;
            ensure!(
                0 < fast && fast < slow,
                "candidate {c:?} needs 0 < fast < slow"
            );
            let trend = parse_options::<usize>(parts[2])?[0];
            let min_atr_pct = parse_options::<f64>(parts[3])?[0];
            Ok(Candidate {
                fast,
                slow,
                filter: Filter { trend, min_atr_pct },
            })
        })
        .collect()
}

fn default_candidates(minutes: usize) -> Vec<Candidate> {
    let text = match minutes {
        240 => "16/60/50/0.8",
        60 => "290/400/none/none,390/400/100/none",
        _ => "none",
    };
    parse_candidates(text).unwrap()
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        start: "2025-10-01".parse()?,
        end: "2026-10-01".parse()?,
        minutes: 240,
        train_months: vec![2, 4, 6],
        min_trades: 10,
        top_k: 10,
        lengths: parse_lengths("1-50,60-400/10")?,
        trends: parse_options("none")?,
        atrs: parse_options("none")?,
        stops: parse_numbers("0.5,1,1.5,2,3,5")?,
        ladders: parse_groups("1,2,3,1/2,1/2/3,2/4/6")?,
        shares: parse_groups("100,50/50,50/25/25")?,
        breakeven: vec![false, true],
        candidates: None,
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
            "--train-months" => {
                args.train_months = value
                    .split(',')
                    .map(|v| v.trim().parse())
                    .collect::<Result<_, _>>()
                    .with_context(bad)?
            }
            "--min-trades" => args.min_trades = value.parse().with_context(bad)?,
            "--top-k" => args.top_k = value.parse().with_context(bad)?,
            "--lengths" => args.lengths = parse_lengths(&value).with_context(bad)?,
            "--trend" => args.trends = parse_options(&value).with_context(bad)?,
            "--atr" => args.atrs = parse_options(&value).with_context(bad)?,
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
            "--candidates" => args.candidates = Some(parse_candidates(&value).with_context(bad)?),
            "--verify" => args.verify = value.parse().with_context(bad)?,
            "--size-btc" => args.size_btc = value.parse().with_context(bad)?,
            "--balance-usdt" => args.balance_usdt = value.parse().with_context(bad)?,
            "--config" => args.config = value.into(),
            "--data" => args.data = value.into(),
            "--out" => args.out = value.into(),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(
        args.start.day() == 1 && args.end.day() == 1,
        "--start and --end must be the first day of a month"
    );
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
        !args.train_months.is_empty() && args.train_months.iter().all(|m| *m > 0),
        "--train-months must be positive"
    );
    ensure!(args.top_k > 0, "--top-k must be positive");
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

/// Training score of one combination on one window, kept small: there are about a million
/// combinations per window.
#[derive(Clone, Copy, Default)]
struct Score {
    total: f32,
    closed: u32,
    refused: bool,
}

/// One walk-forward step: train on `train`, trade on the month `test`.
struct Window {
    months: u32,
    test_month: usize,
    train: Range<usize>,
    test: Range<usize>,
}

struct Ctx<'a> {
    args: &'a Args,
    candles: &'a [Candle],
    market: Market<'a>,
    ind: &'a Indicators,
    filters: &'a [Filter],
    exits: &'a [Option<Exits>],
    costs: Costs,
}

impl Ctx<'_> {
    fn setting(&self, k: usize) -> (&Filter, Option<&Exits>) {
        let n = self.exits.len();
        (&self.filters[k / n], self.exits[k % n].as_ref())
    }

    fn label(&self, k: usize) -> String {
        let n = self.exits.len();
        format!(
            "{} · {}",
            self.filters[k / n].label(),
            exits_label(&self.exits[k % n])
        )
    }

    /// Profit of `o` after also paying the taker fee to close what is still open at the last
    /// close of `bars`, and the size that was still open.
    fn net(&self, o: &Outcome, record: &Record, bars: &Range<usize>) -> (f64, f64) {
        let open_btc = record
            .trades
            .last()
            .filter(|t| t.open)
            .and_then(|t| t.exits.last())
            .map_or(0.0, |e| e.steps as f64 * STEP_BTC);
        let close = self.candles[bars.end - 1].close;
        (o.total() - open_btc * close * self.costs.taker, open_btc)
    }

    /// One combination over `bars`, with every fill.
    fn replay(
        &self,
        signals: &[(u32, i8)],
        exits: Option<&Exits>,
        ids: (u16, u16, u16),
        bars: &Range<usize>,
    ) -> (Outcome, Record, f64) {
        let mut record = Record::default();
        let o = scan::simulate(
            ids,
            signals,
            exits,
            &self.market,
            bars.clone(),
            self.costs,
            Some(&mut record),
        );
        let (net, _) = self.net(&o, &record, bars);
        (o, record, net)
    }

    fn combo_signals(&self, fast: usize, slow: usize, filter: &Filter) -> Vec<(u32, i8)> {
        let (i, j) = (self.ind.index(fast).unwrap(), self.ind.index(slow).unwrap());
        self.ind
            .filtered(&self.ind.signals(i, j), filter, self.candles)
    }

    fn buy_and_hold(&self, bars: &Range<usize>) -> f64 {
        let (first, last) = (
            self.candles[bars.start].open,
            self.candles[bars.end - 1].close,
        );
        self.args.size_btc * (last - first) - self.args.size_btc * (first + last) * self.costs.taker
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let config = AppConfig::load(&args.config)?;
    let taker = config.venues.okx.taker_fee_bps / 10_000.0;
    let maker = config.venues.okx.maker_fee_bps / 10_000.0;
    let bar_ms = args.minutes as i64 * 60_000;

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
    let months: Vec<NaiveDate> = (0..)
        .map(|n| args.start + Months::new(n))
        .take_while(|m| *m <= args.end)
        .collect();
    let month_bars: Vec<Range<usize>> = months
        .windows(2)
        .map(|w| {
            candles.partition_point(|c| c.time_ms < ms(w[0]))
                ..candles.partition_point(|c| c.time_ms < ms(w[1]))
        })
        .collect();
    ensure!(
        month_bars.iter().all(|r| r.len() > 1),
        "every month needs candles"
    );
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

    let filters = filters(&args.trends, &args.atrs);
    let exits = exit_settings(
        &args.stops,
        &args.ladders,
        &args.shares,
        &args.breakeven,
        (args.size_btc * 10_000.0).round() as i64,
    )?;
    let n_settings = filters.len() * exits.len();
    ensure!(n_settings <= usize::from(u16::MAX), "too many settings");
    let pairs: Vec<(usize, usize)> = (0..args.lengths.len())
        .flat_map(|i| (i + 1..args.lengths.len()).map(move |j| (i, j)))
        .collect();
    let n_combos = pairs.len() * n_settings;
    let candidates = args
        .candidates
        .clone()
        .unwrap_or_else(|| default_candidates(args.minutes));
    let mut lengths = args.lengths.clone();
    lengths.extend(candidates.iter().flat_map(|c| [c.fast, c.slow]));
    lengths.sort_unstable();
    lengths.dedup();
    let mut all_filters = filters.clone();
    all_filters.extend(candidates.iter().map(|c| c.filter));
    let ind = Indicators::new(&candles, &lengths, &all_filters);
    let closes = CloseRanges::new(&candles.iter().map(|c| c.close).collect::<Vec<_>>());
    let costs = Costs {
        steps: (args.size_btc * 10_000.0).round() as i64,
        taker,
        maker,
        balance: args.balance_usdt,
    };
    let ctx = Ctx {
        args: &args,
        candles: &candles,
        market: Market {
            candles: &candles,
            tape: &tape,
            bar_start: &tape.bar_start,
            closes: &closes,
        },
        ind: &ind,
        filters: &filters,
        exits: &exits,
        costs,
    };

    let windows: Vec<Window> = args
        .train_months
        .iter()
        .flat_map(|&n| (n as usize..month_bars.len()).map(move |m| (n, m)))
        .map(|(n, m)| Window {
            months: n,
            test_month: m,
            train: month_bars[m - n as usize].start..month_bars[m - 1].end,
            test: month_bars[m].clone(),
        })
        .collect();
    ensure!(
        !windows.is_empty(),
        "the period is too short for the training windows"
    );
    println!(
        "{} combinations ({} RSI pairs × {} filters × {} TP/SL settings) scored on {} training windows ({} months of training each); at least {} closed trades",
        n_combos,
        pairs.len(),
        filters.len(),
        exits.len(),
        windows.len(),
        args.train_months
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
            .join(" / "),
        args.min_trades
    );

    // Scores of every combination on every window, `[window][pair * n_settings + setting]`.
    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut per_pair: Vec<(usize, Vec<Score>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let p = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(i, j)) = pairs.get(p) else {
                            break out;
                        };
                        let raw = ind.signals(i, j);
                        let mut scores = vec![Score::default(); windows.len() * n_settings];
                        for (f, filter) in filters.iter().enumerate() {
                            let signals = ind.filtered(&raw, filter, &candles);
                            for (e, exit) in exits.iter().enumerate() {
                                let k = f * exits.len() + e;
                                let ids =
                                    (args.lengths[i] as u16, args.lengths[j] as u16, k as u16);
                                for (w, window) in windows.iter().enumerate() {
                                    let o = scan::simulate(
                                        ids,
                                        &signals,
                                        exit.as_ref(),
                                        &ctx.market,
                                        window.train.clone(),
                                        costs,
                                        None,
                                    );
                                    scores[w * n_settings + k] = Score {
                                        total: o.total() as f32,
                                        closed: o.closed_trades,
                                        refused: o.refused > 0,
                                    };
                                }
                            }
                        }
                        out.push((p, scores));
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if d % 200 == 0 {
                            let secs = started.elapsed().as_secs_f64();
                            println!(
                                "Scored pair {d} of {} on every window, about {:.0}s left",
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
    println!(
        "Scored every combination on every training window in {:.0}s",
        started.elapsed().as_secs_f64()
    );

    let earlier_start = ms(EARLIER_DATA_START.parse()?);
    let month_json: Vec<Value> = month_bars
        .iter()
        .enumerate()
        .map(|(m, bars)| {
            json!({
                "month": months[m].format("%Y-%m").to_string(),
                "open": candles[bars.start].open,
                "close": candles[bars.end - 1].close,
                "buy_and_hold": round2(ctx.buy_and_hold(bars)),
                "unseen_before": ms(months[m + 1]) <= earlier_start,
            })
        })
        .collect();

    let first_length: Vec<usize> = (0..windows.len())
        .filter(|&w| windows[w].months == args.train_months[0])
        .collect();
    let to_verify = &first_length[first_length.len().saturating_sub(args.verify)..];
    let mut window_json = Vec::new();
    let mut verify_picks: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (w, window) in windows.iter().enumerate() {
        let mut eligible: Vec<(usize, Score)> = (0..n_combos)
            .map(|c| {
                (
                    c,
                    per_pair[c / n_settings].1[w * n_settings + c % n_settings],
                )
            })
            .filter(|(_, s)| s.closed >= args.min_trades && !s.refused)
            .collect();
        let n_eligible = eligible.len();
        let profitable = eligible.iter().filter(|(_, s)| s.total > 0.0).count();
        let k = args.top_k.min(eligible.len());
        if k > 0 {
            eligible.select_nth_unstable_by(k - 1, |a, b| b.1.total.total_cmp(&a.1.total));
        }
        eligible.truncate(k);
        eligible.sort_by(|a, b| b.1.total.total_cmp(&a.1.total));
        let picks: Vec<Value> = eligible
            .iter()
            .map(|&(c, _)| {
                let (i, j) = pairs[c / n_settings];
                let setting = c % n_settings;
                let (filter, exit) = ctx.setting(setting);
                let (fast, slow) = (args.lengths[i], args.lengths[j]);
                let signals = ctx.combo_signals(fast, slow, filter);
                let ids = (fast as u16, slow as u16, setting as u16);
                let (train, _, train_net) = ctx.replay(&signals, exit, ids, &window.train);
                let (test, record, test_net) = ctx.replay(&signals, exit, ids, &window.test);
                json!({
                    "fast": fast,
                    "slow": slow,
                    "setting": setting,
                    "label": ctx.label(setting),
                    "train_total": round2(train.total()),
                    "train_net": round2(train_net),
                    "train_closed": train.closed_trades,
                    "test_total": round2(test.total()),
                    "test_net": round2(test_net),
                    "test_orders": test.orders,
                    "test_closed": test.closed_trades,
                    "test_wins": test.wins,
                    "test_fees": round2(test.fees),
                    "test_trades": record.trades.len(),
                })
            })
            .collect();
        if to_verify.contains(&w)
            && let Some(&(c, _)) = eligible.first()
        {
            let (i, j) = pairs[c / n_settings];
            verify_picks.push((w, args.lengths[i], args.lengths[j], c % n_settings));
        }
        let nets: Vec<f64> = picks
            .iter()
            .map(|p| p["test_net"].as_f64().unwrap())
            .collect();
        let avg = if nets.is_empty() {
            0.0
        } else {
            nets.iter().sum::<f64>() / nets.len() as f64
        };
        let bh = ctx.buy_and_hold(&window.test);
        match picks.first() {
            Some(best) => println!(
                "Train {} month(s) to {}, trade {}: best RSI {}/{} {} made {:+.2} in training, {:+.2} USDT in the test month; top {} average {:+.2}; buy-and-hold {:+.2} ({} of {} eligible made money in training)",
                window.months,
                months[window.test_month],
                months[window.test_month].format("%Y-%m"),
                best["fast"],
                best["slow"],
                best["label"].as_str().unwrap(),
                best["train_net"].as_f64().unwrap(),
                best["test_net"].as_f64().unwrap(),
                nets.len(),
                avg,
                bh,
                profitable,
                n_eligible,
            ),
            None => println!(
                "Train {} month(s), trade {}: no combination had {} closed trades",
                window.months,
                months[window.test_month].format("%Y-%m"),
                args.min_trades
            ),
        }
        window_json.push(json!({
            "train_months": window.months,
            "test_month": months[window.test_month].format("%Y-%m").to_string(),
            "train_from": months[window.test_month - window.months as usize].to_string(),
            "eligible": n_eligible,
            "profitable_in_training": profitable,
            "picks": picks,
            "top_k_average": round2(avg),
            "buy_and_hold": round2(bh),
        }));
    }

    let summaries: Vec<Value> = args
        .train_months
        .iter()
        .map(|&n| {
            let ws: Vec<&Value> = window_json.iter().filter(|w| w["train_months"] == n).collect();
            let best: Vec<f64> = ws.iter().map(|w| w["picks"].get(0).map_or(0.0, |p| p["test_net"].as_f64().unwrap())).collect();
            let avg: Vec<f64> = ws.iter().map(|w| w["top_k_average"].as_f64().unwrap()).collect();
            let bh: Vec<f64> = ws.iter().map(|w| w["buy_and_hold"].as_f64().unwrap()).collect();
            let summary = json!({
                "train_months": n,
                "windows": ws.len(),
                "best_total": round2(best.iter().sum()),
                "best_positive": best.iter().filter(|v| **v > 0.0).count(),
                "top_k_total": round2(avg.iter().sum()),
                "top_k_positive": avg.iter().filter(|v| **v > 0.0).count(),
                "buy_and_hold_total": round2(bh.iter().sum()),
                "best_in_falling": round2(best.iter().zip(&bh).filter(|(_, b)| **b < 0.0).map(|(v, _)| v).sum()),
                "falling_months": bh.iter().filter(|b| **b < 0.0).count(),
            });
            println!(
                "\nWalk-forward with {n} month(s) of training, {} test months: best pick {:+.2} USDT ({} of {} months positive), top {} average {:+.2} USDT ({} positive); buy-and-hold {:+.2} USDT",
                ws.len(),
                summary["best_total"].as_f64().unwrap(),
                summary["best_positive"],
                ws.len(),
                args.top_k,
                summary["top_k_total"].as_f64().unwrap(),
                summary["top_k_positive"],
                summary["buy_and_hold_total"].as_f64().unwrap()
            );
            summary
        })
        .collect();

    let candidate_json: Vec<Value> = candidates
        .iter()
        .map(|c| {
            let signals = ctx.combo_signals(c.fast, c.slow, &c.filter);
            let per_month: Vec<Value> = month_bars
                .iter()
                .enumerate()
                .map(|(m, bars)| {
                    let (o, _, net) = ctx.replay(&signals, None, (c.fast as u16, c.slow as u16, 0), bars);
                    json!({ "month": months[m].format("%Y-%m").to_string(), "net": round2(net), "orders": o.orders, "closed": o.closed_trades, "wins": o.wins, "fees": round2(o.fees), "refused": o.refused })
                })
                .collect();
            let nets: Vec<f64> = per_month.iter().map(|m| m["net"].as_f64().unwrap()).collect();
            let unseen: f64 = nets.iter().zip(&month_json).filter(|(_, m)| m["unseen_before"] == true).map(|(v, _)| v).sum();
            println!(
                "Candidate {}: {:+.2} USDT over all {} months ({} positive), {:+.2} USDT in the months before {EARLIER_DATA_START}; by month {}",
                c.label(),
                nets.iter().sum::<f64>(),
                nets.len(),
                nets.iter().filter(|v| **v > 0.0).count(),
                unseen,
                nets.iter().map(|v| format!("{v:+.0}")).collect::<Vec<_>>().join(" ")
            );
            json!({ "label": c.label(), "months": per_month, "total": round2(nets.iter().sum()), "unseen_total": round2(unseen) })
        })
        .collect();

    let mut verify_json = Vec::new();
    for &(w, fast, slow, setting) in &verify_picks {
        let window = &windows[w];
        for (part, bars, start, end) in [
            (
                "test month",
                &window.test,
                months[window.test_month],
                months[window.test_month + 1],
            ),
            (
                "training window",
                &window.train,
                months[window.test_month - window.months as usize],
                months[window.test_month],
            ),
        ] {
            let v = verify(&ctx, &config, fast, slow, setting, part, bars, start, end)?;
            verify_json.push(v);
        }
    }

    let report = json!({
        "meta": {
            "instrument": okx_history::INSTRUMENT,
            "start": args.start.to_string(),
            "end": args.end.to_string(),
            "timeframe_minutes": args.minutes,
            "train_months": args.train_months,
            "min_trades": args.min_trades,
            "top_k": args.top_k,
            "combinations": n_combos,
            "pairs": pairs.len(),
            "filters": filters.len(),
            "exit_settings": exits.len(),
            "lengths": format!("{}..{}", args.lengths[0], args.lengths[args.lengths.len() - 1]),
            "size_btc": args.size_btc,
            "balance_usdt": args.balance_usdt,
            "taker_fee_bps": config.venues.okx.taker_fee_bps,
            "maker_fee_bps": config.venues.okx.maker_fee_bps,
            "earlier_data_start": EARLIER_DATA_START,
            "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
        },
        "months": month_json,
        "windows": window_json,
        "summaries": summaries,
        "candidates": candidate_json,
        "verify": verify_json,
    });
    fs::create_dir_all(&args.out)?;
    let base = args.out.join(format!(
        "walk_forward_{}m_{}_{}",
        args.minutes, args.start, args.end
    ));
    fs::write(
        base.with_extension("json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    let html = REPORT_HTML
        .replace("/*__TABLE_SORT__*/", TABLE_SORT)
        .replace(
            "/*__REPORT__*/null",
            &serde_json::to_string(&report)?.replace("</", "<\\/"),
        );
    fs::write(base.with_extension("html"), html)?;
    println!(
        "\nReport (open in a browser): {}",
        base.with_extension("html").display()
    );
    if verify_json.iter().any(|v| v["ok"] == false) {
        bail!("NautilusTrader and the scanner disagree, see above");
    }
    Ok(())
}

/// Reruns one combination on `bars` (from `start` to `end`) through NautilusTrader, with the
/// indicators warmed up on every earlier candle, and compares every fill with the scanner.
#[allow(clippy::too_many_arguments)]
fn verify(
    ctx: &Ctx,
    config: &AppConfig,
    fast: usize,
    slow: usize,
    setting: usize,
    part: &str,
    bars: &Range<usize>,
    start: NaiveDate,
    end: NaiveDate,
) -> Result<Value> {
    let (filter, exits) = ctx.setting(setting);
    let name = format!(
        "RSI {fast}/{slow} {} ({part} {start} to {end})",
        ctx.label(setting)
    );
    let run = Run {
        config,
        data: &ctx.args.data,
        start,
        end,
        bar_minutes: ctx.args.minutes as u64,
        size_btc: ctx.args.size_btc,
        balance_usdt: ctx.args.balance_usdt,
    };
    let mut signals = FilteredCross::new(RsiCrossParams { fast, slow }, *filter);
    for bar in &ctx.candles[..bars.start] {
        signals.on_bar(bar);
    }
    let started = Instant::now();
    println!("NautilusTrader check: {name}");
    let mut shown = 0;
    let (result, mut theirs) =
        run_backtest(&run, Box::new(signals), exits.cloned(), |done, total| {
            if done * 4 / total.max(1) > shown {
                shown = done * 4 / total.max(1);
                println!("  archive {done} of {total}");
            }
        })?;
    let pnl = result
        .stats_pnls
        .get("USDT")
        .and_then(|s| s.get("PnL (total)"))
        .copied()
        .unwrap_or(0.0);
    let (o, record, _) = ctx.replay(
        &ctx.combo_signals(fast, slow, filter),
        exits,
        (fast as u16, slow as u16, setting as u16),
        bars,
    );
    let mut ours = record.fills;
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
    // Nautilus' "PnL (total)" counts closed parts and fees, not the open position.
    let closed = o.realized - o.fees;
    let ok = bad.is_empty() && (closed - pnl).abs() < 1e-6;
    println!(
        "  {} fills, {} differ (time, kind, side, size, price, fee); closed PnL Nautilus {pnl:.6} / scanner {closed:.6} USDT ({:.0}s)",
        theirs.len(),
        bad.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(
        json!({ "pair": name, "fills": theirs.len(), "differ": bad.len(), "nautilus_pnl": pnl, "scanner_pnl": closed, "ok": ok, "seconds": started.elapsed().as_secs_f64() }),
    )
}

const REPORT_HTML: &str = include_str!("walk_forward_report.html");
const TABLE_SORT: &str = include_str!("table_sort.js");

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
