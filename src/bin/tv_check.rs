//! Checks our candles, RSI and RSI-strategy trades against TradingView's own numbers, read from
//! TradingView's chart feed for OKX:BTCUSDT.P:
//!
//! 1. Candles: TradingView's candles vs ours, built from every OKX trade.
//! 2. RSI: TradingView's built-in RSI vs our Rust `Rsi` (the converted `ta.rsi`) on our candles,
//!    both started on the first bar TradingView loaded, for every period in `--periods`.
//! 3. Trades: TradingView's built-in "RSI strategy" vs our converted Pine logic on our candles,
//!    filled at the next bar's open like TradingView. Margin calls are turned off in
//!    TradingView so both sides trade on the RSI signals alone.
//!
//! The range is whatever TradingView loads for the timeframe (a free account gets about 6,400
//! bars) and is judged up to the end of our downloaded trades. Running scripts needs a signed-in
//! TradingView account: TRADINGVIEW_SESSIONID and TRADINGVIEW_SESSIONID_SIGN in `.env`.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use chrono::DateTime;
use dynamic_trader::{
    okx_history::{self, Candle},
    pine_rsi::{PineRsiParams, PineRsiSignals},
    rsi::Rsi,
    signals::Signal,
    tradingview::{ChartSession, PineScript, TvBar, auth_token},
};
use serde_json::{Value, json};

const USAGE: &str = "\
Usage: cargo run --release --bin tv_check -- [options]

  --timeframe <min>   Bar size in minutes: 1, 3, 60, ... (3)
  --periods <list>    RSI lengths for the RSI and strategy checks (10,50,100,150,200)
  --oversold <x>      RSI strategy oversold level (30)
  --overbought <x>    RSI strategy overbought level (70)
  --tolerance <pct>   Largest allowed RSI difference in percent (0.001)
  --archives <dir>    Verified OKX trade archives (data/okx/BTC-USDT-SWAP/archives)
  --out <dir>         Where to write the CSV reports (data/tradingview/check)";

const SYMBOL: &str = "OKX:BTCUSDT.P";
/// More than any plan allows; TradingView sends as many as the account may load.
const MAX_BARS: usize = 50_000;

struct Args {
    minutes: i64,
    periods: Vec<usize>,
    strategy: PineRsiParams,
    tolerance: f64,
    archives: PathBuf,
    out: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        minutes: 3,
        periods: vec![10, 50, 100, 150, 200],
        strategy: PineRsiParams {
            length: 10,
            oversold: 30.0,
            overbought: 70.0,
        },
        tolerance: 0.001,
        archives: "data/okx/BTC-USDT-SWAP/archives".into(),
        out: "data/tradingview/check".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        let value = it
            .next()
            .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
        match flag.as_str() {
            "--timeframe" => args.minutes = value.parse()?,
            "--periods" => {
                args.periods = value
                    .split(',')
                    .map(|p| p.trim().parse())
                    .collect::<Result<_, _>>()?
            }
            "--oversold" => args.strategy.oversold = value.parse()?,
            "--overbought" => args.strategy.overbought = value.parse()?,
            "--tolerance" => args.tolerance = value.parse()?,
            "--archives" => args.archives = value.into(),
            "--out" => args.out = value.into(),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(
        args.minutes > 0,
        "--timeframe must be a positive number of minutes"
    );
    ensure!(
        args.periods.iter().all(|p| *p > 0),
        "RSI periods must be positive"
    );
    args.strategy.validate()?;
    Ok(args)
}

/// TradingView's bars and our candles over the same times.
struct Series {
    tv: Vec<TvBar>,
    ours: BTreeMap<i64, Candle>,
    /// Our trades end here (exclusive, ms); later TradingView bars are not judged.
    ours_end_ms: i64,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    dotenvy::dotenv().ok();
    let sessionid = std::env::var("TRADINGVIEW_SESSIONID").unwrap_or_default();
    let sessionid_sign = std::env::var("TRADINGVIEW_SESSIONID_SIGN").unwrap_or_default();
    ensure!(
        !sessionid.is_empty() && !sessionid_sign.is_empty(),
        "TRADINGVIEW_SESSIONID and TRADINGVIEW_SESSIONID_SIGN are missing from .env; \
         TradingView runs RSI and strategies only for signed-in accounts"
    );
    let token = auth_token(&sessionid, &sessionid_sign)?;
    let timeframe = args.minutes.to_string();
    fs::create_dir_all(&args.out)?;

