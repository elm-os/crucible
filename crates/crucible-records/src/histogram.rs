//! The deterministic latency histogram (contract 6.5) and the MCP semantic-convention duration buckets (contract
//! 6.4).
//!
//! The latency histogram is integer log-linear with 16 sub-buckets per octave. The shipper, the query
//! post-operator and the in-process ClickHouse model all use this one implementation. Values below 32 are exact;
//! every other reported value is within 1/32 (3.125%) of every value in its bucket.

/// A bucket of the latency histogram, index 0 to 975 (stored as `UInt16`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Bucket(u16);

impl Bucket {
    /// The last bucket, which holds `u64::MAX`.
    pub const MAX: Self = Self(975);

    /// `bucket(v)`: the bucket of a value in microseconds. Total and monotone over `u64`.
    pub const fn of(v: u64) -> Self {
        if v < 16 {
            return Self(v as u16);
        }
        // Index of the most significant set bit, 4 to 63.
        let m = 63 - v.leading_zeros();
        let sub = ((v >> (m - 4)) & 15) as u32;
        Self(((m - 3) * 16 + sub) as u16)
    }

    /// The bucket with this index, or `None` above 975.
    pub const fn new(index: u16) -> Option<Self> {
        if index <= Self::MAX.0 {
            Some(Self(index))
        } else {
            None
        }
    }

    /// The stored index.
    pub const fn index(self) -> u16 {
        self.0
    }

    /// `lower(b)`: the smallest value in the bucket.
    pub const fn lower(self) -> u64 {
        let b = self.0 as u64;
        if b < 16 {
            return b;
        }
        let m = b / 16 + 3;
        (16 + b % 16) << (m - 4)
    }

    /// `width(b)`: the number of values in the bucket.
    pub const fn width(self) -> u64 {
        let b = self.0 as u64;
        if b < 16 {
            return 1;
        }
        1 << (b / 16 - 1)
    }

    /// `report(b) = lower(b) + width(b) / 2`: the value a quantile in this bucket reports.
    pub const fn report(self) -> u64 {
        self.lower() + self.width() / 2
    }
}

/// Sparse counts per bucket, merged by adding the counts of equal buckets: the `sumMap` merge, so the result does
/// not depend on merge order. Entries are kept in ascending bucket order with non-zero counts; counts saturate.
#[derive(Clone, PartialEq, Eq, Hash, Default, Debug)]
pub struct BucketCounts<B> {
    entries: Vec<(B, u64)>,
}

impl<B: Copy + Ord> BucketCounts<B> {
    /// No counts.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Adds `count` to `bucket`.
    pub fn add(&mut self, bucket: B, count: u64) {
        if count == 0 {
            return;
        }
        match self.entries.binary_search_by(|(b, _)| b.cmp(&bucket)) {
            Ok(i) => {
                if let Some(entry) = self.entries.get_mut(i) {
                    entry.1 = entry.1.saturating_add(count);
                }
            }
            Err(i) => self.entries.insert(i, (bucket, count)),
        }
    }

    /// Adds every count of `other`.
    pub fn merge(&mut self, other: &Self) {
        let mut merged = Vec::with_capacity(self.entries.len() + other.entries.len());
        let (mut a, mut b) = (
            self.entries.iter().peekable(),
            other.entries.iter().peekable(),
        );
        loop {
            let next = match (a.peek(), b.peek()) {
                (Some(&&(ka, ca)), Some(&&(kb, cb))) => match ka.cmp(&kb) {
                    core::cmp::Ordering::Less => {
                        a.next();
                        (ka, ca)
                    }
                    core::cmp::Ordering::Greater => {
                        b.next();
                        (kb, cb)
                    }
                    core::cmp::Ordering::Equal => {
                        a.next();
                        b.next();
                        (ka, ca.saturating_add(cb))
                    }
                },
                (Some(&&entry), None) => {
                    a.next();
                    entry
                }
                (None, Some(&&entry)) => {
                    b.next();
                    entry
                }
                (None, None) => break,
            };
            merged.push(next);
        }
        self.entries = merged;
    }

    /// The `(bucket, count)` entries in ascending bucket order: the two arrays of the stored `sumMap` tuple.
    pub fn iter(&self) -> impl Iterator<Item = (B, u64)> + '_ {
        self.entries.iter().copied()
    }

    /// Whether there are no counts.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The sum of the counts, saturating.
    pub fn total(&self) -> u64 {
        self.entries
            .iter()
            .fold(0u64, |sum, &(_, c)| sum.saturating_add(c))
    }
}

/// The latency histogram (`dur_hist`, `ttft_hist`): counts per [`Bucket`] of values in microseconds.
pub type Histogram = BucketCounts<Bucket>;

impl BucketCounts<Bucket> {
    /// Counts one value.
    pub fn record(&mut self, micros: u64) {
        self.add(Bucket::of(micros), 1);
    }

