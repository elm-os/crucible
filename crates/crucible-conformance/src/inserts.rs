//! The insert-protocol rows: C1 to C4, the single-block row and the two-partition row (contract section 5).

use anyhow::{Context as _, Result, bail};
use crucible_chfmt::MAX_BLOCK_ROWS;
use crucible_env::{InsertError, InsertSettings, TelemetryStore};
use crucible_plan::Value;
use crucible_plan::test_support::batch_probe;
use crucible_records::{Lsn, PartitionId, ProjectId, TableId, Timestamp, Uuid, WalEpoch};
use crucible_schema::{Granularity, TABLES, route};

use crate::blocks::Rows;
use crate::{Ctx, T0, expect};

const DAY_MICROS: i64 = 86_400_000_000;

pub(crate) fn uuid(value: Uuid) -> Value {
    Value::Uuid(value)
}

/// An 8-byte id that differs per row and per `salt`.
pub(crate) fn id8(salt: u32, row: usize) -> Value {
    let low = u32::try_from(row).unwrap_or(u32::MAX);
    Value::Bytes([salt.to_be_bytes(), low.to_be_bytes()].concat())
}

/// `rows` rows of a plain table of spans at `time`, with the ingestion columns of batch `seq`.
pub(crate) fn plain_rows(
    table: TableId,
    project: ProjectId,
    epoch: WalEpoch,
    seq: u64,
    time: Timestamp,
    rows: usize,
    salt: u32,
) -> Rows {
    Rows::new(table, rows)
        .all("project_id", uuid(project.uuid()))
        .all("start_time", Value::Time(time))
        .all("start_minute", Value::Time(time.floor_minute()))
        .each("span_id", |row| id8(salt, row))
        .all("wal_epoch", uuid(epoch.uuid()))
        .all("batch_seq", Value::UInt(seq))
        .each("lsn", |row| Value::UInt(seq + row as u64))
        .all("received_at", Value::Time(time))
}

/// `rows` rows of a fact table at `time`.
fn fact_rows(
    table: TableId,
    project: ProjectId,
    epoch: WalEpoch,
    seq: u64,
    time: Timestamp,
    rows: usize,
) -> Rows {
    Rows::new(table, rows)
        .all("project_id", uuid(project.uuid()))
        .all("target_time", Value::Time(time))
        .all("target_day", Value::Date(time.date()))
        .each("span_id", |row| id8(u32::try_from(seq).unwrap_or(0), row))
        .all("wal_epoch", uuid(epoch.uuid()))
        .all("batch_seq", Value::UInt(seq))
}

/// A block for the `span_metrics` rollup: `rows` data rows with distinct keys, then the block's marker row.
pub(crate) fn rollup_rows(
    project: ProjectId,
    epoch: WalEpoch,
    seq: u64,
    time: Timestamp,
    rows: usize,
) -> Rows {
    let marker = rows;
    let data = move |row: usize| row != marker;
    Rows::new(TableId::SpanMetrics, rows + 1)
        .each("project_id", |row| {
            uuid(if data(row) { project.uuid() } else { Uuid::NIL })
        })
        .all("minute", Value::Time(time.floor_minute()))
        .each("release", |row| {
            Value::String(if data(row) {
                format!("r{row}")
            } else {
                String::new()
            })
        })
        .each("marker_epoch", |row| {
            uuid(if data(row) { Uuid::NIL } else { epoch.uuid() })
        })
        .each("marker_batch_seq", |row| {
            Value::UInt(if data(row) { 0 } else { seq })
        })
        .all("marker_at", Value::Time(time.floor_minute()))
        .each("count", |row| Value::UInt(u64::from(data(row))))
}

async fn rows_of(ctx: &Ctx, table: TableId, project: ProjectId) -> Result<u64> {
    ctx.admin_count(format!(
        "SELECT count() FROM crucible.{} WHERE project_id = '{project}'",
        table.name()
    ))
    .await
}

async fn probe(
    ctx: &Ctx,
    table: TableId,
    time: Timestamp,
    epoch: WalEpoch,
    seq: u64,
) -> Result<bool> {
    let partition = route(table, Granularity::Day, time)?;
    Ok(ctx
        .store
        .probe(batch_probe(table, partition, epoch, Lsn::new(seq))?)
        .await?)
}