    let tv = ChartSession::open(SYMBOL, &timeframe, MAX_BARS, &token)?.bars();
    ensure!(tv.len() > 1, "TradingView sent no bars");
    let bar_ms = args.minutes * 60_000;
    let first_ms = tv[0].time * 1000;
    let ours_end_ms = available_end_ms(&args.archives, first_ms)?;
    let ours: BTreeMap<i64, Candle> =
        okx_history::candles(&args.archives, bar_ms, first_ms, ours_end_ms)?
            .into_iter()
            .map(|c| (c.time_ms, c))
            .collect();
    println!(
        "TradingView {SYMBOL} {}m: {} bars from {} to {} UTC. Our OKX trades cover it until {} UTC.\n",
        args.minutes,
        tv.len(),
        utc(first_ms),
        utc(tv[tv.len() - 1].time * 1000),
        utc(ours_end_ms)
    );
    let series = Series {
        tv,
        ours,
        ours_end_ms,
    };

    let mut passed = check_candles(&args, &series)?;
    let rsi_script = PineScript::fetch("STD;RSI")?;
    passed &= check_rsi(&args, &series, &rsi_script, &token, &timeframe)?;
    let strategy_script = PineScript::fetch("STD;RSI%1Strategy")?;
    passed &= check_trades(&args, &series, &strategy_script, &token, &timeframe)?;

