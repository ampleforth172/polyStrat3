//! Fair price computation: rolling Chainlink price window, per-second vol,
//! log-normal probability that the window resolves UP. Port of the Python
//! `FairPrice` class.

use std::collections::VecDeque;

use crate::config::FairCfg;

const YEAR_SECS: f64 = 365.0 * 24.0 * 3600.0;

fn norm_cdf(x: f64) -> f64 {
    (1.0 + libm::erf(x / std::f64::consts::SQRT_2)) / 2.0
}

pub struct FairPrice {
    cfg: FairCfg,
    interval_secs: f64,
    prices: VecDeque<(f64, f64)>, // (ts, px)
    open_price: f64,
    open_window_ts: f64,
    hist_vol_per_sec: f64,
    /// Rolling vol memoized at each `update()` (1 Hz). `fair_yes` runs on
    /// EVERY market-data event — recomputing the O(n) deque scan there
    /// would put hundreds of loads on the tick→order path for a value that
    /// only changes once a second.
    cached_vol_per_sec: f64,
    /// Single-slot memo for `ret_over`: (window_secs, result). Same
    /// invalidation story as the vol cache — the deque only changes in
    /// `update()`.
    momentum_cache: std::cell::Cell<Option<(f64, Option<f64>)>>,
}

impl FairPrice {
    pub fn new(cfg: FairCfg, interval_secs: f64) -> Self {
        let hist_vol_per_sec = cfg.default_annual_vol / YEAR_SECS.sqrt();
        Self {
            cfg,
            interval_secs,
            prices: VecDeque::new(),
            open_price: 0.0,
            open_window_ts: 0.0,
            hist_vol_per_sec,
            cached_vol_per_sec: hist_vol_per_sec,
            momentum_cache: std::cell::Cell::new(None),
        }
    }

    /// Feed the rolling vol window. Does NOT touch the open price — the
    /// engine rolls that explicitly via `maybe_roll_open` with the latest
    /// aggregated spot.
    pub fn update(&mut self, price: f64, ts_secs: f64) {
        let cutoff = ts_secs - self.cfg.vol_window_secs;
        while self
            .prices
            .front()
            .map(|(t, _)| *t < cutoff)
            .unwrap_or(false)
        {
            self.prices.pop_front();
        }
        self.prices.push_back((ts_secs, price));
        // The deque changed: refresh the memoized derived values.
        self.cached_vol_per_sec = self.compute_vol_per_sec();
        self.momentum_cache.set(None);
    }

    /// Roll the window open price at interval boundaries. The engine calls
    /// this on every market-data event with the latest AGGREGATED spot
    /// (Binance-driven), so a missing REST seed is healed by the freshest
    /// spot price at the boundary — not by a lagging Chainlink print.
    pub fn maybe_roll_open(&mut self, price: f64, ts_secs: f64) {
        let boundary = (ts_secs / self.interval_secs).floor() * self.interval_secs;
        if boundary != self.open_window_ts {
            self.open_window_ts = boundary;
            self.open_price = price;
            tracing::info!("window open rolled from spot: {price:.2} (boundary {boundary})");
        }
    }

    pub fn set_open_price(&mut self, price: f64, window_ts: f64) {
        self.open_price = price;
        self.open_window_ts = window_ts;
    }

    pub fn seed_historical_vol(&mut self, annual_vol: f64) {
        self.hist_vol_per_sec = annual_vol / YEAR_SECS.sqrt();
        self.cached_vol_per_sec = self.compute_vol_per_sec();
    }

    /// Log-normal probability that price is UP at window expiry.
    pub fn fair_yes(&self, price: f64, ts_secs: f64) -> Option<f64> {
        if self.open_price <= 0.0 {
            return None;
        }
        let tte = (self.open_window_ts + self.interval_secs) - ts_secs;
        if tte < 1.0 {
            return None;
        }
        let r = (price / self.open_price).ln();
        let std = self.vol_per_sec() * tte.sqrt();
        if std < 1e-10 {
            return Some(if r > 0.0 { 0.99 } else { 0.01 });
        }
        let p = norm_cdf(r / std);
        Some(((p * 10_000.0).round() / 10_000.0).clamp(0.01, 0.99))
    }

