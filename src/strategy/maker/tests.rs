//! Unit tests for the maker strategy. A child module of `maker`, so private
//! items (cfg, internal helpers) stay accessible without widening visibility.

use super::*;
use crate::strategy::SideSnap;
use crate::types::{BookTop, Outcome};

fn cfg() -> Config {
    let mut c = Config::default();
    c.maker.quote_size = 5.0;
    c.maker.fee_spread_factor = 0.0; // exact-price tests below
    c.alpha.enabled = false;
    c
}

fn snap(center: f64) -> Snap {
    Snap {
        now: 100.0,
        tte: 800.0,
        trading_enabled: true,
        spot: Some(100_000.0),
        fair_yes: Some(center),
        quote_center_yes: Some(center),
        momentum_ret: None,
        hour_utc: 12,
        yes: SideSnap {
            token: "Y".into(),
            top: Some(BookTop {
                bid: Some(center - 0.02),
                bid_sz: 10.0,
                ask: Some(center + 0.02),
                ask_sz: 10.0,
            }),
            ..Default::default()
        },
        no: SideSnap {
            token: "N".into(),
            top: Some(BookTop {
                bid: Some(1.0 - center - 0.02),
                bid_sz: 10.0,
                ask: Some(1.0 - center + 0.02),
                ask_sz: 10.0,
            }),
            ..Default::default()
        },
    }
}

#[test]
fn spread_monotone_in_tte_and_inventory() {
    let m = Maker::new(&cfg());
    let early = m.half_spread(900.0, 900.0, 0.0, None);
    let late = m.half_spread(100.0, 900.0, 0.0, None);
    assert!(late > early);
    let flat = m.half_spread(800.0, 900.0, 0.0, None);
    let skewed = m.half_spread(800.0, 900.0, 0.8, None);
    assert!(skewed > flat);
}

#[test]
fn momentum_factor_inert_below_threshold_then_scales() {
    let m = Maker::new(&cfg()); // threshold 0.001, multiplier 2.0
    let base = m.half_spread(800.0, 900.0, 0.0, None);
    let below = m.half_spread(800.0, 900.0, 0.0, Some(0.0005));
    assert!((below - base).abs() < 1e-12, "below threshold must not widen");
    let at = m.half_spread(800.0, 900.0, 0.0, Some(0.001));
    assert!((at - base).abs() < 1e-12, "exactly at threshold: factor 1");
    let above = m.half_spread(800.0, 900.0, 0.0, Some(0.002));
    // factor = 1 + 2.0 * (2 - 1) = 3.
    assert!((above - (base * 3.0).min(m.cfg.max_spread / 2.0)).abs() < 1e-12);
    // Extreme move respects the max-spread cap.
    let capped = m.half_spread(800.0, 900.0, 0.0, Some(1.0));
    assert!((capped - m.cfg.max_spread / 2.0).abs() < 1e-12);
}

#[test]
fn quotes_symmetric_around_center_when_flat() {
    let m = Maker::new(&cfg());
    let q = m.compute_quotes(&snap(0.50), 900.0);
    // Flat book, no inventory to sell -> YES bid + NO bid only.
    assert_eq!(q.len(), 2);
    let yes_bid = q.iter().find(|x| x.token == "Y").unwrap();
    let no_bid = q.iter().find(|x| x.token == "N").unwrap();
    assert_eq!(yes_bid.side, Side::Buy);
    assert_eq!(no_bid.side, Side::Buy);
    // Symmetric: yes_bid ≈ no_bid for a 0.50 center.
    assert!((yes_bid.px - no_bid.px).abs() < 1e-9);
    assert!(yes_bid.px < 0.50);
}

#[test]
fn out_of_range_rounded_prices_not_quoted() {
    let m = Maker::new(&cfg());
    // High center: ask rounds >= 1 -> NaN -> no YES ask, hence no NO
    // quote either; only the YES bid survives.
    let q = m.compute_quotes(&snap(0.98), 900.0);
    assert_eq!(q.len(), 1, "only the valid side quoted: {q:?}");
    assert_eq!(q[0].token, "Y");
    assert!(q[0].px > 0.0 && q[0].px < 1.0);
    // Low center: bid rounds <= 0 -> NaN -> no YES bid; the NO quote
    // (from the still-valid ask) survives.
    let q = m.compute_quotes(&snap(0.02), 900.0);
    assert_eq!(q.len(), 1, "only the valid side quoted: {q:?}");
    assert_eq!(q[0].token, "N");
    assert!(q[0].px > 0.0 && q[0].px < 1.0);
}

