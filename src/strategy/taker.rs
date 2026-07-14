//! Directional taker strategy — port of `btc_trader.py`'s `_process_side`
//! lifecycle: passive buy at bid, TP re-place after fill, stop-loss,
//! cooldown, hourly overrides, pre-expiry cutoff.
//!
//! Each side (YES / NO) runs one state machine:
//!
//! ```text
//!  New ──entry fill──▶ BuyFilled ──book tick──▶ TakeProfit ──close fill──▶ New (+cooldown)
//!                         │   ▲                     │            (pair netted by engine)
//!                         │   └── close order vanished ┘   (re-arm recovery)
//!                         └──── bid < stop threshold ────▶ StopLoss ──close fill──▶ New (+cooldown)
//!                                (fires even in cutoff)
//! ```
//!
//! CLOSES ARE BUYS OF THE OPPOSITE TOKEN at the complement price
//! (no = 1 − yes): selling the held token requires it to be SETTLED
//! on-chain (1–3 s after a matched buy — sells placed earlier are rejected
//! with "balance 0"), while buying only needs USDC. A filled close makes a
//! matched YES+NO pair that redeems for exactly $1, so the economics equal
//! a sell at the target price; the engine nets pairs into realized PnL.
//!
//! Invariants:
//! - No SELL is ever submitted: every close is a BUY of the opposite token.
//! - The stop-loss outranks everything — it fires even during the
//!   pre-expiry cutoff (and the engine exempts it from the order throttle
//!   and the user-ws gate).
//! - An open position always has a close resting: if the close order
//!   vanishes (rejected/cancelled), recovery re-arms to BuyFilled and the
//!   same drive cycle re-places it.
//! - Cooldown after a close blocks re-entry ABOVE the last buy price only —
//!   chase prevention, not a hard pause.
//!
//! Fill simulation lives in the DryRun executor; this state machine only ever
//! sees confirmed fills, so DRY_RUN and LIVE share one code path.

