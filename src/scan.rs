//! Fast replay of the RSI-cross reversal strategy, with or without a take-profit / stop-loss
//! ladder, following exactly the rules NautilusTrader applies (see [`crate::strategy`] and
//! [`crate::tape`]), so thousands of settings can be tested in seconds and the best ones then
//! confirmed fill by fill in Nautilus.

use std::ops::Range;

use crate::{
    backtest::{Fill, FillKind},
    exits::Exits,
    filters::Filter,
    okx_history::Candle,
    rsi_cross::cross_signal,
    strategy::Signal,
    tape::{CloseRanges, Tape},
};

/// BTC per size step (OKX's 0.01 contract).
pub const STEP_BTC: f64 = 0.0001;
const MAX_TARGETS: usize = 8;

pub struct Market<'a> {
    pub candles: &'a [Candle],
    pub tape: &'a Tape,
    pub closes: &'a CloseRanges,
}

#[derive(Debug, Clone, Copy)]
pub struct Costs {
    /// Position size in steps of 0.0001 BTC.
    pub steps: i64,
    pub taker: f64,
    pub maker: f64,
    pub balance: f64,
}

/// Result of one pair with one exit setting over the whole period, in USDT.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Outcome {
    pub fast: u16,
    pub slow: u16,
    pub setting: u16,
    pub orders: u32,
    pub closed_trades: u32,
    /// Closed trades that made money after fees.
    pub wins: u32,
    pub target_fills: u32,
    pub stop_fills: u32,
    /// Price gains of every closed part of a position.
    pub realized: f64,
    pub fees: f64,
    /// Open position marked at the last close.
    pub open_pnl: f64,
    pub max_drawdown: f64,
    pub position: i8,
    /// Signals not traded because the cash balance was below zero, as Nautilus denies them.
    pub refused: u32,
}

impl Outcome {
    pub fn total(&self) -> f64 {
        self.realized - self.fees + self.open_pnl
    }

    pub fn win_rate(&self) -> f64 {
        if self.closed_trades == 0 { 0.0 } else { f64::from(self.wins) / f64::from(self.closed_trades) * 100.0 }
    }
}

/// How one trade, from its entry until nothing is left, ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Trade {
    pub long: bool,
    pub entry_ms: i64,
    pub entry_price: f64,
    pub steps: i64,
    pub entry_fee: f64,
    pub exits: Vec<Exit>,
    /// Still open at the end; its last exit is the last close, without a fee.
    pub open: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exit {
    pub time_ms: i64,
    pub price: f64,
    pub steps: i64,
    pub fee: f64,
    /// `TP1`, `TP2`, ..., `SL`, `BE` (stop at the entry price), `signal` (reversed) or `open`.
    pub kind: String,
}

/// One trade, from its entry until nothing is left, kept small: every combination's whole
/// history of trades is held in memory at once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClosedTrade {
    /// Time of the trade that closed what was left.
    pub exit_ms: i64,
    /// Bar whose opening trade filled the entry.
    pub entry_bar: u32,
    /// Price gains of every part minus the entry and exit fees, in USDT.
    pub net: f32,
}

/// Every fill and trade of one replay.
#[derive(Debug, Default)]
pub struct Record {
    pub fills: Vec<Fill>,
    pub trades: Vec<Trade>,
}

/// Bars whose opening trade fills a signal: a cross on bar `i`'s close fills at bar `i + 1`'s
/// open; a cross on the last bar never fills.
pub fn signals(x: &[f64], y: &[f64]) -> Vec<(u32, i8)> {
    (1..x.len().saturating_sub(1))
        .filter(|&i| !(x[i].is_nan() || y[i].is_nan() || x[i - 1].is_nan() || y[i - 1].is_nan()))
        .filter_map(|i| match cross_signal(x[i - 1], y[i - 1], x[i], y[i])? {
            Signal::Long => Some((i as u32 + 1, 1)),
            Signal::Short => Some((i as u32 + 1, -1)),
            Signal::Flat => None,
        })
        .collect()
}

