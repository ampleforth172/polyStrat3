//! Market-making strategy: BUY-ONLY quoting around an alpha-adjusted fair.
//!
//! Quote pipeline, one pass per requote:
//!
//! ```text
//!   center  = quote_center (fair + alpha), shifted against inventory
//!   half    = base × tte × inventory × momentum   (clamped to [min,max]/2)
//!           + maker-fee unit                      (post-clamp cost recovery)
//!
//!   logical YES quote:   bid = center − half      ask = center + half
//!        (tick-rounded away from center; outside (0,1) ⇒ NaN ⇒ side not quoted)
//!
//!   physical BUY orders: BUY YES @ bid            (the logical YES bid)
//!                        BUY NO  @ 1 − ask        (the logical YES ask)
//!
//!   eligibility: inventory caps → post-only cross guard → aggressive-amend
//!   limiter → Action::Targets per token (the OMS diffs vs resting orders)
//! ```
//!
//! Invariants:
//! - **No SELL is ever submitted.** Selling YES ≡ buying NO at the complement
//!   price; a matched YES+NO pair redeems for exactly $1, locking the spread
//!   as realized PnL without waiting for on-chain settlement.
//! - With `maker_only`, a quote at/above the token's best ask is dropped,
//!   never crossed.
//! - Fee widening is applied AFTER the max-spread clamp: cost recovery, not
//!   spread.
//! - At most one AGGRESSIVE amend (toward the market) per token/side per
//!   `aggressive_amend_interval_secs`; passive moves always pass.
//! - Quotes are declared, not commanded: the full desired order set is
//!   emitted every round and the OMS reconciles (unchanged ⇒ no traffic).
//! - Risk halts (loss stop, unmatched-exposure limit) cancel everything and
//!   silence the maker for the rest of the window; the pre-expiry cutoff
//!   pulls quotes and holds inventory to settlement.

use std::collections::HashMap;

use crate::config::{Config, MakerCfg, TickCfg};
use crate::position::{calc_fee, MAKER_FEE_DISCOUNT};
use crate::strategy::{FillInfo, Snap};
use crate::types::{Action, BookTop, OrderTag, Side, TargetOrder, TokenId};

/// One physical order to rest (always a BUY — see module doc).
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub token: TokenId,
    pub side: Side,
    pub px: f64,
    pub sz: f64,
    pub tag: OrderTag,
}

/// The logical YES-side quote in probability space, before the buy-only
/// mapping to physical orders. NaN on a side means that side is not quoted
/// (price rounded outside (0, 1)).
struct LogicalQuote {
    bid: f64,
    ask: f64,
}

/// Multiplicative spread-widening factors, each ≥ 1.
struct SpreadFactors {
    /// Time decay: spreads widen as expiry approaches (gamma risk).
    tte: f64,
    /// Inventory skew: widen when net inventory builds.
    skew: f64,
    /// Momentum: widen when the underlying is trending past the threshold.
    momentum: f64,
}

impl SpreadFactors {
    /// Base half-spread scaled by all factors, clamped to the configured
    /// spread band. Fee widening is deliberately NOT part of this — it is
    /// added after the clamp (cost recovery, not spread).
    fn half_spread(&self, cfg: &MakerCfg) -> f64 {
        (cfg.half_spread * self.tte * self.skew * self.momentum)
            .clamp(cfg.min_spread / 2.0, cfg.max_spread / 2.0)
    }
}

impl std::fmt::Display for SpreadFactors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tte×{:.3} inv×{:.3} mom×{:.3}",
            self.tte, self.skew, self.momentum
        )
    }
}

/// Inter-tick memory: everything the maker remembers between quote rounds,
/// stamped once per emitted round and reset at window start.
#[derive(Default)]
struct RequoteState {
    last_quote_ts: f64,
    /// Center at the last emitted round (pre-skew — drift is measured on the
    /// raw alpha-adjusted center, not the inventory-shifted one).
    last_center: Option<f64>,
    last_spot_at_quote: Option<f64>,
    fill_since_quote: bool,
    has_quotes: bool,
}

