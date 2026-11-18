//! Encoding and compression throughput for 1,000,000 rows shaped like `span_core` (contract section 6.4), sent as
//! blocks of at most 262,144 rows. Run with
//!
//! ```text
//! cargo test --release -p crucible-chfmt --bench throughput -- --ignored --nocapture
//! ```
// Test code throughout (the run is an ignored test): lets the helpers use `unwrap` under the workspace lints.
#![cfg(test)]
#![allow(clippy::disallowed_methods)] // a timing run reads the wall clock

use std::time::{Duration, Instant};

use crucible_chfmt::{
    ArrayColumn, Block, Column, Compression, MAX_BLOCK_ROWS, NullableColumn, StringColumn,
    UuidColumn,
};

const ROWS: usize = 1_000_000;

const DT64: &str = "DateTime64(6, 'UTC')";
const ENUMS: [(&str, &str); 11] = [
    (
        "kind",
        "Enum8('unspecified' = 0, 'internal' = 1, 'server' = 2, 'client' = 3, 'producer' = 4, 'consumer' = 5)",
    ),
    ("status_code", "Enum8('unset' = 0, 'ok' = 1, 'error' = 2)"),
    (
        "observation_type",
        "Enum8('span' = 0, 'generation' = 1, 'event' = 2, 'embedding' = 3, 'agent' = 4, 'tool' = 5, 'chain' = 6, \
         'retriever' = 7, 'evaluator' = 8, 'guardrail' = 9)",
    ),
    (
        "level",
        "Enum8('debug' = 0, 'default' = 1, 'warning' = 2, 'error' = 3)",
    ),
    (
        "trace_public",
        "Enum8('unset' = 0, 'private' = 1, 'public' = 2)",
    ),
    (
        "cost_source",
        "Enum8('none' = 0, 'provided' = 1, 'calculated' = 2)",
    ),
    ("mcp_side", "Enum8('none' = 0, 'client' = 1, 'server' = 2)"),
    (
        "io_shape",
        "Enum8('raw' = 0, 'genai_messages' = 1, 'indexed_messages' = 2)",
    ),
    ("input_truncation", "Enum8('none' = 0, 'cap_exceeded' = 1)"),
    ("output_truncation", "Enum8('none' = 0, 'cap_exceeded' = 1)"),
    (
        "source_format",
        "Enum8('otel' = 0, 'crucible_sdk' = 1, 'crucible_proxy' = 2, 'compat' = 3)",
    ),
];
const STRINGS: [&str; 13] = [
    "name",
    "status_message",
    "service_version",
    "scope_version",
    "telemetry_sdk_version",
    "sdk_version",
    "release",
    "version",
    "trace_name",
    "user_id",
    "session_id",
    "prompt_name",
    "prompt_version",
];
const LOW_CARDINALITY: [&str; 9] = [
    "service_name",
    "scope_name",
    "telemetry_sdk_name",
    "telemetry_sdk_language",
    "sdk_name",
    "environment",
    "model_requested",
    "model_response",
    "provider",
];
const COUNTERS: [&str; 6] = [
    "duration_us",
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "input_bytes",
    "output_bytes",
];
const WORDS: [&str; 16] = [
    "chat",
    "gpt-4o",
    "claude",
    "tool",
    "retrieve",
    "embed",
    "search",
    "summarize",
    "agent",
    "plan",
    "openai",
    "anthropic",
    "prod",
    "staging",
    "python",
    "typescript",
];
const PREVIEW: &str = "The user asked for a summary of the quarterly report; the assistant called the search tool, read \
                       three documents and answered with a short list of findings and two follow-up questions.";

/// Deterministic xorshift64 values.
struct Values(u64);

impl Values {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        for chunk in out.chunks_mut(8) {
            let len = chunk.len();
            chunk.copy_from_slice(&self.next().to_le_bytes()[..len]);
        }
        out
    }

    fn word(&mut self) -> &'static str {
        WORDS[(self.next() % WORDS.len() as u64) as usize]
    }
}

