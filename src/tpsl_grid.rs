//! Every pair of one take-profit and one stop-loss from a grid, replayed at once for one RSI
//! pair, by the same rules as [`crate::scan::simulate`] with a single take-profit for the whole
//! position and no break-even.
//!
//! Every signal's possible trade is measured once: the trade step where the price first reaches
//! each take-profit and each stop-loss level of the grid, while the position can be held (until
//! the next signal in the other direction). Each pair of levels then only has to compare two
//! steps per trade.
//!
//! Scanners walk every setting through one small block of signals at a time ([`SIGNAL_BLOCK`])
//! so each block's levels stay in cache; [`Entries::new`] builds per-signal hits in parallel.

use rayon::prelude::*;

use crate::{
    exits::{stop_level, target_level},
    okx_history::Candle,
    scan::{Costs, STEP_BTC},
    tape::Tape,
};

/// Signals every setting goes through before the next ones. Keeps one block's TP/SL levels hot
/// while the large TP × SL grid is replayed (see `full_scan` / `window_scan`).
pub const SIGNAL_BLOCK: usize = 2048;

/// Candles of one timeframe over a tape read with 1-minute candles.
pub struct Frame {
    pub minutes: usize,
    pub candles: Vec<Candle>,
    /// Tape step of each candle's first trade.
    pub bar_start: Vec<u32>,
}

impl Frame {
    /// Candles of `minutes` built from the 1-minute `candles` whose first trades are the tape
    /// steps `bar_start`. A candle opens at a multiple of its size, like the tape's own.
    pub fn from_minutes(candles: &[Candle], bar_start: &[u32], minutes: usize) -> Frame {
        let bar_ms = minutes as i64 * 60_000;
        let mut frame = Frame {
            minutes,
            candles: Vec::new(),
            bar_start: Vec::new(),
        };
        for (c, &s) in candles.iter().zip(bar_start) {
            let open_ms = c.time_ms - c.time_ms.rem_euclid(bar_ms);
            match frame.candles.last_mut() {
                Some(last) if last.time_ms == open_ms => {
                    last.high = last.high.max(c.high);
                    last.low = last.low.min(c.low);
                    last.close = c.close;
                }
                _ => {
                    frame.candles.push(Candle {
                        time_ms: open_ms,
                        ..*c
                    });
                    frame.bar_start.push(s);
                }
            }
        }
        frame
    }
}

/// `0.1-9/0.1` -> 0.1, 0.2, ... 9.0, without floating-point drift.
pub fn parse_range(text: &str) -> anyhow::Result<Vec<f64>> {
    use anyhow::{Context, ensure};
    let (range, step) = text.split_once('/').context("expected from-to/step")?;
    let (from, to) = range.split_once('-').context("expected from-to/step")?;
    let (from, to, step): (f64, f64, f64) = (
        from.trim().parse()?,
        to.trim().parse()?,
        step.trim().parse()?,
    );
    ensure!(0.0 < from && from <= to && step > 0.0, "bad range {text:?}");
    let n = ((to - from) / step).round() as usize;
    Ok((0..=n)
        .map(|k| ((from + k as f64 * step) * 1e6).round() / 1e6)
        .collect())
}

/// `1-10/1` -> 1, 2, ... 10. Whole numbers only, capped at 10 for paper/testnet.
pub fn parse_leverage(text: &str) -> anyhow::Result<Vec<u32>> {
    use anyhow::ensure;
    let vals = parse_range(text)?;
    ensure!(
        vals.iter()
            .all(|v| (1.0..11.0).contains(v) && (*v - v.round()).abs() < 1e-9),
        "leverage must be whole numbers from 1 to 10"
    );
    Ok(vals.into_iter().map(|v| v.round() as u32).collect())
}

/// Take-profit and stop-loss distances in % of the entry price, nearest first, and leverage
/// multipliers (position size = base × leverage). Layout of setting `s`:
/// `((tp * sl_count + sl) * lev_count + lev)`.
pub struct Grid {
    pub tp: Vec<f64>,
    pub sl: Vec<f64>,
    pub lev: Vec<u32>,
}

