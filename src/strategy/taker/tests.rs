//! Unit tests for the taker strategy. A child module of `taker`, so private
//! items (cfg, internal helpers) stay accessible without widening visibility.

use super::*;
use crate::strategy::SideSnap;
use crate::types::BookTop;

fn cfg() -> Config {
    let mut c = Config::default();
    c.taker.order_size = 5.0;
    c.taker.max_position = 5.0;
    c
}

fn snap(bid: f64, ask: f64) -> Snap {
    Snap {
        now: 1000.0,
        tte: 600.0,
        trading_enabled: true,
        hour_utc: 12,
        yes: SideSnap {
            token: "Y".into(),
            top: Some(BookTop {
                bid: Some(bid),
                bid_sz: 100.0,
                ask: Some(ask),
                ask_sz: 100.0,
            }),
            ..Default::default()
        },
        no: SideSnap {
            token: "N".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn fill(side: Side, px: f64, sz: f64) -> FillInfo {
    FillInfo {
        outcome: Outcome::Yes,
        side,
        px,
        sz,
        tag: OrderTag::EntryBuy,
    }
}

/// A TP/SL close fill: BUY of the OPPOSITE (NO) token.
fn close_fill(tag: OrderTag, px: f64, sz: f64) -> FillInfo {
    FillInfo {
        outcome: Outcome::No,
        side: Side::Buy,
        px,
        sz,
        tag,
    }
}

#[test]
fn full_cycle_buy_fill_tp_place_tp_fill_cooldown() {
    let mut t = Taker::new(&cfg());
    // 1. In-range bid -> entry buy at bid.
    let s = snap(0.90, 0.92);
    let a = t.on_book(Outcome::Yes, &s);
    assert_eq!(a.len(), 1);
    assert!(matches!(
        &a[0],
        Action::Place { side: Side::Buy, px, sz, tag: OrderTag::EntryBuy, .. }
        if (*px - 0.90).abs() < 1e-9 && (*sz - 5.0).abs() < 1e-9
    ));

    // 2. Fill confirms -> BuyFilled.
    let mut s2 = snap(0.90, 0.92);
    s2.yes.pos = 5.0;
    s2.yes.settled = 5.0;
    s2.yes.avg_entry = 0.90;
    t.on_fill(&fill(Side::Buy, 0.90, 5.0), &s2);
    assert_eq!(t.lifecycle(Outcome::Yes), LifeCycle::BuyFilled);

    // 3. Next book -> TP = BUY the OPPOSITE token at 1 - (entry + tp):
    //    1 - 0.93 = 0.07, same qty. No settlement wait needed.
    let a = t.on_book(Outcome::Yes, &s2);
    assert_eq!(a[0], Action::CancelToken("Y".into()));
    assert!(matches!(
        &a[1],
        Action::Place { token, side: Side::Buy, px, sz, tag: OrderTag::TakeProfit }
        if token == "N" && (*px - 0.07).abs() < 1e-9 && (*sz - 5.0).abs() < 1e-9
    ));
    assert_eq!(t.lifecycle(Outcome::Yes), LifeCycle::TakeProfit);

    // 4. TP fill (BUY on NO) arrives; the engine has already netted the
    //    pair, so the YES position is flat -> back to New + cooldown.
    let mut s3 = snap(0.93, 0.95);
    s3.yes.pos = 0.0; // netted
    t.on_fill(&close_fill(OrderTag::TakeProfit, 0.07, 5.0), &s3);
    assert_eq!(t.lifecycle(Outcome::Yes), LifeCycle::New);

    // 5. Cooldown blocks a re-buy at a higher bid...
    let mut s4 = snap(0.91, 0.93);
    s4.now = s3.now + 1.0;
    assert!(t.on_book(Outcome::Yes, &s4).is_empty());
    // ...but allows it when bid drops to/below last buy price (0.90).
    let mut s5 = snap(0.90, 0.92);
    s5.now = s3.now + 1.0;
    let a = t.on_book(Outcome::Yes, &s5);
    assert_eq!(a.len(), 1, "cooldown override should re-buy at {a:?}");
}

#[test]
fn rejected_close_order_re_arms() {
    let mut t = Taker::new(&cfg());
    let mut s = snap(0.90, 0.92);
    s.yes.pos = 5.0;
    s.yes.settled = 5.0;
    s.yes.avg_entry = 0.90;
    t.on_fill(&fill(Side::Buy, 0.90, 5.0), &s);
    t.on_book(Outcome::Yes, &s); // TP placed -> TakeProfit
    assert_eq!(t.lifecycle(Outcome::Yes), LifeCycle::TakeProfit);
    // TP got rejected/cancelled: no working orders, position remains.
    let mut s2 = snap(0.90, 0.92);
    s2.yes.pos = 5.0;
    s2.yes.settled = 5.0;
    s2.yes.avg_entry = 0.90;
    s2.yes.working_orders = 0;
    let a = t.on_book(Outcome::Yes, &s2);
    // Re-armed to BuyFilled and TP re-placed in the same drive cycle
    // (recovery runs before the TP block).
    assert!(
        a.iter().any(|x| matches!(x, Action::Place { tag: OrderTag::TakeProfit, .. })),
        "close order must be re-placed after a reject: {a:?}"
    );
}

#[test]
fn stop_loss_buys_opposite_at_complement_even_in_cutoff() {
    let mut t = Taker::new(&cfg());
    let mut s = snap(0.35, 0.40);
    s.trading_enabled = false; // pre-expiry cutoff must NOT block SL
    s.yes.pos = 5.0;
    s.yes.avg_entry = 0.90;
    t.on_fill(&fill(Side::Buy, 0.90, 5.0), &s);
    let a = t.on_book(Outcome::Yes, &s);
    // Cancel BOTH tokens' resting orders, then BUY NO @ 1 - 0.35 = 0.65.
    assert_eq!(a[0], Action::CancelToken("Y".into()));
    assert_eq!(a[1], Action::CancelToken("N".into()));
    assert!(matches!(
        &a[2],
        Action::Place { token, side: Side::Buy, px, sz, tag: OrderTag::StopLoss }
        if token == "N" && (*px - 0.65).abs() < 1e-9 && (*sz - 5.0).abs() < 1e-9
    ));
    assert_eq!(t.lifecycle(Outcome::Yes), LifeCycle::StopLoss);
}

#[test]
fn cutoff_blocks_new_buys() {
    let mut t = Taker::new(&cfg());
    let mut s = snap(0.90, 0.92);
    s.trading_enabled = false;
    assert!(t.on_book(Outcome::Yes, &s).is_empty());
}

#[test]
fn out_of_range_bid_no_buy() {
    let mut t = Taker::new(&cfg());
    assert!(t.on_book(Outcome::Yes, &snap(0.80, 0.82)).is_empty()); // below 0.85
    assert!(t.on_book(Outcome::Yes, &snap(0.97, 0.99)).is_empty()); // above 0.95
}

#[test]
fn hourly_override_side_and_price_min() {
    let mut c = cfg();
    c.taker.hourly.insert(
        "22".into(),
        crate::config::HourlyOverride {
            trade_sides: Some(vec!["NO".into()]),
            order_price_min: Some(0.6),
        },
    );
    let mut t = Taker::new(&c);
    // At hour 22, YES is disabled...
    let mut s = snap(0.90, 0.92);
    s.hour_utc = 22;
    assert!(t.on_book(Outcome::Yes, &s).is_empty());
    // ...and NO buys are allowed down to 0.6.
    let mut s = Snap {
        hour_utc: 22,
        trading_enabled: true,
        now: 1000.0,
        no: SideSnap {
            token: "N".into(),
            top: Some(BookTop {
                bid: Some(0.65),
                bid_sz: 10.0,
                ask: Some(0.70),
                ask_sz: 10.0,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    s.yes.token = "Y".into();
    let a = t.on_book(Outcome::No, &s);
    assert_eq!(a.len(), 1);
}

#[test]
fn pending_buy_counts_against_max_position() {
    let mut t = Taker::new(&cfg());
    let mut s = snap(0.90, 0.92);
    s.yes.pending_buy = 5.0;
    s.yes.resting_buy_px = Some(0.90);
    // Bid unchanged -> nothing to do (no double-buy).
    assert!(t.on_book(Outcome::Yes, &s).is_empty());
}

#[test]
fn resting_buy_replaced_when_bid_moves() {
    let mut t = Taker::new(&cfg());
    // Place initial buy at 0.90.
    let s = snap(0.90, 0.92);
    t.on_book(Outcome::Yes, &s);
    // Bid moves to 0.89 with our order resting at 0.90.
    let mut s2 = snap(0.89, 0.91);
    s2.yes.pending_buy = 5.0;
    s2.yes.resting_buy_px = Some(0.90);
    let a = t.on_book(Outcome::Yes, &s2);
    assert_eq!(a[0], Action::CancelToken("Y".into()));
    assert!(matches!(
        &a[1],
        Action::Place { side: Side::Buy, px, .. } if (*px - 0.89).abs() < 1e-9
    ));
}

#[test]
fn stale_resting_buy_cancelled_when_out_of_range() {
    let mut t = Taker::new(&cfg());
    t.on_book(Outcome::Yes, &snap(0.90, 0.92));
    let mut s2 = snap(0.80, 0.82); // below order_price_min
    s2.yes.pending_buy = 5.0;
    s2.yes.resting_buy_px = Some(0.90);
    let a = t.on_book(Outcome::Yes, &s2);
    assert_eq!(a, vec![Action::CancelToken("Y".into())]);
}
