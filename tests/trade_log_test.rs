
#[cfg(test)]
mod tests {
    use poly_strat3::trade_log::{TradeCsvRow, TradeLog};
    use poly_strat3::types::{Outcome, Side};
    use std::path::PathBuf;

    fn row<'a>(trade_id: &'a str) -> TradeCsvRow<'a> {
        TradeCsvRow {
            ts_utc: "2026-07-11T08:00:00Z".into(),
            slug: "btc-updown-15m-1",
            condition_id: "0xcond",
            token_id: "tok-yes",
            outcome: Outcome::Yes,
            side: Side::Buy,
            px: 0.9,
            sz: 5.0,
            fee: 0.0072,
            order_id: "ord-1",
            trade_id,
            mode: "dry-run",
            position_after: 5.0,
            avg_entry_after: 0.9,
        }
    }

    fn tmpdir(name: &str) -> String {
        let d = std::env::temp_dir().join(format!("polymm_tlog_{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.to_str().unwrap().to_string()
    }

    #[test]
    fn header_written_once_and_rows_round_trip() {
        let dir = tmpdir("roundtrip");
        {
            let mut log = TradeLog::new(true, &dir, "BTC").unwrap();
            log.record(&row("t-1")).unwrap();
        }
        {
            // Re-open (append): no second header.
            let mut log = TradeLog::new(true, &dir, "BTC").unwrap();
            log.record(&row("t-2")).unwrap();
        }
        let path = PathBuf::from(&dir).join("btc_trades.csv");
        let mut rdr = csv::Reader::from_path(&path).unwrap();
        assert_eq!(
            rdr.headers().unwrap().iter().collect::<Vec<_>>()[0..3],
            ["ts_utc", "slug", "condition_id"]
        );
        let rows: Vec<csv::StringRecord> = rdr.records().map(|r| r.unwrap()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(&rows[0][10], "t-1");
        assert_eq!(&rows[1][10], "t-2");
        assert_eq!(&rows[0][4], "YES");
        assert_eq!(&rows[0][5], "BUY");
    }

    #[test]
    fn disabled_writes_nothing() {
        let dir = tmpdir("disabled");
        let mut log = TradeLog::new(false, &dir, "BTC").unwrap();
        log.record(&row("t-1")).unwrap();
        assert!(!log.path().exists());
    }
}
