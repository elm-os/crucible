//! Stage A and stage B together, on real records: a client retry, even re-encoded as OTLP/JSON, drops at stage B as
//! an exact repeat; a changed span survives and becomes the remembered row; a rootless trace gets a provisional
//! head.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::{ctx, frame, request_with, s, span_with};
use crucible_normalize::{NormalizedFrame, StageB, normalize};
use crucible_otlp::transcode_json;
use crucible_records::{Lsn, SpanId};
use prost::Message;

/// Stage A of `body` as the frame at `lsn`.
fn at(body: &[u8], lsn: u64) -> NormalizedFrame {
    let mut f = frame(body, &[]);
    f.lsn = Lsn::new(lsn);
    normalize(&f, &ctx()).unwrap()
}

#[test]
fn a_retry_drops_at_stage_b_and_a_changed_span_survives() {
    let mut request = request_with(
        vec![s("service.name", "svc")],
        vec![
            span_with(1, 1, None, vec![s("user.id", "u")]),
            span_with(1, 2, Some(1), vec![s("gen_ai.request.model", "m")]),
            // Trace 2 arrives without its root.
            span_with(2, 3, Some(9), Vec::new()),
            span_with(2, 4, Some(3), Vec::new()),
        ],
    );
    let mut stage = StageB::default();
    let mut first = at(&request.encode_to_vec(), 7);
    let admitted = stage.admit(&mut first).unwrap();
    assert_eq!((admitted.duplicates, first.spans.len()), (0, 4));
    assert_eq!(admitted.provisional_heads, vec![2]);
    assert_eq!(first.spans[2].core.span_id, SpanId::from_bytes([3; 8]));

    // The same request re-sent as OTLP/JSON: a new LSN and other canonical bytes, but the same digests.
    let json = transcode_json(&serde_json::to_vec(&request).unwrap()).unwrap();
    let mut retry = at(&json, 8);
    assert_ne!(
        retry.spans[0].core.event_bytes,
        first.spans[0].core.event_bytes
    );
    let admitted = stage.admit(&mut retry).unwrap();
    assert_eq!(admitted.duplicates, 4);
    assert!(retry.spans.is_empty() && admitted.provisional_heads.is_empty());

    // One changed attribute: only that span is kept, and it becomes the remembered row of its key.
    request.resource_spans[0].scope_spans[0].spans[1].attributes[0] =
        s("gen_ai.request.model", "other");
    let body = request.encode_to_vec();
    let mut changed = at(&body, 9);
    let admitted = stage.admit(&mut changed).unwrap();
    assert_eq!(admitted.duplicates, 3);
    let kept: Vec<SpanId> = changed.spans.iter().map(|r| r.core.span_id).collect();
    assert_eq!(kept, [SpanId::from_bytes([2; 8])]);
    let mut again = at(&body, 10);
    assert_eq!(stage.admit(&mut again).unwrap().duplicates, 4);
}
