//! Results of one combination over one window of days, and the score that ranks them.
//!
//! Legacy heuristic (still used as cold-start / feature seed):
//! Score = profit / max(largest drop, 5 USDT) × trusted win rate × risk × hold time.
//!
//! Learned ranking uses [`ScoreModel`]:
//! `S = w₀·wr + w₁·gain + w₂·(R/R) + w₃·until + w₄·drawdown + w₅·n_trades`
//! with each factor in `[0, 1]`, `wᵢ ≥ 0`, and `Σ wᵢ = `[`SCORE_BUDGET`]` (10).
//! `until` = how far the last close sits into the lookback (UI “Last trade”); 1 = just at `at`.
//! Perfect factors → S = 10.
//! Fit is **IS → OOS signal**: features(last N) → S*(next M).
//! S* is the user's budget-10 score on the **OOS** window (cold-start·f of next M), not the
//! same IS block (that was a tautology). Predict uses budget-10 `Σ w·f` on IS; gate under
//! [`MAX_SCORE_REL_ERR`] (1%) means the line forecasts OOS score well enough to LOCK.

/// Trades a combination needs in a window to be scored.
pub const MIN_TRADES: u32 = 5;
/// Smallest drop a score divides by, in USDT.
pub const MIN_DROP: f64 = 5.0;
/// Bytes of one [`WindowStats`] on disk (PnL fields only; until clock is in-memory).
pub const RECORD: usize = 20;
/// Hard gate: mean(|S − S*|) / 10 must stay below this.
pub const MAX_SCORE_REL_ERR: f64 = 0.01;
/// Feature count: winrate, gain, risk/reward, until, drawdown, n trades.
pub const SCORE_FEATURES: usize = 6;
/// Point budget shared by the weights: `Σ wᵢ = SCORE_BUDGET` so max S is 10.
pub const SCORE_BUDGET: f64 = 10.0;
/// Minimum same-window samples before the 1% gate is mandatory.
pub const MIN_FIT_SAMPLES: usize = 8;

/// One combination over one window, every trade counted with its fees.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WindowStats {
    /// Profit after fees, in USDT.
    pub profit: f32,
    /// Largest drop of the profit from a high, counted trade by trade, in USDT.
    pub drop: f32,
    /// Biggest losing trade, as a negative USDT amount, or 0 without a loss.
    pub worst: f32,
    /// Average time a trade is held, in hours.
    pub hold_hours: f32,
    pub trades: u16,
    /// Trades that made money after fees.
    pub wins: u16,
    /// Exit time of the last trade in this window (UI `until`); 0 if unknown.
    pub last_exit_ms: i64,
    /// Score clock: pick time `at` (for until factor); 0 if unknown.
    pub score_at_ms: i64,
    /// Lookback length in ms used with `score_at_ms`; 0 if unknown.
    pub lookback_ms: i64,
}

impl WindowStats {
    pub fn win_rate(&self) -> f64 {
        if self.trades == 0 {
            0.0
        } else {
            f64::from(self.wins) / f64::from(self.trades)
        }
    }

    /// Lower end of the 95% confidence range of the win rate (Wilson).
    pub fn trusted_win_rate(&self) -> f64 {
        let n = f64::from(self.trades);
        if n == 0.0 {
            return 0.0;
        }
        let z = 1.96f64;
        let p = f64::from(self.wins) / n;
        let z2 = z * z;
        (p + z2 / (2.0 * n) - z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt()) / (1.0 + z2 / n)
    }

    /// Profit / (profit + biggest loss), for a profitable window.
    pub fn risk_factor(&self) -> f64 {
        let profit = f64::from(self.profit);
        if profit <= 0.0 {
            0.0
        } else {
            profit / (profit - f64::from(self.worst.min(0.0)))
        }
    }

    pub fn hold_factor(&self) -> f64 {
        1.0 / (1.0 + f64::from(self.hold_hours) / 24.0)
    }

    pub fn score(&self) -> Option<f64> {
        if u32::from(self.trades) < MIN_TRADES {
            return None;
        }
        let base = f64::from(self.profit) / f64::from(self.drop).max(MIN_DROP);
        if self.profit <= 0.0 {
            return Some(base);
        }
        Some(base * self.trusted_win_rate() * self.risk_factor() * self.hold_factor())
    }