    println!("\nReports written to {}", args.out.display());
    if passed {
        println!("RESULT: PASS, everything matches TradingView within the limits above");
        Ok(())
    } else {
        bail!("RESULT: FAIL, see the mismatches above")
    }
}

/// End (exclusive, ms) of the verified archives that follow on from `from_ms` without a gap.
fn available_end_ms(dir: &Path, from_ms: i64) -> Result<i64> {
    let mut day = okx_history::utc8_day(from_ms);
    ensure!(
        okx_history::is_verified(dir, day),
        "no verified OKX archive for {day}; run: cargo run --release --bin download_okx_trades -- {} <END>",
        day - chrono::Days::new(1)
    );
    while okx_history::is_verified(dir, day + chrono::Days::new(1)) {
        day = day + chrono::Days::new(1);
    }
    Ok(okx_history::archive_window_ms(day).1)
}

fn same_prices(a: [f64; 4], b: [f64; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
}

fn prices(c: &Candle) -> [f64; 4] {
    [c.open, c.high, c.low, c.close]
}

/// 1. TradingView's candles vs ours. Where they differ, OKX's official candle decides who is
/// right; only a candle of ours that OKX disagrees with is a failure.
fn check_candles(args: &Args, s: &Series) -> Result<bool> {
    println!("1. Candles: TradingView vs ours from every OKX trade");
    let mut out = fs::File::create(args.out.join("candles.csv"))?;
    writeln!(
        out,
        "time_utc,tv_open,our_open,tv_high,our_high,tv_low,our_low,tv_close,our_close,match,okx_official_agrees_with"
    )?;
    let bar_ms = args.minutes * 60_000;
    let (mut compared, mut tv_wrong, mut ours_wrong) = (0, 0, 0);
    for bar in s.tv.iter().filter(|b| b.time * 1000 < s.ours_end_ms) {
        let t = bar.time * 1000;
        let tv = [bar.open, bar.high, bar.low, bar.close];
        let our = s.ours.get(&t);
        compared += 1;
        let same = our.is_some_and(|c| same_prices(prices(c), tv));
        let mut verdict = "";
        if !same {
            let okx = prices(&okx_history::official_candle(bar_ms, t)?);
            if our.is_some_and(|c| same_prices(prices(c), okx)) {
                tv_wrong += 1;
                verdict = "ours";
            } else {
                ours_wrong += 1;
                verdict = if same_prices(tv, okx) {
                    "tradingview"
                } else {
                    "neither"
                };
            }
            println!(
                "   {} TradingView {tv:?} | ours {:?} | OKX official {okx:?}",
                utc(t),
                our.map(prices)
            );
        }
        let o = |i: usize| our.map(|c| prices(c)[i].to_string()).unwrap_or_default();
        writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{verdict}",
            utc(t),
            tv[0],
            o(0),
            tv[1],
            o(1),
            tv[2],
            o(2),
            tv[3],
            o(3),
            u8::from(same)
        )?;
    }
    let tv_times: std::collections::BTreeSet<i64> = s.tv.iter().map(|b| b.time * 1000).collect();
    let extra = s.ours.keys().filter(|t| !tv_times.contains(t)).count();
    println!(
        "   {compared} candles compared: {} identical, {tv_wrong} where TradingView differs from OKX's \
         official candle and ours matches OKX, {ours_wrong} where ours is wrong; {extra} of ours missing on TradingView",
        compared - tv_wrong - ours_wrong
    );
    Ok(compared > 0 && ours_wrong == 0 && extra == 0)
}

/// Our close for every TradingView bar we cover, in TradingView's bar order.
fn our_closes(s: &Series) -> Vec<(i64, Option<f64>)> {
    s.tv.iter()
        .map(|b| b.time * 1000)
        .filter(|t| *t < s.ours_end_ms)
        .map(|t| (t, s.ours.get(&t).map(|c| c.close)))
        .collect()
}

fn pct_diff(tv: f64, ours: f64) -> f64 {
    if tv == 0.0 {
        (ours - tv).abs() * 100.0
    } else {
        ((ours - tv) / tv).abs() * 100.0
    }
}

/// Largest RSI difference and the number of bars over `tolerance`, comparing TradingView's RSI
/// with ours on `closes` (bar open time in ms, close). Rows go to `out` when given.
fn compare_rsi(
    period: usize,
    closes: &[(i64, f64)],
    study: &BTreeMap<i64, Vec<Option<f64>>>,
    tolerance: f64,
    mut out: Option<&mut fs::File>,
) -> Result<(usize, usize, f64, i64)> {
    let mut rsi = Rsi::new(period);
    let (mut compared, mut mismatches, mut worst, mut worst_at) = (0, 0, 0.0_f64, 0);
    for &(t, close) in closes {
        let ours = rsi.update(close);
        let tv = study
            .get(&(t / 1000))
            .and_then(|p| p.first().copied().flatten());
        if tv.is_none() && ours.is_none() {
            continue;
        }
        let diff = match (tv, ours) {
            (Some(tv), Some(ours)) => pct_diff(tv, ours),
            _ => f64::INFINITY,
        };
        let bad = diff > tolerance;
        compared += 1;
        mismatches += usize::from(bad);
        if diff > worst {
            (worst, worst_at) = (diff, t);
        }
        if let Some(file) = out.as_mut() {
            let f = |x: Option<f64>| x.map(|x| format!("{x:.10}")).unwrap_or_default();
            writeln!(
                file,
                "{period},{},{close},{},{},{diff:.10},{}",
                utc(t),
                f(tv),
                f(ours),
                u8::from(bad)
            )?;
        }
    }
    Ok((compared, mismatches, worst, worst_at))
}

/// 2. TradingView's RSI vs our `Rsi`, both from TradingView's first bar, twice: on the exact
/// candles TradingView used (tests the converted formula) and on our candles (tests our data
/// too). TradingView serves each chart from one of several servers whose candles can differ, so
/// every candle where that chart differs from ours is checked against OKX's official candle.
fn check_rsi(
    args: &Args,
    s: &Series,
    script: &PineScript,
    token: &str,
    timeframe: &str,
) -> Result<bool> {
    println!(
        "\n2. RSI: TradingView's RSI vs our Rust Rsi (limit {}%)",
        args.tolerance
    );
    let mut out = fs::File::create(args.out.join("rsi.csv"))?;
    let mut summary = fs::File::create(args.out.join("rsi_summary.csv"))?;
    writeln!(
        out,
        "period,time_utc,our_close,tv_rsi,our_rsi,diff_pct,mismatch"
    )?;
    writeln!(
        summary,
        "period,formula_bars,formula_mismatches,formula_max_diff_pct,data_bars,data_mismatches,data_max_diff_pct,data_max_at_utc,tv_candles_wrong"
    )?;
    let bar_ms = args.minutes * 60_000;
    let ours: Vec<(i64, f64)> = our_closes(s)
        .into_iter()
        .map(|(t, c)| {
            c.map(|c| (t, c))
                .with_context(|| format!("our candle at {} is missing", utc(t)))
        })
        .collect::<Result<_>>()?;
    let mut all_pass = true;
    for &period in &args.periods {
        // A fresh chart per study keeps us inside the free plan's per-chart limit.
        let mut chart = ChartSession::open(SYMBOL, timeframe, MAX_BARS, token)?;
        let study = chart.add_study("st1", script, &[("RSI Length", json!(period))])?;
        let bars = chart.bars();
        ensure!(
            bars.first().map(|b| b.time) == s.tv.first().map(|b| b.time),
            "TradingView started RSI({period}) on a different first bar"
        );

        // The last bar is still forming: its close can change between the candle and RSI updates.
        let finished = &bars[..bars.len().saturating_sub(1)];
        let tv_closes: Vec<(i64, f64)> =
            finished.iter().map(|b| (b.time * 1000, b.close)).collect();
        let (f_bars, f_bad, f_worst, f_at) =
            compare_rsi(period, &tv_closes, &study.plots, args.tolerance, None)?;
        if f_bad > 0 {
            println!(
                "   RSI({period}) on TradingView's candles: largest difference at {}",
                utc(f_at)
            );
        }
        let (d_bars, d_bad, d_worst, d_at) =
            compare_rsi(period, &ours, &study.plots, args.tolerance, Some(&mut out))?;

        let (tv_wrong, ours_wrong) =
            judge_chart_candles(&format!("RSI({period})"), &bars, s, bar_ms)?;

        let at = if d_at > 0 { utc(d_at) } else { "-".into() };
        writeln!(
            summary,
            "{period},{f_bars},{f_bad},{f_worst:.10},{d_bars},{d_bad},{d_worst:.10},{at},{tv_wrong}"
        )?;
        println!(
            "   RSI({period:3}) on TradingView's candles: {f_bars} bars, {f_bad} over the limit, largest difference {f_worst:.10}%"
        );
        println!(
            "   RSI({period:3}) on our candles:           {d_bars} bars, {d_bad} over the limit, largest difference {d_worst:.10}% at {at}"
        );
        // Our formula must match exactly. On our candles, differences are allowed only when they
        // come from TradingView candles that OKX's official candles prove wrong.
        all_pass &= f_bars > 0 && f_bad == 0 && ours_wrong == 0 && (d_bad == 0 || tv_wrong > 0);
    }
    Ok(all_pass)
}

/// The strategy goes `long` (or short) at bar open `time_ms`, filling at `price`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Entry {
    time_ms: i64,
    long: bool,
    price: f64,
}