#[test]
fn fee_rate_widens_the_spread() {
    // Default fee_spread_factor = 1.0 vs the pinned 0 in cfg().
    let mut c_fee = cfg();
    c_fee.maker.fee_spread_factor = 1.0;
    let with_fee = Maker::new(&c_fee);
    let without = Maker::new(&cfg());
    let q_fee = with_fee.compute_quotes(&snap(0.50), 900.0);
    let q_raw = without.compute_quotes(&snap(0.50), 900.0);
    let bid_fee = q_fee.iter().find(|q| q.token == "Y").unwrap().px;
    let bid_raw = q_raw.iter().find(|q| q.token == "Y").unwrap().px;
    assert!(
        bid_fee < bid_raw,
        "fee widening must lower the bid: {bid_fee} vs {bid_raw}"
    );
    // At p=0.5 the maker fee unit is 0.5*0.25*(0.25)^2*0.8 = 0.00625,
    // which moves the 0.47 bid down one tick to 0.46.
    assert!((bid_raw - 0.47).abs() < 1e-9);
    assert!((bid_fee - 0.46).abs() < 1e-9);
}

#[test]
fn maker_only_skips_crossing_quotes() {
    // Our YES bid computes ~0.47 at center 0.50; drop the YES ask to
    // 0.40 so that bid would CROSS -> must be skipped, NO quote stays.
    let m = Maker::new(&cfg()); // maker_only defaults to true
    let mut s = snap(0.50);
    s.yes.top = Some(BookTop {
        bid: Some(0.39),
        bid_sz: 10.0,
        ask: Some(0.40),
        ask_sz: 10.0,
    });
    let q = m.compute_quotes(&s, 900.0);
    assert_eq!(q.len(), 1, "crossing YES bid must be skipped: {q:?}");
    assert_eq!(q[0].token, "N");

    // maker_only = false places it anyway.
    let mut c = cfg();
    c.maker.maker_only = false;
    let m = Maker::new(&c);
    let q = m.compute_quotes(&s, 900.0);
    assert_eq!(q.len(), 2, "crossing quote allowed when maker_only off: {q:?}");

    // Absent ask side cannot be crossed -> quote allowed.
    let m = Maker::new(&cfg());
    let mut s2 = snap(0.50);
    s2.yes.top = Some(BookTop {
        bid: Some(0.39),
        bid_sz: 10.0,
        ask: None,
        ask_sz: 0.0,
    });
    let q = m.compute_quotes(&s2, 900.0);
    assert_eq!(q.len(), 2, "no ask side -> nothing to cross: {q:?}");
}

#[test]
fn buy_only_maps_ask_to_complement_no_buy() {
    let m = Maker::new(&cfg());
    let mut s = snap(0.50);
    s.yes.pos = 10.0; // even with inventory, never sell
    let q = m.compute_quotes(&s, 900.0);
    assert_eq!(q.len(), 2, "buy-only collapses to 2 orders: {q:?}");
    assert!(q.iter().all(|x| x.side == Side::Buy), "no SELL ever in buy-only: {q:?}");
    let yes_bid = q.iter().find(|x| x.token == "Y").unwrap().px;
    let no_bid = q.iter().find(|x| x.token == "N").unwrap().px;
    // Long YES + skew: the NO buy (≡ selling YES) must be the more
    // attractive of the two, encouraging inventory reduction.
    assert!(no_bid > yes_bid, "expected NO bid > YES bid when long YES: {q:?}");
}

#[test]
fn inventory_cap_quotes_reduce_only() {
    let mut c = cfg();
    c.maker.max_inventory = 10.0;
    let m = Maker::new(&c);
    let mut s = snap(0.50);
    s.yes.pos = 12.0; // net +12 > cap
    s.yes.avg_entry = 0.50;
    let q = m.compute_quotes(&s, 900.0);
    // No YES bid (would add long); YES ask + NO bid (reduce) allowed.
    assert!(
        !q.iter().any(|x| x.token == "Y" && x.side == Side::Buy),
        "capped side must not add inventory: {q:?}"
    );
    assert!(q.iter().any(|x| x.token == "N" && x.side == Side::Buy));
}

#[test]
fn skew_shifts_quotes_down_when_long() {
    let m = Maker::new(&cfg());
    let flat = m.compute_quotes(&snap(0.50), 900.0);
    let mut s = snap(0.50);
    s.yes.pos = 10.0; // long YES -> shift ladder down
    s.yes.avg_entry = 0.50;
    let long = m.compute_quotes(&s, 900.0);
    let flat_bid = flat.iter().find(|x| x.token == "Y").unwrap().px;
    let long_bid = long.iter().find(|x| x.token == "Y").unwrap().px;
    assert!(long_bid < flat_bid, "long inventory must lower the YES bid");
}

#[test]
fn loss_halt_cancels_everything_once() {
    let mut m = Maker::new(&cfg());
    let mut s = snap(0.50);
    s.yes.realized_pnl = -10.0; // beyond max_loss_per_market = 5
    let a = m.on_tick(&s, 900.0);
    assert_eq!(a[0], Action::CancelAll);
    assert!(matches!(a[1], Action::Halt(_)));
    assert!(m.is_halted());
    assert!(m.on_tick(&s, 900.0).is_empty(), "halted maker stays silent");
}

#[test]
fn cutoff_pulls_quotes_and_stays_out() {
    let mut m = Maker::new(&cfg());
    let s = snap(0.50);
    let a = m.on_tick(&s, 900.0);
    assert!(has_targets(&a));
    let mut s2 = snap(0.50);
    s2.now = 110.0;
    s2.tte = 30.0; // inside 60s cutoff
    let a = m.on_tick(&s2, 900.0);
    assert_eq!(a, vec![Action::CancelAll]);
    assert!(m.on_tick(&s2, 900.0).is_empty());
}

