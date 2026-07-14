//! DryRun executor: orders rest locally and fill against the live book,
//! reproducing the Python simulation — a resting BUY fills when the ask
//! crosses down to its limit; a resting SELL fills when the bid crosses up.

use std::collections::HashMap;

use crate::types::{BookTop, OrderTag, Side, TokenId};

#[derive(Debug, Clone)]
struct RestingOrder {
    client_id: u64,
    exchange_id: String,
    token: TokenId,
    side: Side,
    px: f64,
    sz: f64,
    tag: OrderTag,
}

#[derive(Debug, Clone)]
pub struct SimFill {
    pub token: TokenId,
    pub side: Side,
    pub px: f64,
    pub sz: f64,
    pub order_id: String,
    pub trade_id: String,
    pub tag: OrderTag,
}

#[derive(Default)]
pub struct DryRunExec {
    resting: HashMap<String, RestingOrder>,
    seq: u64,
}

impl DryRunExec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept an order; returns the fake exchange id (acked immediately).
    pub fn place(&mut self, client_id: u64, token: &TokenId, side: Side, px: f64, sz: f64, tag: OrderTag) -> String {
        self.seq += 1;
        let exchange_id = format!("dry-{}", self.seq);
        tracing::debug!(
            "[DRY-RUN] accepted [{tag}] {side} {sz} @ {px:.4} (token {token_short}…, {exchange_id})",
            token_short = &token[..token.len().min(8)]
        );
        self.resting.insert(
            exchange_id.clone(),
            RestingOrder {
                client_id,
                exchange_id: exchange_id.clone(),
                token: token.clone(),
                side,
                px,
                sz,
                tag,
            },
        );
        exchange_id
    }

    pub fn cancel_order(&mut self, exchange_id: &str) -> bool {
        let removed = self.resting.remove(exchange_id).is_some();
        if removed {
            tracing::debug!("[DRY-RUN] cancelled order {exchange_id}");
        }
        removed
    }

    pub fn cancel_token(&mut self, token: &str) -> usize {
        let before = self.resting.len();
        self.resting.retain(|_, o| o.token != token);
        let n = before - self.resting.len();
        if n > 0 {
            tracing::debug!(
                "[DRY-RUN] cancelled {n} order(s) on token {token_short}…",
                token_short = &token[..token.len().min(8)]
            );
        }
        n
    }

    pub fn cancel_all(&mut self) -> usize {
        let n = self.resting.len();
        self.resting.clear();
        if n > 0 {
            tracing::debug!("[DRY-RUN] cancelled all {n} resting order(s)");
        }
        n
    }

    pub fn resting_count(&self) -> usize {
        self.resting.len()
    }

    /// Check resting orders against a fresh book top; remove and return fills.
    /// Fills execute at the order's limit price (maker assumption).
    pub fn on_book(&mut self, token: &str, top: &BookTop) -> Vec<SimFill> {
        let mut fills = Vec::new();
        let filled_ids: Vec<String> = self
            .resting
            .values()
            .filter(|o| {
                if o.token != token {
                    return false;
                }
                match o.side {
                    Side::Buy => top.ask.map(|a| a <= o.px).unwrap_or(false),
                    Side::Sell => top.bid.map(|b| b >= o.px).unwrap_or(false),
                }
            })
            .map(|o| o.exchange_id.clone())
            .collect();
        for id in filled_ids {
            let o = self.resting.remove(&id).unwrap();
            self.seq += 1;
            fills.push(SimFill {
                token: o.token,
                side: o.side,
                px: o.px,
                sz: o.sz,
                order_id: o.exchange_id,
                trade_id: format!("dry-trade-{}", self.seq),
                tag: o.tag,
            });
            let _ = o.client_id;
        }
        fills
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TokenId {
        s.into()
    }

    fn top(bid: f64, ask: f64) -> BookTop {
        BookTop {
            bid: Some(bid),
            bid_sz: 100.0,
            ask: Some(ask),
            ask_sz: 100.0,
        }
    }

    #[test]
    fn buy_fills_when_ask_crosses_down() {
        let mut x = DryRunExec::new();
        x.place(1, &t("T"), Side::Buy, 0.90, 5.0, OrderTag::EntryBuy);
        // Ask above limit: no fill.
        assert!(x.on_book("T", &top(0.90, 0.92)).is_empty());
        // Ask crosses to the limit: fill at our limit price.
        let fills = x.on_book("T", &top(0.88, 0.90));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].side, Side::Buy);
        assert!((fills[0].px - 0.90).abs() < 1e-12);
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn sell_fills_when_bid_crosses_up() {
        let mut x = DryRunExec::new();
        x.place(1, &t("T"), Side::Sell, 0.93, 5.0, OrderTag::TakeProfit);
        assert!(x.on_book("T", &top(0.92, 0.94)).is_empty());
        let fills = x.on_book("T", &top(0.93, 0.95));
        assert_eq!(fills.len(), 1);
        assert!((fills[0].px - 0.93).abs() < 1e-12);
    }

    #[test]
    fn fills_scoped_to_token() {
        let mut x = DryRunExec::new();
        x.place(1, &t("A"), Side::Buy, 0.90, 5.0, OrderTag::EntryBuy);
        x.place(2, &t("B"), Side::Buy, 0.90, 5.0, OrderTag::EntryBuy);
        let fills = x.on_book("A", &top(0.88, 0.90));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].token, "A");
        assert_eq!(x.resting_count(), 1);
    }

    #[test]
    fn cancel_token_and_cancel_all() {
        let mut x = DryRunExec::new();
        x.place(1, &t("A"), Side::Buy, 0.9, 5.0, OrderTag::EntryBuy);
        x.place(2, &t("A"), Side::Sell, 0.95, 5.0, OrderTag::TakeProfit);
        x.place(3, &t("B"), Side::Buy, 0.4, 5.0, OrderTag::QuoteBid);
        assert_eq!(x.cancel_token("A"), 2);
        assert_eq!(x.resting_count(), 1);
        assert_eq!(x.cancel_all(), 1);
        assert_eq!(x.resting_count(), 0);
    }

    #[test]
    fn trade_ids_unique() {
        let mut x = DryRunExec::new();
        x.place(1, &t("T"), Side::Buy, 0.90, 5.0, OrderTag::EntryBuy);
        x.place(2, &t("T"), Side::Buy, 0.91, 5.0, OrderTag::EntryBuy);
        let fills = x.on_book("T", &top(0.85, 0.88));
        assert_eq!(fills.len(), 2);
        assert_ne!(fills[0].trade_id, fills[1].trade_id);
    }
}
