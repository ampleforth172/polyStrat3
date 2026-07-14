//! Static configuration: TOML strategy config + separate credentials file.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::types::Side;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrategyKind {
    Taker,
    Maker,
}

// ---------------------------------------------------------------------------
// [general]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GeneralCfg {
    pub symbol: String,
    pub strategy: StrategyKind,
    pub dry_run: bool,
    pub log_level: String,
    pub interval_seconds: u64,
    pub clob_host: String,
    pub gamma_host: String,
    pub data_host: String,
    pub ws_url: String,
    pub rtds_url: String,
    pub binance_ws_url: String,
    pub binance_rest_url: String,
    pub trade_csv_enabled: bool,
    pub trade_csv_dir: String,
    pub pnl_log_dir: String,
    /// Pin the engine thread to this CPU core (hard pin on Linux; scheduler
    /// hint only on macOS). None = no pinning.
    pub pin_core: Option<usize>,
    /// Minimum interval between order placements per (token, side).
    /// Throttled placements are dropped (the strategy re-emits them on a
    /// later event) and their paired cancel is skipped so the previous
    /// order stays resting. Stop-loss orders are exempt. 0 disables.
    pub order_throttle_ms: u64,
    /// Maximum simultaneous working orders per token. Placements beyond the
    /// cap are dropped with a warning.
    pub max_orders_per_token: usize,
}

impl Default for GeneralCfg {
    fn default() -> Self {
        Self {
            symbol: "BTC".into(),
            strategy: StrategyKind::Taker,
            dry_run: true,
            log_level: "info".into(),
            interval_seconds: 900,
            clob_host: "https://clob.polymarket.com".into(),
            gamma_host: "https://gamma-api.polymarket.com".into(),
            data_host: "https://data-api.polymarket.com".into(),
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws".into(),
            rtds_url: "wss://ws-live-data.polymarket.com".into(),
            binance_ws_url: "wss://stream.binance.com:9443/stream".into(),
            binance_rest_url: "https://api.binance.com".into(),
            trade_csv_enabled: true,
            trade_csv_dir: "output".into(),
            pnl_log_dir: "output".into(),
            pin_core: None,
            order_throttle_ms: 200,
            max_orders_per_token: 2,
        }
    }
}

// ---------------------------------------------------------------------------
// [fair]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FairCfg {
    pub vol_window_secs: f64,
    pub default_annual_vol: f64,
    pub hist_vol_bars: usize,
    /// Fixed annualized volatility for pricing. When set, this overrides
    /// BOTH the rolling realized vol and the historical bootstrap — useful
    /// when the realized estimate is unreliable (quiet tape, sparse feed).
    pub vol_override: Option<f64>,
}

impl Default for FairCfg {
    fn default() -> Self {
        Self {
            vol_window_secs: 300.0,
            default_annual_vol: 0.80,
            hist_vol_bars: 20,
            vol_override: None,
        }
    }
}

// ---------------------------------------------------------------------------
// [spot] — aggregated spot price used for pricing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct SpotCfg {
    /// Binance book-mid/trades drive the aggregated spot.
    pub binance_enabled: bool,
    /// Binance leg staleness bound; a stale Binance leg yields no spot
    /// (or raw Chainlink when `chainlink_enabled`).
    pub binance_stale_ms: u64,
    /// Chainlink prints update the aggregate (extrapolation mode:
    /// Chainlink level × Binance return since the last print).
    /// Default DISABLED: pricing follows Binance directly — the open price
    /// is Binance-based too, so the BTCUSDT/BTCUSD basis cancels in
    /// S/S_open. Chainlink is still recorded for the alpha basis term and
    /// the tick log regardless.
    pub chainlink_enabled: bool,
}

impl Default for SpotCfg {
    fn default() -> Self {
        Self {
            binance_enabled: true,
            binance_stale_ms: 1500,
            chainlink_enabled: false,
        }
    }
}

// ---------------------------------------------------------------------------
// [journal]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct JournalCfg {
    /// Record every inbound event + window metadata to an append-only JSONL
    /// journal (replayable with --replay for deterministic backtesting).
    pub enabled: bool,
    pub dir: String,
}

