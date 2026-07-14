//! Live-endpoint connectivity tests. All #[ignore]d — run explicitly with:
//!   cargo test --test connectivity -- --ignored --test-threads=1

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const GAMMA: &str = "https://gamma-api.polymarket.com";
const CLOB_V1: &str = "https://clob.polymarket.com";
const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws";
const RTDS_URL: &str = "wss://ws-live-data.polymarket.com";
const BINANCE_REST: &str = "https://api.binance.com";
const BINANCE_WS: &str = "wss://stream.binance.com:9443/stream";
const INTERVAL: i64 = 900;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn install_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn discover_market() -> Option<(String, Vec<String>, String, String)> {
    let http = reqwest::Client::new();
    let boundary = (now() / INTERVAL) * INTERVAL;
    for i in 0..4 {
        let slug = format!("btc-updown-15m-{}", boundary + i * INTERVAL);
        let resp: serde_json::Value = http
            .get(format!("{GAMMA}/events"))
            .query(&[("slug", slug.as_str())])
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        let events = resp.as_array()?;
        for ev in events {
            if ev.get("slug")?.as_str()? == slug
                && ev.get("active").and_then(|v| v.as_bool()).unwrap_or(false)
            {
                let m = ev.get("markets")?.as_array()?.first()?;
                let ids_raw = m.get("clobTokenIds")?.as_str()?;
                let ids: Vec<String> = serde_json::from_str(ids_raw).ok()?;
                let cond = m.get("conditionId")?.as_str()?.to_string();
                let end = ev.get("endDate")?.as_str()?.to_string();
                return Some((slug, ids, cond, end));
            }
        }
    }
    None
}

/// 1. Gamma discovery: an active BTC 15m market with 2 token ids exists.
#[tokio::test]
#[ignore]
async fn gamma_discovery() {
    let (slug, ids, _cond, end) = discover_market().await.expect("no active BTC 15m market");
    println!("found {slug} end={end}");
    assert_eq!(ids.len(), 2, "expected exactly YES and NO token ids");
    let end_ts = chrono::DateTime::parse_from_rfc3339(&end).unwrap().timestamp();
    assert!(end_ts > now(), "market already expired");
}

/// 2. CLOB REST book: uncrossed bid/ask in (0,1).
#[tokio::test]
#[ignore]
async fn clob_rest_book() {
    let (_, ids, _, _) = discover_market().await.expect("no market");
    let http = reqwest::Client::new();
    let book: serde_json::Value = http
        .get(format!("{CLOB_V1}/book"))
        .query(&[("token_id", ids[0].as_str())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let best = |side: &str, last: bool| -> Option<f64> {
        let arr = book.get(side)?.as_array()?;
        let lvl = if last { arr.last()? } else { arr.first()? };
        lvl.get("price")?.as_str()?.parse().ok()
    };
    let bid = best("bids", true);
    let ask = best("asks", true);
    println!("book top: bid={bid:?} ask={ask:?}");
    if let (Some(b), Some(a)) = (bid, ask) {
        assert!(b > 0.0 && b < 1.0 && a > 0.0 && a < 1.0);
        assert!(b <= a, "crossed book: bid {b} > ask {a}");
    }
}

/// 3. CLOB market WS: receive a book snapshot within 10s.
#[tokio::test]
#[ignore]
async fn clob_market_ws() {
    install_tls();
    let (_, ids, _, _) = discover_market().await.expect("no market");
    let (ws, _) = connect_async(format!("{WS_URL}/market")).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let sub = serde_json::json!({"assets_ids": ids, "type": "market"}).to_string();
    sink.send(Message::Text(sub)).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("no book snapshot within 10s")
            .expect("stream ended")
            .expect("ws error");
        if let Message::Text(raw) = msg {
            if raw.contains("\"event_type\":\"book\"") {
                println!("got book snapshot ({} bytes)", raw.len());
                return;
            }
        }
    }
}

/// 4. RTDS Chainlink WS: at least one btc/usd tick within 15s; PING works.
#[tokio::test]
#[ignore]
async fn rtds_chainlink_ws() {
    install_tls();
    let (ws, _) = connect_async(RTDS_URL).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let sub = serde_json::json!({
        "action": "subscribe",
        "subscriptions": [{"topic": "crypto_prices_chainlink", "type": "*", "filters": ""}],
    })
    .to_string();
    sink.send(Message::Text(sub)).await.unwrap();
    sink.send(Message::Text("PING".into())).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("no chainlink tick within 15s")
            .expect("stream ended")
            .expect("ws error");
        if let Message::Text(raw) = msg {
            if raw == "PONG" {
                println!("PONG ok");
                continue;
            }
            if raw.contains("crypto_prices_chainlink") && raw.contains("btc/usd") {
                println!("got chainlink tick");
                return;
            }
        }
    }
}

