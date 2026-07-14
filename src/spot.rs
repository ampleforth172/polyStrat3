//! Aggregated spot price for pricing.
//!
//! Two legs, each independently switchable via `[spot]` config:
//!
//! - **Binance** (default ON): book mid / last trade — the fast leg.
//! - **Chainlink** (default OFF): when enabled, the aggregate keeps the
//!   Chainlink *level* and extrapolates it with Binance *returns* since the
//!   last oracle print: `agg(t) = chainlink_last × binance(t) / binance(t_cl)`.
//!   When disabled (default), Chainlink prints do NOT move the aggregate —
//!   pricing follows Binance directly. This is consistent because the window
//!   open price is Binance-based too, so the BTCUSDT/BTCUSD basis cancels in
//!   the S/S_open ratio.
//!
//! Chainlink prints are always *recorded* (for the alpha basis term and the
//! tick log) regardless of whether they drive the aggregate.

/// Which Binance update type produced the latest price in the aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotSource {
    /// Book mid from a depth update.
    Depth,
    /// Last trade price from an aggTrade.
    Trade,
}

impl std::fmt::Display for SpotSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotSource::Depth => write!(f, "depth"),
            SpotSource::Trade => write!(f, "trade"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpotAggregator {
    binance_enabled: bool,
    chainlink_enabled: bool,
    stale_ms: u64,
    chainlink: Option<(f64, f64)>, // (px, ts) — always recorded
    /// (px, ts, source) — freshest of book mid / last trade.
    binance: Option<(f64, f64, SpotSource)>,
    /// Binance price captured at the last Chainlink print (basis anchor,
    /// only meaningful in chainlink-extrapolation mode).
    anchor: Option<f64>,
}

impl SpotAggregator {
    pub fn new(binance_enabled: bool, chainlink_enabled: bool, stale_ms: u64) -> Self {
        Self {
            binance_enabled,
            chainlink_enabled,
            stale_ms,
            chainlink: None,
            binance: None,
            anchor: None,
        }
    }

    pub fn from_cfg(cfg: &crate::config::SpotCfg) -> Self {
        Self::new(cfg.binance_enabled, cfg.chainlink_enabled, cfg.binance_stale_ms)
    }

    /// Always records the print; re-anchors the basis only when the
    /// Chainlink leg drives the aggregate.
    pub fn on_chainlink(&mut self, px: f64, ts: f64) {
        self.chainlink = Some((px, ts));
        if self.chainlink_enabled {
            self.anchor = self.binance.map(|(p, _, _)| p);
        }
    }

    pub fn on_binance(&mut self, px: f64, ts: f64, source: SpotSource) {
        if px > 0.0 {
            self.binance = Some((px, ts, source));
        }
    }

    /// Source of the latest Binance update feeding the aggregate
    /// (trade vs depth), if any.
    pub fn latest_source(&self) -> Option<SpotSource> {
        self.binance.map(|(_, _, s)| s)
    }

    /// Latest recorded Chainlink print (for alpha basis / logging), present
    /// even when the Chainlink leg does not drive the aggregate.
    pub fn chainlink_px(&self) -> Option<f64> {
        self.chainlink.map(|(p, _)| p)
    }

    fn binance_fresh(&self, now: f64) -> Option<f64> {
        self.binance
            .filter(|(_, ts, _)| (now - ts) * 1000.0 <= self.stale_ms as f64)
            .map(|(p, _, _)| p)
    }

    /// The aggregated spot used for pricing.
    pub fn spot(&self, now: f64) -> Option<f64> {
        let cl = self
            .chainlink_enabled
            .then(|| self.chainlink.map(|(p, _)| p))
            .flatten();
        let bn = self
            .binance_enabled
            .then(|| self.binance_fresh(now))
            .flatten();
        match (cl, bn, self.anchor) {
            // Both legs: Chainlink level extrapolated by the Binance move.
            (Some(cp), Some(bp), Some(anchor)) if anchor > 0.0 => Some(cp * bp / anchor),
            // Chainlink leg only (binance disabled/stale, or no anchor yet).
            (Some(cp), _, _) => Some(cp),
            // Binance leg only (default mode, or no Chainlink print yet).
            (None, Some(bp), _) => Some(bp),
            _ => None,
        }
    }
}