//! Retention and deletion as `crucible-lifecycle` issues them (contract 13.2 to 13.4), on the server and on the
//! store model with every answer compared: the resolution of a request's traces, a read that carries the
//! pending-deletion set, the delete of a request in a partition, the retention delete in a partition that
//! straddles the cut-off, the read that verifies a delete, the mask application, the partition drop, and a
//! deletion by user on the MCP operations by `user_id` and subject hash with the issuers it hashes under.

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_env::TelemetryStore;
use crucible_plan::{
    CmpOp, DeletePlan, Hidden, HiddenSelector, Literal, Pred, QueryPlan, Row, TRACE_ID_LIST_CHUNK,
    TraceSelector, Value, deletion_issuers, deletion_traces, expired_before, lifecycle_oldest,
    mcp_user,
};
use crucible_records::{ProjectId, TableId, TimeWindow, Timestamp, TraceId, WalEpoch};
use crucible_schema::{Column, Granularity, McpOpsCol, SpanPayloadCol, route};

use crate::blocks::Rows;
use crate::differential::{finished, insert, rows, same};
use crate::inserts::{plain_rows, uuid};
use crate::{Ctx, T0, expect};

const SECOND: i64 = 1_000_000;
const DAY: i64 = 86_400 * SECOND;
const SALT: u32 = 77;

fn trace(n: u8) -> TraceId {
    TraceId::from_bytes([n; 16])
}

/// `count` spans of the trace `n`, of one session, received at `received_at`. Trace 1 is of the user `ada`,
/// any other of `bob`.
fn spans(
    table: TableId,
    project: ProjectId,
    epoch: WalEpoch,
    seq: u64,
    n: u8,
    received_at: Timestamp,
    count: usize,
) -> Rows {
    plain_rows(
        table,
        project,
        epoch,
        seq,
        T0,
        count,
        SALT + u32::from(n) + u32::try_from(seq).unwrap_or(0) * 100,
    )
    .all("trace_id", Value::Bytes(trace(n).to_bytes().to_vec()))
    .all("trace_hash", Value::UInt(u64::from(trace(n).trace_hash())))
    .all(
        "user_id",
        Value::String(if n == 1 { "ada" } else { "bob" }.to_owned()),
    )
    .all("session_id", Value::String(format!("s-{n}")))
    .all("received_at", Value::Time(received_at))
    .all("lsn", Value::UInt(seq))
}

fn facts(project: ProjectId, epoch: WalEpoch, seq: u64, time: Timestamp, count: usize) -> Rows {
    Rows::new(TableId::FactScores, count)
        .all("project_id", uuid(project.uuid()))
        .all("target_time", Value::Time(time))
        .all("target_day", Value::Date(time.date()))
        .all("trace_id", Value::Bytes(trace(1).to_bytes().to_vec()))
        .each("key", |row| Value::String(format!("k{seq}-{row}")))
        .all("wal_epoch", uuid(epoch.uuid()))
        .all("batch_seq", Value::UInt(seq))
        .all("lsn", Value::UInt(seq))
        .all("received_at", Value::Time(time))
}

