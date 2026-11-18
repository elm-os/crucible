//! Marker rows and the generic builders over every `TableDef` (contract sections 5.5 and 6.7).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{BASE, column_index, decode, decoded_rows};
use crucible_chfmt::Compression;
use crucible_records::{CivilDate, Lsn, PartitionId, TableId, Timestamp, WalEpoch};
use crucible_schema::{Granularity, PartitionExpr, TABLES, TableKind, Ty, route, table};
use crucible_shipper::value::{Row, Value, encode_block, marker_row, sort_and_combine};

#[test]
fn every_aggregating_table_gets_its_marker_row() {
    let epoch = WalEpoch::from_bytes([0xAB; 16]);
    let marker_at = Timestamp::from_micros(BASE + 999_999);
    let mut aggregating = 0;
    for def in TABLES.iter().filter(|d| d.kind == TableKind::Aggregating) {
        aggregating += 1;
        let (partition, start) = match def.partition {
            PartitionExpr::None => (PartitionId::UNPARTITIONED, None),
            PartitionExpr::Month(_) => (
                PartitionId::month(2026, 11).unwrap(),
                CivilDate::new(2026, 11, 1),
            ),
            _ => (
                PartitionId::date(CivilDate::new(2026, 11, 17).unwrap()),
                CivilDate::new(2026, 11, 17),
            ),
        };
        let marker = marker_row(def, partition, epoch, Lsn::new(4242), marker_at);
        for (column, value) in def.columns.iter().zip(&marker.cells) {
            let context = format!("{}.{}", def.name(), column.name);
            let expected = match column.name {
                // The nil project: no scoped read can see the row.
                "project_id" => Value::Uuid([0; 16]),
                "marker_epoch" => Value::Uuid([0xAB; 16]),
                "marker_batch_seq" => Value::U64(4242),
                "marker_at" => Value::U32((BASE / 1_000_000) as u32),
                // The first instant of the partition, so the marker lands in the block's partition.
                name if Some(name) == def.partition.column() => {
                    let start = start.unwrap();
                    let (cell, micros) = match column.ty.scalar() {
                        Ty::Date => {
                            let days = start.day_number();
                            (Value::U16(days as u16), days * 86_400_000_000)
                        }
                        Ty::DateTime => {
                            let seconds = start.start().micros() / 1_000_000;
                            (Value::U32(seconds as u32), seconds * 1_000_000)
                        }
                        _ => (Value::I64(start.start().micros()), start.start().micros()),
                    };
                    let routed = route(def.id, Granularity::Day, Timestamp::from_micros(micros));
                    assert_eq!(routed.unwrap(), partition, "{context}");
                    cell
                }
                // Every other key and measure column holds its type default.
                _ => Value::default_of(&column.ty),
            };
            assert_eq!(*value, expected, "{context}");
        }
        // The marker alone is a valid block of its table.
        let block = encode_block(def, std::slice::from_ref(&marker), Compression::Lz4).unwrap();
        assert_eq!(decoded_rows(&decode(def, &block)), [marker.cells.clone()]);
    }
    assert_eq!(aggregating, 7);
}

#[test]
fn marker_enums_without_a_zero_value_take_their_first_variant() {
    let def = table(TableId::McpToolRollup);
    let marker = marker_row(
        def,
        PartitionId::month(2026, 11).unwrap(),
        WalEpoch::from_bytes([1; 16]),
        Lsn::new(1),
        Timestamp::EPOCH,
    );
    let cell = |name: &str| &marker.cells[column_index(def, name)];
    // `capture_source` starts at sdk (1) and `era` at legacy (1); neither has a 0 value (contract 6.7).
    assert_eq!(*cell("capture_source"), Value::I8(1));
    assert_eq!(*cell("era"), Value::I8(1));
    // Enums with a 0 value keep it.
    assert_eq!(*cell("side"), Value::I8(0));
    assert_eq!(*cell("error_class"), Value::I8(0));
}

#[test]
fn a_weekly_marker_holds_the_monday_of_its_partition() {
    let def = table(TableId::TraceStats);
    // 2026-11-16 is a Monday.
    let monday = CivilDate::new(2026, 11, 16).unwrap();
    let marker = marker_row(
        def,
        PartitionId::date(monday),
        WalEpoch::from_bytes([1; 16]),
        Lsn::new(1),
        Timestamp::EPOCH,
    );
    let day = marker.cells[column_index(def, "row_day")].clone();
    assert_eq!(day, Value::U16(monday.day_number() as u16));
    let routed = route(def.id, Granularity::Week, monday.start()).unwrap();
    assert_eq!(routed, PartitionId::date(monday));
}

