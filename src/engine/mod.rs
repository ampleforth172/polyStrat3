//! The engine: one single-threaded event loop owning ALL mutable state.
//!
//! ```text
//!  chainlink ─┐                                            ┌─ DryRun sim
//!  clob book ─┤   event    drain +     journal   on_event  │  (fills against
//!  binance  ──┼─▶ queue ─▶ coalesce ─▶ (after  ─▶ ingest ──┤   live book)
//!  user fills┘   (mpsc)    stale MD    dispatch)  state    └─ Live (SDK:
//!                                                  │          sign+post)
//!                                        should_reprice?        ▲
//!                                                  │            │
//!                       snapshot ─▶ strategy ─▶ actions ─▶ throttle ─▶ OMS
//!                       (Snap)      (pure FSM)             + reconcile
//!                                                          + per-token cap
//!  fills flow back as events ─▶ resolve (OMS authoritative) ─▶ positions
//!                               ─▶ pair netting ─▶ trade CSV ─▶ strategy
//! ```
//!
//! Submodules: [`execution`] (order chokepoint + executor facade),
//! [`portfolio`] (fills, netting, settled sync), [`feeds`] (WS wiring),
//! [`session`] (window lifecycle, shutdown, replay).
//!
//! ## Invariants (each one paid for by a live incident or a test)
//!
//! - **Single writer**: only this thread mutates engine state; feeds
//!   communicate exclusively through the event queue.
//! - **Virtual clock**: decision paths call `self.now()`, never
//!   `now_secs()` directly — replay determinism depends on it.
//! - **OMS is authoritative** for a fill's token/side/tag; WS messages are
//!   hints. Unknown live fills are ignored, never guessed.
//! - **Cancel before place**, and never cancel into nothing: a fully
//!   throttled replacement also skips its paired cancel.
//! - **Stop-loss orders bypass** the throttle and the user-channel gate.
//! - **Placement is gated** on the user channel being CONNECTED (the SDK
//!   dials lazily; fills placed earlier race the subscription).
//! - **Journal after dispatch**: it is a replay log, not a write-ahead
//!   log — its cost must never sit between tick and order.
//! - **Coalescing may drop stale books, never trades/fills/acks.**
//! - **Matched YES+NO pairs are riskless**: netted immediately, locked
//!   PnL realized, credited to the originally-held side.

use std::collections::HashMap;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::alpha::Alpha;
use crate::config::{Config, StrategyKind};
use crate::exec::dry_run::DryRunExec;
use crate::fair::FairPrice;
use crate::journal::{JournalWriter, RecordKind};
use crate::latency::StageStats;
use crate::md;
use crate::oms::Oms;
use crate::position::Position;
use crate::pnl_log;
use crate::spot::{SpotAggregator, SpotSource};
use crate::strategy::maker::Maker;
use crate::strategy::taker::Taker;
use crate::strategy::{FillInfo, SideSnap, Snap};
use crate::trade_log::{TradeCsvRow, TradeLog};
use crate::types::{
    now_secs, Action, BookTop, Event, MarketInfo, Outcome, OrderTag, Side, TokenId,
};

mod execution;
mod feeds;
mod portfolio;
mod session;

use execution::PlaceThrottle;
#[cfg(test)]
mod tests;

enum Strategy {
    Taker(Taker),
    Maker(Maker),
}

