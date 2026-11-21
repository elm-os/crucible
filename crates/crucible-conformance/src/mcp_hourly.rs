//! The hourly tier of the MCP tool rollup on real servers (contract 3.10): the MCP analytics plans that read it
//! over a long window, on the server and on `crucible_sim::ch_model::ChModel`, each equal to the same plan over
//! minute rows only; its retention delete and partition drop; and the schema step that adds it and copies the
//! minute rollup into it (`crucible_schema::schema_mcp_tool_rollup_hourly`).

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_env::TelemetryStore;
use crucible_plan::test_support::{batch_probe, minute_rows_only};
use crucible_plan::{
    Aggregation, Caller, DeletePlan, DimensionId, Fill, Filter, Granularity, Literal, McpRead,
    McpSelection, MeasureId, MetricsQuery, Pred, QueryClass, QueryPlan, RetentionClamp, Row,
    Source, SourceChoice, Step, Value, expired_before, lifecycle_oldest, mcp_clients,
    mcp_compare_tools, mcp_error_classes, mcp_metrics_ranking, mcp_metrics_series,
    mcp_metrics_totals, mcp_presence, mcp_servers, mcp_tool_clients, mcp_tool_series, mcp_tools,
};
use crucible_records::{
    CaptureSource, ErrorClass, Lsn, McpSide, ProjectId, ProjectScope, TableId, TimeWindow,
    Timestamp, WalEpoch,
};
use crucible_schema::{
    Granularity as Partitioning, Ident, RetentionSettings, route, schema_mcp_tool_rollup_hourly,
};

use crate::blocks::Rows;
use crate::differential::{finished, insert, rows, same, stamped};
use crate::reads::rollup;
use crate::{Ctx, T0, expect};

const MINUTE: i64 = 60_000_000;
const HOUR: i64 = 3_600_000_000;
/// The hours of the fixture, around the start of the hour of `T0`.
const HOURS: std::ops::Range<i64> = -3..28;
/// The minutes of each hour that hold rows.
const MINUTES: [i64; 2] = [5, 35];

/// One kind of rollup row, repeated at each minute of [`MINUTES`] in every hour of [`HOURS`].
#[derive(Clone, Copy)]
struct Kind {
    side: McpSide,
    server: &'static str,
    source: CaptureSource,
    target: &'static str,
    client: &'static str,
    class: ErrorClass,
    fault: u64,
    count: u64,
}

const fn kind(server: &'static str, source: CaptureSource, count: u64) -> Kind {
    Kind {
        side: McpSide::Server,
        server,
        source,
        target: "read",
        client: "agent",
        class: ErrorClass::None,
        fault: 0,
        count,
    }
}

/// A server captured by the SDK and the proxy (whose rows are never summed with the SDK's), one by OTLP only,
/// a caller fault, an error, two clients and the client side.
const KINDS: [Kind; 6] = [
    kind("files", CaptureSource::Sdk, 2),
    Kind {
        client: "ide",
        class: ErrorClass::Validation,
        fault: 1,
        ..kind("files", CaptureSource::Sdk, 1)
    },
    Kind {
        target: "write",
        class: ErrorClass::ToolExecution,
        ..kind("files", CaptureSource::Sdk, 1)
    },
    kind("files", CaptureSource::ProxyStdio, 3),
    Kind {
        target: "query",
        ..kind("db", CaptureSource::Otlp, 1)
    },
    Kind {
        side: McpSide::Client,
        ..kind("files", CaptureSource::Sdk, 4)
    },
];

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn histogram(bucket: u64, count: u64) -> Value {
    let array = |v: u64| Value::Array(vec![Value::UInt(v)]);
    Value::Tuple2(Box::new(array(bucket)), Box::new(array(count)))
}

fn empty_histogram() -> Value {
    Value::Tuple2(
        Box::new(Value::Array(Vec::new())),
        Box::new(Value::Array(Vec::new())),
    )
}

fn hour_start() -> Timestamp {
    T0.floor_hour()
}

