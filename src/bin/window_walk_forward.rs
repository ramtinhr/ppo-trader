use std::{
    cmp::Ordering,
    fs::{self, File},
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use dynamic_trader::{
    config::AppConfig,
    exits::Exits,
    grid::rsi_series,
    scan::{self, Costs, Market, Record, STEP_BTC},
    tape::{CloseRanges, Tape},
    tpsl_grid::Frame,
    window_score::{MIN_DROP, MIN_TRADES, RECORD, WindowStats},
};
use serde::Deserialize;
use serde_json::{Value, json};

const USAGE: &str = "\
Walk-forward over window_scan's windows: every combination is scored on one window, the best
one is traded in the next window, then everything is scored again on that window, and so on.

Score = profit / max(largest drop, 5 USDT) x trusted win rate x risk x hold time, with at least
5 trades in the window (see src/window_score.rs). The traded window's result is the
combination's own result there: it starts flat, and a trade still open at the end is closed
with the taker fee. Each combination traded is replayed again by the trade-by-trade scanner,
which must give the same result.

Usage: cargo run --release --bin window_walk_forward -- [options]

  --scan <dir>      window_scan's results (default: data/windowscan/2025-10-01_2026-10-01_10d)
  --top <n>         Best combinations of each window shown with their next window (default: 100)
  --check <0|1>     1 to replay every combination traded through the scanner (default: 1)
  --config <path>   Fee config (default: config/testnet.toml)
  --data <dir>      Verified OKX archives (default: data/okx/BTC-USDT-SWAP/archives)";

struct Args {
    scan: PathBuf,
    top: usize,
    check: bool,
    config: PathBuf,
    data: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        scan: "data/windowscan/2025-10-01_2026-10-01_10d".into(),
        top: 100,
        check: true,
        config: "config/testnet.toml".into(),
        data: "data/okx/BTC-USDT-SWAP/archives".into(),
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
            "--top" => args.top = value.parse().with_context(bad)?,
            "--check" => args.check = value.parse::<u8>().with_context(bad)? != 0,
            "--config" => args.config = value.into(),
            "--data" => args.data = value.into(),
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    ensure!(args.top > 0, "--top must be at least 1");
    Ok(args)
}

#[derive(Deserialize)]
struct Window {
    start: String,
    end: String,
    file: String,
    first_price: f64,
    last_price: f64,
}

#[derive(Deserialize)]
struct Meta {
    start: NaiveDate,
    end: NaiveDate,
    days: u64,
    #[serde(default)]
    hours: Option<u64>,
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    pairs: usize,
    tp: Vec<f64>,
    sl: Vec<f64>,
    /// Missing in older scans: treated as `[1]` (no leverage axis).
    #[serde(default = "default_leverage")]
    leverage: Vec<u32>,
    settings: usize,
    combinations: usize,
    windows: Vec<Window>,
    size_btc: f64,
    taker_fee_bps: f64,
    maker_fee_bps: f64,
    /// Staged scans: maps file group index → original timeframe×pair group id.
    #[serde(default)]
    survivor_groups: Option<Vec<usize>>,
}

fn default_leverage() -> Vec<u32> {
    vec![1]
}

impl Meta {
    fn window_hours(&self) -> u64 {
        self.hours.unwrap_or(self.days.saturating_mul(24)).max(1)
    }
}

impl Meta {
    /// (timeframe, fast length, slow length, tp, sl, leverage) indexes of combination `id`.
    fn decode(&self, id: u32) -> (usize, usize, usize, usize, usize, usize) {
        let id = id as usize;
        let file_group = id / self.settings;
        let s = id % self.settings;
        let group = self
            .survivor_groups
            .as_ref()
            .map(|sg| sg[file_group])
            .unwrap_or(file_group);
        let (f, mut p) = (group / self.pairs, group % self.pairs);
        let n = self.lengths.len();
        let mut i = 0;
        while p >= n - 1 - i {
            p -= n - 1 - i;
            i += 1;
        }
        let n_sl = self.sl.len();
        let n_lev = self.leverage.len().max(1);
        let lev = s % n_lev;
        let tpsl = s / n_lev;
        (f, i, i + 1 + p, tpsl / n_sl, tpsl % n_sl, lev)
    }

    fn label(&self, id: u32) -> String {
        let (f, i, j, tp, sl, li) = self.decode(id);
        format!(
            "{}m RSI {}/{} · TP {}% · SL {}% · {}x",
            self.timeframes[f],
            self.lengths[i],
            self.lengths[j],
            self.tp[tp],
            self.sl[sl],
            self.leverage[li]
        )
    }
}

#[derive(Clone, Copy)]
struct Scored {
    id: u32,
    score: f64,
    stats: WindowStats,
}

/// Higher score first, then higher profit, then the lower id.
fn better(a: &Scored, b: &Scored) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then(b.stats.profit.total_cmp(&a.stats.profit))
        .then(a.id.cmp(&b.id))
}

