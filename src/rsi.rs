/// Wilder's RSI, matching TradingView's `ta.rsi`: simple averages of gains and losses for the
/// first `period` changes, then smoothed with `(previous * (period - 1) + current) / period`.
#[derive(Debug)]
pub struct Rsi {
    period: usize,
    prev_close: Option<f64>,
    changes_seen: usize,
    avg_gain: f64,
    avg_loss: f64,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "RSI period must be positive");
        Self {
            period,
            prev_close: None,
            changes_seen: 0,
            avg_gain: 0.0,
            avg_loss: 0.0,
        }
    }

    /// Adds a close price and returns the RSI once `period` price changes have been seen.
    pub fn update(&mut self, close: f64) -> Option<f64> {
        let prev = self.prev_close.replace(close)?;
        let change = close - prev;
        let (gain, loss) = (change.max(0.0), (-change).max(0.0));
        let n = self.period as f64;

        self.changes_seen += 1;
        if self.changes_seen <= self.period {
            self.avg_gain += gain / n;
            self.avg_loss += loss / n;
            if self.changes_seen < self.period {
                return None;
            }
        } else {
            self.avg_gain = (self.avg_gain * (n - 1.0) + gain) / n;
            self.avg_loss = (self.avg_loss * (n - 1.0) + loss) / n;
        }

        // Same edge cases as TradingView's ta.rsi: no losses -> 100, then no gains -> 0.
        Some(if self.avg_loss == 0.0 {
            100.0
        } else if self.avg_gain == 0.0 {
            0.0
        } else {
            100.0 - 100.0 / (1.0 + self.avg_gain / self.avg_loss)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rsi_after(period: usize, closes: &[f64]) -> Option<f64> {
        let mut rsi = Rsi::new(period);
        closes.iter().fold(None, |_, close| rsi.update(*close))
    }

    #[test]
    fn matches_worked_example() {
        // Changes +100, -50, +200, +50, -100: average gain 70, average loss 30.
        let closes = [1000.0, 1100.0, 1050.0, 1250.0, 1300.0, 1200.0];
        let rsi = rsi_after(5, &closes).unwrap();
        assert!((rsi - 70.0).abs() < 1e-9, "rsi was {rsi}");
    }

    #[test]
    fn waits_for_enough_changes() {
        assert_eq!(rsi_after(5, &[1.0, 2.0, 3.0, 4.0, 5.0]), None);
    }

    #[test]
    fn applies_wilder_smoothing_after_first_window() {
        // Period 2: first window gains +10 and -10 -> 5 / 5, then a +10 change.
        let rsi = rsi_after(2, &[100.0, 110.0, 100.0, 110.0]).unwrap();
        let (gain, loss) = ((5.0 + 10.0) / 2.0, (5.0 + 0.0) / 2.0);
        assert!((rsi - (100.0 - 100.0 / (1.0 + gain / loss))).abs() < 1e-9);
    }

    #[test]
    fn period_one_is_all_or_nothing() {
        assert_eq!(rsi_after(1, &[100.0, 101.0]), Some(100.0));
        assert_eq!(rsi_after(1, &[100.0, 99.0]), Some(0.0));
        assert_eq!(rsi_after(1, &[100.0, 100.0]), Some(100.0));
    }
}
