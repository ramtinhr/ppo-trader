//! Every trade of a period as NautilusTrader's simulated exchange matches orders against it, so
//! the fast scanner can find the exact trade where a take-profit or stop-loss fills.
//!
//! With trade execution on an L1 book, Nautilus checks resting orders against a matching price
//! that follows a trade only in its aggressor's direction: a buy above the previous trade or a
//! sell below it moves the matching price to the trade's price; any other trade leaves it at the
//! previous trade's price. A take-profit (limit) fills when the matching price reaches its price,
//! a stop-loss (stop market) when it reaches its trigger, and with `BestPriceFillModel` both fill
//! completely at the matching price.

use std::path::Path;

use anyhow::{Result, bail, ensure};

use crate::okx_history::{self, Candle};

/// Children per node of the search tree over prices.
const FAN: usize = 16;

pub struct Tape {
    /// Matching price of each step in ticks of 0.1 USDT. A step is a run of trades with the same
    /// matching price inside one bar; a bar's first trade is always a step of its own, because a
    /// signal fills on it and new exits are only matched from the next trade on.
    pub price: Vec<u32>,
    /// Time of each step's first trade, in ms after its bar's open.
    offset_ms: Vec<u32>,
    /// Index of each candle's first step.
    pub bar_start: Vec<u32>,
    /// Open time of each candle.
    bar_time: Vec<i64>,
    /// Highest and lowest price of each block of `FAN` steps, then of each block of those, ...
    highs: Vec<Vec<u32>>,
    lows: Vec<Vec<u32>>,
}