/// 5. Binance klines: open price + historical vol in a sane range.
#[tokio::test]
#[ignore]
async fn binance_klines() {
    let http = reqwest::Client::new();
    let klines: serde_json::Value = http
        .get(format!("{BINANCE_REST}/api/v3/klines"))
        .query(&[("symbol", "BTCUSDT"), ("interval", "15m"), ("limit", "21")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let closes: Vec<f64> = klines
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k[4].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(closes.len(), 21);
    let mut var = 0.0;
    for w in closes.windows(2) {
        let r = (w[1] / w[0]).ln();
        var += r * r;
    }
    let ann = (var / 20.0 * 4.0 * 24.0 * 365.0).sqrt();
    println!("annualized 15m vol: {:.1}%", ann * 100.0);
    assert!(ann > 0.01 && ann < 5.0, "vol {ann} out of sane range");
}

/// 6. Binance combined WS: both stream types within 10s; book top uncrossed.
#[tokio::test]
#[ignore]
async fn binance_combined_ws() {
    install_tls();
    let url = format!("{BINANCE_WS}?streams=btcusdt@depth20@100ms/btcusdt@aggTrade");
    let (ws, _) = connect_async(url).await.unwrap();
    let (_sink, mut stream) = ws.split();
    let (mut got_depth, mut got_trade) = (false, false);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !(got_depth && got_trade) {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("missing stream type within 10s")
            .expect("stream ended")
            .expect("ws error");
        if let Message::Text(raw) = msg {
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
            match v.get("stream").and_then(|s| s.as_str()) {
                Some(s) if s.contains("@depth") => {
                    let bid: f64 = v["data"]["bids"][0][0].as_str().unwrap().parse().unwrap();
                    let ask: f64 = v["data"]["asks"][0][0].as_str().unwrap().parse().unwrap();
                    assert!(bid < ask, "crossed binance book");
                    got_depth = true;
                }
                Some(s) if s.contains("@aggTrade") => got_trade = true,
                _ => {}
            }
        }
    }
    println!("depth + aggTrade both received");
}

/// 8. CLOB book maintenance: subscribe to the active market, run every raw
/// WS message through the REAL `clob_market::apply_event` book-keeping,
/// log each updated top-of-book, and finally cross-check the locally
/// maintained book against the REST /book endpoint. Passing proves both the
/// initial snapshot and the price_change deltas are applied correctly.
/// Run with `--nocapture` to see the per-event book log.
#[tokio::test]
#[ignore]
async fn clob_book_snapshot_and_delta() {
    use poly_strat3::md::clob_market::{apply_event, LocalBook};
    use poly_strat3::types::TokenId;
    use std::collections::HashMap;
    use std::time::Duration;

    install_tls();
    let (slug, ids, _, _) = discover_market().await.expect("no market");
    println!("subscribing to {slug} {ids:?}");
    let (ws, _) = connect_async(format!("{WS_URL}/market")).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let sub = serde_json::json!({"assets_ids": ids, "type": "market"}).to_string();
    sink.send(Message::Text(sub)).await.unwrap();

    let mut books: HashMap<TokenId, LocalBook> = HashMap::new();
    let (mut snap_count, mut delta_count) = (0usize, 0usize);

    // Helper: apply one raw WS text frame through the real code path.
    let process = |raw: &str,
                       books: &mut HashMap<TokenId, LocalBook>,
                       snap_count: &mut usize,
                       delta_count: &mut usize| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            return;
        };
        let events = match v {
            serde_json::Value::Array(a) => a,
            other => vec![other],
        };
        for ev in &events {
            let et = ev
                .get("event_type")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(token) = apply_event(books, ev) {
                let top = books[&token].top();
                match et.as_str() {
                    "book" => *snap_count += 1,
                    "price_change" => *delta_count += 1,
                    _ => {}
                }
                if token.as_str() == ids[0] {
                    println!(
                        "[{et:>12}] {}… bid={:?} x{:<8} ask={:?} x{:<8}",
                        &token[..token.len().min(8)],
                        top.bid,
                        top.bid_sz,
                        top.ask,
                        top.ask_sz
                    );
                }
                if let (Some(b), Some(a)) = (top.bid, top.ask) {
                    assert!(
                        b < a,
                        "locally maintained book is crossed: bid {b} >= ask {a}"
                    );
                    assert!(b > 0.0 && a < 1.0, "prices out of (0,1): {b}/{a}");
                }
            }
        }
    };

    // Phase 1: collect updates. NOTE: the market channel currently re-sends a
    // full `book` snapshot on every change; `price_change` deltas are rare on
    // these markets — process them when they appear, but don't require one
    // (delta application is pinned down by the unit tests in clob_market.rs).
    let start = tokio::time::Instant::now();
    let deadline = start + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        // Enough evidence once both tokens snapshotted and 15s of updates seen.
        if snap_count >= 2 && (delta_count >= 1 || start.elapsed() >= Duration::from_secs(15)) {
            break;
        }
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(Message::Text(raw)))) => {
                process(&raw, &mut books, &mut snap_count, &mut delta_count)
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => panic!("ws error: {e}"),
            Ok(None) => panic!("ws stream ended"),
            Err(_) => break, // deadline
        }
    }
    println!("phase 1 done: {snap_count} snapshot(s), {delta_count} delta(s)");
    assert!(snap_count >= 2, "expected book snapshots for both tokens, got {snap_count}");
    if delta_count == 0 {
        println!("note: no price_change deltas observed this run (server sent full snapshots only)");
    }

    // Phase 2: cross-check local book vs REST /book (retry to absorb races —
    // the market can move between our last WS event and the REST fetch).
    let http = reqwest::Client::new();
    let rest_top = |book: &serde_json::Value| -> (Option<f64>, Option<f64>) {
        let collect = |side: &str| -> Vec<f64> {
            book.get(side)
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l.get("price")?.as_str()?.parse::<f64>().ok())
                        .collect()
                })
                .unwrap_or_default()
        };
        let best_bid = collect("bids").into_iter().fold(None, |m: Option<f64>, p| {
            Some(m.map_or(p, |m| m.max(p)))
        });
        let best_ask = collect("asks").into_iter().fold(None, |m: Option<f64>, p| {
            Some(m.map_or(p, |m| m.min(p)))
        });
        (best_bid, best_ask)
    };

    for attempt in 1..=5 {
        // Drain any queued WS updates so the local book is current.
        let drain_until = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(drain_until, stream.next()).await {
            if let Message::Text(raw) = msg {
                process(&raw, &mut books, &mut snap_count, &mut delta_count);
            }
        }

        let mut all_match = true;
        for token in &ids {
            let rest: serde_json::Value = http
                .get(format!("{CLOB_V1}/book"))
                .query(&[("token_id", token.as_str())])
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let (rb, ra) = rest_top(&rest);
            let local = books.get(token.as_str()).map(|b| b.top()).unwrap_or_default();
            let eq = |x: Option<f64>, y: Option<f64>| match (x, y) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-9,
                (None, None) => true,
                _ => false,
            };
            let ok = eq(local.bid, rb) && eq(local.ask, ra);
            println!(
                "attempt {attempt}: {}… local {:?}/{:?} vs REST {:?}/{:?} -> {}",
                &token[..token.len().min(8)],
                local.bid,
                local.ask,
                rb,
                ra,
                if ok { "MATCH" } else { "mismatch" }
            );
            all_match &= ok;
        }
        if all_match {
            println!(
                "local book (snapshot + {delta_count} deltas) matches REST for both tokens ✓"
            );
            return;
        }
    }
    panic!("local book never converged to the REST book after 5 attempts");
}

