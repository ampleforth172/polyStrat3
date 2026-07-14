//! End-of-window PnL summary, one line per market — same fields as the
//! Python bot's `{symbol}_trader_pnl.txt`.

use std::io::Write;
use std::path::PathBuf;

use crate::position::Position;

pub struct WindowSummary<'a> {
    pub slug: &'a str,
    pub end_date_iso: &'a str,
    pub symbol: &'a str,
    pub spot_open: Option<f64>,
    pub spot_close: Option<f64>,
    /// (label, position) pairs, e.g. [("YES", &pos_yes), ("NO", &pos_no)].
    pub sides: Vec<(&'a str, &'a Position)>,
}

pub fn format_line(now_local: &str, s: &WindowSummary) -> String {
    let mut total_realized = 0.0;
    let mut total_unrealized = 0.0;
    let mut total_fees = 0.0;
    let mut parts = Vec::new();
    for (label, p) in &s.sides {
        let unreal = if p.position > 0.0 {
            (p.settlement_price() - p.avg_entry) * p.position
        } else {
            0.0
        };
        total_realized += p.realized_pnl;
        total_unrealized += unreal;
        total_fees += p.total_fees;
        parts.push(format!(
            "{label}: pos={:.2}  buys={}  fees={:.6}  rPnL={:+.4}  uPnL={:+.4}",
            p.position, p.buy_count, p.total_fees, p.realized_pnl, unreal
        ));
    }
    let total_pnl = total_realized + total_unrealized;
    let open_s = s.spot_open.map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into());
    let close_s = s.spot_close.map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into());
    format!(
        "{now_local} | {slug} | end={end} | {sym}_open={open_s} | {sym}_close={close_s} | {parts} | total_rPnL={tr:+.4} | total_uPnL={tu:+.4} | total_fees={tf:.6} | total_PnL={tp:+.4} | total_netPnL={tn:+.4}\n",
        slug = s.slug,
        end = s.end_date_iso,
        sym = s.symbol,
        parts = parts.join(" | "),
        tr = total_realized,
        tu = total_unrealized,
        tf = total_fees,
        tp = total_pnl,
        tn = total_pnl - total_fees,
    )
}

pub fn append(dir: &str, symbol: &str, line: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create output dir {dir}: {e}"))?;
    let path = PathBuf::from(dir).join(format!("{}_trader_pnl.txt", symbol.to_lowercase()));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open pnl log {}: {e}", path.display()))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("write pnl log: {e}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Side;

    #[test]
    fn line_contains_all_totals() {
        let mut yes = Position::default();
        yes.on_fill(Side::Buy, 5.0, 0.9);
        yes.on_fill(Side::Sell, 5.0, 0.93);
        yes.add_fee(0.9, 5.0, true);
        let mut no = Position::default();
        no.on_fill(Side::Buy, 2.0, 0.4);
        no.last_bid = 0.8; // settles to 1.0
        let s = WindowSummary {
            slug: "btc-updown-15m-1",
            end_date_iso: "2026-07-11T08:15:00Z",
            symbol: "BTC",
            spot_open: Some(118_000.0),
            spot_close: Some(118_100.0),
            sides: vec![("YES", &yes), ("NO", &no)],
        };
        let line = format_line("2026-07-11 08:15:01", &s);
        assert!(line.contains("btc-updown-15m-1"));
        assert!(line.contains("BTC_open=118000.00"));
        assert!(line.contains("YES: pos=0.00"));
        // NO settles at 1.0: uPnL = (1.0 - 0.4) * 2 = +1.2
        assert!(line.contains("uPnL=+1.2000"), "{line}");
        // total_rPnL = (0.93-0.90)*5 = +0.15
        assert!(line.contains("total_rPnL=+0.1500"), "{line}");
        assert!(line.contains("total_netPnL="));
        assert!(line.ends_with('\n'));
    }
}