/// The columns of `span_core`, in TableDef order.
struct SpanCore {
    project_id: UuidColumn,
    start_minute: Vec<u32>,
    trace_hash: Vec<u32>,
    start_time: Vec<i64>,
    end_time: Vec<i64>,
    span_id: Vec<[u8; 8]>,
    parent_span_id: Vec<[u8; 8]>,
    trace_id: Vec<[u8; 16]>,
    flags: [Vec<u8>; 3],
    enums: [Vec<i8>; 11],
    strings: [StringColumn; 13],
    low_cardinality: [StringColumn; 9],
    counters: [Vec<u64>; 6],
    completion_start_time: NullableColumn<i64>,
    string_arrays: [ArrayColumn<StringColumn>; 5],
    usage_values: ArrayColumn<Vec<u64>>,
    cost_values: ArrayColumn<Vec<i64>>,
    total_cost_nusd: Vec<i64>,
    ids: [UuidColumn; 3],
    u32s: [Vec<u32>; 3],
    compat_dialect: Vec<u8>,
    previews: [StringColumn; 2],
    hashes: [Vec<[u8; 32]>; 2],
    row_digest: Vec<u64>,
    wal_epoch: UuidColumn,
    batch_seq: Vec<u64>,
    lsn: Vec<u64>,
    received_at: Vec<i64>,
}

impl SpanCore {
    fn new() -> Self {
        let n = MAX_BLOCK_ROWS;
        Self {
            project_id: UuidColumn::with_capacity(n),
            start_minute: Vec::with_capacity(n),
            trace_hash: Vec::with_capacity(n),
            start_time: Vec::with_capacity(n),
            end_time: Vec::with_capacity(n),
            span_id: Vec::with_capacity(n),
            parent_span_id: Vec::with_capacity(n),
            trace_id: Vec::with_capacity(n),
            flags: std::array::from_fn(|_| Vec::with_capacity(n)),
            enums: std::array::from_fn(|_| Vec::with_capacity(n)),
            strings: std::array::from_fn(|_| StringColumn::with_capacity(n, n * 12)),
            low_cardinality: std::array::from_fn(|_| StringColumn::with_capacity(n, n * 8)),
            counters: std::array::from_fn(|_| Vec::with_capacity(n)),
            completion_start_time: NullableColumn::with_capacity(n),
            string_arrays: std::array::from_fn(|_| {
                ArrayColumn::with_capacity(n, StringColumn::with_capacity(2 * n, 16 * n))
            }),
            usage_values: ArrayColumn::with_capacity(n, Vec::with_capacity(2 * n)),
            cost_values: ArrayColumn::with_capacity(n, Vec::with_capacity(2 * n)),
            total_cost_nusd: Vec::with_capacity(n),
            ids: std::array::from_fn(|_| UuidColumn::with_capacity(n)),
            u32s: std::array::from_fn(|_| Vec::with_capacity(n)),
            compat_dialect: Vec::with_capacity(n),
            previews: std::array::from_fn(|_| StringColumn::with_capacity(n, n * 160)),
            hashes: std::array::from_fn(|_| Vec::with_capacity(n)),
            row_digest: Vec::with_capacity(n),
            wal_epoch: UuidColumn::with_capacity(n),
            batch_seq: Vec::with_capacity(n),
            lsn: Vec::with_capacity(n),
            received_at: Vec::with_capacity(n),
        }
    }

    fn columns(&mut self) -> Vec<&mut dyn Column> {
        let mut columns: Vec<&mut dyn Column> = vec![
            &mut self.project_id,
            &mut self.start_minute,
            &mut self.trace_hash,
            &mut self.start_time,
            &mut self.end_time,
            &mut self.span_id,
            &mut self.parent_span_id,
            &mut self.trace_id,
        ];
        columns.extend(self.flags.iter_mut().map(|c| c as &mut dyn Column));
        columns.extend(self.enums.iter_mut().map(|c| c as &mut dyn Column));
        columns.extend(self.strings.iter_mut().map(|c| c as &mut dyn Column));
        columns.extend(
            self.low_cardinality
                .iter_mut()
                .map(|c| c as &mut dyn Column),
        );
        columns.extend(self.counters.iter_mut().map(|c| c as &mut dyn Column));
        columns.push(&mut self.completion_start_time);
        columns.extend(self.string_arrays.iter_mut().map(|c| c as &mut dyn Column));
        columns.push(&mut self.usage_values);
        columns.push(&mut self.cost_values);
        columns.push(&mut self.total_cost_nusd);
        columns.extend(self.ids.iter_mut().map(|c| c as &mut dyn Column));
        columns.extend(self.u32s.iter_mut().map(|c| c as &mut dyn Column));
        columns.push(&mut self.compat_dialect);
        columns.extend(self.previews.iter_mut().map(|c| c as &mut dyn Column));
        columns.extend(self.hashes.iter_mut().map(|c| c as &mut dyn Column));
        columns.push(&mut self.row_digest);
        columns.push(&mut self.wal_epoch);
        columns.push(&mut self.batch_seq);
        columns.push(&mut self.lsn);
        columns.push(&mut self.received_at);
        columns
    }