/// Combinations with a score in one window.
struct Ranking {
    top: Vec<Scored>,
    traded: usize,
    eligible: usize,
    profitable: usize,
}

/// Every combination of `path`, scored; the best `keep` in order.
fn rank(path: &Path, n_combos: usize, keep: usize) -> Result<Ranking> {
    let bytes = fs::read(path).with_context(|| path.display().to_string())?;
    ensure!(
        bytes.len() == n_combos * RECORD,
        "{} has the wrong size",
        path.display()
    );
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = n_combos.div_ceil(threads);
    let parts: Vec<Ranking> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let bytes = &bytes;
                scope.spawn(move || {
                    let mut r = Ranking {
                        top: Vec::with_capacity(2 * keep),
                        traded: 0,
                        eligible: 0,
                        profitable: 0,
                    };
                    for id in t * chunk..((t + 1) * chunk).min(n_combos) {
                        let stats = WindowStats::from_bytes(&bytes[id * RECORD..(id + 1) * RECORD]);
                        r.traded += usize::from(stats.trades > 0);
                        r.profitable += usize::from(stats.profit > 0.0);
                        let Some(score) = stats.score() else { continue };
                        r.eligible += 1;
                        r.top.push(Scored {
                            id: id as u32,
                            score,
                            stats,
                        });
                        if r.top.len() == 2 * keep {
                            r.top.select_nth_unstable_by(keep - 1, better);
                            r.top.truncate(keep);
                        }
                    }
                    r.top.sort_by(better);
                    r.top.truncate(keep);
                    r
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all = Ranking {
        top: Vec::new(),
        traded: 0,
        eligible: 0,
        profitable: 0,
    };
    for p in parts {
        all.top.extend(p.top);
        all.traded += p.traded;
        all.eligible += p.eligible;
        all.profitable += p.profitable;
    }
    all.top.sort_by(better);
    all.top.truncate(keep);
    Ok(all)
}

fn read_stats(file: &File, id: u32) -> Result<WindowStats> {
    let mut b = [0u8; RECORD];
    file.read_exact_at(&mut b, id as u64 * RECORD as u64)?;
    Ok(WindowStats::from_bytes(&b))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn stats_json(s: &WindowStats) -> Value {
    json!({
        "profit": round2(f64::from(s.profit)),
        "drop": round2(f64::from(s.drop)),
        "worst": round2(f64::from(s.worst)),
        "hold_hours": round2(f64::from(s.hold_hours)),
        "trades": s.trades,
        "wins": s.wins,
        "win_rate": round2(100.0 * s.win_rate()),
        "trusted_win_rate": round2(100.0 * s.trusted_win_rate()),
        "risk": round2(s.risk_factor()),
        "hold": round2(s.hold_factor()),
        "score": s.score().map(|x| (x * 1000.0).round() / 1000.0),
    })
}

fn ms(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn parse_window_ms(s: &str) -> Result<i64> {
    if let Ok(d) = s.parse::<NaiveDate>() {
        return Ok(ms(d));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt.and_utc().timestamp_millis());
        }
    }
    bail!("bad window time {s:?} (want YYYY-MM-DD or YYYY-MM-DD HH:MM:SS)")
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let meta: Meta = serde_json::from_slice(&fs::read(args.scan.join("meta.json")).with_context(|| {
        format!("no finished scan in {}: window_scan writes meta.json when it is done (see data/windowscan/run.log)", args.scan.display())
    })?)?;
    let config = AppConfig::load(&args.config)?;
    let n = meta.windows.len();
    ensure!(n >= 2, "need at least two windows");
    let files: Vec<File> = meta
        .windows
        .iter()
        .map(|w| File::open(args.scan.join(&w.file)).with_context(|| w.file.clone()))
        .collect::<Result<_>>()?;
    let taker = meta.taker_fee_bps / 10_000.0;
    let buy_and_hold = |w: &Window| {
        meta.size_btc * (w.last_price - w.first_price)
            - meta.size_btc * (w.first_price + w.last_price) * taker
    };

    let mut rounds = Vec::new();
    let mut picks: Vec<(usize, u32, WindowStats)> = Vec::new();
    let (mut total, mut bh_total, mut positive, mut flat) = (0.0, 0.0, 0, 0);
    for w in 0..n - 1 {
        let scored = &meta.windows[w];
        let traded = &meta.windows[w + 1];
        let r = rank(&args.scan.join(&scored.file), meta.combinations, args.top)?;
        let next: Vec<WindowStats> = r
            .top
            .iter()
            .map(|s| read_stats(&files[w + 1], s.id))
            .collect::<Result<_>>()?;
        let bh = buy_and_hold(traded);
        bh_total += bh;
        let top_next: Vec<f64> = next.iter().map(|s| f64::from(s.profit)).collect();
        let tops: Vec<Value> = r
            .top
            .iter()
            .zip(&next)
            .enumerate()
            .take(20)
            .map(|(k, (s, nx))| json!({ "rank": k + 1, "label": meta.label(s.id), "id": s.id, "scored": stats_json(&s.stats), "next": stats_json(nx) }))
            .collect();
        let mut round = json!({
            "scored_from": &scored.start,
            "scored_to": &scored.end,
            "traded_from": &traded.start,
            "traded_to": &traded.end,
            "combinations_traded": r.traded,
            "eligible": r.eligible,
            "profitable": r.profitable,
            "buy_and_hold": round2(bh),
            "top_n": top_next.len(),
            "top_next_total": round2(top_next.iter().sum::<f64>()),
            "top_next_positive": top_next.iter().filter(|p| **p > 0.0).count(),
            "tops": tops,
        });
        if let (Some(best), Some(result)) = (r.top.first(), next.first()) {
            let net = f64::from(result.profit);
            total += net;
            positive += usize::from(net > 0.0);
            picks.push((w + 1, best.id, *result));
            round["pick"] = json!({ "label": meta.label(best.id), "id": best.id, "scored": stats_json(&best.stats), "traded": stats_json(result) });
            println!(
                "{} → {}: {} (score {:.3}) made {:+.2} USDT in {} trades; buy-and-hold {:+.2}",
                scored.start,
                traded.start,
                meta.label(best.id),
                best.score,
                net,
                result.trades,
                bh
            );
        } else {
            flat += 1;
            println!(
                "{} → {}: no combination had {MIN_TRADES} trades, stayed flat",
                scored.start, traded.start
            );
        }
        round["total"] = json!(round2(total));
        round["buy_and_hold_total"] = json!(round2(bh_total));
        rounds.push(round);
    }
    println!(
        "\nTotal after fees: {total:+.2} USDT over {} windows ({positive} profitable, {flat} flat); buy-and-hold {bh_total:+.2} USDT",
        n - 1
    );

    let mut report = json!({
        "meta": {
            "start": meta.start.to_string(),
            "end": meta.end.to_string(),
            "days": meta.days,
            "hours": meta.window_hours(),
            "timeframes": meta.timeframes,
            "pairs": meta.pairs,
            "lengths": meta.lengths.len(),
            "tp": [meta.tp.first(), meta.tp.last(), meta.tp.len()],
            "sl": [meta.sl.first(), meta.sl.last(), meta.sl.len()],
            "leverage": [meta.leverage.first(), meta.leverage.last(), meta.leverage.len()],
            "combinations": meta.combinations,
            "windows": n,
            "size_btc": meta.size_btc,
            "taker_fee_bps": meta.taker_fee_bps,
            "maker_fee_bps": meta.maker_fee_bps,
            "min_trades": MIN_TRADES,
            "min_drop": MIN_DROP,
            "generated_utc": Utc::now().format("%Y-%m-%d %H:%M").to_string(),
        },
        "summary": { "total": round2(total), "buy_and_hold_total": round2(bh_total), "rounds": n - 1, "positive": positive, "flat": flat },
        "rounds": rounds,
        "check": Value::Null,
    });
    let base = args.scan.join("walk_forward");
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
        "Report (open in a browser): {}",
        base.with_extension("html").display()
    );

    if args.check {
        let checked = check_picks(&meta, &args, &config, &picks)?;
        report["check"] = json!({ "combinations": checked, "identical": true });
        save(&report)?;
        println!(
            "{checked} combinations traded, replayed again by the trade-by-trade scanner: identical"
        );
    }
    Ok(())
}

/// Replays every pick through the trade-by-trade scanner over the window it traded, and
/// compares the result with window_scan's.
fn check_picks(
    meta: &Meta,
    args: &Args,
    config: &AppConfig,
    picks: &[(usize, u32, WindowStats)],
) -> Result<usize> {
    let base_steps = (meta.size_btc * 10_000.0).round() as i64;
    let taker = config.venues.okx.taker_fee_bps / 10_000.0;
    let maker = config.venues.okx.maker_fee_bps / 10_000.0;
    ensure!(
        (taker * 10_000.0 - meta.taker_fee_bps).abs() < 1e-9
            && (maker * 10_000.0 - meta.maker_fee_bps).abs() < 1e-9,
        "the fees in {} differ from the scan's",
        args.config.display()
    );
    println!("Reading every OKX trade again to check the combinations traded...");
    let (minutes, tape) = Tape::load(&args.data, 60_000, ms(meta.start), ms(meta.end), |_, _| {})?;
    let end_ms = ms(meta.end);
    let step_of = |t: i64| {
        let i = minutes.partition_point(|c| c.time_ms < t);
        tape.bar_start
            .get(i)
            .copied()
            .unwrap_or(tape.price.len() as u32)
    };
    for &(w, id, expected) in picks {
        let window = &meta.windows[w];
        let (f, i, j, tp, sl, li) = meta.decode(id);
        let lev = meta.leverage[li];
        let costs = Costs {
            steps: base_steps * i64::from(lev),
            taker,
            maker,
            balance: 1e9,
        };
        let qty = costs.steps as f64 * STEP_BTC;
        let frame = Frame::from_minutes(&minutes, &tape.bar_start, meta.timeframes[f]);
        let rsi = rsi_series(&[meta.lengths[i], meta.lengths[j]], &frame.candles);
        let signals = scan::signals(&rsi[0], &rsi[1]);
        let closes = CloseRanges::new(&frame.candles.iter().map(|c| c.close).collect::<Vec<_>>());
        let market = Market {
            candles: &frame.candles,
            tape: &tape,
            bar_start: &frame.bar_start,
            closes: &closes,
        };
        let start_ms = parse_window_ms(&window.start)?;
        let stop_ms = parse_window_ms(&window.end)?;
        let (from, to) = (
            step_of(start_ms),
            if stop_ms >= end_ms {
                tape.price.len() as u32
            } else {
                step_of(stop_ms)
            },
        );
        let first = frame.bar_start.partition_point(|&b| b < from);
        let last = frame.bar_start.partition_point(|&b| b < to);
        let exits = Exits {
            stop_pct: meta.sl[sl],
            targets: vec![(meta.tp[tp], 1.0)],
            breakeven: false,
        };
        let mut record = Record::default();
        scan::simulate(
            (0, 0, 0),
            &signals,
            Some(&exits),
            &market,
            first.saturating_sub(1)..last,
            costs,
            Some(&mut record),
        );
        let nets: Vec<f64> = record
            .trades
            .iter()
            .map(|t| {
                let sign = if t.long { 1.0 } else { -1.0 };
                let gain: f64 = t
                    .exits
                    .iter()
                    .map(|e| sign * (e.price - t.entry_price) * e.steps as f64 * STEP_BTC - e.fee)
                    .sum();
                gain - t.entry_fee
                    - if t.open {
                        costs.taker * qty * window.last_price
                    } else {
                        0.0
                    }
            })
            .collect();
        let profit: f64 = nets.iter().sum();
        let wins = nets.iter().filter(|x| **x > 0.0).count();
        let worst = nets.iter().copied().fold(0.0, f64::min);
        let name = format!("{} in {}", meta.label(id), window.start);
        ensure!(
            nets.len() == usize::from(expected.trades),
            "{name}: scanner {} trades, the scan {}",
            nets.len(),
            expected.trades
        );
        ensure!(
            wins == usize::from(expected.wins),
            "{name}: scanner {wins} winning trades, the scan {}",
            expected.wins
        );
        ensure!(
            (profit - f64::from(expected.profit)).abs() < 1e-2,
            "{name}: scanner {profit:.4} USDT, the scan {}",
            expected.profit
        );
        ensure!(
            (worst - f64::from(expected.worst)).abs() < 1e-3,
            "{name}: scanner biggest loss {worst:.4}, the scan {}",
            expected.worst
        );
    }
    Ok(picks.len())
}

const REPORT_HTML: &str = include_str!("window_walk_forward_report.html");
const TABLE_SORT: &str = include_str!("table_sort.js");

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(lengths: usize, tp: usize, sl: usize, lev: usize) -> Meta {
        let pairs = lengths * (lengths - 1) / 2;
        Meta {
            start: "2025-10-01".parse().unwrap(),
            end: "2025-10-21".parse().unwrap(),
            days: 10,
            hours: None,
            timeframes: vec![1, 60],
            lengths: (1..=lengths).collect(),
            pairs,
            tp: (1..=tp).map(|k| k as f64 / 100.0).collect(),
            sl: (1..=sl).map(|k| k as f64 / 100.0).collect(),
            leverage: (1..=lev as u32).collect(),
            settings: tp * sl * lev,
            combinations: 2 * pairs * tp * sl * lev,
            windows: Vec::new(),
            size_btc: 0.005,
            taker_fee_bps: 5.0,
            maker_fee_bps: 2.0,
            survivor_groups: None,
        }
    }

    /// Ids are laid out as window_scan writes them.
    #[test]
    fn decodes_ids_like_the_scan_lays_them_out() {
        let m = meta(6, 3, 4, 2);
        let mut id = 0u32;
        for f in 0..2 {
            for i in 0..6 {
                for j in i + 1..6 {
                    for tp in 0..3 {
                        for sl in 0..4 {
                            for lev in 0..2 {
                                assert_eq!(m.decode(id), (f, i, j, tp, sl, lev));
                                id += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(id as usize, m.combinations);
    }

    #[test]
    fn ranks_by_score_then_profit_then_id() {
        let dir = std::env::temp_dir().join(format!("wwf_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w.bin");
        let s = |profit: f32, trades: u16| WindowStats {
            profit,
            drop: 5.0,
            worst: -1.0,
            hold_hours: 1.0,
            trades,
            wins: trades,
            ..Default::default()
        };
        let all = [
            s(10.0, 10),
            s(20.0, 10),
            s(20.0, 4),
            s(-5.0, 10),
            s(20.0, 10),
            s(0.0, 0),
        ];
        fs::write(
            &path,
            all.iter().flat_map(|x| x.bytes()).collect::<Vec<u8>>(),
        )
        .unwrap();
        let r = rank(&path, all.len(), 3).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            r.top.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![1, 4, 0]
        );
        assert_eq!((r.traded, r.eligible, r.profitable), (5, 4, 4));
    }
}