impl RequoteState {
    fn stamp(&mut self, snap: &Snap, center: f64, any_quotes: bool) {
        self.last_quote_ts = snap.now;
        self.last_center = Some(center);
        self.last_spot_at_quote = snap.spot;
        self.fill_since_quote = false;
        self.has_quotes = any_quotes;
    }
}

/// Per quote key: last emitted price and when it last moved AGGRESSIVELY
/// (bid up / ask down — toward the market).
#[derive(Clone, Copy)]
struct QuoteHist {
    px: f64,
    last_aggressive_at: f64,
}

/// Sentinel for "never amended aggressively" — any interval has elapsed.
const NEVER: f64 = f64::NEG_INFINITY;

pub struct Maker {
    cfg: MakerCfg,
    tick: TickCfg,
    halted: Option<String>,
    requote: RequoteState,
    quote_hist: HashMap<(TokenId, Side), QuoteHist>,
}

impl Maker {
    pub fn new(cfg: &Config) -> Self {
        Self {
            cfg: cfg.maker.clone(),
            tick: cfg.tick,
            halted: None,
            requote: RequoteState::default(),
            quote_hist: HashMap::new(),
        }
    }

    pub fn on_window_start(&mut self) {
        self.halted = None;
        self.requote = RequoteState::default();
        self.quote_hist.clear();
    }

    pub fn is_halted(&self) -> bool {
        self.halted.is_some()
    }

    pub fn on_fill(&mut self, _fill: &FillInfo, _snap: &Snap) -> Vec<Action> {
        self.requote.fill_since_quote = true;
        Vec::new()
    }

    /// Main tick: called on the 100 ms alpha tick, book updates, and the 1 s
    /// timer. A gauntlet of guards — order matters: risk halts before the
    /// cutoff before the requote triggers — then one declared quote round.
    pub fn on_tick(&mut self, snap: &Snap, interval: f64) -> Vec<Action> {
        if self.halted.is_some() {
            return Vec::new();
        }
        if let Some(actions) = self.check_risk_halts(snap) {
            return actions;
        }

        // Pre-expiry cutoff: pull quotes once, hold inventory to settlement.
        if snap.tte <= self.cfg.pre_expiry_cutoff_secs {
            if self.requote.has_quotes {
                self.requote.has_quotes = false;
                return vec![Action::CancelAll];
            }
            return Vec::new();
        }

        let Some(center) = Self::quote_center(snap) else {
            return Vec::new();
        };
        if !self.requote_due(snap, center) {
            return Vec::new();
        }

        // Declare the full desired order set per token; the OMS reconciles:
        // unchanged quotes produce zero traffic, price/qty changes cancel
        // the old order first and place the new one.
        let quotes = self.compute_quotes(snap, interval);
        let actions = self.targets_from(&quotes, snap);
        self.requote.stamp(snap, center, !quotes.is_empty());
        actions
    }

    pub fn on_expiry(&mut self) -> Vec<Action> {
        self.requote.has_quotes = false;
        vec![Action::CancelAll]
    }

    // ── guards ───────────────────────────────────────────────────────────────

    /// Loss stop and unmatched-exposure limit. Either trips once, cancels
    /// everything, and silences the maker for the rest of the window.
    fn check_risk_halts(&mut self, snap: &Snap) -> Option<Vec<Action>> {
        let pnl = self.window_pnl(snap);
        if pnl < -self.cfg.max_loss_per_market {
            let why = format!(
                "loss stop: window pnl {pnl:.4} < -{}",
                self.cfg.max_loss_per_market
            );
            return Some(self.halt(why));
        }
        let exposure = self.unmatched_exposure(snap);
        if exposure > self.cfg.gross_exposure_limit {
            let why = format!(
                "gross exposure {exposure:.2} > limit {}",
                self.cfg.gross_exposure_limit
            );
            return Some(self.halt(why));
        }
        None
    }

