#[cfg(test)]
mod tests {
    use poly_strat3::spot::{SpotAggregator, SpotSource};

    fn both() -> SpotAggregator {
        SpotAggregator::new(true, true, 1500)
    }

    fn binance_only() -> SpotAggregator {
        SpotAggregator::new(true, false, 1500) // config default
    }

    #[test]
    fn default_mode_chainlink_does_not_move_aggregate() {
        let mut a = binance_only();
        a.on_binance(64_000.0, 10.0, SpotSource::Depth);
        assert_eq!(a.spot(10.1), Some(64_000.0));
        // Chainlink print arrives at a very different level: aggregate
        // ignores it, but the print is still recorded for basis/logging.
        a.on_chainlink(63_950.0, 10.2);
        assert_eq!(a.spot(10.3), Some(64_000.0), "chainlink must not move the aggregate");
        assert_eq!(a.chainlink_px(), Some(63_950.0), "print still recorded");
        // Aggregate keeps following Binance.
        a.on_binance(64_100.0, 10.4, SpotSource::Depth);
        assert_eq!(a.spot(10.5), Some(64_100.0));
    }

    #[test]
    fn latest_source_tracks_update_type() {
        let mut a = binance_only();
        assert_eq!(a.latest_source(), None);
        a.on_binance(64_000.0, 10.0, SpotSource::Depth);
        assert_eq!(a.latest_source(), Some(SpotSource::Depth));
        a.on_binance(64_001.0, 10.1, SpotSource::Trade);
        assert_eq!(a.latest_source(), Some(SpotSource::Trade));
        assert_eq!(a.spot(10.2), Some(64_001.0), "trade px is the latest");
        a.on_binance(64_002.0, 10.2, SpotSource::Depth);
        assert_eq!(a.latest_source(), Some(SpotSource::Depth));
        // A Chainlink print never changes the Binance source.
        a.on_chainlink(63_900.0, 10.3);
        assert_eq!(a.latest_source(), Some(SpotSource::Depth));
    }

    #[test]
    fn default_mode_stale_binance_yields_no_spot() {
        let mut a = binance_only();
        a.on_binance(64_000.0, 10.0, SpotSource::Depth);
        a.on_chainlink(63_950.0, 10.1);
        // Binance stale (> 1500ms): with the Chainlink leg disabled there is
        // no fallback — no spot, so strategies stop quoting on a dead feed.
        assert_eq!(a.spot(12.0), None);
    }

    #[test]
    fn extrapolates_chainlink_with_binance_returns() {
        let mut a = both();
        a.on_binance(100_050.0, 10.0, SpotSource::Depth);
        a.on_chainlink(100_000.0, 10.1); // anchor = 100_050
        a.on_binance(100_150.0, 10.3, SpotSource::Depth);
        let s = a.spot(10.4).unwrap();
        let expected = 100_000.0 * 100_150.0 / 100_050.0;
        assert!((s - expected).abs() < 1e-6, "got {s}, want {expected}");
        // A fresh Chainlink print re-anchors: agg snaps back to its level.
        a.on_chainlink(100_120.0, 11.0);
        let s = a.spot(11.0).unwrap();
        assert!((s - 100_120.0).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_chainlink_when_binance_stale() {
        let mut a = both();
        a.on_binance(100_050.0, 10.0, SpotSource::Depth);
        a.on_chainlink(100_000.0, 10.1);
        a.on_binance(101_000.0, 10.2, SpotSource::Depth);
        // 2 seconds later (> 1500ms stale) -> raw Chainlink.
        assert_eq!(a.spot(12.5), Some(100_000.0));
    }

    #[test]
    fn binance_only_before_first_chainlink() {
        let mut a = both();
        assert_eq!(a.spot(1.0), None);
        a.on_binance(100_050.0, 1.0, SpotSource::Depth);
        assert_eq!(a.spot(1.1), Some(100_050.0));
    }

    #[test]
    fn binance_disabled_returns_raw_chainlink() {
        let mut a = SpotAggregator::new(false, true, 1500);
        a.on_binance(200_000.0, 1.0, SpotSource::Depth);
        a.on_chainlink(100_000.0, 1.1);
        a.on_binance(300_000.0, 1.2, SpotSource::Depth);
        assert_eq!(a.spot(1.3), Some(100_000.0));
    }

    #[test]
    fn no_anchor_until_binance_seen_before_chainlink() {
        let mut a = both();
        a.on_chainlink(100_000.0, 1.0); // no binance yet -> no anchor
        a.on_binance(100_500.0, 1.1, SpotSource::Depth);
        // Without an anchor the ratio is undefined: raw Chainlink.
        assert_eq!(a.spot(1.2), Some(100_000.0));
        // Next Chainlink print anchors and extrapolation starts.
        a.on_chainlink(100_010.0, 2.0);
        a.on_binance(100_600.0, 2.1, SpotSource::Depth);
        let s = a.spot(2.2).unwrap();
        let expected = 100_010.0 * 100_600.0 / 100_500.0;
        assert!((s - expected).abs() < 1e-6);
    }
}