/// The deploy of an hour: the server moves from `v1` to `v2` halfway through the fixture.
fn deploy(hour: i64) -> &'static str {
    if hour < 12 { "v1" } else { "v2" }
}

/// One data row of either tier: its time (the minute, or the start of the hour), its kind and hour, and how
/// many minute rows it stands for.
type Line = (Timestamp, usize, i64, u64);

/// The rows of `table`, a tier of the tool rollup: one data row per line and the marker row.
fn tier_rows(table: TableId, project: ProjectId, epoch: WalEpoch, lines: &[Line]) -> Rows {
    let time = match table {
        TableId::McpToolRollupHourly => "hour",
        _ => "minute",
    };
    let line = |row: usize| lines.get(row).copied();
    let of = |row: usize| line(row).and_then(|(_, k, _, _)| KINDS.get(k).copied());
    rollup(table, project, epoch, 0, lines.len())
        .each(time, |row| {
            // The marker row holds the first instant of the partition.
            Value::Time(
                line(row).map_or_else(|| Timestamp::from_micros(1_793_491_200_000_000), |l| l.0),
            )
        })
        .each("side", |row| {
            Value::Enum(of(row).map_or(McpSide::None, |k| k.side).value())
        })
        .each("server_name", |row| text(of(row).map_or("", |k| k.server)))
        .each("deploy_id", |row| {
            text(line(row).map_or("", |(_, _, hour, _)| deploy(hour)))
        })
        .each("capture_source", |row| {
            Value::Enum(of(row).map_or(CaptureSource::Sdk, |k| k.source).value())
        })
        .each("method", |row| {
            text(if of(row).is_some() { "tools/call" } else { "" })
        })
        .each("target_name", |row| text(of(row).map_or("", |k| k.target)))
        .each("client_name", |row| text(of(row).map_or("", |k| k.client)))
        .each("error_class", |row| {
            Value::Enum(of(row).map_or(ErrorClass::None, |k| k.class).value())
        })
        .each("caller_fault", |row| {
            Value::UInt(of(row).map_or(0, |k| k.fault))
        })
        .each("count", |row| {
            Value::UInt(line(row).map_or(0, |(_, k, _, n)| n * KINDS[k].count))
        })
        .each("request_bytes", |row| {
            Value::UInt(line(row).map_or(0, |(_, k, _, n)| 10 * n * KINDS[k].count))
        })
        .each("response_bytes", |row| {
            Value::UInt(line(row).map_or(0, |(_, k, _, n)| 100 * n * KINDS[k].count))
        })
        .each("dur_hist", |row| match line(row) {
            Some((_, k, _, n)) => histogram(100 + k as u64, n * KINDS[k].count),
            None => empty_histogram(),
        })
        .each("semconv_hist", |_| empty_histogram())
        .each("last_seen", |row| {
            Value::Time(line(row).map_or(Timestamp::EPOCH, |(_, _, hour, _)| {
                // The last minute row of the hour, one second in.
                hour_start().saturating_add_micros(hour * HOUR + MINUTES[1] * MINUTE + 1_000_000)
            }))
        })
}

/// The minute rows of the fixture, and what the shipper writes for the same operations in the hourly tier.
fn lines() -> (Vec<Line>, Vec<Line>) {
    let (mut minutes, mut hours) = (Vec::new(), Vec::new());
    for hour in HOURS {
        let start = hour_start().saturating_add_micros(hour * HOUR);
        for k in 0..KINDS.len() {
            for minute in MINUTES {
                minutes.push((start.saturating_add_micros(minute * MINUTE), k, hour, 1));
            }
            hours.push((start, k, hour, MINUTES.len() as u64));
        }
    }
    (minutes, hours)
}

async fn load(ctx: &Ctx) -> Result<ProjectScope> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let (minutes, hours) = lines();
    for (seq, table, lines) in [
        (700, TableId::McpToolRollup, &minutes),
        (701, TableId::McpToolRollupHourly, &hours),
    ] {
        let rows = stamped(tier_rows(table, project, epoch, lines), epoch, seq);
        insert(ctx, rows, ctx.token(epoch, seq, table, T0)?)
            .await
            .with_context(|| format!("the fixture of {}", table.name()))?;
    }
    scope(project).map_err(|e| anyhow!("{e}"))
}