    /// The quantile `q_ppm`, in parts per million (990000 is p99; values above 1000000 count as 1000000):
    /// `rank = max(1, ceil(q * total / 1000000))`, then `report(b)` of the first bucket `b`, in ascending order,
    /// at which the cumulative count reaches `rank`. `None` for an empty histogram.
    pub fn quantile(&self, q_ppm: u32) -> Option<u64> {
        let total: u128 = self.entries.iter().map(|&(_, c)| u128::from(c)).sum();
        if total == 0 {
            return None;
        }
        let rank = (u128::from(q_ppm.min(1_000_000)) * total)
            .div_ceil(1_000_000)
            .max(1);
        let mut cumulative = 0u128;
        for &(bucket, count) in &self.entries {
            cumulative += u128::from(count);
            if cumulative >= rank {
                return Some(bucket.report());
            }
        }
        None
    }

    /// A histogram from a `sumMap` result (bucket indexes and counts), or `None` when the arrays differ in length
    /// or an index is above 975. Repeated indexes are summed and zero counts dropped.
    pub fn from_sum_map(buckets: &[u16], counts: &[u64]) -> Option<Self> {
        if buckets.len() != counts.len() {
            return None;
        }
        let mut histogram = Self::new();
        for (&index, &count) in buckets.iter().zip(counts) {
            histogram.add(Bucket::new(index)?, count);
        }
        Some(histogram)
    }
}

/// Upper bounds, in microseconds, of the MCP operation-duration buckets (`semconv_hist`): the explicit bucket
/// boundaries that the OpenTelemetry semantic conventions advise for `mcp.client.operation.duration` and
/// `mcp.server.operation.duration` (release v1.40.0, unchanged in the GenAI conventions repository), converted
/// from seconds.
pub const MCP_SEMCONV_DURATION_BOUNDS: [u64; 14] = [
    10_000,
    20_000,
    50_000,
    100_000,
    200_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    30_000_000,
    60_000_000,
    120_000_000,
    300_000_000,
];

/// The semantic-convention bucket of a duration, 0 to 14: bucket `i` holds `(bounds[i - 1], bounds[i]]`, bucket 0
/// everything up to the first bound and bucket 14 everything above the last (explicit-bucket semantics, upper
/// bounds inclusive).
pub fn semconv_bucket(duration_us: u64) -> u8 {
    let above = MCP_SEMCONV_DURATION_BOUNDS
        .iter()
        .take_while(|&&bound| duration_us > bound)
        .count();
    above as u8
}

/// Exact counts per semantic-convention duration bucket (`semconv_hist`, stored with `UInt8` indexes).
pub type SemconvHistogram = BucketCounts<u8>;

