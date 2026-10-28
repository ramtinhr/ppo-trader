use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, Write},
    net::{TcpListener, TcpStream},
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
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
Tests every pair of RSI lengths (fast < slow) on every OKX BTC-USDT-SWAP trade: long when the
fast RSI crosses up through the slow RSI, short when it crosses down; each signal reverses the
position and fills at the next bar's open with the taker fee.

Every pair is tested with every take-profit / stop-loss setting: a stop-loss for the whole
position, and one or more take-profits at multiples (R) of the stop distance, each closing its
share, optionally moving the stop to the entry price (break-even) once the first take-profit
fills. Take-profits pay the maker fee, stops and signals the taker fee. Optional filters (off by
default): a trend filter (long only above the EMA, short only below) and a volatility filter
(only when the ATR is at least a % of the price); a cross a filter rejects only closes the
position it is against.

The period is split in two: combinations are ranked on the training part only, then each one is
also run on the test part, which played no role in picking it, to see whether it holds up. The
best combinations are rerun through NautilusTrader on both parts to confirm every fill.

Usage: cargo run --release --bin rsi_combos -- [options]

  --start <YYYY-MM-DD>     First UTC day, inclusive (default: first day of last month)
  --end <YYYY-MM-DD>       Last UTC day, exclusive (default: first day of this month)
  --test-from <YYYY-MM-DD> First day of the test part (default: two thirds into the period)
  --timeframe <min>        Bar size in minutes (default: 1)
  --lengths <list>         RSI lengths: numbers, ranges and stepped ranges
                           (default: 1-50,60-400/10, i.e. 1..50 then 60, 70, ... 400)
  --trend <list>           EMA lengths of the trend filter, `none` for no trend filter
                           (default: none, e.g. none,50,100,200)
  --atr <list>             Minimum ATR(14) in % of the price, `none` for no volatility filter
                           (default: none, e.g. none,0.2,0.4,0.8)
  --stops <list>           Stop-loss distances in % of the entry price, or `none` to test without
                           exits only (default: 0.5,1,1.5,2,3,5)
  --ladders <list>         Take-profit ladders in R, i.e. multiples of the stop distance
                           (default: 1,2,3,1/2,1/2/3,2/4/6)
  --shares <list>          Share of the position per take-profit, in %; each ladder uses the
                           splits with as many parts (default: 100,50/50,50/25/25)
  --breakeven <mode>       Move the stop to the entry after the first of several take-profits:
                           yes, no or both (default: both)
  --size-btc <btc>         Position size in BTC, in 0.0001 steps (default: 0.005)
  --balance-usdt <usdt>    Starting account balance in USDT, for each part (default: 500)
  --top <n>                Combinations to print (default: 20)
  --verify <n>             Combinations to rerun through Nautilus on both parts: the best that
                           made money in both, else the best in training (default: 1, 0 to skip)
  --report <n>             Best combinations with every trade in the HTML report file (default: 50)
  --serve [port]           Also open a live report at http://127.0.0.1:<port> (default 8080):
                           watch the run's progress, then open any combination
  --config <path>          Fee and risk config (default: config/testnet.toml)
  --data <dir>             Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --out <dir>              Where to write the CSV of every combination (default: data/combos)";

struct Args {
    start: NaiveDate,
    end: NaiveDate,
    test_from: Option<NaiveDate>,
    minutes: usize,
    lengths: Vec<usize>,
    trends: Vec<Option<usize>>,
    atrs: Vec<Option<f64>>,
    stops: Vec<f64>,
    ladders: Vec<Vec<f64>>,
    shares: Vec<Vec<f64>>,
    breakeven: Vec<bool>,
    size_btc: f64,
    balance_usdt: f64,
    top: usize,
    verify: usize,
    report: usize,
    serve: Option<u16>,
    config: PathBuf,
    data: PathBuf,
    out: PathBuf,
}

