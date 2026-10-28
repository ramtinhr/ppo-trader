//! Two-RSI cross strategy, as this Pine Script would run it:
//!
//! ```pine
//! x = ta.rsi(close, fastLength)
//! y = ta.rsi(close, slowLength)
//! if ta.crossover(x, y)
//!     strategy.entry("Long", strategy.long)
//! if ta.crossunder(x, y)
//!     strategy.entry("Short", strategy.short)
//! ```
//!
//! Swapping the two lengths gives exactly the opposite signals, so only `fast < slow` pairs
//! need testing.

use crate::{
    okx_history::Candle,
    rsi::Rsi,
    strategy::{BarSignals, Signal},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsiCrossParams {
    pub fast: usize,
    pub slow: usize,
}

impl RsiCrossParams {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            0 < self.fast && self.fast < self.slow,
            "RSI lengths must satisfy 0 < fast < slow"
        );
        Ok(())
    }
}

/// `ta.crossover(x, y)` is long, `ta.crossunder(x, y)` is short. `x[1] == y[1]` counts as a cross
/// in either direction, like Pine.
pub fn cross_signal(prev_x: f64, prev_y: f64, x: f64, y: f64) -> Option<Signal> {
    if x > y && prev_x <= prev_y {
        Some(Signal::Long)
    } else if x < y && prev_x >= prev_y {
        Some(Signal::Short)
    } else {
        None
    }
}

#[derive(Debug)]
pub struct RsiCrossSignals {
    fast: Rsi,
    slow: Rsi,
    prev: Option<(f64, f64)>,
}

impl RsiCrossSignals {
    pub fn new(params: RsiCrossParams) -> Self {
        Self {
            fast: Rsi::new(params.fast),
            slow: Rsi::new(params.slow),
            prev: None,
        }
    }
}

impl BarSignals for RsiCrossSignals {
    fn on_bar(&mut self, bar: &Candle) -> Option<Signal> {
        let close = bar.close;
        let (x, y) = (self.fast.update(close), self.slow.update(close));
        let (x, y) = (x?, y?);
        let signal = self
            .prev
            .and_then(|(prev_x, prev_y)| cross_signal(prev_x, prev_y, x, y));
        self.prev = Some((x, y));
        signal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crosses_like_pine() {
        assert_eq!(cross_signal(40.0, 50.0, 55.0, 50.0), Some(Signal::Long));
        assert_eq!(cross_signal(50.0, 50.0, 50.1, 50.0), Some(Signal::Long));
        assert_eq!(cross_signal(60.0, 50.0, 45.0, 50.0), Some(Signal::Short));
        assert_eq!(cross_signal(50.0, 50.0, 49.9, 50.0), Some(Signal::Short));
        assert_eq!(cross_signal(60.0, 50.0, 70.0, 50.0), None);
        assert_eq!(cross_signal(50.0, 50.0, 50.0, 50.0), None);
    }

    fn close(signals: &mut RsiCrossSignals, close: f64) -> Option<Signal> {
        signals.on_bar(&Candle {
            time_ms: 0,
            open: close,
            high: close,
            low: close,
            close,
        })
    }

    #[test]
    fn waits_for_both_rsis_and_a_previous_bar() {
        let mut signals = RsiCrossSignals::new(RsiCrossParams { fast: 1, slow: 2 });
        // RSI(2) has its first value on the third close; the first cross can come on the fourth.
        assert_eq!(close(&mut signals, 100.0), None);
        assert_eq!(close(&mut signals, 101.0), None);
        assert_eq!(close(&mut signals, 100.0), None);
        // RSI(1) = 100 > RSI(2) after a rise, after being 0 < RSI(2): long.
        assert_eq!(close(&mut signals, 102.0), Some(Signal::Long));
        assert_eq!(close(&mut signals, 101.0), Some(Signal::Short));
    }
}
