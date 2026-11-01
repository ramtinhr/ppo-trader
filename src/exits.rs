//! Take-profit ladder and stop-loss placed on every entry, shared by the fast scanner and the
//! NautilusTrader strategy so both compute the same prices and sizes.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Exits placed as soon as an entry fills: one stop-loss for the whole position `stop_pct`
/// percent away from the entry price, and take-profits each closing its share of the position.
/// With `breakeven`, the stop moves to the entry price once the first take-profit fills. Prices
/// are in ticks of 0.1 USDT, sizes in steps of 0.0001 BTC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exits {
    pub stop_pct: f64,
    /// (distance in % of the entry price, share of the position), nearest first.
    pub targets: Vec<(f64, f64)>,
    pub breakeven: bool,
}

/// Scale-out template: take-profit distances as multiples of the stop (`ladder_r`), position
/// shares in percent, optional break-even after the first fill. Stage-2 scans pair each template
/// with every stop-loss × leverage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitProfile {
    pub label: String,
    pub ladder_r: Vec<f64>,
    pub shares_pct: Vec<f64>,
    pub breakeven: bool,
}

impl ExitProfile {
    /// Concrete [`Exits`] for a stop distance: each target is `r × stop_pct`.
    pub fn to_exits(&self, stop_pct: f64) -> Exits {
        Exits {
            stop_pct,
            targets: self
                .ladder_r
                .iter()
                .zip(&self.shares_pct)
                .map(|(r, s)| (r * stop_pct, s / 100.0))
                .collect(),
            breakeven: self.breakeven && self.ladder_r.len() > 1,
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.ladder_r.is_empty(), "exit profile needs at least one R target");
        ensure!(
            self.ladder_r.len() == self.shares_pct.len(),
            "exit profile ladder_r and shares_pct length mismatch"
        );
        ensure!(
            self.ladder_r.windows(2).all(|w| w[0] < w[1]) && self.ladder_r[0] > 0.0,
            "exit profile R targets must be positive and increasing"
        );
        let total: f64 = self.shares_pct.iter().sum();
        ensure!(
            (total - 100.0).abs() < 1e-6,
            "exit profile shares_pct must sum to 100"
        );
        Ok(())
    }
}

/// Default stage-2 scale-out catalog (single-TP R multiples + multi-TP ladders with BE).
pub fn default_scaleout_profiles() -> Vec<ExitProfile> {
    vec![
        ExitProfile {
            label: "1R".into(),
            ladder_r: vec![1.0],
            shares_pct: vec![100.0],
            breakeven: false,
        },
        ExitProfile {
            label: "1.5R".into(),
            ladder_r: vec![1.5],
            shares_pct: vec![100.0],
            breakeven: false,
        },
        ExitProfile {
            label: "2R".into(),
            ladder_r: vec![2.0],
            shares_pct: vec![100.0],
            breakeven: false,
        },
        ExitProfile {
            label: "1R+2R".into(),
            ladder_r: vec![1.0, 2.0],
            shares_pct: vec![50.0, 50.0],
            breakeven: true,
        },
        ExitProfile {
            label: "1R+3R".into(),
            ladder_r: vec![1.0, 3.0],
            shares_pct: vec![50.0, 50.0],
            breakeven: true,
        },
        ExitProfile {
            label: "1R+2R+3R".into(),
            ladder_r: vec![1.0, 2.0, 3.0],
            shares_pct: vec![34.0, 33.0, 33.0],
            breakeven: true,
        },
    ]
}

impl Exits {
    pub fn validate(&self, size_steps: i64) -> Result<()> {
        ensure!(
            self.stop_pct > 0.0 && self.stop_pct < 50.0,
            "stop-loss must be between 0% and 50%"
        );
        ensure!(
            !self.targets.is_empty(),
            "at least one take-profit is needed"
        );
        ensure!(
            self.targets.windows(2).all(|w| w[0].0 < w[1].0) && self.targets[0].0 > 0.0,
            "take-profits must be positive and in increasing order"
        );
        let total: f64 = self.targets.iter().map(|t| t.1).sum();
        ensure!(
            (total - 1.0).abs() < 1e-9,
            "take-profit shares must add up to 100%"
        );
        ensure!(
            self.target_steps(size_steps).iter().all(|s| *s > 0),
            "position too small to split into {} take-profits",
            self.targets.len()
        );
        Ok(())
    }