fn parse_args() -> Result<Args> {
    let this_month = Utc::now().date_naive().with_day(1).unwrap();
    let mut args = Args {
        start: this_month - Months::new(1),
        end: this_month,
        test_from: None,
        minutes: 1,
        lengths: parse_lengths("1-50,60-400/10")?,
        trends: parse_options("none")?,
        atrs: parse_options("none")?,
        stops: parse_numbers("0.5,1,1.5,2,3,5")?,
        ladders: parse_groups("1,2,3,1/2,1/2/3,2/4/6")?,
        shares: parse_groups("100,50/50,50/25/25")?,
        breakeven: vec![false, true],
        size_btc: 0.005,
        balance_usdt: 500.0,
        top: 20,
        verify: 1,
        report: 50,
        serve: None,
        config: "config/testnet.toml".into(),
        data: "data/okx/BTC-USDT-SWAP/archives".into(),
        out: "data/combos".into(),
    };
    let mut raw = std::env::args().skip(1).peekable();
    while let Some(flag) = raw.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        if flag == "--serve" {
            let port = raw.next_if(|v| !v.starts_with("--"));
            args.serve = Some(
                port.map_or(Ok(8080), |p| p.parse())
                    .with_context(|| "--serve takes a port number")?,
            );
            continue;
        }
        let value = raw
            .next()
            .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
        let bad = || format!("bad value for {flag}: {value}");
        match flag.as_str() {
            "--start" => args.start = value.parse().with_context(bad)?,
            "--end" => args.end = value.parse().with_context(bad)?,
            "--test-from" => args.test_from = Some(value.parse().with_context(bad)?),
            "--timeframe" => args.minutes = value.parse().with_context(bad)?,
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
            "--size-btc" => args.size_btc = value.parse().with_context(bad)?,
            "--balance-usdt" => args.balance_usdt = value.parse().with_context(bad)?,
            "--top" => args.top = value.parse().with_context(bad)?,
            "--verify" => args.verify = value.parse().with_context(bad)?,
            "--report" => args.report = value.parse().with_context(bad)?,
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
        args.trends.iter().flatten().all(|n| *n > 0),
        "--trend lengths must be positive"
    );
    ensure!(
        args.atrs.iter().flatten().all(|a| *a > 0.0),
        "--atr minimums must be positive"
    );
    ensure!(
        !args.trends.is_empty() && !args.atrs.is_empty(),
        "--trend and --atr need at least one value"
    );
    let test_from = *args.test_from.get_or_insert_with(|| {
        let days = (args.end - args.start).num_days();
        args.start + chrono::Days::new((days * 2 / 3).max(1) as u64)
    });
    ensure!(
        args.start < test_from && test_from < args.end,
        "--test-from must be inside the period"
    );
    let steps = args.size_btc * 10_000.0;
    ensure!(
        args.size_btc >= 0.0001 && (steps.round() - steps).abs() < 1e-6,
        "--size-btc must be positive and use OKX's 0.0001 BTC step"
    );
    ensure!(args.balance_usdt > 0.0, "--balance-usdt must be positive");
    Ok(args)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Train,
    Test,
}

impl Period {
    fn name(self) -> &'static str {
        match self {
            Period::Train => "training",
            Period::Test => "test",
        }
    }
}

/// Everything a replay needs, shared with the web server.
struct Data {
    candles: Vec<Candle>,
    tape: Tape,
    closes: CloseRanges,
    /// First bar of the test part.
    split: usize,
    ind: Indicators,
    filters: Vec<Filter>,
    exits: Vec<Option<Exits>>,
    costs: Costs,
}

impl Data {
    fn market(&self) -> Market<'_> {
        Market {
            candles: &self.candles,
            tape: &self.tape,
            bar_start: &self.tape.bar_start,
            closes: &self.closes,
        }
    }

    /// Setting `k`: its filter and its exits.
    fn setting(&self, k: usize) -> Option<(&Filter, Option<&Exits>)> {
        let n = self.exits.len();
        Some((self.filters.get(k / n)?, self.exits.get(k % n)?.as_ref()))
    }

    fn label(&self, k: usize) -> String {
        let n = self.exits.len();
        let exits = exits_label(&self.exits[k % n]);
        if self.filters.len() == 1 && self.filters[0] == Filter::default() {
            return exits;
        }
        format!("{} · {}", self.filters[k / n].label(), exits)
    }

    fn bars(&self, period: Period) -> Range<usize> {
        match period {
            Period::Train => 0..self.split,
            Period::Test => self.split..self.candles.len(),
        }
    }

    /// One combination in one period, with every fill and trade.
    fn replay(
        &self,
        fast: usize,
        slow: usize,
        k: usize,
        period: Period,
    ) -> Option<(Outcome, Record)> {
        let (i, j) = (self.ind.index(fast)?, self.ind.index(slow)?);
        let (filter, exits) = self.setting(k)?;
        let signals = self
            .ind
            .filtered(&self.ind.signals(i, j), filter, &self.candles);
        let mut record = Record::default();
        let ids = (fast as u16, slow as u16, k as u16);
        let o = scan::simulate(
            ids,
            &signals,
            exits,
            &self.market(),
            self.bars(period),
            self.costs,
            Some(&mut record),
        );
        Some((o, record))
    }

    fn details(&self, fast: usize, slow: usize, k: usize) -> Option<Value> {
        let part = |period| {
            let (o, record) = self.replay(fast, slow, k, period)?;
            Some(json!({ "row": row(&o), "trades": trades_json(&record) }))
        };
        Some(json!({ "train": part(Period::Train)?, "test": part(Period::Test)? }))
    }
}