    fn halt(&mut self, why: String) -> Vec<Action> {
        tracing::warn!("[MM] HALT — {why}");
        self.halted = Some(why.clone());
        self.requote.has_quotes = false;
        vec![Action::CancelAll, Action::Halt(why)]
    }

    /// Requote when any trigger fires: periodic refresh due, center drifted
    /// beyond `max_quote_drift` (or first quote of the window), spot moved
    /// beyond `reprice_threshold`, or a fill landed since the last round.
    fn requote_due(&self, snap: &Snap, center: f64) -> bool {
        let r = &self.requote;
        let refresh_due = snap.now - r.last_quote_ts >= self.cfg.quote_refresh_secs;
        let center_drift = r
            .last_center
            .map(|c| (center - c).abs() > self.cfg.max_quote_drift)
            .unwrap_or(true);
        let spot_move = match (snap.spot, r.last_spot_at_quote) {
            (Some(s), Some(prev)) if prev > 0.0 => {
                ((s - prev) / prev).abs() > self.cfg.reprice_threshold
            }
            _ => false,
        };
        refresh_due || center_drift || spot_move || r.fill_since_quote
    }

    // ── pricing: snapshot → logical YES quote ────────────────────────────────

    /// The quote center: alpha-adjusted fair when available, raw fair
    /// otherwise. None until pricing is primed.
    fn quote_center(snap: &Snap) -> Option<f64> {
        snap.quote_center_yes.or(snap.fair_yes)
    }

    /// Net inventory in YES-equivalent terms (matched pairs cancel out).
    fn net_inventory(snap: &Snap) -> f64 {
        snap.yes.pos - snap.no.pos
    }

    /// Spread-widening factors for the current state; combined and clamped
    /// by [`SpreadFactors::half_spread`].
    fn spread_factors(
        &self,
        tte: f64,
        interval: f64,
        inv_ratio: f64,
        mom_ret: Option<f64>,
    ) -> SpreadFactors {
        SpreadFactors {
            tte: 1.0 + (1.0 - (tte / interval).clamp(0.0, 1.0)) * self.cfg.tte_spread_multiplier,
            skew: 1.0 + inv_ratio.abs().min(1.0) * self.cfg.skew_spread_multiplier,
            momentum: match mom_ret {
                Some(r) if self.cfg.momentum_threshold > 0.0 => {
                    1.0 + self.cfg.momentum_spread_multiplier
                        * ((r.abs() / self.cfg.momentum_threshold) - 1.0).max(0.0)
                }
                _ => 1.0,
            },
        }
    }

    /// Effective half-spread with tte / inventory-skew / momentum widening.
    /// `inv_ratio` is net inventory over max inventory, in [-1, 1].
    pub fn half_spread(&self, tte: f64, interval: f64, inv_ratio: f64, mom_ret: Option<f64>) -> f64 {
        self.spread_factors(tte, interval, inv_ratio, mom_ret)
            .half_spread(&self.cfg)
    }

