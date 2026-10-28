use std::{collections::BTreeMap, env, path::PathBuf, time::Instant};

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Months, NaiveDate, Utc};
use dynamic_trader::{
    backtest::{Run, run_backtest},
    config::AppConfig,
    pine_rsi::{PineRsiParams, PineRsiSignals},
};
use nautilus_backtest::result::BacktestResult;

const USAGE: &str = "\
Backtest TradingView's \"RSI Strategy\" (converted to Rust) on every OKX BTC-USDT-SWAP trade,
using 1-hour bars. Long when RSI crosses up through oversold, short when it crosses down through
overbought; each signal reverses the position.

Usage: cargo run --release --bin backtest -- [options]

  --start <YYYY-MM-DD>   First UTC day, inclusive (default: first day of last month)
  --end <YYYY-MM-DD>     Last UTC day, exclusive (default: first day of this month)
  --rsi-period <list>    One or more RSI lengths, e.g. 14 or 10,14,50 (default: 14)
  --oversold <level>     Default: 30
  --overbought <level>   Default: 70
  --size-btc <btc>       Position size in BTC, in 0.0001 steps (default: 0.005)
  --balance-usdt <usdt>  Starting account balance in USDT (default: 500)
  --config <path>        Fee and risk config (default: config/testnet.toml)
  --data <dir>           Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)";

struct Args {
    start: NaiveDate,
    end: NaiveDate,
    periods: Vec<usize>,
    oversold: f64,
    overbought: f64,
    size_btc: f64,
    balance_usdt: f64,
    config: PathBuf,
    data: PathBuf,
}

fn parse_args() -> Result<Args> {
    let this_month = Utc::now().date_naive().with_day(1).unwrap();
    let mut args = Args {
        start: this_month - Months::new(1),
        end: this_month,
        periods: vec![14],
        oversold: 30.0,
        overbought: 70.0,
        size_btc: 0.005,
        balance_usdt: 500.0,
        config: PathBuf::from("config/testnet.toml"),
        data: PathBuf::from("data/okx/BTC-USDT-SWAP/archives"),
    };

    let mut raw = env::args().skip(1);
    while let Some(flag) = raw.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            std::process::exit(0);
        }
        let value = raw
            .next()
            .with_context(|| format!("missing value for {flag}\n\n{USAGE}"))?;
        let number = |name: &str| format!("{name} must be a number");
        match flag.as_str() {
            "--start" => args.start = value.parse().context("--start must be YYYY-MM-DD")?,
            "--end" => args.end = value.parse().context("--end must be YYYY-MM-DD")?,
            "--rsi-period" => {
                args.periods = value
                    .split(',')
                    .map(|p| p.trim().parse())
                    .collect::<Result<_, _>>()
                    .context("--rsi-period must be numbers separated by commas")?;
            }
            "--oversold" => args.oversold = value.parse().with_context(|| number(&flag))?,
            "--overbought" => args.overbought = value.parse().with_context(|| number(&flag))?,
            "--size-btc" => args.size_btc = value.parse().with_context(|| number(&flag))?,
            "--balance-usdt" => args.balance_usdt = value.parse().with_context(|| number(&flag))?,
            "--config" => args.config = PathBuf::from(value),
            "--data" => args.data = PathBuf::from(value),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }

    if args.start >= args.end {
        bail!("--start must be before --end");
    }
    if args.periods.is_empty() {
        bail!("--rsi-period needs at least one value");
    }
    let steps = args.size_btc * 10_000.0;
    if args.size_btc < 0.0001 || (steps.round() - steps).abs() > 1e-6 {
        bail!("--size-btc must be positive and use OKX's 0.0001 BTC step");
    }
    if args.balance_usdt <= 0.0 {
        bail!("--balance-usdt must be positive");
    }
    Ok(args)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let config = AppConfig::load(&args.config)?;
    println!(
        "Backtesting {} to {} (exclusive), {} BTC position, {} USDT balance, 1-hour bars",
        args.start, args.end, args.size_btc, args.balance_usdt
    );

    let run = Run {
        config: &config,
        data: &args.data,
        start: args.start,
        end: args.end,
        bar_minutes: 60,
        size_btc: args.size_btc,
        balance_usdt: args.balance_usdt,
    };
    let mut results = Vec::new();
    for &length in &args.periods {
        let params = PineRsiParams {
            length,
            oversold: args.oversold,
            overbought: args.overbought,
        };
        params.validate()?;
        let started = Instant::now();
        let (result, _) =
            run_backtest(&run, Box::new(PineRsiSignals::new(params)), None, |_, _| {})?;
        println!(
            "  {:<28} done in {:.1}s",
            label(&params),
            started.elapsed().as_secs_f64()
        );
        results.push((params, result));
    }

    if let [(params, result)] = results.as_slice() {
        print_details(params, result);
    } else {
        print_comparison(&results);
    }
    Ok(())
}

fn label(p: &PineRsiParams) -> String {
    format!(
        "Pine RSI({}) cross {}/{}",
        p.length, p.oversold, p.overbought
    )
}

fn usdt_stat(result: &BacktestResult, name: &str) -> f64 {
    result
        .stats_pnls
        .get("USDT")
        .and_then(|stats| stats.get(name))
        .copied()
        .unwrap_or(0.0)
}

fn print_comparison(results: &[(PineRsiParams, BacktestResult)]) {
    println!(
        "\n{:<28} {:>7} {:>12} {:>8} {:>9} {:>8} {:>8}",
        "Strategy", "Orders", "PnL (USDT)", "PnL %", "Win rate", "PF", "Sharpe"
    );
    for (params, result) in results {
        println!(
            "{:<28} {:>7} {:>12.6} {:>7.2}% {:>8.1}% {:>8.2} {:>8.2}",
            label(params),
            result.total_orders,
            usdt_stat(result, "PnL (total)"),
            usdt_stat(result, "PnL% (total)"),
            usdt_stat(result, "Win Rate") * 100.0,
            result
                .stats_returns
                .get("Profit Factor")
                .copied()
                .unwrap_or(0.0),
            result
                .stats_returns
                .get("Sharpe Ratio (252 days)")
                .copied()
                .unwrap_or(0.0),
        );
    }
}

fn print_details(params: &PineRsiParams, result: &BacktestResult) {
    println!(
        "\n{}: {} orders, {} positions",
        label(params),
        result.total_orders,
        result.total_positions
    );
    for (currency, stats) in &result.stats_pnls {
        print_section(&format!("PnL statistics ({currency})"), stats);
    }
    print_section("Return statistics", &result.stats_returns);
    print_section("General statistics", &result.stats_general);
}

fn print_section<'a>(title: &str, stats: impl IntoIterator<Item = (&'a String, &'a f64)>) {
    println!("\n{title}");
    for (name, value) in stats.into_iter().collect::<BTreeMap<_, _>>() {
        println!("  {name:<32} {value:>14.6}");
    }
}