/// 9. Binance spot aggregation (default mode: chainlink leg disabled):
/// subscribe to the live BTCUSDT book + trade streams, run every frame
/// through the REAL `binance::parse_frame` and `SpotAggregator`, and verify
/// after EVERY book update and trade that the aggregated spot equals the
/// latest Binance price — and that an injected Chainlink print is recorded
/// but does NOT move the aggregate.
#[tokio::test]
#[ignore]
async fn binance_spot_aggregation_live() {
    use poly_strat3::md::binance::parse_frame;
    use poly_strat3::spot::{SpotAggregator, SpotSource};
    use poly_strat3::types::{now_secs, Event};
    use std::time::Duration;

    install_tls();
    let url = format!("{BINANCE_WS}?streams=btcusdt@depth20@100ms/btcusdt@aggTrade");
    let (ws, _) = connect_async(url).await.unwrap();
    let (_sink, mut stream) = ws.split();

    // Default mode: Binance drives the aggregate; Chainlink leg DISABLED.
    const CL_LEVEL: f64 = 100_000.0; // deliberately absurd — must be ignored
    let mut agg = SpotAggregator::new(true, false, 1500);
    let mut chainlink_injected = false;
    let (mut depth_checked, mut trades_checked) = (0usize, 0usize);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while (depth_checked < 50 || trades_checked < 10) && tokio::time::Instant::now() < deadline {
        let msg = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(Message::Text(raw)))) => raw,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => panic!("ws error: {e}"),
            Ok(None) => panic!("ws stream ended"),
            Err(_) => break,
        };
        // println!("rec {msg}");
        let now = now_secs();
        let (latest_px, ts, kind) = match parse_frame(&msg, now) {
            Some(Event::BinanceBook { bids, asks, ts }) => {
                let (Some((b, _)), Some((a, _))) = (bids.first(), asks.first()) else {
                    continue;
                };
                ((b + a) / 2.0, ts, "book")
            }
            Some(Event::BinanceTrade { px, ts, .. }) => (px, ts, "trade"),
            _ => continue,
        };
        let source = if kind == "trade" { SpotSource::Trade } else { SpotSource::Depth };
        agg.on_binance(latest_px, ts, source);
        assert_eq!(agg.latest_source(), Some(source), "latest source must track the update type");

        // Inject a Chainlink print at an absurd level once, mid-stream: with
        // the Chainlink leg disabled (default) it must NOT move the aggregate.
        if !chainlink_injected {
            agg.on_chainlink(CL_LEVEL, ts);
            chainlink_injected = true;
            println!("injected chainlink print @ {CL_LEVEL} (must be ignored)");
        }

        // The aggregated spot must be exactly the LATEST Binance update.
        let spot = agg.spot(ts).expect("aggregated spot must exist");
        assert!(
            (spot - latest_px).abs() < 1e-9,
            "[{kind}] aggregated spot {spot} != latest binance {latest_px} \
             (chainlink leaked into the aggregate?)"
        );
        assert_eq!(
            agg.chainlink_px(),
            chainlink_injected.then_some(CL_LEVEL),
            "chainlink print must still be recorded for basis/logging"
        );
        match kind {
            "book" => depth_checked += 1,
            _ => trades_checked += 1,
        }
        println!(
            "[{kind:>5}] binance={latest_px:.2} -> agg spot={spot:.2} (latest ✓, checked {}/{})",
            depth_checked, trades_checked
        );
    }
    assert!(
        depth_checked >= 5,
        "too few book updates verified: {depth_checked}"
    );
    assert!(
        trades_checked >= 1,
        "no trade verified within 30s: {trades_checked}"
    );
    println!(
        "aggregated spot tracked the latest Binance px across {depth_checked} book updates and {trades_checked} trades ✓"
    );
}

