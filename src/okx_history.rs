//! Every OKX BTC-USDT-SWAP trade, from OKX's free daily archives.
//!
//! OKX names archives by UTC+8 day: the archive for day D holds trades from D-1 16:00 UTC to
//! D 16:00 UTC, the same window as OKX's official 1D candle for D. An archive is accepted only
//! if its trade IDs have no gaps and its traded contracts equal that candle's volume exactly.
//!
//! A few archived trades are missing from OKX's own trade API and candles: OKX voided them after
//! the fact. They are listed in [`VOIDED`] and skipped everywhere, so the candles match OKX's
//! (and TradingView's).
//!
//! A few archives are known to be incomplete and are accepted anyway, see [`KNOWN_INCOMPLETE`]:
//! - 2024-02-20: one ID gap 751214804→751214809 (4 missing); volume short 65 contracts.
//! - 2024-12-01: two ID gaps around 2024-12-01 02:10 UTC (1,317 + 13,870 missing IDs); volume
//!   short 122,843.7 contracts vs OKX's 1D candle.
//! - 2026-01-14: two ID gaps in the minute from 2026-01-13 22:10 UTC; volume short 44,527.47.
//!   That minute's high/low still match OKX's 1m candle, so prices are complete.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use ureq::Proxy;

/// Optional override for [`http`], set once from the download CLI (`--proxy`).
static PROXY_OVERRIDE: OnceLock<String> = OnceLock::new();

/// Force every OKX HTTP call through this proxy URL (e.g. `socks5://127.0.0.1:1080`).
/// Call before the first request. Ignored if already set.
pub fn set_proxy(url: impl Into<String>) {
    let url = url.into();
    let _ = PROXY_OVERRIDE.set(url);
}