pub struct Engine {
    cfg: Config,
    http: reqwest::Client,
    fair: FairPrice,
    spot: SpotAggregator,
    alpha: Alpha,
    oms: Oms,
    strat: Strategy,
    throttle: PlaceThrottle,
    exec_dry: DryRunExec,
    #[cfg(feature = "live")]
    exec_live: Option<std::rc::Rc<crate::exec::live::LiveExec>>,
    trade_log: TradeLog,
    positions: HashMap<TokenId, Position>,
    tops: HashMap<TokenId, BookTop>,
    market: Option<MarketInfo>,
    /// Live mode: order placement is gated until the SDK user channel is
    /// CONNECTED (fills would otherwise race the subscription). Always true
    /// in dry-run.
    user_ws_ready: bool,
    #[cfg(feature = "live")]
    user_ws: Option<crate::md::user_channel::AuthedWs>,
    /// Guards against overlapping Data-API position polls (1s cadence).
    #[cfg(feature = "live")]
    pos_sync_inflight: std::rc::Rc<std::cell::Cell<bool>>,
    tx: UnboundedSender<Event>,
    rx: UnboundedReceiver<Event>,
    halted: bool,
    /// client_id -> submitted order info for live ack correlation.
    mode_label: &'static str,
    /// Event journal (record mode); every inbound event + window meta.
    journal: Option<JournalWriter>,
    /// Replay mode: virtual clock driven by journal timestamps. None = live.
    sim_now: Option<f64>,
    /// Tick-to-trade latency histograms.
    lat: StageStats,
    /// Wall-clock instant of the current engine wake (event arrival), used
    /// for the tick_to_order measurement.
    cycle_start: Option<std::time::Instant>,
    /// Reporting cadence for latency summaries.
    lat_report_at: f64,
    /// Reused event-batch buffer for `drain_and_dispatch` — no per-wake
    /// allocation.
    ev_batch: Vec<Event>,
    /// Reused scratch for `coalesce` (a market has 2 tokens; linear search
    /// beats a fresh HashMap per wake).
    coalesce_scratch: Vec<(TokenId, usize)>,
}

impl Engine {
    pub fn new(cfg: Config) -> Result<Self, String> {
        let (tx, rx) = unbounded_channel();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let fair = FairPrice::new(cfg.fair.clone(), cfg.general.interval_seconds as f64);
        let spot = SpotAggregator::from_cfg(&cfg.spot);
        let alpha = Alpha::new(cfg.alpha.clone());
        let strat = match cfg.general.strategy {
            StrategyKind::Taker => Strategy::Taker(Taker::new(&cfg)),
            StrategyKind::Maker => Strategy::Maker(Maker::new(&cfg)),
        };
        let trade_log = TradeLog::new(
            cfg.general.trade_csv_enabled,
            &cfg.general.trade_csv_dir,
            &cfg.general.symbol,
        )?;
        let mode_label = if cfg.general.dry_run { "dry-run" } else { "live" };
        let journal = if cfg.journal.enabled {
            Some(JournalWriter::create(
                &cfg.journal.dir,
                &cfg.general.symbol,
                now_secs(),
            )?)
        } else {
            None
        };
        let cfg_throttle_ms = cfg.general.order_throttle_ms;
        let cfg_dry_run = cfg.general.dry_run;
        Ok(Self {
            cfg,
            http,
            fair,
            spot,
            alpha,
            oms: Oms::new(),
            strat,
            throttle: PlaceThrottle::new(cfg_throttle_ms),
            exec_dry: DryRunExec::new(),
            #[cfg(feature = "live")]
            exec_live: None,
            trade_log,
            positions: HashMap::new(),
            tops: HashMap::new(),
            market: None,
            user_ws_ready: cfg_dry_run,
            #[cfg(feature = "live")]
            user_ws: None,
            #[cfg(feature = "live")]
            pos_sync_inflight: std::rc::Rc::new(std::cell::Cell::new(false)),
            tx,
            rx,
            halted: false,
            mode_label,
            journal,
            sim_now: None,
            lat: StageStats::default(),
            cycle_start: None,
            lat_report_at: 0.0,
            ev_batch: Vec::with_capacity(256),
            coalesce_scratch: Vec::with_capacity(4),
        })
    }

    #[cfg(feature = "live")]
    pub fn set_live_exec(&mut self, exec: crate::exec::live::LiveExec) {
        self.exec_live = Some(std::rc::Rc::new(exec));
    }

    pub fn event_sender(&self) -> UnboundedSender<Event> {
        self.tx.clone()
    }

    /// Engine time: wall clock live, journal time in replay. Every decision
    /// path must use this (never now_secs directly) so replays are
    /// deterministic.
    fn now(&self) -> f64 {
        self.sim_now.unwrap_or_else(now_secs)
    }

    /// Human label for a token in logs: YES / NO, or the short id if the
    /// token doesn't belong to the active market.
    fn token_label(&self, token: &str) -> String {
        match self.market.as_ref().and_then(|m| m.outcome_of(token)) {
            Some(o) => o.label().to_string(),
            None => format!("{}…", &token[..token.len().min(8)]),
        }
    }