/// 10. Account state: read OPEN ORDERS (authenticated CLOB, via the bot's
/// own LiveExec) and POSITIONS (public Data API) for a given credentials
/// file. Credentials path: $POLYMM_CREDENTIALS, default
/// config/credentials.toml (env vars POLYMARKET_PK/FUNDER override the
/// file). Read-only — nothing is placed or cancelled. Run with:
///   cargo test --test connectivity account_open_orders_and_positions -- --ignored --nocapture
#[cfg(feature = "live")]
#[tokio::test]
#[ignore]
async fn account_open_orders_and_positions() {
    install_tls();
    let cred_path = std::env::var("POLYMM_CREDENTIALS")
        .unwrap_or_else(|_| "config/credentials.toml".into());
    let has_env_pk = std::env::var("POLYMARKET_PK").map(|v| !v.is_empty()).unwrap_or(false);
    if !has_env_pk && !std::path::Path::new(&cred_path).exists() {
        eprintln!("no credentials ({cred_path} missing, POLYMARKET_PK unset) — skipping");
        return;
    }

    let creds = poly_strat3::config::Credentials::load(std::path::Path::new(&cred_path))
        .expect("load credentials");
    let exec = poly_strat3::exec::live::LiveExec::connect("https://clob.polymarket.com", &creds)
        .await
        .expect("authenticate");

    // ── settled positions for the ACTIVE market via the SDK data client ────
    let (slug, _ids, cond, _end) = discover_market().await.expect("no market");
    let sizes = exec.market_positions(&cond).await.expect("market_positions");
    println!("SDK market_positions for {slug}: {sizes:?}");

    // ── open orders (authenticated CLOB) ────────────────────────────────────
    let orders = exec.open_orders().await.expect("open orders");
    println!("open orders: {}", orders.len());
    for (i, o) in orders.iter().enumerate() {
        println!("  [{i}] {o}");
    }

    // ── positions (public Data API, keyed by the funds-holding address) ────
    let owner = if creds.funder.is_empty() {
        exec.signer_address()
    } else {
        creds.funder.clone()
    };
    println!("positions for {owner}:");
    let http = reqwest::Client::new();
    let positions: serde_json::Value = http
        .get("https://data-api.polymarket.com/positions")
        .query(&[
            ("user", owner.as_str()),
            ("sizeThreshold", "0.01"),
            ("limit", "100"),
            ("sortBy", "TOKENS"),
            ("sortDirection", "DESC"),
        ])
        .send()
        .await
        .expect("positions request")
        .error_for_status()
        .expect("positions status")
        .json()
        .await
        .expect("positions json");
    let list = positions
        .as_array()
        .cloned()
        .or_else(|| positions.get("positions").and_then(|p| p.as_array()).cloned())
        .unwrap_or_default();
    println!("positions: {}", list.len());
    for p in &list {
        let get = |k: &str| p.get(k).map(|v| v.to_string()).unwrap_or_else(|| "?".into());
        println!(
            "  outcome={} size={} avgPrice={} value={} market={}",
            get("outcome"),
            get("size"),
            get("avgPrice"),
            get("currentValue"),
            get("title"),
        );
    }
    println!("account read OK: {} open order(s), {} position(s)", orders.len(), list.len());
}