#[test]
fn requote_on_center_drift_but_not_noise() {
    let mut m = Maker::new(&cfg());
    let s = snap(0.50);
    assert!(!m.on_tick(&s, 900.0).is_empty(), "first tick always quotes");
    // Tiny drift within max_quote_drift, refresh not due -> silent.
    let mut s2 = snap(0.505);
    s2.now = 101.0;
    assert!(m.on_tick(&s2, 900.0).is_empty());
    // Drift beyond max_quote_drift (0.01) -> requote.
    let mut s3 = snap(0.52);
    s3.now = 101.5;
    let a = m.on_tick(&s3, 900.0);
    assert!(has_targets(&a));
}

fn yes_bid_px(actions: &[Action]) -> f64 {
    actions
        .iter()
        .find_map(|x| match x {
            Action::Targets { token, orders } if token == "Y" => orders
                .iter()
                .find(|t| t.side == Side::Buy)
                .map(|t| t.px),
            _ => None,
        })
        .expect("no YES bid in actions")
}

fn has_targets(actions: &[Action]) -> bool {
    actions.iter().any(|x| matches!(x, Action::Targets { orders, .. } if !orders.is_empty()))
}

#[test]
fn aggressive_amends_limited_to_one_per_interval() {
    let mut m = Maker::new(&cfg()); // aggressive_amend_interval_secs = 1.0
    // t=100.0: initial quote — not an amend.
    let a = m.on_tick(&snap(0.50), 900.0);
    assert_eq!(yes_bid_px(&a), 0.47);
    // t=100.2: center up -> first aggressive amend allowed.
    let mut s = snap(0.52);
    s.now = 100.2;
    let a = m.on_tick(&s, 900.0);
    assert_eq!(yes_bid_px(&a), 0.49);
    // t=100.4: center up again within 1s -> blocked, holds 0.49.
    let mut s = snap(0.54);
    s.now = 100.4;
    let a = m.on_tick(&s, 900.0);
    assert_eq!(yes_bid_px(&a), 0.49, "second aggressive amend within 1s must hold");
    // t=101.5: interval elapsed -> aggressive move allowed again.
    let mut s = snap(0.56);
    s.now = 101.5;
    let a = m.on_tick(&s, 900.0);
    assert!(yes_bid_px(&a) > 0.49, "aggressive amend after 1s must pass");
}

#[test]
fn passive_moves_never_limited() {
    let mut m = Maker::new(&cfg());
    m.on_tick(&snap(0.50), 900.0); // bid 0.47 @ t=100
    let mut s = snap(0.52);
    s.now = 100.2;
    m.on_tick(&s, 900.0); // aggressive to 0.49, stamped
    // t=100.3: center drops -> passive bid move down, always allowed.
    let mut s = snap(0.48);
    s.now = 100.3;
    let a = m.on_tick(&s, 900.0);
    assert_eq!(yes_bid_px(&a), 0.45, "passive move must apply immediately");
    // t=100.4: back up -> aggressive again, stamp recent -> held at 0.45.
    let mut s = snap(0.52);
    s.now = 100.4;
    let a = m.on_tick(&s, 900.0);
    assert_eq!(yes_bid_px(&a), 0.45);
}

#[test]
fn aggressive_limit_disabled_with_zero() {
    let mut c = cfg();
    c.maker.aggressive_amend_interval_secs = 0.0;
    let mut m = Maker::new(&c);
    m.on_tick(&snap(0.50), 900.0);
    let mut s = snap(0.52);
    s.now = 100.1;
    assert_eq!(yes_bid_px(&m.on_tick(&s, 900.0)), 0.49);
    let mut s = snap(0.54);
    s.now = 100.2;
    assert_eq!(yes_bid_px(&m.on_tick(&s, 900.0)), 0.51, "0 disables the limit");
}

#[test]
fn requote_after_fill_and_refresh_interval() {
    let mut m = Maker::new(&cfg());
    let s = snap(0.50);
    m.on_tick(&s, 900.0);
    // Fill arrives -> next tick requotes even with no drift.
    m.on_fill(
        &FillInfo {
            outcome: Outcome::Yes,
            side: Side::Buy,
            px: 0.47,
            sz: 5.0,
            tag: OrderTag::QuoteBid,
        },
        &s,
    );
    let mut s2 = snap(0.50);
    s2.now = 100.2;
    s2.yes.pos = 5.0;
    s2.yes.avg_entry = 0.47;
    assert!(!m.on_tick(&s2, 900.0).is_empty());
    // And again after quote_refresh_secs elapse.
    let mut s3 = snap(0.50);
    s3.now = s2.now + 6.0;
    s3.yes.pos = 5.0;
    s3.yes.avg_entry = 0.47;
    assert!(!m.on_tick(&s3, 900.0).is_empty());
}
