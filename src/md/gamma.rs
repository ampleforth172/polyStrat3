//! Market discovery via the Gamma API plus Binance kline helpers for the
//! window open price and historical volatility bootstrap.

use serde_json::Value;

use crate::types::MarketInfo;

/// Candidate expiry timestamps: current boundary + next `lookahead-1` windows.
pub fn candidate_expiries(now_secs: i64, interval_secs: i64, lookahead: usize) -> Vec<i64> {
    let boundary = (now_secs / interval_secs) * interval_secs;
    (0..lookahead as i64)
        .map(|i| boundary + i * interval_secs)
        .collect()
}

pub fn build_slug(symbol: &str, expiry_ts: i64) -> String {
    format!("{}-updown-15m-{}", symbol.to_lowercase(), expiry_ts)
}

/// Parse a Gamma /events response entry into MarketInfo.
/// Token order follows the Python bot: clobTokenIds[0] = YES, [1] = NO.
pub fn parse_event(event: &Value, slug: &str) -> Option<MarketInfo> {
    if event.get("slug")?.as_str()? != slug {
        return None;
    }
    if !event.get("active").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let end_date_iso = event.get("endDate")?.as_str()?.to_string();
    let end_ts = chrono::DateTime::parse_from_rfc3339(&end_date_iso)
        .ok()?
        .timestamp() as f64;
    let market = event.get("markets")?.as_array()?.first()?;
    let condition_id = market.get("conditionId")?.as_str()?.to_string();
    let ids_raw = market.get("clobTokenIds")?;
    let ids: Vec<String> = match ids_raw {
        Value::String(s) => serde_json::from_str(s).ok()?,
        Value::Array(_) => serde_json::from_value(ids_raw.clone()).ok()?,
        _ => return None,
    };
    if ids.len() < 2 {
        return None;
    }
    Some(MarketInfo {
        slug: slug.to_string(),
        condition_id,
        token_yes: ids[0].as_str().into(),
        token_no: ids[1].as_str().into(),
        end_date_iso,
        end_ts,
    })
}

pub async fn fetch_event_by_slug(
    http: &reqwest::Client,
    gamma_host: &str,
    slug: &str,
) -> Result<Option<MarketInfo>, String> {
    let url = format!("{gamma_host}/events");
    let resp = http
        .get(&url)
        .query(&[("slug", slug)])
        .send()
        .await
        .map_err(|e| format!("gamma request: {e}"))?
        .error_for_status()
        .map_err(|e| format!("gamma status: {e}"))?;
    let data: Value = resp.json().await.map_err(|e| format!("gamma json: {e}"))?;
    let events: Vec<Value> = match data {
        Value::Array(v) => v,
        v @ Value::Object(_) => vec![v],
        _ => vec![],
    };
    Ok(events.iter().find_map(|e| parse_event(e, slug)))
}

/// Probe candidate slugs and return the first active market.
pub async fn find_active_market(
    http: &reqwest::Client,
    gamma_host: &str,
    symbol: &str,
    interval_secs: i64,
) -> Result<Option<MarketInfo>, String> {
    let now = crate::types::now_secs() as i64;
    for ts in candidate_expiries(now, interval_secs, 4) {
        let slug = build_slug(symbol, ts);
        tracing::debug!("probing {slug}");
        if let Some(mi) = fetch_event_by_slug(http, gamma_host, &slug).await? {
            // Skip already-expired candidates.
            if mi.end_ts > crate::types::now_secs() {
                return Ok(Some(mi));
            }
        }
    }
    Ok(None)
}

/// Open price of the 1m kline at `open_ts_secs` from Binance.
pub async fn fetch_open_price(
    http: &reqwest::Client,
    binance_rest: &str,
    symbol: &str,
    open_ts_secs: i64,
) -> Result<Option<f64>, String> {
    let url = format!("{binance_rest}/api/v3/klines");
    let resp = http
        .get(&url)
        .query(&[
            ("symbol", format!("{}USDT", symbol.to_uppercase()).as_str()),
            ("interval", "1m"),
            ("startTime", (open_ts_secs * 1000).to_string().as_str()),
            ("limit", "1"),
        ])
        .send()
        .await
        .map_err(|e| format!("binance klines: {e}"))?
        .error_for_status()
        .map_err(|e| format!("binance status: {e}"))?;
    let klines: Value = resp.json().await.map_err(|e| format!("binance json: {e}"))?;
    Ok(klines
        .as_array()
        .and_then(|a| a.first())
        .and_then(|k| k.get(1))
        .and_then(|open| open.as_str())
        .and_then(|s| s.parse::<f64>().ok()))
}

