//! Seal thresholds, the block limit and the sort order (contract 5.1 and 5.2).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use crucible_env::Instant;
use crucible_records::{CivilDate, Lsn, PartitionId, TableId, Timestamp};
use crucible_schema::table;
use crucible_shipper::tables::RoutedRow;
use crucible_shipper::value::{Row, Value, sort_and_combine};
use crucible_shipper::{OpenBatch, SealLimits, ShipperConfig};

const SEAL_BYTES: u64 = 128 << 20;

fn limits() -> SealLimits {
    SealLimits::new(&ShipperConfig::default())
}

fn at(millis: u64) -> Instant {
    Instant::from_nanos(millis * 1_000_000)
}

fn day(d: u8) -> PartitionId {
    PartitionId::date(CivilDate::new(2026, 11, d).unwrap())
}

fn rows(table: TableId, partition: PartitionId, count: usize) -> Vec<RoutedRow> {
    (0..count)
        .map(|i| RoutedRow {
            table,
            partition,
            row: Row {
                cells: Vec::new(),
                lsn: Lsn::new(1),
                ordinal: i as u32,
            },
        })
        .collect()
}

#[test]
fn a_batch_seals_at_the_linger() {
    let mut batch = OpenBatch::new(Lsn::new(10));
    assert!(!batch.is_due(&limits(), SEAL_BYTES, at(10_000)));
    batch.take_frame(Lsn::new(10), Timestamp::EPOCH, 100, true, at(1_000));
    assert!(!batch.is_due(&limits(), SEAL_BYTES, at(1_499)));
    assert!(batch.is_due(&limits(), SEAL_BYTES, at(1_500)));
    // Under pressure the linger doubles, up to 2 s.
    let mut widened = limits();
    widened.widen();
    assert!(!batch.is_due(&widened, SEAL_BYTES, at(1_999)));
    assert!(batch.is_due(&widened, SEAL_BYTES, at(2_000)));
}

#[test]
fn a_batch_seals_at_131072_span_rows() {
    let mut batch = OpenBatch::new(Lsn::new(1));
    batch.take_frame(Lsn::new(1), Timestamp::EPOCH, 100, true, at(0));
    batch.add_rows(Vec::new(), 131_071);
    assert!(!batch.is_full(&limits(), SEAL_BYTES));
    assert!(!batch.is_due(&limits(), SEAL_BYTES, at(1)));
    batch.take_frame(Lsn::new(2), Timestamp::EPOCH, 100, true, at(1));
    batch.add_rows(Vec::new(), 1);
    assert!(batch.is_full(&limits(), SEAL_BYTES));
    assert!(batch.is_due(&limits(), SEAL_BYTES, at(1)));
    let mut widened = limits();
    widened.widen();
    assert!(!batch.is_full(&widened, SEAL_BYTES));
}

#[test]
fn a_batch_seals_at_128_mib_of_raw_frames() {
    let mut batch = OpenBatch::new(Lsn::new(1));
    batch.take_frame(Lsn::new(1), Timestamp::EPOCH, SEAL_BYTES - 1, true, at(0));
    assert!(!batch.is_due(&limits(), SEAL_BYTES, at(1)));
    batch.take_frame(Lsn::new(2), Timestamp::EPOCH, 1, true, at(1));
    assert!(batch.is_due(&limits(), SEAL_BYTES, at(1)));
}

