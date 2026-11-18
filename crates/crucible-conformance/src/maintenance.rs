//! The maintenance calls of contract 2.6 on the real server: lightweight deletes in a partition, mask
//! application, mutation status, partition drops, partition projects and the marker purge.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use crucible_auth::test_support::scope;
use crucible_env::{MutationId, MutationStatus, TelemetryStore};
use crucible_plan::test_support::batch_probe;
use crucible_plan::{CmpOp, DeletePlan, Pred};
use crucible_records::{Lsn, ProjectId, TableId, Timestamp};
use crucible_schema::{Granularity, McpOpsCol, route};

use crate::inserts::{plain_rows, rollup_rows};
use crate::{Ctx, T0, expect};

const DAY: i64 = 86_400_000_000;

async fn count(ctx: &Ctx, table: TableId, project: ProjectId) -> Result<u64> {
    ctx.admin_count(format!(
        "SELECT count() FROM crucible.{} WHERE project_id = '{project}'",
        table.name()
    ))
    .await
}

async fn done(ctx: &Ctx, id: &MutationId) -> Result<()> {
    for _ in 0..120 {
        match ctx.store.mutation_status(id).await? {
            MutationStatus::Done => return Ok(()),
            MutationStatus::Pending => ctx.env.clock.sleep(Duration::from_millis(250)).await,
            MutationStatus::Failed { code, reason } => bail!("mutation failed ({code}): {reason}"),
        }
    }
    bail!("mutation {} did not finish", id.name())
}

pub(crate) async fn maintenance(ctx: &Ctx) -> Result<String> {
    let table = TableId::McpOps;
    let (first, second, epoch) = (ctx.project(), ctx.project(), ctx.epoch());
    let partition = route(table, Granularity::Day, T0)?;
    for (seq, project) in [(1, first), (2, second)] {
        let rows = plain_rows(table, project, epoch, seq, T0, 10, 1)
            .each("duration_us", |row| crucible_plan::Value::UInt(row as u64));
        ctx.insert(&rows, ctx.token(epoch, seq, table, T0)?).await?;
    }

    let projects = ctx.store.partition_projects(table, partition).await?;
    expect(
        projects.is_sorted() && projects.contains(&first) && projects.contains(&second),
        || format!("partition projects {projects:?}"),
    )?;

    // A typed delete hides exactly the matching rows of one project, at once.
    let plan = DeletePlan::project(
        scope(first).map_err(|e| anyhow!("{e}"))?,
        table,
        partition,
        Pred::cmp(McpOpsCol::DurationUs, CmpOp::Ge, 4u64),
    )?;
    let delete = ctx.store.delete(plan).await.context("delete")?;
    let left = (
        count(ctx, table, first).await?,
        count(ctx, table, second).await?,
    );
    expect(left == (4, 10), || {
        format!("after the delete the projects hold {left:?} rows, not (4, 10)")
    })?;
    done(ctx, &delete).await?;
    let masked = ctx.store.masked_parts(table, partition).await?;
    expect(masked >= 1, || "no part carries a delete mask".to_owned())?;

    let mask = ctx
        .store
        .apply_deleted_mask(table, partition)
        .await
        .context("apply mask")?;
    done(ctx, &mask).await?;
    let masked = ctx.store.masked_parts(table, partition).await?;
    expect(masked == 0, || format!("{masked} parts still carry a mask"))?;
    let unlisted = MutationId::new(table, "mutation_999999999.txt".to_owned());
    expect(
        ctx.store.mutation_status(&unlisted).await? == MutationStatus::Done,
        || "a mutation the server does not list is not done".to_owned(),
    )?;

    // Deleting the rest of a project removes it from the partition's projects.
    let rest = DeletePlan::project(
        scope(first).map_err(|e| anyhow!("{e}"))?,
        table,
        partition,
        Pred::all(),
    )?;
    ctx.store.delete(rest).await?;
    let projects = ctx.store.partition_projects(table, partition).await?;
    expect(
        !projects.contains(&first) && projects.contains(&second),
        || "a fully deleted project is still listed".to_owned(),
    )?;

    // Dropping a partition removes its rows; dropping it again, or one that never existed, succeeds.
    let later = T0.saturating_add_micros(30 * DAY);
    let other = route(table, Granularity::Day, later)?;
    let rows = plain_rows(table, second, epoch, 3, later, 5, 2);
    ctx.insert(&rows, ctx.token(epoch, 3, table, later)?)
        .await?;
    for _ in 0..2 {
        ctx.store
            .drop_partition(table, other)
            .await
            .context("drop partition")?;
    }
    let left = count(ctx, table, second).await?;
    expect(left == 10, || {
        format!("after the drop the project holds {left} rows, not 10")
    })?;

    // Markers: invisible to partition projects, and purged by the marker delete.
    let rollup = TableId::SpanMetrics;
    ctx.admin("SYSTEM START MERGES crucible.span_metrics")
        .await?;
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let month = route(rollup, Granularity::Day, T0)?;
    ctx.insert(
        &rollup_rows(project, epoch, 1, T0, 3),
        ctx.token(epoch, 1, rollup, T0)?,
    )
    .await?;
    let probe = batch_probe(rollup, month, epoch, Lsn::new(1))?;
    expect(ctx.store.probe(probe).await?, || {
        "the marker is not there".to_owned()
    })?;
    let projects = ctx.store.partition_projects(rollup, month).await?;
    expect(
        projects.contains(&project) && projects.iter().all(|p| !p.is_nil()),
        || format!("rollup partition projects {projects:?}"),
    )?;
    let older_than = |t: Timestamp| DeletePlan::marker_purge(rollup, month, t, Pred::all());
    ctx.store
        .delete(older_than(T0.saturating_add_micros(-DAY))?)
        .await?;
    expect(ctx.store.probe(probe).await?, || {
        "a purge of older markers removed a newer one".to_owned()
    })?;
    ctx.store
        .delete(older_than(T0.saturating_add_micros(DAY))?)
        .await?;
    expect(!ctx.store.probe(probe).await?, || {
        "the marker survived its purge".to_owned()
    })?;
    let data = count(ctx, rollup, project).await?;
    expect(data == 3, || {
        format!("the marker purge left {data} data rows, not 3")
    })?;

    Ok(
        "a typed delete hides its rows at once and reports a mutation that finishes; the mask application \
         leaves no masked part; partition drops are idempotent; partition projects are ascending and never \
         the marker project; the marker purge removes markers older than its instant only"
            .to_owned(),
    )
}
