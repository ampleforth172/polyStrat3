//! Chainlink price feed via Polymarket RTDS WebSocket.
//! Long-lived task: reconnects with backoff, forwards PriceTick events.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::types::Event;

const PING_SECS: u64 = 5;

pub fn parse_tick(raw: &str, chainlink_symbol: &str) -> Option<(f64, f64)> {
    let msg: serde_json::Value = serde_json::from_str(raw).ok()?;
    if msg.get("topic")?.as_str()? != "crypto_prices_chainlink" {
        return None;
    }
    let payload = msg.get("payload")?;
    if payload.get("symbol")?.as_str()? != chainlink_symbol {
        return None;
    }
    let px = payload.get("value")?.as_f64()?;
    let ts_ms = payload.get("timestamp")?.as_f64()?;
    Some((px, ts_ms / 1000.0))
}

pub async fn run(rtds_url: String, symbol: String, tx: UnboundedSender<Event>) {
    let chainlink_symbol = format!("{}/usd", symbol.to_lowercase());
    let sub = serde_json::json!({
        "action": "subscribe",
        "subscriptions": [{"topic": "crypto_prices_chainlink", "type": "*", "filters": ""}],
    })
    .to_string();

    loop {
        match connect_async(&rtds_url).await {
            Ok((ws, _)) => {
                let (mut sink, mut stream) = ws.split();
                tracing::info!("[RTDS] connected to {rtds_url}, subscribing: {sub}");
                if sink.send(Message::Text(sub.clone())).await.is_err() {
                    continue;
                }
                let _ = tx.send(Event::FeedInfo("rtds connected".into()));
                let mut ping = tokio::time::interval(std::time::Duration::from_secs(PING_SECS));
                ping.tick().await; // immediate first tick consumed
                loop {
                    tokio::select! {
                        _ = ping.tick() => {
                            if sink.send(Message::Text("PING".into())).await.is_err() {
                                break;
                            }
                        }
                        msg = stream.next() => {
                            match msg {
                                Some(Ok(Message::Text(raw))) => {
                                    if raw == "PONG" { continue; }
                                    if let Some((px, ts)) = parse_tick(&raw, &chainlink_symbol) {
                                        if tx.send(Event::PriceTick { px, ts }).is_err() {
                                            return; // engine gone
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => {
                                    tracing::warn!("[RTDS] read error: {e}");
                                    break;
                                }
                                None => break,
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("[RTDS] connect failed: {e} — retrying in 5s");
            }
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_matching_tick() {
        let raw = r#"{"topic":"crypto_prices_chainlink","type":"update","payload":{"symbol":"btc/usd","value":118000.5,"timestamp":1752200000123}}"#;
        let (px, ts) = parse_tick(raw, "btc/usd").unwrap();
        assert!((px - 118_000.5).abs() < 1e-9);
        assert!((ts - 1_752_200_000.123).abs() < 1e-6);
    }

    #[test]
    fn ignores_other_symbols_topics_and_garbage() {
        let raw = r#"{"topic":"crypto_prices_chainlink","payload":{"symbol":"eth/usd","value":3000.0,"timestamp":1}}"#;
        assert!(parse_tick(raw, "btc/usd").is_none());
        let raw = r#"{"topic":"comments","payload":{}}"#;
        assert!(parse_tick(raw, "btc/usd").is_none());
        assert!(parse_tick("not json", "btc/usd").is_none());
    }
}
