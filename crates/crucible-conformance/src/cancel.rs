//! C13: cancellation of reads (contract 8.5). A read is a POSTed SELECT run by a `readonly` identity with
//! `cancel_http_readonly_queries_on_client_close = 1`.
//!
//! Four paths are observed in `system.processes`, each on a statement that would otherwise run for a minute:
//! closing the connection alone (whether the server cancels is pinned, not required), `TelemetryStore::kill`,
//! dropping a stream that kills on drop, and the Rust-side deadline. The last three must end the statement.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use crucible_env::{QueryError, TelemetryStore};
use crucible_records::Uuid;
use crucible_schema::ServiceIdentity;
use crucible_store_clickhouse::RawRead;

use crate::{Ctx, expect, with_settings};

/// How long a statement may take to leave `system.processes` after it was cancelled.
const GONE_WITHIN: Duration = Duration::from_secs(20);

fn slow(ctx: &Ctx, deadline: Duration, kill_on_drop: bool) -> Result<RawRead> {
    let statement = with_settings(
        "SELECT sleepEachRow(1) FROM numbers(60)",
        &[
            ("max_block_size", "1"),
            ("max_threads", "1"),
            ("max_execution_time", "120"),
            ("cancel_http_readonly_queries_on_client_close", "1"),
        ],
    );
    ctx.store
        .raw_read(ServiceIdentity::ReadUi, &statement, deadline, kill_on_drop)
        .map_err(|error| anyhow!("{error}"))
}

async fn running(ctx: &Ctx, query_id: Uuid) -> Result<bool> {
    Ok(ctx
        .admin_count(format!(
            "SELECT count() FROM system.processes WHERE query_id = '{query_id}'"
        ))
        .await?
        > 0)
}

/// Starts the statement and waits until the server lists it.
async fn started(ctx: &Ctx, reading: &mut RawRead) -> Result<()> {
    let first = ctx
        .env
        .timeout(Duration::from_secs(3), reading.next())
        .await;
    if let Ok(Some(Err(error))) = first {
        bail!("the slow statement failed: {error}");
    }
    expect(running(ctx, reading.query_id()).await?, || {
        "the slow statement is not in system.processes".to_owned()
    })
}

/// Whether the statement leaves `system.processes` within [`GONE_WITHIN`], and after how long.
async fn gone(ctx: &Ctx, query_id: Uuid) -> Result<Option<Duration>> {
    let start = ctx.env.clock.now();
    loop {
        let waited = ctx.env.clock.now().saturating_duration_since(start);
        if !running(ctx, query_id).await? {
            return Ok(Some(waited));
        }
        if waited > GONE_WITHIN {
            return Ok(None);
        }
        ctx.env.clock.sleep(Duration::from_millis(250)).await;
    }
}

pub(crate) async fn c13(ctx: &Ctx) -> Result<String> {
    let long = Duration::from_secs(120);

    // 1. Close the connection and do nothing else.
    let mut reading = slow(ctx, long, false)?;
    started(ctx, &mut reading).await?;
    let query_id = reading.query_id();
    drop(reading);
    let on_close = gone(ctx, query_id).await?;
    if on_close.is_none() {
        ctx.store.kill(query_id).await?;
    }

    // 2. `kill` by query id, with the connection still open.
    let mut reading = slow(ctx, long, false)?;
    started(ctx, &mut reading).await?;
    let query_id = reading.query_id();
    ctx.store.kill(query_id).await?;
    let killed = gone(ctx, query_id).await?;
    expect(killed.is_some(), || {
        "KILL QUERY did not end the statement".to_owned()
    })?;
    // Killing a statement that has ended succeeds.
    ctx.store.kill(query_id).await?;

    // 3. Drop a stream that kills on drop: the path of a dropped `RowStream`.
    let mut reading = slow(ctx, long, true)?;
    started(ctx, &mut reading).await?;
    let query_id = reading.query_id();
    drop(reading);
    let dropped = gone(ctx, query_id).await?;
    expect(dropped.is_some(), || {
        "dropping the stream did not end the statement".to_owned()
    })?;

    // 4. The Rust-side deadline ends the stream with a timeout and the statement on the server.
    let mut reading = slow(ctx, Duration::from_secs(3), true)?;
    let query_id = reading.query_id();
    let outcome = loop {
        match reading.next().await {
            Some(Ok(_)) => {}
            other => break other,
        }
    };
    expect(matches!(outcome, Some(Err(QueryError::Timeout))), || {
        format!("the deadline ended the stream with {outcome:?}")
    })?;
    let timed_out = gone(ctx, query_id).await?;
    expect(timed_out.is_some(), || {
        "the deadline did not end the statement".to_owned()
    })?;

    let on_close = match on_close {
        Some(after) => format!(
            "closing the connection of a POSTed SELECT cancels it (gone after {} ms)",
            after.as_millis()
        ),
        None => "closing the connection of a POSTed SELECT does not cancel it".to_owned(),
    };
    Ok(format!(
        "{on_close}; KILL QUERY, a dropped stream and the Rust deadline each end the statement"
    ))
}
