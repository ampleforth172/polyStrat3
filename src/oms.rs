//! Order management: registry of working orders, state transitions,
//! pending-buy accounting, fill/ack correlation, trade-id dedup.

use std::collections::{HashMap, HashSet};

use crate::types::{OrderTag, Side, TokenId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    /// Submitted; exchange ack (and id) not received yet.
    PendingNew,
    PendingCancel,
    Open,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct Order {
    pub client_id: u64,
    pub exchange_id: Option<String>,
    pub token: TokenId,
    pub side: Side,
    pub px: f64,
    pub sz: f64,
    pub filled: f64,
    pub tag: OrderTag,
    pub state: OrderState,
}

impl Order {
    pub fn is_working(&self) -> bool {
        matches!(
            self.state,
            OrderState::PendingNew | OrderState::PendingCancel | OrderState::Open
        )
    }
}

#[derive(Default)]
pub struct Oms {
    next_id: u64,
    orders: HashMap<u64, Order>,
    by_exchange_id: HashMap<String, u64>,
    seen_trade_ids: HashSet<String>,
}

impl Oms {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a just-submitted order; returns its client id.
    pub fn submit(&mut self, token: &TokenId, side: Side, px: f64, sz: f64, tag: OrderTag) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.orders.insert(
            id,
            Order {
                client_id: id,
                exchange_id: None,
                token: token.clone(),
                side,
                px,
                sz,
                filled: 0.0,
                tag,
                state: OrderState::PendingNew,
            },
        );
        id
    }

    /// Apply an exchange ack. Returns Some(exchange_id) when a DEFERRED
    /// cancel must be sent now (the order was cancel-requested while its
    /// ack was still in flight).
    pub fn on_ack(&mut self, client_id: u64, result: &Result<String, String>) -> Option<String> {
        let o = self.orders.get_mut(&client_id)?;
        match result {
            Ok(exchange_id) => {
                o.exchange_id = Some(exchange_id.clone());
                self.by_exchange_id.insert(exchange_id.clone(), client_id);
                if o.state == OrderState::PendingCancel {
                    // Cancel was requested before the id existed: release
                    // the exposure now and hand the id back for the wire
                    // cancel.
                    o.state = OrderState::Cancelled;
                    return Some(exchange_id.clone());
                }
                o.state = OrderState::Open;
                None
            }
            Err(_) => {
                o.state = OrderState::Rejected;
                None
            }
        }
    }

    /// Register a trade id; returns true when it was already seen
    /// (WS redelivery on reconnect). Call before applying any fill.
    pub fn is_duplicate_trade(&mut self, trade_id: &str) -> bool {
        !trade_id.is_empty() && !self.seen_trade_ids.insert(trade_id.to_string())
    }

    /// Apply a (already-deduped) trade to its order, if we know the order.
    /// A fill for an unknown order id still counts against the position —
    /// the caller applies position updates regardless.
    pub fn on_trade(&mut self, order_id: &str, trade_id: &str, sz: f64) -> Option<Order> {
        let _ = trade_id;
        let cid = self.by_exchange_id.get(order_id).copied()?;
        let o = self.orders.get_mut(&cid)?;
        o.filled += sz;
        if o.filled >= o.sz - 1e-9 {
            o.state = OrderState::Filled;
        }
        Some(o.clone())
    }

    /// Request cancellation of one order. An order without an exchange id
    /// yet becomes PendingCancel (still counted as exposure; the wire cancel
    /// is deferred to its ack); an acked order is marked Cancelled.
    pub fn mark_cancelled(&mut self, client_id: u64) {
        if let Some(o) = self.orders.get_mut(&client_id) {
            if o.is_working() {
                o.state = if o.exchange_id.is_some() {
                    OrderState::Cancelled
                } else {
                    OrderState::PendingCancel
                };
            }
        }
    }

    /// Request cancellation of all working orders on a token. Returns the
    /// exchange ids that can be cancelled NOW; un-acked orders transition to
    /// PendingCancel and their wire cancel is deferred to the ack.
    pub fn cancel_token(&mut self, token: &str) -> Vec<String> {
        let mut ids = Vec::new();
        for o in self.orders.values_mut() {
            if o.token == token && o.is_working() {
                if let Some(eid) = &o.exchange_id {
                    o.state = OrderState::Cancelled;
                    ids.push(eid.clone());
                } else if o.state == OrderState::PendingNew {
                    o.state = OrderState::PendingCancel;
                }
            }
        }
        ids
    }

    pub fn cancel_all(&mut self) -> Vec<String> {
        let mut ids = Vec::new();
        for o in self.orders.values_mut() {
            if o.is_working() {
                if let Some(eid) = &o.exchange_id {
                    o.state = OrderState::Cancelled;
                    ids.push(eid.clone());
                } else if o.state == OrderState::PendingNew {
                    o.state = OrderState::PendingCancel;
                }
            }
        }
        ids
    }

    pub fn working_orders(&self, token: &str) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| o.token == token && o.is_working())
            .collect()
    }

    pub fn working_count(&self, token: &str) -> usize {
        self.working_orders(token).len()
    }

    /// Unfilled BUY quantity resting on a token (counts against max position).
    pub fn pending_buy_qty(&self, token: &str) -> f64 {
        self.orders
            .values()
            .filter(|o| o.token == token && o.is_working() && o.side == Side::Buy)
            .map(|o| o.sz - o.filled)
            .sum()
    }

    /// Like pending_buy_qty, but ONLY EntryBuy orders. TP/SL closes are
    /// buys resting on the OPPOSITE token — that token's own entry
    /// management must not mistake them for its stale entry (observed live:
    /// the YES side kept cancelling the NO side's TP in a loop).
    pub fn pending_entry_buy_qty(&self, token: &str) -> f64 {
        self.orders
            .values()
            .filter(|o| {
                o.token == token
                    && o.is_working()
                    && o.side == Side::Buy
                    && o.tag == OrderTag::EntryBuy
            })
            .map(|o| o.sz - o.filled)
            .sum()
    }

    /// Price of the resting entry-buy on a token, if exactly one is working.
    /// PendingCancel orders are EXCLUDED: their price must never be treated
    /// as "the resting quote" (strategies would cancel-replace against an
    /// order that is already going away), yet they still count in
    /// pending_buy_qty — which is exactly what blocks a re-place until the
    /// deferred cancel resolves.
    pub fn resting_buy_price(&self, token: &str) -> Option<f64> {
        let buys: Vec<&Order> = self
            .orders
            .values()
            .filter(|o| {
                o.token == token
                    && o.side == Side::Buy
                    && o.tag == OrderTag::EntryBuy
                    && matches!(o.state, OrderState::PendingNew | OrderState::Open)
            })
            .collect();
        match buys.as_slice() {
            [o] => Some(o.px),
            _ => None,
        }
    }

    /// Display helper: the unique resting BUY on a token regardless of tag
    /// (maker quotes are QuoteBid, taker entries EntryBuy). Strategy logic
    /// must keep using the tag-filtered resting_buy_price.
    pub fn any_resting_buy_price(&self, token: &str) -> Option<f64> {
        let buys: Vec<&Order> = self
            .orders
            .values()
            .filter(|o| {
                o.token == token
                    && o.side == Side::Buy
                    && matches!(o.state, OrderState::PendingNew | OrderState::Open)
            })
            .collect();
        match buys.as_slice() {
            [o] => Some(o.px),
            _ => None,
        }
    }

    /// Start a fresh window: forget filled/cancelled orders, keep dedup set
    /// bounded by clearing it (trade ids are unique per market anyway).
    pub fn reset_window(&mut self) {
        self.orders.retain(|_, o| o.is_working());
        self.seen_trade_ids.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TokenId {
        s.into()
    }

    fn ack(oms: &mut Oms, cid: u64, eid: &str) {
        oms.on_ack(cid, &Ok(eid.to_string()));
    }

    #[test]
    fn lifecycle_pending_open_filled() {
        let mut oms = Oms::new();
        let cid = oms.submit(&t("tokA"), Side::Buy, 0.9, 5.0, OrderTag::EntryBuy);
        assert_eq!(oms.working_count("tokA"), 1);
        assert!((oms.pending_buy_qty("tokA") - 5.0).abs() < 1e-12);
        ack(&mut oms, cid, "ex-1");
        let o = oms.on_trade("ex-1", "t-1", 5.0).unwrap();
        assert_eq!(o.state, OrderState::Filled);
        assert_eq!(oms.working_count("tokA"), 0);
        assert_eq!(oms.pending_buy_qty("tokA"), 0.0);
    }

    #[test]
    fn partial_fill_keeps_order_working() {
        let mut oms = Oms::new();
        let cid = oms.submit(&t("tokA"), Side::Buy, 0.9, 5.0, OrderTag::EntryBuy);
        ack(&mut oms, cid, "ex-1");
        let o = oms.on_trade("ex-1", "t-1", 2.0).unwrap();
        assert_eq!(o.state, OrderState::Open);
        assert!((oms.pending_buy_qty("tokA") - 3.0).abs() < 1e-12);
    }

    #[test]
    fn duplicate_trade_id_detected() {
        let mut oms = Oms::new();
        assert!(!oms.is_duplicate_trade("t-1"));
        assert!(oms.is_duplicate_trade("t-1"));
        // Empty trade ids never dedup (no id to key on).
        assert!(!oms.is_duplicate_trade(""));
        assert!(!oms.is_duplicate_trade(""));
    }

    #[test]
    fn rejection_removes_pending_qty() {
        let mut oms = Oms::new();
        let cid = oms.submit(&t("tokA"), Side::Buy, 0.9, 5.0, OrderTag::EntryBuy);
        oms.on_ack(cid, &Err("insufficient balance".into()));
        assert_eq!(oms.pending_buy_qty("tokA"), 0.0);
        assert_eq!(oms.working_count("tokA"), 0);
    }

    #[test]
    fn cancel_of_unacked_order_defers_and_keeps_exposure() {
        let mut oms = Oms::new();
        let cid = oms.submit(&t("tokA"), Side::Buy, 0.84, 5.2, OrderTag::EntryBuy);
        // Cancel BEFORE the ack: nothing cancellable at the exchange yet.
        let ids = oms.cancel_token("tokA");
        assert!(ids.is_empty(), "no exchange id to cancel yet");
        // The order must STILL count as exposure (this is the fix for the
        // live double-fill: the exchange still has it).
        assert!((oms.pending_buy_qty("tokA") - 5.2).abs() < 1e-9);
        assert_eq!(oms.working_count("tokA"), 1);
        // ...but must not look like a resting quote to strategies.
        assert_eq!(oms.resting_buy_price("tokA"), None);
        // Ack arrives -> the deferred cancel id is handed back and the
        // exposure is released.
        let deferred = oms.on_ack(cid, &Ok("ex-1".into()));
        assert_eq!(deferred.as_deref(), Some("ex-1"));
        assert_eq!(oms.pending_buy_qty("tokA"), 0.0);
        assert_eq!(oms.working_count("tokA"), 0);
    }

    #[test]
    fn fill_on_pending_cancel_order_still_applies() {
        let mut oms = Oms::new();
        let cid = oms.submit(&t("tokA"), Side::Buy, 0.84, 5.2, OrderTag::EntryBuy);
        oms.cancel_token("tokA"); // PendingCancel, ack in flight
        // The exchange matched it before our (unsendable) cancel.
        let deferred = oms.on_ack(cid, &Ok("ex-1".into()));
        assert!(deferred.is_some());
        let o = oms.on_trade("ex-1", "t-1", 5.2).expect("fill applies to known order");
        assert!((o.filled - 5.2).abs() < 1e-9);
        assert_eq!(o.side, Side::Buy);
    }

    #[test]
    fn normal_ack_returns_no_deferred_cancel() {
        let mut oms = Oms::new();
        let cid = oms.submit(&t("tokA"), Side::Buy, 0.84, 5.2, OrderTag::EntryBuy);
        assert_eq!(oms.on_ack(cid, &Ok("ex-1".into())), None);
        assert_eq!(oms.resting_buy_price("tokA"), Some(0.84));
    }

    #[test]
    fn cancel_token_scopes_to_token() {
        let mut oms = Oms::new();
        let a = oms.submit(&t("tokA"), Side::Buy, 0.9, 5.0, OrderTag::EntryBuy);
        let b = oms.submit(&t("tokB"), Side::Buy, 0.4, 5.0, OrderTag::EntryBuy);
        ack(&mut oms, a, "ex-a");
        ack(&mut oms, b, "ex-b");
        let cancelled = oms.cancel_token("tokA");
        assert_eq!(cancelled, vec!["ex-a".to_string()]);
        assert_eq!(oms.working_count("tokA"), 0);
        assert_eq!(oms.working_count("tokB"), 1);
    }

    #[test]
    fn resting_buy_price_only_when_unique() {
        let mut oms = Oms::new();
        assert!(oms.resting_buy_price("tokA").is_none());
        oms.submit(&t("tokA"), Side::Buy, 0.9, 5.0, OrderTag::EntryBuy);
        assert_eq!(oms.resting_buy_price("tokA"), Some(0.9));
        oms.submit(&t("tokA"), Side::Buy, 0.89, 5.0, OrderTag::EntryBuy);
        assert!(oms.resting_buy_price("tokA").is_none());
    }

    #[test]
    fn reset_window_clears_dedup_and_dead_orders() {
        let mut oms = Oms::new();
        let cid = oms.submit(&t("tokA"), Side::Buy, 0.9, 5.0, OrderTag::EntryBuy);
        ack(&mut oms, cid, "ex-1");
        assert!(!oms.is_duplicate_trade("t-1"));
        oms.on_trade("ex-1", "t-1", 5.0);
        oms.reset_window();
        // Same trade id usable again after reset.
        assert!(!oms.is_duplicate_trade("t-1"));
    }
}