/// 3. TradingView's "RSI strategy" trades vs our converted Pine logic on our candles, for every
/// RSI length in `--periods`.
fn check_trades(
    args: &Args,
    s: &Series,
    script: &PineScript,
    token: &str,
    timeframe: &str,
) -> Result<bool> {
    println!(
        "\n3. Trades: TradingView's RSI strategy (oversold {}, overbought {}) vs our converted Pine on our candles",
        args.strategy.oversold, args.strategy.overbought
    );
    let mut out = fs::File::create(args.out.join("trades.csv"))?;
    writeln!(
        out,
        "length,tv_time_utc,tv_side,tv_price,our_time_utc,our_side,our_price,match"
    )?;
    let mut all_pass = true;
    for &length in &args.periods {
        let p = PineRsiParams {
            length,
            ..args.strategy
        };
        all_pass &= check_strategy(args, s, script, token, timeframe, p, &mut out)?;
    }
    Ok(all_pass)
}

fn check_strategy(
    args: &Args,
    s: &Series,
    script: &PineScript,
    token: &str,
    timeframe: &str,
    p: PineRsiParams,
    out: &mut fs::File,
) -> Result<bool> {
    let length = p.length;
    let bar_ms = args.minutes * 60_000;
    let mut chart = ChartSession::open(SYMBOL, timeframe, MAX_BARS, token)?;
    let study = chart.add_study(
        "st1",
        script,
        &[
            ("Length", json!(p.length)),
            ("Oversold", json!(p.oversold)),
            ("Overbought", json!(p.overbought)),
            ("Default entry/order Qty Type", json!("fixed")),
            ("Default entry/order Qty Value", json!(1)),
            ("Margin Long", json!(0)),
            ("Margin Short", json!(0)),
        ],
    )?;
    let bars = chart.bars();
    let report = study
        .report
        .context("TradingView sent no strategy report")?;
    fs::write(
        args.out.join(format!("tv_strategy_report_{length}.json")),
        serde_json::to_string_pretty(&report)?,
    )?;
    let tv_start_ms = report["settings"]["dateRange"]["backtest"]["from"]
        .as_i64()
        .context("strategy report has no backtest start")?
        // TradingView can only run the strategy from the first bar it loaded.
        .max(bars.first().context("TradingView sent no bars")?.time * 1000);
    let (tv_entries, tv_open) = tv_entries(&report)?;

    // Formula check: our strategy on the exact candles this chart used (the last one is still
    // forming, so it is left out).
    let finished = &bars[..bars.len().saturating_sub(1)];
    let tv_bars: Vec<(i64, Candle)> = finished
        .iter()
        .map(|b| {
            let time_ms = b.time * 1000;
            (
                time_ms,
                Candle {
                    time_ms,
                    open: b.open,
                    high: b.high,
                    low: b.low,
                    close: b.close,
                },
            )
        })
        .collect();
    let (f_entries, f_bad, _) = compare_entries(
        length,
        p,
        tv_start_ms,
        &tv_bars,
        &tv_entries,
        tv_open,
        bar_ms,
        None,
    )?;

    // Data check: our strategy on our candles.
    let ours: Vec<(i64, Candle)> = s.ours.iter().map(|(t, c)| (*t, *c)).collect();
    let (d_entries, d_bad, d_tv) = compare_entries(
        length,
        p,
        tv_start_ms,
        &ours,
        &tv_entries,
        tv_open,
        bar_ms,
        Some(out),
    )?;

    let (tv_wrong, ours_wrong) =
        judge_chart_candles(&format!("RSI({length}) strategy"), &bars, s, bar_ms)?;
    println!(
        "   RSI({length:3}) strategy on TradingView's candles: {f_entries} entries, {f_bad} differ from TradingView's trades"
    );
    println!(
        "   RSI({length:3}) strategy on our candles:           TradingView {d_tv} entries, ours {d_entries}, {d_bad} differ (time, side or fill price)"
    );
    // Our strategy must reproduce TradingView's trades exactly on TradingView's candles. On our
    // candles, differences are allowed only when OKX's official candles prove TradingView's wrong.
    Ok(f_bad == 0 && ours_wrong == 0 && (d_bad == 0 || tv_wrong > 0))
}

