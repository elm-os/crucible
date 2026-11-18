//! The zero-loss seam: which requests a run got acknowledged, and the check against what a store returns.
//!
//! A request counts as acknowledged when it was answered 2xx with an `ExportTraceServiceResponse` that
//! rejected no span. Every span of such a request must be readable afterwards (invariant I1). A request that
//! was answered 2xx with rejected spans is not counted: the response says how many spans were rejected, not
//! which, so none of its spans can be held to the check.
//!
//! No reader exists yet: ingest and the read API land in a later wave. [`SequenceReader`] is the interface
//! the store-backed reader will implement; [`check_zero_loss`] is the comparison it will feed.

use std::future::Future;

use crucible_records::Uuid;
use serde::Serialize;

/// Which requests of a run were acknowledged without rejected spans.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AckLedger {
    run_id: Uuid,
    spans_per_request: u32,
    requests: u64,
    acked: Vec<u64>,
}

impl AckLedger {
    /// An empty ledger for `requests` requests of `spans_per_request` spans each.
    pub fn new(run_id: Uuid, spans_per_request: u32, requests: u64) -> Self {
        Self {
            run_id,
            spans_per_request,
            requests,
            acked: vec![0; words(requests)],
        }
    }

    /// The run the ledger belongs to.
    pub fn run_id(&self) -> Uuid {
        self.run_id
    }

    /// Spans in each request.
    pub fn spans_per_request(&self) -> u32 {
        self.spans_per_request
    }

    /// Requests in the schedule.
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// Records request `index` as acknowledged. Indices outside the schedule are ignored.
    pub fn ack(&mut self, index: u64) {
        if index < self.requests {
            self.acked[(index / 64) as usize] |= 1 << (index % 64);
        }
    }

    /// Whether request `index` was acknowledged.
    pub fn is_acked(&self, index: u64) -> bool {
        index < self.requests && self.acked[(index / 64) as usize] & (1 << (index % 64)) != 0
    }

    /// How many requests were acknowledged.
    pub fn acked_requests(&self) -> u64 {
        self.acked.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    /// The requests that were not acknowledged, as sorted, disjoint, half-open `[start, end)` ranges.
    pub fn not_acked_ranges(&self) -> Vec<[u64; 2]> {
        let mut ranges = Vec::new();
        let mut open: Option<u64> = None;
        for index in 0..self.requests {
            match (self.is_acked(index), open) {
                (false, None) => open = Some(index),
                (true, Some(start)) => {
                    ranges.push([start, index]);
                    open = None;
                }
                _ => {}
            }
        }
        if let Some(start) = open {
            ranges.push([start, self.requests]);
        }
        ranges
    }

    /// The sequence numbers of every span of every acknowledged request, in increasing order.
    pub fn acked_sequences(&self) -> impl Iterator<Item = u64> + '_ {
        let spans = u64::from(self.spans_per_request);
        (0..self.requests)
            .filter(|&index| self.is_acked(index))
            .flat_map(move |index| index * spans..(index + 1) * spans)
    }
}

/// Reads back the sequence numbers a store holds for one run: the `loadgen.seq` value of every span whose
/// `loadgen.run_id` attribute is the run id, in any order, duplicates included.
pub trait SequenceReader {
    /// Why the store could not be read.
    type Error;

    /// The stored sequence numbers of `run_id`.
    fn sequences(&self, run_id: Uuid)
    -> impl Future<Output = Result<Vec<u64>, Self::Error>> + Send;
}

/// The outcome of [`check_zero_loss`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub struct LossCheck {
    /// Spans of acknowledged requests.
    pub acked_spans: u64,
    /// Spans of acknowledged requests that the store does not hold. Zero loss means zero here.
    pub lost_spans: u64,
    /// Stored sequence numbers seen more than once (every repeat counts).
    pub duplicate_spans: u64,
    /// The smallest lost sequence number.
    pub first_lost: Option<u64>,
}

/// Compares the ledger with the sequence numbers a store returned. Stored numbers outside the run's
/// schedule are ignored.
pub fn check_zero_loss(ledger: &AckLedger, stored: &[u64]) -> LossCheck {
    let total = ledger
        .requests
        .saturating_mul(u64::from(ledger.spans_per_request));
    let mut seen = vec![0u64; words(total)];
    let mut duplicate_spans = 0;
    for &seq in stored.iter().filter(|&&seq| seq < total) {
        let (word, bit) = ((seq / 64) as usize, 1u64 << (seq % 64));
        if seen[word] & bit != 0 {
            duplicate_spans += 1;
        }
        seen[word] |= bit;
    }
    let (mut acked_spans, mut lost_spans, mut first_lost) = (0, 0, None);
    for seq in ledger.acked_sequences() {
        acked_spans += 1;
        if seen[(seq / 64) as usize] & (1 << (seq % 64)) == 0 {
            lost_spans += 1;
            first_lost.get_or_insert(seq);
        }
    }
    LossCheck {
        acked_spans,
        lost_spans,
        duplicate_spans,
        first_lost,
    }
}