/// Proxy URL from CLI override or env (`OKX_PROXY`, `ALL_PROXY`, `all_proxy`, `SOCKS5_PROXY`).
pub fn proxy_url() -> Option<String> {
    if let Some(url) = PROXY_OVERRIDE.get() {
        return Some(url.clone());
    }
    for key in ["OKX_PROXY", "ALL_PROXY", "all_proxy", "SOCKS5_PROXY"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

pub const INSTRUMENT: &str = "BTC-USDT-SWAP";
/// BTC per contract.
pub const CONTRACT_VALUE: f64 = 0.01;
const ARCHIVE_URL: &str = "https://static.okx.com/cdn/okex/traderecords/trades/daily";
const CANDLE_URL: &str = "https://www.okx.com/api/v5/market/history-candles";
const HOUR_MS: i64 = 3_600_000;

/// Trade IDs in OKX's archives that OKX's trade API (`history-trades`) and candles leave out,
/// sorted. 2026-08-01 10:01:37.501 UTC: three sells at 63036.3, 5.78 contracts in total.
const VOIDED: &[u64] = &[2_818_465_146, 2_818_465_147, 2_818_465_148];

fn voided(trade_id: u64) -> bool {
    VOIDED.binary_search(&trade_id).is_ok()
}

/// One OKX archive day that is incomplete in the published zip but still usable for prices.
struct KnownIncomplete {
    /// Archive day `(year, month, day)`.
    day: (i32, u32, u32),
    /// Allowed trade-ID jumps `(last_id_before, first_id_after)`.
    gaps: &'static [(u64, u64)],
    /// Contracts missing vs OKX's official 1D candle, in hundredths.
    shortfall_hundredths: u64,
}

/// Published archives that fail a strict ID/volume check but are accepted with a fixed shortfall.
const KNOWN_INCOMPLETE: &[KnownIncomplete] = &[
    // 2024-02-20 15:29 UTC: IDs 751214804→751214809 (4 missing); archive short 65 vs 1D candle.
    KnownIncomplete {
        day: (2024, 2, 20),
        gaps: &[(751_214_804, 751_214_809)],
        shortfall_hundredths: 6_500, // 65.00 contracts
    },
    // 2024-12-01 02:10 UTC: two bursts of missing IDs; archive 7,944,393.30 vs candle 8,067,237.
    KnownIncomplete {
        day: (2024, 12, 1),
        gaps: &[(1_159_375_039, 1_159_376_357), (1_159_405_059, 1_159_418_930)],
        shortfall_hundredths: 12_284_370, // 122,843.70 contracts
    },
    // 2026-01-14: gaps 2105958516→2105979022 and 2106031988→2106045535 (2026-01-13 22:10 UTC).
    // Missing volume 44,527.47; that minute's high/low still match OKX's 1m candle.
    KnownIncomplete {
        day: (2026, 1, 14),
        gaps: &[(2_105_958_516, 2_105_979_022), (2_106_031_988, 2_106_045_535)],
        shortfall_hundredths: 4_452_747,
    },
];

fn known_incomplete(day: NaiveDate) -> Option<&'static KnownIncomplete> {
    KNOWN_INCOMPLETE.iter().find(|k| {
        let (y, m, d) = k.day;
        NaiveDate::from_ymd_opt(y, m, d) == Some(day)
    })
}

fn known_gap(day: NaiveDate, last_id: u64, next_id: u64) -> bool {
    known_incomplete(day).is_some_and(|k| k.gaps.contains(&(last_id, next_id)))
}

fn known_gap_shortfall(day: NaiveDate) -> u64 {
    known_incomplete(day).map(|k| k.shortfall_hundredths).unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trade {
    pub trade_id: u64,
    pub buy: bool,
    pub price: f64,
    /// Contracts, in steps of 0.01 (0.0001 BTC).
    pub contracts: f64,
    pub time_ms: i64,
}

/// What a verified archive contained, saved next to it so later runs skip the full check.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DaySummary {
    pub first_id: u64,
    pub last_id: u64,
    pub trades: u64,
    /// Total contracts in hundredths, equal to OKX's official 1D candle volume.
    pub volume_hundredths: u64,
}

pub fn archive_path(dir: &Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{INSTRUMENT}-trades-{day}.zip"))
}

fn summary_path(dir: &Path, day: NaiveDate) -> PathBuf {
    dir.join(format!("{INSTRUMENT}-trades-{day}.verified.json"))
}

/// Whether archive `day` has been downloaded and passed [`fetch_day`]'s checks.
pub fn is_verified(dir: &Path, day: NaiveDate) -> bool {
    archive_path(dir, day).exists() && summary_path(dir, day).exists()
}

/// First and last millisecond (exclusive) of archive `day`, in UTC.
pub fn archive_window_ms(day: NaiveDate) -> (i64, i64) {
    let start = day
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
        - 8 * HOUR_MS;
    (start, start + 24 * HOUR_MS)
}

/// Archive days holding every trade from UTC day `start` (inclusive) to `end` (exclusive).
pub fn archive_days(start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    start.iter_days().take_while(|d| *d <= end).collect()
}

/// Downloads (if needed) and verifies archive `day`. `previous` is the verified summary of the
/// day before, if known, so IDs are also checked across archives.
pub fn fetch_day(dir: &Path, day: NaiveDate, previous: Option<&DaySummary>) -> Result<DaySummary> {
    fs::create_dir_all(dir)?;
    let path = archive_path(dir, day);
    let summary_file = summary_path(dir, day);
    let summary = match fs::read_to_string(&summary_file) {
        Ok(text) if path.exists() => serde_json::from_str(&text)?,
        _ => {
            if !path.exists() {
                download(day, &path)?;
            }
            let summary = verify(day, &path)?;
            fs::write(&summary_file, serde_json::to_string(&summary)?)?;
            summary
        }
    };
    if let Some(previous) = previous {
        ensure!(
            summary.first_id == previous.last_id + 1,
            "{day}: trade IDs jump from {} (previous archive) to {}",
            previous.last_id,
            summary.first_id
        );
    }
    Ok(summary)
}

/// HTTP client whose requests fail instead of hanging forever when OKX stalls.
/// Routes through [`proxy_url`] when set (SOCKS5 / HTTP CONNECT).
fn http() -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .timeout_global(Some(std::time::Duration::from_secs(600)));
    if let Some(url) = proxy_url() {
        let proxy = Proxy::new(&url).unwrap_or_else(|e| {
            panic!("invalid OKX proxy URL {url:?}: {e:#} (example: socks5://127.0.0.1:1080)")
        });
        builder = builder.proxy(Some(proxy));
    }
    builder.build().new_agent()
}