/// Progress and results shared with the web server.
#[derive(Default)]
struct Live {
    status: Mutex<Status>,
    computed: OnceLock<Computed>,
}

#[derive(Default, serde::Serialize)]
struct Status {
    stage: String,
    message: String,
    done: usize,
    total: usize,
    log: Vec<String>,
    verify: Vec<Value>,
    ready: bool,
    finished: bool,
    error: Option<String>,
}

struct Computed {
    report: String,
    data: Arc<Data>,
}

impl Live {
    fn log(&self, line: impl Into<String>) {
        let line = line.into();
        println!("{line}");
        self.status.lock().unwrap().log.push(line);
    }

    fn stage(&self, stage: &str, message: impl Into<String>, done: usize, total: usize) {
        let mut s = self.status.lock().unwrap();
        s.stage = stage.into();
        s.message = message.into();
        s.done = done;
        s.total = total;
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let live = Arc::new(Live::default());
    if let Some(port) = args.serve {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .with_context(|| format!("port {port} is busy; try --serve {}", port + 1))?;
        println!("Live report: http://127.0.0.1:{port}  (open it in a browser; Ctrl+C to stop)\n");
        let live = live.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let live = live.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve(stream, &live) {
                        eprintln!("web request failed: {e:#}");
                    }
                });
            }
        });
    }
    let result = work(&args, &live);
    {
        let mut s = live.status.lock().unwrap();
        s.finished = true;
        match &result {
            Ok(()) => s.stage = "done".into(),
            Err(e) => {
                s.stage = "error".into();
                s.error = Some(format!("{e:#}"));
            }
        }
    }
    if let Some(port) = args.serve {
        if let Err(e) = &result {
            eprintln!("Error: {e:#}");
        }
        println!(
            "\nFinished. The live report stays open at http://127.0.0.1:{port} until you press Ctrl+C."
        );
        loop {
            std::thread::park();
        }
    }
    result
}

/// One combination: pair index and setting.
type Combo = (u32, u16);

