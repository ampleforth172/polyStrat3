//! Feed wiring: process-lifetime WS feeds and per-window
//! subscriptions (CLOB market channel + SDK user channel with the
//! CONNECTED gate).

use super::*;

/// Per-window feed task handles, aborted on window roll or shutdown.
pub(super) struct WindowFeeds {
    md_task: tokio::task::JoinHandle<()>,
    #[cfg(feature = "live")]
    user_task: Option<tokio::task::JoinHandle<()>>,
}

impl WindowFeeds {
    fn abort(&self) {
        self.md_task.abort();
        #[cfg(feature = "live")]
        if let Some(t) = &self.user_task {
            t.abort();
        }
    }
}

impl Engine {
    /// Process-lifetime WS feeds: Chainlink RTDS, Binance (when the
    /// aggregated spot or alpha needs it), and — in live mode — the single
    /// SDK user-channel client (one connection for the whole process; the
    /// SDK multiplexes per-market subscriptions over it and resubscribes on
    /// reconnect).
    pub(super) fn spawn_static_feeds(&mut self) -> Result<(), String> {
        let g = &self.cfg.general;
        tokio::task::spawn_local(md::chainlink::run(
            g.rtds_url.clone(),
            g.symbol.clone(),
            self.tx.clone(),
        ));
        if self.cfg.spot.binance_enabled || self.cfg.alpha.enabled {
            tokio::task::spawn_local(md::binance::run(
                g.binance_ws_url.clone(),
                g.symbol.clone(),
                self.tx.clone(),
            ));
        }
        #[cfg(feature = "live")]
        if !g.dry_run {
            if let Some(exec) = &self.exec_live {
                self.user_ws = Some(crate::md::user_channel::client(
                    exec.api_creds(),
                    exec.address(),
                )?);
            }
        }
        Ok(())
    }

    /// Per-window WS subscriptions: the CLOB market channel for the window's
    /// two tokens, and (live) ONE user-channel subscription for the window's
    /// condition id. Order placement stays gated until the user channel is
    /// CONNECTED — a fill landing before the subscription reaches the server
    /// would be silently missed.
    pub(super) async fn spawn_window_feeds(&mut self, market: &MarketInfo) -> WindowFeeds {
        let g = &self.cfg.general;
        let md_task = tokio::task::spawn_local(md::clob_market::run(
            g.ws_url.clone(),
            vec![market.token_yes.clone(), market.token_no.clone()],
            self.tx.clone(),
        ));

        #[cfg(feature = "live")]
        let user_task = self.user_ws.as_ref().map(|ws| {
            tokio::task::spawn_local(crate::md::user_channel::run_window(
                ws.clone(),
                market.condition_id.clone(),
                self.tx.clone(),
            ))
        });
        #[cfg(feature = "live")]
        if let Some(ws) = &self.user_ws {
            match crate::md::user_channel::wait_connected(ws, std::time::Duration::from_secs(15))
                .await
            {
                Ok(()) => {
                    if !self.user_ws_ready {
                        self.user_ws_ready = true;
                        tracing::info!("user channel CONNECTED — order placement enabled");
                    }
                }
                Err(e) => {
                    self.user_ws_ready = false;
                    tracing::error!("{e} — order placement DISABLED until it connects");
                }
            }
        }

        WindowFeeds {
            md_task,
            #[cfg(feature = "live")]
            user_task,
        }
    }

    /// Tear down the per-window subscriptions: abort the tasks and release
    /// the user-channel market registration (single refcount — see
    /// md/user_channel.rs).
    pub(super) fn stop_window_feeds(&mut self, feeds: &WindowFeeds, market: &MarketInfo) {
        feeds.abort();
        #[cfg(feature = "live")]
        if let Some(ws) = &self.user_ws {
            if let Ok(cond) = std::str::FromStr::from_str(&market.condition_id) {
                let _ = ws.unsubscribe_user_events(&[cond]);
            }
        }
        #[cfg(not(feature = "live"))]
        let _ = market;
    }
}