    fn clear(&mut self) {
        self.columns().into_iter().for_each(|c| c.clear());
    }

    /// Appends the span with sequence number `i`.
    fn push(&mut self, i: u64, values: &mut Values) {
        let start = 1_794_907_800_000_000 + i as i64 * 997;
        let duration = (values.next() % 2_000_000) as i64;
        let trace = i / 8;
        self.project_id.push([(trace % 4) as u8 + 1; 16]);
        self.start_minute.push((start / 60_000_000 * 60) as u32);
        self.trace_hash
            .push((trace.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32) as u32);
        self.start_time.push(start);
        self.end_time.push(start + duration);
        self.span_id.push(values.bytes());
        self.parent_span_id.push(if i.is_multiple_of(8) {
            [0; 8]
        } else {
            values.bytes()
        });
        let mut trace_id = [0u8; 16];
        trace_id[..8].copy_from_slice(&trace.to_le_bytes());
        self.trace_id.push(trace_id);
        for (k, flag) in self.flags.iter_mut().enumerate() {
            flag.push(u8::from((i + k as u64).is_multiple_of(8)));
        }
        for (k, column) in self.enums.iter_mut().enumerate() {
            column.push(((i + k as u64) % 3) as i8);
        }
        for (k, column) in self.strings.iter_mut().enumerate() {
            match k {
                0 | 8 => column.push(format!("{} {}", values.word(), values.word())),
                9 => column.push(format!("user-{}", trace % 1000)),
                10 => column.push(format!("session-{}", trace % 5000)),
                2..=5 => column.push("1.4.2"),
                _ => column.push(""),
            }
        }
        for column in &mut self.low_cardinality {
            column.push(values.word());
        }
        for column in &mut self.counters {
            column.push(values.next() % 10_000);
        }
        self.completion_start_time
            .push(i.is_multiple_of(3).then_some(start + duration / 4));
        for (k, column) in self.string_arrays.iter_mut().enumerate() {
            let len = (values.next() % (k as u64 + 1)) as usize;
            column.push((0..len).map(|_| values.word()));
        }
        self.usage_values
            .push([values.next() % 4096, values.next() % 4096]);
        self.cost_values.push([
            (values.next() % 100_000) as i64,
            (values.next() % 100_000) as i64,
        ]);
        self.total_cost_nusd.push((values.next() % 200_000) as i64);
        for column in &mut self.ids {
            column.push([7; 16]);
        }
        for column in &mut self.u32s {
            column.push((values.next() % 64) as u32);
        }
        self.compat_dialect.push(0);
        for column in &mut self.previews {
            let from = (values.next() % 40) as usize;
            column.push(&PREVIEW[from..from + 120]);
        }
        for column in &mut self.hashes {
            column.push(values.bytes());
        }
        self.row_digest.push(values.next());
        self.wal_epoch.push([9; 16]);
        self.batch_seq.push(i / MAX_BLOCK_ROWS as u64);
        self.lsn.push(i);
        self.received_at.push(start + 5_000_000);
    }