fn ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn work(args: &Args, live: &Live) -> Result<()> {
    let config = AppConfig::load(&args.config)?;
    let taker = config.venues.okx.taker_fee_bps / 10_000.0;
    let maker = config.venues.okx.maker_fee_bps / 10_000.0;
    let bar_ms = args.minutes as i64 * 60_000;
    let test_from = args.test_from.unwrap();

    let started = Instant::now();
    let (candles, tape) = Tape::load(
        &args.data,
        bar_ms,
        ms(args.start),
        ms(args.end),
        |done, total| {
            live.stage(
                "candles",
                format!(
                    "Reading every OKX trade: archive {} of {total}",
                    (done + 1).min(total)
                ),
                done,
                total,
            )
        },
    )?;
    ensure!(
        candles.len() > 2,
        "no candles between {} and {}",
        args.start,
        args.end
    );
    let split = candles.partition_point(|c| c.time_ms < ms(test_from));
    ensure!(
        split > 1 && split + 1 < candles.len(),
        "the training or test part has no candles"
    );
    let gaps = candles
        .windows(2)
        .filter(|w| w[1].time_ms - w[0].time_ms != bar_ms)
        .count();
    live.log(format!(
        "{} {}-minute candles from {} to {} (exclusive), built from every OKX trade in {:.0}s; {gaps} gaps without trades; {} price changes for TP/SL",
        candles.len(),
        args.minutes,
        args.start,
        args.end,
        started.elapsed().as_secs_f64(),
        tape.price.len()
    ));
    live.log(format!(
        "Training: {} to {} ({} bars). Test: {} to {} ({} bars), never used to pick combinations",
        args.start,
        test_from,
        split,
        test_from,
        args.end,
        candles.len() - split
    ));

    let filters = filters(&args.trends, &args.atrs);
    let exits = exit_settings(
        &args.stops,
        &args.ladders,
        &args.shares,
        &args.breakeven,
        (args.size_btc * 10_000.0).round() as i64,
    )?;
    let pairs: Vec<(usize, usize)> = (0..args.lengths.len())
        .flat_map(|i| (i + 1..args.lengths.len()).map(move |j| (i, j)))
        .collect();
    let n_settings = filters.len() * exits.len();
    live.log(format!(
        "{} RSI lengths ({}..{}), {} pairs ×{} {} TP/SL settings (incl. none) = {} combinations; {} BTC position, {} USDT balance, taker {} / maker {} bps",
        args.lengths.len(),
        args.lengths[0],
        args.lengths[args.lengths.len() - 1],
        pairs.len(),
        if filters.len() > 1 { format!(" {} filters ×", filters.len()) } else { String::new() },
        exits.len(),
        pairs.len() * n_settings,
        args.size_btc,
        args.balance_usdt,
        config.venues.okx.taker_fee_bps,
        config.venues.okx.maker_fee_bps
    ));
    ensure!(n_settings <= usize::from(u16::MAX), "too many settings");

    live.stage(
        "rsi",
        format!("Calculating {} RSI lines", args.lengths.len()),
        0,
        1,
    );
    let closes = CloseRanges::new(&candles.iter().map(|c| c.close).collect::<Vec<_>>());
    let ind = Indicators::new(&candles, &args.lengths, &filters);
    let costs = Costs {
        steps: (args.size_btc * 10_000.0).round() as i64,
        taker,
        maker,
        balance: args.balance_usdt,
    };
    let data = Arc::new(Data {
        candles,
        tape,
        closes,
        split,
        ind,
        filters,
        exits,
        costs,
    });

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    live.stage(
        "scan",
        format!("Testing {} combinations", pairs.len() * n_settings),
        0,
        pairs.len(),
    );
    let market = data.market();
    let (train_bars, test_bars) = (data.bars(Period::Train), data.bars(Period::Test));
    let mut per_pair: Vec<(usize, Vec<Outcome>, Vec<Outcome>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let p = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(i, j)) = pairs.get(p) else { break out };
                        let raw = data.ind.signals(i, j);
                        let (mut train, mut test) = (Vec::with_capacity(n_settings), Vec::with_capacity(n_settings));
                        for (f, filter) in data.filters.iter().enumerate() {
                            let signals = data.ind.filtered(&raw, filter, &data.candles);
                            for (e, exits) in data.exits.iter().enumerate() {
                                let ids = (args.lengths[i] as u16, args.lengths[j] as u16, (f * data.exits.len() + e) as u16);
                                train.push(scan::simulate(ids, &signals, exits.as_ref(), &market, train_bars.clone(), costs, None));
                                test.push(scan::simulate(ids, &signals, exits.as_ref(), &market, test_bars.clone(), costs, None));
                            }
                        }
                        out.push((p, train, test));
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if d % 10 == 0 {
                            let secs = started.elapsed().as_secs_f64();
                            let left = secs / d as f64 * (pairs.len() - d) as f64;
                            live.stage("scan", format!("Testing pair {d} of {} with every TP/SL setting, about {left:.0}s left", pairs.len()), d, pairs.len());
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
    per_pair.sort_by_key(|(p, _, _)| *p);
    let (train, test): (Vec<Vec<Outcome>>, Vec<Vec<Outcome>>) =
        per_pair.into_iter().map(|(_, a, b)| (a, b)).unzip();
    let total = pairs.len() * n_settings;
    live.log(format!(
        "Tested {total} combinations in both parts in {:.1}s",
        started.elapsed().as_secs_f64()
    ));

    let tr = |(p, k): Combo| &train[p as usize][k as usize];
    let te = |(p, k): Combo| &test[p as usize][k as usize];
    let mut ranked: Vec<Combo> = (0..pairs.len() as u32)
        .flat_map(|p| (0..n_settings as u16).map(move |k| (p, k)))
        .collect();
    ranked.sort_by(|a, b| tr(*b).total().total_cmp(&tr(*a).total()));
    let best: Vec<Combo> = (0..pairs.len() as u32)
        .map(|p| {
            (
                p,
                (0..n_settings as u16)
                    .max_by(|a, b| tr((p, *a)).total().total_cmp(&tr((p, *b)).total()))
                    .unwrap(),
            )
        })
        .collect();

    fs::create_dir_all(&args.out)?;
    let path = args.out.join(format!(
        "rsi_cross_filters_{}m_{}_{}.csv",
        args.minutes, args.start, args.end
    ));
    live.stage("report", "Writing the CSV of every combination", 0, 1);
    write_csv(&path, &ranked, &data, &train, &test, args.balance_usdt)?;

    let train_wins: Vec<Combo> = ranked
        .iter()
        .copied()
        .filter(|c| tr(*c).total() > 0.0)
        .collect();
    let both: Vec<Combo> = train_wins
        .iter()
        .copied()
        .filter(|c| te(*c).total() > 0.0)
        .collect();
    let test_wins = ranked.iter().filter(|c| te(**c).total() > 0.0).count();
    live.log(format!(
        "Training: {} of {total} combinations made money after fees. Test: {} of those also made money on the unseen months ({:.1}%); {} of all combinations made money in the test part",
        train_wins.len(),
        both.len(),
        if train_wins.is_empty() { 0.0 } else { both.len() as f64 / train_wins.len() as f64 * 100.0 },
        test_wins
    ));
    let top = ranked[0];
    live.log(format!(
        "Best in training: RSI {}/{} with {}: {:+.2} USDT in training, {:+.2} USDT in the test part",
        tr(top).fast,
        tr(top).slow,
        data.label(top.1 as usize),
        tr(top).total(),
        te(top).total()
    ));
    let top10: Vec<f64> = ranked.iter().take(10).map(|c| te(*c).total()).collect();
    live.log(format!(
        "The 10 best in training made {} in the test part",
        top10
            .iter()
            .map(|v| format!("{v:+.2}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    println!(
        "\n{:>4} {:>5}  {:<62} {:>7} {:>11} {:>7} {:>11} {:>9}",
        "fast", "slow", "TP/SL", "orders", "training", "orders", "test", "test DD"
    );
    for &c in ranked.iter().take(args.top) {
        let (a, b) = (tr(c), te(c));
        println!(
            "{:>4} {:>5}  {:<62} {:>7} {:>+11.2} {:>7} {:>+11.2} {:>9.2}",
            a.fast,
            a.slow,
            data.label(c.1 as usize),
            a.orders,
            a.total(),
            b.orders,
            b.total(),
            b.max_drawdown
        );
    }
    live.log(format!("Every combination: {}", path.display()));

    live.stage("report", "Writing the report", 0, 1);
    let n_exits = data.exits.len();
    let mut report = json!({
        "meta": {
            "instrument": okx_history::INSTRUMENT,
            "start": args.start.to_string(),
            "test_start": test_from.to_string(),
            "end": args.end.to_string(),
            "test_start_ms": ms(test_from),
            "timeframe_minutes": args.minutes,
            "lengths": args.lengths,
            "atr_length": dynamic_trader::filters::ATR_LENGTH,
            "filters": data.filters.len(),
            "exit_settings": n_exits,
            "size_btc": args.size_btc,
            "balance_usdt": args.balance_usdt,
            "taker_fee_bps": config.venues.okx.taker_fee_bps,
            "maker_fee_bps": config.venues.okx.maker_fee_bps,
            "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
        },
        "settings": (0..n_settings).map(|k| {
            let (f, e) = (&data.filters[k / n_exits], &data.exits[k % n_exits]);
            json!({
                "label": data.label(k),
                "filter": f.label(),
                "trend": f.trend,
                "min_atr_pct": f.min_atr_pct,
                "exits": exits_label(e),
                "stop_pct": e.as_ref().map(|e| e.stop_pct),
                "targets": e.as_ref().map(|e| e.targets.clone()),
                "breakeven": e.as_ref().map(|e| e.breakeven),
            })
        }).collect::<Vec<_>>(),
        "columns": COLUMNS,
        "best": best.iter().map(|c| row(tr(*c))).collect::<Vec<_>>(),
        "best_test": best.iter().map(|c| row(te(*c))).collect::<Vec<_>>(),
        "grid": (0..n_settings).map(|k| train.iter().map(|r| round2(r[k].total())).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "grid_test": (0..n_settings).map(|k| test.iter().map(|r| round2(r[k].total())).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "trade_columns": ["side", "entry_time_ms", "entry_price", "size_btc", "entry_fee", "exits", "open"],
        "exit_columns": ["time_ms", "price", "size_btc", "fee", "kind"],
        "details": {},
        "candles": data.candles.iter().map(|c| json!([c.time_ms / 1000, c.open, c.high, c.low, c.close])).collect::<Vec<_>>(),
    });
    if args.serve.is_some() {
        // Any combination is replayed on request, so the browser gets the summary only.
        let _ = live.computed.set(Computed {
            report: serde_json::to_string(&report)?,
            data: data.clone(),
        });
        live.status.lock().unwrap().ready = true;
        live.log("Results are ready in the live report; every combination can be opened");
    }
    let mut detailed: Vec<Combo> = both.iter().copied().take(args.report).collect();
    let more: Vec<Combo> = ranked
        .iter()
        .copied()
        .filter(|c| !detailed.contains(c))
        .take(args.report.saturating_sub(detailed.len()))
        .collect();
    detailed.extend(more);
    report["details"] = detailed
        .iter()
        .map(|&c| {
            let o = tr(c);
            (
                format!("{}/{}/{}", o.fast, o.slow, c.1),
                data.details(o.fast as usize, o.slow as usize, c.1 as usize)
                    .unwrap(),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();
    let html = REPORT_HTML
        .replace("/*__TABLE_SORT__*/", TABLE_SORT)
        .replace(
            "/*__REPORT__*/null",
            &serde_json::to_string(&report)?.replace("</", "<\\/"),
        );
    let html_path = path.with_extension("html");
    fs::write(&html_path, html)?;
    live.log(format!(
        "Report file (open in a browser): {}",
        html_path.display()
    ));

    if args.verify > 0 {
        let mut picks: Vec<Combo> = both.iter().copied().take(args.verify).collect();
        if picks.is_empty() {
            picks = ranked.iter().copied().take(args.verify).collect();
        }
        verify(
            args,
            &config,
            &data,
            &picks.iter().map(|c| *tr(*c)).collect::<Vec<_>>(),
            live,
        )?;
    }
    live.stage("done", "Finished", 1, 1);
    Ok(())
}

/// Reruns `picks` through NautilusTrader on both parts and compares every fill. The test part
/// starts from the archives of its first day, with the indicators warmed up on the training
/// candles, as the scanner does.
fn verify(
    args: &Args,
    config: &AppConfig,
    data: &Data,
    picks: &[Outcome],
    live: &Live,
) -> Result<()> {
    let test_from = args.test_from.unwrap();
    live.log(format!(
        "Rerunning {} combination(s) through NautilusTrader, on the training and the test part",
        picks.len()
    ));
    let mut all_same = true;
    for (n, o) in picks.iter().enumerate() {
        let (filter, exits) = data.setting(o.setting as usize).context("setting")?;
        let params = RsiCrossParams {
            fast: o.fast as usize,
            slow: o.slow as usize,
        };
        for period in [Period::Train, Period::Test] {
            let name = format!(
                "RSI {}/{} {} ({})",
                o.fast,
                o.slow,
                data.label(o.setting as usize),
                period.name()
            );
            let (start, end) = match period {
                Period::Train => (args.start, test_from),
                Period::Test => (test_from, args.end),
            };
            let run = Run {
                config,
                data: &args.data,
                start,
                end,
                bar_minutes: args.minutes as u64,
                size_btc: args.size_btc,
                balance_usdt: args.balance_usdt,
            };
            let mut signals = FilteredCross::new(params, *filter);
            if period == Period::Test {
                for bar in &data.candles[..data.split] {
                    signals.on_bar(bar);
                }
            }
            let started = Instant::now();
            let label = format!("NautilusTrader check {} of {}: {name}", n + 1, picks.len());
            live.stage("verify", label.clone(), 0, 1);
            let (result, mut theirs) =
                run_backtest(&run, Box::new(signals), exits.cloned(), |done, total| {
                    live.stage(
                        "verify",
                        format!("{label}, archive {done} of {total}"),
                        done,
                        total,
                    )
                })?;
            let pnl = result
                .stats_pnls
                .get("USDT")
                .and_then(|s| s.get("PnL (total)"))
                .copied()
                .unwrap_or(0.0);
            let (ours_o, record) = data
                .replay(o.fast as usize, o.slow as usize, o.setting as usize, period)
                .context("replay")?;
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
                .filter(
                    |&k| !matches!((ours.get(k), theirs.get(k)), (Some(a), Some(b)) if same(a, b)),
                )
                .collect();
            for &k in bad.iter().take(3) {
                live.log(format!(
                    "    fill {}: scanner {:?} | Nautilus {:?}",
                    k + 1,
                    ours.get(k),
                    theirs.get(k)
                ));
            }
            if !bad.is_empty() {
                let base = args.out.join(format!(
                    "verify_{}_{}_{}_{}",
                    o.fast,
                    o.slow,
                    o.setting,
                    period.name()
                ));
                for (who, fills) in [("scanner", &ours), ("nautilus", &theirs)] {
                    let path = base.with_extension(format!("{who}.csv"));
                    let mut file = BufWriter::new(fs::File::create(&path)?);
                    writeln!(file, "time_ms,kind,side,size_btc,price,fee_usdt")?;
                    for f in fills.iter() {
                        writeln!(
                            file,
                            "{},{:?},{},{},{},{}",
                            f.time_ms,
                            f.kind,
                            if f.buy { "buy" } else { "sell" },
                            f.size_btc,
                            f.price,
                            f.fee_usdt
                        )?;
                    }
                    file.flush()?;
                }
                live.log(format!(
                    "    every fill of both: {}.scanner.csv / .nautilus.csv",
                    base.display()
                ));
            }
            // Nautilus' "PnL (total)" counts closed parts and fees, not the open position.
            let closed = ours_o.realized - ours_o.fees;
            let ok = bad.is_empty() && (closed - pnl).abs() < 1e-6;
            all_same &= ok;
            live.log(format!(
                "  {name}: {} fills, {} differ (time, kind, side, size, price, fee); closed PnL Nautilus {pnl:.6} / scanner {closed:.6} USDT ({:.0}s)",
                theirs.len(),
                bad.len(),
                started.elapsed().as_secs_f64()
            ));
            live.status.lock().unwrap().verify.push(json!({
                "pair": name,
                "fills": theirs.len(),
                "differ": bad.len(),
                "nautilus_pnl": pnl,
                "scanner_pnl": closed,
                "ok": ok,
                "seconds": started.elapsed().as_secs_f64(),
            }));
        }
    }
    if all_same {
        live.log("NautilusTrader agrees with the scanner fill by fill");
        Ok(())
    } else {
        bail!("NautilusTrader and the scanner disagree, see above")
    }
}

const COLUMNS: [&str; 15] = [
    "fast",
    "slow",
    "setting",
    "orders",
    "refused",
    "closed",
    "wins",
    "target_fills",
    "stop_fills",
    "realized",
    "fees",
    "open_pnl",
    "total",
    "max_drawdown",
    "position",
];

fn row(o: &Outcome) -> Value {
    json!([
        o.fast,
        o.slow,
        o.setting,
        o.orders,
        o.refused,
        o.closed_trades,
        o.wins,
        o.target_fills,
        o.stop_fills,
        round6(o.realized),
        round6(o.fees),
        round6(o.open_pnl),
        round6(o.total()),
        round6(o.max_drawdown),
        o.position
    ])
}

fn trades_json(record: &Record) -> Value {
    record
        .trades
        .iter()
        .map(|t| {
            let exits: Vec<Value> = t
                .exits
                .iter()
                .map(|e| json!([e.time_ms, e.price, e.steps as f64 * STEP_BTC, e.fee, e.kind]))
                .collect();
            json!([
                if t.long { 1 } else { -1 },
                t.entry_ms,
                t.entry_price,
                t.steps as f64 * STEP_BTC,
                t.entry_fee,
                exits,
                t.open
            ])
        })
        .collect()
}

fn write_csv(
    path: &std::path::Path,
    ranked: &[Combo],
    data: &Data,
    train: &[Vec<Outcome>],
    test: &[Vec<Outcome>],
    balance: f64,
) -> Result<()> {
    let mut file = BufWriter::new(fs::File::create(path)?);
    let part = |p: &str| {
        [
            "orders",
            "refused_orders",
            "closed_trades",
            "win_rate_pct",
            "take_profit_fills",
            "stop_fills",
            "fees_usdt",
            "pnl_usdt",
            "pnl_pct",
            "max_drawdown_usdt",
            "final_position",
        ]
        .map(|c| format!("{p}_{c}"))
        .join(",")
    };
    writeln!(
        file,
        "fast,slow,filter,trend_ema,min_atr_pct,tp_sl,stop_pct,targets_pct,shares_pct,breakeven,{},{}",
        part("train"),
        part("test")
    )?;
    let n_exits = data.exits.len();
    for &(p, k) in ranked {
        let k = k as usize;
        let (f, s) = (&data.filters[k / n_exits], &data.exits[k % n_exits]);
        let list = |g: fn(&(f64, f64)) -> f64| {
            s.as_ref().map_or(String::new(), |e| {
                e.targets
                    .iter()
                    .map(|t| format!("{}", g(t)))
                    .collect::<Vec<_>>()
                    .join("/")
            })
        };
        let a = &train[p as usize][k];
        write!(
            file,
            "{},{},\"{}\",{},{},\"{}\",{},{},{},{}",
            a.fast,
            a.slow,
            f.label(),
            f.trend.map_or(String::new(), |n| n.to_string()),
            f.min_atr_pct.map_or(String::new(), |n| n.to_string()),
            exits_label(s),
            s.as_ref().map_or(String::new(), |e| e.stop_pct.to_string()),
            list(|t| t.0),
            list(|t| (t.1 * 100.0).round()),
            s.as_ref()
                .map_or(String::new(), |e| e.breakeven.to_string()),
        )?;
        for o in [a, &test[p as usize][k]] {
            write!(
                file,
                ",{},{},{},{:.2},{},{},{:.6},{:.6},{:.4},{:.6},{}",
                o.orders,
                o.refused,
                o.closed_trades,
                o.win_rate(),
                o.target_fills,
                o.stop_fills,
                o.fees,
                o.total(),
                o.total() / balance * 100.0,
                o.max_drawdown,
                o.position
            )?;
        }
        writeln!(file)?;
    }
    file.flush()?;
    Ok(())
}

const REPORT_HTML: &str = include_str!("rsi_combos_report.html");
const TABLE_SORT: &str = include_str!("table_sort.js");

/// Answers one browser request: the page, the run's status, the summary, or one combination's trades.
fn serve(stream: TcpStream, live: &Live) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");
    let not_ready = || {
        (
            503,
            "application/json",
            r#"{"error":"not ready"}"#.to_string(),
        )
    };
    let (code, kind, body) = match path {
        "/" => (
            200,
            "text/html; charset=utf-8",
            REPORT_HTML.replace("/*__TABLE_SORT__*/", TABLE_SORT),
        ),
        "/api/status" => (
            200,
            "application/json",
            serde_json::to_string(&*live.status.lock().unwrap())?,
        ),
        "/api/report" => match live.computed.get() {
            Some(c) => (200, "application/json", c.report.clone()),
            None => not_ready(),
        },
        _ if path.starts_with("/api/pair/") => match live.computed.get() {
            None => not_ready(),
            Some(c) => match combo_from_path(path)
                .and_then(|(f, s, k)| (f < s).then(|| c.data.details(f, s, k)).flatten())
            {
                Some(details) => (200, "application/json", serde_json::to_string(&details)?),
                None => (
                    404,
                    "application/json",
                    r#"{"error":"unknown pair or setting"}"#.into(),
                ),
            },
        },
        _ => (404, "text/plain", "not found".into()),
    };
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {code} {}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        if code == 200 { "OK" } else { "Error" },
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}

/// `/api/pair/80/260/12` -> (80, 260, 12).
fn combo_from_path(path: &str) -> Option<(usize, usize, usize)> {
    let mut parts = path.strip_prefix("/api/pair/")?.split('/');
    let mut next = || parts.next()?.parse().ok();
    Some((next()?, next()?, next()?))
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}