#[test]
fn the_shippers_own_control_frames_never_warrant_a_seal() {
    let mut batch = OpenBatch::new(Lsn::new(5));
    assert_eq!(batch.last_lsn(), Lsn::new(4));
    for lsn in 5..50 {
        batch.take_frame(Lsn::new(lsn), Timestamp::EPOCH, SEAL_BYTES, false, at(lsn));
    }
    assert_eq!(
        (batch.first_lsn(), batch.last_lsn()),
        (Lsn::new(5), Lsn::new(49))
    );
    assert!(!batch.is_sealable());
    // Nor is such a batch ever full: it must keep reading until a frame that warrants a seal arrives.
    assert!(!batch.is_full(&limits(), SEAL_BYTES));
    assert!(!batch.is_due(&limits(), SEAL_BYTES, at(1_000_000)));
    batch.take_frame(Lsn::new(50), Timestamp::from_micros(7), 1, true, at(60));
    assert!(batch.is_sealable());
    assert_eq!(batch.marker_at(), Timestamp::from_micros(7));
    assert!(batch.is_due(&limits(), SEAL_BYTES, at(61)));
}

#[test]
fn a_frame_must_fit_every_builder_it_adds_to() {
    let mut batch = OpenBatch::new(Lsn::new(1));
    batch.take_frame(Lsn::new(1), Timestamp::EPOCH, 1, true, at(0));
    batch.add_rows(rows(TableId::SpanCore, day(17), 262_000), 262_000);
    batch.add_rows(rows(TableId::TraceStats, day(17), 262_000), 0);
    // A plain block holds 262,144 rows.
    assert!(batch.fits(&rows(TableId::SpanCore, day(17), 144)));
    assert!(!batch.fits(&rows(TableId::SpanCore, day(17), 145)));
    // An aggregating block holds its marker row too.
    assert!(batch.fits(&rows(TableId::TraceStats, day(17), 143)));
    assert!(!batch.fits(&rows(TableId::TraceStats, day(17), 144)));
    // Other pairs have their own builders.
    assert!(batch.fits(&rows(TableId::SpanCore, day(18), 262_144)));
    assert!(batch.fits(&rows(TableId::SpanPayload, day(17), 262_144)));
    let mut mixed = rows(TableId::SpanCore, day(18), 10);
    mixed.extend(rows(TableId::SpanCore, day(17), 145));
    assert!(!batch.fits(&mixed));
}

/// A `trace_heads` row with the given sort key and origin; the other cells are defaults.
fn head(project: u8, start: i64, lsn: u64, ordinal: u32) -> Row {
    let def = table(TableId::TraceHeads);
    let cells = def
        .columns
        .iter()
        .map(|c| match c.name {
            "project_id" => Value::Uuid([project; 16]),
            "start_time" => Value::I64(start),
            _ => Value::default_of(&c.ty),
        })
        .collect();
    Row {
        cells,
        lsn: Lsn::new(lsn),
        ordinal,
    }
}

#[test]
fn rows_sort_by_the_order_by_key_then_lsn_then_ordinal() {
    let def = table(TableId::TraceHeads);
    let mut rows = vec![
        head(2, 5, 1, 0),
        head(1, 9, 3, 1),
        head(1, 9, 3, 0),
        head(1, 9, 2, 7),
        head(1, 4, 9, 9),
        head(2, 5, 1, 0),
    ];
    sort_and_combine(def, &mut rows).unwrap();
    let order: Vec<(u8, i64, u64, u32)> = rows
        .iter()
        .map(|r| {
            let Value::Uuid(project) = r.cells[0] else {
                panic!("project_id")
            };
            let Value::I64(start) = r.cells[1] else {
                panic!("start_time")
            };
            (project[0], start, r.lsn.get(), r.ordinal)
        })
        .collect();
    assert_eq!(
        order,
        [
            (1, 4, 9, 9),
            (1, 9, 2, 7),
            (1, 9, 3, 0),
            (1, 9, 3, 1),
            (2, 5, 1, 0),
            (2, 5, 1, 0),
        ]
    );
    // A plain table keeps rows with equal keys apart.
    assert_eq!(rows.len(), 6);
}

#[test]
fn the_seal_limits_stay_inside_the_contract() {
    let mut limits = limits();
    for _ in 0..10 {
        limits.widen();
        assert!(limits.linger() <= Duration::from_secs(2));
        assert!(limits.rows() <= 262_144);
    }
}
