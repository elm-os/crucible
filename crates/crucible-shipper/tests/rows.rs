//! Rows and their columns, routing, and block contents (contract sections 3.9, 5.2 and 6.4).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::cmp::Ordering;
use std::collections::BTreeMap;

use bytes::Bytes;
use common::{BASE, EPOCH, column_index, decode, decoded_rows, distinct, normalized};
use crucible_chfmt::Compression;
use crucible_records::{
    AuthorKind, Bucket, CivilDate, CorrectionBody, Fact, FactBody, FactEnvelope, FactId,
    FactTargetKind, FlagBody, HeadKind, Ingestion, Lsn, NO_HEAD_START, PartitionId, Pick,
    ScoreBody, ScoreDataType, ScoreSource, SessionRollupRow, SpanId, SpanMetricsRow, TableId,
    TagBody, TagOp, Timestamp, TraceId, TraceStatsRow, UserRollupRow, Uuid,
};
use crucible_schema::{Granularity, TableKind, route, table};
use crucible_shipper::tables::{
    RoutedRow, derive, derive_facts, fact_row, session_rollup, session_rollup_row, span_core_row,
    span_metrics, span_metrics_row, span_payload_row, trace_head, trace_head_row, trace_stats,
    trace_stats_row, user_rollup, user_rollup_row,
};
use crucible_shipper::value::{Row, Value, marker_row, sort_and_combine};
use crucible_shipper::{BatchId, pair_block};
use proptest::prelude::*;

fn s(v: &str) -> Value {
    Value::str(v)
}

fn list(items: &[&str]) -> Value {
    Value::Array(items.iter().map(|i| s(i)).collect())
}

fn u64s(items: &[u64]) -> Value {
    Value::Array(items.iter().map(|&i| Value::U64(i)).collect())
}

fn i64s(items: &[i64]) -> Value {
    Value::Array(items.iter().map(|&i| Value::I64(i)).collect())
}

/// Asserts that `cells` holds, for every column of the table and in its order, the value `expected` names.
fn assert_columns(table_id: TableId, cells: &[Value], expected: &[(&str, Value)]) {
    let def = table(table_id);
    let by_name: BTreeMap<&str, &Value> = expected.iter().map(|(n, v)| (*n, v)).collect();
    assert_eq!(by_name.len(), expected.len(), "a column is named twice");
    assert_eq!(cells.len(), def.columns.len(), "{}", def.name());
    for (column, cell) in def.columns.iter().zip(cells) {
        let want = by_name
            .get(column.name)
            .unwrap_or_else(|| panic!("{}.{} has no expected value", def.name(), column.name));
        assert_eq!(cell, *want, "{}.{}", def.name(), column.name);
    }
    assert_eq!(
        expected.len(),
        def.columns.len(),
        "an expected column does not exist"
    );
}

fn ingestion_columns() -> Vec<(&'static str, Value)> {
    vec![
        ("wal_epoch", Value::Uuid([10; 16])),
        ("batch_seq", Value::U64(88)),
        ("lsn", Value::U64(99)),
        ("received_at", Value::I64(BASE + 70_000_000)),
    ]
}

/// 09:31:01 falls in the minute that starts 60 s after the base.
fn start_minute() -> Value {
    Value::U32(((BASE + 60_000_000) / 1_000_000) as u32)
}

fn trace_hash() -> Value {
    Value::U32(TraceId::from_bytes([2; 16]).trace_hash())
}