    pub fn bytes(&self) -> [u8; RECORD] {
        let mut b = [0; RECORD];
        b[0..4].copy_from_slice(&self.profit.to_le_bytes());
        b[4..8].copy_from_slice(&self.drop.to_le_bytes());
        b[8..12].copy_from_slice(&self.worst.to_le_bytes());
        b[12..16].copy_from_slice(&self.hold_hours.to_le_bytes());
        b[16..18].copy_from_slice(&self.trades.to_le_bytes());
        b[18..20].copy_from_slice(&self.wins.to_le_bytes());
        b
    }

    pub fn from_bytes(b: &[u8]) -> WindowStats {
        let f = |k: usize| f32::from_le_bytes(b[k..k + 4].try_into().unwrap());
        let u = |k: usize| u16::from_le_bytes(b[k..k + 2].try_into().unwrap());
        WindowStats {
            profit: f(0),
            drop: f(4),
            worst: f(8),
            hold_hours: f(12),
            trades: u(16),
            wins: u(18),
            ..Default::default()
        }
    }

    /// Same trade path with position size scaled by `leverage` (PnL scales linearly).
    pub fn scale_leverage(self, leverage: u32) -> WindowStats {
        let l = leverage as f32;
        WindowStats {
            profit: self.profit * l,
            drop: self.drop * l,
            worst: self.worst * l,
            ..self
        }
    }

    /// Attach pick clock so the `until` factor matches UI “Last trade” freshness in lookback.
    pub fn with_score_clock(mut self, at: i64, lookback_ms: i64) -> WindowStats {
        self.score_at_ms = at;
        self.lookback_ms = lookback_ms;
        self
    }

    /// How far `last_exit_ms` sits into `[at - lookback, at]`; 1 = last trade at `at`.
    /// Neutral 0.5 when the clock is unknown (e.g. disk scan rows).
    pub fn until_factor(&self) -> f64 {
        if self.last_exit_ms <= 0 || self.score_at_ms <= 0 || self.lookback_ms <= 0 {
            return 0.5;
        }
        let start = self.score_at_ms - self.lookback_ms;
        ((self.last_exit_ms - start) as f64 / self.lookback_ms as f64).clamp(0.0, 1.0)
    }

    /// Factors for the budget-10 score — each in `[0, 1]`.
    /// Order: winrate, gain, risk/reward, until, drawdown, n trades.
    pub fn score_features(&self) -> [f64; SCORE_FEATURES] {
        let profit = f64::from(self.profit);
        let drop = f64::from(self.drop).max(MIN_DROP);
        let worst = f64::from(self.worst).min(0.0);
        let wr = self.win_rate().clamp(0.0, 1.0);
        let gain = (profit / 100.0).tanh().clamp(0.0, 1.0);
        // Risk/reward → [0, 1]: 0 if no edge, 1 at ~5× or better (or profit with no loss).
        let rr = if worst < 0.0 {
            (profit / (-worst)).clamp(0.0, 5.0) / 5.0
        } else if profit > 0.0 {
            1.0
        } else {
            0.0
        };
        let until = self.until_factor();
        // Drawdown: small peak-to-trough drop → closer to 1 (MIN_DROP counts as full).
        let drawdown = (MIN_DROP / drop).clamp(0.0, 1.0);
        let n = ((1.0 + f64::from(self.trades)).ln() / (1.0 + 200.0_f64).ln()).clamp(0.0, 1.0);
        [wr, gain, rr, until, drawdown, n]
    }

    /// Budget-10 quality of **this** window (user formula with cold-start reference weights).
    /// Used as S* on the OOS (next M) side of fit: predict from IS features → this score.
    pub fn target_quality(&self) -> f64 {
        if u32::from(self.trades) < MIN_TRADES {
            return 0.0;
        }
        ScoreModel::cold_start().predict_features(&self.score_features())
    }
}

/// Learned ranking: `S = Σ wᵢ fᵢ` with `fᵢ ∈ [0,1]`, `wᵢ ≥ 0`, `Σ wᵢ = SCORE_BUDGET` (10).
/// No intercept — perfect factors score exactly 10.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreModel {
    /// Point allocation across factors; always sums to [`SCORE_BUDGET`].
    pub weights: [f64; SCORE_FEATURES],
    /// Unused (always 0); kept for report JSON compatibility.
    pub intercept: f64,
    pub score_mae: f64,
    pub score_rel_err: f64,
    pub samples: usize,
    pub cold_start: bool,
}

