//! The insert protocol on one shard with several replicas (contracts 5.2 to 5.6 and 6.2): the rows that need
//! every replica, each reached directly at its own address (`--replica-url`).
//!
//! The faults are set up with statements, so the rows run against any cluster: `SYSTEM STOP FETCHES` on the other
//! replicas keeps a block on one replica only, so its quorum waits, and detaching the table on the replica that
//! holds it takes that replica out of the cluster's view, as a replica that went down does.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use crucible_auth::test_support::scope;
use crucible_env::{
    InsertError, InsertSettings, MutationId, MutationStatus, StoreError, TelemetryStore,
};
use crucible_plan::test_support::batch_probe;
use crucible_plan::{DeletePlan, Pred, QueryPlan, lifecycle_oldest};
use crucible_records::{Lsn, PartitionId, ProjectId, TableId, TimeWindow, Timestamp, WalEpoch};
use crucible_schema::{Granularity, ServiceIdentity, route};
use crucible_store_clickhouse::{ClickHouseStore, render};

use crate::blocks::Rows;
use crate::inserts::{plain_rows, rollup_rows};
use crate::{Ctx, T0, expect, statement};

const DAY: i64 = 86_400_000_000;

/// How long a test insert waits for its quorum before the server answers that the outcome is unknown.
const QUORUM_TIMEOUT_MS: &str = "3000";

/// The other replicas, or why the row cannot run.
fn others(ctx: &Ctx) -> Result<&[ClickHouseStore]> {
    if ctx.replicas.is_empty() {
        bail!(
            "the cluster rows read every replica: give each other replica's URL with --replica-url"
        );
    }
    Ok(&ctx.replicas)
}

/// Runs a statement as `crucible_admin` on one replica.
pub(crate) async fn admin_on(store: &ClickHouseStore, sql: impl Into<String>) -> Result<()> {
    let sql = sql.into();
    store
        .raw_command(ServiceIdentity::Admin, &statement(sql.clone()))
        .await
        .with_context(|| sql)
}

/// A number read as `crucible_admin` on one replica.
pub(crate) async fn admin_count_on(store: &ClickHouseStore, sql: impl Into<String>) -> Result<u64> {
    let sql = sql.into();
    let answer = store
        .raw_text(ServiceIdentity::Admin, &statement(sql.clone()))
        .await
        .with_context(|| sql)?;
    answer
        .trim()
        .parse()
        .map_err(|_| anyhow!("not a number: {answer:?}"))
}

/// The rows of `project` that one replica holds in `table`, after it has fetched every part it knows of.
async fn rows_on(store: &ClickHouseStore, table: TableId, project: ProjectId) -> Result<u64> {
    admin_on(
        store,
        format!("SYSTEM SYNC REPLICA crucible.{} LIGHTWEIGHT", table.name()),
    )
    .await?;
    admin_count_on(
        store,
        format!(
            "SELECT count() FROM crucible.{} WHERE project_id = '{project}'",
            table.name()
        ),
    )
    .await
}

/// Inserts a block on one replica through the production adapter, with the settings of contract 5.4.
async fn insert_on(
    store: &ClickHouseStore,
    rows: &Rows,
    settings: InsertSettings,
) -> Result<(), InsertError> {
    let block = rows.encode().map_err(|error| InsertError::Poison {
        code: 0,
        message: format!("the fixture block could not be built: {error:#}"),
    })?;
    store.insert_block(block, settings).await
}

/// The insert of contract 5.4 on the adapter's path, with a short quorum wait so that a test does not wait for
/// the server's default of ten minutes.
async fn insert_waiting_briefly(
    store: &ClickHouseStore,
    rows: &Rows,
    settings: InsertSettings,
) -> Result<(), InsertError> {
    let block = rows.encode().map_err(|error| InsertError::Poison {
        code: 0,
        message: format!("the fixture block could not be built: {error:#}"),
    })?;
    let mut pairs = settings.pairs();
    pairs.push(("insert_quorum_timeout", QUORUM_TIMEOUT_MS.to_owned()));
    store
        .raw_insert(
            &render::insert(settings.table()),
            block.compressed().clone(),
            &pairs,
        )
        .await
}