    /// Build the world snapshot handed to strategies. `&mut` only because
    /// the alpha signal decays internal state on read — no market or order
    /// state changes here.
    fn snapshot(&mut self) -> Snap {
        let now = self.now();
        let tte = self
            .market
            .as_ref()
            .map(|m| (m.end_ts - now).max(0.0))
            .unwrap_or(0.0);
        let cutoff = match self.cfg.general.strategy {
            StrategyKind::Taker => self.cfg.taker.cutoff_secs(self.cfg.general.interval_seconds),
            StrategyKind::Maker => self.cfg.maker.pre_expiry_cutoff_secs,
        };
        // Pricing uses the aggregated spot (Chainlink level extrapolated by
        // Binance moves); raw Chainlink is kept for the alpha basis term.
        let spot = self.spot.spot(now);
        let chainlink = self.spot.chainlink_px();
        let fair_yes = spot.and_then(|s| self.fair.fair_yes(s, now));
        let alpha_shift = self.alpha.shift(&self.fair, spot, chainlink, now);
        let quote_center_yes = fair_yes.map(|f| (f + alpha_shift).clamp(0.01, 0.99));
        Snap {
            now,
            tte,
            trading_enabled: tte > cutoff,
            spot,
            fair_yes,
            quote_center_yes,
            // Momentum is a maker-only input; the taker never reads it, so
            // don't pay the lookup on its snapshot path.
            momentum_ret: match self.cfg.general.strategy {
                StrategyKind::Maker => self.fair.ret_over(self.cfg.maker.momentum_window_secs),
                StrategyKind::Taker => None,
            },
            hour_utc: ((now / 3600.0) as u32) % 24,
            yes: self.side_snap(Outcome::Yes),
            no: self.side_snap(Outcome::No),
        }
    }

    // ── strategy drive ───────────────────────────────────────────────────────

    /// Re-evaluate the strategy against a fresh snapshot. Called on every
    /// market-data event (CLOB book, Binance book/trade, Chainlink tick) and
    /// on the fast timer — strategies decide internally whether to act, so
    /// redundant calls are cheap no-ops.
    fn drive_strategy(&mut self) {
        // Window open price rolls from the latest AGGREGATED spot (not the
        // Chainlink print) at every interval boundary.
        let now = self.now();
        if let Some(px) = self.spot.spot(now) {
            self.fair.maybe_roll_open(px, now);
        }
        if self.halted || self.market.is_none() {
            return;
        }
        let t0 = std::time::Instant::now();
        let snap = self.snapshot();
        let interval = self.cfg.general.interval_seconds as f64;
        let actions = match &mut self.strat {
            Strategy::Taker(t) => {
                let mut acts = t.on_book(Outcome::Yes, &snap);
                acts.extend(t.on_book(Outcome::No, &snap));
                acts
            }
            Strategy::Maker(mk) => mk.on_tick(&snap, interval),
        };
        self.lat.decision.record(t0.elapsed());
        self.execute(actions);
    }

    // ── event dispatch ───────────────────────────────────────────────────────

    /// The repricing policy in one place: which events re-drive the
    /// strategy after their state is ingested.
    /// - Book / BinanceBook / BinanceTrade: yes — market moved.
    /// - UserTrade / PositionSync: their handlers drive conditionally
    ///   (after booking the fill / only on a settled-balance change).
    /// - OrderAck / FeedInfo: bookkeeping only.
    /// NOTE: PriceTick (Chainlink) deliberately does NOT re-drive — it only
    /// feeds the spot aggregator; with the Chainlink leg disabled (default)
    /// it never moves pricing, and re-driving here would double-fire.
    fn should_reprice(ev: &Event) -> bool {
        matches!(
            ev,
            Event::Book { .. } | Event::BinanceBook { .. } | Event::BinanceTrade { .. }
        )
    }

