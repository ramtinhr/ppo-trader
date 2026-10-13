//! Port of TradingView's built-in "RSI Strategy" Pine Script (v6):
//!
//! ```pine
//! vrsi = ta.rsi(close, length)
//! co = ta.crossover(vrsi, overSold)
//! cu = ta.crossunder(vrsi, overBought)
//! if (not na(vrsi))
//!     if (co)
//!         strategy.entry("RsiLE", strategy.long)
//!     if (cu)
//!         strategy.entry("RsiSE", strategy.short)
//! ```
//!
//! `strategy.entry` in the opposite direction reverses the position, and with the default
//! `pyramiding = 1` an entry in the current direction is ignored, so the strategy is always
//! either flat (before the first signal), long, or short.

use crate::{
    rsi::Rsi,
    strategy::{BarSignals, Signal},
};

#[derive(Debug, Clone, Copy)]
pub struct PineRsiParams {
    pub length: usize,
    pub oversold: f64,
    pub overbought: f64,
}

impl PineRsiParams {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.length > 0, "RSI length must be at least 1");
        anyhow::ensure!(
            0.0 <= self.oversold && self.oversold < self.overbought && self.overbought <= 100.0,
            "RSI levels must satisfy 0 <= oversold < overbought <= 100"
        );
        Ok(())
    }
}

/// `ta.crossover(x, level)`: above the level now, at or below it on the previous bar.
pub fn crossover(prev: f64, current: f64, level: f64) -> bool {
    current > level && prev <= level
}

/// `ta.crossunder(x, level)`: below the level now, at or above it on the previous bar.
pub fn crossunder(prev: f64, current: f64, level: f64) -> bool {
    current < level && prev >= level
}

/// Bar-by-bar signal logic of the Pine script, independent of Nautilus.
#[derive(Debug)]
pub struct PineRsiSignals {
    params: PineRsiParams,
    rsi: Rsi,
    prev_rsi: Option<f64>,
}

impl PineRsiSignals {
    pub fn new(params: PineRsiParams) -> Self {
        Self {
            params,
            rsi: Rsi::new(params.length),
            prev_rsi: None,
        }
    }

    /// Feeds one bar close and returns the RSI (once available) and any entry signal.
    pub fn update(&mut self, close: f64) -> (Option<f64>, Option<Signal>) {
        let Some(rsi) = self.rsi.update(close) else {
            return (None, None);
        };
        let signal = self.prev_rsi.and_then(|prev| {
            if crossover(prev, rsi, self.params.oversold) {
                Some(Signal::Long)
            } else if crossunder(prev, rsi, self.params.overbought) {
                Some(Signal::Short)
            } else {
                None
            }
        });
        self.prev_rsi = Some(rsi);
        (Some(rsi), signal)
    }
}

impl BarSignals for PineRsiSignals {
    fn on_bar(&mut self, bar: &crate::okx_history::Candle) -> Option<Signal> {
        self.update(bar.close).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: PineRsiParams = PineRsiParams {
        length: 2,
        oversold: 30.0,
        overbought: 70.0,
    };

    #[test]
    fn crossover_includes_touching_previous_bar() {
        assert!(crossover(30.0, 30.1, 30.0));
        assert!(crossover(25.0, 35.0, 30.0));
        assert!(!crossover(30.0, 30.0, 30.0));
        assert!(!crossover(31.0, 35.0, 30.0));
    }

    #[test]
    fn crossunder_includes_touching_previous_bar() {
        assert!(crossunder(70.0, 69.9, 70.0));
        assert!(crossunder(75.0, 65.0, 70.0));
        assert!(!crossunder(70.0, 70.0, 70.0));
        assert!(!crossunder(69.0, 60.0, 70.0));
    }

    #[test]
    fn first_rsi_value_never_signals() {
        let mut signals = PineRsiSignals::new(PARAMS);
        assert_eq!(signals.update(100.0), (None, None));
        assert_eq!(signals.update(90.0), (None, None));
        let (rsi, signal) = signals.update(80.0);
        assert_eq!(rsi, Some(0.0));
        assert_eq!(signal, None);
    }

    #[test]
    fn long_after_rsi_rises_through_oversold_then_short_through_overbought() {
        let mut signals = PineRsiSignals::new(PARAMS);
        for close in [100.0, 90.0, 80.0] {
            signals.update(close);
        }
        // RSI 0 -> above 30: long.
        let (rsi, signal) = signals.update(100.0);
        assert!(rsi.unwrap() > 30.0);
        assert_eq!(signal, Some(Signal::Long));

        // Push RSI to 100, then a drop takes it below 70: short.
        signals.update(130.0);
        let (rsi, signal) = signals.update(120.0);
        assert!(rsi.unwrap() < 70.0, "rsi was {rsi:?}");
        assert_eq!(signal, Some(Signal::Short));
    }
}
