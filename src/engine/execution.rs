//! The order chokepoint: throttle policy, action execution, the
//! per-token cap, target reconciliation, and the executor facade —
//! the ONLY code that knows DryRun from Live.

use super::*;

/// Minimum interval between order placements per (token, side).
/// Stop-loss orders are exempt — they must never wait behind a rate limit.
pub(super) struct PlaceThrottle {
    min_interval: f64, // seconds; <= 0 disables
    last: HashMap<(TokenId, Side), f64>,
}

impl PlaceThrottle {
    pub(super) fn new(throttle_ms: u64) -> Self {
        Self {
            min_interval: throttle_ms as f64 / 1000.0,
            last: HashMap::new(),
        }
    }

    /// Returns true if a placement is allowed now (and reserves the slot).
    pub(super) fn allow(&mut self, token: &TokenId, side: Side, tag: OrderTag, now: f64) -> bool {
        if self.min_interval <= 0.0 || tag == OrderTag::StopLoss {
            return true;
        }
        let key = (token.clone(), side);
        if let Some(t) = self.last.get(&key) {
            if now - t < self.min_interval {
                return false;
            }
        }
        self.last.insert(key, now);
        true
    }

    pub(super) fn reset(&mut self) {
        self.last.clear();
    }
}

/// Outcome of the throttle pass over one action batch: which placements
/// may go out now, and which token-level cancels must be skipped because
/// ALL their replacements were throttled.
struct ThrottleVerdict {
    allowed: Vec<bool>,
    skip_cancels: std::collections::HashSet<TokenId>,
}

impl Engine {
    /// Hand one order to the active executor. Returns Some(exchange_id)
    /// when the executor acked synchronously (DryRun); live acks arrive
    /// later as Event::OrderAck.
    fn exec_place(
        &mut self,
        client_id: u64,
        token: &TokenId,
        side: Side,
        px: f64,
        sz: f64,
        tag: OrderTag,
    ) -> Option<String> {
        if self.cfg.general.dry_run {
            return Some(self.exec_dry.place(client_id, token, side, px, sz, tag));
        }
        #[cfg(feature = "live")]
        if let Some(exec) = &self.exec_live {
            let exec = exec.clone();
            let tx = self.tx.clone();
            let token = token.clone();
            tokio::task::spawn_local(async move {
                let result = exec.place(&token, side, px, sz).await;
                let _ = tx.send(Event::OrderAck { client_id, result });
            });
        }
        None
    }