impl ScoreModel {
    /// Default point split (Σ = 10): wr, gain, R/R, until, drawdown, n trades.
    pub fn cold_start() -> ScoreModel {
        ScoreModel {
            weights: [2.0, 1.5, 2.0, 1.0, 2.0, 1.5],
            intercept: 0.0,
            score_mae: 0.0,
            score_rel_err: 0.0,
            samples: 0,
            cold_start: true,
        }
    }

    pub fn score_ok(&self) -> bool {
        self.cold_start || self.score_rel_err < MAX_SCORE_REL_ERR
    }

    pub fn predict(&self, stats: &WindowStats) -> Option<f64> {
        if u32::from(stats.trades) < MIN_TRADES {
            return None;
        }
        Some(self.predict_features(&stats.score_features()))
    }

    pub fn predict_features(&self, x: &[f64; SCORE_FEATURES]) -> f64 {
        let mut s = 0.0;
        for (w, f) in self.weights.iter().zip(x.iter()) {
            s += w * f.clamp(0.0, 1.0);
        }
        s.clamp(0.0, SCORE_BUDGET)
    }

    /// Relative equation error on a labeled set: mean(|S − S*|) / 10.
    pub fn relative_error(&self, samples: &[(WindowStats, f64)]) -> f64 {
        if samples.is_empty() {
            return f64::INFINITY;
        }
        let mut sum = 0.0;
        let mut n = 0usize;
        for (stats, target) in samples {
            let Some(pred) = self.predict(stats) else {
                continue;
            };
            sum += (pred - target).abs();
            n += 1;
        }
        if n == 0 {
            f64::INFINITY
        } else {
            (sum / n as f64) / SCORE_BUDGET
        }
    }

    /// Fit weights on the simplex `w ≥ 0`, `Σ w = 10` (features(window) → S*(same window)).
    ///
    /// Always keeps the best weights (memory). Pass `warm` from the previous day so the
    /// optimizer continues toward the same line. The 1% gate is **not** applied here —
    /// use [`score_ok`] to decide LOCK; until then keep ranking with these weights.
    pub fn fit(
        samples: &[(WindowStats, f64)],
        warm: Option<&ScoreModel>,
    ) -> Result<ScoreModel, String> {
        if samples.len() < MIN_FIT_SAMPLES {
            let mut m = warm.cloned().unwrap_or_else(ScoreModel::cold_start);
            m.cold_start = warm.map(|w| w.cold_start).unwrap_or(true);
            if !samples.is_empty() {
                m.score_rel_err = m.relative_error(samples);
                m.score_mae = m.score_rel_err * SCORE_BUDGET;
                m.samples = samples.len();
            }
            return Ok(m);
        }
        let mut best: Option<ScoreModel> = None;
        for seed in budget_fit_seeds(warm.map(|m| m.weights)) {
            let Ok(model) = budget_fit(samples, seed) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|b| model.score_rel_err < b.score_rel_err)
            {
                best = Some(model);
            }
        }
        let Some(mut model) = best else {
            return Err("score fit failed for all budget-10 seeds".into());
        };
        // Never discard a worse day if warm was better on this sample set.
        if let Some(prev) = warm {
            if !prev.cold_start {
                let prev_err = prev.relative_error(samples);
                if prev_err < model.score_rel_err {
                    model.weights = prev.weights;
                    model.score_rel_err = prev_err;
                    model.score_mae = prev_err * SCORE_BUDGET;
                    model.samples = samples.len();
                    model.cold_start = false;
                }
            }
        }
        Ok(model)
    }
}

/// Build IS→OOS pairs: features(window i) → S*(window i+1).
pub fn fit_pairs_from_windows(windows: &[WindowStats]) -> Vec<(WindowStats, f64)> {
    windows
        .windows(2)
        .filter_map(|w| {
            if u32::from(w[0].trades) < MIN_TRADES || u32::from(w[1].trades) < MIN_TRADES {
                return None;
            }
            Some((w[0], w[1].target_quality()))
        })
        .collect()
}