#[test]
fn span_core_puts_every_field_in_its_column() {
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("trace_id", Value::bytes(&[2; 16])),
        ("span_id", Value::bytes(&[3; 8])),
        ("parent_span_id", Value::bytes(&[4; 8])),
        ("start_minute", start_minute()),
        ("trace_hash", trace_hash()),
        ("start_time", Value::I64(BASE + 61_000_000)),
        ("end_time", Value::I64(BASE + 62_500_000)),
        ("duration_us", Value::U64(1_500_000)),
        ("completion_start_time", Value::I64(BASE + 61_200_000)),
        ("is_root", Value::U8(1)),
        ("is_app_root", Value::U8(0)),
        ("clock_skew", Value::U8(1)),
        ("kind", Value::I8(3)),
        ("status_code", Value::I8(2)),
        ("observation_type", Value::I8(1)),
        ("level", Value::I8(2)),
        ("trace_public", Value::I8(2)),
        ("cost_source", Value::I8(1)),
        ("mcp_side", Value::I8(1)),
        ("io_shape", Value::I8(1)),
        ("input_truncation", Value::I8(0)),
        ("output_truncation", Value::I8(1)),
        ("source_format", Value::I8(3)),
        ("trace_tags", list(&["trace_tag_a", "trace_tag_b"])),
        ("tags", list(&["tag"])),
        ("tool_names", list(&["tool_a", "tool_b", "tool_c"])),
        ("finish_reasons", list(&["stop"])),
        ("usage_keys", list(&["input", "output"])),
        ("usage_values", u64s(&[11, 22])),
        ("cost_values", i64s(&[110, 220])),
        ("input_tokens", Value::U64(11)),
        ("output_tokens", Value::U64(22)),
        ("total_tokens", Value::U64(33)),
        ("total_cost_nusd", Value::I64(330)),
        ("input_bytes", Value::U64(55)),
        ("output_bytes", Value::U64(66)),
        ("price_model_id", Value::Uuid([5; 16])),
        ("price_tier_id", Value::Uuid([6; 16])),
        ("api_key_id", Value::Uuid([11; 16])),
        ("tool_call_count", Value::U32(44)),
        ("normalizer_version", Value::U32(77)),
        ("event_bytes", Value::U32(111)),
        ("compat_dialect", Value::U8(9)),
        ("input_sha256", Value::bytes(&[7; 32])),
        ("output_sha256", Value::bytes(&[8; 32])),
        ("row_digest", Value::U64(0x1234_5678_9abc_def0)),
    ];
    // Every string field of the record holds the name of its column.
    for name in [
        "name",
        "status_message",
        "service_name",
        "service_version",
        "scope_name",
        "scope_version",
        "telemetry_sdk_name",
        "telemetry_sdk_language",
        "telemetry_sdk_version",
        "sdk_name",
        "sdk_version",
        "environment",
        "release",
        "version",
        "trace_name",
        "user_id",
        "session_id",
        "model_requested",
        "model_response",
        "provider",
        "prompt_name",
        "prompt_version",
        "input_preview",
        "output_preview",
    ] {
        expected.push((name, s(name)));
    }
    expected.extend(ingestion_columns());
    expected.push(("usage_aggregate", Value::U8(0)));
    assert_columns(TableId::SpanCore, &span_core_row(&distinct()), &expected);
}

#[test]
fn span_core_writes_absent_optionals_as_null_nil_and_zero() {
    let mut record = distinct();
    record.core.completion_start_time = None;
    record.core.price_model_id = None;
    record.core.input_sha256 = None;
    let def = table(TableId::SpanCore);
    let cells = span_core_row(&record);
    let cell = |name: &str| &cells[column_index(def, name)];
    assert_eq!(*cell("completion_start_time"), Value::Null);
    assert_eq!(*cell("price_model_id"), Value::Uuid([0; 16]));
    assert_eq!(*cell("input_sha256"), Value::bytes(&[0; 32]));
}

#[test]
fn span_payload_puts_every_field_in_its_column() {
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("start_minute", start_minute()),
        ("trace_hash", trace_hash()),
        ("span_id", Value::bytes(&[3; 8])),
        ("trace_id", Value::bytes(&[2; 16])),
        ("start_time", Value::I64(BASE + 61_000_000)),
        ("user_id", s("user_id")),
        ("session_id", s("session_id")),
        ("metadata_keys", list(&["k1", "k2"])),
        ("metadata_values", list(&["v1", "v2"])),
        ("metadata_kv", list(&["k1=v1", "k2=v2"])),
        ("usage_provided_keys", list(&["input", "total"])),
        ("usage_provided_values", u64s(&[12, 13])),
        ("cost_provided_keys", list(&["output"])),
        ("cost_provided_values", i64s(&[14])),
        ("truncated_fields", list(&["output", "events"])),
    ];
    for name in [
        "input",
        "output",
        "system_instructions",
        "attributes",
        "resource_attributes",
        "events",
        "links",
        "tool_definitions",
        "tool_calls",
        "model_parameters",
        "trace_metadata",
        "trace_state",
    ] {
        expected.push((name, s(name)));
    }
    expected.extend(ingestion_columns());
    assert_columns(
        TableId::SpanPayload,
        &span_payload_row(&distinct()),
        &expected,
    );
}

