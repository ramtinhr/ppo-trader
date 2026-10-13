//! The grid of strategy settings the scanners test, its command-line formats, and the
//! indicators every setting needs, computed once over all candles.

use anyhow::{Context, Result, ensure};

use crate::{
    exits::Exits,
    filters::{ATR_LENGTH, Filter, atr_series, ema_series},
    okx_history::Candle,
    rsi::Rsi,
    scan,
};

/// `1-50,60-400/10` -> 1, 2, ... 50, 60, 70, ... 400.
pub fn parse_lengths(text: &str) -> Result<Vec<usize>> {
    let mut lengths = Vec::new();
    for item in text.split(',').map(str::trim) {
        let (range, step) = item.split_once('/').unwrap_or((item, "1"));
        let (from, to) = range.split_once('-').unwrap_or((range, range));
        let (from, to, step): (usize, usize, usize) = (from.parse()?, to.parse()?, step.parse()?);
        ensure!(0 < from && from <= to && step > 0, "bad RSI length range {item:?}");
        lengths.extend((from..=to).step_by(step));
    }
    lengths.sort_unstable();
    lengths.dedup();
    Ok(lengths)
}

/// `0.1,0.5,1` -> [0.1, 0.5, 1.0]; `none` -> [].
pub fn parse_numbers(text: &str) -> Result<Vec<f64>> {
    if text == "none" {
        return Ok(Vec::new());
    }
    text.split(',').map(|v| Ok(v.trim().parse()?)).collect()
}

/// `none,50,100` -> [None, Some(50), Some(100)].
pub fn parse_options<T: std::str::FromStr>(text: &str) -> Result<Vec<Option<T>>>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    text.split(',').map(str::trim).map(|v| Ok(if v == "none" { None } else { Some(v.parse()?) })).collect()
}

/// `1/2/3,0.5/1/1.5` -> [[1, 2, 3], [0.5, 1, 1.5]].
pub fn parse_groups(text: &str) -> Result<Vec<Vec<f64>>> {
    text.split(',').map(|g| g.split('/').map(|v| Ok(v.trim().parse()?)).collect()).collect()
}

/// Every exit setting to test; the first, `None`, is the strategy without exits.
pub fn exit_settings(stops: &[f64], ladders: &[Vec<f64>], shares: &[Vec<f64>], breakeven: &[bool], size_steps: i64) -> Result<Vec<Option<Exits>>> {
    let mut out = vec![None];
    for &stop_pct in stops {
        for ladder in ladders {
            for split in shares.iter().filter(|s| s.len() == ladder.len()) {
                // With one take-profit nothing is left to protect after it fills.
                let modes: &[bool] = if ladder.len() == 1 { &[false] } else { breakeven };
                for &breakeven in modes {
                    let exits = Exits {
                        stop_pct,
                        targets: ladder.iter().zip(split).map(|(r, s)| (r * stop_pct, s / 100.0)).collect(),
                        breakeven,
                    };
                    exits.validate(size_steps).with_context(|| exits.label())?;
                    out.push(Some(exits));
                }
            }
        }
    }
    if !stops.is_empty() {
        ensure!(out.len() > 1, "no ladder in --ladders has as many take-profits as a split in --shares");
    }
    Ok(out)
}

/// Every pair of a trend and a volatility filter.
pub fn filters(trends: &[Option<usize>], atrs: &[Option<f64>]) -> Vec<Filter> {
    trends.iter().flat_map(|&trend| atrs.iter().map(move |&min_atr_pct| Filter { trend, min_atr_pct })).collect()
}

pub fn exits_label(exits: &Option<Exits>) -> String {
    exits.as_ref().map_or_else(|| "No TP/SL".into(), Exits::label)
}

/// RSI of every length over the candle closes; NaN until the RSI has a value.
pub fn rsi_series(lengths: &[usize], candles: &[Candle]) -> Vec<Vec<f64>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = lengths
            .iter()
            .map(|&length| {
                scope.spawn(move || {
                    let mut rsi = Rsi::new(length);
                    candles.iter().map(|c| rsi.update(c.close).unwrap_or(f64::NAN)).collect()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Every RSI, EMA and ATR the grid needs, over the same candles.
pub struct Indicators {
    pub lengths: Vec<usize>,
    pub rsi: Vec<Vec<f64>>,
    pub emas: Vec<(usize, Vec<f64>)>,
    pub atr: Vec<f64>,
    nan: Vec<f64>,
}

impl Indicators {
    pub fn new(candles: &[Candle], lengths: &[usize], filters: &[Filter]) -> Self {
        let mut ema_lengths: Vec<usize> = filters.iter().filter_map(|f| f.trend).collect();
        ema_lengths.sort_unstable();
        ema_lengths.dedup();
        Self {
            lengths: lengths.to_vec(),
            rsi: rsi_series(lengths, candles),
            emas: ema_lengths.into_iter().map(|n| (n, ema_series(candles, n))).collect(),
            atr: atr_series(candles, ATR_LENGTH),
            nan: vec![f64::NAN; candles.len()],
        }
    }

    pub fn index(&self, length: usize) -> Option<usize> {
        self.lengths.iter().position(|l| *l == length)
    }

    /// Crosses of RSI `i` (fast) and `j` (slow), as fill bars and directions.
    pub fn signals(&self, i: usize, j: usize) -> Vec<(u32, i8)> {
        scan::signals(&self.rsi[i], &self.rsi[j])
    }

    /// `raw` signals through `filter`; see [`scan::filtered`].
    pub fn filtered(&self, raw: &[(u32, i8)], filter: &Filter, candles: &[Candle]) -> Vec<(u32, i8)> {
        let ema = filter.trend.map_or(&self.nan, |n| &self.emas.iter().find(|(l, _)| *l == n).expect("EMA of every filter").1);
        scan::filtered(raw, filter, candles, ema, &self.atr)
    }
}
