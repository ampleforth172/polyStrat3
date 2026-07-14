//! Authenticated user-channel feed via the official Polymarket SDK:
//! order and trade updates for OUR account.
//!
//! One WS client lives for the whole process (the SDK multiplexes market
//! subscriptions and auto-resubscribes on reconnect); the engine subscribes
//! per market window via `run_window` and unsubscribes on rollover.
//!
//! CRITICAL: the SDK dials the socket lazily on the first subscription —
//! callers must gate order placement on `wait_connected`, or fills that
//! happen before the subscription reaches the server are silently missed
//! (verified live; see tests/connectivity.rs test 13).

use std::str::FromStr as _;

use futures_util::StreamExt as _;
use polymarket_client_sdk_v2::auth::state::Authenticated;
use polymarket_client_sdk_v2::auth::{Credentials as ApiCredentials, Normal};
use polymarket_client_sdk_v2::clob::ws::{ChannelType, Client as WsClient, TradeMessage, WsMessage};
use polymarket_client_sdk_v2::types::{Address, B256};
use polymarket_client_sdk_v2::ws::connection::ConnectionState;
use tokio::sync::mpsc::UnboundedSender;

use crate::types::{Event, Side};

pub type AuthedWs = WsClient<Authenticated<Normal>>;

/// Build the authenticated user-channel client (no I/O yet — lazy dial).
pub fn client(creds: ApiCredentials, address: Address) -> Result<AuthedWs, String> {
    WsClient::default()
        .authenticate(creds, address)
        .map_err(|e| format!("user ws authenticate: {e}"))
}

pub fn is_connected(ws: &AuthedWs) -> bool {
    matches!(
        ws.connection_state(ChannelType::User),
        ConnectionState::Connected { .. }
    )
}

/// Wait until the user channel is CONNECTED. A subscription must already
/// have been issued (that is what triggers the dial).
pub async fn wait_connected(ws: &AuthedWs, timeout: std::time::Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while !is_connected(ws) {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "user channel not connected within {timeout:?} (state: {:?})",
                ws.connection_state(ChannelType::User)
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Subscribe to order + trade updates for one market window and forward
/// them into the engine's event queue. Runs until aborted by the engine;
/// the engine unsubscribes the market on window roll.
pub async fn run_window(ws: AuthedWs, condition_id: String, tx: UnboundedSender<Event>) {
    let cond = match B256::from_str(&condition_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("[USER-WS] bad condition id {condition_id}: {e}");
            return;
        }
    };
    // Subscribe EXACTLY ONCE per market: the SDK ref-counts subscriptions
    // per condition id and stream drops do NOT decrement, so the paired
    // subscribe_orders + subscribe_trades helpers would pin the refcount at
    // 2 while the engine's single unsubscribe only brings it to 1 — the
    // server unsubscribe would never fire and dead 15m markets would pile
    // up in the reconnect-resubscribe list. One subscribe_user_events
    // stream (refcount 1) split by variant avoids that; the engine's
    // unsubscribe on window roll then fully releases the market.
    let stream = match ws.subscribe_user_events(vec![cond]) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[USER-WS] subscribe_user_events failed: {e}");
            return;
        }
    };
    tracing::info!("[USER-WS] subscribed to orders+trades for market {condition_id}");

    let mut stream = std::pin::pin!(stream);
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(WsMessage::Order(o)) => {
                tracing::info!(
                    "[USER-WS] order {}… {:?} {:?} {:?} @ {} matched={}",
                    &o.id[..o.id.len().min(10)],
                    o.msg_type,
                    o.side,
                    o.original_size,
                    o.price,
                    o.size_matched.map(|d| d.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
            Ok(WsMessage::Trade(t)) => forward_trade(&t, &tx),
            Ok(_) => {} // market-data variants don't arrive on the user channel
            Err(e) => tracing::warn!("[USER-WS] stream error: {e}"),
        }
    }
    tracing::warn!("[USER-WS] user stream ended");
}

fn dec_f64(d: &polymarket_client_sdk_v2::types::Decimal) -> f64 {
    d.to_string().parse().unwrap_or(0.0)
}

