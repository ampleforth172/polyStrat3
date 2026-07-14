//! Trade recorder: appends every trade from the trade subscription (live
//! user-channel fills; simulated fills in DryRun) to a per-symbol CSV.

use std::fs::OpenOptions;
use std::path::PathBuf;

use crate::types::{Outcome, Side};

pub struct TradeCsvRow<'a> {
    pub ts_utc: String,
    pub slug: &'a str,
    pub condition_id: &'a str,
    pub token_id: &'a str,
    pub outcome: Outcome,
    pub side: Side,
    pub px: f64,
    pub sz: f64,
    pub fee: f64,
    pub order_id: &'a str,
    pub trade_id: &'a str,
    pub mode: &'a str,
    pub position_after: f64,
    pub avg_entry_after: f64,
}

pub struct TradeLog {
    writer: Option<csv::Writer<std::fs::File>>,
    path: PathBuf,
}

const HEADER: [&str; 14] = [
    "ts_utc",
    "slug",
    "condition_id",
    "token_id",
    "outcome",
    "side",
    "price",
    "size",
    "fee",
    "order_id",
    "trade_id",
    "mode",
    "position_after",
    "avg_entry_after",
];

impl TradeLog {
    /// `enabled = false` produces a no-op logger.
    pub fn new(enabled: bool, dir: &str, symbol: &str) -> Result<Self, String> {
        let path = PathBuf::from(dir).join(format!("{}_trades.csv", symbol.to_lowercase()));
        if !enabled {
            return Ok(Self { writer: None, path });
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("create output dir {dir}: {e}"))?;
        let is_new = !path.exists() || std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open trade csv {}: {e}", path.display()))?;
        let mut writer = csv::WriterBuilder::new().has_headers(false).from_writer(file);
        if is_new {
            writer
                .write_record(HEADER)
                .and_then(|_| writer.flush().map_err(csv::Error::from))
                .map_err(|e| format!("write csv header: {e}"))?;
        }
        Ok(Self {
            writer: Some(writer),
            path,
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn record(&mut self, row: &TradeCsvRow) -> Result<(), String> {
        let Some(w) = self.writer.as_mut() else {
            return Ok(());
        };
        w.write_record([
            row.ts_utc.as_str(),
            row.slug,
            row.condition_id,
            row.token_id,
            row.outcome.label(),
            &row.side.to_string(),
            &format!("{:.4}", row.px),
            &format!("{:.4}", row.sz),
            &format!("{:.6}", row.fee),
            row.order_id,
            row.trade_id,
            row.mode,
            &format!("{:.4}", row.position_after),
            &format!("{:.4}", row.avg_entry_after),
        ])
        .map_err(|e| format!("write csv row: {e}"))?;
        // Fills are rare; flush per row for crash safety.
        w.flush().map_err(|e| format!("flush csv: {e}"))
    }
}