//! Bar-by-bar signal types shared by the fast scanner and the Nautilus strategy.
//!
//! Kept out of the Nautilus strategy module so the scanner compiles without linking Nautilus.

use std::fmt::Debug;

use crate::okx_history::Candle;

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
