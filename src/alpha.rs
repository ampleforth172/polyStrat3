//! Short-horizon alpha signal from Binance order-book imbalance, decayed
//! trade flow, and the Binance–Chainlink basis. Recomputed on a 100 ms timer;
//! output is an expected log-return pushed through the fair-value model to a
//! clamped quote-center shift (plan §4.3b).

use crate::config::AlphaCfg;
use crate::fair::FairPrice;

pub struct Alpha {
    cfg: AlphaCfg,
    // Binance book (top-N), updated passively.
    bids: Vec<(f64, f64)>,
    asks: Vec<(f64, f64)>,
    binance_mid: Option<f64>,
    // Signed taker flow with exponential decay.
    flow: f64,
    flow_ts: f64,
    last_update_ts: f64,
}

impl Alpha {
    pub fn new(cfg: AlphaCfg) -> Self {
        Self {
            cfg,
            bids: Vec::new(),
            asks: Vec::new(),
            binance_mid: None,
            flow: 0.0,
            flow_ts: 0.0,
            last_update_ts: 0.0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn on_book(&mut self, bids: Vec<(f64, f64)>, asks: Vec<(f64, f64)>, ts: f64) {
        self.binance_mid = match (bids.first(), asks.first()) {
            (Some((b, _)), Some((a, _))) => Some((b + a) / 2.0),
            _ => None,
        };
        self.bids = bids;
        self.asks = asks;
        self.last_update_ts = self.last_update_ts.max(ts);
    }

    pub fn on_trade(&mut self, px: f64, sz: f64, is_buyer_maker: bool, ts: f64) {
        let _ = px;
        self.decay_flow(ts);
        // is_buyer_maker == true means the aggressor SOLD.
        let signed = if is_buyer_maker { -sz } else { sz };
        self.flow += signed;
        self.flow_ts = ts;
        self.last_update_ts = self.last_update_ts.max(ts);
    }

    fn decay_flow(&mut self, now: f64) {
        if self.flow_ts > 0.0 && now > self.flow_ts && self.cfg.trade_flow_halflife_secs > 0.0 {
            let dt = now - self.flow_ts;
            self.flow *= (-dt * std::f64::consts::LN_2 / self.cfg.trade_flow_halflife_secs).exp();
            self.flow_ts = now;
        }
    }

    /// Raw combined signal in ~[-1, 1]. None when disabled or the feed is stale.
    pub fn alpha_raw(&mut self, chainlink_px: Option<f64>, now: f64) -> Option<f64> {
        if !self.cfg.enabled {
            return None;
        }
        if self.last_update_ts <= 0.0
            || (now - self.last_update_ts) * 1000.0 > self.cfg.stale_ms as f64
        {
            return None; // stale feed -> no signal rather than a wrong one
        }
        self.decay_flow(now);

        let levels = self.cfg.imbalance_levels;
        let bid_qty: f64 = self.bids.iter().take(levels).map(|(_, q)| q).sum();
        let ask_qty: f64 = self.asks.iter().take(levels).map(|(_, q)| q).sum();
        let obi = if bid_qty + ask_qty > 0.0 {
            (bid_qty - ask_qty) / (bid_qty + ask_qty)
        } else {
            0.0
        };

        let tfi = self.flow / (self.flow.abs() + self.cfg.flow_normalizer);

        let basis = match (self.binance_mid, chainlink_px) {
            (Some(bm), Some(cl)) if bm > 0.0 && cl > 0.0 => {
                ((bm / cl).ln() / self.cfg.basis_scale).clamp(-1.0, 1.0)
            }
            _ => 0.0,
        };

        Some(self.cfg.w_obi * obi + self.cfg.w_tfi * tfi + self.cfg.w_basis * basis)
    }

    /// Quote-center shift: alpha expressed as an expected log-return, pushed
    /// through the log-normal fair model, then hard-clamped.
    /// `spot` is the (possibly Binance-aggregated) pricing spot; `chainlink`
    /// is the raw oracle print, used only for the basis term — passing the
    /// aggregate there would cancel the basis out.
    pub fn shift(
        &mut self,
        fair: &FairPrice,
        spot: Option<f64>,
        chainlink: Option<f64>,
        now: f64,
    ) -> f64 {
        let basis_ref = chainlink.or(spot);
        let (Some(px), Some(raw)) = (spot, self.alpha_raw(basis_ref, now)) else {
            return 0.0;
        };
        let base = match fair.fair_yes(px, now) {
            Some(v) => v,
            None => return 0.0,
        };
        let shifted = match fair.fair_yes_with_alpha(px, now, raw * self.cfg.alpha_ret_scale) {
            Some(v) => v,
            None => return 0.0,
        };
        (shifted - base).clamp(-self.cfg.max_alpha_shift, self.cfg.max_alpha_shift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FairCfg;

    fn cfg() -> AlphaCfg {
        AlphaCfg {
            enabled: true,
            ..AlphaCfg::default()
        }
    }

    #[test]
    fn disabled_returns_none_and_zero_shift() {
        let mut a = Alpha::new(AlphaCfg::default()); // enabled = false
        a.on_book(vec![(100.0, 5.0)], vec![(101.0, 1.0)], 1.0);
        assert!(a.alpha_raw(Some(100.5), 1.0).is_none());
        let fair = FairPrice::new(FairCfg::default(), 900.0);
        assert_eq!(a.shift(&fair, Some(100.5), Some(100.5), 1.0), 0.0);
    }

    #[test]
    fn obi_sign_follows_book_imbalance() {
        let mut a = Alpha::new(AlphaCfg { w_tfi: 0.0, w_basis: 0.0, ..cfg() });
        a.on_book(vec![(100.0, 9.0)], vec![(100.1, 1.0)], 1.0);
        let r = a.alpha_raw(None, 1.0).unwrap();
        assert!(r > 0.0, "bid-heavy book must be positive, got {r}");
        a.on_book(vec![(100.0, 1.0)], vec![(100.1, 9.0)], 2.0);
        assert!(a.alpha_raw(None, 2.0).unwrap() < 0.0);
    }

    #[test]
    fn trade_flow_decays_with_halflife() {
        let mut a = Alpha::new(AlphaCfg {
            w_obi: 0.0,
            w_basis: 0.0,
            w_tfi: 1.0,
            trade_flow_halflife_secs: 5.0,
            flow_normalizer: 10.0,
            ..cfg()
        });
        a.on_book(vec![(100.0, 1.0)], vec![(100.1, 1.0)], 1.0);
        a.on_trade(100.0, 10.0, false, 1.0); // aggressive buy of 10
        let r0 = a.alpha_raw(None, 1.0).unwrap();
        assert!((r0 - 0.5).abs() < 1e-9, "10/(10+10) = 0.5, got {r0}");
        // After exactly one halflife the flow halves -> 5/(5+10) = 1/3.
        a.last_update_ts = 6.0; // keep feed fresh for the staleness check
        let r1 = a.alpha_raw(None, 6.0).unwrap();
        assert!((r1 - 1.0 / 3.0).abs() < 1e-6, "got {r1}");
    }

    #[test]
    fn basis_term_and_weighting() {
        let mut a = Alpha::new(AlphaCfg {
            w_obi: 0.0,
            w_tfi: 0.0,
            w_basis: 1.0,
            basis_scale: 0.001,
            ..cfg()
        });
        // Binance mid 0.1% above Chainlink -> basis = ln(1.001)/0.001 ≈ 1 (clamped).
        a.on_book(vec![(1001.0, 1.0)], vec![(1001.0, 1.0)], 1.0);
        let r = a.alpha_raw(Some(1000.0), 1.0).unwrap();
        assert!(r > 0.9 && r <= 1.0, "got {r}");
    }

    #[test]
    fn stale_feed_gives_no_signal() {
        let mut a = Alpha::new(cfg());
        a.on_book(vec![(100.0, 9.0)], vec![(100.1, 1.0)], 1.0);
        assert!(a.alpha_raw(None, 1.5).is_some());
        // 2 seconds later with stale_ms = 1000 -> stale.
        assert!(a.alpha_raw(None, 3.0).is_none());
    }

    #[test]
    fn shift_is_clamped() {
        let mut a = Alpha::new(AlphaCfg {
            alpha_ret_scale: 1.0, // absurdly large on purpose
            max_alpha_shift: 0.05,
            w_obi: 1.0,
            w_tfi: 0.0,
            w_basis: 0.0,
            ..cfg()
        });
        let mut fair = FairPrice::new(FairCfg::default(), 900.0);
        fair.set_open_price(100_000.0, 0.0);
        a.on_book(vec![(100_000.0, 100.0)], vec![(100_000.1, 0.001)], 10.0);
        let s = a.shift(&fair, Some(100_000.0), Some(100_000.0), 10.0);
        assert!((s - 0.05).abs() < 1e-9, "expected clamp at +0.05, got {s}");
    }
}
