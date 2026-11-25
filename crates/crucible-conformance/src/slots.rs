//! Reads of `crucible_read_agent` wait for one of the identity's statement slots. Its settings profile caps it at
//! `AGENT_MAX_CONCURRENT_QUERIES` statements at a time (contract 7), and a request sends several reads together.
//! With every slot held by a running statement, a typed agent read waits until one of them ends and then
//! succeeds, where the server would have refused it: on the node whose statements hold the slots, which waits for
//! one of its own, and on another node, whose own slots are free, so that the server refuses its read and the read
//! is sent again within its deadline.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_env::{QueryError, TelemetryStore};
use crucible_plan::{Caller, RetentionClamp, trace_time_bounds};
use crucible_records::{TimeWindow, TraceId};
use crucible_schema::{AGENT_MAX_CONCURRENT_QUERIES, RetentionSettings, ServiceIdentity};
use crucible_store_clickhouse::{ClickHouseStore, RawRead};
use futures::future::join_all;

use crate::{Ctx, T0, expect, with_settings};

pub(crate) async fn agent_slots(ctx: &Ctx) -> Result<String> {
    let waited = read_while_every_slot_is_held(ctx, &ctx.store).await?;
    Ok(format!(
        "with all {AGENT_MAX_CONCURRENT_QUERIES} statement slots of crucible_read_agent held by running statements, a \
         typed agent read waited {} ms for one to end and then succeeded",
        waited.as_millis()
    ))
}

pub(crate) async fn agent_slots_across_nodes(ctx: &Ctx) -> Result<String> {
    // Another node over the same server, whose own slots are all free.
    let other = ClickHouseStore::new(ctx.env.clone(), ctx.config.clone());
    let waited = read_while_every_slot_is_held(ctx, &other).await?;
    Ok(format!(
        "with all {AGENT_MAX_CONCURRENT_QUERIES} statement slots of crucible_read_agent held by another node's \
         running statements, the server refused a typed agent read, which was sent again and succeeded after {} ms",
        waited.as_millis()
    ))
}

/// Holds every statement slot of the agent identity with statements of the suite's store, makes a typed agent read
/// through `reader` meanwhile, and returns how long the read took.
async fn read_while_every_slot_is_held(ctx: &Ctx, reader: &ClickHouseStore) -> Result<Duration> {
    // Statements that hold the slots for about two seconds, each sending its first row after half a second.
    let sleeper = with_settings(
        "SELECT sleepEachRow(0.5) FROM numbers(4)",
        &[("max_block_size", "1")],
    );
    let mut held = Vec::new();
    for _ in 0..AGENT_MAX_CONCURRENT_QUERIES {
        let read = ctx
            .store
            .raw_read(
                ServiceIdentity::ReadAgent,
                &sleeper,
                Duration::from_secs(30),
                true,
            )
            .map_err(|e| anyhow!("{e}"))?;
        held.push(read);
    }
    // A statement is sent on its first poll; once every first row is in, all of them run on the server.
    for first in join_all(held.iter_mut().map(RawRead::next)).await {
        first
            .context("a sleeper ended before its first row")?
            .map_err(|e| anyhow!("a sleeper failed: {e}"))?;
    }
    let scope = scope(ctx.project()).map_err(|e| anyhow!("{e}"))?;
    let window = TimeWindow::new(T0, T0.saturating_add_micros(1_000_000))?;
    let clamp = RetentionClamp::new(RetentionSettings::default(), T0);
    let unknown = TraceId::from_bytes([0x5b; 16]);
    let plan = trace_time_bounds(scope, window, &clamp, Caller::Agent, unknown)?
        .context("the builder returned no plan")?;
    let clock = &ctx.env.clock;
    let started = clock.now();
    let typed = async {
        let rows = reader.execute(plan).await?.rows().await?;
        Ok::<_, QueryError>((rows.len(), clock.now()))
    };
    let (sleepers, typed) = futures::join!(join_all(held.into_iter().map(RawRead::drain)), typed);
    for sleeper in sleepers {
        sleeper.map_err(|e| anyhow!("a sleeper failed: {e}"))?;
    }
    let (rows, answered) =
        typed.map_err(|e| anyhow!("the agent read failed while every slot was held: {e}"))?;
    expect(rows == 0, || {
        format!("the read of an unknown trace returned {rows} rows")
    })?;
    // The sleepers had about 1.5 s left when the read started, so a read sent at once would have been refused.
    let waited = answered.saturating_duration_since(started);
    expect(waited >= Duration::from_millis(500), || {
        format!(
            "the agent read answered after {} ms, before any slot came free",
            waited.as_millis()
        )
    })?;
    Ok(waited)
}