use crate::config::{Config, TakerCfg, TickCfg};
use crate::strategy::{FillInfo, SideSnap, Snap};
use crate::types::{Action, Outcome, OrderTag, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifeCycle {
    New,
    BuyFilled,
    TakeProfit,
    StopLoss,
}

#[derive(Debug)]
struct SideState {
    lifecycle: LifeCycle,
    cooldown_until: f64,
    last_buy_price: f64,
}

impl Default for SideState {
    fn default() -> Self {
        Self {
            lifecycle: LifeCycle::New,
            cooldown_until: 0.0,
            last_buy_price: 0.0,
        }
    }
}

/// Entry-gating conditions, computed once per book tick and consumed by
/// both the resting-order management and the new-entry decision.
struct EntryGate {
    /// Side allowed this hour (hourly override) AND cutoff not reached.
    side_enabled: bool,
    /// Bid within [order_price_min (hourly), order_price_max].
    in_range: bool,
    in_cooldown: bool,
    /// Chase prevention: during cooldown a re-buy is allowed only at or
    /// below the last buy price — never chasing the market up after a
    /// close.
    cooldown_buy_allowed: bool,
}

pub struct Taker {
    cfg: TakerCfg,
    tick: TickCfg,
    yes: SideState,
    no: SideState,
}

impl Taker {
    pub fn new(cfg: &Config) -> Self {
        Self {
            cfg: cfg.taker.clone(),
            tick: cfg.tick,
            yes: SideState::default(),
            no: SideState::default(),
        }
    }

    pub fn lifecycle(&self, o: Outcome) -> LifeCycle {
        self.state(o).lifecycle
    }

    fn state(&self, o: Outcome) -> &SideState {
        match o {
            Outcome::Yes => &self.yes,
            Outcome::No => &self.no,
        }
    }

    fn state_mut(&mut self, o: Outcome) -> &mut SideState {
        match o {
            Outcome::Yes => &mut self.yes,
            Outcome::No => &mut self.no,
        }
    }

    fn transition(&mut self, o: Outcome, to: LifeCycle) {
        let st = self.state_mut(o);
        if st.lifecycle != to {
            tracing::info!("[{}] lifecycle {:?} -> {:?}", o.label(), st.lifecycle, to);
            st.lifecycle = to;
        }
    }

    /// New market window: clear per-window state.
    pub fn on_window_start(&mut self) {
        self.yes = SideState::default();
        self.no = SideState::default();
    }

    /// Confirmed fill on one of our orders.
    pub fn on_fill(&mut self, fill: &FillInfo, snap: &Snap) -> Vec<Action> {
        match fill.tag {
            OrderTag::EntryBuy => {
                self.transition(fill.outcome, LifeCycle::BuyFilled);
            }
            OrderTag::TakeProfit | OrderTag::StopLoss => {
                // The close is a BUY of the OPPOSITE token; the engine nets
                // the matched pair before this callback, so the original
                // side's position is already reduced. Re-arm once flat.
                let orig = fill.outcome.other();
                let lc = self.state(orig).lifecycle;
                if matches!(lc, LifeCycle::StopLoss | LifeCycle::TakeProfit)
                    && snap.side(orig).pos <= 1e-9
                {
                    self.transition(orig, LifeCycle::New);
                    self.state_mut(orig).cooldown_until = snap.now + self.cfg.cooldown_secs;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Book update (or 1s timer re-poll) for one outcome. A gauntlet whose
    /// ORDER is the design: the stop-loss outranks everything (fires even
    /// during the cutoff); recovery re-arms BEFORE the TP block so a
    /// vanished close is re-placed in this same cycle; entry management
    /// runs only when nothing needs closing.
    pub fn on_book(&mut self, o: Outcome, snap: &Snap) -> Vec<Action> {
        let s = snap.side(o).clone();
        let Some(top) = s.top else {
            return Vec::new();
        };
        let bid = top.bid;
        let opp = snap.side(o.other()).clone();

        if let Some(actions) = self.check_stop_loss(o, &s, &opp, bid) {
            return actions;
        }
        self.recover_missing_close(o, &s, &opp);
        if let Some(actions) = self.place_take_profit(o, &s, &opp) {
            return actions;
        }
        let gate = self.entry_gate(o, snap, bid);
        if let Some(actions) = self.manage_resting_entry(o, &s, bid, &gate) {
            return actions;
        }
        self.try_new_entry(o, &s, bid, &gate)
    }

    /// Market expiry: cancel everything (engine also settles + logs PnL).
    pub fn on_expiry(&mut self) -> Vec<Action> {
        vec![Action::CancelAll]
    }

    // ── closes: sell expressed as an opposite-token buy ──────────────────────

    /// Complement price for closing at `sell_px`: selling this token at
    /// `sell_px` ⇔ buying the opposite token at `1 − sell_px` (tick-rounded
    /// down, as a buy). Needs only USDC — no wait for the entry tokens to
    /// settle on-chain.
    fn complement_close_px(&self, sell_px: f64) -> f64 {
        self.tick.round(1.0 - sell_px, Side::Buy)
    }

    /// Stop-loss: bid dropped below the threshold with a position open.
    /// Fires from BuyFilled OR TakeProfit (yanking the resting TP), and is
    /// deliberately checked before the cutoff-gated entry logic — risk
    /// comes off at ANY point in the window.
    fn check_stop_loss(
        &mut self,
        o: Outcome,
        s: &SideSnap,
        opp: &SideSnap,
        bid: Option<f64>,
    ) -> Option<Vec<Action>> {
        let bid = bid?;
        if s.pos <= 0.0
            || bid >= self.cfg.stop_loss_price
            || !matches!(
                self.state(o).lifecycle,
                LifeCycle::BuyFilled | LifeCycle::TakeProfit
            )
        {
            return None;
        }
        let px = self.complement_close_px(bid);
        tracing::warn!(
            "[{}] STOP-LOSS @ bid={bid:.4} (entry={:.4}, threshold={}) -> BUY {} @ {px:.4}",
            o.label(),
            s.avg_entry,
            self.cfg.stop_loss_price,
            o.other().label(),
        );
        // Cancel any resting orders on BOTH tokens (entry leftovers + a
        // resting TP) BEFORE the stop order — unlike the TP, which only
        // clears its own side, the stop must not race a stale close.
        self.transition(o, LifeCycle::StopLoss);
        Some(vec![
            Action::CancelToken(s.token.clone()),
            Action::CancelToken(opp.token.clone()),
            Action::Place {
                token: opp.token.clone(),
                side: Side::Buy,
                px,
                sz: s.pos,
                tag: OrderTag::StopLoss,
            },
        ])
    }

    /// Recovery: the close order (a BUY resting on the OPPOSITE token)
    /// vanished (rejected or cancelled) while a position remains — go back
    /// to BuyFilled so the TP block below re-places it this same cycle.
    fn recover_missing_close(&mut self, o: Outcome, s: &SideSnap, opp: &SideSnap) {
        if matches!(
            self.state(o).lifecycle,
            LifeCycle::TakeProfit | LifeCycle::StopLoss
        ) && s.pos > 0.0
            && opp.working_orders == 0
        {
            tracing::warn!(
                "[{}] close order missing with open position — re-arming",
                o.label()
            );
            self.transition(o, LifeCycle::BuyFilled);
        }
    }

    /// Place the TP after a confirmed buy: sell at `entry + take_profit`
    /// expressed as the complement buy. Returns Some (ending the tick) for
    /// any position that needs a close — even when the TP is inexpressible
    /// (px rounds to 0) — so entry logic never runs with a position open.
    fn place_take_profit(
        &mut self,
        o: Outcome,
        s: &SideSnap,
        opp: &SideSnap,
    ) -> Option<Vec<Action>> {
        if self.state(o).lifecycle != LifeCycle::BuyFilled || s.pos <= 0.0 {
            return None;
        }
        let tp = (s.avg_entry + self.cfg.take_profit_price).min(0.99);
        let px = self.complement_close_px(tp);
        if px <= 0.0 {
            return Some(Vec::new()); // tp too close to 1.0 to express
        }
        self.transition(o, LifeCycle::TakeProfit);
        Some(vec![
            // Clear this side's entry leftovers only; the opposite token may
            // carry the other side's independent orders.
            Action::CancelToken(s.token.clone()),
            Action::Place {
                token: opp.token.clone(),
                side: Side::Buy,
                px,
                sz: s.pos,
                tag: OrderTag::TakeProfit,
            },
        ])
    }

    // ── entries ──────────────────────────────────────────────────────────────

    fn entry_gate(&self, o: Outcome, snap: &Snap, bid: Option<f64>) -> EntryGate {
        let st = self.state(o);
        let in_cooldown = snap.now < st.cooldown_until;
        let cooldown_buy_allowed =
            in_cooldown && bid.map(|b| b <= st.last_buy_price).unwrap_or(false);
        let (sides, eff_price_min) = self.cfg.hourly_cfg(snap.hour_utc);
        let side_enabled =
            snap.trading_enabled && sides.iter().any(|x| x.eq_ignore_ascii_case(o.label()));
        let in_range = bid
            .map(|b| b >= eff_price_min && b <= self.cfg.order_price_max)
            .unwrap_or(false);
        EntryGate {
            side_enabled,
            in_range,
            in_cooldown,
            cooldown_buy_allowed,
        }
    }

    /// Manage a resting entry buy: cancel it when the gate closed (stale),
    /// cancel-and-replace when the bid moved, leave it alone otherwise.
    /// Returns Some whenever a resting buy exists — the new-entry block
    /// never runs concurrently with one.
    fn manage_resting_entry(
        &mut self,
        o: Outcome,
        s: &SideSnap,
        bid: Option<f64>,
        gate: &EntryGate,
    ) -> Option<Vec<Action>> {
        if self.state(o).lifecycle != LifeCycle::New || s.pending_buy <= 0.0 {
            return None;
        }
        let stale = !gate.side_enabled || !gate.in_range || s.pos >= self.cfg.max_position;
        let moved = match (bid, s.resting_buy_px) {
            (Some(b), Some(rp)) => (b - rp).abs() > 1e-9,
            _ => false,
        };
        if stale {
            tracing::info!("[{}] cancelling stale buy (out of range/cutoff/cap)", o.label());
            return Some(vec![Action::CancelToken(s.token.clone())]);
        }
        if moved {
            // Cancel-and-replace at the new bid, same tick. The resting
            // order dies in the same batch, so it must NOT count against
            // max_position here — size from the FILLED position only
            // (unlike a new entry, which counts the pending buy too).
            let b = bid.unwrap();
            let sz = (self.cfg.order_size).min(self.cfg.max_position - s.pos);
            let mut actions = Vec::new();
            if sz > 0.0 {
                tracing::info!(
                    "[{}] refreshing buy {:.4} -> {b:.4}",
                    o.label(),
                    s.resting_buy_px.unwrap_or(0.0)
                );
                actions.push(Action::CancelToken(s.token.clone()));
                actions.push(Action::Place {
                    token: s.token.clone(),
                    side: Side::Buy,
                    px: self.tick.round(b, Side::Buy),
                    sz,
                    tag: OrderTag::EntryBuy,
                });
                self.state_mut(o).last_buy_price = b;
            }
            return Some(actions);
        }
        Some(Vec::new()) // resting buy still good
    }

    /// Place a fresh entry buy at the bid when every gate is open.
    fn try_new_entry(
        &mut self,
        o: Outcome,
        s: &SideSnap,
        bid: Option<f64>,
        gate: &EntryGate,
    ) -> Vec<Action> {
        // A resting buy counts toward the cap — never double-buy.
        let effective_pos = s.pos + s.pending_buy;
        if self.state(o).lifecycle != LifeCycle::New
            || s.pending_buy > 0.0
            || !gate.side_enabled
            || (gate.in_cooldown && !gate.cooldown_buy_allowed)
            || !gate.in_range
            || effective_pos >= self.cfg.max_position
        {
            return Vec::new();
        }
        let b = bid.unwrap(); // in_range implies Some
        let sz = self.cfg.order_size.min(self.cfg.max_position - effective_pos);
        self.state_mut(o).last_buy_price = b;
        vec![Action::Place {
            token: s.token.clone(),
            side: Side::Buy,
            px: self.tick.round(b, Side::Buy),
            sz,
            tag: OrderTag::EntryBuy,
        }]
    }
}

#[cfg(test)]
mod tests;