    /// Price the logical YES quote: skewed center ± (half-spread + fee).
    /// None when pricing is unprimed or the rounded quote is degenerate
    /// (bid ≥ ask).
    fn price_logical(&self, snap: &Snap, interval: f64) -> Option<LogicalQuote> {
        let center_raw = Self::quote_center(snap)?;
        let inv_ratio = (Self::net_inventory(snap) / self.cfg.max_inventory).clamp(-1.0, 1.0);
        let factors = self.spread_factors(snap.tte, interval, inv_ratio, snap.momentum_ret);
        let half = factors.half_spread(&self.cfg);

        // Inventory skew shifts the whole quote ladder against inventory.
        let center = (center_raw - inv_ratio * self.cfg.skew_shift).clamp(0.01, 0.99);

        // Widen each side by the per-token maker fee at the center so the
        // quoted edge is NET of fees. Applied after the max-spread clamp on
        // purpose: fee compensation is cost recovery, not spread.
        let fee_unit = calc_fee(center, 1.0) * MAKER_FEE_DISCOUNT * self.cfg.fee_spread_factor;
        let half = half + fee_unit;
        tracing::debug!(
            "pricing: center {center_raw:.4} -> {center:.4} (skew), half {half:.4} [{factors} + fee {fee_unit:.4}]"
        );

        // Round DOWN for the bid, UP for the ask; a rounded price outside
        // (0, 1) is NaN — that side is simply not quoted.
        let bid = self.round_quote(center - half, Side::Buy);
        let ask = self.round_quote(center + half, Side::Sell);
        if bid.is_finite() && ask.is_finite() && bid >= ask {
            return None; // degenerate — don't quote crossed
        }
        Some(LogicalQuote { bid, ask })
    }

    // ── emission: logical quote → physical BUY orders ────────────────────────

    /// Buy-only mapping (never submit SELLs):
    ///   BUY YES @ bid       <=> the logical YES bid.
    ///   BUY NO  @ 1 - ask   <=> the logical YES ask (selling YES at ask).
    /// The NO token's own logical quotes collapse into these two, and
    /// matched YES/NO pairs settle risk-free at $1. Eligibility applied
    /// here: inventory caps and the post-only cross guard.
    fn to_physical_orders(&self, snap: &Snap, lq: &LogicalQuote) -> Vec<Quote> {
        let inv_net = Self::net_inventory(snap);
        let sz = self.cfg.quote_size;
        let long_capped = inv_net >= self.cfg.max_inventory; // stop adding net-long
        let short_capped = inv_net <= -self.cfg.max_inventory; // stop adding net-short

        // Post-only guard: a BUY at or above the token's best ask would
        // execute as a taker — skip it when maker_only is set. An absent
        // ask side cannot be crossed, so the quote is allowed.
        let crosses = |top: Option<BookTop>, px: f64| -> bool {
            self.cfg.maker_only
                && top.and_then(|t| t.ask).map(|ask| px >= ask).unwrap_or(false)
        };

        let mut quotes = Vec::new();
        if !long_capped && lq.bid.is_finite() && !crosses(snap.yes.top, lq.bid) {
            quotes.push(Quote {
                token: snap.yes.token.clone(),
                side: Side::Buy,
                px: lq.bid,
                sz,
                tag: OrderTag::QuoteBid,
            });
        }
        if !short_capped {
            // NaN ask propagates: 1 - NaN = NaN -> no NO quote either.
            let no_px = self.round_quote(1.0 - lq.ask, Side::Buy);
            if no_px.is_finite() && !crosses(snap.no.top, no_px) {
                quotes.push(Quote {
                    token: snap.no.token.clone(),
                    side: Side::Buy,
                    px: no_px,
                    sz,
                    tag: OrderTag::QuoteBid,
                });
            }
        }
        quotes
    }

    /// Compute the target quote set for the current snapshot.
    pub fn compute_quotes(&self, snap: &Snap, interval: f64) -> Vec<Quote> {
        match self.price_logical(snap, interval) {
            Some(lq) => self.to_physical_orders(snap, &lq),
            None => Vec::new(),
        }
    }

    /// Run each quote through the aggressive-amend limiter and group into
    /// one `Action::Targets` per token (both always emitted — an empty
    /// target set tells the OMS to clear that token's quotes).
    fn targets_from(&mut self, quotes: &[Quote], snap: &Snap) -> Vec<Action> {
        let mut yes_orders: Vec<TargetOrder> = Vec::new();
        let mut no_orders: Vec<TargetOrder> = Vec::new();
        for q in quotes {
            let px = self.limit_aggressive(&q.token, q.side, q.px, snap.now);
            let t = TargetOrder {
                side: q.side,
                px,
                sz: q.sz,
                tag: q.tag,
            };
            if q.token == snap.yes.token {
                yes_orders.push(t);
            } else {
                no_orders.push(t);
            }
        }
        vec![
            Action::Targets {
                token: snap.yes.token.clone(),
                orders: yes_orders,
            },
            Action::Targets {
                token: snap.no.token.clone(),
                orders: no_orders,
            },
        ]
    }