/// Convert one trade message into per-leg UserTrade events.
///
/// We forward on FIRST sighting (status MATCHED — timely inventory beats
/// waiting minutes for on-chain CONFIRMED); later status re-deliveries of
/// the same trade dedupe in the OMS via the composed trade id. Our orders
/// are always post-only makers, so our legs are in `maker_orders`; the
/// engine resolves side/token from the OMS and ignores legs that aren't
/// ours. The taker leg is forwarded too as a belt-and-braces measure.
pub fn forward_trade(t: &TradeMessage, tx: &UnboundedSender<Event>) {
    tracing::info!(
        "[USER-WS] trade {} status={:?} {:?} {} @ {} ({} maker leg(s))",
        &t.id[..t.id.len().min(10)],
        t.status,
        t.side,
        t.size,
        t.price,
        t.maker_orders.len(),
    );
    for mo in &t.maker_orders {
        let _ = tx.send(Event::UserTrade {
            token: mo.asset_id.to_string().into(),
            // Hint only — the engine takes the authoritative side/token from
            // the OMS entry for this order id.
            side: Side::Buy,
            px: dec_f64(&mo.price),
            sz: dec_f64(&mo.matched_amount),
            order_id: mo.order_id.clone(),
            // One trade can fill several of our orders: compose per-leg ids
            // so OMS dedup keeps each leg exactly once.
            trade_id: format!("{}:{}", t.id, mo.order_id),
            maker: true,
        });
    }
    if let Some(taker_id) = &t.taker_order_id {
        let _ = tx.send(Event::UserTrade {
            token: t.asset_id.to_string().into(),
            side: match t.side {
                polymarket_client_sdk_v2::clob::types::Side::Buy => Side::Buy,
                _ => Side::Sell,
            },
            px: dec_f64(&t.price),
            sz: dec_f64(&t.size),
            order_id: taker_id.clone(),
            trade_id: format!("{}:taker", t.id),
            maker: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    /// Real frame shape captured live (tests/connectivity.rs test 13).
    const TRADE_JSON: &str = r#"{
        "event_type": "trade",
        "id": "tr-1",
        "market": "0x0b84f18390d78c8e62652abf5e7483c50772c5c3057bdaaa46871291a295280c",
        "asset_id": "21513953487211661465701355043920698966441668421200170344875399558814938901843",
        "side": "SELL",
        "size": "8",
        "price": "0.13",
        "status": "MATCHED",
        "taker_order_id": "0xtaker",
        "maker_orders": [
            {
                "asset_id": "21513953487211661465701355043920698966441668421200170344875399558814938901843",
                "matched_amount": "5",
                "order_id": "0xmine",
                "outcome": "Yes",
                "owner": "b0fa9991-b239-2723-d156-9abee9523c1e",
                "price": "0.13"
            },
            {
                "asset_id": "21513953487211661465701355043920698966441668421200170344875399558814938901843",
                "matched_amount": "3",
                "order_id": "0xother",
                "outcome": "Yes",
                "owner": "deadbeef-0000-0000-0000-000000000000",
                "price": "0.13"
            }
        ]
    }"#;

    #[test]
    fn forwards_each_maker_leg_and_taker_with_unique_trade_ids() {
        let t: TradeMessage = serde_json::from_str(TRADE_JSON).unwrap();
        let (tx, mut rx) = unbounded_channel();
        forward_trade(&t, &tx);
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::UserTrade { order_id, trade_id, px, sz, maker, .. } = ev {
                got.push((order_id, trade_id, px, sz, maker));
            }
        }
        assert_eq!(got.len(), 3, "2 maker legs + 1 taker leg");
        assert_eq!(got[0].0, "0xmine");
        assert_eq!(got[0].1, "tr-1:0xmine");
        assert!((got[0].2 - 0.13).abs() < 1e-12);
        assert!((got[0].3 - 5.0).abs() < 1e-12);
        assert!(got[0].4, "maker leg flagged maker");
        assert_eq!(got[1].1, "tr-1:0xother", "per-leg ids stay unique");
        assert_eq!(got[2].0, "0xtaker");
        assert!(!got[2].4, "taker leg flagged taker");
    }
}
