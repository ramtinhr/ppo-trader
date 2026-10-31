//! Batch fast-path replay for the Go product layer.
//!
//! Loads the OKX tape once, then reads JSONL combo requests on stdin and writes one JSONL
//! response per request. Walk-forward must batch here — never spawn once per combo.
//!
//! Request line:
//! `{"id":"1","minutes":60,"fast":14,"slow":50,"exits":{"stop_pct":1.0,"targets":[[1.0,1.0]],"breakeven":false},"size_btc":0.005,"leverage":1,"logged":true}`
//!
//! Optional `from_ms` / `to_ms` clip the candle range. Send `{"flush":true}` to force a batch flush.

use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::Mutex,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDate;
use dynamic_trader::{
    config::AppConfig,
    exits::Exits,
    fills::Fill,
    okx_history::Candle,
    rsi::Rsi,
    scan::{self, ClosedTrade, Costs, Market, Record, STEP_BTC},
    tape::{CloseRanges, Tape},
    tpsl_grid::Frame,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

const USAGE: &str = "\
Batch scanner replay: load the tape once, then JSONL combos on stdin → JSONL results on stdout.

Usage: cargo run --release --bin simulate -- [options]

  --start <YYYY-MM-DD>   First UTC day, inclusive (required)
  --end <YYYY-MM-DD>     Last UTC day, exclusive (required)
  --size-btc <btc>       Base position size before leverage (default: 0.005)
  --balance-usdt <usdt>  Starting balance (default: 1e9 = never refuse)
  --config <path>        Fee config (default: config/testnet.toml)
  --data <dir>           Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)
  --jobs <n>             Rayon threads for a stdin batch (default: rayon default)
";

#[derive(Debug)]
struct Args {
    start: NaiveDate,
    end: NaiveDate,
    size_btc: f64,
    balance_usdt: f64,
    config: PathBuf,
    data: PathBuf,
    jobs: Option<usize>,
}

fn parse_args() -> Result<Args> {
    let mut start = None;
    let mut end = None;
    let mut size_btc = 0.005;
    let mut balance_usdt = 1e9;
    let mut config = PathBuf::from("config/testnet.toml");
    let mut data = PathBuf::from("data/okx/BTC-USDT-SWAP/archives");
    let mut jobs = None;
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
            "--size-btc" => size_btc = value.parse().with_context(bad)?,
            "--balance-usdt" => balance_usdt = value.parse().with_context(bad)?,
            "--config" => config = value.into(),
            "--data" => data = value.into(),
            "--jobs" => jobs = Some(value.parse().with_context(bad)?),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    let start = start.context("--start is required")?;
    let end = end.context("--end is required")?;
    ensure!(start < end, "--start must be before --end");
    let steps = size_btc * 10_000.0_f64;
    ensure!(
        size_btc >= 0.0001 && (steps.round() - steps).abs() < 1e-6,
        "--size-btc must use OKX's 0.0001 BTC step"
    );
    Ok(Args {
        start,
        end,
        size_btc,
        balance_usdt,
        config,
        data,
        jobs,
    })
}

fn ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

#[derive(Debug, Deserialize)]
struct Request {
    id: String,
    minutes: usize,
    fast: usize,
    slow: usize,
    #[serde(default)]
    exits: Option<Exits>,
    #[serde(default)]
    size_btc: Option<f64>,
    #[serde(default = "one")]
    leverage: u32,
    #[serde(default)]
    from_ms: Option<i64>,
    #[serde(default)]
    to_ms: Option<i64>,
    #[serde(default)]
    logged: bool,
    #[serde(default)]
    record_fills: bool,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Serialize)]
