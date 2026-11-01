//! Unified scanner: `--bucket window` (default) or `--bucket month`.
//!
//! Preserves the on-disk layouts of `window_scan` (20-byte records) and `full_scan`
//! (12-byte records) so existing walk-forward / browse tools keep working.

mod month;
mod window;

use anyhow::{Result, bail};

fn main() -> Result<()> {
    let mut bucket = "window".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--bucket" {
            bucket = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--bucket needs window|month"))?;
            break;
        }
    }
    // Submodules also see --bucket in argv; their parse_args ignore it.
    match bucket.as_str() {
        "window" => window::run(),
        "month" => month::run(),
        other => bail!("unknown --bucket {other}; use window or month"),
    }
}
