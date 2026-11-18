//! C12: the server codes that classify failures are stable across the version matrix (contract 5.6). Each
//! failure is provoked on the real server and must come back through the adapter in its class.
//!
//! Codes that need a fault this suite cannot stage on one healthy server are pinned by the table only: 242
//! (read-only table), 243 (no space), 319 and 999 (quorum and Keeper failures), 159 on an insert, and 53, 27 and
//! 117, which a Native block does not provoke: its data errors are 33, 271, 432, 376 and 43.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;
use crucible_chfmt::{ArrayColumn, Block, Compression, MAX_BLOCK_ROWS, StringColumn};
use crucible_env::{InsertError, InsertSettings, QueryError, RetryReason, TelemetryStore};
use crucible_records::{PartitionId, TableId};
use crucible_schema::ServiceIdentity;
use crucible_store_clickhouse::{ClickHouseStore, StoreConfig};

use crate::blocks::Rows;
use crate::inserts::plain_rows;
use crate::{Ctx, T0, with_settings};

const SELFTEST_COLUMNS: &str = "(id, wal_epoch, batch_seq, created_at)";

fn settings(ctx: &Ctx) -> Result<Vec<(&'static str, String)>> {
    let token = ctx.token_in(
        ctx.epoch(),
        1,
        TableId::DedupSelftest,
        PartitionId::UNPARTITIONED,
    )?;
    Ok(InsertSettings::new(token, ctx.flavour).pairs())
}