impl Default for JournalCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: "output".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// [tick]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct TickCfg {
    pub inner_tick: f64,
    pub outer_tick: f64,
    pub inner_lo: f64,
    pub inner_hi: f64,
}

impl Default for TickCfg {
    fn default() -> Self {
        Self {
            inner_tick: 0.01,
            outer_tick: 0.001,
            inner_lo: 0.04,
            inner_hi: 0.96,
        }
    }
}

impl TickCfg {
    fn tick_for(&self, px: f64) -> f64 {
        if px >= self.inner_lo && px <= self.inner_hi {
            self.inner_tick
        } else {
            self.outer_tick
        }
    }

    /// Round a price onto the grid: bids round down, asks round up.
    /// Re-checks the band after rounding so prices never end up off-tick
    /// around the inner_lo/inner_hi boundaries.
    pub fn round(&self, px: f64, side: Side) -> f64 {
        let round_with = |px: f64, tick: f64| -> f64 {
            let steps = px / tick;
            // Epsilon guards against float noise: 1.0 - 0.53 = 0.46999…97
            // must round to 0.47, not floor a full tick down to 0.46.
            let steps = match side {
                Side::Buy => (steps + 1e-9).floor(),
                Side::Sell => (steps - 1e-9).ceil(),
            };
            // Snap away float noise to the tick's decimals.
            let decimals = (-tick.log10()).round() as i32;
            let scale = 10f64.powi(decimals);
            (steps * tick * scale).round() / scale
        };
        let first = round_with(px, self.tick_for(px));
        let second_tick = self.tick_for(first);
        if (second_tick - self.tick_for(px)).abs() > f64::EPSILON {
            round_with(first, second_tick)
        } else {
            first
        }
    }
}

// ---------------------------------------------------------------------------
// [taker]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HourlyOverride {
    pub trade_sides: Option<Vec<String>>,
    pub order_price_min: Option<f64>,
}

