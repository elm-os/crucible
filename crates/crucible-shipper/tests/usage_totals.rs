//! Which spans count toward a trace's token and cost totals (contracts 3.1 and 3.9): every span, whatever its
//! type, unless it has a descendant that carries usage or cost in the same request or earlier in the LSN stream
//! (`usage_aggregate`). Each case runs stage A, stage B and the row derivation, and sums the `trace_stats`
//! partials.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crucible_normalize::{IdentityCtx, IngestFrame, NormCtx, PriceTable, StageB, normalize};
use crucible_records::{
    ApiKeyId, Lsn, ObservationType, PayloadCaps, ProjectId, SpanRecord, Timestamp, WalEpoch,
};
use crucible_shipper::tables::trace_stats;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

const BASE_NANOS: u64 = 1_794_907_800_000_000_000;

fn attribute(key: &str, value: any_value::Value) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value: Some(value) }),
        ..KeyValue::default()
    }
}

fn tokens(input: i64, output: i64) -> Vec<KeyValue> {
    vec![
        attribute(
            "gen_ai.usage.input_tokens",
            any_value::Value::IntValue(input),
        ),
        attribute(
            "gen_ai.usage.output_tokens",
            any_value::Value::IntValue(output),
        ),
    ]
}

fn span(id: u8, parent: Option<u8>, attributes: Vec<KeyValue>) -> Span {
    Span {
        trace_id: vec![9; 16],
        span_id: vec![id; 8],
        parent_span_id: parent.map(|p| vec![p; 8]).unwrap_or_default(),
        name: format!("span-{id}"),
        kind: 1,
        start_time_unix_nano: BASE_NANOS,
        end_time_unix_nano: BASE_NANOS + 1_000_000,
        attributes,
        ..Span::default()
    }
}

