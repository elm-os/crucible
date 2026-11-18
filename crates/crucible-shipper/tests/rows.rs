//! Rows and their columns, routing, and block contents (contract sections 3.9, 5.2 and 6.4).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::cmp::Ordering;
use std::collections::BTreeMap;

use common::{BASE, EPOCH, column_index, decode, decoded_rows, distinct, normalized};
use crucible_chfmt::Compression;
use crucible_records::{
    CivilDate, HeadKind, Lsn, NO_HEAD_START, PartitionId, Pick, TableId, Timestamp, TraceId,
    TraceStatsRow,
};
use crucible_schema::{Granularity, TableKind, route, table};
use crucible_shipper::tables::{
    RoutedRow, derive, span_core_row, span_payload_row, trace_head, trace_head_row, trace_stats,
    trace_stats_row,
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
                let anchor = if weekly { date.monday_on_or_before() } else { date };
                prop_assert_eq!(routed.partition, PartitionId::date(anchor));
            }
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
        for (frame, admission) in normalized(&frames) {
            spans += frame.spans.len() as u64;
            for routed in derive(&frame, &admission, Granularity::Day).unwrap() {
                let RoutedRow { table, partition, row } = routed;
                pairs.entry((table, partition)).or_default().push(row);
            }
        }
        let mut stats_spans = 0u64;
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
                let count = column_index(def, "span_count");
                for row in data {
                    let Value::U64(n) = row[count] else { panic!("span_count") };
                    stats_spans += n;
                }
            }
        }
        prop_assert_eq!(core_rows, spans);
        prop_assert_eq!(stats_spans, spans);
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