fn sum_of(found: &[Row], at: usize) -> u64 {
    found
        .iter()
        .filter_map(|row| match row.values().get(at) {
            Some(Value::UInt(v)) => Some(*v),
            _ => None,
        })
        .sum()
}

fn images(found: &[Row]) -> Vec<String> {
    let mut images: Vec<String> = found
        .iter()
        .map(|row| format!("{:?}", row.values()))
        .collect();
    images.sort_unstable();
    images
}

/// The rows of a plan that reads the hourly tier, on the server and on the model (which must agree), checked
/// to be the rows of the same plan over minute rows only: invariant I7 for the tier.
async fn tiered(ctx: &Ctx, what: &str, plan: QueryPlan) -> Result<Vec<Row>> {
    expect(
        plan.root().tables().contains(&TableId::McpToolRollupHourly),
        || format!("{what} does not read the hourly tier"),
    )?;
    let over_tier = rows(ctx, plan.clone())
        .await
        .with_context(|| what.to_owned())?;
    let over_minutes = rows(ctx, minute_rows_only(&plan))
        .await
        .with_context(|| format!("{what} over minute rows"))?;
    same(what, &images(&over_tier), &images(&over_minutes))?;
    Ok(over_tier)
}

fn live<E: std::fmt::Display>(plan: Result<Option<QueryPlan>, E>) -> Result<QueryPlan> {
    plan.map_err(|e| anyhow!("{e}"))?
        .context("the builder returned no plan")
}