/// `signals` through `filter`, judged on the crossing bar's close: a rejected cross becomes 0,
/// which closes the position without opening one.
pub fn filtered(signals: &[(u32, i8)], filter: &Filter, candles: &[Candle], ema: &[f64], atr: &[f64]) -> Vec<(u32, i8)> {
    signals
        .iter()
        .map(|&(bar, dir)| {
            let i = bar as usize - 1;
            (bar, if filter.allows(dir, candles[i].close, ema[i], atr[i]) { dir } else { 0 })
        })
        .collect()
}

struct Sim<'a> {
    market: &'a Market<'a>,
    costs: Costs,
    exits: Option<&'a Exits>,
    o: Outcome,
    /// Signed position in steps.
    pos: i64,
    entry: f64,
    entry_ticks: i64,
    /// Net result so far of the trade being held.
    trade_net: f64,
    // Exits of the position, if working.
    active: bool,
    targets: [u32; MAX_TARGETS],
    target_steps: [i64; MAX_TARGETS],
    n_targets: usize,
    next: usize,
    stop: u32,
    at_breakeven: bool,
    from_target: usize,
    from_stop: usize,
    // Drawdown: bars before `marked` are accounted for.
    marked: usize,
    peak: f64,
    record: Option<&'a mut Record>,
    entry_bar: u32,
    log: Option<&'a mut Vec<ClosedTrade>>,
}

/// Replays one pair's `signals` with `exits` (or none) through every trade of the candles `bars`
/// of `market`, starting flat with the full balance. Only signals from crosses inside `bars` are
/// traded: those filling from bar `bars.start + 1` on. At the end, the position is marked at the
/// close of the last bar.
pub fn simulate(
    ids: (u16, u16, u16),
    signals: &[(u32, i8)],
    exits: Option<&Exits>,
    market: &Market,
    bars: Range<usize>,
    costs: Costs,
    record: Option<&mut Record>,
) -> Outcome {
    replay(ids, signals, exits, market, bars, costs, record, None)
}

/// [`simulate`], also appending every trade closed inside `bars` to `log`.
pub fn simulate_logged(
    ids: (u16, u16, u16),
    signals: &[(u32, i8)],
    exits: Option<&Exits>,
    market: &Market,
    bars: Range<usize>,
    costs: Costs,
    log: &mut Vec<ClosedTrade>,
) -> Outcome {
    replay(ids, signals, exits, market, bars, costs, None, Some(log))
}

#[allow(clippy::too_many_arguments)]
fn replay<'a>(
    (fast, slow, setting): (u16, u16, u16),
    signals: &[(u32, i8)],
    exits: Option<&'a Exits>,
    market: &'a Market,
    bars: Range<usize>,
    costs: Costs,
    record: Option<&'a mut Record>,
    log: Option<&'a mut Vec<ClosedTrade>>,
) -> Outcome {
    let mut target_steps = [0; MAX_TARGETS];
    let mut n_targets = 0;
    if let Some(e) = exits {
        assert!(e.targets.len() <= MAX_TARGETS, "at most {MAX_TARGETS} take-profits");
        for (k, s) in e.target_steps(costs.steps).into_iter().enumerate() {
            target_steps[k] = s;
        }
        n_targets = e.targets.len();
    }
    let mut sim = Sim {
        market,
        costs,
        exits,
        o: Outcome { fast, slow, setting, ..Default::default() },
        pos: 0,
        entry: 0.0,
        entry_ticks: 0,
        trade_net: 0.0,
        active: false,
        targets: [0; MAX_TARGETS],
        target_steps,
        n_targets,
        next: 0,
        stop: 0,
        at_breakeven: false,
        from_target: 0,
        from_stop: 0,
        marked: bars.start,
        peak: costs.balance,
        record,
        entry_bar: 0,
        log,
    };
    let fills = bars.start + 1..bars.end;
    for &(bar, dir) in signals.iter().filter(|(bar, _)| fills.contains(&(*bar as usize))) {
        sim.on_signal(bar as usize, dir);
    }
    let tape = market.tape;
    let last_step = match tape.bar_start.get(bars.end) {
        Some(&next) => next as usize - 1,
        None => tape.price.len().saturating_sub(1),
    };
    sim.run_exits(last_step);
    sim.mark_to(bars.end);
    if let Some(last) = bars.end.checked_sub(1).and_then(|b| market.candles.get(b)) {
        let held = sim.pos as f64 * STEP_BTC;
        sim.o.open_pnl = held * (last.close - sim.entry);
        sim.o.position = sim.pos.signum() as i8;
        if sim.pos != 0
            && let Some(record) = sim.record.as_mut()
            && let Some(trade) = record.trades.last_mut()
        {
            trade.exits.push(Exit { time_ms: last.time_ms, price: last.close, steps: sim.pos.abs(), fee: 0.0, kind: "open".into() });
            trade.open = true;
        }
    }
    sim.o
}