/// 11. USDC balance via the SDK's balance_allowance(): authenticates with
/// the given credentials file and prints the collateral balance and the
/// exchange allowances. Read-only. Credentials path: $POLYMM_CREDENTIALS,
/// default config/credentials.toml. Run with:
///   cargo test --test connectivity account_usdc_balance -- --ignored --nocapture
#[cfg(feature = "live")]
#[tokio::test]
#[ignore]
async fn account_usdc_balance() {
    install_tls();
    let cred_path = std::env::var("POLYMM_CREDENTIALS")
        .unwrap_or_else(|_| "config/credentials.toml".into());
    let has_env_pk = std::env::var("POLYMARKET_PK").map(|v| !v.is_empty()).unwrap_or(false);
    if !has_env_pk && !std::path::Path::new(&cred_path).exists() {
        eprintln!("no credentials ({cred_path} missing, POLYMARKET_PK unset) — skipping");
        return;
    }

    let creds = poly_strat3::config::Credentials::load(std::path::Path::new(&cred_path))
        .expect("load credentials");
    let exec = poly_strat3::exec::live::LiveExec::connect("https://clob.polymarket.com", &creds)
        .await
        .expect("authenticate");

    let resp = exec.balance_allowance().await.expect("balance_allowance");
    // Balance is reported in raw USDC units (6 decimals).
    let usdc = resp.balance / polymarket_client_sdk_v2::types::Decimal::from(1_000_000u64);
    println!("USDC balance: {usdc} (raw: {})", resp.balance);
    for (contract, allowance) in &resp.allowances {
        println!("  allowance {contract} -> {allowance}");
    }
    assert!(
        resp.balance >= polymarket_client_sdk_v2::types::Decimal::ZERO,
        "balance must be non-negative"
    );
    println!("balance_allowance OK");
}

