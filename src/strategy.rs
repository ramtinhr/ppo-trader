//! Nautilus strategy that follows a bar-by-bar signal the way Pine's `strategy.entry` does with
//! `pyramiding = 1`: a signal against the position reverses it, a signal in its direction is
//! ignored. Like TradingView, a signal on a bar's close is filled at the next bar's open: the
//! strategy builds its bars from trades (a bar holds trades from its open time up to, but not
//! including, the next bar's open; time without trades has no bar), so the first trade of a new
//! bar closes the previous one, and the order fills completely at that trade's price.
//!
//! With [`Exits`], every entry also gets take-profit limit orders and a stop-market stop-loss. A
//! take-profit fill shrinks the stop to what is left (and, with break-even, moves it to the entry
//! price); a stop fill cancels the take-profits. A new signal cancels the exits and reverses
//! whatever is left of the position; a flat signal only closes it. The exits are not flagged reduce-only: Nautilus' simulated
//! venue resizes every reduce-only order to the whole position after each fill, which OKX never
//! does; the sizes kept here never exceed the position.

use std::fmt::Debug;

use nautilus_common::actor::DataActor;
use nautilus_model::{
    data::TradeTick,
    enums::OrderSide,
    orders::Order,
    events::OrderFilled,
    identifiers::{ClientOrderId, InstrumentId, StrategyId},
    types::{Price, Quantity},
};
use nautilus_trading::{
    nautilus_strategy,
    strategy::{Strategy, StrategyConfig, StrategyCore},
};
use rust_decimal::Decimal;

use crate::{exits::Exits, okx_history::Candle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Long,
    Short,
    /// Close the position, if any, and open nothing.
    Flat,
}

/// Signal logic fed one closed bar at a time, independent of Nautilus.
pub trait BarSignals: Debug {
    fn on_bar(&mut self, bar: &Candle) -> Option<Signal>;
}

const PRICE_PRECISION: u8 = 1;
const SIZE_PRECISION: u8 = 4;

pub struct ReversalStrategy {
    core: StrategyCore,
    instrument_id: InstrumentId,
    bar_ns: u64,
    trade_size: Quantity,
    signals: Box<dyn BarSignals>,
    /// The bar being built.
    bar: Option<Candle>,
    exits: Option<Exits>,
    /// The entry order waiting for its fill, and whether it opens a position to protect.
    entry: Option<(ClientOrderId, bool)>,
    /// Entry price in ticks and side of the position the exits protect.
    entry_ticks: i64,
    long: bool,
    /// Take-profits still working, and the stop.
    targets: Vec<ClientOrderId>,
    stop: Option<ClientOrderId>,
    /// Steps of 0.0001 BTC still open under the exits.
    open_steps: i64,
    targets_filled: usize,
}

impl ReversalStrategy {
    pub fn new(
        instrument_id: InstrumentId,
        bar_minutes: u64,
        trade_size: Quantity,
        signals: Box<dyn BarSignals>,
        exits: Option<Exits>,
    ) -> Self {
        let config = StrategyConfig {
            strategy_id: Some(StrategyId::from("REVERSAL-001")),
            order_id_tag: Some("001".to_string()),
            ..Default::default()
        };
        Self {
            core: StrategyCore::new(config),
            instrument_id,
            bar_ns: bar_minutes * 60_000_000_000,
            trade_size,
            signals,
            bar: None,
            exits,
            entry: None,
            entry_ticks: 0,
            long: true,
            targets: Vec::new(),
            stop: None,
            open_steps: 0,
            targets_filled: 0,
        }
    }