impl Grid {
    pub fn len(&self) -> usize {
        self.tp.len() * self.sl.len() * self.lev.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take-profit × stop-loss pairs (ignoring leverage).
    pub fn tpsl_len(&self) -> usize {
        self.tp.len() * self.sl.len()
    }

    /// (take-profit, stop-loss, leverage) indexes of setting `s`.
    pub fn split(&self, s: usize) -> (usize, usize, usize) {
        let n_sl = self.sl.len();
        let n_lev = self.lev.len();
        let lev = s % n_lev;
        let tpsl = s / n_lev;
        (tpsl / n_sl, tpsl % n_sl, lev)
    }
}

/// One trade of one setting, from its entry until it is closed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridTrade {
    /// Signal that opened it.
    pub signal: u32,
    /// Tape step of the trade that closed it.
    pub exit_step: u32,
    /// Price gain minus the entry and exit fees, in USDT.
    pub net: f32,
}

/// Every signal of one RSI pair with the steps where the price reaches each level.
pub struct Entries {
    /// Tape step each signal fills on (a candle's first trade).
    pub step: Vec<u32>,
    /// Wall time of each fill (`tape.time_ms(step)`), so hold-time tallies skip a bar search.
    pub entry_ms: Vec<i64>,
    /// Next signal in the other direction, or the number of signals if none.
    opposite: Vec<u32>,
    /// Result of closing the trade at that next signal.
    reversed_net: Vec<f32>,
    /// `tp_step[tp_from[k]..tp_from[k + 1]]`: steps where signal `k`'s trade first reaches each
    /// take-profit level, nearest first, as long as it reaches them; `tp_net` the trade's result
    /// when closed there.
    tp_from: Vec<u32>,
    tp_step: Vec<u32>,
    tp_net: Vec<f32>,
    sl_from: Vec<u32>,
    sl_step: Vec<u32>,
    sl_net: Vec<f32>,
    /// Entry price and direction of each signal's trade, to close it at any price.
    entry: Vec<f64>,
    long: Vec<bool>,
    qty: f64,
    taker: f64,
}

impl Entries {
    /// `signals` (fill candle of `frame`, direction) measured against `grid` on `tape`.
    pub fn new(
        signals: &[(u32, i8)],
        frame: &Frame,
        tape: &Tape,
        grid: &Grid,
        costs: Costs,
    ) -> Entries {
        let n = signals.len();
        let qty = costs.steps as f64 * STEP_BTC;
        let last_step = tape.price.len().saturating_sub(1);
        let step: Vec<u32> = signals
            .iter()
            .map(|&(bar, _)| frame.bar_start[bar as usize])
            .collect();
        let entry: Vec<f64> = signals
            .iter()
            .map(|&(bar, _)| frame.candles[bar as usize].open)
            .collect();
        let long: Vec<bool> = signals.iter().map(|&(_, dir)| dir > 0).collect();
        let entry_ms: Vec<i64> = step
            .iter()
            .map(|&s| tape.time_ms(s as usize))
            .collect();
        let mut opposite = vec![n as u32; n];
        let mut next_long = n as u32;
        let mut next_short = n as u32;
        for k in (0..n).rev() {
            opposite[k] = if signals[k].1 > 0 {
                next_short
            } else {
                next_long
            };
            if signals[k].1 > 0 {
                next_long = k as u32
            } else {
                next_short = k as u32
            }
        }
        let mut reversed_net = vec![0.0f32; n];
        for (k, &(_, dir)) in signals.iter().enumerate() {
            let opp = opposite[k] as usize;
            if opp >= n {
                continue;
            }
            let long = dir > 0;
            let sign = if long { 1.0 } else { -1.0 };
            let e0 = entry[k];
            let price = frame.candles[signals[opp].0 as usize].open;
            let entry_fee = costs.taker * qty * e0;
            reversed_net[k] =
                (sign * (price - e0) * qty - entry_fee - costs.taker * qty * price) as f32;
        }
        // Per-signal TP/SL hits are independent once `opposite`/`until` are known; build them
        // in parallel then pack into CSR arrays.
        let hits: Vec<(Vec<(u32, f32)>, Vec<(u32, f32)>)> = (0..n)
            .into_par_iter()
            .map(|k| {
                let long = long[k];
                let sign = if long { 1.0 } else { -1.0 };
                let e0 = entry[k];
                let entry_fee = costs.taker * qty * e0;
                let net = |price: f64, rate: f64| {
                    (sign * (price - e0) * qty - entry_fee - rate * qty * price) as f32
                };
                let opp = opposite[k] as usize;
                let until = if opp < n {
                    step[opp] as usize
                } else {
                    last_step
                };
                let entry_ticks = (e0 * 10.0).round() as i64;
                let from = step[k] as usize + 1;
                let mut tp = Vec::new();
                let mut at = from;
                for &pct in &grid.tp {
                    let level = target_level(entry_ticks, pct, long) as u32;
                    let Some(hit) = tape.first_hit(at, until, level, long) else {
                        break;
                    };
                    tp.push((hit as u32, net(f64::from(tape.price[hit]) / 10.0, costs.maker)));
                    at = hit;
                }
                let mut sl = Vec::new();
                let mut at = from;
                for &pct in &grid.sl {
                    let level = stop_level(entry_ticks, pct, long) as u32;
                    let Some(hit) = tape.first_hit(at, until, level, !long) else {
                        break;
                    };
                    sl.push((hit as u32, net(f64::from(tape.price[hit]) / 10.0, costs.taker)));
                    at = hit;
                }
                (tp, sl)
            })
            .collect();
        let mut tp_from = Vec::with_capacity(n + 1);
        let mut tp_step = Vec::new();
        let mut tp_net = Vec::new();
        let mut sl_from = Vec::with_capacity(n + 1);
        let mut sl_step = Vec::new();
        let mut sl_net = Vec::new();
        for (tp, sl) in &hits {
            tp_from.push(tp_step.len() as u32);
            for &(s, net) in tp {
                tp_step.push(s);
                tp_net.push(net);
            }
            sl_from.push(sl_step.len() as u32);
            for &(s, net) in sl {
                sl_step.push(s);
                sl_net.push(net);
            }
        }
        tp_from.push(tp_step.len() as u32);
        sl_from.push(sl_step.len() as u32);
        Entries {
            step,
            entry_ms,
            opposite,
            reversed_net,
            tp_from,
            tp_step,
            tp_net,
            sl_from,
            sl_step,
            sl_net,
            entry,
            long,
            qty,
            taker: costs.taker,
        }
    }

