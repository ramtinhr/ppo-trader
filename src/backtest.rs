use std::path::Path;

use anyhow::{Result, ensure};
use chrono::NaiveDate;
use nautilus_backtest::{
    config::{BacktestEngineConfig, SimulatedVenueConfig},
    engine::BacktestEngine,
    result::BacktestResult,
};
use nautilus_common::logging::logger::LoggerConfig;
use nautilus_core::UnixNanos;
use nautilus_execution::models::{
    fee::{FeeModelAny, MakerTakerFeeModel},
    fill::{BestPriceFillModel, FillModelHandle},
};
use nautilus_model::{
    data::{Data, TradeTick},
    enums::{AccountType, AggressorSide, BookType, OmsType, OrderType},
    identifiers::{InstrumentId, Symbol, TradeId, Venue},
    instruments::{CryptoPerpetual, Instrument, InstrumentAny},
    orders::Order,
    types::{Currency, Money, Price, Quantity},
};
use rust_decimal::Decimal;

use crate::{
    config::{AppConfig, VenueConfig},
    exits::Exits,
    okx_history::{self, CONTRACT_VALUE, Trade, archive_days},
    signals::BarSignals,
    strategy::ReversalStrategy,
};

/// Re-export for bins that still `use dynamic_trader::backtest::{Fill, …}`.
pub use crate::fills::{Fill, FillKind};

pub const OKX_VENUE: &str = "OKX";
pub const BTC_PERPETUAL: &str = "BTC-USDT-SWAP.OKX";
const PRICE_PRECISION: u8 = 1;
/// Sizes are in BTC: OKX's 0.01-contract step is 0.0001 BTC.
const SIZE_PRECISION: u8 = 4;

/// OKX BTC-USDT-SWAP: linear, USDT-settled, $0.10 ticks. Sizes are in BTC with OKX's
/// 0.0001 BTC (0.01 contract) step.
pub fn btc_usdt_perpetual() -> Result<InstrumentAny> {
    let perpetual = CryptoPerpetual::builder()
        .instrument_id(InstrumentId::from(BTC_PERPETUAL))
        .raw_symbol(Symbol::from(okx_history::INSTRUMENT))
        .base_currency(Currency::BTC())
        .quote_currency(Currency::USDT())
        .settlement_currency(Currency::USDT())
        .is_inverse(false)
        .price_precision(PRICE_PRECISION)
        .size_precision(SIZE_PRECISION)
        .price_increment(Price::from("0.1"))
        .size_increment(Quantity::from("0.0001"))
        .multiplier(Quantity::from(1))
        .lot_size(Quantity::from("0.0001"))
        .min_quantity(Quantity::from("0.0001"))
        .ts_event(UnixNanos::default())
        .ts_init(UnixNanos::default())
        .build()?;
    Ok(InstrumentAny::CryptoPerpetual(perpetual))
}

/// Simulated OKX margin account in USDT, filling orders against recorded trades. Market orders
/// fill completely at the last trade price, as TradingView fills them at the bar's open.
pub fn okx_venue(
    costs: &VenueConfig,
    starting_usdt: f64,
    leverage: u32,
) -> Result<SimulatedVenueConfig> {
    let maker = bps_to_rate(costs.maker_fee_bps)?;
    let taker = bps_to_rate(costs.taker_fee_bps)?;
    Ok(SimulatedVenueConfig::builder()
        .venue(Venue::from(OKX_VENUE))
        .oms_type(OmsType::Netting)
        .account_type(AccountType::Margin)
        .book_type(BookType::L1_MBP)
        .starting_balances(vec![Money::new(starting_usdt, Currency::USDT())])
        .base_currency(Currency::USDT())
        .default_leverage(Decimal::from(leverage))
        .fee_model(FeeModelAny::MakerTaker(MakerTakerFeeModel::new(maker, taker)).into())
        .bar_execution(false)
        .trade_execution(true)
        .fill_model(FillModelHandle::new(BestPriceFillModel::default()))
        .build()?)
}

/// One backtest run on the verified archives in `data` between `start` and `end` (UTC days, end
/// exclusive), with `bar_minutes` bars built from every trade.
pub struct Run<'a> {
    pub config: &'a AppConfig,
    pub data: &'a Path,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub bar_minutes: u64,
    pub size_btc: f64,
    pub balance_usdt: f64,
}

