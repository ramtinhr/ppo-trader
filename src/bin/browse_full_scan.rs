use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

const USAGE: &str = "\
Browse every combination full_scan replayed, in the browser: filter by timeframe, RSI lengths,
take-profit, stop-loss, month and trades, sort by score, profit, drop or trades, page through
the results, download them as CSV, and see any combination month by month.

Usage: cargo run --release --bin browse_full_scan -- [options]

  --scan <dir>      full_scan results (default: data/fullscan/2025-10-01_2026-10-01)
  --port <port>     Local port (default: 8090)
  --min-drop <usdt> Smallest drop the score divides by, 1% of the balance (default: 5)";

/// Bytes per combination and month in full_scan's files.
const RECORD: usize = 12;
/// Results kept for paging and CSV, best first.
const MAX_ROWS: usize = 1_000_000;

#[derive(Deserialize)]
struct Meta {
    timeframes: Vec<usize>,
    lengths: Vec<usize>,
    tp: Vec<f64>,
    sl: Vec<f64>,
    #[serde(default = "default_leverage")]
    leverage: Vec<u32>,
    combinations: usize,
    every_pair: usize,
    months: Vec<String>,
}

fn default_leverage() -> Vec<u32> {
    vec![1]
}

struct Scan {
    meta: Meta,
    pairs: Vec<(usize, usize)>,
    settings: usize,
    /// One file per month, then the whole period.
    files: Vec<File>,
    min_drop: f64,
}

#[derive(Debug, Clone, Copy)]
struct Row {
    id: u32,
    profit: f32,
    drop: f32,
    trades: u32,
    score: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct Filter {
    period: usize,
    tf: Option<usize>,
    fast: Option<usize>,
    slow: Option<usize>,
    tp: Option<usize>,
    sl: Option<usize>,
    lev: Option<usize>,
    min_trades: u32,
    profitable: bool,
    sort: Vec<(String, bool)>,
}

impl Scan {
    fn decode(&self, id: u32) -> (usize, usize, usize, usize, usize) {
        let id = id as usize;
        let (group, s) = (id / self.settings, id % self.settings);
        let n_sl = self.meta.sl.len();
        let n_lev = self.meta.leverage.len().max(1);
        let lev = s % n_lev;
        let tpsl = s / n_lev;
        (
            group / self.pairs.len(),
            group % self.pairs.len(),
            tpsl / n_sl,
            tpsl % n_sl,
            lev,
        )
    }

    fn row_json(&self, rank: usize, r: &Row) -> Value {
        let (f, p, tp, sl, li) = self.decode(r.id);
        let (i, j) = self.pairs[p];
        json!({
            "rank": rank, "id": r.id, "tf": self.meta.timeframes[f], "fast": self.meta.lengths[i], "slow": self.meta.lengths[j],
            "tp": self.meta.tp[tp], "sl": self.meta.sl[sl], "leverage": self.meta.leverage[li],
            "profit": round2(r.profit), "drop": round2(r.drop), "score": round2(r.score), "trades": r.trades,
        })
    }

    fn read(&self, period: usize, id: u32) -> Result<Row> {
        let mut b = [0u8; RECORD];
        self.files[period].read_exact_at(&mut b, id as u64 * RECORD as u64)?;
        Ok(self.row(id, &b))
    }

    fn row(&self, id: u32, b: &[u8]) -> Row {
        let f = |k: usize| f32::from_le_bytes(b[k..k + 4].try_into().unwrap());
        let (profit, drop, trades) = (f(0), f(4), u32::from_le_bytes(b[8..12].try_into().unwrap()));
        Row {
            id,
            profit,
            drop,
            trades,
            score: (f64::from(profit) / f64::from(drop).max(self.min_drop)) as f32,
        }
    }