fn budget_fit_seeds(warm: Option<[f64; SCORE_FEATURES]>) -> Vec<[f64; SCORE_FEATURES]> {
    let equal = [SCORE_BUDGET / SCORE_FEATURES as f64; SCORE_FEATURES];
    let cold = ScoreModel::cold_start().weights;
    // Emphasize one factor each, then project onto Σw=10.
    let mut wr_heavy = equal;
    wr_heavy[0] = 4.0;
    let mut gain_heavy = equal;
    gain_heavy[1] = 4.0;
    let mut rr_heavy = equal;
    rr_heavy[2] = 4.0;
    let mut until_heavy = equal;
    until_heavy[3] = 4.0;
    let mut dd_heavy = equal;
    dd_heavy[4] = 4.0;
    let mut seeds = vec![
        equal,
        cold,
        project_budget(wr_heavy),
        project_budget(gain_heavy),
        project_budget(rr_heavy),
        project_budget(until_heavy),
        project_budget(dd_heavy),
    ];
    // Previous day's line first — continuity toward the main fit.
    if let Some(w) = warm {
        seeds.insert(0, project_budget(w));
    }
    seeds
}

/// `w ≥ 0` and `Σ w = SCORE_BUDGET`.
fn project_budget(mut w: [f64; SCORE_FEATURES]) -> [f64; SCORE_FEATURES] {
    for x in &mut w {
        *x = x.max(0.0);
    }
    let sum: f64 = w.iter().sum();
    if sum <= 1e-12 {
        return [SCORE_BUDGET / SCORE_FEATURES as f64; SCORE_FEATURES];
    }
    for x in &mut w {
        *x *= SCORE_BUDGET / sum;
    }
    w
}

/// Projected gradient on MSE: `S = w·f`, `w` on the budget-10 simplex.
fn budget_fit(
    samples: &[(WindowStats, f64)],
    seed: [f64; SCORE_FEATURES],
) -> Result<ScoreModel, String> {
    let mut rows: Vec<([f64; SCORE_FEATURES], f64)> = Vec::new();
    for (stats, y) in samples {
        if u32::from(stats.trades) < MIN_TRADES {
            continue;
        }
        rows.push((stats.score_features(), *y));
    }
    if rows.len() < MIN_FIT_SAMPLES {
        return Err("not enough scored samples".into());
    }
    let n = rows.len() as f64;
    let mut w = project_budget(seed);
    let mut best_w = w;
    let mut best_mse = f64::INFINITY;
    let lr0 = 0.35;
    for step in 0..400 {
        let mut grad = [0.0; SCORE_FEATURES];
        let mut mse = 0.0;
        for (x, y) in &rows {
            let mut pred = 0.0;
            for i in 0..SCORE_FEATURES {
                pred += w[i] * x[i];
            }
            let err = pred - y;
            mse += err * err;
            for i in 0..SCORE_FEATURES {
                grad[i] += 2.0 * err * x[i] / n;
            }
        }
        mse /= n;
        if mse < best_mse {
            best_mse = mse;
            best_w = w;
        }
        let lr = lr0 / (1.0 + step as f64 * 0.01);
        for i in 0..SCORE_FEATURES {
            w[i] -= lr * grad[i];
        }
        w = project_budget(w);
    }
    let mut model = ScoreModel {
        weights: best_w,
        intercept: 0.0,
        score_mae: 0.0,
        score_rel_err: 0.0,
        samples: rows.len(),
        cold_start: false,
    };
    model.score_rel_err = model.relative_error(samples);
    model.score_mae = model.score_rel_err * SCORE_BUDGET;
    Ok(model)
}

/// [`WindowStats`] built trade by trade.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tally {
    sum: f64,
    peak: f64,
    drop: f64,
    worst: f64,
    held_ms: i64,
    trades: u32,
    wins: u32,
    last_exit_ms: i64,
}

impl Tally {
    pub fn add(&mut self, net: f64, held_ms: i64) {
        self.sum += net;
        self.peak = self.peak.max(self.sum);
        self.drop = self.drop.max(self.peak - self.sum);
        self.worst = self.worst.min(net);
        self.held_ms += held_ms;
        self.trades += 1;
        self.wins += u32::from(net > 0.0);
    }

    /// Record the exit time of the trade just added (UI `until` = last of these).
    pub fn note_exit(&mut self, exit_ms: i64) {
        self.last_exit_ms = exit_ms;
    }