    /// Take-profits in % of the entry price, e.g. `SL 0.5% · TP 0.5/1/1.5% · 50/25/25% · BE`;
    /// a single take-profit has no share: `SL 1% · TP 2%`.
    pub fn label(&self) -> String {
        let join = |f: &dyn Fn(&(f64, f64)) -> f64| {
            self.targets
                .iter()
                .map(|t| format!("{}", (f(t) * 100.0).round() / 100.0))
                .collect::<Vec<_>>()
                .join("/")
        };
        let shares = if self.targets.len() > 1 {
            format!(" · {}%", join(&|t| t.1 * 100.0))
        } else {
            String::new()
        };
        format!(
            "SL {}% · TP {}%{shares}{}",
            self.stop_pct,
            join(&|t| t.0),
            if self.breakeven { " · BE" } else { "" }
        )
    }

    pub fn stop_ticks(&self, entry: i64, long: bool) -> i64 {
        stop_level(entry, self.stop_pct, long)
    }

    pub fn target_ticks(&self, entry: i64, long: bool, k: usize) -> i64 {
        target_level(entry, self.targets[k].0, long)
    }

    /// Size of each take-profit out of `steps`: its share rounded down, the last one taking the
    /// rest so the take-profits always close the whole position.
    pub fn target_steps(&self, steps: i64) -> Vec<i64> {
        let mut sizes: Vec<i64> = self
            .targets
            .iter()
            .map(|t| (steps as f64 * t.1 + 1e-9).floor() as i64)
            .collect();
        let rest = steps - sizes[..sizes.len() - 1].iter().sum::<i64>();
        *sizes.last_mut().unwrap() = rest;
        sizes
    }
}

/// Stop-loss price in ticks for a position entered at `entry` ticks.
pub fn stop_level(entry: i64, pct: f64, long: bool) -> i64 {
    let offset = pct / 100.0;
    level(entry, if long { -offset } else { offset })
}

/// Take-profit price in ticks for a position entered at `entry` ticks.
pub fn target_level(entry: i64, pct: f64, long: bool) -> i64 {
    let offset = pct / 100.0;
    level(entry, if long { offset } else { -offset })
}

/// `entry` moved by `offset` (a fraction), rounded to the nearest tick and at least one tick away.
fn level(entry: i64, offset: f64) -> i64 {
    let price = (entry as f64 * (1.0 + offset)).round() as i64;
    if price == entry {
        entry + offset.signum() as i64
    } else {
        price
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exits() -> Exits {
        Exits {
            stop_pct: 0.5,
            targets: vec![(0.5, 0.5), (1.0, 0.25), (1.5, 0.25)],
            breakeven: true,
        }
    }

    #[test]
    fn levels_are_percentages_of_the_entry() {
        let e = exits();
        // Entry 80,000.0 USDT = 800,000 ticks; 0.5% = 400 USDT.
        assert_eq!(e.stop_ticks(800_000, true), 796_000);
        assert_eq!(e.target_ticks(800_000, true, 0), 804_000);
        assert_eq!(e.target_ticks(800_000, true, 2), 812_000);
        assert_eq!(e.stop_ticks(800_000, false), 804_000);
        assert_eq!(e.target_ticks(800_000, false, 1), 792_000);
    }

    #[test]
    fn splits_size_into_whole_steps() {
        assert_eq!(exits().target_steps(50), vec![25, 12, 13]);
        let thirds = Exits {
            targets: vec![(0.5, 0.33), (1.0, 0.33), (1.5, 0.34)],
            ..exits()
        };
        assert_eq!(thirds.target_steps(50), vec![16, 16, 18]);
        assert!(thirds.validate(2).is_err());
    }

    #[test]
    fn labels_settings() {
        assert_eq!(exits().label(), "SL 0.5% · TP 0.5/1/1.5% · 50/25/25% · BE");
    }

    #[test]
    fn scaleout_profile_maps_r_multiples_to_exits() {
        let p = &default_scaleout_profiles()[3]; // 1R+2R
        p.validate().unwrap();
        let e = p.to_exits(0.5);
        assert_eq!(e.stop_pct, 0.5);
        assert_eq!(e.targets, vec![(0.5, 0.5), (1.0, 0.5)]);
        assert!(e.breakeven);
        e.validate(100).unwrap();
    }
}