    /// Every matching combination, the best `MAX_ROWS` kept in order, and how many matched.
    fn query(&self, q: &Filter) -> Result<(Vec<Row>, usize)> {
        let key = |r: &Row, name: &str| -> f64 {
            match name {
                "profit" => f64::from(r.profit),
                "drop" => f64::from(r.drop),
                "trades" => f64::from(r.trades),
                "tf" | "fast" | "slow" | "tp" | "sl" | "leverage" => {
                    let (f, p, tp, sl, li) = self.decode(r.id);
                    let (i, j) = self.pairs[p];
                    match name {
                        "tf" => self.meta.timeframes[f] as f64,
                        "fast" => self.meta.lengths[i] as f64,
                        "slow" => self.meta.lengths[j] as f64,
                        "tp" => self.meta.tp[tp],
                        "sl" => self.meta.sl[sl],
                        _ => f64::from(self.meta.leverage[li]),
                    }
                }
                _ => f64::from(r.score),
            }
        };
        let order = |a: &Row, b: &Row| {
            q.sort
                .iter()
                .fold(std::cmp::Ordering::Equal, |o, (name, ascending)| {
                    o.then_with(|| {
                        let c = key(a, name).total_cmp(&key(b, name));
                        if *ascending { c } else { c.reverse() }
                    })
                })
                .then(a.id.cmp(&b.id))
        };
        // Groups (timeframe x RSI pair) to read: each one's settings are next to each other.
        let groups: Vec<usize> = (0..self.meta.timeframes.len())
            .filter(|&f| q.tf.is_none_or(|t| self.meta.timeframes[f] == t))
            .flat_map(|f| {
                (0..self.pairs.len())
                    .filter(|&p| {
                        let (i, j) = self.pairs[p];
                        q.fast.is_none_or(|v| self.meta.lengths[i] == v)
                            && q.slow.is_none_or(|v| self.meta.lengths[j] == v)
                    })
                    .map(move |p| f * self.pairs.len() + p)
            })
            .collect();
        let n_sl = self.meta.sl.len();
        let n_lev = self.meta.leverage.len().max(1);
        let n_sl_lev = n_sl * n_lev;
        let file = &self.files[q.period];
        let next = AtomicUsize::new(0);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let parts: Vec<(Vec<Row>, usize)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| -> Result<(Vec<Row>, usize)> {
                        let mut buf = vec![0u8; self.settings * RECORD];
                        let mut kept: Vec<Row> = Vec::new();
                        let mut matched = 0;
                        loop {
                            let g = next.fetch_add(1, AtomicOrdering::Relaxed);
                            let Some(&group) = groups.get(g) else { break };
                            let first = group * self.settings;
                            file.read_exact_at(&mut buf, (first * RECORD) as u64)?;
                            for s in 0..self.settings {
                                if q.tp.is_some_and(|t| s / n_sl_lev != t)
                                    || q.sl.is_some_and(|v| (s / n_lev) % n_sl != v)
                                    || q.lev.is_some_and(|v| s % n_lev != v)
                                {
                                    continue;
                                }
                                let r = self
                                    .row((first + s) as u32, &buf[s * RECORD..(s + 1) * RECORD]);
                                if r.trades == 0
                                    || r.trades < q.min_trades
                                    || (q.profitable && r.profit <= 0.0)
                                {
                                    continue;
                                }
                                matched += 1;
                                kept.push(r);
                                if kept.len() >= 2 * MAX_ROWS {
                                    kept.select_nth_unstable_by(MAX_ROWS - 1, order);
                                    kept.truncate(MAX_ROWS);
                                }
                            }
                        }
                        Ok((kept, matched))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Result<_>>()
        })?;
        let matched = parts.iter().map(|p| p.1).sum();
        let mut rows: Vec<Row> = parts.into_iter().flat_map(|p| p.0).collect();
        if rows.len() > MAX_ROWS {
            rows.select_nth_unstable_by(MAX_ROWS - 1, order);
            rows.truncate(MAX_ROWS);
        }
        rows.sort_by(order);
        Ok((rows, matched))
    }
}

fn round2(x: f32) -> f64 {
    (f64::from(x) * 100.0).round() / 100.0
}

