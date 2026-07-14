//! Position management: per-token inventory, weighted average entry,
//! realized PnL, fee accrual, settlement marking.

use crate::types::Side;

/// Polymarket fee = C × p × 0.25 × (p × (1 − p))²
pub fn calc_fee(price: f64, size: f64) -> f64 {
    size * price * 0.25 * (price * (1.0 - price)).powi(2)
}

pub const MAKER_FEE_DISCOUNT: f64 = 0.8;

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub position: f64,
    /// On-chain SETTLED (sellable) quantity — live mode only, synced from
    /// the Data API; lags fills by the 1–3s mint time. Dry-run snapshots
    /// substitute `position` (nothing to mint).
    pub settled: f64,
    pub avg_entry: f64,
    pub realized_pnl: f64,
    pub total_fees: f64,
    pub buy_count: usize,
    pub last_bid: f64,
}

impl Position {
    /// Apply a fill. Returns the realized PnL of this leg (0 for buys).
    /// Fee accrual is separate (`add_fee`) so maker discounts stay explicit.
    pub fn on_fill(&mut self, side: Side, sz: f64, px: f64) -> f64 {
        match side {
            Side::Buy => {
                let total_cost = self.avg_entry * self.position + px * sz;
                self.position += sz;
                self.avg_entry = if self.position > 0.0 {
                    total_cost / self.position
                } else {
                    0.0
                };
                self.buy_count += 1;
                0.0
            }
            Side::Sell => {
                let close_sz = sz.min(self.position);
                let pnl = (px - self.avg_entry) * close_sz;
                self.realized_pnl += pnl;
                self.position = (self.position - close_sz).max(0.0);
                if self.position == 0.0 {
                    self.avg_entry = 0.0;
                }
                pnl
            }
        }
    }

    pub fn add_fee(&mut self, price: f64, size: f64, maker: bool) -> f64 {
        let mut fee = calc_fee(price, size);
        if maker {
            fee *= MAKER_FEE_DISCOUNT;
        }
        self.total_fees += fee;
        fee
    }

    /// Unrealized PnL marked at `mark` (mid or bid).
    pub fn unrealized(&self, mark: f64) -> f64 {
        if self.position > 0.0 {
            (mark - self.avg_entry) * self.position
        } else {
            0.0
        }
    }

    /// Settlement mark: binary token resolves 1 if last bid > 0.5 else 0.
    pub fn settlement_price(&self) -> f64 {
        if self.last_bid > 0.5 {
            1.0
        } else {
            0.0
        }
    }
}

/// Matched YES/NO pairs are risk-free: each pair pays exactly $1 at expiry.
/// Returns (matched_pairs, locked_in_pnl).
pub fn matched_pair_pnl(yes: &Position, no: &Position) -> (f64, f64) {
    let matched = yes.position.min(no.position);
    let pnl = if matched > 0.0 {
        (1.0 - yes.avg_entry - no.avg_entry) * matched
    } else {
        0.0
    };
    (matched, pnl)
}