    /// Cancel specific orders by exchange id.
    pub(super) fn exec_cancel_ids(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        if self.cfg.general.dry_run {
            for id in &ids {
                self.exec_dry.cancel_order(id);
            }
            return;
        }
        #[cfg(feature = "live")]
        if let Some(exec) = &self.exec_live {
            let exec = exec.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = exec.cancel_orders(&ids).await {
                    tracing::warn!("cancel_orders failed: {e}");
                }
            });
        }
        #[cfg(not(feature = "live"))]
        let _ = ids;
    }

    /// Cancel everything resting on one token (`ids` are the acked orders;
    /// the DryRun executor cancels by token directly).
    fn exec_cancel_token(&mut self, token: &str, ids: Vec<String>) {
        if self.cfg.general.dry_run {
            self.exec_dry.cancel_token(token);
            return;
        }
        self.exec_cancel_ids(ids);
    }

    /// Fire-and-forget cancel-all (the awaited variant for shutdown is
    /// cancel_all_and_wait).
    fn exec_cancel_all(&mut self) {
        if self.cfg.general.dry_run {
            self.exec_dry.cancel_all();
            return;
        }
        #[cfg(feature = "live")]
        if let Some(exec) = &self.exec_live {
            let exec = exec.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = exec.cancel_all().await {
                    tracing::warn!("cancel_all failed: {e}");
                }
            });
        }
    }

    fn throttle_pass(&mut self, actions: &[Action], now: f64) -> ThrottleVerdict {
        let mut allowed = vec![true; actions.len()];
        let mut any_place: std::collections::HashSet<TokenId> = Default::default();
        let mut any_allowed: std::collections::HashSet<TokenId> = Default::default();
        for (i, a) in actions.iter().enumerate() {
            if let Action::Place { token, side, tag, .. } = a {
                let ok = self.throttle.allow(token, *side, *tag, now);
                allowed[i] = ok;
                any_place.insert(token.clone());
                if ok {
                    any_allowed.insert(token.clone());
                }
            }
        }
        // "Never cancel into nothing": if EVERY replacement for a token was
        // throttled, its cancel is skipped too and the old order keeps
        // resting.
        let skip_cancels = &any_place - &any_allowed;
        ThrottleVerdict { allowed, skip_cancels }
    }

    pub(super) fn execute(&mut self, actions: Vec<Action>) {
        let now = self.now();
        let verdict = self.throttle_pass(&actions, now);

        for (i, a) in actions.into_iter().enumerate() {
            match a {
                Action::Place { token, side, px, sz, tag } => {
                    if !verdict.allowed[i] {
                        tracing::debug!(
                            "throttled [{tag}] {side} {sz}@{px:.4} (min {}ms per token/side)",
                            self.cfg.general.order_throttle_ms
                        );
                        continue;
                    }
                    self.submit_place(&token, side, px, sz, tag);
                }
                Action::Targets { token, orders } => {
                    self.reconcile_targets(&token, orders, now);
                }
                Action::CancelToken(token) => {
                    if verdict.skip_cancels.contains(&token) {
                        tracing::debug!(
                            "skipping cancel on {}… (replacements throttled)",
                            &token[..token.len().min(8)]
                        );
                        continue;
                    }
                    let ids = self.oms.cancel_token(&token);
                    if !ids.is_empty() {
                        tracing::info!("[CANCEL] {} order(s) on {}", ids.len(), self.token_label(&token));
                    }
                    self.exec_cancel_token(&token, ids);
                }
                Action::CancelAll => {
                    let _ids = self.oms.cancel_all();
                    self.exec_cancel_all();
                }
                Action::Halt(why) => {
                    tracing::warn!("strategy halted for this window: {why}");
                    self.halted = true;
                }
            }
        }
    }

    /// Place one order, enforcing the per-token working-order cap.
    fn submit_place(&mut self, token: &TokenId, side: Side, px: f64, sz: f64, tag: OrderTag) {
        if px <= 0.0 || px >= 1.0 || sz <= 0.0 {
            tracing::warn!("skipping degenerate order {side} {sz}@{px}");
            return;
        }
        if !self.user_ws_ready && tag != OrderTag::StopLoss {
            tracing::warn!(
                "user channel not connected — dropping [{tag}] {side} {sz}@{px:.4} (fills would be missed)"
            );
            return;
        }
        let cap = self.cfg.general.max_orders_per_token;
        if self.oms.working_count(token) >= cap {
            tracing::warn!(
                "max_orders_per_token ({cap}) reached on {}… — dropping [{tag}] {side} {sz}@{px:.4}",
                &token[..token.len().min(8)]
            );
            return;
        }
        if let Some(t0) = self.cycle_start.take() {
            // First order of this engine wake: wake -> order handed off.
            self.lat.tick_to_order.record(t0.elapsed());
        }
        let label = self.token_label(token);
        let client_id = self.oms.submit(token, side, px, sz, tag);
        match self.exec_place(client_id, token, side, px, sz, tag) {
            Some(eid) => {
                tracing::info!("[DRY-RUN] [{tag}] {side} {sz} {label} @ {px:.4} ({eid})");
                let _ = self.oms.on_ack(client_id, &Ok(eid));
            }
            None => {
                tracing::info!("[ORDER] [{tag}] {side} {sz} {label} @ {px:.4}");
            }
        }
    }

    /// Reconcile the working orders on one token toward the declared target
    /// set. Matching is per (side, tag):
    ///   - price AND qty unchanged        -> no action;
    ///   - price or qty changed           -> cancel the old order FIRST, then
    ///     place the new one (skipped entirely if throttled: old order kept);
    ///   - working order with no target   -> cancel;
    ///   - target with no working order   -> place.
    fn reconcile_targets(&mut self, token: &TokenId, targets: Vec<crate::types::TargetOrder>, now: f64) {
        /// Just the fields the cancel pass needs — the common no-change
        /// round clones nothing.
        struct CancelCandidate {
            client_id: u64,
            side: Side,
            tag: OrderTag,
            remaining: f64,
            px: f64,
            exchange_id: Option<String>,
        }
        let mut matched: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut to_cancel: Vec<CancelCandidate> = Vec::new();
        let mut to_place: Vec<crate::types::TargetOrder> = Vec::new();
        {
            let working = self.oms.working_orders(token);
            for t in targets {
                let existing = working
                    .iter()
                    .find(|o| o.side == t.side && o.tag == t.tag && !matched.contains(&o.client_id));
                match existing {
                    Some(o) => {
                        matched.insert(o.client_id);
                        let remaining = o.sz - o.filled;
                        let unchanged =
                            (o.px - t.px).abs() < 1e-9 && (remaining - t.sz).abs() < 1e-9;
                        if unchanged {
                            continue; // no traffic
                        }
                        if o.exchange_id.is_none() {
                            // Ack still in flight: the old order cannot be
                            // cancelled at the exchange yet, so placing the new
                            // one would double the resting exposure (the live
                            // double-fill race). Keep the old quote this round;
                            // the ack lands within ~1 RTT and the next reconcile
                            // amends safely.
                            tracing::debug!(
                                "amend deferred (ack in flight): keeping [{}] {} {}@{:.4}",
                                o.tag, o.side, remaining, o.px
                            );
                            continue;
                        }
                        if self.throttle.allow(token, t.side, t.tag, now) {
                            to_cancel.push(CancelCandidate {
                                client_id: o.client_id,
                                side: o.side,
                                tag: o.tag,
                                remaining,
                                px: o.px,
                                exchange_id: o.exchange_id.clone(),
                            });
                            to_place.push(t);
                        } else {
                            tracing::debug!(
                                "amend throttled: keeping [{}] {} {}@{:.4}",
                                o.tag, o.side, remaining, o.px
                            );
                        }
                    }
                    None => {
                        if self.throttle.allow(token, t.side, t.tag, now) {
                            to_place.push(t);
                        }
                    }
                }
            }
            // Working orders no target claimed -> cancel.
            for o in &working {
                if !matched.contains(&o.client_id)
                    && !to_cancel.iter().any(|c| c.client_id == o.client_id)
                {
                    to_cancel.push(CancelCandidate {
                        client_id: o.client_id,
                        side: o.side,
                        tag: o.tag,
                        remaining: o.sz - o.filled,
                        px: o.px,
                        exchange_id: o.exchange_id.clone(),
                    });
                }
            }
        }

        // Cancels FIRST, then placements (submit_place enforces the cap).
        if !to_cancel.is_empty() {
            let label = self.token_label(token);
            let mut ids = Vec::new();
            for c in &to_cancel {
                tracing::info!(
                    "[CANCEL] [{}] {} {} {label} @ {:.4}",
                    c.tag, c.side, c.remaining, c.px
                );
                self.oms.mark_cancelled(c.client_id);
                if let Some(eid) = &c.exchange_id {
                    ids.push(eid.clone());
                }
            }
            self.exec_cancel_ids(ids);
        }
        for t in to_place {
            self.submit_place(token, t.side, t.px, t.sz, t.tag);
        }
    }
}