/// Annualized realized vol from the last `n_bars` 15m klines.
pub async fn fetch_historical_vol(
    http: &reqwest::Client,
    binance_rest: &str,
    symbol: &str,
    n_bars: usize,
) -> Result<Option<f64>, String> {
    let url = format!("{binance_rest}/api/v3/klines");
    let resp = http
        .get(&url)
        .query(&[
            ("symbol", format!("{}USDT", symbol.to_uppercase()).as_str()),
            ("interval", "15m"),
            ("limit", (n_bars + 1).to_string().as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("binance klines: {e}"))?
        .error_for_status()
        .map_err(|e| format!("binance status: {e}"))?;
    let klines: Value = resp.json().await.map_err(|e| format!("binance json: {e}"))?;
    let closes: Vec<f64> = klines
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|k| k.get(4)?.as_str()?.parse::<f64>().ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(annualized_vol_from_15m_closes(&closes))
}

pub fn annualized_vol_from_15m_closes(closes: &[f64]) -> Option<f64> {
    if closes.len() < 2 {
        return None;
    }
    let mut var_sum = 0.0;
    for w in closes.windows(2) {
        let r = (w[1] / w[0]).ln();
        var_sum += r * r;
    }
    let var = var_sum / (closes.len() - 1) as f64;
    Some((var * 4.0 * 24.0 * 365.0).sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn candidate_expiries_aligned() {
        let c = candidate_expiries(1_741_824_100, 900, 4);
        assert_eq!(c[0], 1_741_824_000);
        assert_eq!(c[1] - c[0], 900);
        assert_eq!(c.len(), 4);
        assert!(c.iter().all(|t| t % 900 == 0));
    }

    #[test]
    fn slug_format_matches_python() {
        assert_eq!(build_slug("BTC", 1_741_824_000), "btc-updown-15m-1741824000");
        assert_eq!(build_slug("eth", 1), "eth-updown-15m-1");
    }

    #[test]
    fn parse_event_happy_path_with_string_token_ids() {
        let ev = json!({
            "slug": "btc-updown-15m-1",
            "active": true,
            "endDate": "2026-07-11T08:15:00Z",
            "markets": [{
                "conditionId": "0xcond",
                "clobTokenIds": "[\"111\", \"222\"]"
            }]
        });
        let mi = parse_event(&ev, "btc-updown-15m-1").unwrap();
        assert_eq!(mi.condition_id, "0xcond");
        assert_eq!(mi.token_yes, "111");
        assert_eq!(mi.token_no, "222");
        assert!(mi.end_ts > 0.0);
    }

    #[test]
    fn parse_event_rejects_inactive_or_wrong_slug() {
        let ev = json!({
            "slug": "btc-updown-15m-1",
            "active": false,
            "endDate": "2026-07-11T08:15:00Z",
            "markets": [{"conditionId": "0xc", "clobTokenIds": "[\"1\",\"2\"]"}]
        });
        assert!(parse_event(&ev, "btc-updown-15m-1").is_none());
        let ev2 = json!({
            "slug": "other",
            "active": true,
            "endDate": "2026-07-11T08:15:00Z",
            "markets": [{"conditionId": "0xc", "clobTokenIds": "[\"1\",\"2\"]"}]
        });
        assert!(parse_event(&ev2, "btc-updown-15m-1").is_none());
    }

    #[test]
    fn vol_from_synthetic_closes() {
        // Constant 0.1% per 15m bar.
        let mut closes = vec![100_000.0];
        for _ in 0..20 {
            closes.push(closes.last().unwrap() * 1.001);
        }
        let v = annualized_vol_from_15m_closes(&closes).unwrap();
        // Per-bar log-return is ln(1.001), not 0.001.
        let expected = 1.001f64.ln() * (4.0f64 * 24.0 * 365.0).sqrt();
        assert!((v - expected).abs() / expected < 1e-6, "got {v}, want {expected}");
        assert!(annualized_vol_from_15m_closes(&[1.0]).is_none());
    }
}