/// The existence probe of a block through one replica's adapter.
async fn probe_on(
    store: &ClickHouseStore,
    table: TableId,
    time: Timestamp,
    epoch: WalEpoch,
    seq: u64,
) -> Result<Result<bool, StoreError>> {
    let partition = route(table, Granularity::Day, time)?;
    Ok(store
        .probe(batch_probe(table, partition, epoch, Lsn::new(seq))?)
        .await)
}

/// Polls `check` every 250 ms for up to 30 s.
async fn eventually<F, Fut>(ctx: &Ctx, what: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    for _ in 0..120 {
        if check().await? {
            return Ok(());
        }
        ctx.env.clock.sleep(Duration::from_millis(250)).await;
    }
    bail!("{what} did not happen within 30 s")
}

/// One token sent through two replicas leaves one copy on every replica: the deduplication log of a replicated
/// table lives in Keeper, so every replica checks a resend against it, also one with other bytes.
pub(crate) async fn dedup_across_replicas(ctx: &Ctx) -> Result<String> {
    let others = others(ctx)?;
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let blocks = [
        (
            TableId::SpanCore,
            plain_rows(TableId::SpanCore, project, epoch, 1, T0, 100, 1),
            plain_rows(TableId::SpanCore, project, epoch, 1, T0, 7, 2),
            100,
        ),
        (
            TableId::SpanMetrics,
            rollup_rows(project, epoch, 1, T0, 50),
            rollup_rows(project, epoch, 1, T0, 9),
            50,
        ),
    ];
    for (table, rows, changed, expected) in blocks {
        let settings = InsertSettings::new(ctx.token(epoch, 1, table, T0)?, ctx.flavour);
        insert_on(&ctx.store, &rows, settings)
            .await
            .with_context(|| format!("{}: the first send", table.name()))?;
        for other in others {
            insert_on(other, &rows, settings).await.with_context(|| {
                format!("{}: the same block through another replica", table.name())
            })?;
            insert_on(other, &changed, settings)
                .await
                .with_context(|| {
                    format!(
                        "{}: other bytes under the token through another replica",
                        table.name()
                    )
                })?;
        }
        for (n, store) in std::iter::once(&ctx.store).chain(others).enumerate() {
            let held = rows_on(store, table, project).await?;
            expect(held == expected, || {
                format!(
                    "{}: replica {} holds {held} rows of the project, not {expected}",
                    table.name(),
                    n + 1
                )
            })?;
        }
    }
    Ok(format!(
        "a block sent through each of {} replicas, and other bytes under its token, left one copy in span_core \
         and span_metrics on every replica",
        others.len() + 1
    ))
}

/// The visibility grace of the clustered flavour (contract 8.6): how long the recent-span index keeps serving a span
/// after its checkpoint, so that a replica that lags behind has that long to fetch it.
const CLUSTERED_VISIBILITY_GRACE: Duration = Duration::from_secs(10);

