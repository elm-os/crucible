//! The same request sent as protobuf and as OTLP/JSON normalizes to the same records (contract 10.4).
//!
//! The one field allowed to differ is `event_bytes`, the size of the span in its own canonical request: the JSON
//! rendering spells out default values that protobuf omits, and the transcoder keeps them, so the two canonical
//! bodies differ in size. Each side's `event_bytes` is checked against its own body instead.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::{ctx, frame, generate, request_with, s, span_with};
use crucible_normalize::{NormalizedFrame, normalize};
use crucible_otlp::{TraceRequest, transcode_json};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message;

/// The canonical size of each span that becomes a record, in wire order.
fn canonical_sizes(body: &[u8], rejected: &[u8]) -> Vec<u32> {
    let f = frame(body, rejected);
    TraceRequest::scan(body, None)
        .unwrap()
        .spans()
        .filter(|s| !f.is_rejected(s.ordinal()) && s.rejection().is_none())
        .map(|s| u32::try_from(s.encoded_len()).unwrap())
        .collect()
}

fn event_bytes(f: &NormalizedFrame) -> Vec<u32> {
    f.spans.iter().map(|s| s.core.event_bytes).collect()
}

fn without_event_bytes(mut f: NormalizedFrame) -> NormalizedFrame {
    for span in &mut f.spans {
        span.core.event_bytes = 0;
    }
    f
}

/// Both encodings of `request`: protobuf, and OTLP/JSON transcoded to canonical protobuf.
fn encodings(request: &ExportTraceServiceRequest) -> (Vec<u8>, Vec<u8>) {
    let json = serde_json::to_vec(request).unwrap();
    (request.encode_to_vec(), transcode_json(&json).unwrap())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    #[test]
    fn protobuf_and_json_normalize_identically(
        request in generate::request(true),
        rejected in vec(any::<u8>(), 0..3),
    ) {
        let (proto, json) = encodings(&request);
        let ctx = ctx();
        let from_proto = normalize(&frame(&proto, &rejected), &ctx).unwrap();
        let from_json = normalize(&frame(&json, &rejected), &ctx).unwrap();
        prop_assert_eq!(event_bytes(&from_proto), canonical_sizes(&proto, &rejected));
        prop_assert_eq!(event_bytes(&from_json), canonical_sizes(&json, &rejected));
        prop_assert_eq!(without_event_bytes(from_proto), without_event_bytes(from_json));
    }
}

/// The exception above is real: for an ordinary span the two canonical sizes differ, and everything else agrees.
#[test]
fn json_spells_out_defaults_so_only_event_bytes_differs() {
    let request = request_with(
        vec![s("service.name", "svc")],
        vec![span_with(
            1,
            1,
            None,
            vec![s("gen_ai.operation.name", "chat")],
        )],
    );
    let (proto, json) = encodings(&request);
    let (a, b) = (
        normalize(&frame(&proto, &[]), &ctx()).unwrap(),
        normalize(&frame(&json, &[]), &ctx()).unwrap(),
    );
    assert_ne!(event_bytes(&a), event_bytes(&b));
    assert_eq!(a.spans[0].core.row_digest, b.spans[0].core.row_digest);
    assert_eq!(without_event_bytes(a), without_event_bytes(b));
}