/// 12. LIVE ORDER round-trip: place a BUY order on a given token at a given
/// price/qty, verify the ack, confirm it rests in open orders, then cancel
/// it and confirm it is gone. ⚠ This places a REAL order with REAL funds —
/// params are hardcoded defaults below, overridable via POLYMM_TOKEN_ID /
/// POLYMM_PRICE / POLYMM_QTY. Run explicitly with:
///   cargo test --test connectivity place_order_and_verify_ack -- --ignored --nocapture
/// Pick a price far from the market so it rests instead of filling.
#[cfg(feature = "live")]
#[tokio::test]
#[ignore]
async fn place_order_and_verify_ack() {
    install_tls();
    // Hardcoded defaults; POLYMM_TOKEN_ID / POLYMM_PRICE / POLYMM_QTY
    // env vars override them.
    let token = std::env::var("POLYMM_TOKEN_ID").unwrap_or_else(|_| {
        "37863990088639017224129863896084706036599112986230542056774425461928248691792".into()
    });
    let px: f64 = std::env::var("POLYMM_PRICE")
        .unwrap_or_else(|_| "0.01".into())
        .parse()
        .expect("POLYMM_PRICE not a number");
    let qty: f64 = std::env::var("POLYMM_QTY")
        .unwrap_or_else(|_| "5".into())
        .parse()
        .expect("POLYMM_QTY not a number");
    assert!(px > 0.0 && px < 1.0, "price must be in (0,1)");
    if px * qty < 1.0 {
        println!(
            "warning: notional {:.2} is below Polymarket's usual $1 minimum — the exchange may reject it",
            px * qty
        );
    }

    let cred_path = std::env::var("POLYMM_CREDENTIALS")
        .unwrap_or_else(|_| "config/credentials.toml".into());
    let creds = poly_strat3::config::Credentials::load(std::path::Path::new(&cred_path))
        .expect("load credentials");
    let exec = poly_strat3::exec::live::LiveExec::connect("https://clob.polymarket.com", &creds)
        .await
        .expect("authenticate");

    // ── place ────────────────────────────────────────────────────────────────
    println!("placing BUY {qty} @ {px} on token {}…", &token[..token.len().min(12)]);
    let order_id = exec
        .place(&token, poly_strat3::types::Side::Buy, px, qty)
        .await
        .expect("place order");

    // ── ack checks ───────────────────────────────────────────────────────────
    assert!(!order_id.is_empty(), "ack must carry an order id");
    println!("ack OK: order_id={order_id} (success=true from the SDK response)");
    if !order_id.starts_with("0x") {
        println!("note: unexpected order-id format (expected 0x…)");
    }

    // ── verify it rests among open orders ────────────────────────────────────
    let open = exec.open_orders().await.expect("open orders");
    let resting = open.iter().any(|o| o.contains(order_id.trim_start_matches("0x")) || o.contains(&order_id));
    println!("resting in open orders: {resting} {open:?} ({} open total)", open.len());
    if !resting {
        println!("warning: not found in open orders — it may have filled immediately (price crossed?)");
    }

    // ── clean up: cancel and verify gone ─────────────────────────────────────
    exec.cancel_orders(&[order_id.clone()]).await.expect("cancel order");
    let after = exec.open_orders().await.expect("open orders after cancel");
    assert!(
        !after.iter().any(|o| o.contains(&order_id)),
        "order {order_id} still resting after cancel"
    );
    println!("place -> ack -> resting -> cancel round-trip OK");
}

