//! Polymarket CLOB market-channel WebSocket: full book snapshots plus
//! price_change deltas per token. Maintains local books and forwards
//! best bid/ask (Event::Book) on every change.

use std::collections::{BTreeMap, HashMap};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::types::{BookTop, Event, TokenId};

/// Price levels keyed by price in tenths of a basis point (integer to make
/// BTreeMap ordering exact).
#[derive(Default)]
pub struct LocalBook {
    bids: BTreeMap<i64, f64>, // price_key -> size
    asks: BTreeMap<i64, f64>,
}

fn key(px: f64) -> i64 {
    (px * 100_000.0).round() as i64
}

fn px_of(key: i64) -> f64 {
    key as f64 / 100_000.0
}

impl LocalBook {
    pub fn apply_snapshot(&mut self, bids: &[(f64, f64)], asks: &[(f64, f64)]) {
        self.bids.clear();
        self.asks.clear();
        for &(p, s) in bids {
            if s > 0.0 {
                self.bids.insert(key(p), s);
            }
        }
        for &(p, s) in asks {
            if s > 0.0 {
                self.asks.insert(key(p), s);
            }
        }
    }

    /// `side` is the CLOB convention: "BUY" levels are bids, "SELL" are asks.
    /// `size` is the new aggregate size at that level (0 removes it).
    pub fn apply_change(&mut self, side: &str, px: f64, size: f64) {
        let book = if side.eq_ignore_ascii_case("BUY") {
            &mut self.bids
        } else {
            &mut self.asks
        };
        if size <= 0.0 {
            book.remove(&key(px));
        } else {
            book.insert(key(px), size);
        }
    }

    pub fn top(&self) -> BookTop {
        let best_bid = self.bids.iter().next_back();
        let best_ask = self.asks.iter().next();
        BookTop {
            bid: best_bid.map(|(k, _)| px_of(*k)),
            bid_sz: best_bid.map(|(_, s)| *s).unwrap_or(0.0),
            ask: best_ask.map(|(k, _)| px_of(*k)),
            ask_sz: best_ask.map(|(_, s)| *s).unwrap_or(0.0),
        }
    }
}

fn parse_levels(v: Option<&serde_json::Value>) -> Vec<(f64, f64)> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|lvl| {
                    let p = lvl.get("price")?.as_str()?.parse::<f64>().ok()?;
                    let s = lvl.get("size")?.as_str()?.parse::<f64>().ok()?;
                    Some((p, s))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Apply one market-channel event to the book map; returns the token whose
/// book changed, if any.
pub fn apply_event(
    books: &mut HashMap<TokenId, LocalBook>,
    ev: &serde_json::Value,
) -> Option<TokenId> {
    let event_type = ev.get("event_type")?.as_str()?;
    match event_type {
        "book" => {
            let token: TokenId = ev.get("asset_id")?.as_str()?.into();
            let bids = parse_levels(ev.get("bids").or_else(|| ev.get("buys")));
            let asks = parse_levels(ev.get("asks").or_else(|| ev.get("sells")));
            books
                .entry(token.clone())
                .or_default()
                .apply_snapshot(&bids, &asks);
            Some(token)
        }
        "price_change" => {
            let token: TokenId = ev.get("asset_id")?.as_str()?.into();
            let book = books.entry(token.clone()).or_default();
            let mut changed = false;
            if let Some(changes) = ev.get("changes").and_then(|c| c.as_array()) {
                for ch in changes {
                    let (Some(p), Some(side), Some(s)) = (
                        ch.get("price").and_then(|x| x.as_str()).and_then(|x| x.parse::<f64>().ok()),
                        ch.get("side").and_then(|x| x.as_str()),
                        ch.get("size").and_then(|x| x.as_str()).and_then(|x| x.parse::<f64>().ok()),
                    ) else {
                        continue;
                    };
                    book.apply_change(side, p, s);
                    changed = true;
                }
            }
            changed.then_some(token)
        }
        _ => None,
    }
}

/// Long-lived task for one market window's tokens. Aborted by the engine on
/// window roll (a fresh task is spawned with the new token ids).
pub async fn run(ws_url: String, token_ids: Vec<TokenId>, tx: UnboundedSender<Event>) {
    let sub = serde_json::json!({
        "assets_ids": token_ids,
        "type": "market",
    })
    .to_string();
    let url = format!("{ws_url}/market");

    loop {
        let mut books: HashMap<TokenId, LocalBook> = HashMap::new();
        match connect_async(&url).await {
            Ok((ws, _)) => {
                let (mut sink, mut stream) = ws.split();
                tracing::info!("[CLOB-WS] connected to {url}, subscribing: {sub}");
                if sink.send(Message::Text(sub.clone())).await.is_err() {
                    continue;
                }
                let _ = tx.send(Event::FeedInfo("clob market ws connected".into()));
                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(Message::Text(raw)) => {
                            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                                continue;
                            };
                            let events: Vec<serde_json::Value> = match v {
                                serde_json::Value::Array(a) => a,
                                other => vec![other],
                            };
                            for ev in &events {
                                if let Some(token) = apply_event(&mut books, ev) {
                                    let top = books[&token].top();
                                    if tx.send(Event::Book { token, top }).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(Message::Ping(p)) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("[CLOB-WS] read error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("[CLOB-WS] connect failed: {e} — retrying in 3s");
            }
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_then_top() {
        let mut books = HashMap::new();
        let ev = json!({
            "event_type": "book",
            "asset_id": "tok",
            "bids": [{"price": "0.48", "size": "30"}, {"price": "0.50", "size": "10"}],
            "asks": [{"price": "0.53", "size": "5"}, {"price": "0.52", "size": "7"}]
        });
        let token = apply_event(&mut books, &ev).unwrap();
        let top = books[&token].top();
        assert_eq!(top.bid, Some(0.50));
        assert_eq!(top.bid_sz, 10.0);
        assert_eq!(top.ask, Some(0.52));
        assert_eq!(top.ask_sz, 7.0);
    }

    #[test]
    fn price_change_updates_and_removes_levels() {
        let mut books = HashMap::new();
        apply_event(
            &mut books,
            &json!({
                "event_type": "book",
                "asset_id": "tok",
                "bids": [{"price": "0.50", "size": "10"}],
                "asks": [{"price": "0.52", "size": "7"}]
            }),
        );
        // New better bid appears.
        apply_event(
            &mut books,
            &json!({
                "event_type": "price_change",
                "asset_id": "tok",
                "changes": [{"price": "0.51", "side": "BUY", "size": "3"}]
            }),
        );
        assert_eq!(books["tok"].top().bid, Some(0.51));
        // Best bid removed -> falls back to 0.50.
        apply_event(
            &mut books,
            &json!({
                "event_type": "price_change",
                "asset_id": "tok",
                "changes": [{"price": "0.51", "side": "BUY", "size": "0"}]
            }),
        );
        assert_eq!(books["tok"].top().bid, Some(0.50));
    }

    #[test]
    fn unknown_events_ignored() {
        let mut books = HashMap::new();
        assert!(apply_event(&mut books, &json!({"event_type": "tick_size_change", "asset_id": "t"})).is_none());
        assert!(apply_event(&mut books, &json!({"foo": "bar"})).is_none());
    }

    #[test]
    fn empty_book_top_is_none() {
        let b = LocalBook::default();
        let top = b.top();
        assert!(top.bid.is_none() && top.ask.is_none());
    }
}
