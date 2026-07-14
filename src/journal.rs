//! Event journal: append-only JSONL record of every inbound engine event,
//! plus window-start metadata. A journal replayed through the engine
//! (`--replay`) reproduces the exact decision path — the strategies are
//! pure state machines, so same events + same clock ⇒ same orders.
//!
//! Record grammar (one JSON object per line):
//!   {"ts": <epoch secs>, "kind": {"Window": {...}} | {"Ev": <Event>}}

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::{Event, MarketInfo};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RecordKind {
    /// A market window began: everything needed to reprime pricing state.
    Window {
        market: MarketInfo,
        open_price: f64,
        open_ts: f64,
        annual_vol: f64,
    },
    /// One inbound engine event.
    Ev(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub ts: f64,
    pub kind: RecordKind,
}

pub struct JournalWriter {
    w: BufWriter<File>,
    path: PathBuf,
    lines: u64,
    /// Reused serialization buffer — no per-record String allocation.
    buf: Vec<u8>,
}

impl JournalWriter {
    /// One journal file per session: `{dir}/journal_{symbol}_{start_ts}.jsonl`.
    pub fn create(dir: &str, symbol: &str, start_ts: f64) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create journal dir {dir}: {e}"))?;
        let path = PathBuf::from(dir).join(format!(
            "journal_{}_{}.jsonl",
            symbol.to_lowercase(),
            start_ts as u64
        ));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open journal {}: {e}", path.display()))?;
        tracing::info!("journal recording to {}", path.display());
        Ok(Self {
            w: BufWriter::new(file),
            path,
            lines: 0,
            buf: Vec::with_capacity(512),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&mut self, ts: f64, kind: RecordKind) {
        let rec = Record { ts, kind };
        // Serialize into the reused buffer (same bytes as to_string + '\n').
        self.buf.clear();
        match serde_json::to_writer(&mut self.buf, &rec) {
            Ok(()) => {
                self.buf.push(b'\n');
                if self.w.write_all(&self.buf).is_err() {
                    tracing::warn!("journal write failed");
                }
                self.lines += 1;
                // Amortized flush: cheap durability without a syscall per event.
                if self.lines % 64 == 0 {
                    let _ = self.w.flush();
                }
            }
            Err(e) => tracing::warn!("journal serialize failed: {e}"),
        }
    }

    pub fn flush(&mut self) {
        let _ = self.w.flush();
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Iterate a journal file; malformed lines are skipped with a warning.
pub fn read(path: &Path) -> Result<impl Iterator<Item = Record>, String> {
    let f = File::open(path).map_err(|e| format!("open journal {}: {e}", path.display()))?;
    Ok(BufReader::new(f).lines().filter_map(|line| {
        let line = line.ok()?;
        match serde_json::from_str::<Record>(&line) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("skipping malformed journal line: {e}");
                None
            }
        }
    }))
}