    fn on_event(&mut self, ev: Event) {
        let reprice = Self::should_reprice(&ev);
        match ev {
            Event::PriceTick { px, ts } => {
                self.spot.on_chainlink(px, ts);
            }
            Event::Book { token, top } => {
                self.tops.insert(token.clone(), top);
                if let Some(pos) = self.positions.get_mut(&token) {
                    if let Some(b) = top.bid {
                        pos.last_bid = b;
                    }
                } else if let Some(b) = top.bid {
                    let p = self.positions.entry(token.clone()).or_default();
                    p.last_bid = b;
                }
                // DryRun: check resting orders against the new top first.
                if self.cfg.general.dry_run {
                    for f in self.exec_dry.on_book(&token, &top) {
                        self.apply_fill(
                            &f.token.clone(),
                            f.side,
                            f.px,
                            f.sz,
                            &f.order_id,
                            &f.trade_id,
                            true, // resting orders fill as maker
                        );
                    }
                }
            }
            Event::UserTrade {
                token,
                side,
                px,
                sz,
                order_id,
                trade_id,
                maker,
            } => {
                self.apply_fill(&token, side, px, sz, &order_id, &trade_id, maker);
            }
            Event::BinanceBook { bids, asks, ts } => {
                let mid = match (bids.first(), asks.first()) {
                    (Some((b, _)), Some((a, _))) => Some((b + a) / 2.0),
                    _ => None,
                };
                self.alpha.on_book(bids, asks, ts);
                if let Some(m) = mid {
                    self.spot.on_binance(m, ts, SpotSource::Depth);
                }
            }
            Event::BinanceTrade {
                px,
                sz,
                is_buyer_maker,
                ts,
            } => {
                self.alpha.on_trade(px, sz, is_buyer_maker, ts);
                self.spot.on_binance(px, ts, SpotSource::Trade);
            }
            Event::OrderAck { client_id, result } => {
                if let Err(e) = &result {
                    tracing::warn!("order {client_id} rejected: {e}");
                }
                // A cancel requested while the ack was in flight fires NOW.
                if let Some(deferred_id) = self.oms.on_ack(client_id, &result) {
                    tracing::info!("sending deferred cancel for {deferred_id}");
                    self.exec_cancel_ids(vec![deferred_id]);
                }
            }
            Event::PositionSync { token, settled } => {
                // The poll task only emits on change, so this logs changes.
                let label = self.token_label(&token);
                tracing::info!("[POS-SYNC] {label}: {settled}");
                self.positions.entry(token).or_default().settled = settled;
            }
            Event::FeedInfo(msg) => {
                tracing::info!("[FEED] {msg}");
            }
        }
        if reprice {
            self.drive_strategy();
        }
    }

    /// One engine wake: drain the queue, coalesce stale market data, then
    /// dispatch each survivor with latency accounting and post-dispatch
    /// journaling.
    ///
    /// Coalescing: a burst of book updates for one token collapses to the
    /// newest one (only the latest top matters), but trades/fills/acks are
    /// never dropped — under load the engine jumps straight to the freshest
    /// state instead of chewing through stale ticks.
    ///
    /// Journal AFTER dispatch: this is a replay log, not a write-ahead
    /// log — nothing requires the event on disk before acting on it, so its
    /// cost (serialize + buffered write + periodic flush) is paid in
    /// post-decision slack, never between tick and order. Single thread ⇒
    /// file ordering is unchanged.
    fn drain_and_dispatch(&mut self, first: Event) {
        // Reuse the batch buffer across wakes (capacity is retained).
        let mut batch = std::mem::take(&mut self.ev_batch);
        batch.clear();
        batch.push(first);
        while batch.len() < 256 {
            match self.rx.try_recv() {
                Ok(ev) => batch.push(ev),
                Err(_) => break,
            }
        }
        let coalesced = Self::coalesce(&mut batch, &mut self.coalesce_scratch);
        if coalesced > 0 {
            tracing::debug!("coalesced {coalesced} stale market-data event(s)");
        }
        let recv_ts = now_secs();
        for ev in batch.drain(..) {
            let journal_ev = self.journal.is_some().then(|| ev.clone());
            let t0 = std::time::Instant::now();
            self.cycle_start = Some(t0);
            self.on_event(ev);
            self.lat.dispatch.record(t0.elapsed());
            self.cycle_start = None;
            if let (Some(j), Some(ev)) = (self.journal.as_mut(), journal_ev) {
                let j0 = std::time::Instant::now();
                j.record(recv_ts, RecordKind::Ev(ev));
                self.lat.journal.record(j0.elapsed());
            }
        }
        self.ev_batch = batch;
    }