#[test]
fn trace_heads_puts_every_field_in_its_column() {
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("start_time", Value::I64(BASE + 61_000_000)),
        ("trace_hash", trace_hash()),
        ("trace_id", Value::bytes(&[2; 16])),
        ("span_id", Value::bytes(&[3; 8])),
        ("head_kind", Value::I8(2)),
        ("end_time", Value::I64(BASE + 62_500_000)),
        ("level", Value::I8(2)),
        ("status_code", Value::I8(2)),
        ("tags", list(&["trace_tag_a", "trace_tag_b"])),
    ];
    for name in [
        "name",
        "user_id",
        "session_id",
        "environment",
        "release",
        "version",
        "input_preview",
        "output_preview",
    ] {
        expected.push((name, s(name)));
    }
    expected.extend(ingestion_columns());
    let head = trace_head(&distinct(), HeadKind::Provisional);
    assert_columns(TableId::TraceHeads, &trace_head_row(&head), &expected);
}

#[test]
fn trace_stats_puts_every_measure_in_its_column() {
    let record = distinct();
    let span = record.core.span_id;
    let end = record.core.end_time;
    let pick = |value: &str| Value::bytes(Pick::new(true, end, span, value).as_bytes());
    let day = CivilDate::new(2026, 11, 17).unwrap().day_number() as u16;
    let expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("trace_id", Value::bytes(&[2; 16])),
        ("row_day", Value::U16(day)),
        ("marker_epoch", Value::Uuid([0; 16])),
        ("marker_batch_seq", Value::U64(0)),
        ("marker_at", Value::U32(0)),
        ("min_start", Value::I64(BASE + 61_000_000)),
        ("max_end", Value::I64(BASE + 62_500_000)),
        ("has_root", Value::U8(1)),
        ("min_head_start", Value::I64(BASE + 61_000_000)),
        ("max_level", Value::U8(2)),
        ("span_count", Value::U64(1)),
        ("error_count", Value::U64(1)),
        ("generation_count", Value::U64(1)),
        ("tool_count", Value::U64(0)),
        ("mcp_count", Value::U64(1)),
        ("input_tokens", Value::U64(11)),
        ("output_tokens", Value::U64(22)),
        ("total_tokens", Value::U64(33)),
        ("total_cost_nusd", Value::I64(330)),
        ("environments", list(&["environment"])),
        ("trace_name_pick", pick("trace_name")),
        ("user_id_pick", pick("user_id")),
        ("session_id_pick", pick("session_id")),
    ];
    let stats = trace_stats(&record, true);
    assert_columns(TableId::TraceStats, &trace_stats_row(&stats), &expected);
    // A span that heads nothing leaves the head start at the sentinel that loses every `min`.
    assert_eq!(trace_stats(&record, false).min_head_start, NO_HEAD_START);
}

fn no_marker() -> Vec<(&'static str, Value)> {
    vec![
        ("marker_epoch", Value::Uuid([0; 16])),
        ("marker_batch_seq", Value::U64(0)),
        ("marker_at", Value::U32(0)),
    ]
}

fn sums() -> Vec<(&'static str, Value)> {
    vec![
        ("error_count", Value::U64(1)),
        ("input_tokens", Value::U64(11)),
        ("output_tokens", Value::U64(22)),
        ("total_tokens", Value::U64(33)),
        ("total_cost_nusd", Value::I64(330)),
    ]
}

