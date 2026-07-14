use super::*;
use crate::config::Config;

    #[test]
    fn place_throttle_semantics() {
        let mut t = PlaceThrottle::new(200);
        assert!(t.allow(&"Y".into(), Side::Buy, OrderTag::EntryBuy, 100.0));
        assert!(!t.allow(&"Y".into(), Side::Buy, OrderTag::EntryBuy, 100.1), "within 200ms");
        assert!(t.allow(&"Y".into(), Side::Sell, OrderTag::TakeProfit, 100.1), "other side is a different key");
        assert!(t.allow(&"N".into(), Side::Buy, OrderTag::QuoteBid, 100.1), "other token is a different key");
        assert!(t.allow(&"Y".into(), Side::Buy, OrderTag::StopLoss, 100.1), "stop-loss exempt");
        assert!(t.allow(&"Y".into(), Side::Buy, OrderTag::EntryBuy, 100.31), "interval elapsed");
        t.reset();
        assert!(t.allow(&"Y".into(), Side::Buy, OrderTag::EntryBuy, 100.32), "reset clears history");
        // 0 disables throttling entirely.
        let mut t0 = PlaceThrottle::new(0);
        assert!(t0.allow(&"Y".into(), Side::Buy, OrderTag::EntryBuy, 1.0));
        assert!(t0.allow(&"Y".into(), Side::Buy, OrderTag::EntryBuy, 1.0));
    }

    fn engine_cfg(f: impl FnOnce(&mut Config)) -> Engine {
        let mut cfg = Config::default();
        cfg.general.trade_csv_enabled = false;
        f(&mut cfg);
        let mut e = Engine::new(cfg).unwrap();
        e.start_window(MarketInfo {
            slug: "test-window".into(),
            condition_id: "0xc".into(),
            token_yes: "Y".into(),
            token_no: "N".into(),
            end_date_iso: "2099-01-01T00:00:00Z".into(),
            end_ts: now_secs() + 900.0,
        });
        e
    }

    fn engine() -> Engine {
        engine_cfg(|_| {})
    }

    #[test]
    fn targets_reconciliation_diffing() {
        use crate::types::TargetOrder;
        let mut e = engine_cfg(|c| c.general.order_throttle_ms = 0);
        let bid = |px: f64, sz: f64| TargetOrder {
            side: Side::Buy,
            px,
            sz,
            tag: OrderTag::QuoteBid,
        };

        // New target -> placed.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.50, 5.0)] }]);
        assert_eq!(e.exec_dry.resting_count(), 1);
        let eid1 = e.oms.working_orders("Y")[0].exchange_id.clone().unwrap();

        // Identical price + qty -> NO action: same exchange order survives.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.50, 5.0)] }]);
        assert_eq!(e.exec_dry.resting_count(), 1);
        assert_eq!(e.oms.working_orders("Y")[0].exchange_id.as_deref(), Some(eid1.as_str()));

        // Price updated -> old order cancelled FIRST, new one placed.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.51, 5.0)] }]);
        let w = e.oms.working_orders("Y");
        assert_eq!(w.len(), 1);
        assert_ne!(w[0].exchange_id.as_deref(), Some(eid1.as_str()));
        assert!((w[0].px - 0.51).abs() < 1e-9);
        assert_eq!(e.exec_dry.resting_count(), 1);

        // Qty updated -> also cancel + replace.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.51, 7.0)] }]);
        let w = e.oms.working_orders("Y");
        assert_eq!(w.len(), 1);
        assert!((w[0].sz - 7.0).abs() < 1e-9);

        // Empty target set -> everything on the token cancelled.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![] }]);
        assert_eq!(e.oms.working_count("Y"), 0);
        assert_eq!(e.exec_dry.resting_count(), 0);
    }

    #[test]
    fn targets_amend_throttled_keeps_old_order() {
        use crate::types::TargetOrder;
        let mut e = engine(); // default 200ms throttle
        let bid = |px: f64| TargetOrder {
            side: Side::Buy,
            px,
            sz: 5.0,
            tag: OrderTag::QuoteBid,
        };
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.50)] }]);
        let eid1 = e.oms.working_orders("Y")[0].exchange_id.clone().unwrap();
        // Immediate price amend: throttled -> old order kept untouched.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.51)] }]);
        let w = e.oms.working_orders("Y");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].exchange_id.as_deref(), Some(eid1.as_str()));
        assert!((w[0].px - 0.50).abs() < 1e-9, "old price kept while throttled");
        // Unchanged target meanwhile is still a no-op.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.50)] }]);
        assert_eq!(e.oms.working_orders("Y")[0].exchange_id.as_deref(), Some(eid1.as_str()));
    }

    #[test]
    fn max_orders_per_token_enforced() {
        use crate::types::TargetOrder;
        let mut e = engine_cfg(|c| {
            c.general.order_throttle_ms = 0;
            c.general.max_orders_per_token = 2;
        });
        let orders = vec![
            TargetOrder { side: Side::Buy, px: 0.50, sz: 5.0, tag: OrderTag::QuoteBid },
            TargetOrder { side: Side::Sell, px: 0.60, sz: 5.0, tag: OrderTag::QuoteAsk },
            TargetOrder { side: Side::Sell, px: 0.70, sz: 5.0, tag: OrderTag::TakeProfit },
        ];
        e.execute(vec![Action::Targets { token: "Y".into(), orders }]);
        assert_eq!(e.oms.working_count("Y"), 2, "third order beyond the cap must be dropped");
        // Plain Place beyond the cap is dropped too.
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.40,
            sz: 1.0,
            tag: OrderTag::EntryBuy,
        }]);
        assert_eq!(e.oms.working_count("Y"), 2);
    }

    #[test]
    fn live_placement_gated_until_user_channel_ready() {
        let mut e = engine_cfg(|c| c.general.dry_run = false);
        assert!(!e.user_ws_ready, "live engines start gated");
        // Normal placement is dropped while gated…
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.50,
            sz: 5.0,
            tag: OrderTag::EntryBuy,
        }]);
        assert_eq!(e.oms.working_count("Y"), 0, "gated placement must not reach the OMS");
        // …but stop-loss orders always pass.
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Sell,
            px: 0.30,
            sz: 5.0,
            tag: OrderTag::StopLoss,
        }]);
        assert_eq!(e.oms.working_count("Y"), 1);
        // Once connected, normal placements flow again.
        e.user_ws_ready = true;
        e.execute(vec![Action::Place {
            token: "N".into(),
            side: Side::Buy,
            px: 0.40,
            sz: 5.0,
            tag: OrderTag::EntryBuy,
        }]);
        assert_eq!(e.oms.working_count("N"), 1);
    }

    #[test]
    fn live_fill_resolves_side_and_token_from_oms() {
        let mut e = engine_cfg(|c| c.general.dry_run = false);
        e.user_ws_ready = true;
        // A live BUY on YES, acked with exchange id ex-1.
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.50,
            sz: 5.0,
            tag: OrderTag::QuoteBid,
        }]);
        let cid = e.oms.working_orders("Y")[0].client_id;
        e.oms.on_ack(cid, &Ok("ex-1".into()));
        // WS fill arrives with WRONG hints (token/side) — the OMS entry wins.
        e.on_event(Event::UserTrade {
            token: "COMPLETELY-WRONG".into(),
            side: Side::Sell,
            px: 0.50,
            sz: 5.0,
            order_id: "ex-1".into(),
            trade_id: "t-1".into(),
            maker: true,
        });
        let pos = e.positions.get("Y").cloned().unwrap_or_default();
        assert!((pos.position - 5.0).abs() < 1e-9, "fill applied to the OMS token as a BUY");
        assert!((pos.avg_entry - 0.50).abs() < 1e-9);
        assert!(e.positions.get("COMPLETELY-WRONG").is_none());
    }

    #[test]
    fn live_fill_for_unknown_order_ignored() {
        let mut e = engine_cfg(|c| c.general.dry_run = false);
        e.on_event(Event::UserTrade {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.50,
            sz: 5.0,
            order_id: "not-ours".into(),
            trade_id: "t-x".into(),
            maker: true,
        });
        assert!(e.positions.get("Y").map(|p| p.position).unwrap_or(0.0) == 0.0);
    }

    #[test]
    fn coalesce_keeps_latest_book_and_never_drops_fills() {
        let top = |b: f64| BookTop {
            bid: Some(b),
            bid_sz: 1.0,
            ask: Some(b + 0.01),
            ask_sz: 1.0,
        };
        let fill = Event::UserTrade {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.5,
            sz: 5.0,
            order_id: "o".into(),
            trade_id: "t".into(),
            maker: true,
        };
        let mut batch = vec![
            Event::Book { token: "Y".into(), top: top(0.40) }, // stale
            Event::BinanceBook { bids: vec![(1.0, 1.0)], asks: vec![(2.0, 1.0)], ts: 1.0 }, // stale
            fill.clone(),
            Event::Book { token: "Y".into(), top: top(0.50) }, // latest Y
            Event::Book { token: "N".into(), top: top(0.30) }, // latest N
            Event::BinanceBook { bids: vec![(3.0, 1.0)], asks: vec![(4.0, 1.0)], ts: 2.0 }, // latest
            Event::PriceTick { px: 64_000.0, ts: 3.0 },
        ];
        let dropped = Engine::coalesce(&mut batch, &mut Vec::new());
        assert_eq!(dropped, 2, "one stale book + one stale binance book");
        assert_eq!(batch.len(), 5);
        assert!(matches!(&batch[0], Event::UserTrade { .. }), "fills always survive");
        assert!(matches!(&batch[1], Event::Book { token, top } if token == "Y" && top.bid == Some(0.50)));
        assert!(matches!(&batch[2], Event::Book { token, .. } if token == "N"));
        assert!(matches!(&batch[3], Event::BinanceBook { ts, .. } if *ts == 2.0));
        assert!(matches!(&batch[4], Event::PriceTick { .. }));
    }

    #[test]
    fn pending_cancel_race_blocks_replacement_until_ack() {
        // Live mode: acks are asynchronous, so a cancel can race the ack.
        let mut e = engine_cfg(|c| {
            c.general.dry_run = false;
            c.general.order_throttle_ms = 0;
        });
        e.user_ws_ready = true;
        // Entry buy submitted; ack still in flight (exec_live is None).
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.84,
            sz: 5.2,
            tag: OrderTag::EntryBuy,
        }]);
        let cid = e.oms.working_orders("Y")[0].client_id;
        // Bid moves; strategy cancels. Exposure must SURVIVE the cancel.
        e.execute(vec![Action::CancelToken("Y".into())]);
        assert!(
            (e.oms.pending_buy_qty("Y") - 5.2).abs() < 1e-9,
            "un-acked order must keep counting as exposure after cancel"
        );
        assert_eq!(
            e.oms.resting_buy_price("Y"),
            None,
            "pending-cancel order must not look like a resting quote"
        );
        // Ack arrives -> deferred cancel fires and exposure is released.
        e.on_event(Event::OrderAck {
            client_id: cid,
            result: Ok("ex-race".into()),
        });
        assert_eq!(e.oms.pending_buy_qty("Y"), 0.0);
        assert_eq!(e.oms.working_count("Y"), 0);
    }

    #[test]
    fn reconcile_defers_amend_while_ack_in_flight() {
        let mut e = engine_cfg(|c| {
            c.general.dry_run = false;
            c.general.order_throttle_ms = 0;
        });
        e.user_ws_ready = true;
        let bid = |px: f64| crate::types::TargetOrder {
            side: Side::Buy,
            px,
            sz: 5.4,
            tag: OrderTag::QuoteBid,
        };
        // Quote placed, ack in flight.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.50)] }]);
        assert_eq!(e.oms.working_count("Y"), 1);
        // Amend attempt while un-acked: must NOT place a second order.
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.52)] }]);
        let w = e.oms.working_orders("Y");
        assert_eq!(w.len(), 1, "no double-quote while ack in flight: {w:?}");
        assert!((w[0].px - 0.50).abs() < 1e-9, "old quote kept");
        // Ack lands; next reconcile amends normally.
        let cid = w[0].client_id;
        e.on_event(Event::OrderAck { client_id: cid, result: Ok("ex-q1".into()) });
        e.execute(vec![Action::Targets { token: "Y".into(), orders: vec![bid(0.52)] }]);
        let w = e.oms.working_orders("Y");
        assert_eq!(w.len(), 1);
        assert!((w[0].px - 0.52).abs() < 1e-9, "amend applied after ack");
    }

    #[test]
    fn tp_on_opposite_token_survives_other_sides_entry_management() {
        // Regression for the live churn loop (2026-07-12): the TP close is
        // a BUY resting on the OPPOSITE token; that side's stale-entry
        // logic must not cancel it.
        let mut e = engine_cfg(|c| {
            c.general.dry_run = false;
            c.general.order_throttle_ms = 0;
        });
        e.user_ws_ready = true;

        // NO entry fills at 0.81.
        e.execute(vec![Action::Place {
            token: "N".into(),
            side: Side::Buy,
            px: 0.81,
            sz: 5.2,
            tag: OrderTag::EntryBuy,
        }]);
        let cid = e.oms.working_orders("N")[0].client_id;
        e.on_event(Event::OrderAck { client_id: cid, result: Ok("ex-n".into()) });
        e.on_event(Event::UserTrade {
            token: "N".into(),
            side: Side::Buy,
            px: 0.81,
            sz: 5.2,
            order_id: "ex-n".into(),
            trade_id: "t-n".into(),
            maker: true,
        });

        // Books arrive: NO in range, YES far out of range (0.18).
        let top = |b: f64| BookTop {
            bid: Some(b),
            bid_sz: 10.0,
            ask: Some(b + 0.01),
            ask_sz: 10.0,
        };
        e.on_event(Event::Book { token: "N".into(), top: top(0.81) });
        e.on_event(Event::Book { token: "Y".into(), top: top(0.18) });

        // The taker must now have a TP BUY resting on the YES token.
        let tp_resting = |e: &Engine| {
            e.oms
                .working_orders("Y")
                .iter()
                .filter(|o| o.tag == OrderTag::TakeProfit && o.side == Side::Buy)
                .count()
        };
        assert_eq!(tp_resting(&e), 1, "TP close resting on the opposite token");
        // Ack it so it is fully Open.
        let tp_cid = e.oms.working_orders("Y")[0].client_id;
        e.on_event(Event::OrderAck { client_id: tp_cid, result: Ok("ex-tp".into()) });

        // Repeated YES book updates (out-of-range bid) must NOT cancel the
        // TP — this was the infinite cancel/re-place churn.
        for _ in 0..5 {
            e.on_event(Event::Book { token: "Y".into(), top: top(0.18) });
            e.on_event(Event::Book { token: "N".into(), top: top(0.81) });
        }
        assert_eq!(tp_resting(&e), 1, "TP must survive the other side's entry management");
        let o = &e.oms.working_orders("Y")[0];
        assert_eq!(o.exchange_id.as_deref(), Some("ex-tp"), "same order, never churned");
    }

    #[test]
    fn pair_netting_realizes_locked_pnl() {
        // Entry: BUY 5.2 YES @ 0.80. Close: BUY 5.2 NO @ 0.19 (≡ sell YES
        // at 0.81). Pair redeems $1 -> locked PnL = (1 - .80 - .19) * 5.2.
        let mut e = engine_cfg(|c| {
            c.general.dry_run = false;
            c.general.order_throttle_ms = 0;
        });
        e.user_ws_ready = true;
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.80,
            sz: 5.2,
            tag: OrderTag::EntryBuy,
        }]);
        let cid_y = e.oms.working_orders("Y")[0].client_id;
        e.on_event(Event::OrderAck { client_id: cid_y, result: Ok("ex-y".into()) });
        e.on_event(Event::UserTrade {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.80,
            sz: 5.2,
            order_id: "ex-y".into(),
            trade_id: "t-y".into(),
            maker: true,
        });
        assert!((e.positions.get("Y").unwrap().position - 5.2).abs() < 1e-9);

        // TP close: buy the opposite token.
        e.execute(vec![Action::Place {
            token: "N".into(),
            side: Side::Buy,
            px: 0.19,
            sz: 5.2,
            tag: OrderTag::TakeProfit,
        }]);
        let cid_n = e.oms.working_orders("N")[0].client_id;
        e.on_event(Event::OrderAck { client_id: cid_n, result: Ok("ex-n".into()) });
        e.on_event(Event::UserTrade {
            token: "N".into(),
            side: Side::Buy,
            px: 0.19,
            sz: 5.2,
            order_id: "ex-n".into(),
            trade_id: "t-n".into(),
            maker: true,
        });

        // Both legs netted; locked PnL realized and credited to YES.
        let y = e.positions.get("Y").unwrap();
        let n = e.positions.get("N").unwrap();
        assert_eq!(y.position, 0.0, "YES netted flat");
        assert_eq!(n.position, 0.0, "NO netted flat");
        let expected = (1.0 - 0.80 - 0.19) * 5.2;
        assert!(
            (y.realized_pnl - expected).abs() < 1e-9,
            "locked PnL {expected} credited to the held side, got {}",
            y.realized_pnl
        );
        assert_eq!(n.realized_pnl, 0.0);
    }

    #[test]
    fn throttled_replace_keeps_resting_order() {
        let mut e = engine();
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.50,
            sz: 5.0,
            tag: OrderTag::EntryBuy,
        }]);
        assert_eq!(e.exec_dry.resting_count(), 1);
        assert_eq!(e.oms.resting_buy_price("Y"), Some(0.50));

        // Immediate cancel-and-replace: replacement throttled -> cancel is
        // skipped too, the ORIGINAL order stays resting.
        e.execute(vec![
            Action::CancelToken("Y".into()),
            Action::Place {
                token: "Y".into(),
                side: Side::Buy,
                px: 0.51,
                sz: 5.0,
                tag: OrderTag::EntryBuy,
            },
        ]);
        assert_eq!(e.exec_dry.resting_count(), 1, "book must not go naked");
        assert_eq!(e.oms.resting_buy_price("Y"), Some(0.50), "old order must remain");

        // Stop-loss bypasses the throttle immediately (cancel proceeds too).
        e.execute(vec![
            Action::CancelToken("Y".into()),
            Action::Place {
                token: "Y".into(),
                side: Side::Sell,
                px: 0.30,
                sz: 5.0,
                tag: OrderTag::StopLoss,
            },
        ]);
        assert_eq!(e.oms.resting_buy_price("Y"), None, "buy cancelled for the stop");
        assert_eq!(e.exec_dry.resting_count(), 1, "stop order resting");

        // After the interval the replacement passes normally.
        std::thread::sleep(std::time::Duration::from_millis(210));
        e.execute(vec![Action::Place {
            token: "Y".into(),
            side: Side::Buy,
            px: 0.52,
            sz: 5.0,
            tag: OrderTag::EntryBuy,
        }]);
        assert_eq!(e.oms.resting_buy_price("Y"), Some(0.52));
    }