/// Every MCP analytics builder that reads the tool rollup, over a 26-hour window that starts 37 minutes into an
/// hour: whole hours from the tier, and the minutes of the last partial hour from the minute rollup (the first
/// partial hour holds none of the window's rows). Then the tier's retention delete and partition drop.
pub(crate) async fn reads(ctx: &Ctx) -> Result<String> {
    let scope = load(ctx).await?;
    let from = hour_start().saturating_add_micros(37 * MINUTE);
    let window =
        TimeWindow::new(from, from.saturating_add_micros(26 * HOUR)).map_err(|e| anyhow!("{e}"))?;
    let clamp = RetentionClamp::new(RetentionSettings::default(), T0);
    let caller = Caller::Session;
    // The window's minute rows of one kind: two in each of its 25 whole hours, and 11:05 and 11:35 of the last.
    let per_kind = 25 * 2 + 2;
    let mut plans = 0;
    for merged in [false, true] {
        if merged {
            ctx.model.merge_all();
        }
        let when = if merged { "after" } else { "before" };
        let presence = tiered(
            ctx,
            "the presence",
            live(mcp_presence(
                scope,
                window,
                &clamp,
                caller.class(QueryClass::Analytics),
                McpSide::Server,
                None,
            ))?,
        )
        .await?;
        let pairs: Vec<(String, CaptureSource)> = presence
            .iter()
            .filter_map(|row| match (row.values().first(), row.values().get(1)) {
                (Some(Value::String(server)), Some(Value::Enum(source))) => {
                    Some((server.clone(), CaptureSource::from_value(*source)?))
                }
                _ => None,
            })
            .collect();
        expect(pairs.len() == 3, || {
            format!("the presence {when} the merge: {pairs:?}")
        })?;
        let selection = McpSelection::select(SourceChoice::Auto, pairs);
        let read = McpRead {
            side: McpSide::Server,
            selection: &selection,
            include_caller_faults: false,
            server: None,
        };
        let files = McpRead {
            server: Some("files"),
            ..read
        };
        // The SDK's rows of files (2 + 1 + 1 a minute) and the OTLP rows of db (1), never the proxy's.
        let servers = tiered(
            ctx,
            "the servers",
            live(mcp_servers(scope, window, &clamp, caller, &read))?,
        )
        .await?;
        expect(sum_of(&servers, 4) == 5 * per_kind, || {
            format!("the servers' calls {when} the merge: {servers:?}")
        })?;
        let tools = tiered(
            ctx,
            "the tools",
            live(mcp_tools(scope, window, &clamp, caller, &read))?,
        )
        .await?;
        expect(sum_of(&tools, 4) == 5 * per_kind, || {
            format!("the tools {when} the merge: {tools:?}")
        })?;
        let series = live(mcp_tool_series(
            scope,
            window,
            &clamp,
            caller,
            &files,
            "read",
            Step::Hour,
        ))?;
        let series = tiered(ctx, "the tool series", series).await?;
        expect(sum_of(&series, 3) == 3 * per_kind, || {
            format!("the tool series {when} the merge: {series:?}")
        })?;
        let clients = live(mcp_tool_clients(
            scope, window, &clamp, caller, &files, "read",
        ))?;
        tiered(ctx, "the tool clients", clients).await?;
        let classes = live(mcp_error_classes(scope, window, &clamp, caller, &read))?;
        tiered(ctx, "the error classes", classes).await?;
        let by_client = live(mcp_clients(scope, window, &clamp, caller, &read))?;
        tiered(ctx, "the clients", by_client).await?;
        let deploys = ["v1".to_owned(), "v2".to_owned()];
        let compare = live(mcp_compare_tools(
            scope, window, &clamp, caller, &files, &deploys,
        ))?;
        let compare = tiered(ctx, "the compare", compare).await?;
        expect(
            compare
                .iter()
                .any(|row| row.values().first() == Some(&text("v1"))),
            || format!("the compare {when} the merge: {compare:?}"),
        )?;
        plans += 8;

        // The mcp_ops metrics source in hourly buckets, zero-filled, and an alert's totals.
        let metrics = |measure: MeasureId, aggregation: Aggregation| MetricsQuery {
            source: Source::McpOps,
            measures: vec![(measure, aggregation)],
            dimensions: vec![DimensionId::McpServer],
            filter: Filter::all(),
            granularity: Granularity::Step(Step::Hour),
            fill: Fill::Zero,
            top_n: Some(5),
        };
        let calls = metrics(MeasureId::McpCalls, Aggregation::Sum);
        let ranking = live(mcp_metrics_ranking(
            scope, window, &clamp, caller, &calls, &read,
        ))?;
        tiered(ctx, "the metrics ranking", ranking).await?;
        let keys = [vec![Literal::String("files".to_owned())]];
        let series = live(mcp_metrics_series(
            scope,
            window,
            &clamp,
            caller,
            &calls,
            (&read, Some(&keys)),
        ))?;
        let series = tiered(ctx, "the metrics series", series).await?;
        // Caller faults are left out: 2 + 1 a minute.
        expect(sum_of(&series, 2) == 3 * per_kind, || {
            format!("the metrics series {when} the merge: {series:?}")
        })?;
        let rate = metrics(MeasureId::McpErrorRate, Aggregation::Rate);
        let rate = live(mcp_metrics_series(
            scope,
            window,
            &clamp,
            caller,
            &rate,
            (&read, Some(&keys)),
        ))?;
        tiered(ctx, "the error rate series", rate).await?;
        let totals = live(mcp_metrics_totals(
            scope,
            window,
            &clamp,
            &calls,
            &read,
            (true, 5),
        ))?;
        tiered(ctx, "an alert's totals", totals).await?;
        plans += 4;
    }

    // Retention (contract 13.2): the tier's oldest row, the delete of the hours before a cut-off in the monthly
    // partition that straddles it, and the drop of the partition, on both stores alike.
    let table = TableId::McpToolRollupHourly;
    let every = TimeWindow::new(
        Timestamp::EPOCH,
        T0.saturating_add_micros(30 * 86_400_000_000),
    )
    .map_err(|e| anyhow!("{e}"))?;
    let oldest = async || -> Result<Vec<Row>> {
        rows(ctx, lifecycle_oldest(scope, table, every, Pred::all())?).await
    };
    let first = oldest().await?;
    let start = hour_start().saturating_add_micros(HOURS.start * HOUR);
    expect(
        first.first().map(|r| r.values().to_vec()) == Some(vec![Value::Time(start)]),
        || format!("the tier's oldest row: {first:?}"),
    )?;
    // The cut-off at the start of the fixture's second day: the hours of the first go.
    let cutoff = T0.floor_day().saturating_add_micros(86_400_000_000);
    let partition = route(table, Partitioning::Day, T0)?;
    let both: [&dyn TelemetryStore; 2] = [&ctx.store, &ctx.model];
    let delete = DeletePlan::project(scope, table, partition, expired_before(table, cutoff)?)?;
    for store in both {
        let id = store
            .delete(delete.clone())
            .await
            .context("the retention delete of the tier")?;
        finished(ctx, store, &id).await?;
    }
    let after = oldest().await?;
    expect(
        after.first().map(|r| r.values().to_vec()) == Some(vec![Value::Time(cutoff)]),
        || format!("the tier's oldest row after the cut-off {cutoff}: {after:?}"),
    )?;
    for store in both {
        store
            .drop_partition(table, partition)
            .await
            .context("the drop of the tier's partition")?;
    }
    let gone = oldest().await?;
    expect(gone.is_empty(), || format!("the drop left {gone:?}"))?;
    let violations = ctx.model.violations();
    expect(violations.is_empty(), || {
        format!("the model recorded {violations:?}")
    })?;
    Ok(format!(
        "{plans} plans of the MCP analytics builders over a 26-hour window (presence, servers, tools, one tool's \
         hourly series and clients, error classes, clients, compare, and the mcp_ops metrics ranking, hourly \
         series and an alert's totals) read whole hours from mcp_tool_rollup_hourly and returned, on the server \
         and on the store model, before and after the model merged its parts, the rows of the same plans over \
         minute rows only; the tier's retention delete before a cut-off and its partition drop agreed"
    ))
}

