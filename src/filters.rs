//! Filters that decide whether an RSI cross may open a position:
//!
//! - **Trend:** a long only while the close is above its EMA, a short only while it is below.
//! - **Volatility:** only while the ATR, as a % of the close, is at least a minimum, so the moves
//!   are big enough to pay the fees.
//!
//! A cross the filter rejects still closes the position it is against; it only opens nothing.
//! Until the EMA or ATR has a value, every cross is rejected.

use crate::{
    okx_history::Candle,
    rsi_cross::{RsiCrossParams, RsiCrossSignals},
    strategy::{BarSignals, Signal},
};

/// Bars of the ATR, TradingView's default.
pub const ATR_LENGTH: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Filter {
    /// EMA length of the trend filter, in bars.
    pub trend: Option<usize>,
    /// Minimum ATR in % of the close.
    pub min_atr_pct: Option<f64>,
}

impl Filter {
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = self.trend {
            parts.push(format!("Trend EMA {n}"));
        }
        if let Some(a) = self.min_atr_pct {
            parts.push(format!("ATR ≥ {a}%"));
        }
        if parts.is_empty() {
            "No filter".into()
        } else {
            parts.join(" · ")
        }
    }

    /// Whether a cross in direction `dir` (+1 long, -1 short) on a bar with this `close`, `ema`
    /// and `atr` may open a position. NaN means the indicator has no value yet.
    pub fn allows(&self, dir: i8, close: f64, ema: f64, atr: f64) -> bool {
        let trend = self.trend.is_none() || if dir > 0 { close > ema } else { close < ema };
        let volatile = self
            .min_atr_pct
            .is_none_or(|min| atr / close * 100.0 >= min);
        trend && volatile
    }
}

/// TradingView's `ta.ema`: `alpha = 2 / (length + 1)`, starting from the simple average of the
/// first `length` closes.
#[derive(Debug, Clone)]
pub struct Ema {
    length: usize,
    seen: usize,
    value: f64,
}

impl Ema {
    pub fn new(length: usize) -> Self {
        assert!(length > 0, "EMA length must be positive");
        Self {
            length,
            seen: 0,
            value: 0.0,
        }
    }

    pub fn update(&mut self, x: f64) -> Option<f64> {
        self.seen += 1;
        let n = self.length as f64;
        if self.seen <= self.length {
            self.value += x / n;
            return (self.seen == self.length).then_some(self.value);
        }
        let alpha = 2.0 / (n + 1.0);
        self.value = alpha * x + (1.0 - alpha) * self.value;
        Some(self.value)
    }
}

/// TradingView's `ta.atr`: Wilder's average of the true range, starting from the simple average
/// of the first `length` true ranges; the first bar's true range is its high - low.
#[derive(Debug, Clone)]
pub struct Atr {
    length: usize,
    prev_close: Option<f64>,
    seen: usize,
    value: f64,
}

impl Atr {
    pub fn new(length: usize) -> Self {
        assert!(length > 0, "ATR length must be positive");
        Self {
            length,
            prev_close: None,
            seen: 0,
            value: 0.0,
        }
    }

    pub fn update(&mut self, bar: &Candle) -> Option<f64> {
        let tr = match self.prev_close.replace(bar.close) {
            None => bar.high - bar.low,
            Some(c) => (bar.high - bar.low)
                .max((bar.high - c).abs())
                .max((bar.low - c).abs()),
        };
        self.seen += 1;
        let n = self.length as f64;
        if self.seen <= self.length {
            self.value += tr / n;
            return (self.seen == self.length).then_some(self.value);
        }
        self.value = (self.value * (n - 1.0) + tr) / n;
        Some(self.value)
    }
}

/// EMA of every close; NaN until it has a value.
pub fn ema_series(candles: &[Candle], length: usize) -> Vec<f64> {
    let mut ema = Ema::new(length);
    candles
        .iter()
        .map(|c| ema.update(c.close).unwrap_or(f64::NAN))
        .collect()
}

/// ATR of every bar; NaN until it has a value.
pub fn atr_series(candles: &[Candle], length: usize) -> Vec<f64> {
    let mut atr = Atr::new(length);
    candles
        .iter()
        .map(|c| atr.update(c).unwrap_or(f64::NAN))
        .collect()
}

/// RSI crosses through a [`Filter`]: a rejected cross becomes [`Signal::Flat`].
#[derive(Debug)]
pub struct FilteredCross {
    cross: RsiCrossSignals,
    filter: Filter,
    ema: Option<Ema>,
    atr: Atr,
}

impl FilteredCross {
    pub fn new(params: RsiCrossParams, filter: Filter) -> Self {
        Self {
            cross: RsiCrossSignals::new(params),
            filter,
            ema: filter.trend.map(Ema::new),
            atr: Atr::new(ATR_LENGTH),
        }
    }
}

impl BarSignals for FilteredCross {
    fn on_bar(&mut self, bar: &Candle) -> Option<Signal> {
        let ema = self
            .ema
            .as_mut()
            .and_then(|e| e.update(bar.close))
            .unwrap_or(f64::NAN);
        let atr = self.atr.update(bar).unwrap_or(f64::NAN);
        let signal = self.cross.on_bar(bar)?;
        let dir = if signal == Signal::Long { 1 } else { -1 };
        Some(if self.filter.allows(dir, bar.close, ema, atr) {
            signal
        } else {
            Signal::Flat
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(high: f64, low: f64, close: f64) -> Candle {
        Candle {
            time_ms: 0,
            open: close,
            high,
            low,
            close,
        }
    }

    #[test]
    fn ema_starts_from_the_simple_average() {
        let mut ema = Ema::new(3);
        assert_eq!(ema.update(1.0), None);
        assert_eq!(ema.update(2.0), None);
        assert_eq!(ema.update(3.0), Some(2.0));
        assert_eq!(ema.update(6.0), Some(4.0));
    }

    #[test]
    fn atr_uses_the_true_range() {
        let mut atr = Atr::new(2);
        assert_eq!(atr.update(&bar(10.0, 8.0, 9.0)), None);
        // Gap up: true range is high - previous close = 14 - 9.
        assert_eq!(atr.update(&bar(14.0, 12.0, 13.0)), Some((2.0 + 5.0) / 2.0));
        assert_eq!(atr.update(&bar(14.0, 13.0, 13.5)), Some((3.5 + 1.0) / 2.0));
    }

    #[test]
    fn filter_needs_the_trend_and_enough_volatility() {
        let f = Filter {
            trend: Some(50),
            min_atr_pct: Some(0.5),
        };
        assert!(f.allows(1, 100.0, 99.0, 0.5));
        assert!(!f.allows(-1, 100.0, 99.0, 0.5));
        assert!(!f.allows(1, 100.0, 99.0, 0.4));
        assert!(!f.allows(1, 100.0, f64::NAN, 1.0));
        assert!(Filter::default().allows(-1, 100.0, f64::NAN, f64::NAN));
        assert_eq!(f.label(), "Trend EMA 50 · ATR ≥ 0.5%");
    }
}