fn ids(found: &[Row]) -> Vec<Vec<u8>> {
    found
        .iter()
        .filter_map(|row| match row.values().first() {
            Some(Value::Bytes(id)) => Some(id.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) async fn lifecycle(ctx: &Ctx) -> Result<String> {
    let (project, other, epoch) = (ctx.project(), ctx.project(), ctx.epoch());
    let scope = scope(project).map_err(|e| anyhow!("{e}"))?;
    let both: [&dyn TelemetryStore; 2] = [&ctx.store, &ctx.model];
    let table = TableId::SpanCore;
    // The deletes run on `span_payload`: the insert rows stop the merges of `span_core` to count parts, and a
    // table whose merges are stopped runs no mutation either.
    let payload = TableId::SpanPayload;
    let payload_partition = route(payload, Granularity::Day, T0)?;
    let requested_at = T0.saturating_add_micros(100 * SECOND);
    let (before, after) = (
        T0.saturating_add_micros(10 * SECOND),
        T0.saturating_add_micros(200 * SECOND),
    );
    // Trace 1 of ada: three spans before the request and two after it. Trace 2 of bob, before it. Another
    // project holds a trace with the same id and the same user.
    let blocks = |table: TableId| {
        [
            (1, spans(table, project, epoch, 1, 1, before, 3)),
            (2, spans(table, project, epoch, 2, 1, after, 2)),
            (3, spans(table, project, epoch, 3, 2, before, 2)),
            (4, spans(table, other, epoch, 4, 1, before, 4)),
        ]
    };
    for target in [table, payload] {
        for (seq, block) in blocks(target) {
            insert(ctx, block, ctx.token(epoch, seq, target, T0)?).await?;
        }
    }
    let mut reads = 0usize;
    let mut read = async |plan: QueryPlan, what: &str| -> Result<Vec<Row>> {
        reads += 1;
        rows(ctx, plan).await.with_context(|| what.to_owned())
    };

    // 13.3, selector resolution: the traces a request selects among the spans received before it.
    let ada = TraceSelector::User("ada");
    let resolved = read(
        deletion_traces(scope, ada, requested_at, None, 1_000)?,
        "the traces of a user",
    )
    .await?;
    same(
        "the traces of a user",
        &ids(&resolved),
        &vec![trace(1).to_bytes().to_vec()],
    )?;
    let traces = [trace(1), trace(2), trace(9)];
    let listed = read(
        deletion_traces(scope, TraceSelector::Traces(&traces), requested_at, None, 1)?,
        "the first page of a list of traces",
    )
    .await?;
    same(
        "the first page",
        &ids(&listed),
        &vec![trace(1).to_bytes().to_vec()],
    )?;
    let next = read(
        deletion_traces(
            scope,
            TraceSelector::Traces(&traces),
            requested_at,
            Some(trace(1)),
            1,
        )?,
        "the page after it",
    )
    .await?;
    same(
        "the page after it",
        &ids(&next),
        &vec![trace(2).to_bytes().to_vec()],
    )?;
    let session = read(
        deletion_traces(
            scope,
            TraceSelector::Session("s-2"),
            requested_at,
            None,
            1_000,
        )?,
        "the traces of a session",
    )
    .await?;
    same(
        "the traces of a session",
        &ids(&session),
        &vec![trace(2).to_bytes().to_vec()],
    )?;

    // The longest lists a statement carries. The executor resolves the traces of a request a chunk of ids at
    // a time, and a read hides at most `HIDDEN_TRACE_IDS` of a pending request: every id is a parameter in
    // the text of the statement, which the server bounds.
    let synthetic = |n: u128| TraceId::from_bytes(((0xabu128 << 120) | n).to_be_bytes());
    let mut chunk: Vec<TraceId> = (0..TRACE_ID_LIST_CHUNK as u128 - 1)
        .map(synthetic)
        .collect();
    chunk.push(trace(2));
    let among_many = read(
        deletion_traces(
            scope,
            TraceSelector::Traces(&chunk),
            requested_at,
            None,
            1_000,
        )?,
        "the traces of one chunk of a list",
    )
    .await?;
    same(
        "the traces of one chunk of a list",
        &ids(&among_many),
        &vec![trace(2).to_bytes().to_vec()],
    )?;
    let mut longest = vec![trace(2)];
    longest.extend((0..9_999u128).map(synthetic));
    let pending_longest = [Hidden {
        selector: HiddenSelector::Traces(longest),
        requested_at,
    }];
    let under_many = read(
        deletion_traces(
            scope,
            TraceSelector::Traces(&traces),
            requested_at,
            None,
            1_000,
        )?
        .hiding(&pending_longest),
        "a read under a pending request of 10,000 traces",
    )
    .await?;
    same(
        "a read under a pending request of 10,000 traces",
        &ids(&under_many),
        &vec![trace(1).to_bytes().to_vec()],
    )?;

    // 13.4, typed reads: a plan that carries the pending-deletion set returns none of the selected rows.
    let every =
        TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE).map_err(|e| anyhow!("{e}"))?;
    let pending = |selector: HiddenSelector| Hidden {
        selector,
        requested_at,
    };
    for (hidden, expected, what) in [
        (Vec::new(), (1, 2), "no pending deletion"),
        // The spans of trace 1 that arrived after the request stay visible.
        (
            vec![pending(HiddenSelector::Traces(vec![trace(1)]))],
            (1, 2),
            "a pending trace deletion",
        ),
        (
            vec![pending(HiddenSelector::User("bob".to_owned()))],
            (1, 1),
            "a pending user deletion",
        ),
        (
            vec![
                pending(HiddenSelector::Session("s-2".to_owned())),
                Hidden {
                    selector: HiddenSelector::User("ada".to_owned()),
                    requested_at: Timestamp::MAX_EXCLUSIVE,
                },
            ],
            (0, 0),
            "two pending deletions",
        ),
    ] {
        let user = read(
            deletion_traces(scope, ada, Timestamp::MAX_EXCLUSIVE, None, 1_000)?.hiding(&hidden),
            "the traces of the user under the pending set",
        )
        .await?
        .len();
        let all = read(
            deletion_traces(
                scope,
                TraceSelector::Traces(&traces),
                Timestamp::MAX_EXCLUSIVE,
                None,
                1_000,
            )?
            .hiding(&hidden),
            "every trace under the pending set",
        )
        .await?
        .len();
        let found = (user, all);
        expect(found == expected, || {
            format!("under {what} the reads return {found:?} traces, not {expected:?}")
        })?;
    }
    let early = read(
        lifecycle_oldest(scope, table, every, Pred::all())?
            .hiding(&[pending(HiddenSelector::Traces(vec![trace(1), trace(2)]))]),
        "the oldest span under the pending set",
    )
    .await?;
    expect(early.len() == 1, || {
        format!("the spans received after the request should stay visible: {early:?}")
    })?;

    // 13.3 step 5: the delete of a request in a partition, among the rows received before the request.
    // A full page of the executor: as many trace ids as one delete carries.
    let mut page: Vec<Literal> = (0..TRACE_ID_LIST_CHUNK as u128 - 1)
        .map(|n| Literal::from(synthetic(n)))
        .collect();
    page.push(Literal::from(trace(1)));
    let trace_id = SpanPayloadCol::TraceId.column_ref();
    let selected = Pred::And(vec![
        Pred::Or(vec![
            Pred::In(trace_id, page),
            Pred::eq(SpanPayloadCol::UserId, "nobody"),
        ]),
        Pred::cmp(SpanPayloadCol::ReceivedAt, CmpOp::Lt, requested_at),
    ]);
    let held = |scope, pred: Pred| lifecycle_oldest(scope, payload, every, pred);
    let delete = DeletePlan::project(scope, payload, payload_partition, selected.clone())?;
    for store in both {
        let id = store
            .delete(delete.clone())
            .await
            .context("the delete of a request")?;
        finished(ctx, store, &id).await?;
    }
    // 13.3 step 7: the read that verifies it finds nothing. The rows of the trace that arrived later, the
    // other trace and the other project are still there.
    let left = read(
        held(scope, selected.clone())?,
        "the verification of the delete",
    )
    .await?;
    expect(left.is_empty(), || format!("the delete left {left:?}"))?;
    let other_scope = crucible_auth::test_support::scope(other).map_err(|e| anyhow!("{e}"))?;
    for (whose, pred, what) in [
        (
            scope,
            Pred::In(trace_id, vec![Literal::from(trace(1))]),
            "the later rows of the deleted trace",
        ),
        (
            scope,
            Pred::In(trace_id, vec![Literal::from(trace(2))]),
            "the other trace",
        ),
        (other_scope, Pred::all(), "the other project"),
    ] {
        let kept = read(held(whose, pred)?, what).await?;
        expect(!kept.is_empty(), || format!("the delete removed {what}"))?;
    }

    // 13.2 step 6: the mask application removes the masked rows for good.
    for store in both {
        let masked = store.masked_parts(payload, payload_partition).await?;
        expect(masked >= 1, || "no part carries a delete mask".to_owned())?;
        let mask = store
            .apply_deleted_mask(payload, payload_partition)
            .await
            .context("the mask application")?;
        finished(ctx, store, &mask).await?;
        let masked = store.masked_parts(payload, payload_partition).await?;
        expect(masked == 0, || format!("{masked} parts still carry a mask"))?;
    }
    let left = read(held(scope, selected)?, "the verification after the mask").await?;
    expect(left.is_empty(), || format!("the mask left {left:?}"))?;

    // 13.2 step 3: a monthly partition that straddles the cut-off loses the rows before the cut-off day.
    let facts_table = TableId::FactScores;
    let cutoff = Timestamp::from_micros(T0.micros() - T0.micros().rem_euclid(DAY));
    let (old, new) = (cutoff.saturating_add_micros(-SECOND), cutoff);
    let month = route(facts_table, Granularity::Day, new)?;
    expect(route(facts_table, Granularity::Day, old)? == month, || {
        "the fixture days are not in one month".to_owned()
    })?;
    for (seq, time, count) in [(11, old, 3), (12, new, 2)] {
        let block = facts(project, epoch, seq, time, count);
        insert(ctx, block, ctx.token(epoch, seq, facts_table, time)?).await?;
    }
    let expired = expired_before(facts_table, cutoff)?;
    let retention = DeletePlan::project(scope, facts_table, month, expired.clone())?;
    for store in both {
        let id = store
            .delete(retention.clone())
            .await
            .context("the retention delete")?;
        finished(ctx, store, &id).await?;
    }
    let left = read(
        lifecycle_oldest(scope, facts_table, every, expired)?,
        "the verification of the retention delete",
    )
    .await?;
    expect(left.is_empty(), || {
        format!("the retention delete left {left:?}")
    })?;
    let oldest = read(
        lifecycle_oldest(scope, facts_table, every, Pred::all())?,
        "the oldest fact after the retention delete",
    )
    .await?;
    expect(
        oldest.first().map(Row::values) == Some(&[Value::Time(new)][..]),
        || format!("the oldest fact after the retention delete is {oldest:?}"),
    )?;

    // 13.2 step 2: a partition whose every project has expired is dropped.
    for store in both {
        store
            .drop_partition(payload, payload_partition)
            .await
            .context("the partition drop")?;
        let projects = store.partition_projects(payload, payload_partition).await?;
        expect(
            !projects.contains(&project) && !projects.contains(&other),
            || format!("the dropped partition still lists {projects:?}"),
        )?;
    }
    let gone = read(held(scope, Pred::all())?, "the payloads after the drop").await?;
    expect(gone.is_empty(), || format!("the drop left {gone:?}"))?;

    // 13.3, a deletion by user on the MCP tables: by `user_id` and by the subject hash under every issuer the
    // project's operations carry, whether or not a span names the user. Trace 5 carries ada only as a hash,
    // trace 6 as `user_id`, trace 7 is bob's under the other issuer; trace 5 also has an operation that
    // arrived after the request.
    let ops = TableId::McpOps;
    let ops_partition = route(ops, Granularity::Day, T0)?;
    let (ada_a, ada_b, bob_b) = ([0xa1; 16], [0xa2; 16], [0xb2; 16]);
    let operation = |seq: u64, n: u8, user: &str, hash: [u8; 16], issuer: &str, at: Timestamp| {
        plain_rows(
            ops,
            project,
            epoch,
            seq,
            T0,
            2,
            SALT + u32::try_from(seq).unwrap_or(0) * 100,
        )
        .all("trace_id", Value::Bytes(trace(n).to_bytes().to_vec()))
        .all("trace_hash", Value::UInt(u64::from(trace(n).trace_hash())))
        .all("user_id", Value::String(user.to_owned()))
        .all("user_subject_hash", Value::Bytes(hash.to_vec()))
        .all("user_issuer", Value::String(issuer.to_owned()))
        .all("received_at", Value::Time(at))
        .all("lsn", Value::UInt(seq))
    };
    for (seq, block) in [
        (21, operation(21, 5, "", ada_a, "iss-a", before)),
        (22, operation(22, 6, "ada", [0; 16], "", before)),
        (23, operation(23, 7, "", bob_b, "iss-b", before)),
        (24, operation(24, 5, "", ada_a, "iss-a", after)),
    ] {
        insert(ctx, block, ctx.token(epoch, seq, ops, T0)?).await?;
    }
    let issuers = |found: &[Row]| -> Vec<String> {
        found
            .iter()
            .filter_map(|row| match row.values().first() {
                Some(Value::String(issuer)) => Some(issuer.clone()),
                _ => None,
            })
            .collect()
    };
    let seen = read(
        deletion_issuers(scope, requested_at, None, 10)?,
        "the issuers of the operations",
    )
    .await?;
    same(
        "the issuers of the operations",
        &issuers(&seen),
        &vec!["iss-a".to_owned(), "iss-b".to_owned()],
    )?;
    let later = read(
        deletion_issuers(scope, requested_at, Some("iss-a"), 1)?,
        "the issuers after the first",
    )
    .await?;
    same(
        "the issuers after the first",
        &issuers(&later),
        &vec!["iss-b".to_owned()],
    )?;
    // A full chunk of hashes, as many as one delete carries.
    let mut hashes = vec![ada_a, ada_b];
    hashes.extend(
        (0..TRACE_ID_LIST_CHUNK as u128 - 2).map(|n| ((0xcd_u128 << 120) | n).to_be_bytes()),
    );
    let of_ada = read(
        deletion_traces(
            scope,
            TraceSelector::McpUser("ada", &hashes),
            requested_at,
            None,
            1_000,
        )?,
        "the traces of a user's operations",
    )
    .await?;
    same(
        "the traces of a user's operations",
        &ids(&of_ada),
        &vec![trace(5).to_bytes().to_vec(), trace(6).to_bytes().to_vec()],
    )?;
    let by_identity = Pred::And(vec![
        mcp_user(ops, "ada", &hashes)?,
        Pred::cmp(McpOpsCol::ReceivedAt, CmpOp::Lt, requested_at),
    ]);
    let delete = DeletePlan::project(scope, ops, ops_partition, by_identity.clone())?;
    for store in both {
        let id = store
            .delete(delete.clone())
            .await
            .context("the delete of a user's operations")?;
        finished(ctx, store, &id).await?;
    }
    let left = read(
        lifecycle_oldest(scope, ops, every, by_identity)?,
        "the verification of the delete of a user's operations",
    )
    .await?;
    expect(left.is_empty(), || {
        format!("the delete of a user's operations left {left:?}")
    })?;
    for (n, what) in [
        (5, "the operation of ada that arrived after the request"),
        (7, "the operations of bob"),
    ] {
        let kept = read(
            lifecycle_oldest(
                scope,
                ops,
                every,
                Pred::In(
                    McpOpsCol::TraceId.column_ref(),
                    vec![Literal::from(trace(n))],
                ),
            )?,
            what,
        )
        .await?;
        expect(!kept.is_empty(), || format!("the delete removed {what}"))?;
    }

    Ok(format!(
        "{reads} reads agree between the model and the server through the resolution of a request's traces, \
         reads under the pending-deletion set, the delete of a request in a partition with its verification, \
         the mask application, the retention delete in a partition that straddles the cut-off, the \
         partition drop, and a deletion by user on the MCP operations by `user_id` and a full chunk of \
         subject hashes, with the issuers it hashes under"
    ))
}