fn parse_query(scan: &Scan, query: &str) -> Result<(Filter, usize, usize)> {
    let mut q = Filter {
        period: scan.meta.months.len(),
        tf: None,
        fast: None,
        slow: None,
        tp: None,
        sl: None,
        lev: None,
        min_trades: 0,
        profitable: false,
        sort: Vec::new(),
    };
    let (mut page, mut per) = (0, 100);
    let (mut sort, mut ascending) = (String::new(), false);
    let index = |values: &[f64], v: &str| -> Result<usize> {
        let x: f64 = v.parse()?;
        values
            .iter()
            .position(|y| (y - x).abs() < 1e-9)
            .with_context(|| format!("{v}% is not in the grid"))
    };
    let lev_index = |values: &[u32], v: &str| -> Result<usize> {
        let x: u32 = v.parse()?;
        values
            .iter()
            .position(|y| *y == x)
            .with_context(|| format!("leverage {v} is not in the grid"))
    };
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if v.is_empty() || v == "any" {
            continue;
        }
        match k {
            "period" => {
                q.period = if v == "all" {
                    scan.meta.months.len()
                } else {
                    scan.meta
                        .months
                        .iter()
                        .position(|m| m == v)
                        .with_context(|| format!("no month {v}"))?
                }
            }
            "tf" => q.tf = Some(v.parse()?),
            "fast" => q.fast = Some(v.parse()?),
            "slow" => q.slow = Some(v.parse()?),
            "tp" => q.tp = Some(index(&scan.meta.tp, v)?),
            "sl" => q.sl = Some(index(&scan.meta.sl, v)?),
            "leverage" => q.lev = Some(lev_index(&scan.meta.leverage, v)?),
            "min_trades" => q.min_trades = v.parse()?,
            "profitable" => q.profitable = v == "1",
            "sort" => sort = v.to_string(),
            "dir" => ascending = v == "asc",
            "page" => page = v.parse()?,
            "per" => per = v.parse::<usize>()?.clamp(1, 1000),
            _ => {}
        }
    }
    q.sort = parse_sort(&sort, ascending);
    Ok((q, page, per))
}

/// `sort=profit-desc.trades-asc` sorts by profit, then trades; a column without a direction uses `dir`.
fn parse_sort(sort: &str, ascending: bool) -> Vec<(String, bool)> {
    let keys: Vec<(String, bool)> = sort
        .split('.')
        .filter(|s| !s.is_empty())
        .map(|s| match s.rsplit_once('-') {
            Some((k, "asc")) => (k.to_string(), true),
            Some((k, "desc")) => (k.to_string(), false),
            _ => (s.to_string(), ascending),
        })
        .collect();
    if keys.is_empty() {
        vec![("score".to_string(), ascending)]
    } else {
        keys
    }
}

/// The last query's results, so paging does not read everything again.
type Cache = Mutex<Option<(Filter, Vec<Row>, usize)>>;

fn results(scan: &Scan, cache: &Cache, q: &Filter) -> Result<(Vec<Row>, usize, f64)> {
    let mut c = cache.lock().unwrap();
    if let Some((f, rows, matched)) = c.as_ref()
        && f == q
    {
        return Ok((rows.clone(), *matched, 0.0));
    }
    let started = Instant::now();
    let (rows, matched) = scan.query(q)?;
    *c = Some((q.clone(), rows.clone(), matched));
    Ok((rows, matched, started.elapsed().as_secs_f64()))
}

fn answer(
    scan: &Scan,
    cache: &Cache,
    path: &str,
    query: &str,
) -> Result<(u16, &'static str, String)> {
    Ok(match path {
        "/" => (
            200,
            "text/html; charset=utf-8",
            PAGE.replace("/*__TABLE_SORT__*/", TABLE_SORT),
        ),
        "/api/meta" => {
            let m = &scan.meta;
            (200, "application/json", json!({ "timeframes": m.timeframes, "lengths": m.lengths, "tp": m.tp, "sl": m.sl, "leverage": m.leverage, "months": m.months, "combinations": m.combinations, "min_drop": scan.min_drop, "max_rows": MAX_ROWS }).to_string())
        }
        "/api/query" => {
            let (q, page, per) = parse_query(scan, query)?;
            let (rows, matched, secs) = results(scan, cache, &q)?;
            let from = (page * per).min(rows.len());
            let shown: Vec<Value> = rows[from..(from + per).min(rows.len())]
                .iter()
                .enumerate()
                .map(|(k, r)| scan.row_json(from + k + 1, r))
                .collect();
            (200, "application/json", json!({ "matched": matched, "kept": rows.len(), "page": page, "per": per, "rows": shown, "seconds": secs }).to_string())
        }
        "/api/csv" => {
            let (q, ..) = parse_query(scan, query)?;
            let (rows, ..) = results(scan, cache, &q)?;
            let mut out = String::from(
                "rank,timeframe_min,rsi_fast,rsi_slow,tp_pct,sl_pct,leverage,profit_usdt,largest_drop_usdt,score,trades\n",
            );
            for (k, r) in rows.iter().enumerate() {
                let (f, p, tp, sl, li) = scan.decode(r.id);
                let (i, j) = scan.pairs[p];
                out.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{},{}\n",
                    k + 1,
                    scan.meta.timeframes[f],
                    scan.meta.lengths[i],
                    scan.meta.lengths[j],
                    scan.meta.tp[tp],
                    scan.meta.sl[sl],
                    scan.meta.leverage[li],
                    round2(r.profit),
                    round2(r.drop),
                    round2(r.score),
                    r.trades
                ));
            }
            (200, "text/csv", out)
        }
        _ if path.starts_with("/api/combo/") => {
            let id: u32 = path.trim_start_matches("/api/combo/").parse()?;
            ensure!(
                (id as usize) < scan.meta.combinations,
                "no combination {id}"
            );
            let months: Vec<Value> = (0..=scan.meta.months.len())
                .map(|m| {
                    let r = scan.read(m, id)?;
                    let name = scan.meta.months.get(m).cloned().unwrap_or_else(|| "whole period".into());
                    Ok(json!({ "period": name, "profit": round2(r.profit), "drop": round2(r.drop), "score": round2(r.score), "trades": r.trades }))
                })
                .collect::<Result<_>>()?;
            (200, "application/json", json!({ "combo": scan.row_json(0, &scan.read(scan.meta.months.len(), id)?), "months": months }).to_string())
        }
        _ => (404, "text/plain", "not found".into()),
    })
}

