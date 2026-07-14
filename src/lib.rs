//! Polymarket 15m Up/Down trader — library crate.
//! Single-threaded event-loop design; see polymm/docs/rust_implementation_plan.md.

pub mod alpha;
pub mod config;
pub mod engine;
pub mod exec;
pub mod fair;
pub mod journal;
pub mod latency;
pub mod md;
pub mod oms;
pub mod pnl_log;
pub mod position;
pub mod spot;
pub mod strategy;
pub mod trade_log;
pub mod types;