    pub fn stats(&self) -> WindowStats {
        WindowStats {
            profit: self.sum as f32,
            drop: self.drop as f32,
            worst: self.worst as f32,
            hold_hours: if self.trades == 0 {
                0.0
            } else {
                (self.held_ms as f64 / f64::from(self.trades) / 3_600_000.0) as f32
            },
            trades: self.trades.min(u32::from(u16::MAX)) as u16,
            wins: self.wins.min(u32::from(u16::MAX)) as u16,
            last_exit_ms: self.last_exit_ms,
            score_at_ms: 0,
            lookback_ms: 0,
        }
    }
}

/// Best coarse setting of one RSI pair × timeframe group in one window (staged stage 1).
#[derive(Debug, Clone, Copy)]
pub struct GroupBest {
    pub group: usize,
    pub score: f64,
    pub profit: f32,
}

/// Top-`keep` groups by score then profit then lower id; optional minimum best-setting profit.
pub fn top_groups(mut ranked: Vec<GroupBest>, keep: usize, min_profit: Option<f32>) -> Vec<GroupBest> {
    if let Some(floor) = min_profit {
        ranked.retain(|g| g.profit >= floor);
    }
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.profit.total_cmp(&a.profit))
            .then(a.group.cmp(&b.group))
    });
    ranked.truncate(keep);
    ranked
}