struct Response {
    id: String,
    outcome: OutcomeJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fills: Vec<Fill>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    closed: Vec<ClosedJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    trades: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct OutcomeJson {
    fast: u16,
    slow: u16,
    setting: u16,
    orders: u32,
    closed_trades: u32,
    wins: u32,
    target_fills: u32,
    stop_fills: u32,
    realized: f64,
    fees: f64,
    open_pnl: f64,
    max_drawdown: f64,
    position: i8,
    refused: u32,
    total: f64,
}

#[derive(Debug, Serialize)]
struct ClosedJson {
    exit_ms: i64,
    entry_bar: u32,
    entry_ms: i64,
    net: f32,
}

struct Engine {
    tape: Tape,
    minutes: Vec<Candle>,
    base_costs: Costs,
    base_size_btc: f64,
    frames: Mutex<HashMap<usize, Frame>>,
    rsi: Mutex<HashMap<(usize, usize), Vec<f64>>>,
}

impl Engine {
    fn frame(&self, minutes: usize) -> Result<Frame> {
        {
            let cache = self.frames.lock().unwrap();
            if let Some(f) = cache.get(&minutes) {
                return Ok(Frame {
                    minutes: f.minutes,
                    candles: f.candles.clone(),
                    bar_start: f.bar_start.clone(),
                });
            }
        }
        let frame = Frame::from_minutes(&self.minutes, &self.tape.bar_start, minutes);
        self.frames.lock().unwrap().insert(
            minutes,
            Frame {
                minutes: frame.minutes,
                candles: frame.candles.clone(),
                bar_start: frame.bar_start.clone(),
            },
        );
        Ok(frame)
    }

    fn rsi_series(&self, minutes: usize, length: usize, candles: &[Candle]) -> Result<Vec<f64>> {
        {
            let cache = self.rsi.lock().unwrap();
            if let Some(s) = cache.get(&(minutes, length)) {
                return Ok(s.clone());
            }
        }
        let mut rsi = Rsi::new(length);
        let series: Vec<f64> = candles
            .iter()
            .map(|c| rsi.update(c.close).unwrap_or(f64::NAN))
            .collect();
        self.rsi
            .lock()
            .unwrap()
            .insert((minutes, length), series.clone());
        Ok(series)
    }