/// C1: one token retried within the window yields one copy, on the plain and the aggregating engine.
pub(crate) async fn c1(ctx: &Ctx) -> Result<String> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let blocks = [
        (
            TableId::SpanCore,
            plain_rows(TableId::SpanCore, project, epoch, 1, T0, 100, 1),
            100,
        ),
        (
            TableId::SpanMetrics,
            rollup_rows(project, epoch, 1, T0, 50),
            50,
        ),
    ];
    for (table, rows, expected) in blocks {
        ctx.stop_merges(table.name()).await?;
        expect(!probe(ctx, table, T0, epoch, 1).await?, || {
            format!(
                "{}: the probe reads present before the insert",
                table.name()
            )
        })?;
        let token = ctx.token(epoch, 1, table, T0)?;
        for attempt in 1..=3 {
            ctx.insert(&rows, token)
                .await
                .with_context(|| format!("{} attempt {attempt}", table.name()))?;
        }
        let landed = rows_of(ctx, table, project).await?;
        expect(landed == expected, || {
            format!(
                "{}: three attempts left {landed} rows, not {expected}",
                table.name()
            )
        })?;
        expect(probe(ctx, table, T0, epoch, 1).await?, || {
            format!("{}: the probe reads absent after the insert", table.name())
        })?;
        expect(!probe(ctx, table, T0, epoch, 2).await?, || {
            format!(
                "{}: the probe reads present for another batch",
                table.name()
            )
        })?;
    }
    Ok(format!(
        "{:?}: 3 sends of one token left one copy in span_core (plain) and span_metrics (aggregating); both \
         probe forms read it",
        ctx.flavour
    ))
}

/// C2: a plain table deduplicates only with `non_replicated_deduplication_window` set, and the DDL sets it.
pub(crate) async fn c2(ctx: &Ctx) -> Result<String> {
    let with_setting = ctx
        .admin_count(
            "SELECT count() FROM system.tables WHERE database = 'crucible' AND create_table_query LIKE \
             '%non_replicated_deduplication_window = 20000%' AND name NOT LIKE 'conformance%'",
        )
        .await?;
    expect(with_setting == TABLES.len() as u64, || {
        format!("{with_setting} of {} tables set the window", TABLES.len())
    })?;

    let columns = "(id UUID, wal_epoch UUID, batch_seq UInt64, created_at DateTime64(6, 'UTC'))";
    let block = Rows::new(TableId::DedupSelftest, 3)
        .each("id", |row| uuid(Uuid::from_bytes([row as u8 + 1; 16])))
        .encode()?;
    let mut counts = Vec::new();
    for (name, settings) in [
        ("conformance_nowindow", ""),
        (
            "conformance_window",
            " SETTINGS non_replicated_deduplication_window = 20000",
        ),
    ] {
        ctx.admin(format!("DROP TABLE IF EXISTS crucible.{name} SYNC"))
            .await?;
        ctx.admin(format!(
            "CREATE TABLE crucible.{name} {columns} ENGINE = MergeTree ORDER BY id{settings}"
        ))
        .await?;
        ctx.admin(format!(
            "GRANT INSERT ON crucible.{name} TO crucible_ingest"
        ))
        .await?;
        let token = ctx.token_in(
            ctx.epoch(),
            1,
            TableId::DedupSelftest,
            PartitionId::UNPARTITIONED,
        )?;
        let settings = InsertSettings::new(token, ctx.flavour).pairs();
        let sql = format!(
            "INSERT INTO crucible.{name} (id, wal_epoch, batch_seq, created_at) FORMAT Native"
        );
        for _ in 0..2 {
            ctx.store
                .raw_insert(&sql, block.compressed().clone(), &settings)
                .await?;
        }
        counts.push(
            ctx.admin_count(format!("SELECT count() FROM crucible.{name}"))
                .await?,
        );
        ctx.admin(format!("DROP TABLE crucible.{name} SYNC"))
            .await?;
    }
    expect(counts == [6, 3], || {
        format!("two sends of 3 rows left {counts:?} rows without and with the window, not [6, 3]")
    })?;
    Ok(format!(
        "all {} tables set the window in their DDL; 2 sends of one token left 2 copies without the window and \
         1 with it",
        TABLES.len()
    ))
}

/// C3: the deduplication log is bounded; beyond it only the probe prevents a duplicate.
pub(crate) async fn c3(ctx: &Ctx) -> Result<String> {
    let table = TableId::FactFlags;
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let batch = |seq: u64| fact_rows(table, project, epoch, seq, T0, 10);
    let token = |seq: u64| ctx.token(epoch, seq, table, T0);
    let window =
        "ALTER TABLE crucible.fact_flags MODIFY SETTING non_replicated_deduplication_window";
    ctx.stop_merges(table.name()).await?;
    ctx.admin(format!("{window} = 2")).await?;
    let observed = async {
        ctx.insert(&batch(1), token(1)?).await?;
        ctx.insert(&batch(1), token(1)?).await?;
        let inside = rows_of(ctx, table, project).await?;
        // Push the first token out of a log that keeps two entries.
        for seq in 2..=6 {
            ctx.insert(&batch(seq), token(seq)?).await?;
        }
        let present = probe(ctx, table, T0, epoch, 1).await?;
        let before = rows_of(ctx, table, project).await?;
        ctx.insert(&batch(1), token(1)?).await?;
        let after = rows_of(ctx, table, project).await?;
        anyhow::Ok((inside, present, before, after))
    }
    .await;
    ctx.admin(format!("{window} = 20000")).await?;
    let (inside, present, before, after) = observed?;
    expect(inside == 10, || {
        format!("a retry inside the window left {inside} rows, not 10")
    })?;
    expect(present, || {
        "the probe reads absent for a block that landed".to_owned()
    })?;
    expect((before, after) == (60, 70), || {
        format!(
            "a resend beyond the window took the table from {before} to {after} rows, not 60 to 70"
        )
    })?;
    Ok(
        "with a log of 2 entries a retry inside it is dropped; after 5 more inserts the same token lands a \
         second copy, while the probe still reads the first as present"
            .to_owned(),
    )
}