    /// Fair value with an additive expected log-return `alpha_ret` applied to
    /// the spot leg — used by the alpha module (§4.3b of the plan).
    pub fn fair_yes_with_alpha(&self, price: f64, ts_secs: f64, alpha_ret: f64) -> Option<f64> {
        if self.open_price <= 0.0 {
            return None;
        }
        let tte = (self.open_window_ts + self.interval_secs) - ts_secs;
        if tte < 1.0 {
            return None;
        }
        let r = (price / self.open_price).ln() + alpha_ret;
        let std = self.vol_per_sec() * tte.sqrt();
        if std < 1e-10 {
            return Some(if r > 0.0 { 0.99 } else { 0.01 });
        }
        Some(norm_cdf(r / std).clamp(0.01, 0.99))
    }

    /// Seconds remaining in the current window.
    pub fn tte(&self, ts_secs: f64) -> f64 {
        ((self.open_window_ts + self.interval_secs) - ts_secs).max(0.0)
    }

    pub fn annualized_vol(&self) -> f64 {
        self.vol_per_sec() * YEAR_SECS.sqrt()
    }

    pub fn latest_price(&self) -> Option<f64> {
        self.prices.back().map(|(_, p)| *p)
    }

    pub fn open_price(&self) -> f64 {
        self.open_price
    }

    /// Absolute log-return over the trailing `window_secs` (oldest in-window
    /// vs latest). None until the rolling deque spans the window.
    /// Memoized per `update()`: called on every snapshot but the underlying
    /// deque only changes at 1 Hz.
    pub fn ret_over(&self, window_secs: f64) -> Option<f64> {
        if let Some((w, r)) = self.momentum_cache.get() {
            if w == window_secs {
                return r;
            }
        }
        let r = self.compute_ret_over(window_secs);
        self.momentum_cache.set(Some((window_secs, r)));
        r
    }

    fn compute_ret_over(&self, window_secs: f64) -> Option<f64> {
        let (last_ts, last_px) = *self.prices.back()?;
        let cutoff = last_ts - window_secs;
        // Deque must reach back at least to the cutoff to span the window.
        let (first_ts, _) = *self.prices.front()?;
        if first_ts > cutoff {
            return None;
        }
        // Oldest observation still inside the window.
        let base = self
            .prices
            .iter()
            .find(|(t, _)| *t >= cutoff)
            .map(|(_, p)| *p)?;
        if base <= 0.0 {
            return None;
        }
        Some((last_px / base).ln())
    }

    fn vol_per_sec(&self) -> f64 {
        // Config override beats everything: rolling estimate AND bootstrap.
        if let Some(v) = self.cfg.vol_override {
            return v / YEAR_SECS.sqrt();
        }
        self.cached_vol_per_sec
    }

    /// The O(n) deque scan — run once per `update()`, never per event.
    fn compute_vol_per_sec(&self) -> f64 {
        if self.prices.len() < 5 {
            return self.hist_vol_per_sec;
        }
        let pts: Vec<&(f64, f64)> = self.prices.iter().collect();
        let mut var_sum = 0.0;
        let mut dt_sum = 0.0;
        let n = pts.len() - 1;
        for i in 1..pts.len() {
            let r = (pts[i].1 / pts[i - 1].1).ln();
            var_sum += r * r;
            dt_sum += pts[i].0 - pts[i - 1].0;
        }
        let avg_dt = if dt_sum / n as f64 == 0.0 {
            1.0
        } else {
            dt_sum / n as f64
        };
        ((var_sum / n as f64) / avg_dt).sqrt()
    }
}