/// Union of per-window top groups, capped: prefer groups that appear in more windows, then
/// higher max score, then lower group id. Result is sorted by group id for stable file layout.
pub fn union_survivor_groups(per_window: &[Vec<GroupBest>], cap: usize) -> Vec<usize> {
    use std::collections::HashMap;
    let mut best: HashMap<usize, (usize, f64)> = HashMap::new();
    for window in per_window {
        for g in window {
            let e = best.entry(g.group).or_insert((0, f64::NEG_INFINITY));
            e.0 += 1;
            if g.score > e.1 {
                e.1 = g.score;
            }
        }
    }
    let mut all: Vec<(usize, usize, f64)> = best
        .into_iter()
        .map(|(group, (count, score))| (group, count, score))
        .collect();
    all.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.2.total_cmp(&a.2))
            .then(a.0.cmp(&b.0))
    });
    all.truncate(cap);
    let mut ids: Vec<usize> = all.into_iter().map(|(g, _, _)| g).collect();
    ids.sort_unstable();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(trades: u16, wins: u16) -> WindowStats {
        WindowStats {
            profit: 50.0,
            drop: 10.0,
            worst: -5.0,
            hold_hours: 0.0,
            trades,
            wins,
            ..Default::default()
        }
    }

    #[test]
    fn few_trades_are_not_trusted() {
        let t = |n, w| (stats(n, w).trusted_win_rate() * 100.0).round();
        assert_eq!(t(1, 1), 21.0);
        assert_eq!(t(10, 6), 31.0);
        assert_eq!(t(100, 60), 50.0);
        assert_eq!(stats(4, 4).score(), None);
        assert!(stats(5, 5).score().is_some());
    }

    #[test]
    fn every_part_counts() {
        let s = WindowStats {
            profit: 50.0,
            drop: 10.0,
            worst: -5.0,
            hold_hours: 24.0,
            trades: 100,
            wins: 60,
            ..Default::default()
        };
        let expected = 5.0 * s.trusted_win_rate() * (50.0 / 55.0) * 0.5;
        assert!((s.score().unwrap() - expected).abs() < 1e-9);
        // A drop below 5 USDT counts as 5.
        let small = WindowStats { drop: 1.0, ..s };
        assert_eq!(small.score(), s.score().map(|x| x * 10.0 / 5.0));
        // Each part on its own lowers the score.
        assert!(
            WindowStats {
                hold_hours: 48.0,
                ..s
            }
            .score()
                < s.score()
        );
        assert!(WindowStats { worst: -40.0, ..s }.score() < s.score());
        assert!(WindowStats { wins: 50, ..s }.score() < s.score());
        assert!(WindowStats { drop: 20.0, ..s }.score() < s.score());
    }

    #[test]
    fn losers_rank_below_every_winner() {
        let tiny_win = WindowStats {
            profit: 0.01,
            drop: 50.0,
            worst: -40.0,
            hold_hours: 200.0,
            trades: 5,
            wins: 1,
            ..Default::default()
        };
        let small_loss = WindowStats {
            profit: -0.01,
            drop: 5.0,
            worst: -1.0,
            hold_hours: 1.0,
            trades: 50,
            wins: 40,
            ..Default::default()
        };
        assert!(tiny_win.score().unwrap() > 0.0);
        assert!(small_loss.score().unwrap() < 0.0);
        assert!(
            WindowStats {
                profit: -20.0,
                ..small_loss
            }
            .score()
                < small_loss.score()
        );
    }

    #[test]
    fn tally_counts_trade_by_trade() {
        let mut t = Tally::default();
        for (net, hours) in [(3.0, 1), (-4.0, 2), (-2.0, 3), (10.0, 6)] {
            t.add(net, hours * 3_600_000);
        }
        let s = t.stats();
        assert_eq!(
            (s.profit, s.drop, s.worst, s.hold_hours, s.trades, s.wins),
            (7.0, 6.0, -4.0, 3.0, 4, 2)
        );
        assert_eq!(WindowStats::from_bytes(&s.bytes()), s);
    }

    #[test]
    fn learned_score_fits_under_one_percent_error() {
        // Synthetic S* = exact budget-10 equation → fit must hit << 1%.
        let mut samples = Vec::new();
        let true_w = project_budget([2.0, 1.5, 2.0, 1.0, 2.0, 1.5]);
        assert!((true_w.iter().sum::<f64>() - SCORE_BUDGET).abs() < 1e-9);
        for k in 0..40 {
            let s = WindowStats {
                profit: 10.0 + k as f32,
                drop: 8.0,
                worst: -3.0,
                hold_hours: (k % 5) as f32,
                trades: 20 + (k % 10) as u16,
                wins: 12 + (k % 8) as u16,
                last_exit_ms: 1_000_000 + k as i64 * 86_400_000,
                score_at_ms: 1_000_000 + 30 * 86_400_000,
                lookback_ms: 30 * 86_400_000,
            };
            let x = s.score_features();
            let mut y = 0.0;
            for (w, f) in true_w.iter().zip(x.iter()) {
                y += w * f;
            }
            samples.push((s, y));
        }
        let model = ScoreModel::fit(&samples, None).expect("fit");
        assert!(!model.cold_start);
        assert!(
            (model.weights.iter().sum::<f64>() - SCORE_BUDGET).abs() < 1e-6,
            "weights sum {}",
            model.weights.iter().sum::<f64>()
        );
        assert!(
            model.score_rel_err < MAX_SCORE_REL_ERR,
            "rel_err={}",
            model.score_rel_err
        );
        assert!(model.score_ok());
    }

    #[test]
    fn cold_start_weights_sum_to_budget() {
        let m = ScoreModel::cold_start();
        assert!((m.weights.iter().sum::<f64>() - SCORE_BUDGET).abs() < 1e-9);
        assert_eq!(m.intercept, 0.0);
        let perfect = [1.0; SCORE_FEATURES];
        assert!((m.predict_features(&perfect) - SCORE_BUDGET).abs() < 1e-9);
    }

    #[test]
    fn until_factor_is_last_trade_freshness_in_lookback() {
        let lookback = 10 * 86_400_000;
        let at = 100 * 86_400_000;
        let fresh = WindowStats {
            last_exit_ms: at,
            score_at_ms: at,
            lookback_ms: lookback,
            ..Default::default()
        };
        assert!((fresh.until_factor() - 1.0).abs() < 1e-9);
        let old = WindowStats {
            last_exit_ms: at - lookback,
            score_at_ms: at,
            lookback_ms: lookback,
            ..Default::default()
        };
        assert!((old.until_factor() - 0.0).abs() < 1e-9);
        assert!((WindowStats::default().until_factor() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn learned_score_rejects_unfittable_noise() {
        let mut samples = Vec::new();
        for k in 0..20 {
            let s = WindowStats {
                profit: 5.0,
                drop: 5.0,
                worst: -1.0,
                hold_hours: 1.0,
                trades: 10,
                wins: 5,
                ..Default::default()
            };
            // Same features, wildly different targets → cannot hit 1%.
            samples.push((s, if k % 2 == 0 { 0.0 } else { 10.0 }));
        }
        let model = ScoreModel::fit(&samples, None).expect("keeps best weights even if noisy");
        assert!(!model.cold_start);
        assert!(
            !model.score_ok(),
            "noise must stay above 1% gate, rel_err={}",
            model.score_rel_err
        );
    }

    #[test]
    fn same_window_target_is_budget_formula_not_heuristic() {
        let s = WindowStats {
            profit: 50.0,
            drop: 10.0,
            worst: -5.0,
            hold_hours: 2.0,
            trades: 40,
            wins: 24,
            last_exit_ms: 10_000_000,
            score_at_ms: 10_000_000,
            lookback_ms: 30 * 86_400_000,
        };
        let budget = ScoreModel::cold_start().predict_features(&s.score_features());
        assert!((s.target_quality() - budget).abs() < 1e-9);
        // Legacy heuristic mapped to 0–10 is a different number.
        let raw = s.score().unwrap();
        let heuristic_01_10 = if raw >= 0.0 {
            (5.0 + 5.0 * (raw / (raw + 2.0))).clamp(0.0, SCORE_BUDGET)
        } else {
            (5.0 + 5.0 * (raw / (raw.abs() + 2.0))).clamp(0.0, SCORE_BUDGET)
        };
        assert!(
            (s.target_quality() - heuristic_01_10).abs() > 0.05,
            "S* must follow budget-10, not heuristic map"
        );
        let model = ScoreModel::fit(&[(s, s.target_quality()); 12], None).expect("fit");
        assert!(model.score_rel_err < MAX_SCORE_REL_ERR, "rel_err={}", model.score_rel_err);
    }

    #[test]
    fn warm_start_keeps_previous_line_when_better() {
        let mut samples = Vec::new();
        let true_w = project_budget([2.0, 1.5, 2.0, 1.0, 2.0, 1.5]);
        for k in 0..30 {
            let s = WindowStats {
                profit: 20.0 + k as f32,
                drop: 8.0,
                worst: -3.0,
                hold_hours: 1.0,
                trades: 20,
                wins: 12,
                last_exit_ms: 1_000_000 + k as i64 * 86_400_000,
                score_at_ms: 1_000_000 + 30 * 86_400_000,
                lookback_ms: 30 * 86_400_000,
            };
            let x = s.score_features();
            let y: f64 = true_w.iter().zip(x.iter()).map(|(w, f)| w * f).sum();
            samples.push((s, y));
        }
        let day1 = ScoreModel::fit(&samples[..15], None).expect("day1");
        let day2 = ScoreModel::fit(&samples, Some(&day1)).expect("day2");
        assert!(!day2.cold_start);
        assert!(day2.score_rel_err <= day1.relative_error(&samples) + 1e-9);
        assert!(day2.score_ok());
    }

    #[test]
    fn cold_start_when_few_samples() {
        let m = ScoreModel::fit(&[], None).unwrap();
        assert!(m.cold_start);
        assert!(m.score_ok());
    }

    #[test]
    fn staged_survivors_prefer_score_then_cap_by_appearances() {
        let w0 = top_groups(
            vec![
                GroupBest {
                    group: 3,
                    score: 2.0,
                    profit: 10.0,
                },
                GroupBest {
                    group: 1,
                    score: 3.0,
                    profit: 5.0,
                },
                GroupBest {
                    group: 2,
                    score: 2.0,
                    profit: 20.0,
                },
            ],
            2,
            None,
        );
        assert_eq!(
            w0.iter().map(|g| g.group).collect::<Vec<_>>(),
            vec![1, 2]
        );
        let filtered = top_groups(
            vec![GroupBest {
                group: 9,
                score: 9.0,
                profit: -1.0,
            }],
            5,
            Some(0.0),
        );
        assert!(filtered.is_empty());
        let union = union_survivor_groups(
            &[
                vec![
                    GroupBest {
                        group: 1,
                        score: 1.0,
                        profit: 1.0,
                    },
                    GroupBest {
                        group: 2,
                        score: 2.0,
                        profit: 2.0,
                    },
                ],
                vec![
                    GroupBest {
                        group: 2,
                        score: 5.0,
                        profit: 5.0,
                    },
                    GroupBest {
                        group: 3,
                        score: 4.0,
                        profit: 4.0,
                    },
                ],
            ],
            2,
        );
        // group 2 in both windows; then 3 (score 4) over 1 (score 1).
        assert_eq!(union, vec![2, 3]);
    }
}
