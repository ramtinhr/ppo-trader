//! Filled-order types shared by the fast scanner and the Nautilus verifier.
//!
//! Kept out of [`crate::backtest`] so the scanner can compile without linking Nautilus.

use serde::{Deserialize, Serialize};

/// What a filled order was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FillKind {
    /// Market order opening or reversing a position on a signal.
    Signal,
    TakeProfit,
    StopLoss,
}

/// One filled order of a backtest or scanner replay.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub time_ms: i64,
    pub kind: FillKind,
    pub buy: bool,
    pub size_btc: f64,
    pub price: f64,
    pub fee_usdt: f64,
}
