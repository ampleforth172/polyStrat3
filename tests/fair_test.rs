
#[cfg(test)]
mod tests {
    use poly_strat3::config::FairCfg;
    use poly_strat3::fair::FairPrice;

    fn fp() -> FairPrice {
        FairPrice::new(FairCfg::default(), 900.0)
    }

    #[test]
    fn fair_none_without_open_price() {
        let f = fp();
        assert!(f.fair_yes(100_000.0, 0.0).is_none());
    }

    #[test]
    fn fair_at_open_is_half() {
        let mut f = fp();
        f.set_open_price(100_000.0, 0.0);
        let v = f.fair_yes(100_000.0, 1.0).unwrap();
        assert!((v - 0.5).abs() < 1e-9, "fair at open price should be 0.50, got {v}");
    }

    #[test]
    fn fair_converges_near_expiry() {
        let mut f = fp();
        f.set_open_price(100_000.0, 0.0);
        // Price 0.5% up with 2 seconds left: essentially certain UP.
        let v = f.fair_yes(100_500.0, 898.0).unwrap();
        assert!(v > 0.95, "expected near-certain UP, got {v}");
        let v = f.fair_yes(99_500.0, 898.0).unwrap();
        assert!(v < 0.05, "expected near-certain DOWN, got {v}");
    }

    #[test]
    fn fair_clipped_and_none_after_expiry() {
        let mut f = fp();
        f.set_open_price(100_000.0, 0.0);
        let v = f.fair_yes(150_000.0, 899.0).unwrap();
        assert!((0.01..=0.99).contains(&v));
        assert!(f.fair_yes(100_000.0, 900.5).is_none());
    }

    #[test]
    fn fallback_vol_used_when_sparse() {
        let mut f = fp();
        f.seed_historical_vol(0.5);
        assert!((f.annualized_vol() - 0.5).abs() < 1e-9);
        // Two points only -> still fallback.
        f.update(100_000.0, 0.0);
        f.update(100_010.0, 1.0);
        assert!((f.annualized_vol() - 0.5).abs() < 1e-9);
    }


    #[test]
    fn window_boundary_rollover_via_maybe_roll_open() {
        let mut f = fp();
        f.maybe_roll_open(100_000.0, 1000.0); // seeds window [900, 1800)
        assert_eq!(f.open_price(), 100_000.0);
        f.maybe_roll_open(100_500.0, 1799.9); // same window: no change
        assert_eq!(f.open_price(), 100_000.0);
        f.maybe_roll_open(101_000.0, 1800.5); // crosses boundary at 1800
        assert_eq!(f.open_price(), 101_000.0);
        assert!((f.tte(1801.0) - 899.0).abs() < 1e-9);
    }

    #[test]
    fn vol_update_never_touches_open_price() {
        let mut f = fp();
        f.set_open_price(100_000.0, 0.0);
        // Ticks crossing a boundary feed vol only — open stays as seeded
        // until the engine rolls it explicitly from the aggregated spot.
        f.update(105_000.0, 950.0);
        assert_eq!(f.open_price(), 100_000.0);
    }

    #[test]
    fn ret_over_requires_spanned_window() {
        let mut f = fp();
        f.update(100.0, 0.0);
        f.update(101.0, 30.0);
        assert!(f.ret_over(60.0).is_none(), "window not spanned yet");
        f.update(102.0, 61.0);
        let r = f.ret_over(60.0).unwrap();
        // Base is the oldest obs >= (61-60)=1.0 -> ts=30 px=101.
        assert!((r - (102.0f64 / 101.0).ln()).abs() < 1e-12);
    }

    #[test]
    fn alpha_shift_grows_near_expiry() {
        let mut f = fp();
        f.set_open_price(100_000.0, 0.0);
        let ret = 0.0005;
        let early = f.fair_yes_with_alpha(100_000.0, 10.0, ret).unwrap()
            - f.fair_yes(100_000.0, 10.0).unwrap();
        let late = f.fair_yes_with_alpha(100_000.0, 880.0, ret).unwrap()
            - f.fair_yes(100_000.0, 880.0).unwrap();
        assert!(late > early, "same alpha_ret must move fair more near expiry (early={early}, late={late})");
    }
}