impl Tape {
    /// Candles of `bar_ms` and the tape of every trade from `from_ms` to `to_ms` (exclusive),
    /// read from the verified archives in `dir`. `progress` gets (archives read, archives).
    pub fn load(
        dir: &Path,
        bar_ms: i64,
        from_ms: i64,
        to_ms: i64,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<(Vec<Candle>, Tape)> {
        let mut candles: Vec<Candle> = Vec::new();
        let mut tape = Tape {
            price: Vec::new(),
            offset_ms: Vec::new(),
            bar_start: Vec::new(),
            bar_time: Vec::new(),
            highs: Vec::new(),
            lows: Vec::new(),
        };
        let mut previous: Option<u32> = None;
        let first_day = okx_history::utc8_day(from_ms);
        let days: Vec<_> = first_day
            .iter_days()
            .take_while(|d| *d <= okx_history::utc8_day(to_ms - 1))
            .collect();
        for (n, day) in days.iter().enumerate() {
            progress(n, days.len());
            let path = okx_history::archive_path(dir, *day);
            ensure!(
                okx_history::is_verified(dir, *day),
                "{} is missing or unverified; run download_okx_trades first",
                path.display()
            );
            okx_history::for_each_trade(&path, |t| {
                if t.time_ms < from_ms || t.time_ms >= to_ms {
                    return Ok(());
                }
                let p = (t.price * 10.0).round() as u32;
                let matching = match previous {
                    Some(q) if !((t.buy && p > q) || (!t.buy && p < q)) => q,
                    _ => p,
                };
                previous = Some(p);
                let open_ms = t.time_ms - t.time_ms.rem_euclid(bar_ms);
                match candles.last_mut() {
                    Some(c) if c.time_ms == open_ms => {
                        c.high = c.high.max(t.price);
                        c.low = c.low.min(t.price);
                        c.close = t.price;
                        let bar_first =
                            *tape.bar_start.last().unwrap() as usize == tape.price.len() - 1;
                        if bar_first || *tape.price.last().unwrap() != matching {
                            tape.grow();
                            tape.price.push(matching);
                            tape.offset_ms.push(u32::try_from(t.time_ms - open_ms)?);
                        }
                    }
                    Some(c) if c.time_ms > open_ms => {
                        bail!(
                            "trade {} at {} ms is out of time order",
                            t.trade_id,
                            t.time_ms
                        )
                    }
                    _ => {
                        candles.push(Candle {
                            time_ms: open_ms,
                            open: t.price,
                            high: t.price,
                            low: t.price,
                            close: t.price,
                        });
                        tape.bar_start.push(u32::try_from(tape.price.len())?);
                        tape.bar_time.push(open_ms);
                        tape.grow();
                        tape.price.push(matching);
                        tape.offset_ms.push(u32::try_from(t.time_ms - open_ms)?);
                    }
                }
                Ok(())
            })?;
        }
        progress(days.len(), days.len());
        tape.price.shrink_to_fit();
        tape.offset_ms.shrink_to_fit();
        tape.highs = layers(&tape.price, u32::max);
        tape.lows = layers(&tape.price, u32::min);
        Ok((candles, tape))
    }

    /// First step in `from..=to` whose price is at or above `level` (`up`) or at or below it.
    pub fn first_hit(&self, from: usize, to: usize, level: u32, up: bool) -> Option<usize> {
        let hit = |p: u32| if up { p >= level } else { p <= level };
        let tree = if up { &self.highs } else { &self.lows };
        let to = to.min(self.price.len().checked_sub(1)?);
        if from > to {
            return None;
        }
        let row = |depth: usize| {
            if depth == 0 {
                &self.price[..]
            } else {
                &tree[depth - 1][..]
            }
        };
        // Climb: scan the rest of the current block at each depth until something hits.
        let (mut depth, mut i, mut span) = (0, from, 1);
        let found = loop {
            let r = row(depth);
            let end = ((i / FAN + 1) * FAN).min(r.len());
            if let Some(j) = (i..end).find(|&j| hit(r[j])) {
                break j;
            }
            if depth == tree.len() {
                return None;
            }
            i = i / FAN + 1;
            depth += 1;
            span *= FAN;
            if i >= row(depth).len() || i * span > to {
                return None;
            }
        };
        // Descend into the first child that hits.
        let mut j = found;
        while depth > 0 {
            depth -= 1;
            let r = row(depth);
            j = (j * FAN..((j + 1) * FAN).min(r.len()))
                .find(|&c| hit(r[c]))
                .expect("parent hit");
        }
        (j <= to).then_some(j)
    }

    /// Time of step `step`'s first trade.
    /// Room for the next step. A year has hundreds of millions of steps, so the tape grows by a
    /// fixed number of steps rather than doubling, which could need twice the memory.
    fn grow(&mut self) {
        const CHUNK: usize = 1 << 24;
        if self.price.len() == self.price.capacity() {
            self.price.reserve_exact(CHUNK);
            self.offset_ms.reserve_exact(CHUNK);
        }
    }

    /// A tape of the matching prices `price`, one trade per millisecond, with a candle of
    /// `bar_ms` starting at each step in `bar_start`.
    #[cfg(test)]
    pub(crate) fn from_prices(price: Vec<u32>, bar_start: Vec<u32>, bar_ms: i64) -> Tape {
        let mut offset_ms = Vec::with_capacity(price.len());
        for (b, &s) in bar_start.iter().enumerate() {
            let end = bar_start.get(b + 1).map_or(price.len(), |e| *e as usize);
            offset_ms.extend(0..(end - s as usize) as u32);
        }
        let bar_time = (0..bar_start.len() as i64).map(|b| b * bar_ms).collect();
        let highs = layers(&price, u32::max);
        let lows = layers(&price, u32::min);
        Tape {
            price,
            offset_ms,
            bar_start,
            bar_time,
            highs,
            lows,
        }
    }

    pub fn time_ms(&self, step: usize) -> i64 {
        self.bar_time[self.bar_of(step)] + i64::from(self.offset_ms[step])
    }

    /// Bar holding step `step`.
    pub fn bar_of(&self, step: usize) -> usize {
        self.bar_start.partition_point(|&s| s as usize <= step) - 1
    }
}

fn layers(base: &[u32], pick: fn(u32, u32) -> u32) -> Vec<Vec<u32>> {
    let mut out: Vec<Vec<u32>> = Vec::new();
    let mut current = base;
    while current.len() > 1 {
        let next: Vec<u32> = current
            .chunks(FAN)
            .map(|c| c.iter().copied().reduce(pick).unwrap())
            .collect();
        out.push(next);
        current = out.last().unwrap();
    }
    out
}

/// Highest and lowest close of any range of bars, and the largest fall and rise inside it, for
/// the drawdown of a position held over many bars.
pub struct CloseRanges {
    size: usize,
    nodes: Vec<Span>,
}

#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub max: f64,
    pub min: f64,
    /// Largest `close[i] - close[j]` with `i <= j`.
    pub fall: f64,
    /// Largest `close[j] - close[i]` with `i <= j`.
    pub rise: f64,
}