    pub fn len(&self) -> usize {
        self.step.len()
    }

    pub fn is_empty(&self) -> bool {
        self.step.is_empty()
    }

    /// Every closed trade of take-profit `tp` and stop-loss `sl` (indexes into the grid), in
    /// order. A trade still open at the end of the tape is left out.
    pub fn trades(&self, tp: usize, sl: usize, f: impl FnMut(GridTrade)) {
        let mut k = 0;
        self.trades_from(tp, sl, &mut k, self.len(), f);
    }

    /// [`Entries::trades`] in parts: the trades opened by signals `*k..limit`, leaving `*k` at
    /// the signal that opens the next one (or past the end). Going through every setting one
    /// block of signals at a time keeps the block's levels in the CPU cache.
    #[inline]
    pub fn trades_from(
        &self,
        tp: usize,
        sl: usize,
        k: &mut usize,
        limit: usize,
        f: impl FnMut(GridTrade),
    ) {
        self.trades_until(tp, sl, k, limit, None, f);
    }

    /// [`Entries::trades_from`] inside a window that ends before tape step `end.0`: a trade
    /// still open then is closed at price `end.1` with the taker fee, as of step `end.0 - 1`,
    /// and `*k` moves past the last signal. Without `end`, such a trade is left out.
    #[inline]
    pub fn trades_until(
        &self,
        tp: usize,
        sl: usize,
        k: &mut usize,
        limit: usize,
        end: Option<(u32, f64)>,
        mut f: impl FnMut(GridTrade),
    ) {
        let n = self.step.len();
        let limit = limit.min(n);
        let end_step = end.map_or(u32::MAX, |e| e.0);
        while *k < limit {
            let i = *k;
            let t = self.tp_from[i] as usize + tp;
            let s = self.sl_from[i] as usize + sl;
            let t = (t < self.tp_from[i + 1] as usize)
                .then(|| self.tp_step[t])
                .map(|step| (step, t));
            let s = (s < self.sl_from[i + 1] as usize)
                .then(|| self.sl_step[s])
                .map(|step| (step, s));
            // One trade cannot reach both levels, which are on both sides of the entry.
            let (exit_step, net, reversed_at) = match (t, s) {
                (Some((ts, t)), Some((ss, _))) if ts < ss => (ts, self.tp_net[t], None),
                (Some((ts, t)), None) => (ts, self.tp_net[t], None),
                (_, Some((ss, s))) => (ss, self.sl_net[s], None),
                (None, None) => {
                    let opp = self.opposite[i] as usize;
                    if opp < n {
                        (self.step[opp], self.reversed_net[i], Some(opp))
                    } else {
                        (u32::MAX, 0.0, Some(n))
                    }
                }
            };
            if exit_step >= end_step {
                if let Some((end_step, price)) = end {
                    f(GridTrade {
                        signal: i as u32,
                        exit_step: end_step - 1,
                        net: self.close_net(i, price),
                    });
                }
                *k = n;
                return;
            }
            f(GridTrade {
                signal: i as u32,
                exit_step,
                net,
            });
            *k = match reversed_at {
                Some(opp) => opp,
                // Flat after the exit: the next signal filling on or after it opens a trade.
                None => {
                    let next = i + 1;
                    if next >= n {
                        n
                    } else {
                        next + self.step[next..n].partition_point(|&s| s < exit_step)
                    }
                }
            };
        }
    }