    /// Fast timer (default 100 ms): safety-net re-evaluation for purely
    /// time-driven transitions (cooldown expiry, cutoff, quote refresh) in
    /// case market-data events go quiet.
    fn on_fast_tick(&mut self) {
        self.cycle_start = Some(std::time::Instant::now());
        self.drive_strategy();
        self.cycle_start = None;
    }

    /// Keep only the LAST Book per token and the LAST BinanceBook in the
    /// batch; everything else (trades, fills, acks, price ticks) is kept in
    /// order. Returns how many stale events were dropped.
    fn coalesce(batch: &mut Vec<Event>, last_book: &mut Vec<(TokenId, usize)>) -> usize {
        let before = batch.len();
        // `last_book` is a reused scratch: a market has exactly two tokens,
        // so a linear scan over <=2 entries beats a per-wake HashMap.
        last_book.clear();
        let mut last_binance: Option<usize> = None;
        for (i, ev) in batch.iter().enumerate() {
            match ev {
                Event::Book { token, .. } => {
                    match last_book.iter_mut().find(|(t, _)| t == token) {
                        Some(slot) => slot.1 = i,
                        None => last_book.push((token.clone(), i)),
                    }
                }
                Event::BinanceBook { .. } => last_binance = Some(i),
                _ => {}
            }
        }
        let mut idx = 0usize;
        batch.retain(|ev| {
            let keep = match ev {
                Event::Book { token, .. } => last_book
                    .iter()
                    .find(|(t, _)| t == token)
                    .map(|(_, i)| *i)
                    == Some(idx),
                Event::BinanceBook { .. } => last_binance == Some(idx),
                _ => true,
            };
            idx += 1;
            keep
        });
        before - batch.len()
    }

