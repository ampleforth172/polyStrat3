//! Strategies are pure state machines: a snapshot of the world plus an event
//! goes in, a list of Actions comes out. No I/O in this module tree.

pub mod maker;
pub mod taker;

use crate::types::{BookTop, Outcome, OrderTag, Side, TokenId};

/// Per-outcome view built by the engine before each strategy call.
#[derive(Debug, Clone, Default)]
pub struct SideSnap {
    pub token: TokenId,
    pub top: Option<BookTop>,
    pub pos: f64,
    /// Sellable (on-chain settled) quantity. In dry-run this equals `pos`;
    /// live it lags fills by a few seconds until the trade is mined.
    pub settled: f64,
    pub avg_entry: f64,
    /// Unfilled resting BUY qty (counts against max position).
    pub pending_buy: f64,
    /// Price of the unique resting entry buy, if any.
    pub resting_buy_px: Option<f64>,
    pub working_orders: usize,
    pub realized_pnl: f64,
    pub total_fees: f64,
}

/// World snapshot handed to strategies. Plain data, no borrows into the engine.
#[derive(Debug, Clone, Default)]
pub struct Snap {
    pub now: f64,
    pub tte: f64,
    pub trading_enabled: bool,
    pub spot: Option<f64>,
    pub fair_yes: Option<f64>,
    /// fair + alpha shift, clamped to [0.01, 0.99].
    pub quote_center_yes: Option<f64>,
    /// Trailing 1-minute |log-return| of the underlying (momentum input).
    pub momentum_ret: Option<f64>,
    pub hour_utc: u32,
    pub yes: SideSnap,
    pub no: SideSnap,
}

impl Snap {
    pub fn side(&self, o: Outcome) -> &SideSnap {
        match o {
            Outcome::Yes => &self.yes,
            Outcome::No => &self.no,
        }
    }
}

/// A fill notification reduced to what strategies need.
#[derive(Debug, Clone)]
pub struct FillInfo {
    pub outcome: Outcome,
    pub side: Side,
    pub px: f64,
    pub sz: f64,
    /// Why the filled order existed (entry / tp / sl / quote) — resolved
    /// from the OMS entry. TP/SL closes are BUYS of the opposite token, so
    /// the tag (not the side) identifies a close.
    pub tag: OrderTag,
}
