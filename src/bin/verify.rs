//! One-combo NautilusTrader fill check for the Go product layer.
//!
//! Reads scanner fills as JSON on stdin (or `--fills path`) and compares them with a Nautilus
//! backtest of the same RSI-cross + exits over `--start`..`--end`.

use std::{
    fs,
    io::{self, Read},
    path::PathBuf,
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDate;
use dynamic_trader::{
    backtest::{Run, run_backtest},
    config::AppConfig,
    exits::Exits,
    fills::Fill,
    filters::{Filter, FilteredCross},
    rsi_cross::RsiCrossParams,
    signals::BarSignals,
    verify_cmp::compare_fills,
};
use serde::Deserialize;

const USAGE: &str = "\
Verify one RSI-cross combination fill-by-fill against NautilusTrader.

Usage: cargo run --release --features verify --bin verify -- [options]

  --start <YYYY-MM-DD>   First UTC day, inclusive (required)
  --end <YYYY-MM-DD>     Last UTC day, exclusive (required)
  --minutes <n>          Bar size in minutes (required)
  --fast <n>             Fast RSI length (required)
  --slow <n>             Slow RSI length (required)
  --size-btc <btc>       Position size in BTC (default: 0.005)
  --balance-usdt <usdt>  Starting balance (default: 1e9)
  --config <path>        Fee config (default: config/testnet.toml)
  --data <dir>           Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --exits <json>         Exits object JSON (optional; none = signal-only)
  --fills <path>         Scanner fills JSON array (default: stdin)
  --label <text>         Label in the JSON summary (default: RSI fast/slow)
";

#[derive(Debug)]
struct Args {
    start: NaiveDate,
    end: NaiveDate,
    minutes: u64,
    fast: usize,
    slow: usize,
    size_btc: f64,
    balance_usdt: f64,
    config: PathBuf,
    data: PathBuf,
    exits: Option<Exits>,
    fills: Option<PathBuf>,
    label: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut start = None;
    let mut end = None;
    let mut minutes = None;
    let mut fast = None;
    let mut slow = None;
    let mut size_btc = 0.005;
    let mut balance_usdt = 1e9;
    let mut config = PathBuf::from("config/testnet.toml");
    let mut data = PathBuf::from("data/okx/BTC-USDT-SWAP/archives");
    let mut exits = None;
    let mut fills = None;
    let mut label = None;
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
            "--start" => start = Some(value.parse().with_context(bad)?),
            "--end" => end = Some(value.parse().with_context(bad)?),
            "--minutes" => minutes = Some(value.parse().with_context(bad)?),
            "--fast" => fast = Some(value.parse().with_context(bad)?),
            "--slow" => slow = Some(value.parse().with_context(bad)?),
            "--size-btc" => size_btc = value.parse().with_context(bad)?,
            "--balance-usdt" => balance_usdt = value.parse().with_context(bad)?,
            "--config" => config = value.into(),
            "--data" => data = value.into(),
            "--exits" => exits = Some(serde_json::from_str(&value).with_context(bad)?),
            "--fills" => fills = Some(value.into()),
            "--label" => label = Some(value),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    let start = start.context("--start is required")?;
    let end = end.context("--end is required")?;
    ensure!(start < end, "--start must be before --end");
    Ok(Args {
        start,
        end,
        minutes: minutes.context("--minutes is required")?,
        fast: fast.context("--fast is required")?,
        slow: slow.context("--slow is required")?,
        size_btc,
        balance_usdt,
        config,
        data,
        exits,
        fills,
        label,
    })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FillsIn {
    Array(Vec<Fill>),
    Wrapped { fills: Vec<Fill> },
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let config = AppConfig::load(&args.config)?;
    let raw = match &args.fills {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("failed to read fills {}", path.display()))?,
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf)?;
            buf
        }
    };
    let ours = match serde_json::from_str::<FillsIn>(&raw)
        .with_context(|| "fills JSON must be an array or {\"fills\":[...]}")?
    {
        FillsIn::Array(v) => v,
        FillsIn::Wrapped { fills } => fills,
    };
    let label = args.label.unwrap_or_else(|| {
        format!(
            "RSI {}/{} ({} to {})",
            args.fast, args.slow, args.start, args.end
        )
    });
    let run = Run {
        config: &config,
        data: &args.data,
        start: args.start,
        end: args.end,
        bar_minutes: args.minutes,
        size_btc: args.size_btc,
        balance_usdt: args.balance_usdt,
    };
    let signals = FilteredCross::new(
        RsiCrossParams {
            fast: args.fast,
            slow: args.slow,
        },
        Filter::default(),
    );
    let started = Instant::now();
    eprintln!("NautilusTrader check: {label}");
    let mut shown = 0;
    let (_, theirs) = run_backtest(
        &run,
        Box::new(signals) as Box<dyn BarSignals>,
        args.exits,
        |done, total| {
            if done * 10 / total.max(1) > shown {
                shown = done * 10 / total.max(1);
                eprintln!("  archive {done} of {total}");
            }
        },
    )?;
    let summary = compare_fills(
        &label,
        ours,
        theirs,
        started.elapsed().as_secs_f64(),
    );
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if summary["ok"] != true {
        std::process::exit(1);
    }
    Ok(())
}
