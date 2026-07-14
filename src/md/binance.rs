//! Binance combined-stream WebSocket: partial depth (top-20 @ 100ms) plus
//! aggTrade for the alpha signal inputs.

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::types::Event;

fn parse_px_qty_array(v: Option<&serde_json::Value>) -> Vec<(f64, f64)> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|lvl| {
                    let a = lvl.as_array()?;
                    let p = a.first()?.as_str()?.parse::<f64>().ok()?;
                    let q = a.get(1)?.as_str()?.parse::<f64>().ok()?;
                    Some((p, q))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a combined-stream frame into an Event.
pub fn parse_frame(raw: &str, now: f64) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let stream = v.get("stream")?.as_str()?;
    let data = v.get("data")?;
    if stream.contains("@depth") {
        let bids = parse_px_qty_array(data.get("bids"));
        let asks = parse_px_qty_array(data.get("asks"));
        if bids.is_empty() && asks.is_empty() {
            return None;
        }
        Some(Event::BinanceBook { bids, asks, ts: now })
    } else if stream.contains("@aggTrade") {
        let px = data.get("p")?.as_str()?.parse::<f64>().ok()?;
        let sz = data.get("q")?.as_str()?.parse::<f64>().ok()?;
        let is_buyer_maker = data.get("m")?.as_bool()?;
        let ts = data
            .get("T")
            .and_then(|t| t.as_f64())
            .map(|ms| ms / 1000.0)
            .unwrap_or(now);
        Some(Event::BinanceTrade {
            px,
            sz,
            is_buyer_maker,
            ts,
        })
    } else {
        None
    }
}

pub async fn run(binance_ws: String, symbol: String, tx: UnboundedSender<Event>) {
    let sym = format!("{}usdt", symbol.to_lowercase());
    let url = format!("{binance_ws}?streams={sym}@depth20@100ms/{sym}@aggTrade");

    loop {
        match connect_async(&url).await {
            Ok((ws, _)) => {
                let (mut sink, mut stream) = ws.split();
                tracing::info!("[BINANCE-WS] connected, streams subscribed via URL: {url}");
                let _ = tx.send(Event::FeedInfo("binance ws connected".into()));
                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(Message::Text(raw)) => {
                            if let Some(ev) = parse_frame(&raw, crate::types::now_secs()) {
                                if tx.send(ev).is_err() {
                                    return;
                                }
                            }
                        }
                        Ok(Message::Ping(p)) => {
                            let _ = sink.send(Message::Pong(p)).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("[BINANCE-WS] read error: {e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("[BINANCE-WS] connect failed: {e} — retrying in 5s");
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
    fn parses_depth_frame() {
        let raw = r#"{"stream":"btcusdt@depth20@100ms","data":{"lastUpdateId":1,"bids":[["118000.10","0.5"],["118000.00","1.2"]],"asks":[["118000.20","0.3"]]}}"#;
        match parse_frame(raw, 42.0) {
            Some(Event::BinanceBook { bids, asks, ts }) => {
                assert_eq!(bids.len(), 2);
                assert!((bids[0].0 - 118_000.10).abs() < 1e-9);
                assert_eq!(asks.len(), 1);
                assert_eq!(ts, 42.0);
            }
            other => panic!("expected BinanceBook, got {other:?}"),
        }
    }

    #[test]
    fn parses_agg_trade_frame() {
        let raw = r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","p":"118001.5","q":"0.25","m":true,"T":1752200000123}}"#;
        match parse_frame(raw, 0.0) {
            Some(Event::BinanceTrade { px, sz, is_buyer_maker, ts }) => {
                assert!((px - 118_001.5).abs() < 1e-9);
                assert!((sz - 0.25).abs() < 1e-9);
                assert!(is_buyer_maker);
                assert!((ts - 1_752_200_000.123).abs() < 1e-6);
            }
            other => panic!("expected BinanceTrade, got {other:?}"),
        }
    }

    #[test]
    fn ignores_unknown_streams_and_garbage() {
        assert!(parse_frame(r#"{"stream":"btcusdt@kline_1m","data":{}}"#, 0.0).is_none());
        assert!(parse_frame("not json", 0.0).is_none());
    }
}
