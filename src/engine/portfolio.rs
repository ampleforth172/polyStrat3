//! Fills and holdings: dedup + OMS-authoritative fill resolution,
//! position/fee booking, YES+NO pair netting, and the 1 Hz settled-
//! balance sync against the Data API.

use super::*;

impl Engine {
    /// Fetch the ACTUAL sellable balances for BOTH tokens of the active
    /// market from the Data API (tokens become spendable only once trades
    /// are mined). Called every second from the 1s timer; a single request
    /// covers the whole market. PositionSync is emitted only when a size
    /// CHANGED vs the last known value, so logs and the journal stay quiet
    /// while positions are static. Overlapping requests are skipped.
    #[cfg(feature = "live")]
    pub(super) fn spawn_position_sync(&self) {
        if self.cfg.general.dry_run || self.exec_live.is_none() {
            return;
        }
        let Some(m) = &self.market else {
            return;
        };
        if self.pos_sync_inflight.get() {
            return;
        }
        self.pos_sync_inflight.set(true);
        let inflight = self.pos_sync_inflight.clone();
        let exec = self.exec_live.as_ref().expect("checked above").clone();
        let cond = m.condition_id.clone();
        let settled_of = |t: &crate::types::TokenId| {
            self.positions.get(t).map(|p| p.settled).unwrap_or(0.0)
        };
        let tokens: [(crate::types::TokenId, f64); 2] = [
            (m.token_yes.clone(), settled_of(&m.token_yes)),
            (m.token_no.clone(), settled_of(&m.token_no)),
        ];
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            match exec.market_positions(&cond).await {
                Ok(list) => {
                    for (token, prev) in tokens {
                        // Position.asset is a U256 token id; our TokenId is
                        // its decimal-string form. Size is a Decimal.
                        let size = list
                            .iter()
                            .find(|p| p.asset.to_string() == token.as_str())
                            .map(|p| p.size.to_string().parse().unwrap_or(0.0))
                            .unwrap_or(0.0);
                        if (size - prev).abs() > 1e-9 {
                            let _ = tx.send(Event::PositionSync {
                                token,
                                settled: size,
                            });
                        }
                    }
                }
                Err(e) => tracing::debug!("position sync failed: {e}"),
            }
            inflight.set(false);
        });
    }

    #[cfg(not(feature = "live"))]
    pub(super) fn spawn_position_sync(&self) {}

    // ── snapshot ─────────────────────────────────────────────────────────────

    pub(super) fn side_snap(&self, o: Outcome) -> SideSnap {
        let Some(m) = &self.market else {
            return SideSnap::default();
        };
        let token = m.token(o).clone();
        let pos = self.positions.get(&token).cloned().unwrap_or_default();
        // Dry-run has no mint latency: everything held is sellable.
        let settled = if self.cfg.general.dry_run {
            pos.position
        } else {
            pos.settled
        };
        SideSnap {
            top: self.tops.get(&token).copied(),
            pending_buy: self.oms.pending_entry_buy_qty(&token),
            resting_buy_px: self.oms.resting_buy_price(&token),
            working_orders: self.oms.working_count(&token),
            pos: pos.position,
            settled,
            avg_entry: pos.avg_entry,
            realized_pnl: pos.realized_pnl,
            total_fees: pos.total_fees,
            token,
        }
    }

    /// Dedup (WS redelivery on reconnect) and resolve a fill against the
    /// OMS. The OMS entry is AUTHORITATIVE for token/side/tag — WS trade
    /// messages only carry hints (maker legs don't state OUR side at all).
    /// Live fills for unknown orders are ignored; dry-run falls back to the
    /// hints (direct-injection tests only).
    fn resolve_fill(
        &mut self,
        order_id: &str,
        trade_id: &str,
        sz: f64,
        token_hint: &str,
        side_hint: Side,
    ) -> Option<(TokenId, Side, OrderTag)> {
        if self.oms.is_duplicate_trade(trade_id) {
            return None;
        }
        match self.oms.on_trade(order_id, trade_id, sz) {
            Some(o) => Some((o.token.clone(), o.side, o.tag)),
            None if self.cfg.general.dry_run => {
                Some((token_hint.into(), side_hint, OrderTag::EntryBuy))
            }
            None => {
                tracing::debug!(
                    "fill for unknown order {order_id} — ignored (not ours, or ack still in flight)"
                );
                None
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_fill(
        &mut self,
        token: &str,
        side: Side,
        px: f64,
        sz: f64,
        order_id: &str,
        trade_id: &str,
        maker: bool,
    ) {
        let Some(m) = self.market.clone() else {
            return;
        };
        let Some((token, side, tag)) = self.resolve_fill(order_id, trade_id, sz, token, side)
        else {
            return;
        };
        let Some(outcome) = m.outcome_of(&token) else {
            tracing::warn!("fill on token outside the active market — ignored");
            return;
        };
        let pos = self.positions.entry(token.clone()).or_default();
        let pnl = pos.on_fill(side, sz, px);
        let fee = pos.add_fee(px, sz, maker);
        let (position_after, avg_after) = (pos.position, pos.avg_entry);
        if !self.cfg.general.dry_run && side == Side::Sell {
            // Local estimate on sells; the 1s Data-API poll is authoritative.
            let p = self.positions.entry(token.clone()).or_default();
            p.settled = (p.settled - sz).max(0.0);
        }
        // Matched YES+NO pairs redeem for exactly $1: net them out of both
        // positions and REALIZE the locked PnL, credited to the side that
        // was already held (TP/SL closes buy the opposite token).
        self.net_market_pairs(&m, outcome.other());
        tracing::info!(
            "[TRADE] {} {side} {sz:.2}@{px:.4} fee={fee:.6} pos={position_after:.2} pnl={pnl:+.4}",
            outcome.label()
        );
        let row = TradeCsvRow {
            ts_utc: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            slug: &m.slug,
            condition_id: &m.condition_id,
            token_id: token.as_str(),
            outcome,
            side,
            px,
            sz,
            fee,
            order_id,
            trade_id,
            mode: self.mode_label,
            position_after,
            avg_entry_after: avg_after,
        };
        if let Err(e) = self.trade_log.record(&row) {
            tracing::warn!("trade csv write failed: {e}");
        }

        let snap = self.snapshot();
        let fill = FillInfo {
            outcome,
            side,
            px,
            sz,
            tag,
        };
        let actions = match &mut self.strat {
            Strategy::Taker(t) => t.on_fill(&fill, &snap),
            Strategy::Maker(mk) => mk.on_fill(&fill, &snap),
        };
        self.execute(actions);
    }

    /// Net matched YES+NO pairs: each pair is worth exactly $1 at
    /// settlement regardless of outcome, so `matched` tokens leave both
    /// positions and their locked PnL = (1 − avg_yes − avg_no) × matched
    /// is realized immediately, credited to `credit` (the originally-held
    /// side for a TP/SL close).
    fn net_market_pairs(&mut self, m: &MarketInfo, credit: Outcome) {
        let y = self.positions.get(&m.token_yes).map(|p| (p.position, p.avg_entry));
        let n = self.positions.get(&m.token_no).map(|p| (p.position, p.avg_entry));
        let (Some((ypos, yavg)), Some((npos, navg))) = (y, n) else {
            return;
        };
        let matched = ypos.min(npos);
        if matched <= 1e-9 {
            return;
        }
        let pnl = (1.0 - yavg - navg) * matched;
        for (tok, was) in [(&m.token_yes, ypos), (&m.token_no, npos)] {
            if let Some(p) = self.positions.get_mut(tok) {
                p.position = (was - matched).max(0.0);
                if p.position <= 1e-9 {
                    p.position = 0.0;
                    p.avg_entry = 0.0;
                }
            }
        }
        if let Some(p) = self.positions.get_mut(m.token(credit)) {
            p.realized_pnl += pnl;
        }
        tracing::info!(
            "[PAIR] netted {matched:.2} YES+NO pair(s): locked PnL {pnl:+.4} (yes@{yavg:.4} + no@{navg:.4} -> $1)"
        );
    }
}