fn download(day: NaiveDate, path: &Path) -> Result<()> {
    let url = format!(
        "{ARCHIVE_URL}/{:04}{:02}{:02}/{}",
        day.year(),
        day.month(),
        day.day(),
        path.file_name().unwrap().to_string_lossy()
    );
    println!("Downloading {url}");
    let fetch = || -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        http()
            .get(&url)
            .call()
            .with_context(|| {
                format!("download {url} (OKX publishes a day's archive the next day)")
            })?
            .into_body()
            .into_reader()
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    // OKX's CDN drops long downloads now and then.
    let mut attempt = 0;
    let bytes = loop {
        attempt += 1;
        match fetch() {
            Ok(bytes) => break bytes,
            Err(e) if attempt >= 5 => return Err(e),
            Err(e) => {
                println!("  attempt {attempt} failed ({e:#}), retrying");
                std::thread::sleep(std::time::Duration::from_secs(5 * attempt));
            }
        }
    };
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn verify(day: NaiveDate, path: &Path) -> Result<DaySummary> {
    let (start, end) = archive_window_ms(day);
    let mut summary: Option<DaySummary> = None;
    for_each_trade(path, |t| {
        ensure!(
            (start..end).contains(&t.time_ms),
            "{day}: trade {} at {} ms is outside the archive's day",
            t.trade_id,
            t.time_ms
        );
        let hundredths = (t.contracts * 100.0).round() as u64;
        match summary.as_mut() {
            None => {
                summary = Some(DaySummary {
                    first_id: t.trade_id,
                    last_id: t.trade_id,
                    trades: 1,
                    volume_hundredths: hundredths,
                })
            }
            Some(s) => {
                ensure!(
                    t.trade_id > s.last_id
                        && ((s.last_id + 1..t.trade_id).all(voided)
                            || known_gap(day, s.last_id, t.trade_id)),
                    "{day}: trade IDs jump from {} to {}",
                    s.last_id,
                    t.trade_id
                );
                s.last_id = t.trade_id;
                s.trades += 1;
                s.volume_hundredths += hundredths;
            }
        }
        Ok(())
    })?;
    let summary = summary.with_context(|| format!("{day}: archive has no trades"))?;
    let official = official_volume_hundredths(day)?;
    let shortfall = known_gap_shortfall(day);
    ensure!(
        summary.volume_hundredths + shortfall == official,
        "{day}: archive has {} contracts (+ {} known missing) but OKX's daily candle says {}",
        summary.volume_hundredths as f64 / 100.0,
        shortfall as f64 / 100.0,
        official as f64 / 100.0
    );
    if shortfall > 0 {
        println!(
            "{day}: {} trades, IDs {}..{} without gaps except the known ones, {} contracts = OKX daily candle - {} known missing",
            summary.trades,
            summary.first_id,
            summary.last_id,
            summary.volume_hundredths as f64 / 100.0,
            shortfall as f64 / 100.0
        );
    } else {
        println!(
            "{day}: {} trades, IDs {}..{} without gaps, {} contracts = OKX daily candle",
            summary.trades,
            summary.first_id,
            summary.last_id,
            summary.volume_hundredths as f64 / 100.0
        );
    }
    Ok(summary)
}

/// Volume of OKX's official 1D candle for archive `day`, in hundredths of a contract.
fn official_volume_hundredths(day: NaiveDate) -> Result<u64> {
    let (start, _) = archive_window_ms(day);
    let reply: serde_json::Value = http()
        .get(CANDLE_URL)
        .query("instId", INSTRUMENT)
        .query("bar", "1D")
        .query("after", (start + 1).to_string())
        .query("limit", "1")
        .call()?
        .body_mut()
        .read_json()?;
    let candle = &reply["data"][0];
    ensure!(
        candle[0].as_str() == Some(start.to_string().as_str()),
        "{day}: OKX returned no daily candle ({reply})"
    );
    ensure!(
        candle[8] == "1",
        "{day}: OKX's daily candle is not final yet"
    );
    hundredths(candle[5].as_str().context("candle volume")?)
}

/// OKX's official candle of `bar_ms` opening at `time_ms`, for bars OKX publishes
/// (1, 3, 5, 15, 30 minutes; 1, 2, 4 hours).
pub fn official_candle(bar_ms: i64, time_ms: i64) -> Result<Candle> {
    let minutes = bar_ms / 60_000;
    let bar = match minutes {
        1 | 3 | 5 | 15 | 30 => format!("{minutes}m"),
        60 | 120 | 240 => format!("{}H", minutes / 60),
        _ => bail!("OKX publishes no {minutes}-minute candles"),
    };
    let fetch = || -> Result<serde_json::Value> {
        Ok(http()
            .get(CANDLE_URL)
            .query("instId", INSTRUMENT)
            .query("bar", &bar)
            .query("after", (time_ms + 1).to_string())
            .query("limit", "1")
            .call()?
            .body_mut()
            .read_json()?)
    };
    // OKX drops connections and rate-limits (20 requests / 2 s) now and then.
    let mut attempt = 0;
    let reply = loop {
        attempt += 1;
        match fetch() {
            Ok(reply) if reply["code"] == "0" => break reply,
            Ok(reply) if attempt >= 5 => bail!("OKX candle request failed: {reply}"),
            Err(e) if attempt >= 5 => return Err(e),
            _ => std::thread::sleep(std::time::Duration::from_secs(attempt)),
        }
    };
    let c = &reply["data"][0];
    ensure!(
        c[0].as_str() == Some(time_ms.to_string().as_str()),
        "OKX returned no {bar} candle at {time_ms} ({reply})"
    );
    let f = |i: usize| -> Result<f64> { Ok(c[i].as_str().context("candle field")?.parse()?) };
    Ok(Candle {
        time_ms,
        open: f(1)?,
        high: f(2)?,
        low: f(3)?,
        close: f(4)?,
    })
}

/// Official OKX candles whose open is in `from_ms..=to_ms`, oldest first.
pub fn official_candles(bar_ms: i64, from_ms: i64, to_ms: i64) -> Result<Vec<Candle>> {
    if to_ms < from_ms {
        return Ok(Vec::new());
    }
    let minutes = bar_ms / 60_000;
    let bar = match minutes {
        1 | 3 | 5 | 15 | 30 => format!("{minutes}m"),
        60 | 120 | 240 => format!("{}H", minutes / 60),
        _ => bail!("OKX publishes no {minutes}-minute candles"),
    };
    let parse = |c: &serde_json::Value| -> Result<Candle> {
        let time_ms: i64 = c[0].as_str().context("candle ts")?.parse()?;
        let f = |i: usize| -> Result<f64> { Ok(c[i].as_str().context("candle field")?.parse()?) };
        Ok(Candle {
            time_ms,
            open: f(1)?,
            high: f(2)?,
            low: f(3)?,
            close: f(4)?,
        })
    };
    let mut out = Vec::new();
    let mut after = to_ms + bar_ms + 1;
    while after > from_ms {
        let fetch = || -> Result<serde_json::Value> {
            Ok(http()
                .get(CANDLE_URL)
                .query("instId", INSTRUMENT)
                .query("bar", &bar)
                .query("after", after.to_string())
                .query("limit", "100")
                .call()?
                .body_mut()
                .read_json()?)
        };
        let mut attempt = 0;
        let reply = loop {
            attempt += 1;
            match fetch() {
                Ok(reply) if reply["code"] == "0" => break reply,
                Ok(reply) if attempt >= 5 => bail!("OKX candle request failed: {reply}"),
                Err(e) if attempt >= 5 => return Err(e),
                _ => std::thread::sleep(std::time::Duration::from_secs(attempt)),
            }
        };
        let data = reply["data"].as_array().cloned().unwrap_or_default();
        if data.is_empty() {
            break;
        }
        let mut oldest = after;
        for c in &data {
            let candle = parse(c)?;
            oldest = oldest.min(candle.time_ms);
            if (from_ms..=to_ms).contains(&candle.time_ms) {
                out.push(candle);
            }
        }
        if data.len() < 100 || oldest <= from_ms {
            break;
        }
        after = oldest;
    }
    out.sort_by_key(|c| c.time_ms);
    out.dedup_by_key(|c| c.time_ms);
    Ok(out)
}

/// Parses a decimal with at most 2 decimal places into hundredths, exactly.
fn hundredths(text: &str) -> Result<u64> {
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    ensure!(frac.len() <= 2, "{text} has more than 2 decimals");
    Ok(whole.parse::<u64>()? * 100 + format!("{frac:0<2}").parse::<u64>()?)
}

/// Calls `f` for every trade in a downloaded archive, in trade ID order, except [`VOIDED`] ones.
pub fn for_each_trade(path: &Path, mut f: impl FnMut(Trade) -> Result<()>) -> Result<()> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)?;
    let reader = BufReader::new(archive.by_index(0)?);
    for (n, line) in reader.lines().enumerate().skip(1) {
        let line = line?;
        let trade = parse_line(&line)
            .with_context(|| format!("{} line {}: {line:?}", path.display(), n + 1))?;
        if !voided(trade.trade_id) {
            f(trade)?;
        }
    }
    Ok(())
}