#[test]
fn the_rollups_put_every_measure_in_its_column() {
    let record = distinct();
    let day = Value::U16(CivilDate::new(2026, 11, 17).unwrap().day_number() as u16);
    let seen = vec![
        ("first_seen", Value::I64(BASE + 61_000_000)),
        ("last_seen", Value::I64(BASE + 62_500_000)),
        ("trace_count", Value::U64(1)),
        ("span_count", Value::U64(1)),
    ];
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("day", day.clone()),
        ("session_id", s("session_id")),
        ("user_ids", list(&["user_id"])),
    ];
    expected.extend(no_marker().into_iter().chain(seen.clone()).chain(sums()));
    let session = session_rollup(&record).unwrap();
    assert_columns(
        TableId::SessionRollup,
        &session_rollup_row(&session),
        &expected,
    );

    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("day", day),
        ("user_id", s("user_id")),
    ];
    expected.extend(no_marker().into_iter().chain(seen).chain(sums()));
    let user = user_rollup(&record).unwrap();
    assert_columns(TableId::UserRollup, &user_rollup_row(&user), &expected);

    let hist = |micros: u64| {
        Value::Tuple(Box::new((
            Value::Array(vec![Value::U16(Bucket::of(micros).index())]),
            u64s(&[1]),
        )))
    };
    let mut expected = vec![
        ("project_id", Value::Uuid([1; 16])),
        ("minute", start_minute()),
        ("environment", s("environment")),
        ("observation_type", Value::I8(1)),
        ("model_response", s("model_response")),
        ("level", Value::I8(2)),
        ("release", s("release")),
        ("count", Value::U64(1)),
        // The span lasts 1.5 s and its first token arrives 200 ms after its start.
        ("dur_hist", hist(1_500_000)),
        ("ttft_hist", hist(200_000)),
    ];
    expected.extend(no_marker().into_iter().chain(sums()));
    assert_columns(
        TableId::SpanMetrics,
        &span_metrics_row(&span_metrics(&record)),
        &expected,
    );

    // Only root spans count a trace; a span without a session or a user has no partial; only generations
    // have a time to first token.
    let mut child = distinct();
    child.core.is_root = false;
    assert_eq!(session_rollup(&child).unwrap().trace_count, 0);
    assert_eq!(user_rollup(&child).unwrap().trace_count, 0);
    child.core.session_id = "".into();
    child.core.user_id = "".into();
    assert!(session_rollup(&child).is_none() && user_rollup(&child).is_none());
    child.core.observation_type = crucible_records::ObservationType::Span;
    assert!(span_metrics(&child).ttft_hist.is_empty());
}

/// Token and cost sums take a generation or an embedding that does not repeat the usage of a descendant, and
/// nothing else. Every span is still counted, and its own row keeps its usage.
#[test]
fn sums_count_generations_and_embeddings_that_are_not_usage_aggregates() {
    use crucible_records::{NanoUsd, ObservationType as T};
    for (kind, aggregate, counted) in [
        (T::Generation, false, true),
        (T::Embedding, false, true),
        (T::Generation, true, false),
        (T::Embedding, true, false),
        (T::Span, false, false),
        (T::Agent, false, false),
        (T::Tool, true, false),
    ] {
        let mut record = distinct();
        record.core.observation_type = kind;
        record.core.usage_aggregate = aggregate;
        let c = &record.core;
        assert_eq!(c.counts_usage(), counted);
        let expected = if counted {
            (
                c.input_tokens,
                c.output_tokens,
                c.total_tokens,
                c.total_cost_nusd,
            )
        } else {
            (0, 0, 0, NanoUsd::ZERO)
        };
        assert!(c.input_tokens > 0 && c.total_cost_nusd > NanoUsd::ZERO);
        let stats = trace_stats(&record, true);
        let session = session_rollup(&record).unwrap();
        let user = user_rollup(&record).unwrap();
        let metrics = span_metrics(&record);
        let sums = [
            (
                stats.input_tokens,
                stats.output_tokens,
                stats.total_tokens,
                stats.total_cost_nusd,
            ),
            (
                session.input_tokens,
                session.output_tokens,
                session.total_tokens,
                session.total_cost_nusd,
            ),
            (
                user.input_tokens,
                user.output_tokens,
                user.total_tokens,
                user.total_cost_nusd,
            ),
            (
                metrics.input_tokens,
                metrics.output_tokens,
                metrics.total_tokens,
                metrics.total_cost_nusd,
            ),
        ];
        assert_eq!(sums, [expected; 4], "{kind} aggregate={aggregate}");
        assert_eq!(
            (
                stats.span_count,
                session.span_count,
                user.span_count,
                metrics.count
            ),
            (1, 1, 1, 1)
        );
        let def = table(TableId::SpanCore);
        let cells = span_core_row(&record);
        let cell = |name: &str| &cells[column_index(def, name)];
        assert_eq!(*cell("input_tokens"), Value::U64(c.input_tokens));
        assert_eq!(
            *cell("total_cost_nusd"),
            Value::I64(c.total_cost_nusd.nanos())
        );
        assert_eq!(*cell("usage_aggregate"), Value::U8(u8::from(aggregate)));
    }
}