impl Sim<'_> {
    fn cash(&self) -> f64 {
        self.costs.balance + self.o.realized - self.o.fees
    }

    fn on_signal(&mut self, bar: usize, dir: i8) {
        let step = self.market.tape.bar_start[bar] as usize;
        self.run_exits(step);
        if (dir > 0 && self.pos > 0) || (dir < 0 && self.pos < 0) || (dir == 0 && self.pos == 0) {
            return;
        }
        // Nautilus' strategy cancels the exits before submitting, even if the order is denied.
        let had_exits = std::mem::replace(&mut self.active, false);
        // Below zero cash, Nautilus denies an order unless it only closes the position; the
        // exits being cancelled still count as closing it, so then a close is denied too.
        if (dir != 0 || had_exits) && self.cash() < 0.0 {
            self.o.refused += 1;
            return;
        }
        let price = self.market.candles[bar].open;
        let time_ms = self.market.tape.time_ms(step);
        self.mark_to(bar);
        let closing = self.pos.abs();
        let buy = if dir == 0 { self.pos < 0 } else { dir > 0 };
        if closing > 0 {
            self.close(time_ms, closing, price, self.costs.taker, "signal");
        }
        self.o.orders += 1;
        let size = if dir == 0 { 0 } else { self.costs.steps };
        if let Some(record) = self.record.as_mut() {
            let total = closing + size;
            record.fills.push(Fill {
                time_ms,
                kind: FillKind::Signal,
                buy,
                size_btc: total as f64 * STEP_BTC,
                price,
                fee_usdt: self.costs.taker * total as f64 * STEP_BTC * price,
            });
        }
        if size == 0 {
            return;
        }
        let fee = self.costs.taker * size as f64 * STEP_BTC * price;
        self.o.fees += fee;
        self.trade_net = -fee;
        self.pos = i64::from(dir) * size;
        self.entry = price;
        self.entry_bar = bar as u32;
        if let Some(record) = self.record.as_mut() {
            record.trades.push(Trade { long: dir > 0, entry_ms: time_ms, entry_price: price, steps: size, entry_fee: fee, exits: Vec::new(), open: false });
        }
        if let Some(exits) = self.exits {
            let long = dir > 0;
            self.entry_ticks = (price * 10.0).round() as i64;
            for k in 0..self.n_targets {
                self.targets[k] = exits.target_ticks(self.entry_ticks, long, k) as u32;
            }
            self.stop = exits.stop_ticks(self.entry_ticks, long) as u32;
            self.at_breakeven = false;
            self.next = 0;
            self.from_target = step + 1;
            self.from_stop = step + 1;
            self.active = true;
        }
    }

    /// Fills the take-profits and stop the matching price reaches up to step `until`.
    fn run_exits(&mut self, until: usize) {
        let tape = self.market.tape;
        while self.active && self.pos != 0 {
            let long = self.pos > 0;
            let target = if self.next < self.n_targets {
                tape.first_hit(self.from_target, until, self.targets[self.next], long)
            } else {
                None
            };
            let stop = tape.first_hit(self.from_stop, target.unwrap_or(until), self.stop, !long);
            // The stop is only searched up to the take-profit's trade, and one trade cannot reach both.
            match (target, stop) {
                (_, Some(k)) => {
                    let kind = if self.at_breakeven { "BE" } else { "SL" };
                    self.exit_fill(k, self.pos.abs(), self.costs.taker, FillKind::StopLoss, kind);
                    self.o.stop_fills += 1;
                    self.active = false;
                }
                (Some(k), _) => {
                    let steps = self.target_steps[self.next].min(self.pos.abs());
                    self.next += 1;
                    self.exit_fill(k, steps, self.costs.maker, FillKind::TakeProfit, &format!("TP{}", self.next));
                    self.o.target_fills += 1;
                    // Other take-profits can fill on the same trade; the stop changes after it.
                    self.from_target = k;
                    self.from_stop = self.from_stop.max(k + 1);
                    if self.next == 1 && self.exits.is_some_and(|e| e.breakeven) {
                        self.stop = self.entry_ticks as u32;
                        self.at_breakeven = true;
                    }
                    if self.pos == 0 {
                        self.active = false;
                    }
                }
                (None, None) => {
                    self.from_target = until + 1;
                    self.from_stop = until + 1;
                    break;
                }
            }
        }
    }

    fn exit_fill(&mut self, step: usize, steps: i64, rate: f64, kind: FillKind, label: &str) {
        let tape = self.market.tape;
        let price = f64::from(tape.price[step]) / 10.0;
        let time_ms = tape.time_ms(step);
        self.mark_to(tape.bar_of(step));
        let long = self.pos > 0;
        self.close(time_ms, steps, price, rate, label);
        self.o.orders += 1;
        if let Some(record) = self.record.as_mut() {
            record.fills.push(Fill {
                time_ms,
                kind,
                buy: !long,
                size_btc: steps as f64 * STEP_BTC,
                price,
                fee_usdt: rate * steps as f64 * STEP_BTC * price,
            });
        }
    }

    /// Closes `steps` of the position at `price`.
    fn close(&mut self, time_ms: i64, steps: i64, price: f64, rate: f64, kind: &str) {
        let qty = steps as f64 * STEP_BTC;
        let sign = self.pos.signum() as f64;
        let gain = sign * (price - self.entry) * qty;
        let fee = rate * qty * price;
        self.o.realized += gain;
        self.o.fees += fee;
        self.trade_net += gain - fee;
        self.pos -= self.pos.signum() * steps;
        if let Some(trade) = self.record.as_mut().and_then(|r| r.trades.last_mut()) {
            trade.exits.push(Exit { time_ms, price, steps, fee, kind: kind.into() });
        }
        if self.pos == 0 {
            self.o.closed_trades += 1;
            self.o.wins += u32::from(self.trade_net > 0.0);
            if let Some(log) = self.log.as_mut() {
                log.push(ClosedTrade { exit_ms: time_ms, entry_bar: self.entry_bar, net: self.trade_net as f32 });
            }
        }
    }

    /// Accounts for the drawdown at the closes of bars `marked..bar`, during which nothing
    /// changed.
    fn mark_to(&mut self, bar: usize) {
        if bar <= self.marked {
            return;
        }
        let closed = self.cash();
        if self.pos == 0 {
            self.o.max_drawdown = self.o.max_drawdown.max(self.peak - closed);
            self.peak = self.peak.max(closed);
        } else {
            let held = self.pos as f64 * STEP_BTC;
            let span = self.market.closes.span(self.marked, bar);
            let base = closed - held * self.entry;
            let (low, high, inner) = if held > 0.0 {
                (base + held * span.min, base + held * span.max, held * span.fall)
            } else {
                (base + held * span.max, base + held * span.min, -held * span.rise)
            };
            self.o.max_drawdown = self.o.max_drawdown.max(self.peak - low).max(inner);
            self.peak = self.peak.max(high);
        }
        self.marked = bar;
    }
}