/// `instrument_name,trade_id,side,price,size,created_time,source`
fn parse_line(line: &str) -> Result<Trade> {
    let mut fields = line.split(',');
    let mut next = || fields.next().context("missing column");
    ensure!(next()? == INSTRUMENT, "unexpected instrument");
    let trade_id = next()?.parse()?;
    let buy = match next()? {
        "buy" => true,
        "sell" => false,
        other => bail!("unknown side {other:?}"),
    };
    Ok(Trade {
        trade_id,
        buy,
        price: next()?.parse()?,
        contracts: next()?.parse()?,
        time_ms: next()?.parse()?,
    })
}

/// One candle built from trades; `time_ms` is its open time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candle {
    pub time_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// Candles of `bar_ms` from every trade between `from_ms` and `to_ms` (exclusive), reading the
/// verified archives in `dir`. Bars without trades are skipped.
pub fn candles(dir: &Path, bar_ms: i64, from_ms: i64, to_ms: i64) -> Result<Vec<Candle>> {
    let mut out: Vec<Candle> = Vec::new();
    let first_day = utc8_day(from_ms);
    let last_day = utc8_day(to_ms - 1);
    for day in first_day.iter_days().take_while(|d| *d <= last_day) {
        let path = archive_path(dir, day);
        ensure!(
            is_verified(dir, day),
            "{} is missing or unverified; run download_okx_trades first",
            path.display()
        );
        for_each_trade(&path, |t| {
            if t.time_ms < from_ms || t.time_ms >= to_ms {
                return Ok(());
            }
            let open_ms = t.time_ms - t.time_ms.rem_euclid(bar_ms);
            match out.last_mut() {
                Some(c) if c.time_ms == open_ms => {
                    c.high = c.high.max(t.price);
                    c.low = c.low.min(t.price);
                    c.close = t.price;
                }
                Some(c) if c.time_ms > open_ms => {
                    bail!(
                        "trade {} at {} ms is out of time order",
                        t.trade_id,
                        t.time_ms
                    )
                }
                _ => out.push(Candle {
                    time_ms: open_ms,
                    open: t.price,
                    high: t.price,
                    low: t.price,
                    close: t.price,
                }),
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// The archive day holding the trade at `time_ms`.
pub fn utc8_day(time_ms: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp_millis(time_ms + 8 * HOUR_MS)
        .expect("valid timestamp")
        .date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_day_covers_utc8_day() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let (start, end) = archive_window_ms(day);
        assert_eq!(start, 1_789_401_600_000); // 2026-09-14 16:00 UTC
        assert_eq!(end - start, 24 * HOUR_MS);
        assert_eq!(utc8_day(start), day);
        assert_eq!(utc8_day(end - 1), day);
    }

    #[test]
    fn parses_archive_line() {
        let t = parse_line("BTC-USDT-SWAP,2916314144,buy,78536.7,19.73,1789401600155,0").unwrap();
        assert_eq!(t.trade_id, 2_916_314_144);
        assert!(t.buy);
        assert_eq!(t.price, 78_536.7);
        assert_eq!(t.contracts, 19.73);
        assert_eq!(t.time_ms, 1_789_401_600_155);
    }

    #[test]
    fn known_gaps_only_on_their_day() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 14).unwrap();
        assert!(known_gap(day, 2_105_958_516, 2_105_979_022));
        assert!(known_gap(day, 2_106_031_988, 2_106_045_535));
        assert!(!known_gap(day, 2_105_958_516, 2_105_979_023));
        assert!(!known_gap(day, 2_105_958_517, 2_105_979_022));
        assert!(!known_gap(
            day.succ_opt().unwrap(),
            2_105_958_516,
            2_105_979_022
        ));
        let d2024 = NaiveDate::from_ymd_opt(2024, 12, 1).unwrap();
        assert!(known_gap(d2024, 1_159_375_039, 1_159_376_357));
        assert!(known_gap(d2024, 1_159_405_059, 1_159_418_930));
        assert_eq!(known_gap_shortfall(d2024), 12_284_370);
        assert!(!known_gap(d2024.succ_opt().unwrap(), 1_159_375_039, 1_159_376_357));
        let d_feb = NaiveDate::from_ymd_opt(2024, 2, 20).unwrap();
        assert!(known_gap(d_feb, 751_214_804, 751_214_809));
        assert_eq!(known_gap_shortfall(d_feb), 6_500);
    }

    #[test]
    fn parses_volume_exactly() {
        assert_eq!(hundredths("10001876.01").unwrap(), 1_000_187_601);
        assert_eq!(hundredths("5.1").unwrap(), 510);
        assert_eq!(hundredths("7").unwrap(), 700);
        assert!(hundredths("1.234").is_err());
    }
}