impl Span {
    const EMPTY: Span = Span {
        max: f64::NEG_INFINITY,
        min: f64::INFINITY,
        fall: 0.0,
        rise: 0.0,
    };

    fn then(self, next: Span) -> Span {
        Span {
            max: self.max.max(next.max),
            min: self.min.min(next.min),
            fall: self.fall.max(next.fall).max(self.max - next.min),
            rise: self.rise.max(next.rise).max(next.max - self.min),
        }
    }
}

impl CloseRanges {
    pub fn new(closes: &[f64]) -> Self {
        let size = closes.len().next_power_of_two();
        let mut nodes = vec![Span::EMPTY; 2 * size];
        for (i, c) in closes.iter().enumerate() {
            nodes[size + i] = Span {
                max: *c,
                min: *c,
                fall: 0.0,
                rise: 0.0,
            };
        }
        for i in (1..size).rev() {
            nodes[i] = nodes[2 * i].then(nodes[2 * i + 1]);
        }
        Self { size, nodes }
    }

    /// Bars `from..to`.
    pub fn span(&self, from: usize, to: usize) -> Span {
        let (mut left, mut right) = (Span::EMPTY, Span::EMPTY);
        let (mut l, mut r) = (from + self.size, to + self.size);
        while l < r {
            if l & 1 == 1 {
                left = left.then(self.nodes[l]);
                l += 1;
            }
            if r & 1 == 1 {
                r -= 1;
                right = self.nodes[r].then(right);
            }
            l /= 2;
            r /= 2;
        }
        left.then(right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tape(price: Vec<u32>) -> Tape {
        let n = price.len();
        let highs = layers(&price, u32::max);
        let lows = layers(&price, u32::min);
        Tape {
            price,
            offset_ms: (0..n as u32).collect(),
            bar_start: vec![0],
            bar_time: vec![0],
            highs,
            lows,
        }
    }

    #[test]
    fn finds_first_hit_like_a_scan() {
        let mut x: u64 = 7;
        let price: Vec<u32> = (0..5000)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                1000 + (x >> 33) as u32 % 200
            })
            .collect();
        let t = tape(price.clone());
        for (from, to, level) in [
            (0, 4999, 1199),
            (17, 3000, 1195),
            (100, 100, 1100),
            (4000, 4999, 1000),
            (5, 2, 1100),
            (0, 4999, 2000),
        ] {
            for up in [true, false] {
                let scan = (from..=to.min(4999)).find(|&j| {
                    if up {
                        price[j] >= level
                    } else {
                        price[j] <= level
                    }
                });
                assert_eq!(
                    t.first_hit(from, to, level, up),
                    scan,
                    "{from} {to} {level} {up}"
                );
            }
        }
    }

    #[test]
    fn spans_falls_and_rises() {
        let r = CloseRanges::new(&[5.0, 9.0, 4.0, 6.0, 1.0, 8.0]);
        let s = r.span(0, 6);
        assert_eq!((s.max, s.min, s.fall, s.rise), (9.0, 1.0, 8.0, 7.0));
        let s = r.span(1, 4);
        assert_eq!((s.max, s.min, s.fall, s.rise), (9.0, 4.0, 5.0, 2.0));
    }
}