/// One minute block of the tool rollup for batch `seq` of `epoch`, as an earlier version shipped it.
async fn minute_block(
    ctx: &Ctx,
    project: ProjectId,
    epoch: WalEpoch,
    seq: u64,
    hour: i64,
) -> Result<()> {
    let lines: Vec<Line> = (0..KINDS.len())
        .flat_map(|k| {
            MINUTES.map(|minute| {
                let at = hour_start().saturating_add_micros(hour * HOUR + minute * MINUTE);
                (at, k, hour, 1)
            })
        })
        .collect();
    let table = TableId::McpToolRollup;
    let rows = stamped(tier_rows(table, project, epoch, &lines), epoch, seq).sorted();
    ctx.insert(&rows, ctx.token(epoch, seq, table, T0)?)
        .await
        .map_err(|e| anyhow!("{e:?}"))
}

/// Schema step 0005 against a deployment of the earlier version: the minute rollup holds two batches and the
/// hourly table is empty, as the base schema of this version creates it. A copy that stopped part-way is
/// refused with the fix; after the fix the step copies every minute row by hour and every marker row, so the
/// hourly pairs of the two batches probe present (a node that recovers either batch does not send its hourly
/// rows again) while another batch's probe absent; a tiered read equals the minute read; and the step applied
/// again changes nothing.
pub(crate) async fn backfill(ctx: &Ctx) -> Result<String> {
    let tier = "crucible.mcp_tool_rollup_hourly";
    let step = schema_mcp_tool_rollup_hourly(&ctx.variant, &Ident::new(&ctx.config.admin.user)?);
    let (project, epoch) = (ctx.project(), ctx.epoch());
    ctx.admin(format!("TRUNCATE TABLE {tier}")).await?;
    for (seq, hour) in [(801, 0), (802, 30)] {
        minute_block(ctx, project, epoch, seq, hour).await?;
    }

    // A copy that stopped part-way: rows without the record of a finished copy.
    ctx.admin(format!(
        "INSERT INTO {tier} (project_id, side, hour, count) VALUES ('{}', 'server', toDateTime({}, 'UTC'), 1)",
        project.uuid(),
        hour_start().micros() / 1_000_000
    ))
    .await?;
    let refused = ctx.store.apply_migration(&step).await;
    expect(
        matches!(&refused, Err(error) if format!("{error:?}").contains("TRUNCATE TABLE crucible.mcp_tool_rollup_hourly")),
        || format!("the step over an incomplete copy answered {refused:?}"),
    )?;
    ctx.admin(format!("TRUNCATE TABLE {tier}")).await?;
    ctx.store
        .apply_migration(&step)
        .await
        .context("the step after the fix")?;
    let held = format!("SELECT count() FROM {tier}");
    let copied = ctx.admin_count(&held).await?;

    // Both batches' hourly pairs read present; a batch the minute rollup never had reads absent.
    let partition = route(TableId::McpToolRollupHourly, Partitioning::Day, T0)?;
    for (seq, present) in [(801, true), (802, true), (803, false)] {
        let probe = batch_probe(
            TableId::McpToolRollupHourly,
            partition,
            epoch,
            Lsn::new(seq),
        )?;
        let found = ctx.store.probe(probe).await?;
        expect(found == present, || {
            format!("the hourly pair of batch {seq} probes {found}, not {present}")
        })?;
    }

    // A tiered read of the project equals its minute read, on the server: both batches' hours, 12 minute rows
    // each of the five server kinds.
    let scope = scope(project).map_err(|e| anyhow!("{e}"))?;
    let window = TimeWindow::new(
        hour_start().saturating_add_micros(-HOUR),
        hour_start().saturating_add_micros(32 * HOUR),
    )
    .map_err(|e| anyhow!("{e}"))?;
    let clamp = RetentionClamp::new(RetentionSettings::default(), T0);
    let selection = McpSelection::select(
        SourceChoice::Auto,
        [
            ("files".to_owned(), CaptureSource::Sdk),
            ("files".to_owned(), CaptureSource::ProxyStdio),
            ("db".to_owned(), CaptureSource::Otlp),
        ],
    );
    let read = McpRead {
        side: McpSide::Server,
        selection: &selection,
        include_caller_faults: true,
        server: None,
    };
    let plan = live(mcp_servers(scope, window, &clamp, Caller::Session, &read))?;
    expect(
        plan.root().tables().contains(&TableId::McpToolRollupHourly),
        || "the servers plan does not read the tier".to_owned(),
    )?;
    let read_rows = async |plan: QueryPlan| -> Result<Vec<Row>> {
        Ok(ctx.store.execute(plan).await?.rows().await?)
    };
    let over_tier = read_rows(plan.clone()).await?;
    let over_minutes = read_rows(minute_rows_only(&plan)).await?;
    same(
        "the copied project's servers",
        &images(&over_tier),
        &images(&over_minutes),
    )?;
    expect(sum_of(&over_tier, 4) == 2 * 2 * 5, || {
        format!("the copied project's calls: {over_tier:?}")
    })?;

    // Again: the record of the finished copy makes it a no-op.
    ctx.store
        .apply_migration(&step)
        .await
        .context("the step applied again")?;
    let again = ctx.admin_count(&held).await?;
    expect(again == copied, || {
        format!("the step applied again changed the hourly table from {copied} to {again} rows")
    })?;
    let done = ctx
        .admin_count(format!(
            "SELECT count() FROM {tier} WHERE marker_epoch = toUUID('{}')",
            crucible_schema::HOURLY_COPY_DONE
        ))
        .await?;
    expect(done == 1, || {
        format!("{done} rows record the finished copy")
    })?;
    Ok(format!(
        "step 0005 refused an incomplete copy and named the fix; after it, it copied the minute rollup by hour \
         ({copied} rows, two batches' markers included, so both batches' hourly pairs probe present and another \
         batch's absent), a tiered read of the copied project equals its minute read, and the step applied again \
         changed nothing"
    ))
}