fn serve(stream: TcpStream, scan: &Scan, cache: &Cache) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
    }
    let target = request.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (code, kind, body) = answer(scan, cache, path, query).unwrap_or_else(|e| {
        (
            400,
            "application/json",
            json!({ "error": format!("{e:#}") }).to_string(),
        )
    });
    let mut stream = stream;
    let disposition = if kind == "text/csv" {
        "Content-Disposition: attachment; filename=\"combinations.csv\"\r\n"
    } else {
        ""
    };
    write!(
        stream,
        "HTTP/1.1 {code} {}\r\nContent-Type: {kind}\r\n{disposition}Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        if code == 200 { "OK" } else { "Error" },
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}

fn main() -> Result<()> {
    let (mut dir, mut port, mut min_drop): (PathBuf, u16, f64) =
        ("data/fullscan/2025-10-01_2026-10-01".into(), 8090, 5.0);
    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            return Ok(());
        }
        let value = raw
            .next()
            .with_context(|| format!("{flag} needs a value\n\n{USAGE}"))?;
        match flag.as_str() {
            "--scan" => dir = value.into(),
            "--port" => port = value.parse()?,
            "--min-drop" => min_drop = value.parse()?,
            _ => bail!("unknown option {flag}\n\n{USAGE}"),
        }
    }
    let meta: Meta = serde_json::from_str(
        &fs::read_to_string(dir.join("meta.json")).context("full_scan's meta.json")?,
    )?;
    ensure!(
        meta.every_pair == 1,
        "full_scan ran on every {}th RSI pair only",
        meta.every_pair
    );
    let pairs: Vec<(usize, usize)> = (0..meta.lengths.len())
        .flat_map(|i| (i + 1..meta.lengths.len()).map(move |j| (i, j)))
        .collect();
    let settings = meta.tp.len() * meta.sl.len() * meta.leverage.len().max(1);
    ensure!(
        meta.timeframes.len() * pairs.len() * settings == meta.combinations,
        "meta.json does not add up"
    );
    let files = meta
        .months
        .iter()
        .map(|m| dir.join(format!("month_{m}.bin")))
        .chain(std::iter::once(dir.join("all.bin")))
        .map(|p| File::open(&p).with_context(|| p.display().to_string()))
        .collect::<Result<_>>()?;
    let scan = Scan {
        meta,
        pairs,
        settings,
        files,
        min_drop,
    };
    let cache: Cache = Mutex::new(None);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("port {port} is busy; try --port {}", port + 1))?;
    println!("Every combination: http://127.0.0.1:{port}  (open it in a browser; Ctrl+C to stop)");
    std::thread::scope(|scope| {
        for stream in listener.incoming().flatten() {
            let (scan, cache) = (&scan, &cache);
            scope.spawn(move || {
                if let Err(e) = serve(stream, scan, cache) {
                    eprintln!("web request failed: {e:#}");
                }
            });
        }
    });
    Ok(())
}

const PAGE: &str = include_str!("browse_full_scan.html");
const TABLE_SORT: &str = include_str!("table_sort.js");
