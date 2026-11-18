//! `usage_aggregate` (contract 3.1): stage B flags a span that has a descendant carrying usage or cost, in its own
//! frame or earlier in the LSN stream, so that token and cost sums count the descendant. Stage A never sets the
//! flag, the flag is outside the row digest, and the state behind it is a function of the LSN stream alone.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::{ctx, frame, int, request, s, span_with};
use crucible_normalize::{StageB, normalize, row_digest};
use crucible_records::Lsn;
use opentelemetry_proto::tonic::trace::v1::Span;
use proptest::collection::vec;
use proptest::prelude::*;

/// A span that reports `tokens` input tokens, or nothing.
fn span(trace: u8, id: u8, parent: Option<u8>, tokens: Option<i64>) -> Span {
    let attributes = tokens
        .map(|n| vec![int("gen_ai.usage.input_tokens", n)])
        .unwrap_or_default();
    span_with(trace, id, parent, attributes)
}

/// Runs the frames in LSN order through stage A and `stage_b`; the flags of the kept spans, per frame.
fn admit(stage_b: &mut StageB, first_lsn: u64, frames: &[Vec<Span>]) -> Vec<Vec<bool>> {
    frames
        .iter()
        .enumerate()
        .map(|(i, spans)| {
            let body = request(spans.clone());
            let mut f = frame(&body, &[]);
            f.lsn = Lsn::new(first_lsn + i as u64);
            let mut out = normalize(&f, &ctx()).unwrap();
            // Stage A leaves the flag unset, and the flag does not enter the digest.
            assert!(out.spans.iter().all(|r| !r.core.usage_aggregate));
            stage_b.admit(&mut out).unwrap();
            for record in &out.spans {
                assert_eq!(record.core.row_digest, row_digest(record));
            }
            out.spans.iter().map(|r| r.core.usage_aggregate).collect()
        })
        .collect()
}

fn flags(frames: &[Vec<Span>]) -> Vec<Vec<bool>> {
    admit(&mut StageB::default(), 1, frames)
}

#[test]
fn a_span_above_usage_in_its_frame_is_an_aggregate_in_any_wire_order() {
    assert_eq!(
        flags(&[vec![
            span(1, 1, None, Some(10)),
            span(1, 2, Some(1), Some(10))
        ]]),
        [[true, false]]
    );
    // The usage may sit any number of levels below; a span between that carries nothing is flagged too.
    assert_eq!(
        flags(&[vec![
            span(1, 3, Some(2), Some(5)),
            span(1, 2, Some(1), None),
            span(1, 1, None, Some(5)),
        ]]),
        [[false, true, true]]
    );
    // Siblings, a child without usage and a child that reports zero mark nothing.
    assert_eq!(
        flags(&[vec![
            span(1, 1, None, Some(10)),
            span(1, 2, Some(1), None),
            span(1, 3, Some(1), Some(0)),
        ]]),
        [[false, false, false]]
    );
    // The same span ids in another trace are other spans.
    assert_eq!(
        flags(&[vec![
            span(1, 1, None, Some(10)),
            span(2, 2, Some(1), Some(10))
        ]]),
        [[false, false]]
    );
}

#[test]
fn a_provided_cost_marks_the_parent_as_usage_does() {
    let child = span_with(1, 2, Some(1), vec![s("crucible.cost.total", "0.5")]);
    assert_eq!(
        flags(&[vec![span(1, 1, None, Some(10)), child]]),
        [[true, false]]
    );
}

#[test]
fn a_child_in_an_earlier_frame_marks_its_parent_and_a_later_one_does_not() {
    assert_eq!(
        flags(&[
            vec![span(1, 2, Some(1), Some(10))],
            vec![span(1, 1, None, Some(10))],
        ]),
        [vec![false], vec![true]]
    );
    // The mark passes through a middle span of a later frame.
    assert_eq!(
        flags(&[
            vec![span(1, 3, Some(2), Some(10))],
            vec![span(1, 2, Some(1), None)],
            vec![span(1, 1, None, Some(10))],
        ]),
        [vec![false], vec![true], vec![true]]
    );
    // The known limit: a descendant that arrives after its ancestor leaves the ancestor counted.
    assert_eq!(
        flags(&[
            vec![span(1, 1, None, Some(10))],
            vec![span(1, 2, Some(1), Some(10))],
        ]),
        [vec![false], vec![false]]
    );
}

#[test]
fn parent_cycles_end() {
    // A span that names itself as its parent is not its own descendant.
    assert_eq!(flags(&[vec![span(1, 1, Some(1), Some(3))]]), [[false]]);
    assert_eq!(
        flags(&[vec![
            span(1, 1, Some(2), Some(3)),
            span(1, 2, Some(1), Some(3))
        ]]),
        [[true, true]]
    );
}

#[test]
fn the_set_is_bounded_and_evicts_the_oldest_parent() {
    let frames = [
        vec![span(1, 2, Some(1), Some(10))],
        vec![span(1, 4, Some(3), Some(10))],
        vec![span(1, 1, None, Some(10)), span(1, 3, None, Some(10))],
    ];
    assert_eq!(flags(&frames)[2], [true, true]);
    // With room for one parent, the first is forgotten: its span is counted beside its child.
    let mut small = StageB::default().with_usage_parents(1);
    assert_eq!(admit(&mut small, 1, &frames)[2], [false, true]);
}

proptest! {
    /// The flags are a function of the LSN stream: a fresh stage B that replays the same frames gives the same
    /// flags, and a frame's own descendants always mark, whatever came before.
    #[test]
    fn replaying_the_stream_gives_the_same_flags(
        frames in vec(
            vec((1u8..3, 1u8..7, proptest::option::of(1u8..7), proptest::option::of(0i64..3)), 0..6),
            1..5,
        ),
    ) {
        let wire: Vec<Vec<Span>> = frames
            .iter()
            .map(|f| f.iter().map(|&(t, id, parent, tokens)| span(t, id, parent, tokens)).collect())
            .collect();
        let first = flags(&wire);
        prop_assert_eq!(&first, &flags(&wire));
        // A stream that starts later (a retained prefix that begins at a later frame) never flags more.
        let later = admit(&mut StageB::default(), 2, &wire[1..]);
        for (with_history, alone) in first[1..].iter().zip(&later) {
            // Stage B may drop a repeat with history that it keeps without; compare when the frames kept the
            // same spans.
            if with_history.len() == alone.len() {
                for (a, b) in with_history.iter().zip(alone) {
                    prop_assert!(*a || !*b);
                }
            }
        }
    }
}