/// C4: the token, not the content, decides deduplication.
pub(crate) async fn c4(ctx: &Ctx) -> Result<String> {
    let table = TableId::SpanCore;
    ctx.stop_merges(table.name()).await?;

    // One token, different bytes: the second block is dropped.
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let token = ctx.token(epoch, 1, table, T0)?;
    ctx.insert(&plain_rows(table, project, epoch, 1, T0, 5, 1), token)
        .await?;
    ctx.insert(&plain_rows(table, project, epoch, 1, T0, 7, 2), token)
        .await?;
    let same_token = rows_of(ctx, table, project).await?;
    expect(same_token == 5, || {
        format!("one token with two contents left {same_token} rows, not 5")
    })?;

    // Two tokens, identical bytes: both land.
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let block = plain_rows(table, project, epoch, 1, T0, 5, 1).encode()?;
    for seq in [1, 2] {
        let settings = InsertSettings::new(ctx.token(epoch, seq, table, T0)?, ctx.flavour);
        ctx.store.insert_block(block.clone(), settings).await?;
    }
    let same_bytes = rows_of(ctx, table, project).await?;
    expect(same_bytes == 10, || {
        format!("two tokens with one content left {same_bytes} rows, not 10")
    })?;
    Ok(
        "one token with different bytes lands once; identical bytes under two tokens land twice"
            .to_owned(),
    )
}

/// A block of 262,144 rows yields one part and one deduplication entry.
pub(crate) async fn single_block(ctx: &Ctx) -> Result<String> {
    let table = TableId::DedupSelftest;
    ctx.stop_merges(table.name()).await?;
    let parts = "SELECT count(), sum(rows) FROM system.parts WHERE database = 'crucible' AND table = \
                 'dedup_selftest' AND active AND rows = 262144";
    let before = ctx.admin_text(parts).await?;
    let epoch = ctx.epoch();
    let rows = Rows::new(table, MAX_BLOCK_ROWS)
        .each("id", |row| {
            let mut bytes = epoch.to_bytes();
            bytes[12..].copy_from_slice(&u32::try_from(row).unwrap_or(0).to_be_bytes());
            uuid(Uuid::from_bytes(bytes))
        })
        .all("wal_epoch", uuid(epoch.uuid()));
    let token = ctx.token_in(epoch, 1, table, PartitionId::UNPARTITIONED)?;
    let landed = "SELECT count() FROM crucible.dedup_selftest WHERE wal_epoch = ";
    ctx.insert(&rows, token).await?;
    let first = ctx.admin_text(parts).await?;
    ctx.insert(&rows, token).await?;
    let second = ctx.admin_text(parts).await?;
    let count = ctx.admin_count(format!("{landed}'{epoch}'")).await?;
    let parts_of = |answer: &str| -> Result<u64> {
        let field = answer.split('\t').next().unwrap_or_default();
        Ok(field.trim().parse()?)
    };
    expect(parts_of(&first)? == parts_of(&before)? + 1, || {
        format!(
            "the block made parts go from {before:?} to {first:?}, not one more part of 262144 rows"
        )
    })?;
    expect(second == first && count == MAX_BLOCK_ROWS as u64, || {
        format!("the resend changed the parts from {first:?} to {second:?}, rows {count}")
    })?;
    Ok("262,144 rows landed as one part of 262,144 rows; the resend with the same token added nothing".to_owned())
}

/// A block that spans two partitions is rejected, as poison rather than as a retry.
pub(crate) async fn two_partitions(ctx: &Ctx) -> Result<String> {
    let table = TableId::SpanCore;
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let next_day = T0.saturating_add_micros(DAY_MICROS);
    let rows = plain_rows(table, project, epoch, 1, T0, 2, 1)
        .with("start_time", vec![Value::Time(T0), Value::Time(next_day)]);
    let outcome = ctx.insert(&rows, ctx.token(epoch, 1, table, T0)?).await;
    let landed = rows_of(ctx, table, project).await?;
    expect(landed == 0, || format!("{landed} rows of the block landed"))?;
    match outcome {
        Err(InsertError::Poison { code, .. }) => Ok(format!(
            "rejected with code {code} and classified poison; nothing landed"
        )),
        other => bail!("expected a poison rejection, got {other:?}"),
    }
}
