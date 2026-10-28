//! Candle / market-data price path (not RSI trade stats).
//!
//! From the last finished candle (causal features):
//!   prev_return, range, body, typical_dev
//! predict the close `horizon` bars ahead:
//!
//! `(close_{t+h} / close_t) − 1 = Σ αᵢ fᵢ`   (one OLS per horizon h)
//! `pred_close = close_t · (1 + Σ αᵢ fᵢ)`
//!
//! h=1 is the usual next candle; h=20 is twenty candles ahead.
//! Train gate (lock): return-MAE on h=1 under `--max-rel-err` and beat naive `pred = ref`.

use crate::okx_history::Candle;
use crate::window_score::MIN_FIT_SAMPLES;

pub const PRICE_FEATURES: usize = 4;
pub const PRICE_FEATURE_NAMES: [&str; PRICE_FEATURES] =
    ["prev_return", "range", "body", "typical_dev"];

/// One row: features of finished bar `t`, spot = close_{t+h}, ref = close_t.
#[derive(Debug, Clone, Copy)]
pub struct PriceSample {
    pub features: [f64; PRICE_FEATURES],
    pub spot: f64,
    pub ref_px: f64,
    /// Time of the target bar (`t+h`), when the outcome is known.
    pub at_ms: i64,
    /// Time of the feature bar (`t`), when the forecast is made.
    pub origin_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceModel {
    pub alphas: [f64; PRICE_FEATURES],
    pub mae: f64,
    /// mean(|y − Σαf|) on returns.
    pub rel_err: f64,
    pub price_rel_err: f64,
    pub baseline_rel_err: f64,
    pub samples: usize,
    pub cold_start: bool,
}

impl PriceModel {
    pub fn cold_start(_typical_price: f64) -> PriceModel {
        PriceModel {
            alphas: [0.0; PRICE_FEATURES],
            mae: 0.0,
            rel_err: 0.0,
            price_rel_err: 0.0,
            baseline_rel_err: 0.0,
            samples: 0,
            cold_start: true,
        }
    }

    pub fn predict_return(&self, x: &[f64; PRICE_FEATURES]) -> f64 {
        let mut s = 0.0;
        for (a, f) in self.alphas.iter().zip(x.iter()) {
            s += a * (*f);
        }
        s
    }

    pub fn predict_close(&self, ref_px: f64, x: &[f64; PRICE_FEATURES]) -> f64 {
        (ref_px * (1.0 + self.predict_return(x))).max(0.0)
    }

    pub fn ok(&self, max_rel_err: f64) -> bool {
        !self.cold_start
            && self.rel_err < max_rel_err
            && self.price_rel_err + 1e-12 < self.baseline_rel_err
    }

    fn eval_metrics(samples: &[PriceSample], alphas: &[f64; PRICE_FEATURES]) -> (f64, f64, f64, f64) {
        let mut ret_sum = 0.0;
        let mut price_sum = 0.0;
        let mut base_sum = 0.0;
        let mut abs_sum = 0.0;
        let mut n = 0usize;
        for s in samples {
            if !s.spot.is_finite() || s.spot <= 0.0 || !s.ref_px.is_finite() || s.ref_px <= 0.0 {
                continue;
            }
            let mut rhat = 0.0;
            for (a, f) in alphas.iter().zip(s.features.iter()) {
                rhat += a * f;
            }
            let y = s.spot / s.ref_px - 1.0;
            let pred = s.ref_px * (1.0 + rhat);
            ret_sum += (y - rhat).abs();
            price_sum += (pred - s.spot).abs() / s.spot;
            base_sum += (s.ref_px - s.spot).abs() / s.spot;
            abs_sum += (pred - s.spot).abs();
            n += 1;
        }
        if n == 0 {
            return (f64::INFINITY, f64::INFINITY, f64::INFINITY, f64::INFINITY);
        }
        let n = n as f64;
        (ret_sum / n, price_sum / n, base_sum / n, abs_sum / n)
    }

    pub fn relative_error(&self, samples: &[PriceSample]) -> f64 {
        Self::eval_metrics(samples, &self.alphas).0
    }

    pub fn mean_abs_error(&self, samples: &[PriceSample]) -> f64 {
        Self::eval_metrics(samples, &self.alphas).3
    }