/// Runs our converted strategy on `bars` (bar open time in ms, candle) from TradingView's backtest
/// start and compares its entries with TradingView's. Returns (our entries, mismatches,
/// TradingView entries) over the judged range; rows go to `out` when given.
fn compare_entries(
    length: usize,
    p: PineRsiParams,
    tv_start_ms: i64,
    bars: &[(i64, Candle)],
    tv_entries: &[Entry],
    tv_open: Option<Entry>,
    bar_ms: i64,
    out: Option<&mut fs::File>,
) -> Result<(usize, usize, usize)> {
    let bars: Vec<&(i64, Candle)> = bars.iter().filter(|(t, _)| *t >= tv_start_ms).collect();
    // Start on TradingView's first strategy bar when we have it; otherwise judge only after RSI
    // has forgotten the different start.
    let judge_from = if bars.first().is_some_and(|(t, _)| *t == tv_start_ms) {
        tv_start_ms
    } else {
        let warmup = 25 * length;
        bars.get(warmup)
            .context("not enough candles for the RSI warm-up")?
            .0
    };

    let mut signals = PineRsiSignals::new(p);
    let mut ours = Vec::new();
    let mut long: Option<bool> = None;
    for pair in bars.windows(2) {
        let ((time, candle), (next_time, next)) = (pair[0], pair[1]);
        ensure!(
            next_time - time == bar_ms,
            "candle missing after {}",
            utc(*time)
        );
        let want = match signals.update(candle.close).1 {
            Some(Signal::Long) => Some(true),
            Some(Signal::Short) => Some(false),
            Some(Signal::Flat) | None => None,
        };
        // pyramiding = 1: an entry in the current direction is ignored.
        if let Some(want) = want.filter(|w| long != Some(*w)) {
            long = Some(want);
            ours.push(Entry {
                time_ms: *next_time,
                long: want,
                price: next.open,
            });
        }
    }
    let last_bar_ms = bars.last().map(|(t, _)| *t).unwrap_or_default();
    let judged = |e: &&Entry| e.time_ms >= judge_from && e.time_ms <= last_bar_ms;
    let ours: Vec<Entry> = ours.iter().filter(judged).copied().collect();
    let mut theirs: Vec<Entry> = tv_entries.iter().filter(judged).copied().collect();
    // TradingView lists closed trades; the still-open one is matched on side and price only.
    if let (Some(open), Some(last)) = (tv_open, ours.last()) {
        if ours.len() == theirs.len() + 1 && open.long == last.long && open.price == last.price {
            theirs.push(*last);
        }
    }

    let side = |e: &Entry| if e.long { "long" } else { "short" };
    let cell = |e: Option<&Entry>| {
        e.map(|e| format!("{},{},{}", utc(e.time_ms), side(e), e.price))
            .unwrap_or_else(|| ",,".into())
    };
    let mut mismatches = 0;
    let mut out = out;
    for i in 0..theirs.len().max(ours.len()) {
        let (tv, us) = (theirs.get(i), ours.get(i));
        let same = tv.is_some() && tv == us;
        if !same {
            mismatches += 1;
            if mismatches <= 3 {
                let label = if out.is_some() {
                    "our"
                } else {
                    "TradingView's"
                };
                println!(
                    "   RSI({length}) on {label} candles, trade {}: TradingView {} | ours {}",
                    i + 1,
                    cell(tv),
                    cell(us)
                );
            }
        }
        if let Some(file) = out.as_mut() {
            writeln!(
                file,
                "{length},{},{},{}",
                cell(tv),
                cell(us),
                u8::from(same)
            )?;
        }
    }
    Ok((ours.len(), mismatches, theirs.len()))
}

