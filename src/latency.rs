//! Tick-to-trade latency instrumentation.
//!
//! Log2-bucketed histograms over nanoseconds — O(1) record, zero allocation
//! on the hot path, and percentile reads (p50/p90/p99/p99.9) rather than
//! averages: tail latency is the number that matters in trading systems.

#[derive(Debug)]
pub struct Histogram {
    /// bucket i counts samples with floor(log2(ns)) == i.
    buckets: [u64; 64],
    count: u64,
    max_ns: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; 64],
            count: 0,
            max_ns: 0,
        }
    }
}

impl Histogram {
    pub fn record_ns(&mut self, ns: u64) {
        let idx = 63 - ns.max(1).leading_zeros() as usize;
        self.buckets[idx] += 1;
        self.count += 1;
        self.max_ns = self.max_ns.max(ns);
    }

    pub fn record(&mut self, d: std::time::Duration) {
        self.record_ns(d.as_nanos().min(u128::from(u64::MAX)) as u64);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Upper bound of the bucket containing the p-th percentile sample.
    /// Bucketing is log2, so values are within 2x of the true percentile —
    /// the right resolution for order-of-magnitude latency work.
    pub fn percentile_ns(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((p / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in self.buckets.iter().enumerate() {
            seen += c;
            if seen >= target {
                return 1u64 << (i + 1); // bucket upper bound
            }
        }
        self.max_ns
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn summary(&self) -> String {
        if self.count == 0 {
            return "n=0".into();
        }
        format!(
            "n={} p50<{} p90<{} p99<{} p99.9<{} max={}",
            self.count,
            fmt_ns(self.percentile_ns(50.0)),
            fmt_ns(self.percentile_ns(90.0)),
            fmt_ns(self.percentile_ns(99.0)),
            fmt_ns(self.percentile_ns(99.9)),
            fmt_ns(self.max_ns),
        )
    }
}

pub fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.2}s", ns as f64 / 1e9)
    } else if ns >= 1_000_000 {
        format!("{:.2}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.1}µs", ns as f64 / 1e3)
    } else {
        format!("{ns}ns")
    }
}

/// Per-stage latency of the quoting cycle.
#[derive(Debug, Default)]
pub struct StageStats {
    /// Full event dispatch (on_event entry to exit).
    pub dispatch: Histogram,
    /// Strategy compute only (snapshot + decision).
    pub decision: Histogram,
    /// Engine wake (event received) → order handed to the executor.
    pub tick_to_order: Histogram,
    /// Journal record cost (serialize + buffered write + periodic flush).
    /// Runs AFTER dispatch, off the tick-to-order critical path.
    pub journal: Histogram,
}

impl StageStats {
    pub fn report(&self) -> String {
        format!(
            "latency: dispatch[{}] decision[{}] tick_to_order[{}] journal[{}]",
            self.dispatch.summary(),
            self.decision.summary(),
            self.tick_to_order.summary(),
            self.journal.summary(),
        )
    }

    pub fn reset(&mut self) {
        self.dispatch.reset();
        self.decision.reset();
        self.tick_to_order.reset();
        self.journal.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_reflect_distribution() {
        let mut h = Histogram::default();
        // 99 samples at ~1µs, 1 sample at ~1ms.
        for _ in 0..99 {
            h.record_ns(1_000);
        }
        h.record_ns(1_000_000);
        assert_eq!(h.count(), 100);
        let p50 = h.percentile_ns(50.0);
        assert!(p50 <= 2_048, "p50 in the ~1µs bucket, got {p50}");
        let p999 = h.percentile_ns(99.9);
        assert!(p999 >= 1_000_000, "tail must land in the ms bucket, got {p999}");
        assert_eq!(h.max_ns, 1_000_000);
    }

    #[test]
    fn empty_histogram_safe() {
        let h = Histogram::default();
        assert_eq!(h.percentile_ns(99.0), 0);
        assert_eq!(h.summary(), "n=0");
    }

    #[test]
    fn buckets_are_log2_bounded() {
        let mut h = Histogram::default();
        h.record_ns(700); // bucket [512,1024)
        assert_eq!(h.percentile_ns(100.0), 1024);
        // Formatting sanity.
        assert_eq!(fmt_ns(512), "512ns");
        assert_eq!(fmt_ns(1_500), "1.5µs");
        assert_eq!(fmt_ns(2_000_000), "2.00ms");
    }
}