fn fact(body: FactBody) -> Fact {
    Fact {
        envelope: FactEnvelope {
            project_id: crucible_records::ProjectId::from_bytes([1; 16]),
            fact_id: FactId::from_bytes([7; 16]),
            target_kind: FactTargetKind::Span,
            trace_id: TraceId::from_bytes([2; 16]),
            span_id: SpanId::from_bytes([3; 8]),
            session_id: "session".into(),
            target_time: Timestamp::from_micros(BASE + 61_000_000),
            key: "key".into(),
            version: 44,
            retracted: true,
            author_kind: AuthorKind::ApiKey,
            author_id: Uuid::from_bytes([8; 16]),
            created_at: Timestamp::from_micros(BASE + 66_000_000),
            ingestion: Ingestion {
                wal_epoch: crucible_records::WalEpoch::from_bytes([10; 16]),
                batch_seq: Lsn::new(88),
                lsn: Lsn::new(99),
                received_at: Timestamp::from_micros(BASE + 70_000_000),
            },
        },
        body,
    }
}

#[test]
fn facts_put_every_field_in_its_column() {
    let day = Value::U16(CivilDate::new(2026, 11, 17).unwrap().day_number() as u16);
    let shared = || {
        let mut columns = vec![
            ("project_id", Value::Uuid([1; 16])),
            ("key", s("key")),
            ("target_day", day.clone()),
            ("trace_id", Value::bytes(&[2; 16])),
            ("span_id", Value::bytes(&[3; 8])),
            ("session_id", s("session")),
            ("version", Value::U64(44)),
            ("fact_id", Value::Uuid([7; 16])),
            ("target_kind", Value::I8(2)),
            ("target_time", Value::I64(BASE + 61_000_000)),
            ("retracted", Value::U8(1)),
            ("author_kind", Value::I8(2)),
            ("author_id", Value::Uuid([8; 16])),
            ("created_at", Value::I64(BASE + 66_000_000)),
        ];
        columns.extend(ingestion_columns());
        columns
    };
    let score = fact(FactBody::Score(ScoreBody {
        data_type: ScoreDataType::Categorical,
        value_num: 0.25,
        value_str: "good".into(),
        comment: "comment".into(),
        source: ScoreSource::Annotation,
        metadata: Bytes::from_static(b"{}"),
        config_id: Some(Uuid::from_bytes([9; 16])),
    }));
    let mut expected = shared();
    expected.extend([
        ("data_type", Value::I8(2)),
        ("value_num", Value::F64(0.25)),
        ("value_str", s("good")),
        ("comment", s("comment")),
        ("source", Value::I8(3)),
        ("metadata", s("{}")),
        ("config_id", Value::Uuid([9; 16])),
    ]);
    assert_columns(TableId::FactScores, &fact_row(&score), &expected);

    let tag = fact(FactBody::Tag(TagBody { op: TagOp::Remove }));
    let mut expected = shared();
    expected.push(("op", Value::I8(2)));
    assert_columns(TableId::FactTags, &fact_row(&tag), &expected);

    let flag = fact(FactBody::Flag(FlagBody { value: true }));
    let mut expected = shared();
    expected.push(("value", Value::U8(1)));
    assert_columns(TableId::FactFlags, &fact_row(&flag), &expected);

    let correction = fact(FactBody::Correction(CorrectionBody {
        corrected: Bytes::from_static(b"[1]"),
        note: "note".into(),
    }));
    let mut expected = shared();
    expected.extend([("corrected", s("[1]")), ("note", s("note"))]);
    assert_columns(TableId::FactCorrections, &fact_row(&correction), &expected);

    // Each fact routes to its own table by the month of its target time, and its block encodes.
    let facts = [score, tag, flag, correction];
    let rows = derive_facts(&facts, Granularity::Week).unwrap();
    let tables: Vec<TableId> = rows.iter().map(|r| r.table).collect();
    assert_eq!(
        tables,
        [
            TableId::FactScores,
            TableId::FactTags,
            TableId::FactFlags,
            TableId::FactCorrections
        ]
    );
    let batch = BatchId {
        epoch: EPOCH,
        first_lsn: Lsn::new(88),
        last_lsn: Lsn::new(99),
    };
    for (index, routed) in rows.into_iter().enumerate() {
        assert_eq!(routed.partition, PartitionId::month(2026, 11).unwrap());
        assert_eq!(
            (routed.row.lsn, routed.row.ordinal),
            (Lsn::new(99), index as u32)
        );
        let pair = (routed.table, routed.partition);
        let cells = routed.row.cells.clone();
        let block = pair_block(
            &batch,
            pair,
            vec![routed.row],
            Timestamp::EPOCH,
            Compression::Lz4,
        );
        let decoded = decoded_rows(&decode(table(pair.0), &block.unwrap()));
        assert_eq!(decoded, [cells]);
    }
}