/// Normalizes the requests in LSN order through stage A and one stage B, and returns every kept record.
fn ship(requests: Vec<Vec<Span>>) -> Vec<SpanRecord> {
    let mut stage_b = StageB::default();
    let mut records = Vec::new();
    for (i, spans) in requests.into_iter().enumerate() {
        let body = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: None,
                scope_spans: vec![ScopeSpans {
                    scope: None,
                    spans,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
        .encode_to_vec();
        let frame = IngestFrame {
            wal_epoch: WalEpoch::from_bytes([1; 16]),
            lsn: Lsn::new(10 + i as u64),
            received_at: Timestamp::from_micros(1_794_907_801_000_000),
            project_id: ProjectId::from_bytes([2; 16]),
            api_key_id: ApiKeyId::from_bytes([3; 16]),
            compat_dialect: 0,
            sdk_name: "",
            sdk_version: "",
            rejected: &[],
            otlp: &body,
        };
        let ctx = NormCtx::new(PriceTable::bundled().unwrap(), PayloadCaps::DEFAULT);
        let mut normalized = normalize(&frame, &ctx).unwrap();
        stage_b
            .admit(&mut normalized, &IdentityCtx::Suppressed)
            .unwrap();
        records.extend(normalized.spans);
    }
    records
}

/// The trace's (total tokens, total cost in nano-USD), merged from the partial of every record.
fn totals(records: &[SpanRecord]) -> (u64, i64) {
    records.iter().fold((0, 0), |(t, c), record| {
        let partial = trace_stats(record, false);
        (
            t + partial.total_tokens,
            c + partial.total_cost_nusd.nanos(),
        )
    })
}

fn flags(records: &[SpanRecord]) -> Vec<(u8, bool)> {
    records
        .iter()
        .map(|r| (r.core.span_id.as_bytes()[0], r.core.usage_aggregate))
        .collect()
}

/// Golden 1: a native span typed `span` with a provided cost and no descendants counts toward the trace total.
#[test]
fn a_plain_span_with_a_provided_cost_counts() {
    let cost = attribute(
        "crucible.cost.total",
        any_value::Value::StringValue("0.25".to_owned()),
    );
    let records = ship(vec![vec![span(1, None, vec![cost])]]);
    assert_eq!(records[0].core.observation_type, ObservationType::Span);
    assert_eq!(flags(&records), [(1, false)]);
    assert_eq!(totals(&records), (0, 250_000_000));
    // A tool-typed span that carries tokens counts too.
    let mut attributes = tokens(7, 3);
    attributes.push(attribute(
        "gen_ai.operation.name",
        any_value::Value::StringValue("execute_tool".to_owned()),
    ));
    let records = ship(vec![vec![span(1, None, attributes)]]);
    assert_eq!(records[0].core.observation_type, ObservationType::Tool);
    assert_eq!(totals(&records), (10, 0));
}

/// Golden 2: a cumulative root and its usage-bearing child in one request: the root is excluded, the child is
/// counted, in either wire order, and the root keeps its own usage.
#[test]
fn a_root_with_a_usage_bearing_child_in_its_request_is_excluded() {
    for root_first in [true, false] {
        let root = span(1, None, tokens(100, 20));
        let child = span(2, Some(1), tokens(100, 20));
        let request = if root_first {
            vec![root, child]
        } else {
            vec![child, root]
        };
        let mut records = ship(vec![request]);
        records.sort_by_key(|r| r.core.span_id.as_bytes()[0]);
        assert_eq!(flags(&records), [(1, true), (2, false)]);
        assert_eq!(records[0].core.total_tokens, 120);
        assert_eq!(totals(&records), (120, 0));
    }
}

/// Golden 3: the same split across two requests, the child first: the root is excluded. A span between them that
/// carries nothing passes the mark on to the root.
#[test]
fn a_root_whose_usage_bearing_child_came_in_an_earlier_request_is_excluded() {
    let records = ship(vec![
        vec![span(2, Some(1), tokens(100, 20))],
        vec![span(1, None, tokens(100, 20))],
    ]);
    assert_eq!(flags(&records), [(2, false), (1, true)]);
    assert_eq!(totals(&records), (120, 0));

    let records = ship(vec![
        vec![span(3, Some(2), tokens(100, 20))],
        vec![span(2, Some(1), Vec::new())],
        vec![span(1, None, tokens(100, 20))],
    ]);
    assert_eq!(flags(&records), [(3, false), (2, true), (1, true)]);
    assert_eq!(totals(&records), (120, 0));
}

/// Golden 4: the child alone, its parent missing: the child counts.
#[test]
fn a_child_whose_parent_never_arrives_counts() {
    let records = ship(vec![vec![span(2, Some(1), tokens(100, 20))]]);
    assert_eq!(flags(&records), [(2, false)]);
    assert_eq!(totals(&records), (120, 0));
}

/// Golden 5: the parent alone, its children sampled out: the parent counts. So does a parent whose only child
/// carries no usage.
#[test]
fn a_parent_whose_children_were_sampled_out_counts() {
    let records = ship(vec![vec![span(1, None, tokens(100, 20))]]);
    assert_eq!(flags(&records), [(1, false)]);
    assert_eq!(totals(&records), (120, 0));

    let records = ship(vec![vec![
        span(1, None, tokens(100, 20)),
        span(2, Some(1), Vec::new()),
    ]]);
    assert_eq!(flags(&records), [(1, false), (2, false)]);
    assert_eq!(totals(&records), (120, 0));
}

/// The known limit: a descendant that arrives after its ancestor is counted beside it. Another trace or another
/// project never marks a span.
#[test]
fn a_late_descendant_is_counted_beside_its_ancestor() {
    let records = ship(vec![
        vec![span(1, None, tokens(100, 20))],
        vec![span(2, Some(1), tokens(100, 20))],
    ]);
    assert_eq!(flags(&records), [(1, false), (2, false)]);
    assert_eq!(totals(&records), (240, 0));

    let mut other_trace = span(2, Some(1), tokens(1, 1));
    other_trace.trace_id = vec![8; 16];
    let records = ship(vec![vec![other_trace, span(1, None, tokens(100, 20))]]);
    assert!(records.iter().all(|r| !r.core.usage_aggregate));
}