    fn block(&self) -> Block<'_> {
        let mut block = Block::new(self.span_id.len()).unwrap();
        let mut columns: Vec<(&str, &str, &dyn Column)> = vec![
            ("project_id", "UUID", &self.project_id),
            ("start_minute", "DateTime('UTC')", &self.start_minute),
            ("trace_hash", "UInt32", &self.trace_hash),
            ("start_time", DT64, &self.start_time),
            ("end_time", DT64, &self.end_time),
            ("span_id", "FixedString(8)", &self.span_id),
            ("parent_span_id", "FixedString(8)", &self.parent_span_id),
            ("trace_id", "FixedString(16)", &self.trace_id),
        ];
        for (name, column) in ["is_root", "is_app_root", "clock_skew"]
            .into_iter()
            .zip(&self.flags)
        {
            columns.push((name, "UInt8", column));
        }
        for ((name, ty), column) in ENUMS.into_iter().zip(&self.enums) {
            columns.push((name, ty, column));
        }
        for (name, column) in STRINGS.into_iter().zip(&self.strings) {
            columns.push((name, "String", column));
        }
        for (name, column) in LOW_CARDINALITY.into_iter().zip(&self.low_cardinality) {
            columns.push((name, "LowCardinality(String)", column));
        }
        for (name, column) in COUNTERS.into_iter().zip(&self.counters) {
            columns.push((name, "UInt64", column));
        }
        columns.push((
            "completion_start_time",
            "Nullable(DateTime64(6, 'UTC'))",
            &self.completion_start_time,
        ));
        let arrays = [
            ("trace_tags", "Array(String)"),
            ("tags", "Array(String)"),
            ("tool_names", "Array(String)"),
            ("finish_reasons", "Array(LowCardinality(String))"),
            ("usage_keys", "Array(LowCardinality(String))"),
        ];
        for ((name, ty), column) in arrays.into_iter().zip(&self.string_arrays) {
            columns.push((name, ty, column));
        }
        columns.push(("usage_values", "Array(UInt64)", &self.usage_values));
        columns.push(("cost_values", "Array(Int64)", &self.cost_values));
        columns.push(("total_cost_nusd", "Int64", &self.total_cost_nusd));
        for (name, column) in ["price_model_id", "price_tier_id", "api_key_id"]
            .into_iter()
            .zip(&self.ids)
        {
            columns.push((name, "UUID", column));
        }
        for (name, column) in ["tool_call_count", "normalizer_version", "event_bytes"]
            .into_iter()
            .zip(&self.u32s)
        {
            columns.push((name, "UInt32", column));
        }
        columns.push(("compat_dialect", "UInt8", &self.compat_dialect));
        for (name, column) in ["input_preview", "output_preview"]
            .into_iter()
            .zip(&self.previews)
        {
            columns.push((name, "String", column));
        }
        for (name, column) in ["input_sha256", "output_sha256"]
            .into_iter()
            .zip(&self.hashes)
        {
            columns.push((name, "FixedString(32)", column));
        }
        columns.push(("row_digest", "UInt64", &self.row_digest));
        columns.push(("wal_epoch", "UUID", &self.wal_epoch));
        columns.push(("batch_seq", "UInt64", &self.batch_seq));
        columns.push(("lsn", "UInt64", &self.lsn));
        columns.push(("received_at", DT64, &self.received_at));
        assert_eq!(columns.len(), 75);
        for (name, ty, data) in columns {
            block.push(name, ty, data).unwrap();
        }
        block
    }
}

fn rate(count: f64, time: Duration) -> f64 {
    count / time.as_secs_f64()
}

#[test]
#[ignore = "timing run; see the module documentation"]
fn encode_one_million_span_core_rows() {
    let mut spans = SpanCore::new();
    let mut values = Values(0x2545_f491_4f6c_dd1d);
    let (mut fill, mut lz4, mut zstd) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut native, mut lz4_bytes, mut zstd_bytes, mut blocks) = (0usize, 0usize, 0usize, 0usize);
    let mut next = 0u64;
    while next < ROWS as u64 {
        let rows = (ROWS as u64 - next).min(MAX_BLOCK_ROWS as u64);
        spans.clear();
        let started = Instant::now();
        for i in next..next + rows {
            spans.push(i, &mut values);
        }
        fill += started.elapsed();
        next += rows;
        blocks += 1;
        let block = spans.block();
        let started = Instant::now();
        let encoded = block.encode(Compression::Lz4).unwrap();
        lz4 += started.elapsed();
        native += encoded.uncompressed_size();
        lz4_bytes += encoded.compressed().len();
        let started = Instant::now();
        let encoded = block.encode(Compression::Zstd).unwrap();
        zstd += started.elapsed();
        zstd_bytes += encoded.compressed().len();
    }
    let mb = native as f64 / 1e6;
    println!(
        "{ROWS} span_core-like rows in {blocks} blocks, {mb:.1} MB of Native bytes ({} bytes per row)",
        native / ROWS
    );
    println!(
        "append to builders: {:.3} s, {:.0} rows/s",
        fill.as_secs_f64(),
        rate(ROWS as f64, fill)
    );
    for (name, time, bytes) in [("LZ4", lz4, lz4_bytes), ("ZSTD", zstd, zstd_bytes)] {
        println!(
            "encode + {name}: {:.3} s, {:.0} rows/s, {:.0} MB/s of Native bytes, {:.1} MB compressed (ratio {:.2})",
            time.as_secs_f64(),
            rate(ROWS as f64, time),
            rate(mb, time),
            bytes as f64 / 1e6,
            native as f64 / bytes as f64,
        );
    }
}