impl Default for HourlyOverride {
    fn default() -> Self {
        Self {
            trade_sides: None,
            order_price_min: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TakerCfg {
    pub order_size: f64,
    pub order_price_min: f64,
    pub order_price_max: f64,
    pub stop_loss_price: f64,
    pub take_profit_price: f64,
    pub max_position: f64,
    pub trade_sides: Vec<String>,
    pub cooldown_secs: f64,
    pub pre_expiry_cutoff_pct: f64,
    /// UTC hour (as string key in TOML) -> override.
    pub hourly: HashMap<String, HourlyOverride>,
}

impl Default for TakerCfg {
    fn default() -> Self {
        Self {
            order_size: 5.2,
            order_price_min: 0.85,
            order_price_max: 0.95,
            stop_loss_price: 0.4,
            take_profit_price: 0.03,
            max_position: 5.2,
            trade_sides: vec!["YES".into(), "NO".into()],
            cooldown_secs: 10.0,
            pre_expiry_cutoff_pct: 0.12,
            hourly: HashMap::new(),
        }
    }
}

impl TakerCfg {
    pub fn cutoff_secs(&self, interval_seconds: u64) -> f64 {
        (interval_seconds as f64 * self.pre_expiry_cutoff_pct).floor()
    }

    /// Effective (trade_sides, order_price_min) for a UTC hour.
    pub fn hourly_cfg(&self, hour_utc: u32) -> (Vec<String>, f64) {
        let ov = self.hourly.get(&hour_utc.to_string());
        let sides = ov
            .and_then(|o| o.trade_sides.clone())
            .unwrap_or_else(|| self.trade_sides.clone());
        let opm = ov
            .and_then(|o| o.order_price_min)
            .unwrap_or(self.order_price_min);
        (sides, opm)
    }
}

// ---------------------------------------------------------------------------
// [maker]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MakerCfg {
    pub half_spread: f64,
    pub min_spread: f64,
    pub max_spread: f64,
    pub quote_size: f64,
    pub quote_refresh_secs: f64,
    pub reprice_threshold: f64,
    pub max_quote_drift: f64,
    pub max_inventory: f64,
    pub skew_shift: f64,
    pub tte_spread_multiplier: f64,
    pub skew_spread_multiplier: f64,
    pub momentum_window_secs: f64,
    pub momentum_threshold: f64,
    pub momentum_spread_multiplier: f64,
    pub max_loss_per_market: f64,
    pub gross_exposure_limit: f64,
    pub pre_expiry_cutoff_secs: f64,
    /// Post-only: skip a quote whose price crosses the market's far touch
    /// (a BUY at or above the token's best ask would take liquidity).
    pub maker_only: bool,
    /// How many units of the per-token maker fee (at the quote center) to
    /// add to EACH side of the spread, so the quoted edge is net of fees.
    /// fee_unit = p * 0.25 * (p(1-p))^2 * maker_discount. 0 disables.
    pub fee_spread_factor: f64,
    /// Minimum seconds between AGGRESSIVE amends of the same quote (bid
    /// moving up / ask moving down). A blocked aggressive move keeps the
    /// previous price for that round; passive moves (away from the market)
    /// are never restricted. Default 1.0 = at most one aggressive amend per
    /// second per quote. 0 disables.
    pub aggressive_amend_interval_secs: f64,
}

impl Default for MakerCfg {
    fn default() -> Self {
        Self {
            half_spread: 0.02,
            min_spread: 0.01,
            max_spread: 0.10,
            quote_size: 5.4,
            quote_refresh_secs: 5.0,
            reprice_threshold: 0.002,
            max_quote_drift: 0.01,
            max_inventory: 20.0,
            skew_shift: 0.02,
            tte_spread_multiplier: 2.0,
            skew_spread_multiplier: 1.5,
            momentum_window_secs: 60.0,
            momentum_threshold: 0.001,
            momentum_spread_multiplier: 2.0,
            max_loss_per_market: 5.0,
            gross_exposure_limit: 30.0,
            pre_expiry_cutoff_secs: 60.0,
            aggressive_amend_interval_secs: 1.0,
            maker_only: true,
            fee_spread_factor: 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// [alpha]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AlphaCfg {
    pub enabled: bool,
    pub imbalance_levels: usize,
    pub w_obi: f64,
    pub w_tfi: f64,
    pub w_basis: f64,
    pub basis_scale: f64,
    pub trade_flow_halflife_secs: f64,
    pub flow_normalizer: f64,
    pub alpha_ret_scale: f64,
    pub max_alpha_shift: f64,
    pub stale_ms: u64,
    pub tick_ms: u64,
}

impl Default for AlphaCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            imbalance_levels: 10,
            w_obi: 0.3,
            w_tfi: 0.4,
            w_basis: 0.3,
            basis_scale: 0.0005,
            trade_flow_halflife_secs: 5.0,
            flow_normalizer: 10.0,
            alpha_ret_scale: 0.0005,
            max_alpha_shift: 0.05,
            stale_ms: 1000,
            tick_ms: 100,
        }
    }
}

// ---------------------------------------------------------------------------
// Root config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralCfg,
    pub fair: FairCfg,
    pub spot: SpotCfg,
    pub journal: JournalCfg,
    pub tick: TickCfg,
    pub taker: TakerCfg,
    pub maker: MakerCfg,
    pub alpha: AlphaCfg,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).map_err(|e| format!("bad config {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        let g = &self.general;
        if g.symbol.is_empty() {
            return Err("general.symbol must not be empty".into());
        }
        parse_level(&g.log_level)?;
        let t = &self.tick;
        if !(t.inner_lo < t.inner_hi) {
            return Err("tick.inner_lo must be < tick.inner_hi".into());
        }
        if t.inner_tick <= 0.0 || t.outer_tick <= 0.0 {
            return Err("tick sizes must be positive".into());
        }
        let tk = &self.taker;
        if tk.order_price_min > tk.order_price_max {
            return Err("taker.order_price_min must be <= order_price_max".into());
        }
        for h in tk.hourly.keys() {
            let ok = h.parse::<u32>().map(|v| v < 24).unwrap_or(false);
            if !ok {
                return Err(format!("taker.hourly key '{h}' is not a UTC hour 0-23"));
            }
        }
        if let Some(v) = self.fair.vol_override {
            if !(v > 0.0 && v.is_finite()) {
                return Err(format!("fair.vol_override must be a positive number, got {v}"));
            }
        }
        if self.general.max_orders_per_token == 0 {
            return Err("general.max_orders_per_token must be >= 1".into());
        }
        if !self.spot.binance_enabled && !self.spot.chainlink_enabled {
            return Err(
                "spot: at least one of binance_enabled / chainlink_enabled must be true".into(),
            );
        }
        let m = &self.maker;
        if m.min_spread > m.max_spread {
            return Err("maker.min_spread must be <= max_spread".into());
        }
        if m.min_spread < self.tick.inner_tick {
            return Err("maker.min_spread must be >= tick.inner_tick".into());
        }
        if m.max_inventory <= 0.0 {
            return Err("maker.max_inventory must be positive".into());
        }
        Ok(())
    }
}