    /// 1 s timer: samples the vol window at a fixed 1 Hz cadence from the
    /// latest aggregated spot (consistent sampling frequency for σ), and
    /// logs. All trading decisions remain event-driven.
    fn on_second(&mut self) {
        let now = self.now();
        if let Some(px) = self.spot.spot(now) {
            self.fair.update(px, now);
        }
        if now >= self.lat_report_at {
            if self.lat.dispatch.count() > 0 {
                tracing::info!("{}", self.lat.report());
            }
            self.lat_report_at = now + 60.0;
        }
        // Live: authoritative settled-balance poll, once per second.
        self.spawn_position_sync();
        // Live: track user-channel health; placement is gated on it.
        #[cfg(feature = "live")]
        if let Some(ws) = &self.user_ws {
            let connected = crate::md::user_channel::is_connected(ws);
            if connected != self.user_ws_ready {
                self.user_ws_ready = connected;
                if connected {
                    tracing::info!("user channel reconnected — order placement re-enabled");
                } else {
                    tracing::warn!("user channel DISCONNECTED — new orders gated (stop-loss exempt)");
                }
            }
        }
        let Some(m) = self.market.clone() else {
            return;
        };
        let tte = m.end_ts - now;
        let snap = self.snapshot();
        let spot_s = snap
            .spot
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "n/a".into());
        let src_s = self
            .spot
            .latest_source()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "n/a".into());
        let fair_s = snap
            .fair_yes
            .map(|v| format!("{v:.4}"))
            .unwrap_or_else(|| "n/a".into());
        let px_s = |v: Option<f64>| v.map(|v| format!("{v:.4}")).unwrap_or_else(|| "----".into());
        // Latest quote targets in YES terms (buy-only mapping): the resting
        // BUY on YES is our quote bid; the resting BUY on NO at p implies a
        // quote ask of 1 - p. Tag-agnostic on purpose — maker quotes are
        // QuoteBid, taker entries/closes EntryBuy/TakeProfit/StopLoss.
        let quote_bid = self.oms.any_resting_buy_price(&snap.yes.token);
        let quote_ask = self
            .oms
            .any_resting_buy_price(&snap.no.token)
            .map(|p| 1.0 - p);
        tracing::info!(
            "{sym}={spot_s} {src_s} open={open:.2} tte={tte:.1}s{cutoff} \
             | quote bid={qb} fair={fair_s} ask={qa} \
             | market bid={yb} ask={ya} pos={ypos:.2}@{yavg:.4} \
             || NO pos={npos:.2}@{navg:.4}",
            sym = self.cfg.general.symbol,
            open = self.fair.open_price(),
            cutoff = if snap.trading_enabled { "" } else { " [CUTOFF]" },
            qb = px_s(quote_bid),
            qa = px_s(quote_ask),
            yb = px_s(snap.yes.top.and_then(|t| t.bid)),
            ya = px_s(snap.yes.top.and_then(|t| t.ask)),
            ypos = snap.yes.pos,
            yavg = snap.yes.avg_entry,
            npos = snap.no.pos,
            navg = snap.no.avg_entry,
        );
    }

    // ── main loop ────────────────────────────────────────────────────────────

    pub async fn run(mut self) -> Result<(), String> {
        let g = self.cfg.general.clone();

        // Bootstrap historical vol — unless a fixed override is configured.
        if let Some(v) = self.cfg.fair.vol_override {
            tracing::info!(
                "vol override active: {:.1}% annualized (rolling/historical vol ignored)",
                v * 100.0
            );
        } else {
            match md::gamma::fetch_historical_vol(&self.http, &g.binance_rest_url, &g.symbol, self.cfg.fair.hist_vol_bars).await {
                Ok(Some(v)) => {
                    tracing::info!("historical vol seeded: {:.1}%", v * 100.0);
                    self.fair.seed_historical_vol(v);
                }
                Ok(None) => tracing::warn!("historical vol unavailable — using default"),
                Err(e) => tracing::warn!("historical vol fetch failed: {e}"),
            }
        }

        self.spawn_static_feeds()?;

        let mut fast_timer =
            tokio::time::interval(std::time::Duration::from_millis(self.cfg.alpha.tick_ms.max(10)));
        let mut second_timer = tokio::time::interval(std::time::Duration::from_secs(1));
        fast_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        second_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // ── discover the active market ──────────────────────────────────
            let market = loop {
                match md::gamma::find_active_market(
                    &self.http,
                    &g.gamma_host,
                    &g.symbol,
                    g.interval_seconds as i64,
                )
                .await
                {
                    Ok(Some(m)) => break m,
                    Ok(None) => tracing::warn!("no active market — retrying in 30s"),
                    Err(e) => tracing::warn!("market discovery failed: {e} — retrying in 30s"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                    _ = Self::shutdown_signal() => return Ok(()),
                }
            };

            // Seed the open price from Binance.
            let open_ts = market.end_ts as i64 - g.interval_seconds as i64;
            match md::gamma::fetch_open_price(&self.http, &g.binance_rest_url, &g.symbol, open_ts).await
            {
                Ok(Some(px)) => {
                    tracing::info!("open price seeded: {open_ts} {px:.2}");
                    self.fair.set_open_price(px, open_ts as f64);
                }
                _ => tracing::warn!("open price unavailable — latest aggregated spot will seed it at the boundary"),
            }

            self.start_window(market.clone());
            if let Some(j) = self.journal.as_mut() {
                j.record(
                    now_secs(),
                    RecordKind::Window {
                        market: market.clone(),
                        open_price: self.fair.open_price(),
                        open_ts: market.end_ts - g.interval_seconds as f64,
                        annual_vol: self.fair.annualized_vol(),
                    },
                );
            }

            let window_feeds = self.spawn_window_feeds(&market).await;

            // ── inner loop until expiry ─────────────────────────────────────
            let expired = loop {
                if now_secs() >= market.end_ts {
                    break true;
                }
                tokio::select! {
                    ev = self.rx.recv() => {
                        let Some(ev) = ev else { break false };
                        self.drain_and_dispatch(ev);
                    }
                    _ = fast_timer.tick() => self.on_fast_tick(),
                    _ = second_timer.tick() => self.on_second(),
                    _ = Self::shutdown_signal() => {
                        tracing::info!("shutdown signal — cancelling all open orders");
                        // Awaited: the process must not exit before the
                        // exchange confirms the cancel.
                        self.cancel_all_and_wait().await;
                        self.settle_window(&market);
                        self.stop_window_feeds(&window_feeds, &market);
                        return Ok(());
                    }
                }
            };

            self.stop_window_feeds(&window_feeds, &market);
            if !expired {
                return Err("event channel closed".into());
            }
            tracing::info!("market expired — settling and rolling over");
            self.settle_window(&market);
        }
    }
}
