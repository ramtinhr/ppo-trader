//! Compare scanner fills with Nautilus fills (shared by the verify binary and walk-forward tools).

use serde_json::{Value, json};

use crate::fills::Fill;
use crate::scan::STEP_BTC;

/// Sort key used by every historical verify copy: time, kind, side, price ticks, size steps.
pub fn fill_key(f: &Fill) -> (i64, crate::fills::FillKind, bool, i64, i64) {
    (
        f.time_ms,
        f.kind,
        f.buy,
        (f.price * 10.0).round() as i64,
        (f.size_btc / STEP_BTC).round() as i64,
    )
}

pub fn fills_equal(a: &Fill, b: &Fill) -> bool {
    a.time_ms == b.time_ms
        && a.kind == b.kind
        && a.buy == b.buy
        && (a.size_btc - b.size_btc).abs() < 1e-9
        && (a.price - b.price).abs() < 1e-6
        && (a.fee_usdt - b.fee_usdt).abs() < 1e-6
}

/// Sort both sides, print the first few diffs, and return a JSON summary.
pub fn compare_fills(label: &str, mut ours: Vec<Fill>, mut theirs: Vec<Fill>, seconds: f64) -> Value {
    ours.sort_by_key(fill_key);
    theirs.sort_by_key(fill_key);
    let bad: Vec<usize> = (0..ours.len().max(theirs.len()))
        .filter(|&k| {
            !matches!(
                (ours.get(k), theirs.get(k)),
                (Some(a), Some(b)) if fills_equal(a, b)
            )
        })
        .collect();
    for &k in bad.iter().take(3) {
        eprintln!(
            "    fill {}: scanner {:?} | Nautilus {:?}",
            k + 1,
            ours.get(k),
            theirs.get(k)
        );
    }
    eprintln!(
        "  {} fills, {} differ (time, kind, side, size, price, fee) ({seconds:.0}s)",
        theirs.len(),
        bad.len()
    );
    json!({
        "pair": label,
        "fills": theirs.len(),
        "differ": bad.len(),
        "ok": bad.is_empty(),
        "seconds": seconds,
    })
}