/// Validate a log level / EnvFilter directive string.
pub fn parse_level(s: &str) -> Result<(), String> {
    // Accept plain levels and per-module directives; tracing's EnvFilter
    // does the real parse at init. Reject empty and obviously bad values.
    let head = s.split(',').next().unwrap_or("");
    let plain = head.split('=').last().unwrap_or("");
    match plain.to_ascii_lowercase().as_str() {
        "trace" | "debug" | "info" | "warn" | "error" | "off" => Ok(()),
        _ => Err(format!("invalid log level '{s}'")),
    }
}

// ---------------------------------------------------------------------------
// Credentials — separate TOML file, live mode only
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CredentialsFile {
    credentials: CredentialsInner,
}

#[derive(Deserialize)]
struct CredentialsInner {
    private_key: String,
    #[serde(default)]
    funder: String,
}

/// Loaded credentials. Never derives Debug/Display with the secret visible.
pub struct Credentials {
    private_key: String,
    pub funder: String,
}

impl Credentials {
    pub fn expose_private_key(&self) -> &String {
        &self.private_key
    }

    /// Env vars take precedence over the file. File must be 0600 or stricter.
    pub fn load(path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("credentials file {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Err(format!(
                    "credentials file {} is group/world-accessible (mode {:o}); chmod 600 it",
                    path.display(),
                    mode
                ));
            }
        }
        let _ = meta;
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read credentials {}: {e}", path.display()))?;
        let f: CredentialsFile =
            toml::from_str(&raw).map_err(|e| format!("bad credentials file: {e}"))?;
        if f.credentials.private_key.is_empty() {
            return Err("credentials.private_key is empty".into());
        }
        Ok(Self {
            private_key: f.credentials.private_key,
            funder: f.credentials.funder,
        })
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("private_key", &"<redacted>")
            .field("funder", &self.funder)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let cfg = Config::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.general.log_level, "info");
        assert!(cfg.general.dry_run);
        assert_eq!(cfg.general.clob_host, "https://clob.polymarket.com");
    }

    #[test]
    fn toml_round_trip_with_overrides() {
        let toml_src = r#"
            [general]
            symbol = "ETH"
            strategy = "maker"
            log_level = "debug"

            [maker]
            momentum_threshold = 0.002

            [taker.hourly.22]
            trade_sides = ["YES"]
            order_price_min = 0.6
        "#;
        let cfg: Config = toml::from_str(toml_src).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.general.symbol, "ETH");
        assert_eq!(cfg.general.strategy, StrategyKind::Maker);
        assert_eq!(cfg.general.pin_core, None, "pin_core defaults to unpinned");
        let pinned: Config = toml::from_str("[general]\npin_core = 3\n").unwrap();
        assert_eq!(pinned.general.pin_core, Some(3));
        let (sides, opm) = cfg.taker.hourly_cfg(22);
        assert_eq!(sides, vec!["YES".to_string()]);
        assert!((opm - 0.6).abs() < 1e-12);
        // Hour without override falls back to defaults.
        let (sides, opm) = cfg.taker.hourly_cfg(3);
        assert_eq!(sides.len(), 2);
        assert!((opm - 0.85).abs() < 1e-12);
    }

    #[test]
    fn invalid_log_level_rejected() {
        let mut cfg = Config::default();
        cfg.general.log_level = "loud".into();
        assert!(cfg.validate().is_err());
        cfg.general.log_level = "info,polymm::alpha=debug".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn invalid_hourly_key_rejected() {
        let mut cfg = Config::default();
        cfg.taker.hourly.insert("25".into(), HourlyOverride::default());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn tick_rounding_inner_and_outer_bands() {
        let t = TickCfg::default();
        // Inner band: 0.01 ticks.
        assert_eq!(t.round(0.8534, Side::Buy), 0.85);
        assert_eq!(t.round(0.8534, Side::Sell), 0.86);
        // Outer band: 0.001 ticks.
        assert_eq!(t.round(0.0323, Side::Buy), 0.032);
        assert_eq!(t.round(0.0323, Side::Sell), 0.033);
        assert_eq!(t.round(0.9787, Side::Buy), 0.978);
        assert_eq!(t.round(0.9787, Side::Sell), 0.979);
        // Idempotent on already-on-tick prices.
        assert_eq!(t.round(0.85, Side::Buy), 0.85);
        assert_eq!(t.round(0.85, Side::Sell), 0.85);
        assert_eq!(t.round(0.032, Side::Buy), 0.032);
        // Robust to float noise from complement prices (1.0 - x).
        assert_eq!(t.round(1.0 - 0.53, Side::Buy), 0.47);
        assert_eq!(t.round(1.0 - 0.47, Side::Sell), 0.53);
    }

    #[test]
    fn tick_rounding_band_boundaries() {
        let t = TickCfg::default();
        // Just inside the inner band boundary.
        assert_eq!(t.round(0.041, Side::Buy), 0.04);
        // Just below inner_lo uses 0.001 ticks.
        assert_eq!(t.round(0.0388, Side::Buy), 0.038);
        assert_eq!(t.round(0.0388, Side::Sell), 0.039);
        // A sell just below inner_lo that rounds up to the boundary is fine on
        // either grid (0.04 is on both).
        assert_eq!(t.round(0.0395, Side::Sell), 0.04);
        // Just above inner_hi uses 0.001 ticks.
        assert_eq!(t.round(0.9605, Side::Buy), 0.96);
        assert_eq!(t.round(0.9612, Side::Buy), 0.961);
    }

    #[test]
    fn maker_spread_floor_validated() {
        let mut cfg = Config::default();
        cfg.maker.min_spread = 0.001; // below inner tick
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn spot_section_defaults_and_validation() {
        // Defaults: Binance drives the aggregate, Chainlink leg disabled.
        let cfg = Config::default();
        assert!(cfg.spot.binance_enabled);
        assert!(!cfg.spot.chainlink_enabled, "chainlink leg must default to disabled");
        assert_eq!(cfg.spot.binance_stale_ms, 1500);
        // TOML round-trip.
        let cfg: Config =
            toml::from_str("[spot]\nchainlink_enabled = true\nbinance_stale_ms = 900\n").unwrap();
        assert!(cfg.spot.chainlink_enabled);
        assert_eq!(cfg.spot.binance_stale_ms, 900);
        cfg.validate().unwrap();
        // Both legs off is rejected.
        let mut bad = Config::default();
        bad.spot.binance_enabled = false;
        bad.spot.chainlink_enabled = false;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn vol_override_parsing_and_validation() {
        let cfg: Config = toml::from_str("[fair]\nvol_override = 0.35\n").unwrap();
        assert_eq!(cfg.fair.vol_override, Some(0.35));
        cfg.validate().unwrap();
        // Absent by default.
        assert_eq!(Config::default().fair.vol_override, None);
        // Zero/negative rejected.
        let mut bad = Config::default();
        bad.fair.vol_override = Some(0.0);
        assert!(bad.validate().is_err());
        bad.fair.vol_override = Some(-0.5);
        assert!(bad.validate().is_err());
    }
}