    /// Result of signal `k`'s trade closed at `price` with the taker fee.
    pub fn close_net(&self, k: usize, price: f64) -> f32 {
        let sign = if self.long[k] { 1.0 } else { -1.0 };
        let entry = self.entry[k];
        (sign * (price - entry) * self.qty - self.taker * self.qty * (entry + price)) as f32
    }

    /// First signal filling on or after tape step `step`.
    pub fn first_at(&self, step: u32) -> usize {
        self.step.partition_point(|&s| s < step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        exits::Exits,
        scan::{self, ClosedTrade, Market},
        tape::CloseRanges,
    };

    fn random_walk(n: usize, seed: u64) -> Vec<u32> {
        let mut x = seed;
        let mut p = 1_000_000i64;
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                p += ((x >> 33) % 401) as i64 - 200;
                p as u32
            })
            .collect()
    }

    /// The grid's trades are exactly the scanner's for every setting, on random prices and
    /// signals that include repeated signals in the same direction.
    #[test]
    fn matches_the_scanner_for_every_setting() {
        // Short candles and frequent signals make exits on a signal's own trade common.
        matches_the_scanner(50, 7);
        assert!(
            matches_the_scanner(3, 2) > 0,
            "no exit on a signal's own trade was tested"
        );
    }

    struct Setup {
        tape: Tape,
        candles: Vec<Candle>,
        bar_start: Vec<u32>,
        frame: Frame,
        signals: Vec<(u32, i8)>,
        grid: Grid,
        costs: Costs,
    }

    fn setup(steps_per_bar: usize, every: usize) -> Setup {
        let price = random_walk(40_000, 3);
        let bars = price.len() / steps_per_bar;
        let bar_start: Vec<u32> = (0..bars as u32).map(|b| b * steps_per_bar as u32).collect();
        let tape = Tape::from_prices(price.clone(), bar_start.clone(), 60_000);
        let candles: Vec<Candle> = (0..bars)
            .map(|b| {
                let s = &price[b * steps_per_bar..(b + 1) * steps_per_bar];
                let p = |t: u32| f64::from(t) / 10.0;
                Candle {
                    time_ms: b as i64 * 60_000,
                    open: p(s[0]),
                    high: p(*s.iter().max().unwrap()),
                    low: p(*s.iter().min().unwrap()),
                    close: p(*s.last().unwrap()),
                }
            })
            .collect();
        let frame = Frame {
            minutes: 1,
            candles: candles.clone(),
            bar_start: bar_start.clone(),
        };
        let mut x = 11u64;
        let mut signals = Vec::new();
        let mut dir = 1i8;
        for bar in (5..bars as u32 - 1).step_by(every) {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Mostly alternating, sometimes the same direction twice.
            if (x >> 40) % 5 != 0 {
                dir = -dir;
            }
            signals.push((bar, dir));
        }
        let grid = Grid {
            tp: vec![0.01, 0.02, 0.03, 0.05, 0.1, 0.2],
            sl: vec![0.01, 0.02, 0.04, 0.07, 0.2],
            lev: vec![1],
        };
        let costs = Costs {
            steps: 50,
            taker: 0.0005,
            maker: 0.0002,
            balance: 1e9,
        };
        Setup {
            tape,
            candles,
            bar_start,
            frame,
            signals,
            grid,
            costs,
        }
    }

    /// Number of trades closed by a take-profit or stop-loss on a signal's own trade.
    fn matches_the_scanner(steps_per_bar: usize, every: usize) -> usize {
        let Setup {
            tape,
            candles,
            bar_start,
            frame,
            signals,
            grid,
            costs,
        } = setup(steps_per_bar, every);
        let entries = Entries::new(&signals, &frame, &tape, &grid, costs);
        let closes = CloseRanges::new(&candles.iter().map(|c| c.close).collect::<Vec<_>>());
        let market = Market {
            candles: &candles,
            tape: &tape,
            bar_start: &bar_start,
            closes: &closes,
        };
        let mut checked = 0;
        let mut on_signal = 0;
        for s in 0..grid.tpsl_len() {
            let (tp, sl, _) = grid.split(s); // lev_count == 1 in this test
            let exits = Exits {
                stop_pct: grid.sl[sl],
                targets: vec![(grid.tp[tp], 1.0)],
                breakeven: false,
            };
            let mut theirs: Vec<ClosedTrade> = Vec::new();
            scan::simulate_logged(
                (0, 0, 0),
                &signals,
                Some(&exits),
                &market,
                0..candles.len(),
                costs,
                &mut theirs,
            );
            let mut ours = Vec::new();
            entries.trades(tp, sl, |t| ours.push(t));
            // In blocks of signals, the same trades.
            let (mut blocked, mut k, mut limit) = (Vec::new(), 0, 0);
            while k < entries.len() {
                limit += 37;
                entries.trades_from(tp, sl, &mut k, limit, |t| blocked.push(t));
            }
            assert_eq!(blocked, ours, "{}", exits.label());
            assert_eq!(ours.len(), theirs.len(), "{}", exits.label());
            for (a, b) in ours.iter().zip(&theirs) {
                assert_eq!(
                    tape.time_ms(a.exit_step as usize),
                    b.exit_ms,
                    "{}",
                    exits.label()
                );
                assert_eq!(
                    signals[a.signal as usize].0,
                    b.entry_bar,
                    "{}",
                    exits.label()
                );
                assert!(
                    (a.net - b.net).abs() < 1e-4,
                    "{}: {} vs {}",
                    exits.label(),
                    a.net,
                    b.net
                );
            }
            checked += ours.len();
            // Closed by a take-profit or stop-loss on the trade a signal fills on.
            let reversal = |t: &GridTrade| {
                entries.opposite[t.signal as usize] as usize == entries.len()
                    || entries.step[entries.opposite[t.signal as usize] as usize] == t.exit_step
            };
            on_signal += ours
                .iter()
                .filter(|t| entries.step.binary_search(&t.exit_step).is_ok() && !reversal(t))
                .count();
        }
        assert!(checked > 1000, "too few trades compared: {checked}");
        on_signal
    }

    /// Inside a window, every setting starts flat and its trades are the scanner's over the
    /// window's candles; one still open at the end is closed at the last close with the taker
    /// fee, so the window's total is the scanner's minus that fee.
    #[test]
    fn a_window_matches_the_scanner_over_its_candles() {
        for (steps_per_bar, every) in [(50, 7), (3, 2)] {
            let Setup {
                tape,
                candles,
                bar_start,
                frame,
                signals,
                grid,
                costs,
            } = setup(steps_per_bar, every);
            let entries = Entries::new(&signals, &frame, &tape, &grid, costs);
            let closes = CloseRanges::new(&candles.iter().map(|c| c.close).collect::<Vec<_>>());
            let market = Market {
                candles: &candles,
                tape: &tape,
                bar_start: &bar_start,
                closes: &closes,
            };
            let qty = costs.steps as f64 * STEP_BTC;
            let (mut forced, mut compared) = (0, 0);
            // Some windows end on a signal's candle, which then belongs to the next window.
            let on_signal = |k: usize| signals[k].0 as usize;
            let windows = [
                (1, 200),
                (1, on_signal(25)),
                (150, on_signal(60)),
                (300, candles.len() / 2),
                (candles.len() / 2, candles.len() - 1),
            ];
            for (first, last) in windows {
                let end_step = bar_start[last];
                let close = candles[last - 1].close;
                for s in 0..grid.tpsl_len() {
                    let (tp, sl, _) = grid.split(s);
                    let exits = Exits {
                        stop_pct: grid.sl[sl],
                        targets: vec![(grid.tp[tp], 1.0)],
                        breakeven: false,
                    };
                    let mut theirs: Vec<ClosedTrade> = Vec::new();
                    // Fills from bar `first` on, like the window's first signal.
                    let o = scan::simulate_logged(
                        (0, 0, 0),
                        &signals,
                        Some(&exits),
                        &market,
                        first - 1..last,
                        costs,
                        &mut theirs,
                    );
                    let mut ours = Vec::new();
                    let mut k = entries.first_at(bar_start[first]);
                    entries.trades_until(
                        tp,
                        sl,
                        &mut k,
                        entries.first_at(end_step),
                        Some((end_step, close)),
                        |t| ours.push(t),
                    );
                    let name = format!("bars {first}..{last} {}", exits.label());
                    let closed = if o.position != 0 {
                        ours.len() - 1
                    } else {
                        ours.len()
                    };
                    assert_eq!(closed, theirs.len(), "{name}");
                    for (a, b) in ours.iter().zip(&theirs) {
                        assert_eq!(tape.time_ms(a.exit_step as usize), b.exit_ms, "{name}");
                        assert_eq!(signals[a.signal as usize].0, b.entry_bar, "{name}");
                        assert!(
                            (a.net - b.net).abs() < 1e-4,
                            "{name}: {} vs {}",
                            a.net,
                            b.net
                        );
                    }
                    let total: f64 = ours.iter().map(|t| f64::from(t.net)).sum();
                    let fee = if o.position != 0 {
                        costs.taker * qty * close
                    } else {
                        0.0
                    };
                    assert!(
                        (total - (o.total() - fee)).abs() < 1e-3,
                        "{name}: {total} vs {}",
                        o.total() - fee
                    );
                    if o.position != 0 {
                        assert_eq!(ours.last().unwrap().exit_step, end_step - 1, "{name}");
                        forced += 1;
                    }
                    compared += ours.len();
                }
            }
            assert!(
                forced >= 5 && compared > 500,
                "too little tested: {forced} closed at the end, {compared} trades"
            );
        }
    }

    /// Blocked window walks (as in `window_scan`) must match a single trades_until call.
    #[test]
    fn blocked_window_matches_full_window() {
        let Setup {
            tape,
            frame,
            signals,
            grid,
            costs,
            ..
        } = setup(50, 7);
        let entries = Entries::new(&signals, &frame, &tape, &grid, costs);
        let n_bars = frame.candles.len();
        let windows = [
            (1usize, 200),
            (150, 400),
            (300, n_bars / 2),
            (n_bars / 2, n_bars - 1),
        ];
        for (first_bar, last_bar) in windows {
            let start = frame.bar_start[first_bar];
            let end_step = frame.bar_start[last_bar];
            let close = frame.candles[last_bar - 1].close;
            let end = (end_step, close);
            let from = entries.first_at(start);
            let limit = entries.first_at(end_step);
            for tpsl in 0..grid.tpsl_len() {
                let (tp, sl, _) = grid.split(tpsl * grid.lev.len());
                let mut full = Vec::new();
                let mut k = from;
                entries.trades_until(tp, sl, &mut k, limit, Some(end), |t| full.push(t));
                let mut blocked = Vec::new();
                let mut k = from;
                for block_start in (from..limit).step_by(SIGNAL_BLOCK) {
                    let block_end = (block_start + SIGNAL_BLOCK).min(limit);
                    entries.trades_until(tp, sl, &mut k, block_end, Some(end), |t| {
                        blocked.push(t)
                    });
                }
                assert_eq!(blocked, full, "tpsl {tpsl} bars {first_bar}..{last_bar}");
            }
        }
    }

    #[test]
    fn builds_larger_candles_from_minutes() {
        let c = |m: i64, o: f64, h: f64, l: f64, cl: f64| Candle {
            time_ms: m * 60_000,
            open: o,
            high: h,
            low: l,
            close: cl,
        };
        // Minute 2 has no trade.
        let minutes = [
            c(0, 10.0, 12.0, 9.0, 11.0),
            c(1, 11.0, 15.0, 10.0, 14.0),
            c(3, 14.0, 14.0, 8.0, 9.0),
            c(4, 9.0, 9.5, 9.0, 9.2),
        ];
        let f = Frame::from_minutes(&minutes, &[0, 4, 9, 12], 3);
        assert_eq!(f.bar_start, vec![0, 9]);
        assert_eq!(f.candles[0], c(0, 10.0, 15.0, 9.0, 14.0));
        assert_eq!(f.candles[1], c(3, 14.0, 14.0, 8.0, 9.2));
    }
}