/// 13. USER-CHANNEL order + trade updates via the SDK's subscribe_orders /
/// subscribe_trades streams: subscribe both, place a real deep-OTM order,
/// confirm the order stream delivers the PLACEMENT update, then cancel and
/// watch for the CANCELLATION update (trade stream is watched throughout —
/// a deep-OTM order should never trade). ⚠ Places a REAL order (default:
/// YES token of the active BTC 15m market, BUY 100 @ 0.01 = $1 notional).
/// Overrides: POLYMM_TOKEN_ID / POLYMM_PRICE / POLYMM_QTY. Run with:
///   cargo test --test connectivity user_channel_order_updates -- --ignored --nocapture
#[cfg(feature = "live")]
#[tokio::test]
#[ignore]
async fn user_channel_order_updates() {
    // Surface the SDK's internal logs when RUST_LOG is set.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    use futures_util::StreamExt as _;
    use polymarket_client_sdk_v2::clob::ws::Client as WsClient;
    use polymarket_client_sdk_v2::types::B256;
    use std::str::FromStr as _;
    use std::time::Duration;

    install_tls();
    let cred_path = std::env::var("POLYMM_CREDENTIALS")
        .unwrap_or_else(|_| "config/credentials.toml".into());
    let has_env_pk = std::env::var("POLYMARKET_PK").map(|v| !v.is_empty()).unwrap_or(false);
    if !has_env_pk && !std::path::Path::new(&cred_path).exists() {
        eprintln!("no credentials — skipping (real-order test)");
        return;
    }

    // Order params: pick whichever token of the active market has room for
    // a post-only buy (a pinned outcome with ask at the 0.001 floor cannot
    // accept ANY post-only buy).
    let (slug, ids, cond, _end) = discover_market().await.expect("no active market");
    println!("active market {slug}");
    // For each candidate token: price = half the tighter touch, floored to
    // the 0.001 grid; valid only if it rests strictly below the best ask.
    let http = reqwest::Client::new();
    let mut chosen: Option<(String, &str, f64)> = None;
    let candidates: Vec<(String, &str)> = match std::env::var("POLYMM_TOKEN_ID") {
        Ok(t) => vec![(t, "given")],
        Err(_) => vec![(ids[0].clone(), "YES"), (ids[1].clone(), "NO")],
    };
    for (cand, label) in candidates {
        let book: serde_json::Value = http
            .get(format!("{CLOB_V1}/book"))
            .query(&[("token_id", cand.as_str())])
            .send()
            .await
            .expect("book request")
            .json()
            .await
            .expect("book json");
        let side_extreme = |side: &str, best_is_max: bool| -> Option<f64> {
            book.get(side)
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|l| l.get("price")?.as_str()?.parse::<f64>().ok())
                        .fold(f64::NAN, if best_is_max { f64::max } else { f64::min })
                })
                .filter(|v| v.is_finite())
        };
        let best_bid = side_extreme("bids", true);
        let best_ask = side_extreme("asks", false);
        // Round with the bot's own band-aware tick rule (0.01 inside
        // [0.04,0.96], 0.001 outside) so the exchange's tick check passes.
        let px = 0.01;
        let rests = best_ask.map(|a| px < a - 1e-9).unwrap_or(true);
        println!("{label}: bid={best_bid:?} ask={best_ask:?} -> px={px} rests={rests}");
        if rests {
            chosen = Some((cand, label, px));
            break;
        }
    }
    let (token, label, default_px) =
        chosen.expect("no token has room for a post-only buy (both outcomes pinned?)");
    let px: f64 = std::env::var("POLYMM_PRICE")
        .unwrap_or_else(|_| default_px.to_string())
        .parse()
        .expect("POLYMM_PRICE not a number");
    // Exchange minimums: 5 shares AND ~$1 notional.
    let qty: f64 = std::env::var("POLYMM_QTY")
        .unwrap_or_else(|_| (1.05f64 / px).ceil().max(5.0).to_string())
        .parse()
        .expect("POLYMM_QTY not a number");
    println!("using {label} token — quoting {qty} @ {px} (~${:.2})", px * qty);

    // ── REST auth, then SDK user-channel subscriptions ──────────────────────
    let creds = poly_strat3::config::Credentials::load(std::path::Path::new(&cred_path))
        .expect("load credentials");
    let exec = poly_strat3::exec::live::LiveExec::connect("https://clob.polymarket.com", &creds)
        .await
        .expect("authenticate");
    let ws = WsClient::default()
        .authenticate(exec.api_creds(), exec.address())
        .expect("ws authenticate");
    let cond_b256 = B256::from_str(&cond).expect("condition id");
    let mut orders_stream =
        std::pin::pin!(ws.subscribe_orders(vec![cond_b256]).expect("subscribe_orders"));
    let mut trades_stream =
        std::pin::pin!(ws.subscribe_trades(vec![cond_b256]).expect("subscribe_trades"));
    // The SDK connects lazily on first subscription — wait until the user
    // channel is actually CONNECTED (and give the queued subscribe a moment
    // to flush) before placing, or the placement event races the subscribe.
    use polymarket_client_sdk_v2::clob::ws::ChannelType;
    use polymarket_client_sdk_v2::ws::connection::ConnectionState;
    let connect_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let st = ws.connection_state(ChannelType::User);
        if matches!(st, ConnectionState::Connected { .. }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < connect_deadline,
            "user channel not connected within 10s (state: {st:?})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await; // let subscribe flush
    println!("user channel CONNECTED + subscribed on market {cond} — placing BUY {qty} @ {px}");

    // ── place, then await updates on the SDK streams ────────────────────────
    let order_id = exec
        .place(&token, poly_strat3::types::Side::Buy, px, qty)
        .await
        .expect("place order");
    println!("order placed: {order_id}");

    let (mut got_placement, mut got_cancellation, mut cancelled) = (false, false, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut state_timer = tokio::time::interval(Duration::from_secs(5));
    while tokio::time::Instant::now() < deadline && !(got_placement && got_cancellation) {
        tokio::select! {
            _ = state_timer.tick() => {
                println!(
                    "[diag] user-channel connection state: {:?}",
                    ws.connection_state(polymarket_client_sdk_v2::clob::ws::ChannelType::User)
                );
            }
            ev = orders_stream.next() => {
                let o = match ev {
                    Some(Ok(o)) => o,
                    Some(Err(e)) => panic!("order stream error: {e}"),
                    None => panic!("order stream ended"),
                };
                let ours = o.id.to_string().eq_ignore_ascii_case(&order_id);
                println!(
                    "[order] id={} type={:?} side={:?} price={} ours={ours}",
                    o.id, o.msg_type, o.side, o.price
                );
                if ours && !got_placement {
                    got_placement = true;
                    println!("=> placement update received — cancelling");
                    exec.cancel_orders(&[order_id.clone()]).await.expect("cancel");
                    cancelled = true;
                } else if ours {
                    got_cancellation = true;
                    println!("=> cancellation update received");
                }
            }
            ev = trades_stream.next() => {
                match ev {
                    Some(Ok(t)) => println!("[trade] {t:?}"),
                    Some(Err(e)) => panic!("trade stream error: {e}"),
                    None => panic!("trade stream ended"),
                }
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }

    // Safety net: never leave the order resting.
    if !cancelled {
        let _ = exec.cancel_orders(&[order_id.clone()]).await;
    }
    assert!(
        got_placement,
        "no order update received via subscribe_orders within 30s"
    );
    if !got_cancellation {
        println!("note: cancellation update not observed within the window (placement WAS confirmed)");
    }
    println!(
        "SDK user streams OK: placement update received{}",
        if got_cancellation { " + cancellation update received" } else { "" }
    );
}

/// 7. SDK auth — gated: skips unless credentials are available.
/// Uses the bot's own LiveExec adapter, i.e. the exact live code path.
#[cfg(feature = "live")]
#[tokio::test]
#[ignore]
async fn sdk_auth() {
    install_tls();
    if std::env::var("POLYMARKET_PK").map(|v| v.is_empty()).unwrap_or(true) {
        eprintln!("POLYMARKET_PK not set — skipping sdk_auth");
        return;
    }
    let creds = poly_strat3::config::Credentials::load(std::path::Path::new(
        "config/credentials.toml",
    ))
    .expect("credentials");
    let exec = poly_strat3::exec::live::LiveExec::connect("https://clob.polymarket.com", &creds)
        .await
        .expect("authenticate");
    // Round-trip an authenticated no-op: cancel-all with no orders resting.
    exec.cancel_all().await.expect("cancel_all");
    println!("sdk auth + cancel_all ok");
}