    fn on_signal(&mut self, signal: Signal) -> anyhow::Result<()> {
        let current = self.portfolio().net_position(&self.instrument_id);
        let size = self.trade_size.as_decimal();
        let target = match signal {
            Signal::Long if current <= Decimal::ZERO => size,
            Signal::Short if current >= Decimal::ZERO => -size,
            Signal::Flat if current != Decimal::ZERO => Decimal::ZERO,
            _ => return Ok(()),
        };
        self.cancel_exits()?;
        let delta = target - current;
        let side = if delta > Decimal::ZERO { OrderSide::Buy } else { OrderSide::Sell };
        let order = self.order().market(
            self.instrument_id,
            side,
            Quantity::from_decimal(delta.abs())?,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        self.entry = Some((order.client_order_id(), target != Decimal::ZERO));
        self.submit_order(order, None, None, None)
    }

    fn cancel_exits(&mut self) -> anyhow::Result<()> {
        for id in self.targets.drain(..).collect::<Vec<_>>().into_iter().chain(self.stop.take()) {
            self.cancel_order(id, None, None)?;
        }
        self.open_steps = 0;
        Ok(())
    }

    fn place_exits(&mut self, entry_px: Price, long: bool) -> anyhow::Result<()> {
        let Some(exits) = self.exits.clone() else { return Ok(()) };
        let steps = steps(self.trade_size);
        self.entry_ticks = ticks(entry_px);
        self.long = long;
        self.open_steps = steps;
        self.targets_filled = 0;
        let side = if long { OrderSide::Sell } else { OrderSide::Buy };
        for (k, size) in exits.target_steps(steps).into_iter().enumerate() {
            let order = self.order().limit(
                self.instrument_id,
                side,
                quantity(size),
                price(exits.target_ticks(self.entry_ticks, long, k)),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            );
            self.targets.push(order.client_order_id());
            self.submit_order(order, None, None, None)?;
        }
        let order = self.order().stop_market(
            self.instrument_id,
            side,
            quantity(steps),
            price(exits.stop_ticks(self.entry_ticks, long)),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        self.stop = Some(order.client_order_id());
        self.submit_order(order, None, None, None)
    }
}

fn steps(quantity: Quantity) -> i64 {
    (quantity.as_f64() * 10_000.0).round() as i64
}

fn quantity(steps: i64) -> Quantity {
    Quantity::new(steps as f64 / 10_000.0, SIZE_PRECISION)
}

fn ticks(price: Price) -> i64 {
    (price.as_f64() * 10.0).round() as i64
}

fn price(ticks: i64) -> Price {
    Price::new(ticks as f64 / 10.0, PRICE_PRECISION)
}

nautilus_strategy!(ReversalStrategy, {
    fn on_order_filled(&mut self, event: &OrderFilled) {
        if let Err(e) = self.handle_fill(event) {
            eprintln!("ReversalStrategy failed handling {event}: {e:#}");
        }
    }
});

impl ReversalStrategy {
    fn handle_fill(&mut self, event: &OrderFilled) -> anyhow::Result<()> {
        let id = event.client_order_id;
        if let Some((entry, opens)) = self.entry
            && entry == id
        {
            self.entry = None;
            // The portfolio is updated after this handler; the entry moved the position to
            // the target, on the order's side.
            return if opens { self.place_exits(event.last_px, event.order_side == OrderSide::Buy) } else { Ok(()) };
        }
        if let Some(k) = self.targets.iter().position(|t| *t == id) {
            self.targets.remove(k);
            self.targets_filled += 1;
            self.open_steps -= steps(event.last_qty);
            let Some(stop) = self.stop else { return Ok(()) };
            if self.open_steps <= 0 {
                self.stop = None;
                return self.cancel_order(stop, None, None);
            }
            let breakeven = self.exits.as_ref().is_some_and(|e| e.breakeven) && self.targets_filled == 1;
            let trigger = breakeven.then(|| price(self.entry_ticks));
            return self.modify_order(stop, Some(quantity(self.open_steps)), None, trigger, None, None);
        }
        if self.stop == Some(id) {
            self.stop = None;
            for target in std::mem::take(&mut self.targets) {
                self.cancel_order(target, None, None)?;
            }
            self.open_steps = 0;
        }
        Ok(())
    }
}

impl Debug for ReversalStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReversalStrategy")
            .field("instrument_id", &self.instrument_id)
            .field("bar_ns", &self.bar_ns)
            .field("trade_size", &self.trade_size)
            .field("signals", &self.signals)
            .field("exits", &self.exits)
            .finish()
    }
}

impl DataActor for ReversalStrategy {
    fn on_start(&mut self) -> anyhow::Result<()> {
        self.subscribe_trades(self.instrument_id, None, None);
        Ok(())
    }

    fn on_stop(&mut self) -> anyhow::Result<()> {
        self.unsubscribe_trades(self.instrument_id, None, None);
        Ok(())
    }

    fn on_trade(&mut self, tick: &TradeTick) -> anyhow::Result<()> {
        let ts = tick.ts_event.as_u64();
        let open = ((ts - ts % self.bar_ns) / 1_000_000) as i64;
        let price = tick.price.as_f64();
        if let Some(bar) = self.bar.as_mut()
            && bar.time_ms == open
        {
            bar.high = bar.high.max(price);
            bar.low = bar.low.min(price);
            bar.close = price;
            return Ok(());
        }
        let new = Candle { time_ms: open, open: price, high: price, low: price, close: price };
        match self.bar.replace(new) {
            Some(closed) => match self.signals.on_bar(&closed) {
                Some(signal) => self.on_signal(signal),
                None => Ok(()),
            },
            None => Ok(()),
        }
    }
}