impl BucketCounts<u8> {
    /// Counts one duration.
    pub fn record(&mut self, duration_us: u64) {
        self.add(semconv_bucket(duration_us), 1);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn known_buckets() {
        assert_eq!(Bucket::of(0).index(), 0);
        assert_eq!(Bucket::of(15).index(), 15);
        assert_eq!(Bucket::of(16).index(), 16);
        assert_eq!(Bucket::of(31).index(), 31);
        assert_eq!(Bucket::of(32).index(), 32);
        assert_eq!(Bucket::of(33).index(), 32);
        assert_eq!(Bucket::of(u64::MAX), Bucket::MAX);
        assert_eq!(Bucket::new(976), None);
        assert_eq!(Bucket::MAX.lower(), 31 << 59);
        assert_eq!(Bucket::MAX.width(), 1 << 59);
        assert_eq!(Bucket::MAX.report(), 63 << 58);
        let b = Bucket::of(1_000_000);
        assert!(b.lower() <= 1_000_000 && 1_000_000 < b.lower() + b.width());
    }

    #[test]
    fn quantiles_follow_the_rank_rule() {
        let mut h = Histogram::new();
        assert_eq!(h.quantile(500_000), None);
        for v in 1..=100 {
            h.record(v);
        }
        // rank = ceil(0.5 * 100) = 50, which lies in the bucket of 50 (lower 50, width 2).
        assert_eq!(h.quantile(500_000), Some(Bucket::of(50).report()));
        assert_eq!(h.quantile(0), Some(1));
        assert_eq!(h.quantile(1_000_000), Some(Bucket::of(100).report()));
        assert_eq!(h.quantile(u32::MAX), h.quantile(1_000_000));
    }

    #[test]
    fn sum_map_round_trip_and_validation() {
        let mut h = Histogram::new();
        for v in [3, 3, 40, 1 << 40] {
            h.record(v);
        }
        let (buckets, counts): (Vec<u16>, Vec<u64>) = h.iter().map(|(b, c)| (b.index(), c)).unzip();
        assert_eq!(Histogram::from_sum_map(&buckets, &counts), Some(h));
        assert_eq!(Histogram::from_sum_map(&[976], &[1]), None);
        assert_eq!(Histogram::from_sum_map(&[1, 2], &[1]), None);
        let summed = Histogram::from_sum_map(&[5, 5, 7], &[1, 2, 0]).unwrap();
        assert_eq!(summed.iter().collect::<Vec<_>>(), vec![(Bucket::of(5), 3)]);
    }

    #[test]
    fn semconv_buckets_are_upper_inclusive() {
        assert_eq!(semconv_bucket(0), 0);
        assert_eq!(semconv_bucket(10_000), 0);
        assert_eq!(semconv_bucket(10_001), 1);
        assert_eq!(semconv_bucket(300_000_000), 13);
        assert_eq!(semconv_bucket(300_000_001), 14);
        assert_eq!(semconv_bucket(u64::MAX), 14);
        let mut h = SemconvHistogram::new();
        h.record(15_000);
        h.record(20_000);
        h.record(1);
        assert_eq!(h.iter().collect::<Vec<_>>(), vec![(0, 1), (1, 2)]);
    }

    /// Values from every scale: small exact ones, typical latencies, and the whole `u64` range.
    fn arb_value() -> impl Strategy<Value = u64> {
        prop_oneof![0u64..64, 0u64..10_000_000, any::<u64>()]
    }

    proptest! {
        #[test]
        fn bucket_contains_its_value(v in arb_value(), w in arb_value()) {
            let b = Bucket::of(v);
            prop_assert!(b <= Bucket::MAX);
            prop_assert!(b.lower() <= v);
            prop_assert!(u128::from(v) < u128::from(b.lower()) + u128::from(b.width()));
            prop_assert!(Bucket::of(v.min(w)) <= Bucket::of(v.max(w)));
        }

        /// Contract 6.5 and invariant I7: quantiles are within 1/32 of the exact nearest-rank quantile.
        #[test]
        fn quantiles_are_within_one_thirty_second(
            values in proptest::collection::vec(arb_value(), 1..400),
        ) {
            let mut h = Histogram::new();
            for &v in &values {
                h.record(v);
            }
            let mut sorted = values.clone();
            sorted.sort_unstable();
            for q in [500_000u32, 900_000, 990_000] {
                let n = sorted.len() as u128;
                let rank = (u128::from(q) * n).div_ceil(1_000_000).max(1);
                let exact = sorted[usize::try_from(rank - 1).unwrap()];
                let estimate = h.quantile(q).unwrap();
                prop_assert_eq!(estimate, Bucket::of(exact).report());
                prop_assert!(
                    u128::from(estimate.abs_diff(exact)) * 32 <= u128::from(exact),
                    "q={} exact={} estimate={}", q, exact, estimate
                );
            }
        }

        #[test]
        fn merge_is_recording_the_union(
            a in proptest::collection::vec(arb_value(), 0..50),
            b in proptest::collection::vec(arb_value(), 0..50),
        ) {
            let histogram = |values: &[u64]| {
                let mut h = Histogram::new();
                values.iter().for_each(|&v| h.record(v));
                h
            };
            let (ha, hb) = (histogram(&a), histogram(&b));
            let mut ab = ha.clone();
            ab.merge(&hb);
            let mut ba = hb.clone();
            ba.merge(&ha);
            let all: Vec<u64> = a.iter().chain(&b).copied().collect();
            prop_assert_eq!(&ab, &histogram(&all));
            prop_assert_eq!(&ab, &ba);
            prop_assert_eq!(ab.total(), all.len() as u64);
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Contract 4.9: `bucket` is total (every `u64` maps to an index in 0..=975 without overflow) and
    /// `lower(bucket(v)) <= v < lower(bucket(v)) + width(bucket(v))`. Also checks the 1/32 error bound of
    /// `report` (contract 6.5).
    #[kani::proof]
    fn bucket_is_total_and_contains_its_value() {
        let v: u64 = kani::any();
        let b = Bucket::of(v);
        assert!(b.index() <= Bucket::MAX.index());
        assert!(b.lower() <= v);
        assert!(u128::from(v) < u128::from(b.lower()) + u128::from(b.width()));
        assert!(u128::from(b.report().abs_diff(v)) * 32 <= u128::from(v));
    }

    /// Contract 4.9: `bucket` is monotone.
    #[kani::proof]
    fn bucket_is_monotone() {
        let a: u64 = kani::any();
        let b: u64 = kani::any();
        kani::assume(a <= b);
        assert!(Bucket::of(a) <= Bucket::of(b));
    }

    /// `lower`, `width` and `report` are total over every valid index, and each bucket starts with its own lower
    /// bound: no overflow, no empty bucket.
    #[kani::proof]
    fn bounds_are_total_over_indexes() {
        let index: u16 = kani::any();
        if let Some(b) = Bucket::new(index) {
            assert!(b.width() >= 1);
            assert!(Bucket::of(b.lower()) == b);
            assert!(b.report() >= b.lower());
        }
    }
}
