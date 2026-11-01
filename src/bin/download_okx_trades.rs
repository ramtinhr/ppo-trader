//! Downloads and verifies every OKX BTC-USDT-SWAP trade for a range of UTC days.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate, Utc};
use dynamic_trader::okx_history::{archive_days, fetch_day, proxy_url, set_proxy};

const USAGE: &str = "\
Usage: cargo run --release --bin download_okx_trades -- [START END] [--dir <dir>] [--proxy <url>]

START and END are UTC days (YYYY-MM-DD), END exclusive. Default: the previous calendar month.
Archives go to data/okx/BTC-USDT-SWAP/archives by default.

When OKX is blocked on your network, route traffic through a local SOCKS5 (or HTTP) proxy:
  --proxy socks5://127.0.0.1:1080
  # or: export OKX_PROXY=socks5://127.0.0.1:1080
  # or: export ALL_PROXY=socks5://127.0.0.1:1080";

fn main() -> Result<()> {
    let mut days = Vec::new();
    let mut dir = PathBuf::from("data/okx/BTC-USDT-SWAP/archives");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--dir" => dir = args.next().context("--dir needs a value")?.into(),
            "--proxy" => {
                let url = args.next().context("--proxy needs a URL (e.g. socks5://127.0.0.1:1080)")?;
                set_proxy(url);
            }
            _ => days.push(
                arg.parse::<NaiveDate>()
                    .with_context(|| format!("{arg} is not YYYY-MM-DD\n\n{USAGE}"))?,
            ),
        }
    }
    let (start, end) = match days[..] {
        [] => {
            let today = Utc::now().date_naive();
            let this_month = today.with_day(1).unwrap();
            (this_month - chrono::Months::new(1), this_month)
        }
        [start, end] if start < end => (start, end),
        _ => bail!("{USAGE}"),
    };

    match proxy_url() {
        Some(url) => println!("OKX traffic via proxy {url}"),
        None => println!(
            "No proxy set. If downloads fail (timeout/403), re-run with \
             --proxy socks5://127.0.0.1:1080 (or export OKX_PROXY)"
        ),
    }

    let mut previous = None;
    for day in archive_days(start, end) {
        previous = Some(fetch_day(&dir, day, previous.as_ref())?);
    }
    println!(
        "All trades from {start} to {end} (UTC, end exclusive) are downloaded and verified: \
         trade IDs continuous across every archive and each day's volume equals OKX's daily candle \
         (except the documented gaps of 2026-01-14, if in range)"
    );
    Ok(())
}