/// A marker never merges with a data row or with the marker of another batch: the marker columns end the
/// ORDER BY key.
#[test]
fn markers_stay_apart_from_data_and_from_each_other() {
    let def = table(TableId::TraceStats);
    let partition = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
    let epoch = WalEpoch::from_bytes([1; 16]);
    // A data row with the marker's own key columns: the nil project never occurs in data, so this is the
    // closest a row can come to a marker.
    let mut twin = marker_row(def, partition, epoch, Lsn::new(7), Timestamp::EPOCH);
    twin.cells[column_index(def, "marker_epoch")] = Value::Uuid([0; 16]);
    twin.cells[column_index(def, "marker_batch_seq")] = Value::U64(0);
    let mut rows = vec![
        marker_row(def, partition, epoch, Lsn::new(7), Timestamp::EPOCH),
        twin,
        marker_row(def, partition, epoch, Lsn::new(3), Timestamp::EPOCH),
    ];
    sort_and_combine(def, &mut rows).unwrap();
    let seq = column_index(def, "marker_batch_seq");
    let seqs: Vec<&Value> = rows.iter().map(|r| &r.cells[seq]).collect();
    assert_eq!(seqs, [&Value::U64(0), &Value::U64(3), &Value::U64(7)]);
}

/// The builders are generic over `TableDef`: a row of type defaults encodes for every table of the schema.
#[test]
fn a_row_of_defaults_encodes_for_every_table() {
    assert_eq!(TABLES.len(), TableId::ALL.len());
    for def in TABLES.iter() {
        let row = Row {
            cells: def
                .columns
                .iter()
                .map(|c| Value::default_of(&c.ty))
                .collect(),
            lsn: Lsn::new(1),
            ordinal: 0,
        };
        let block = encode_block(def, std::slice::from_ref(&row), Compression::Lz4)
            .unwrap_or_else(|e| panic!("{}: {e}", def.name()));
        assert_eq!(
            decoded_rows(&decode(def, &block)),
            [row.cells],
            "{}",
            def.name()
        );
    }
}

/// Histogram cells merge by `sumMap`: counts add per bucket.
#[test]
fn histograms_combine_by_bucket() {
    let def = table(TableId::SpanMetrics);
    let hist = |buckets: &[(u16, u64)]| {
        Value::Tuple(Box::new((
            Value::Array(buckets.iter().map(|b| Value::U16(b.0)).collect()),
            Value::Array(buckets.iter().map(|b| Value::U64(b.1)).collect()),
        )))
    };
    let row = |buckets: &[(u16, u64)]| {
        let mut cells: Vec<Value> = def
            .columns
            .iter()
            .map(|c| Value::default_of(&c.ty))
            .collect();
        cells[column_index(def, "project_id")] = Value::Uuid([1; 16]);
        cells[column_index(def, "count")] = Value::U64(1);
        cells[column_index(def, "dur_hist")] = hist(buckets);
        Row {
            cells,
            lsn: Lsn::new(1),
            ordinal: 0,
        }
    };
    let mut rows = vec![row(&[(3, 1), (40, 2)]), row(&[(3, 5), (7, 1)]), row(&[])];
    sort_and_combine(def, &mut rows).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cells[column_index(def, "count")], Value::U64(3));
    assert_eq!(
        rows[0].cells[column_index(def, "dur_hist")],
        hist(&[(3, 6), (7, 1), (40, 2)])
    );
    let block = encode_block(def, &rows, Compression::Lz4).unwrap();
    assert_eq!(decoded_rows(&decode(def, &block)), [rows[0].cells.clone()]);
}

#[test]
fn a_cell_of_the_wrong_type_is_refused() {
    let def = table(TableId::TraceHeads);
    let mut cells: Vec<Value> = def
        .columns
        .iter()
        .map(|c| Value::default_of(&c.ty))
        .collect();
    cells[column_index(def, "start_time")] = Value::U64(1);
    let row = Row {
        cells,
        lsn: Lsn::new(1),
        ordinal: 0,
    };
    assert!(encode_block(def, &[row], Compression::Lz4).is_err());
}