    pub fn fit(
        samples: &[PriceSample],
        warm: Option<&PriceModel>,
        _typical_price: f64,
    ) -> Result<PriceModel, String> {
        if samples.len() < MIN_FIT_SAMPLES {
            let mut m = warm
                .cloned()
                .unwrap_or_else(|| PriceModel::cold_start(1.0));
            m.cold_start = warm.map(|w| w.cold_start).unwrap_or(true);
            if !samples.is_empty() {
                let (ret, price, base, mae) = Self::eval_metrics(samples, &m.alphas);
                m.rel_err = ret;
                m.price_rel_err = price;
                m.baseline_rel_err = base;
                m.mae = mae;
                m.samples = samples.len();
            }
            return Ok(m);
        }

        let mut ata = [[0.0; PRICE_FEATURES]; PRICE_FEATURES];
        let mut aty = [0.0; PRICE_FEATURES];
        let mut n = 0usize;
        for s in samples {
            if !s.spot.is_finite() || s.spot <= 0.0 || !s.ref_px.is_finite() || s.ref_px <= 0.0 {
                continue;
            }
            let y = s.spot / s.ref_px - 1.0;
            let x = s.features;
            for i in 0..PRICE_FEATURES {
                aty[i] += x[i] * y;
                for j in 0..PRICE_FEATURES {
                    ata[i][j] += x[i] * x[j];
                }
            }
            n += 1;
        }
        if n < MIN_FIT_SAMPLES {
            return Err("not enough candle samples".into());
        }
        for i in 0..PRICE_FEATURES {
            ata[i][i] += 1e-6;
        }
        let alphas = solve4(ata, aty).ok_or_else(|| "price OLS singular".to_string())?;
        let (ret, price, base, mae) = Self::eval_metrics(samples, &alphas);
        let mut model = PriceModel {
            alphas,
            mae,
            rel_err: ret,
            price_rel_err: price,
            baseline_rel_err: base,
            samples: samples.len(),
            cold_start: false,
        };
        if let Some(prev) = warm {
            if !prev.cold_start && prev.relative_error(samples) < model.rel_err {
                let (ret, price, base, mae) = Self::eval_metrics(samples, &prev.alphas);
                model.alphas = prev.alphas;
                model.rel_err = ret;
                model.price_rel_err = price;
                model.baseline_rel_err = base;
                model.mae = mae;
            }
        }
        Ok(model)
    }
}

/// Features of candle `prev` (and prior close for return). Values are raw fractions (not clamped).
pub fn candle_features(prev: &Candle, prev_prev_close: f64) -> Option<[f64; PRICE_FEATURES]> {
    if prev.open <= 0.0 || prev.close <= 0.0 || prev_prev_close <= 0.0 {
        return None;
    }
    if !prev.high.is_finite() || !prev.low.is_finite() {
        return None;
    }
    let prev_return = prev.close / prev_prev_close - 1.0;
    let range = (prev.high - prev.low) / prev.close;
    let body = (prev.close - prev.open) / prev.open;
    let typical = (prev.high + prev.low + prev.close) / 3.0;
    let typical_dev = typical / prev.close - 1.0;
    if ![prev_return, range, body, typical_dev]
        .iter()
        .all(|v| v.is_finite())
    {
        return None;
    }
    Some([prev_return, range, body, typical_dev])
}

/// Aggregate 1m candles into larger bars (`bar_ms`), skipping empty buckets.
pub fn aggregate_candles(minutes: &[Candle], bar_ms: i64) -> Vec<Candle> {
    if minutes.is_empty() || bar_ms <= 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur: Option<Candle> = None;
    let mut bucket = i64::MIN;
    for c in minutes {
        let b = c.time_ms.div_euclid(bar_ms) * bar_ms;
        match &mut cur {
            Some(acc) if b == bucket => {
                acc.high = acc.high.max(c.high);
                acc.low = acc.low.min(c.low);
                acc.close = c.close;
            }
            _ => {
                if let Some(done) = cur.take() {
                    out.push(done);
                }
                bucket = b;
                cur = Some(Candle {
                    time_ms: b,
                    open: c.open,
                    high: c.high,
                    low: c.low,
                    close: c.close,
                });
            }
        }
    }
    if let Some(done) = cur {
        out.push(done);
    }
    out
}

/// Build samples for horizon `h` (≥1): features(bar t) → close_{t+h}, ref = close_t.
pub fn samples_from_candles_horizon(bars: &[Candle], horizon: usize) -> Vec<PriceSample> {
    let mut out = Vec::new();
    if horizon == 0 || bars.len() <= horizon + 1 {
        return out;
    }
    // t = index of last finished bar used for features (needs t-1 for prev_return).
    for t in 1..bars.len().saturating_sub(horizon) {
        let Some(features) = candle_features(&bars[t], bars[t - 1].close) else {
            continue;
        };
        let target = &bars[t + horizon];
        out.push(PriceSample {
            features,
            spot: target.close,
            ref_px: bars[t].close,
            at_ms: target.time_ms,
            origin_ms: bars[t].time_ms,
        });
    }
    out
}

/// Horizon-1 samples (next candle only).
pub fn samples_from_candles(bars: &[Candle]) -> Vec<PriceSample> {
    samples_from_candles_horizon(bars, 1)
}

fn solve4(mut a: [[f64; 4]; 4], mut b: [f64; 4]) -> Option<[f64; 4]> {
    for col in 0..4 {
        let mut piv = col;
        for r in col + 1..4 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-18 {
            return None;
        }
        if piv != col {
            a.swap(piv, col);
            b.swap(piv, col);
        }
        let diag = a[col][col];
        for r in col + 1..4 {
            let f = a[r][col] / diag;
            for c in col..4 {
                a[r][c] -= f * a[col][c];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = [0.0; 4];
    for i in (0..4).rev() {
        let mut s = b[i];
        for j in i + 1..4 {
            s -= a[i][j] * x[j];
        }
        x[i] = s / a[i][i];
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ols_on_candle_returns() {
        let true_a = [0.3, 0.1, 0.2, 0.05];
        let mut bars = Vec::new();
        let mut close = 50_000.0;
        for k in 0..50 {
            let open = close;
            let high = open * 1.01;
            let low = open * 0.99;
            close = open * (1.0 + 0.001 * ((k % 5) as f64 - 2.0));
            bars.push(Candle {
                time_ms: k as i64 * 3_600_000,
                open,
                high,
                low,
                close,
            });
        }
        // Build samples then overwrite spot using true linear return on features.
        let mut samples = samples_from_candles(&bars);
        for s in &mut samples {
            let mut r = 0.0;
            for (a, f) in true_a.iter().zip(s.features.iter()) {
                r += a * f;
            }
            s.spot = s.ref_px * (1.0 + r);
        }
        let model = PriceModel::fit(&samples, None, 50_000.0).expect("fit");
        assert!(!model.cold_start);
        assert!(model.rel_err < 0.01, "mae={}", model.rel_err);
        assert!(model.ok(0.01));
    }

    #[test]
    fn horizon_20_samples_align() {
        let mut bars = Vec::new();
        let mut close = 100.0;
        for k in 0..40 {
            close += 1.0;
            bars.push(Candle {
                time_ms: k as i64 * 60_000,
                open: close - 0.5,
                high: close + 1.0,
                low: close - 1.0,
                close,
            });
        }
        let h1 = samples_from_candles_horizon(&bars, 1);
        let h20 = samples_from_candles_horizon(&bars, 20);
        assert!(!h1.is_empty());
        assert!(!h20.is_empty());
        assert_eq!(h20[0].origin_ms, bars[1].time_ms);
        assert_eq!(h20[0].at_ms, bars[21].time_ms);
        assert_eq!(h20[0].ref_px, bars[1].close);
        assert_eq!(h20[0].spot, bars[21].close);
    }

    #[test]
    fn aggregate_hourly_from_minutes() {
        let mut mins = Vec::new();
        for k in 0..120 {
            mins.push(Candle {
                time_ms: k * 60_000,
                open: 100.0 + k as f64,
                high: 101.0 + k as f64,
                low: 99.0 + k as f64,
                close: 100.5 + k as f64,
            });
        }
        let hours = aggregate_candles(&mins, 3_600_000);
        assert_eq!(hours.len(), 2);
        assert_eq!(hours[0].time_ms, 0);
        assert_eq!(hours[1].time_ms, 3_600_000);
    }
}