async fn send(
    ctx: &Ctx,
    target: &str,
    body: Bytes,
    extra: &[(&'static str, &str)],
) -> Result<Result<(), InsertError>> {
    let mut settings = settings(ctx)?;
    settings.extend(extra.iter().map(|(name, value)| (*name, (*value).to_owned())));
    let sql = format!("INSERT INTO crucible.{target} FORMAT Native");
    Ok(ctx.store.raw_insert(&sql, body, &settings).await)
}

fn fatal(outcome: Result<(), InsertError>, expected: u32) -> Result<String> {
    match outcome {
        Err(InsertError::FatalConfig { code, .. }) if code == expected => Ok(code.to_string()),
        other => bail!("expected a fatal configuration error with code {expected}, got {other:?}"),
    }
}

fn retry(outcome: Result<(), InsertError>, expected: RetryReason) -> Result<String> {
    match outcome {
        Err(InsertError::Retry { reason, .. }) if reason == expected => Ok(format!("{reason:?}")),
        other => bail!("expected a retry for {expected:?}, got {other:?}"),
    }
}

fn poison(outcome: Result<(), InsertError>, expected: u32) -> Result<String> {
    match outcome {
        Err(InsertError::Poison { code, .. }) if code == expected => Ok(code.to_string()),
        other => bail!("expected poison with code {expected}, got {other:?}"),
    }
}

async fn insert_codes(ctx: &Ctx) -> Result<Vec<String>> {
    let mut seen = Vec::new();
    let block = Rows::new(TableId::DedupSelftest, 3).encode()?;
    let body = block.compressed().clone();
    let selftest = format!("dedup_selftest {SELFTEST_COLUMNS}");

    let outcome = send(ctx, &format!("conformance_missing {SELFTEST_COLUMNS}"), body.clone(), &[]).await?;
    seen.push(format!("unknown table {}", fatal(outcome, 60)?));

    let outcome = send(ctx, "dedup_selftest (id, conformance_missing)", body.clone(), &[]).await?;
    seen.push(format!("unknown column {}", fatal(outcome, 16)?));

    let outcome = send(ctx, "sql_probe (project_id)", body.clone(), &[]).await?;
    seen.push(format!("no grant {}", fatal(outcome, 497)?));

    let mut wrong = StoreConfig::clone(&ctx.config);
    wrong.ingest.password.push_str("-wrong");
    let stranger = ClickHouseStore::new(ctx.env.clone(), wrong);
    let token = ctx.token_in(ctx.epoch(), 1, TableId::DedupSelftest, PartitionId::UNPARTITIONED)?;
    let outcome = stranger
        .insert_block(block.clone(), InsertSettings::new(token, ctx.flavour))
        .await;
    seen.push(format!("wrong password {}", fatal(outcome, 516)?));

    // Too many parts: a table that refuses an insert once it holds one part.
    ctx.admin("DROP TABLE IF EXISTS crucible.conformance_parts SYNC").await?;
    ctx.admin(
        "CREATE TABLE crucible.conformance_parts (id UUID, wal_epoch UUID, batch_seq UInt64, created_at \
         DateTime64(6, 'UTC')) ENGINE = MergeTree ORDER BY id SETTINGS parts_to_throw_insert = 1, \
         parts_to_delay_insert = 1",
    )
    .await?;
    ctx.admin("SYSTEM STOP MERGES crucible.conformance_parts").await?;
    ctx.admin("GRANT INSERT ON crucible.conformance_parts TO crucible_ingest").await?;
    let target = format!("conformance_parts {SELFTEST_COLUMNS}");
    let mut outcome = Ok(());
    for _ in 0..3 {
        outcome = send(ctx, &target, body.clone(), &[]).await?;
        if outcome.is_err() {
            break;
        }
    }
    ctx.admin("DROP TABLE crucible.conformance_parts SYNC").await?;
    seen.push(format!("too many parts {}", retry(outcome, RetryReason::TooManyParts)?));

    // Memory: a full block under a limit far below its size.
    let large = Rows::new(TableId::DedupSelftest, MAX_BLOCK_ROWS).encode()?;
    let outcome = send(
        ctx,
        &selftest,
        large.compressed().clone(),
        &[("max_memory_usage", "1000000")],
    )
    .await?;
    seen.push(format!("memory {}", retry(outcome, RetryReason::Memory)?));

    // Too many queries: one statement of the user is running and the insert allows itself no neighbour.
    let mut running = ctx
        .store
        .raw_read(
            ServiceIdentity::Ingest,
            &with_settings(
                "SELECT sleepEachRow(1) FROM numbers(20)",
                &[("max_block_size", "1"), ("max_execution_time", "30")],
            ),
            Duration::from_secs(30),
            true,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let _first = ctx.env.timeout(Duration::from_secs(2), running.next()).await;
    let outcome = send(
        ctx,
        &selftest,
        body.clone(),
        &[("max_concurrent_queries_for_user", "1")],
    )
    .await?;
    drop(running);
    seen.push(format!("too many queries {}", retry(outcome, RetryReason::TooManyQueries)?));

    // Data the server cannot read or convert.
    let mut text = StringColumn::default();
    text.push("not a uuid");
    let mut unparseable = Block::new(1)?;
    unparseable.push("id", "String", &text)?;
    let unparseable = unparseable.encode(Compression::Lz4)?;
    let outcome = send(ctx, "dedup_selftest (id)", unparseable.compressed().clone(), &[]).await?;
    seen.push(format!("unparseable value {}", poison(outcome, 376)?));

    let mut numbers = ArrayColumn::new(Vec::<u8>::new());
    numbers.push([1u8, 2]);
    let mut mistyped = Block::new(1)?;
    mistyped.push("id", "Array(UInt8)", &numbers)?;
    let mistyped = mistyped.encode(Compression::Lz4)?;
    let outcome = send(ctx, "dedup_selftest (id)", mistyped.compressed().clone(), &[]).await?;
    seen.push(format!("unconvertible type {}", poison(outcome, 43)?));

    let cut = body.slice(..body.len() - 6);
    let outcome = send(ctx, &selftest, cut, &[]).await?;
    seen.push(format!("body cut short {}", poison(outcome, 33)?));

    let mut flipped = body.to_vec();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 0xff;
    let outcome = send(ctx, &selftest, Bytes::from(flipped), &[]).await?;
    seen.push(format!("corrupt frame {}", poison(outcome, 271)?));

    let not_a_frame = Bytes::from_static(b"this body is not a compressed frame at all....");
    let outcome = send(ctx, &selftest, not_a_frame, &[]).await?;
    seen.push(format!("no frame {}", poison(outcome, 432)?));
    Ok(seen)
}

async fn read(ctx: &Ctx, sql: &str, settings: &[(&'static str, &str)]) -> Result<QueryError> {
    let mut all = vec![
        ("max_execution_time", "10"),
        ("timeout_overflow_mode", "throw"),
        ("read_overflow_mode", "throw"),
    ];
    all.retain(|(name, _)| !settings.iter().any(|(set, _)| set == name));
    all.extend_from_slice(settings);
    let reading = ctx
        .store
        .raw_read(
            ServiceIdentity::ReadUi,
            &with_settings(sql, &all),
            Duration::from_secs(20),
            true,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    match reading.drain().await {
        Err(error) => Ok(error),
        Ok(bytes) => bail!("{sql} returned {bytes} bytes instead of failing"),
    }
}

/// A guardrail: its name, a statement, the settings that trip it and the error it must map to.
type Guardrail = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
    QueryError,
);

async fn read_codes(ctx: &Ctx) -> Result<Vec<String>> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let table = TableId::SpanCore;
    ctx.insert(
        &plain_rows(table, project, epoch, 1, T0, 1_000, 1),
        ctx.token(epoch, 1, table, T0)?,
    )
    .await
    .context("the rows the guardrails trip on")?;
    let spans = "SELECT span_id FROM crucible.span_core";
    let cases: [Guardrail; 6] = [
        ("rows to read", spans, &[("max_rows_to_read", "10")], QueryError::TooBroad),
        ("bytes to read", spans, &[("max_bytes_to_read", "10")], QueryError::TooBroad),
        (
            "result rows",
            spans,
            &[("max_result_rows", "1"), ("result_overflow_mode", "throw")],
            QueryError::TooBroad,
        ),
        (
            "set size",
            "SELECT count() FROM crucible.span_core WHERE span_id IN (SELECT span_id FROM crucible.span_core)",
            &[("max_rows_in_set", "1"), ("set_overflow_mode", "throw")],
            QueryError::TooBroad,
        ),
        (
            "time",
            "SELECT sleepEachRow(1) FROM numbers(10)",
            &[("max_execution_time", "1"), ("max_block_size", "1")],
            QueryError::Timeout,
        ),
        (
            "memory",
            "SELECT groupArray(number) FROM numbers(10000000)",
            &[("max_memory_usage", "1000000")],
            QueryError::MemoryExceeded,
        ),
    ];
    let mut seen = Vec::new();
    for (name, sql, settings, expected) in cases {
        let got = read(ctx, sql, settings).await?;
        if got != expected {
            bail!("{name}: expected {expected:?}, got {got:?}");
        }
        seen.push(format!("{name} {}", got.problem_code()));
    }
    Ok(seen)
}

pub(crate) async fn c12(ctx: &Ctx) -> Result<String> {
    let inserts = insert_codes(ctx).await.context("insert classes")?;
    let reads = read_codes(ctx).await.context("read guardrails")?;
    Ok(format!(
        "inserts: {}; reads: {}",
        inserts.join(", "),
        reads.join(", ")
    ))
}