/// Reads the run's stored sequence numbers through `reader` and checks them against the ledger.
pub async fn verify<R: SequenceReader>(
    ledger: &AckLedger,
    reader: &R,
) -> Result<LossCheck, R::Error> {
    let stored = reader.sequences(ledger.run_id).await?;
    Ok(check_zero_loss(ledger, &stored))
}

fn words(bits: u64) -> usize {
    usize::try_from(bits.div_ceil(64)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    const RUN: Uuid = Uuid::from_v7_parts(1_767_225_600_000, 42);

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn ledger(spans: u32, acked: &BTreeSet<u64>, requests: u64) -> AckLedger {
        let mut ledger = AckLedger::new(RUN, spans, requests);
        for &index in acked {
            ledger.ack(index);
        }
        ledger
    }

    struct Stored(Vec<u64>);

    impl SequenceReader for Stored {
        type Error = ();

        async fn sequences(&self, run_id: Uuid) -> Result<Vec<u64>, ()> {
            assert_eq!(run_id, RUN);
            Ok(self.0.clone())
        }
    }

    #[test]
    fn out_of_range_acks_are_ignored() {
        let mut ledger = AckLedger::new(RUN, 2, 3);
        ledger.ack(3);
        assert_eq!(ledger.acked_requests(), 0);
        assert_eq!(ledger.not_acked_ranges(), [[0, 3]]);
        ledger.ack(1);
        assert_eq!(ledger.not_acked_ranges(), [[0, 1], [2, 3]]);
        assert_eq!(ledger.acked_sequences().collect::<Vec<_>>(), [2, 3]);
    }

    proptest! {
        /// The not-acked ranges are exactly the complement of the acked set, sorted and merged.
        #[test]
        fn ranges_are_the_complement_of_the_acked_set(
            requests in 0u64..300,
            acked in proptest::collection::btree_set(0u64..300, 0..300),
        ) {
            let ledger = ledger(3, &acked, requests);
            let ranges = ledger.not_acked_ranges();
            let mut covered = BTreeSet::new();
            let mut previous_end = None;
            for [start, end] in &ranges {
                prop_assert!(start < end);
                prop_assert!(previous_end.is_none_or(|p| p < *start), "ranges must be disjoint and merged");
                previous_end = Some(*end);
                covered.extend(*start..*end);
            }
            let expected: BTreeSet<u64> = (0..requests).filter(|i| !acked.contains(i)).collect();
            prop_assert_eq!(covered, expected);
            prop_assert_eq!(ledger.acked_requests(), acked.iter().filter(|&&i| i < requests).count() as u64);
        }

        /// A store that holds every acked span once (in any order, with unacked spans besides) shows no loss
        /// and no duplicates; dropping one acked span shows exactly that span lost; repeats are duplicates.
        #[test]
        fn the_check_finds_exactly_the_missing_and_repeated_spans(
            spans in 1u32..5,
            acked in proptest::collection::btree_set(0u64..64, 1..64),
            extra in proptest::collection::btree_set(0u64..400, 0..20),
            drop_pick: prop::sample::Index,
            repeats in 0usize..5,
        ) {
            let ledger = ledger(spans, &acked, 64);
            let acked_seqs: Vec<u64> = ledger.acked_sequences().collect();
            let unacked_extra = extra.iter().filter(|&&seq| !ledger.is_acked(seq / u64::from(spans)));
            let stored: Vec<u64> = acked_seqs.iter().rev().chain(unacked_extra).copied().collect();
            let check = block_on(verify(&ledger, &Stored(stored))).unwrap();
            prop_assert_eq!(check, LossCheck {
                acked_spans: acked_seqs.len() as u64,
                lost_spans: 0,
                duplicate_spans: 0,
                first_lost: None,
            });

            let dropped = acked_seqs[drop_pick.index(acked_seqs.len())];
            let missing: Vec<u64> = acked_seqs.iter().copied().filter(|&seq| seq != dropped).collect();
            let check = check_zero_loss(&ledger, &missing);
            prop_assert_eq!((check.lost_spans, check.first_lost), (1, Some(dropped)));

            let mut repeated = acked_seqs.clone();
            repeated.extend(std::iter::repeat_n(acked_seqs[0], repeats));
            let check = check_zero_loss(&ledger, &repeated);
            prop_assert_eq!((check.lost_spans, check.duplicate_spans), (0, repeats as u64));
        }
    }
}
