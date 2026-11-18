//! Properties of the layered mapping over random spans of every dialect (contract 10.2 to 10.4).
//!
//! The generator draws attribute keys, scope names and event names from every dialect table, so the requests
//! below carry spans of every dialect, on every hinted path. Over them:
//!
//! - mapping is total: every frame that scans normalizes, whatever the hint and the SDK identity;
//! - mapping is idempotent: normalizing a frame again gives the same records;
//! - the protobuf and the JSON encoding of a request give the same records;
//! - detection is deterministic: a span's dialect is a function of its scope name, its attribute keys and the
//!   hint, equal to what the detection rules of the tables give when they are evaluated one by one, and equal
//!   whether the production tables are asked through the key index or mapper by mapper;
//! - a span's dialect decides `source_format`, and nothing else is needed to map it.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::{ctx, frame, generate};
use crucible_compat::{Confidence, DetectOn, TABLES};
use crucible_normalize::{MAPPERS, NormalizedFrame, SpanMapper, normalize, normalize_with};
use crucible_otlp::{TraceRequest, transcode_json};
use crucible_records::SourceFormat;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use prost::Message;

/// SDK identities a compat route can report: none, and names with versions on both sides of a major.
const SDKS: [(&str, &str); 5] = [
    ("", ""),
    ("python", "5.2.0"),
    ("python", "2.60.3"),
    ("javascript", "6.1.1"),
    ("other", "not a version"),
];

fn run(body: &[u8], hint: u8, sdk: (&str, &str)) -> NormalizedFrame {
    let mut f = frame(body, &[]);
    f.compat_dialect = hint;
    (f.sdk_name, f.sdk_version) = sdk;
    let mut out = normalize(&f, &ctx()).unwrap();
    // `event_bytes` is the span's size in its own canonical body (see `proto_json.rs`).
    for span in &mut out.spans {
        span.core.event_bytes = 0;
    }
    out
}

/// The dialect the detection rules give a span, evaluated table by table: the highest confidence, ties to the
/// lowest id, the hint counting as a detection of its dialect at confidence 60, else 0.
fn reference_dialect(scope: &str, keys: &[&str], hint: u8) -> u8 {
    let mut best: Option<(Confidence, u8)> = None;
    for table in TABLES {
        let hinted = (table.dialect != 0 && table.dialect == hint).then_some(Confidence::HINT);
        let confidence = table
            .detect
            .iter()
            .filter(|rule| match rule.on {
                DetectOn::ScopeName(name) => name.matches(scope),
                DetectOn::SpanKey(key) => keys.iter().any(|k| key.matches(k)),
            })
            .map(|rule| rule.confidence)
            .chain(hinted)
            .max();
        if let Some(confidence) = confidence
            && best.is_none_or(|(c, _)| confidence > c)
        {
            best = Some((confidence, table.dialect));
        }
    }
    best.map_or(0, |(_, dialect)| dialect)
}

proptest! {
    // A failing case is not persisted: its text would carry dialect keys into this crate.
    #![proptest_config(ProptestConfig {
        cases: 600,
        failure_persistence: Some(Box::new(FileFailurePersistence::Off)),
        ..ProptestConfig::default()
    })]

    #[test]
    fn mapping_is_total_idempotent_and_the_same_for_both_encodings(
        request in generate::request(true),
        hint in prop_oneof![3 => 0u8..=8, 1 => any::<u8>()],
        sdk in proptest::sample::select(SDKS.to_vec()),
    ) {
        let proto = request.encode_to_vec();
        let json = transcode_json(&serde_json::to_vec(&request).unwrap()).unwrap();
        let from_proto = run(&proto, hint, sdk);
        prop_assert_eq!(&from_proto, &run(&proto, hint, sdk));
        prop_assert_eq!(&from_proto, &run(&json, hint, sdk));
        for record in &from_proto.spans {
            let c = &record.core;
            prop_assert!(usize::from(c.compat_dialect) < TABLES.len());
            prop_assert_eq!(c.source_format == SourceFormat::Otel, c.compat_dialect == 0);
        }
    }

    #[test]
    fn detection_is_deterministic(
        request in generate::request(false),
        hint in prop_oneof![3 => 0u8..=8, 1 => any::<u8>()],
    ) {
        let body = request.encode_to_vec();
        let mut f = frame(&body, &[]);
        f.compat_dialect = hint;
        let indexed = normalize(&f, &ctx()).unwrap();
        // The production mappers asked one by one choose and map as the key index does.
        let mappers: Vec<&dyn SpanMapper> = MAPPERS.iter().map(|m| m as &dyn SpanMapper).collect();
        prop_assert_eq!(&indexed, &normalize_with(&mappers, &f, &ctx()).unwrap());
        // Each record names the dialect that the rules give its span.
        let scanned = TraceRequest::scan(&body, None).unwrap();
        let spans = scanned.spans().filter(|s| s.rejection().is_none());
        let mut records = indexed.spans.iter();
        for span in spans {
            let keys: Vec<&str> = span.attributes().map(|(k, _)| k).collect();
            let expected = reference_dialect(span.scope().name(), &keys, hint);
            let record = records.next().unwrap();
            prop_assert_eq!(record.core.span_id, span.span_id());
            prop_assert_eq!(record.core.compat_dialect, expected);
        }
        prop_assert!(records.next().is_none());
    }
}

/// A fixed request maps to the same records whatever happened before: the engine keeps no state between spans
/// or frames.
#[test]
fn mapping_does_not_depend_on_earlier_frames() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strategy = generate::request(true);
    let bodies: Vec<Vec<u8>> = (0..40)
        .map(|_| {
            let request: ExportTraceServiceRequest =
                strategy.new_tree(&mut runner).unwrap().current();
            request.encode_to_vec()
        })
        .collect();
    let alone: Vec<NormalizedFrame> = bodies.iter().map(|b| run(b, 1, ("", ""))).collect();
    for (i, body) in bodies.iter().enumerate().rev() {
        // Other frames, other hints and other SDK identities in between.
        let _ = run(&bodies[(i + 7) % bodies.len()], 5, ("python", "5.2.0"));
        assert_eq!(run(body, 1, ("", "")), alone[i], "frame {i}");
    }
}