/// Candles of a TradingView chart that differ from ours, judged by OKX's official candle.
/// Returns (TradingView wrong, ours wrong).
fn judge_chart_candles(
    label: &str,
    bars: &[TvBar],
    s: &Series,
    bar_ms: i64,
) -> Result<(usize, usize)> {
    let (mut tv_wrong, mut ours_wrong) = (0, 0);
    for bar in bars.iter().filter(|b| b.time * 1000 < s.ours_end_ms) {
        let t = bar.time * 1000;
        let tv = [bar.open, bar.high, bar.low, bar.close];
        let our = s.ours.get(&t).map(prices);
        if our.is_some_and(|o| same_prices(o, tv)) {
            continue;
        }
        let okx = prices(&okx_history::official_candle(bar_ms, t)?);
        if our.is_some_and(|o| same_prices(o, okx)) {
            tv_wrong += 1;
            println!(
                "   {label} chart: TradingView's candle {} {tv:?} differs from OKX's official {okx:?}; ours matches OKX",
                utc(t)
            );
        } else {
            ours_wrong += 1;
            println!(
                "   {label} chart: our candle {} {our:?} differs from OKX's official {okx:?}",
                utc(t)
            );
        }
    }
    Ok((tv_wrong, ours_wrong))
}

/// Entries of TradingView's closed trades, and the side and price of the open position's
/// entry (open trades carry no time in the report).
fn tv_entries(report: &Value) -> Result<(Vec<Entry>, Option<Entry>)> {
    let mut entries = Vec::new();
    for trade in report["trades"].as_array().into_iter().flatten() {
        let e = &trade["e"];
        ensure!(
            matches!(e["c"].as_str(), Some("RsiLE" | "RsiSE")),
            "unexpected TradingView entry {e}; margin calls should be off"
        );
        entries.push(Entry {
            time_ms: e["tm"].as_i64().context("trade has no time")?,
            long: e["tp"] == "le",
            price: e["p"].as_f64().context("trade has no price")?,
        });
    }
    entries.sort_by_key(|e| e.time_ms);
    entries.dedup_by_key(|e| e.time_ms);
    let open = report["filledOrders"]
        .as_array()
        .and_then(|orders| orders.iter().rev().find(|o| o["e"] == true))
        .map(|o| Entry {
            time_ms: 0,
            long: o["c"] == "RsiLE",
            price: o["p"].as_f64().unwrap_or(f64::NAN),
        });
    Ok((entries, open))
}

fn utc(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}