fn arb_frames() -> impl Strategy<Value = Vec<Vec<common::SpanDesc>>> {
    let span = (
        0u8..6,
        0u8..40,
        any::<bool>(),
        0u8..3,
        0u16..120,
        0u16..5_000,
    );
    proptest::collection::vec(proptest::collection::vec(span, 1..8), 1..6)
}

proptest! {
    /// Every row routes by its span's `start_time` through `crucible_schema::route`, at both granularities.
    #[test]
    fn rows_route_by_start_time(frames in arb_frames(), weekly in any::<bool>()) {
        let granularity = if weekly { Granularity::Week } else { Granularity::Day };
        for (frame, admission) in normalized(&frames) {
            let rows = derive(&frame, &admission, granularity).unwrap();
            let per_span = rows.iter().filter(|r| r.table == TableId::SpanCore).count();
            prop_assert_eq!(per_span, frame.spans.len());
            for routed in &rows {
                let span = &frame.spans[routed.row.ordinal as usize];
                let expected = route(routed.table, granularity, span.core.start_time).unwrap();
                prop_assert_eq!(routed.partition, expected);
                prop_assert_eq!(routed.row.lsn, frame.lsn);
                let date = span.core.start_time.date();
                let monthly = matches!(
                    routed.table,
                    TableId::SessionRollup | TableId::UserRollup | TableId::SpanMetrics
                );
                if monthly {
                    let month = PartitionId::month(date.year(), date.month()).unwrap();
                    prop_assert_eq!(routed.partition, month);
                } else {
                    let anchor = if weekly { date.monday_on_or_before() } else { date };
                    prop_assert_eq!(routed.partition, PartitionId::date(anchor));
                }
            }
            // One metrics partial per span, and a session or user partial exactly for the spans that name one.
            let count = |table| rows.iter().filter(|r| r.table == table).count();
            let named = |f: fn(&crucible_records::SpanCore) -> &str| {
                frame.spans.iter().filter(|s| !f(&s.core).is_empty()).count()
            };
            prop_assert_eq!(count(TableId::SpanMetrics), frame.spans.len());
            prop_assert_eq!(count(TableId::SessionRollup), named(|c| &c.session_id));
            prop_assert_eq!(count(TableId::UserRollup), named(|c| &c.user_id));
            // A head row for every root span and every provisional head, and for no other span.
            let heads = rows.iter().filter(|r| r.table == TableId::TraceHeads).count();
            let roots = frame.spans.iter().filter(|s| s.core.is_root).count();
            prop_assert_eq!(heads, roots + admission.provisional_heads.len());
        }
    }

    /// The block of every pair, decoded again, holds exactly the rows that were derived from the normalized
    /// spans: sorted by the ORDER BY key with ties by LSN then ordinal on plain tables; combined by key, with
    /// one marker row, on `trace_stats`.
    #[test]
    fn blocks_hold_the_normalized_rows(frames in arb_frames(), zstd in any::<bool>()) {
        let compression = if zstd { Compression::Zstd } else { Compression::Lz4 };
        let batch = BatchId { epoch: EPOCH, first_lsn: Lsn::new(10), last_lsn: Lsn::new(20) };
        let marker_at = Timestamp::from_micros(BASE + 5_000_000);
        let mut pairs: BTreeMap<(TableId, PartitionId), Vec<Row>> = BTreeMap::new();
        let mut spans = 0u64;
        let (mut in_sessions, mut of_users, mut tokens) = (0u64, 0u64, 0u64);
        for (frame, admission) in normalized(&frames) {
            spans += frame.spans.len() as u64;
            for span in &frame.spans {
                in_sessions += u64::from(!span.core.session_id.is_empty());
                of_users += u64::from(!span.core.user_id.is_empty());
                if span.core.counts_usage() {
                    tokens += span.core.input_tokens;
                }
            }
            for routed in derive(&frame, &admission, Granularity::Day).unwrap() {
                let RoutedRow { table, partition, row } = routed;
                pairs.entry((table, partition)).or_default().push(row);
            }
        }
        let mut counted: BTreeMap<TableId, (u64, u64)> = BTreeMap::new();
        let mut core_rows = 0u64;
        for (pair, rows) in pairs {
            let def = table(pair.0);
            let block = pair_block(&batch, pair, rows.clone(), marker_at, compression).unwrap();
            let decoded = decoded_rows(&decode(def, &block));
            let keys: Vec<usize> = def.order_by.iter().map(|k| column_index(def, k)).collect();
            let key_order = |a: &[Value], b: &[Value]| {
                keys.iter()
                    .map(|&k| a[k].order(&b[k]))
                    .find(|o| o.is_ne())
                    .unwrap_or(Ordering::Equal)
            };
            prop_assert!(decoded.windows(2).all(|w| key_order(&w[0], &w[1]).is_le()));
            if def.kind == TableKind::Plain {
                let mut expected = rows;
                expected.sort_by(|a, b| {
                    key_order(&a.cells, &b.cells)
                        .then(a.lsn.cmp(&b.lsn))
                        .then(a.ordinal.cmp(&b.ordinal))
                });
                let expected: Vec<Vec<Value>> = expected.into_iter().map(|r| r.cells).collect();
                prop_assert_eq!(&decoded, &expected);
                let seq = column_index(def, "batch_seq");
                prop_assert!(decoded.iter().all(|r| r[seq] == Value::U64(10)));
                if pair.0 == TableId::SpanCore {
                    core_rows += decoded.len() as u64;
                }
            } else {
                let project = column_index(def, "project_id");
                let (markers, data): (Vec<_>, Vec<_>) =
                    decoded.iter().partition(|r| r[project] == Value::Uuid([0; 16]));
                prop_assert_eq!(markers.len(), 1);
                let marker = marker_row(def, pair.1, EPOCH, Lsn::new(10), marker_at);
                prop_assert_eq!(markers[0], &marker.cells);
                // Equal keys were combined: the data keys are strictly increasing.
                prop_assert!(data.windows(2).all(|w| key_order(w[0], w[1]).is_lt()));
                let name = if pair.0 == TableId::SpanMetrics { "count" } else { "span_count" };
                let count = column_index(def, name);
                let input = column_index(def, "input_tokens");
                let sum = counted.entry(pair.0).or_default();
                for row in data {
                    let (Value::U64(n), Value::U64(t)) = (&row[count], &row[input]) else {
                        panic!("a sum column")
                    };
                    sum.0 += n;
                    sum.1 += t;
                }
            }
        }
        prop_assert_eq!(core_rows, spans);
        // Every span is counted once in the trace stats and once in the metrics, and the tokens of the spans
        // that count usage (generations and embeddings that are not usage aggregates) are summed; the session
        // and user rollups count the spans that name one.
        let of = |table| counted.get(&table).copied().unwrap_or_default();
        prop_assert_eq!(of(TableId::TraceStats), (spans, tokens));
        prop_assert_eq!(of(TableId::SpanMetrics), (spans, tokens));
        prop_assert_eq!(of(TableId::SessionRollup).0, in_sessions);
        prop_assert_eq!(of(TableId::UserRollup).0, of_users);
    }

    /// Combining rollup rows by their column merge functions gives what the typed merges give.
    #[test]
    fn combining_rollups_equals_the_typed_merges(frames in arb_frames()) {
        let mut sessions: BTreeMap<Vec<u8>, SessionRollupRow> = BTreeMap::new();
        let mut users: BTreeMap<Vec<u8>, UserRollupRow> = BTreeMap::new();
        let mut metrics: BTreeMap<Vec<u8>, SpanMetricsRow> = BTreeMap::new();
        let mut rows: BTreeMap<TableId, Vec<Row>> = BTreeMap::new();
        for (frame, _) in normalized(&frames) {
            for (i, span) in frame.spans.iter().enumerate() {
                let mut push = |table, cells| {
                    rows.entry(table).or_default().push(Row { cells, lsn: frame.lsn, ordinal: i as u32 });
                };
                let project = span.core.project_id.to_bytes();
                let day = span.core.start_time.date().day_number().to_be_bytes();
                if let Some(partial) = session_rollup(span) {
                    push(TableId::SessionRollup, session_rollup_row(&partial));
                    let key = [&project[..], &day, partial.session_id.as_bytes()].concat();
                    match sessions.get_mut(&key) {
                        Some(row) => row.merge(&partial),
                        None => { sessions.insert(key, partial); }
                    }
                }
                if let Some(partial) = user_rollup(span) {
                    push(TableId::UserRollup, user_rollup_row(&partial));
                    let key = [&project[..], &day, partial.user_id.as_bytes()].concat();
                    match users.get_mut(&key) {
                        Some(row) => row.merge(&partial),
                        None => { users.insert(key, partial); }
                    }
                }
                let partial = span_metrics(span);
                push(TableId::SpanMetrics, span_metrics_row(&partial));
                // The spans of this generator differ only in project and minute among the key columns.
                let key = [&project[..], &span.core.start_minute().to_be_bytes()].concat();
                match metrics.get_mut(&key) {
                    Some(row) => row.merge(&partial),
                    None => { metrics.insert(key, partial); }
                }
            }
        }
        let mut combined = |table| {
            let mut rows = rows.remove(&table).unwrap_or_default();
            sort_and_combine(crucible_schema::table(table), &mut rows).unwrap();
            rows.into_iter().map(|r| r.cells).collect::<Vec<_>>()
        };
        let expected: Vec<Vec<Value>> = sessions.values().map(session_rollup_row).collect();
        prop_assert_eq!(combined(TableId::SessionRollup), expected);
        let expected: Vec<Vec<Value>> = users.values().map(user_rollup_row).collect();
        prop_assert_eq!(combined(TableId::UserRollup), expected);
        let expected: Vec<Vec<Value>> = metrics.values().map(span_metrics_row).collect();
        prop_assert_eq!(combined(TableId::SpanMetrics), expected);
    }

    /// Combining `trace_stats` rows by their column merge functions gives what `TraceStatsRow::merge` gives.
    #[test]
    fn combining_equals_the_typed_merge(frames in arb_frames()) {
        let def = table(TableId::TraceStats);
        let mut merged: BTreeMap<([u8; 16], [u8; 16], CivilDate), TraceStatsRow> = BTreeMap::new();
        let mut rows = Vec::new();
        for (frame, admission) in normalized(&frames) {
            for (i, span) in frame.spans.iter().enumerate() {
                let head = span.core.is_root || admission.provisional_heads.contains(&i);
                let partial = trace_stats(span, head);
                rows.push(Row {
                    cells: trace_stats_row(&partial),
                    lsn: frame.lsn,
                    ordinal: i as u32,
                });
                let key = (
                    partial.project_id.to_bytes(),
                    partial.trace_id.to_bytes(),
                    partial.row_day,
                );
                match merged.get_mut(&key) {
                    Some(row) => row.merge(&partial),
                    None => {
                        merged.insert(key, partial);
                    }
                }
            }
        }
        sort_and_combine(def, &mut rows).unwrap();
        let combined: Vec<Vec<Value>> = rows.into_iter().map(|r| r.cells).collect();
        let expected: Vec<Vec<Value>> = merged.values().map(trace_stats_row).collect();
        prop_assert_eq!(combined, expected);
    }
}