/// C11: read-after-write on the replica that took an insert, and a replica that lags behind it, against the
/// visibility grace (contract 8.6). An acknowledged insert waited for a majority of the replicas, so the replica
/// that took it, and with two replicas the other one too, return its rows at once. A block that one replica holds
/// while another fetches nothing is missing from the second until it fetches again, and then appears within the
/// grace.
pub(crate) async fn read_after_write(ctx: &Ctx) -> Result<String> {
    let others = others(ctx)?;
    let table = TableId::SpanCore;
    let name = table.name();
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let time = Timestamp::from_micros(T0.micros() + 3 * DAY);
    let held = format!("SELECT count() FROM crucible.{name} WHERE project_id = '{project}'");

    ctx.insert(
        &plain_rows(table, project, epoch, 1, time, 10, 6),
        ctx.token(epoch, 1, table, time)?,
    )
    .await?;
    let here = admin_count_on(&ctx.store, &held).await?;
    expect(here == 10, || {
        format!("the replica that took an acknowledged insert of 10 rows returns {here}")
    })?;
    if others.len() == 1 {
        let there = admin_count_on(&others[0], &held).await?;
        expect(there == 10, || {
            format!(
                "with two replicas the other one returns {there} rows of an acknowledged insert, not 10"
            )
        })?;
    }

    for other in others {
        admin_on(other, format!("SYSTEM STOP FETCHES crucible.{name}")).await?;
    }
    let lagging = async {
        let rows = plain_rows(table, project, epoch, 2, time, 10, 7);
        let settings = InsertSettings::new(ctx.token(epoch, 2, table, time)?, ctx.flavour);
        let sent = insert_waiting_briefly(&ctx.store, &rows, settings).await;
        expect(
            matches!(sent, Err(InsertError::UnknownOutcome { .. })),
            || {
                format!(
                    "an insert whose quorum timed out answered {sent:?}, not an unknown outcome"
                )
            },
        )?;
        let here = admin_count_on(&ctx.store, &held).await?;
        let there = admin_count_on(&others[0], &held).await?;
        expect((here, there) == (20, 10), || {
            format!(
                "with fetches stopped, the replicas return {here} and {there} rows, not 20 and 10"
            )
        })?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for other in others {
        admin_on(other, format!("SYSTEM START FETCHES crucible.{name}")).await?;
    }
    lagging?;
    let resumed = ctx.env.clock.now();
    loop {
        if admin_count_on(&others[0], &held).await? == 20 {
            break;
        }
        expect(
            ctx.env.clock.now().saturating_duration_since(resumed) < CLUSTERED_VISIBILITY_GRACE,
            || {
                format!(
                    "the lagging replica did not return the block within the visibility grace of \
                     {CLUSTERED_VISIBILITY_GRACE:?} after it fetched again"
                )
            },
        )?;
        ctx.env.clock.sleep(Duration::from_millis(100)).await;
    }
    let caught_up = ctx.env.clock.now().saturating_duration_since(resumed);
    Ok(format!(
        "an acknowledged insert was read at once through the replica that took it{}; a block that the other \
         replica did not fetch was missing there until it fetched again, and then appeared after {caught_up:?}, \
         within the visibility grace of {CLUSTERED_VISIBILITY_GRACE:?}",
        if others.len() == 1 {
            " and through the other one"
        } else {
            ""
        }
    ))
}

/// A block acknowledged by one replica reads present through every replica's probe at once, because the insert
/// waits for a majority (`insert_quorum = auto`). A block whose quorum still waits is an unknown outcome, and
/// the probe does not read it present until a majority holds it: a block on one replica only is dropped when
/// that replica goes down before another fetched it (the next row).
pub(crate) async fn probe_before_quorum(ctx: &Ctx) -> Result<String> {
    let others = others(ctx)?;
    let table = TableId::SpanCore;
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let time = Timestamp::from_micros(T0.micros() + DAY);

    let rows = plain_rows(table, project, epoch, 1, time, 10, 1);
    insert_on(
        &ctx.store,
        &rows,
        InsertSettings::new(ctx.token(epoch, 1, table, time)?, ctx.flavour),
    )
    .await?;
    for (n, store) in std::iter::once(&ctx.store).chain(others).enumerate() {
        let read = probe_on(store, table, time, epoch, 1).await?;
        expect(matches!(read, Ok(true)), || {
            format!(
                "an acknowledged block probed {read:?} through replica {}",
                n + 1
            )
        })?;
    }

    for other in others {
        admin_on(
            other,
            format!("SYSTEM STOP FETCHES crucible.{}", table.name()),
        )
        .await?;
    }
    let waiting = async {
        let rows = plain_rows(table, project, epoch, 2, time, 10, 2);
        let settings = InsertSettings::new(ctx.token(epoch, 2, table, time)?, ctx.flavour);
        let sent = insert_waiting_briefly(&ctx.store, &rows, settings).await;
        expect(
            matches!(sent, Err(InsertError::UnknownOutcome { .. })),
            || {
                format!(
                    "an insert whose quorum timed out answered {sent:?}, not an unknown outcome"
                )
            },
        )?;
        let read = probe_on(&ctx.store, table, time, epoch, 2).await?;
        expect(!matches!(read, Ok(true)), || {
            "the probe read present a block that one replica holds and a majority does not"
                .to_owned()
        })?;
        Ok::<_, anyhow::Error>(read)
    }
    .await;
    for other in others {
        admin_on(
            other,
            format!("SYSTEM START FETCHES crucible.{}", table.name()),
        )
        .await?;
    }
    let while_waiting = waiting?;
    eventually(
        ctx,
        "the probe reading present once a majority holds the block",
        || async {
            Ok(matches!(
                probe_on(&ctx.store, table, time, epoch, 2).await?,
                Ok(true)
            ))
        },
    )
    .await?;
    Ok(format!(
        "an acknowledged block probed present through each of {} replicas; with the other replicas not fetching, \
         the insert was an unknown outcome and the probe answered {while_waiting:?} until a majority held the \
         block, then present",
        others.len() + 1
    ))
}

/// A replica that goes down while the block it holds waits for its quorum: another replica marks the quorum
/// failed, and the block is dropped when the replica returns. The probe then reads absent and the shipper sends
/// the block again under its token. Whatever the server does with that token, no acknowledged block may be
/// missing: the resend lands, or the adapter reports an unknown outcome and the block stays in the write-ahead
/// log.
pub(crate) async fn failed_quorum(ctx: &Ctx) -> Result<String> {
    let others = others(ctx)?;
    let table = TableId::FactFlags;
    let name = table.name();
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let partition = route(table, Granularity::Day, T0)?;
    let rows = Rows::new(table, 10)
        .all("project_id", crate::inserts::uuid(project.uuid()))
        .all("target_time", crucible_plan::Value::Time(T0))
        .all("target_day", crucible_plan::Value::Date(T0.date()))
        .each("span_id", |row| crate::inserts::id8(9, row))
        .all("wal_epoch", crate::inserts::uuid(epoch.uuid()))
        .all("batch_seq", crucible_plan::Value::UInt(1));
    let token = ctx.token_in(epoch, 1, table, partition)?;
    let settings = InsertSettings::new(token, ctx.flavour);

    for other in others {
        admin_on(other, format!("SYSTEM STOP FETCHES crucible.{name}")).await?;
    }
    let sent = insert_waiting_briefly(&ctx.store, &rows, settings).await;
    // The replica that holds the block goes away, so the others find no active replica that has it.
    admin_on(&ctx.store, format!("DETACH TABLE crucible.{name}")).await?;
    for other in others {
        admin_on(other, format!("SYSTEM START FETCHES crucible.{name}")).await?;
    }
    let failed = eventually(ctx, "another replica marking the quorum failed", || async {
        let mut failed = 0;
        for other in others {
            failed += admin_count_on(
                other,
                format!(
                    "SELECT count() FROM system.zookeeper WHERE path = (SELECT zookeeper_path FROM system.replicas \
                     WHERE database = 'crucible' AND table = '{name}') || '/quorum/failed_parts'"
                ),
            )
            .await?;
        }
        Ok(failed > 0)
    })
    .await;
    admin_on(&ctx.store, format!("ATTACH TABLE crucible.{name}")).await?;
    expect(
        matches!(sent, Err(InsertError::UnknownOutcome { .. })),
        || format!("an insert whose quorum timed out answered {sent:?}, not an unknown outcome"),
    )?;
    failed?;
    eventually(
        ctx,
        "the returning replica dropping the block of the failed quorum",
        || async { Ok(rows_on(&ctx.store, table, project).await? == 0) },
    )
    .await?;
    let read = probe_on(&ctx.store, table, T0, epoch, 1).await?;
    expect(matches!(read, Ok(false)), || {
        format!("the probe of a dropped block answered {read:?}, not absent")
    })?;

    let resent = insert_on(&ctx.store, &rows, settings).await;
    let mut held = Vec::new();
    for store in std::iter::once(&ctx.store).chain(others) {
        held.push(rows_on(store, table, project).await?);
    }
    match resent {
        Ok(()) => {
            expect(held.iter().all(|n| *n == 10), || {
                format!(
                    "the resend was acknowledged and the replicas hold {held:?} rows, not 10 each: the server \
                     kept the token of the failed quorum and dropped the resend"
                )
            })?;
            Ok("the quorum failed, the returning replica dropped the block, the probe read absent and the resend \
                landed on every replica"
                .to_owned())
        }
        Err(InsertError::UnknownOutcome { message, .. }) => {
            expect(held.iter().all(|n| *n == 0), || {
                format!("the resend was an unknown outcome and the replicas hold {held:?} rows")
            })?;
            Ok(format!(
                "the quorum failed, the returning replica dropped the block and the probe read absent; the server \
                 kept the token, so the resend was an unknown outcome and the block stays in the write-ahead log \
                 ({message})"
            ))
        }
        Err(other) => Err(other).context("the resend"),
    }
}

/// The masked parts of a table's partition on one replica, read there.
async fn masked_on(store: &ClickHouseStore, table: TableId, partition: PartitionId) -> Result<u64> {
    admin_count_on(
        store,
        format!(
            "SELECT count() FROM system.parts WHERE database = 'crucible' AND table = '{}' AND partition_id = \
             '{partition}' AND active AND has_lightweight_delete",
            table.name()
        ),
    )
    .await
}

/// Polls a mutation until every replica has run it.
async fn every_replica_done(ctx: &Ctx, id: &MutationId) -> Result<()> {
    eventually(ctx, "the mutation finishing on every replica", || async {
        match ctx.store.mutation_status(id).await? {
            MutationStatus::Done => Ok(true),
            MutationStatus::Pending => Ok(false),
            MutationStatus::Failed { code, reason } => {
                bail!("the mutation failed ({code}): {reason}")
            }
        }
    })
    .await
}

/// Retention and deletion on a cluster (contract 13.2 to 13.4): a typed delete, the mask application and a
/// partition drop sent to one replica run on every replica, and the store reads every replica before it calls
/// one done. With a replica down, the delete is queued for it, its mutation stays pending, the masked parts and
/// the verification read have no answer and the drop is sent again, until the replica is back.
pub(crate) async fn lifecycle(ctx: &Ctx) -> Result<String> {
    let others = others(ctx)?;
    let table = TableId::SpanPayload;
    let name = table.name();
    let (first, second, epoch) = (ctx.project(), ctx.project(), ctx.epoch());
    let day = Timestamp::from_micros(T0.micros() + 5 * DAY);
    let partition = route(table, Granularity::Day, day)?;
    for (seq, project) in [(1, first), (2, second)] {
        ctx.insert(
            &plain_rows(table, project, epoch, seq, day, 10, 3),
            ctx.token(epoch, seq, table, day)?,
        )
        .await?;
    }
    let window = TimeWindow::new(
        day.floor_day(),
        Timestamp::from_micros(day.floor_day().micros() + DAY),
    )?;
    let leftover = |project: ProjectId| -> Result<QueryPlan> {
        let scope = scope(project).map_err(|e| anyhow!("{e}"))?;
        Ok(lifecycle_oldest(scope, table, window, Pred::all())?.on_every_replica())
    };

    // A part that only another replica holds keeps its project listed, so a partition drop never removes it.
    let third = ctx.project();
    let elsewhere = Timestamp::from_micros(day.micros() + 2 * DAY);
    let held = route(table, Granularity::Day, elsewhere)?;
    admin_on(&ctx.store, format!("SYSTEM STOP FETCHES crucible.{name}")).await?;
    let listed = async {
        let settings = InsertSettings::new(ctx.token(epoch, 4, table, elsewhere)?, ctx.flavour);
        let rows = plain_rows(table, third, epoch, 4, elsewhere, 5, 5);
        let sent = insert_waiting_briefly(&others[0], &rows, settings).await;
        expect(
            matches!(sent, Err(InsertError::UnknownOutcome { .. })),
            || {
                format!(
                    "an insert whose quorum timed out answered {sent:?}, not an unknown outcome"
                )
            },
        )?;
        ctx.store
            .partition_projects(table, held)
            .await
            .context("the projects of a partition")
    }
    .await;
    admin_on(&ctx.store, format!("SYSTEM START FETCHES crucible.{name}")).await?;
    let listed = listed?;
    expect(listed == [third], || {
        format!("the projects of a partition whose only part another replica holds read {listed:?}")
    })?;

    // Every replica up: the delete, its mask and the verification hold on each.
    let delete = DeletePlan::project(
        scope(first).map_err(|e| anyhow!("{e}"))?,
        table,
        partition,
        Pred::all(),
    )?;
    let mutation = ctx.store.delete(delete).await.context("delete")?;
    every_replica_done(ctx, &mutation).await?;
    let mask = ctx
        .store
        .apply_deleted_mask(table, partition)
        .await
        .context("mask")?;
    every_replica_done(ctx, &mask).await?;
    let masked = ctx.store.masked_parts(table, partition).await?;
    expect(masked == 0, || {
        format!("{masked} masked parts on the replicas after the mask")
    })?;
    for (n, store) in std::iter::once(&ctx.store).chain(others).enumerate() {
        let (deleted, kept, masked) = (
            rows_on(store, table, first).await?,
            rows_on(store, table, second).await?,
            masked_on(store, table, partition).await?,
        );
        expect((deleted, kept, masked) == (0, 10, 0), || {
            format!(
                "replica {} holds {deleted} deleted rows, {kept} kept rows and {masked} masked parts, not 0, 10 \
                 and 0",
                n + 1
            )
        })?;
    }
    let rows = ctx.store.execute(leftover(first)?).await?.rows().await?;
    expect(rows.is_empty(), || {
        format!("the verification read found {rows:?}")
    })?;

    // One replica down: nothing is called done until it is back.
    let later = Timestamp::from_micros(day.micros() + DAY);
    let dropped = route(table, Granularity::Day, later)?;
    ctx.insert(
        &plain_rows(table, second, epoch, 3, later, 5, 4),
        ctx.token(epoch, 3, table, later)?,
    )
    .await?;
    let down = &others[0];
    admin_on(down, format!("DETACH TABLE crucible.{name}")).await?;
    let outage = async {
        let delete = DeletePlan::project(
            scope(second).map_err(|e| anyhow!("{e}"))?,
            table,
            partition,
            Pred::all(),
        )?;
        let mutation = ctx
            .store
            .delete(delete)
            .await
            .context("a delete while a replica is down")?;
        let status = ctx.store.mutation_status(&mutation).await?;
        expect(status == MutationStatus::Pending, || {
            format!("the delete's mutation is {status:?} while a replica is down")
        })?;
        let masked = ctx.store.masked_parts(table, partition).await;
        expect(matches!(masked, Err(StoreError::Unavailable(_))), || {
            format!("the masked parts answered {masked:?} while a replica is down")
        })?;
        let listed = ctx.store.partition_projects(table, partition).await;
        expect(listed.is_err(), || {
            format!("the projects of a partition read {listed:?} while a replica is down")
        })?;
        let read = match ctx.store.execute(leftover(second)?).await {
            Ok(stream) => stream.rows().await.map(|_| ()),
            Err(error) => Err(error),
        };
        expect(read.is_err(), || {
            "the verification read answered while a replica is down".to_owned()
        })?;
        let drop = ctx.store.drop_partition(table, dropped).await;
        expect(matches!(drop, Err(StoreError::Unavailable(_))), || {
            format!("the partition drop answered {drop:?} while a replica is down")
        })?;
        Ok::<_, anyhow::Error>(mutation)
    }
    .await;
    admin_on(down, format!("ATTACH TABLE crucible.{name}")).await?;
    let mutation = outage?;
    every_replica_done(ctx, &mutation).await?;
    ctx.store
        .drop_partition(table, dropped)
        .await
        .context("the drop once every replica is back")?;
    for (n, store) in std::iter::once(&ctx.store).chain(others).enumerate() {
        let kept = rows_on(store, table, second).await?;
        expect(kept == 0, || {
            format!(
                "replica {} holds {kept} rows of the delete made while it was down",
                n + 1
            )
        })?;
    }
    Ok(format!(
        "a part that only another replica held kept its project listed; a delete, its mask and a partition drop \
         sent to one replica held on all {}; with one down, the delete was queued for it, its mutation stayed \
         pending, the masked parts, the partition's projects and the verification read had no answer and the drop \
         was refused, until the replica came back and ran the delete",
        others.len() + 1
    ))
}