    fn run_one(&self, req: Request) -> Result<Response> {
        ensure!(req.minutes > 0, "minutes must be > 0");
        ensure!(req.fast > 0 && req.slow > 0, "fast/slow must be > 0");
        ensure!(req.leverage >= 1, "leverage must be >= 1");
        let size_btc = req.size_btc.unwrap_or(self.base_size_btc) * f64::from(req.leverage);
        let steps = (size_btc / STEP_BTC).round() as i64;
        ensure!(steps > 0, "size too small");
        let costs = Costs {
            steps,
            taker: self.base_costs.taker,
            maker: self.base_costs.maker,
            balance: self.base_costs.balance,
        };
        let frame = self.frame(req.minutes)?;
        let closes = CloseRanges::new(
            &frame
                .candles
                .iter()
                .map(|c| c.close)
                .collect::<Vec<_>>(),
        );
        let market = Market {
            candles: &frame.candles,
            tape: &self.tape,
            bar_start: &frame.bar_start,
            closes: &closes,
        };
        let fast = self.rsi_series(req.minutes, req.fast, &frame.candles)?;
        let slow = self.rsi_series(req.minutes, req.slow, &frame.candles)?;
        let signals = scan::signals(&fast, &slow);
        let (from, to) = clip_bars(&frame.candles, req.from_ms, req.to_ms);
        let mut closed_raw = Vec::new();
        let mut record = Record::default();
        let exits = req.exits.as_ref();
        let ids = (req.fast as u16, req.slow as u16, 0);
        let outcome = if req.logged {
            scan::simulate_logged(ids, &signals, exits, &market, from..to, costs, &mut closed_raw)
        } else if req.record_fills {
            scan::simulate(
                ids,
                &signals,
                exits,
                &market,
                from..to,
                costs,
                Some(&mut record),
            )
        } else {
            scan::simulate(ids, &signals, exits, &market, from..to, costs, None)
        };
        if req.logged && req.record_fills {
            let _ = scan::simulate(
                ids,
                &signals,
                exits,
                &market,
                from..to,
                costs,
                Some(&mut record),
            );
        }
        let closed = closed_raw
            .into_iter()
            .map(|c: ClosedTrade| ClosedJson {
                exit_ms: c.exit_ms,
                entry_bar: c.entry_bar,
                entry_ms: frame
                    .candles
                    .get(c.entry_bar as usize)
                    .map(|b| b.time_ms)
                    .unwrap_or(0),
                net: c.net,
            })
            .collect();
        let trades = record
            .trades
            .iter()
            .map(|t| {
                json!({
                    "long": t.long,
                    "entry_ms": t.entry_ms,
                    "entry_price": t.entry_price,
                    "steps": t.steps,
                    "entry_fee": t.entry_fee,
                    "open": t.open,
                    "exits": t.exits.iter().map(|e| json!({
                        "time_ms": e.time_ms,
                        "price": e.price,
                        "steps": e.steps,
                        "fee": e.fee,
                        "kind": e.kind,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(Response {
            id: req.id,
            outcome: OutcomeJson {
                fast: outcome.fast,
                slow: outcome.slow,
                setting: outcome.setting,
                orders: outcome.orders,
                closed_trades: outcome.closed_trades,
                wins: outcome.wins,
                target_fills: outcome.target_fills,
                stop_fills: outcome.stop_fills,
                realized: outcome.realized,
                fees: outcome.fees,
                open_pnl: outcome.open_pnl,
                max_drawdown: outcome.max_drawdown,
                position: outcome.position,
                refused: outcome.refused,
                total: outcome.total(),
            },
            fills: record.fills,
            closed,
            trades,
        })
    }
}

fn clip_bars(candles: &[Candle], from_ms: Option<i64>, to_ms: Option<i64>) -> (usize, usize) {
    let from = from_ms.map_or(0, |t| candles.partition_point(|c| c.time_ms < t));
    let to = to_ms.map_or(candles.len(), |t| candles.partition_point(|c| c.time_ms < t));
    (from.min(candles.len()), to.min(candles.len()).max(from))
}

fn main() -> Result<()> {
    let args = parse_args()?;
    if let Some(n) = args.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .ok();
    }
    let config = AppConfig::load(&args.config)?;
    eprintln!(
        "simulate: loading tape {} → {} …",
        args.start, args.end
    );
    let (minutes, tape) = Tape::load(
        &args.data,
        60_000,
        ms(args.start),
        ms(args.end),
        |done, total| {
            if done == total || done * 10 / total.max(1) > 0 && done % (total.max(10) / 10).max(1) == 0
            {
                eprintln!("simulate: archive {done} of {total}");
            }
        },
    )?;
    let engine = Engine {
        tape,
        minutes,
        base_costs: Costs {
            steps: (args.size_btc * 10_000.0).round() as i64,
            taker: config.venues.okx.taker_fee_bps / 10_000.0,
            maker: config.venues.okx.maker_fee_bps / 10_000.0,
            balance: args.balance_usdt,
        },
        base_size_btc: args.size_btc,
        frames: Mutex::new(HashMap::new()),
        rsi: Mutex::new(HashMap::new()),
    };
    eprintln!(
        "simulate: ready ({} 1m bars). Reading JSONL on stdin…",
        engine.minutes.len()
    );

    let stdin = io::stdin();
    let mut batch = Vec::new();
    for line in stdin.lock().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("\"flush\"") {
            flush_batch(&engine, &mut batch)?;
            continue;
        }
        let req: Request =
            serde_json::from_str(line).with_context(|| format!("bad request JSON: {line}"))?;
        batch.push(req);
        if batch.len() >= 256 {
            flush_batch(&engine, &mut batch)?;
        }
    }
    flush_batch(&engine, &mut batch)?;
    Ok(())
}

fn flush_batch(engine: &Engine, batch: &mut Vec<Request>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let reqs = std::mem::take(batch);
    let results: Vec<Result<Response>> = reqs.into_par_iter().map(|r| engine.run_one(r)).collect();
    let mut out = io::stdout().lock();
    for r in results {
        let resp = r?;
        serde_json::to_writer(&mut out, &resp)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