/// Replays every trade through Nautilus, one archive at a time, with `signals` driving a
/// `ReversalStrategy` (with `exits` on every entry, if given). Returns Nautilus' result and its
/// filled orders in time order. `progress` is called with (archives done, archives in total).
pub fn run_backtest(
    run: &Run,
    signals: Box<dyn BarSignals>,
    exits: Option<Exits>,
    mut progress: impl FnMut(usize, usize),
) -> Result<(BacktestResult, Vec<Fill>)> {
    let instrument = btc_usdt_perpetual()?;
    let instrument_id = instrument.id();
    let mut engine = BacktestEngine::new(
        BacktestEngineConfig::builder()
            .logging(LoggerConfig::from_spec(
                &std::env::var("NAUTILUS_LOG").unwrap_or_else(|_| "stdout=Error".into()),
            )?)
            .build(),
    )?;
    engine.add_venue(okx_venue(
        &run.config.venues.okx,
        run.balance_usdt,
        run.config.risk.max_leverage,
    )?)?;
    engine.add_instrument(&instrument)?;
    engine.add_strategy(ReversalStrategy::new(
        instrument_id,
        run.bar_minutes,
        Quantity::new(run.size_btc, SIZE_PRECISION),
        signals,
        exits,
    ))?;

    let ms = |day: NaiveDate| {
        day.and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis()
    };
    let (from_ms, to_ms) = (ms(run.start), ms(run.end));
    let days = archive_days(run.start, run.end);
    for (i, day) in days.iter().enumerate() {
        engine.add_data(
            load_archive(run.data, *day, instrument_id, from_ms, to_ms)?,
            None,
            true,
            true,
        )?;
        engine.run(None, None, None, i + 1 < days.len())?;
        engine.clear_data();
        progress(i + 1, days.len());
    }

    let result = engine.get_result();
    let mut fills = Vec::new();
    {
        let cache = engine.kernel_mut().cache();
        let cache = cache.borrow();
        for order in cache.orders(None, None, None, None, None) {
            let Some(price) = order.avg_px() else {
                continue;
            };
            fills.push(Fill {
                time_ms: i64::try_from(order.ts_last().as_u64() / 1_000_000)?,
                kind: match order.order_type() {
                    OrderType::Limit => FillKind::TakeProfit,
                    OrderType::StopMarket => FillKind::StopLoss,
                    _ => FillKind::Signal,
                },
                buy: order.order_side() == nautilus_model::enums::OrderSide::Buy,
                size_btc: order.filled_qty().as_f64(),
                price: price.try_into()?,
                fee_usdt: order.commissions().values().map(|m| m.as_f64()).sum(),
            });
        }
    }
    fills.sort_by_key(|f| f.time_ms);
    engine.dispose();
    Ok((result, fills))
}

fn bps_to_rate(bps: f64) -> Result<Decimal> {
    Ok(Decimal::try_from(bps)? / Decimal::from(10_000))
}

/// Trades of verified OKX archive `day` between `from_ms` and `to_ms` (exclusive), as Nautilus
/// trade ticks.
pub fn load_archive(
    dir: &Path,
    day: NaiveDate,
    instrument_id: InstrumentId,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<Data>> {
    ensure!(
        okx_history::is_verified(dir, day),
        "OKX archive {day} is missing or unverified; run download_okx_trades first"
    );
    let mut ticks = Vec::new();
    okx_history::for_each_trade(&okx_history::archive_path(dir, day), |trade| {
        if (from_ms..to_ms).contains(&trade.time_ms) {
            ticks.push(Data::Trade(trade_tick(instrument_id, &trade)?));
        }
        Ok(())
    })?;
    Ok(ticks)
}

pub fn trade_tick(instrument_id: InstrumentId, trade: &Trade) -> Result<TradeTick> {
    let ts = UnixNanos::from(u64::try_from(trade.time_ms)? * 1_000_000);
    let btc = (trade.contracts * 100.0).round() * CONTRACT_VALUE / 100.0;
    Ok(TradeTick::new(
        instrument_id,
        Price::new(trade.price, PRICE_PRECISION),
        Quantity::new(btc, SIZE_PRECISION),
        if trade.buy {
            AggressorSide::Buy
        } else {
            AggressorSide::Sell
        },
        TradeId::new(&trade.trade_id.to_string()),
        ts,
        ts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_okx_trade_to_trade_tick() {
        let trade = Trade {
            trade_id: 2_916_314_144,
            buy: true,
            price: 78_536.7,
            contracts: 19.73,
            time_ms: 1_789_401_600_155,
        };
        let tick = trade_tick(InstrumentId::from(BTC_PERPETUAL), &trade).unwrap();
        assert_eq!(tick.price, Price::from("78536.7"));
        assert_eq!(tick.size, Quantity::from("0.1973"));
        assert_eq!(tick.aggressor_side, AggressorSide::Buy);
        assert_eq!(tick.ts_event.as_u64(), 1_789_401_600_155_000_000);
    }

    #[test]
    fn btc_perpetual_is_linear_with_okx_increments() {
        let instrument = btc_usdt_perpetual().unwrap();
        assert!(!instrument.is_inverse());
        assert_eq!(instrument.price_increment(), Price::from("0.1"));
        assert_eq!(instrument.size_increment(), Quantity::from("0.0001"));
    }

    #[test]
    fn converts_fee_bps_to_rate() {
        assert_eq!(bps_to_rate(3.5).unwrap(), Decimal::new(35, 5));
    }
}