    /// Tick-round a quote price (bids down, asks up). Returns NaN when the
    /// rounded price is <= 0 or >= 1 — such a quote must not be placed.
    fn round_quote(&self, px: f64, side: Side) -> f64 {
        if !px.is_finite() {
            return f64::NAN;
        }
        let r = self.tick.round(px, side);
        if r <= 0.0 || r >= 1.0 {
            f64::NAN
        } else {
            r
        }
    }

    /// Rate-limit AGGRESSIVE amends: a bid moving up (or ask moving down)
    /// more than once per `aggressive_amend_interval_secs` keeps its
    /// previous price for this round instead. Passive moves (away from the
    /// market) and first-time quotes always pass.
    fn limit_aggressive(&mut self, token: &TokenId, side: Side, px: f64, now: f64) -> f64 {
        let interval = self.cfg.aggressive_amend_interval_secs;
        let key = (token.clone(), side);
        if interval <= 0.0 {
            self.quote_hist.insert(key, QuoteHist { px, last_aggressive_at: now });
            return px;
        }
        let Some(prev) = self.quote_hist.get(&key).copied() else {
            // First quote for this key: not an amend.
            self.quote_hist.insert(key, QuoteHist { px, last_aggressive_at: NEVER });
            return px;
        };
        let prev_px = prev.px;
        let aggressive = match side {
            Side::Buy => px > prev_px + 1e-12,
            Side::Sell => px < prev_px - 1e-12,
        };
        if aggressive && now - prev.last_aggressive_at < interval {
            tracing::debug!(
                "aggressive amend blocked: {side} {prev_px:.4} -> {px:.4} (min {interval}s), keeping {prev_px:.4}"
            );
            return prev_px; // hold the previous price this round
        }
        let stamp = if aggressive { now } else { prev.last_aggressive_at };
        self.quote_hist.insert(key, QuoteHist { px, last_aggressive_at: stamp });
        px
    }

    // ── risk metrics ─────────────────────────────────────────────────────────

    /// Window PnL for the loss stop: realized + unrealized-at-mid − fees,
    /// with matched YES/NO pairs marked at their locked-in value. An empty
    /// book falls back to marking at entry — conservatively flat, so a
    /// missing book can never trip (or mask) the loss stop by itself.
    fn window_pnl(&self, snap: &Snap) -> f64 {
        let y = &snap.yes;
        let n = &snap.no;
        let matched = y.pos.min(n.pos);
        let locked = if matched > 0.0 {
            (1.0 - y.avg_entry - n.avg_entry) * matched
        } else {
            0.0
        };
        let y_mark = y.top.and_then(|t| t.mid()).unwrap_or(y.avg_entry);
        let n_mark = n.top.and_then(|t| t.mid()).unwrap_or(n.avg_entry);
        let y_unreal = (y_mark - y.avg_entry) * (y.pos - matched);
        let n_unreal = (n_mark - n.avg_entry) * (n.pos - matched);
        y.realized_pnl + n.realized_pnl + locked + y_unreal + n_unreal
            - y.total_fees
            - n.total_fees
    }

    /// Exposure net of matched pairs — pairs are risk-free ($1 at expiry),
    /// so only the unmatched remainder counts against the limit.
    fn unmatched_exposure(&self, snap: &Snap) -> f64 {
        Self::net_inventory(snap).abs()
    }
}

#[cfg(test)]
mod tests;
