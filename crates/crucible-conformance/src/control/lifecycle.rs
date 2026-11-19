//! The statements of retention and deletion (contracts 11.1, 11.7 and 13): the node registry, deletion
//! requests with their idempotency key and their per-node acknowledgements, the ledger of leader actions, and
//! the retention floors.

use std::time::Duration;

use anyhow::Result;
use crucible_control::{
    ActionKind, ActionState, DeletionRequest, DeletionSelector, LifecycleAction, Node, NodeAck,
    RetentionFloor,
};
use crucible_records::{
    DeletionRequestId, DeletionState, Lsn, PartitionId, ProjectId, TableId, TraceId, WalEpoch,
};
use crucible_schema::{Granularity, route};

use super::{Ctl, Transcript, at, id, project};

const SHORT: Duration = Duration::from_millis(400);
/// Longer than this script runs.
const WIDE: Duration = Duration::from_secs(3_600);

fn epoch(n: u64) -> WalEpoch {
    WalEpoch::from_uuid(id(n))
}

fn node(n: u64, checkpoint: u64) -> Node {
    Node {
        wal_epoch: epoch(n),
        // The store writes its own clock.
        heartbeat_at: at(0),
        version: format!("0.0.{n}"),
        checkpoint_lsn: Lsn::new(checkpoint),
        address: String::new(),
    }
}

fn request(n: u64, project_id: ProjectId, selector: DeletionSelector) -> DeletionRequest {
    DeletionRequest {
        id: DeletionRequestId::from_uuid(id(n)),
        project_id,
        selector,
        state: DeletionState::Pending,
        requested_at: at(i64::try_from(n).unwrap_or(0)),
        deletion_seq: 0,
        requested_by: "user:1".to_owned(),
        idempotency_key: None,
        request_hash: String::new(),
        places: "{}".to_owned(),
        attempts: 0,
        error: None,
        updated_at: at(0),
    }
}

/// A request without its sequence number, which differs between two stores that behave the same, and with
/// its `places` document without spaces: Postgres stores it as `jsonb` and writes it back in its own layout.
fn shape(request: &DeletionRequest) -> (String, DeletionState, String, u32, Option<String>) {
    (
        request.selector.to_text(),
        request.state,
        request.places.replace(' ', ""),
        request.attempts,
        request.error.clone(),
    )
}

fn shapes(requests: &[DeletionRequest]) -> Vec<(String, DeletionState)> {
    requests
        .iter()
        .map(|request| (request.selector.to_text(), request.state))
        .collect()
}

fn nodes(nodes: &[Node]) -> Vec<(String, u64)> {
    nodes
        .iter()
        .map(|node| (node.version.clone(), node.checkpoint_lsn.get()))
        .collect()
}

fn acks(acks: &[NodeAck]) -> Vec<(u64, bool)> {
    acks.iter()
        .map(|ack| (ack.tombstone_lsn.get(), ack.passed_at.is_some()))
        .collect()
}

fn partition() -> Result<PartitionId> {
    Ok(route(TableId::SpanCore, Granularity::Day, at(0))?)
}

fn action(
    n: u64,
    key: &str,
    kind: ActionKind,
    request: Option<DeletionRequestId>,
) -> Result<LifecycleAction> {
    Ok(LifecycleAction {
        id: id(n),
        key: key.to_owned(),
        kind,
        table: TableId::SpanCore,
        partition: partition()?,
        project_id: None,
        request_id: request,
        detail: "{}".to_owned(),
        state: ActionState::Planned,
        fencing_token: 0,
        mutation: None,
        attempts: 0,
        created_at: at(i64::try_from(n).unwrap_or(0)),
        updated_at: at(i64::try_from(n).unwrap_or(0)),
    })
}

fn actions(actions: &[LifecycleAction]) -> Vec<(String, ActionState, u64, Option<String>, u32)> {
    actions
        .iter()
        .map(|a| {
            (
                a.key.clone(),
                a.state,
                a.fencing_token,
                a.mutation.clone(),
                a.attempts,
            )
        })
        .collect()
}

async fn registry(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let repo = ctl.store.lifecycle();
    t.outcome(
        "no node yet",
        repo.live_nodes(WIDE).await.map(|n| nodes(&n)),
    );
    repo.heartbeat(&node(2, 7)).await?;
    repo.heartbeat(&node(1, 3)).await?;
    t.outcome(
        "two nodes, by epoch",
        repo.live_nodes(WIDE).await.map(|n| nodes(&n)),
    );
    ctl.sleep(SHORT * 2).await;
    // A heartbeat replaces the row of its node and takes the database clock.
    repo.heartbeat(&node(1, 9)).await?;
    t.outcome(
        "only the node that heartbeated within the window is live",
        repo.live_nodes(SHORT).await.map(|n| nodes(&n)),
    );
    t.outcome(
        "both within a wide window",
        repo.live_nodes(WIDE).await.map(|n| nodes(&n)),
    );
    Ok(())
}

async fn requests(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let repo = s.lifecycle();
    let (web, api) = (project(30), project(31));
    s.projects().insert(&web).await?;
    s.projects().insert(&api).await?;
    let traces = DeletionSelector::Traces(vec![TraceId::from_bytes([7; 16])]);
    let user = DeletionSelector::User {
        user_id: "ada".to_owned(),
        issuer: Some("https://issuer.example".to_owned()),
    };
    let session = DeletionSelector::Session("s-1".to_owned());
    let mut first = request(1, web.id, traces);
    first.idempotency_key = Some("key-1".to_owned());
    first.request_hash = "aa".to_owned();
    let one = repo.insert_request(&first).await?;
    let two = repo.insert_request(&request(2, web.id, user)).await?;
    let three = repo
        .insert_request(&request(3, api.id, session.clone()))
        .await?;
    t.note("sequence numbers rise by one", (two - one, three - two));

    // The idempotency key is unique per project and requester.
    let mut again = request(4, web.id, session.clone());
    again.idempotency_key = Some("key-1".to_owned());
    t.outcome(
        "the same key again",
        repo.insert_request(&again).await.map(|_| ()),
    );
    again.requested_by = "key:9".to_owned();
    t.outcome(
        "the same key of another requester",
        repo.insert_request(&again).await.map(|_| ()),
    );
    let mut other = request(5, api.id, session);
    other.idempotency_key = Some("key-1".to_owned());
    t.outcome(
        "the same key in another project",
        repo.insert_request(&other).await.map(|_| ()),
    );
    let nowhere = request(
        6,
        ProjectId::from_uuid(id(99)),
        DeletionSelector::Session("x".to_owned()),
    );
    t.outcome(
        "a request of a project that does not exist",
        repo.insert_request(&nowhere).await.map(|_| ()),
    );

    t.outcome(
        "get",
        repo.request(web.id, first.id)
            .await
            .map(|r| r.as_ref().map(shape)),
    );
    t.outcome(
        "get through another project",
        repo.request(api.id, first.id)
            .await
            .map(|r| r.as_ref().map(shape)),
    );
    t.outcome(
        "by key",
        repo.request_by_key(web.id, "user:1", "key-1")
            .await
            .map(|r| r.as_ref().map(shape)),
    );
    t.outcome(
        "by a key that was never used",
        repo.request_by_key(web.id, "user:1", "key-2")
            .await
            .map(|r| r.as_ref().map(shape)),
    );
    t.outcome(
        "list, newest first",
        repo.list_requests(web.id, None, 10)
            .await
            .map(|r| shapes(&r)),
    );
    t.outcome(
        "list one",
        repo.list_requests(web.id, None, 1)
            .await
            .map(|r| shapes(&r)),
    );
    t.outcome(
        "list below the newest",
        repo.list_requests(web.id, Some(two + 2), 10)
            .await
            .map(|r| shapes(&r)),
    );
    t.outcome(
        "list below the second",
        repo.list_requests(web.id, Some(two), 10)
            .await
            .map(|r| shapes(&r)),
    );
    t.outcome(
        "open, by sequence",
        repo.open_requests().await.map(|r| shapes(&r)),
    );

    let mut stored = DeletionRequest {
        deletion_seq: one,
        ..first.clone()
    };
    stored.state = DeletionState::Completed;
    stored.places = "{\"planned\":true}".to_owned();
    stored.attempts = 2;
    stored.error = Some("why".to_owned());
    stored.updated_at = at(50);
    t.outcome("update", repo.update_request(&stored).await);
    t.outcome(
        "updated",
        repo.request(web.id, first.id)
            .await
            .map(|r| r.as_ref().map(shape)),
    );
    let missing = request(77, web.id, DeletionSelector::Session("x".to_owned()));
    t.outcome(
        "update a request that does not exist",
        repo.update_request(&missing).await,
    );
    t.outcome(
        "a completed request is no longer open",
        repo.open_requests().await.map(|r| shapes(&r)),
    );

    // Acknowledgements.
    let second = DeletionRequestId::from_uuid(id(2));
    let ack = |n: u64, lsn: u64| NodeAck {
        request_id: second,
        wal_epoch: epoch(n),
        tombstone_lsn: Lsn::new(lsn),
        passed_at: None,
    };
    t.outcome("record", repo.record_ack(&ack(2, 40)).await);
    t.outcome("record of another node", repo.record_ack(&ack(1, 12)).await);
    t.outcome(
        "record again keeps the first",
        repo.record_ack(&ack(2, 99)).await,
    );
    t.outcome("acks, by epoch", repo.acks(second).await.map(|a| acks(&a)));
    t.outcome("passed", repo.mark_passed(second, epoch(1), at(60)).await);
    t.outcome(
        "passed again",
        repo.mark_passed(second, epoch(1), at(61)).await,
    );
    t.outcome(
        "passed without a record",
        repo.mark_passed(second, epoch(3), at(62)).await,
    );
    t.outcome("acks after", repo.acks(second).await.map(|a| acks(&a)));
    t.outcome(
        "the acks of a node",
        repo.node_acks(epoch(2)).await.map(|a| acks(&a)),
    );
    let orphan = NodeAck {
        request_id: DeletionRequestId::from_uuid(id(98)),
        ..ack(1, 1)
    };
    t.outcome(
        "an ack of a request that does not exist",
        repo.record_ack(&orphan).await,
    );
    Ok(())
}

async fn ledger(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let repo = s.lifecycle();
    let request_id = DeletionRequestId::from_uuid(id(2));
    let delete = action(
        10,
        "deletion:2:page0:span_core",
        ActionKind::Delete,
        Some(request_id),
    )?;
    let drop = action(11, "drop:span_core:1", ActionKind::Drop, None)?;
    let mask = action(12, "mask:span_core:1", ActionKind::Mask, None)?;
    for row in [&mask, &delete, &drop] {
        t.outcome("plan", repo.plan_action(row).await);
    }
    let mut twin = action(13, &delete.key, ActionKind::Delete, Some(request_id))?;
    twin.detail = "{\"other\":1}".to_owned();
    t.outcome("plan the same key again", repo.plan_action(&twin).await);
    t.outcome(
        "open, by creation",
        repo.open_actions().await.map(|a| actions(&a)),
    );
    t.outcome(
        "of the request",
        repo.request_actions(request_id).await.map(|a| actions(&a)),
    );

    let issued = LifecycleAction {
        state: ActionState::Issued,
        fencing_token: 4,
        mutation: Some("mutation_3.txt".to_owned()),
        attempts: 1,
        updated_at: at(70),
        ..delete.clone()
    };
    t.outcome("update", repo.update_action(&issued).await);
    let missing = action(55, "nothing", ActionKind::Drop, None)?;
    t.outcome(
        "update an action that does not exist",
        repo.update_action(&missing).await,
    );
    for (row, at_secs) in [(&drop, 80), (&mask, 5_000)] {
        let verified = LifecycleAction {
            state: ActionState::Verified,
            updated_at: at(at_secs),
            ..row.clone()
        };
        repo.update_action(&verified).await?;
    }
    let done = LifecycleAction {
        state: ActionState::Verified,
        mutation: None,
        updated_at: at(90),
        ..issued
    };
    repo.update_action(&done).await?;
    t.outcome(
        "nothing is open",
        repo.open_actions().await.map(|a| actions(&a)),
    );
    // Only verified actions without a request that last changed before the instant go.
    t.outcome("prune", repo.prune_actions(at(1_000)).await);
    t.outcome("prune again", repo.prune_actions(at(1_000)).await);
    t.outcome("prune the later one", repo.prune_actions(at(9_000)).await);
    t.outcome(
        "the action of the request stays",
        repo.request_actions(request_id).await.map(|a| actions(&a)),
    );

    // Planning is part of its transaction.
    let late = action(14, "drop:span_core:2", ActionKind::Drop, None)?;
    let tx = s.begin().await?;
    t.outcome(
        "plan inside a transaction",
        tx.lifecycle().plan_action(&late).await,
    );
    tx.rollback().await;
    t.outcome(
        "open after the rollback",
        repo.open_actions().await.map(|a| actions(&a)),
    );
    Ok(())
}

async fn floors(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let repo = ctl.store.lifecycle();
    let web = ProjectId::from_uuid(id(30));
    t.outcome("no floor yet", repo.floors().await);
    let floor = |retention: Option<i64>, payload: Option<i64>| RetentionFloor {
        retention: retention.map(at),
        payload: payload.map(at),
        mcp_content: None,
    };
    t.outcome("raise", repo.raise_floor(web, floor(Some(100), None)).await);
    t.outcome(
        "raise lower and another setting",
        repo.raise_floor(web, floor(Some(50), Some(70))).await,
    );
    t.outcome(
        "raise higher",
        repo.raise_floor(web, floor(Some(200), Some(60))).await,
    );
    t.outcome(
        "raise nothing",
        repo.raise_floor(web, RetentionFloor::default()).await,
    );
    t.outcome(
        "the floor keeps the later instant of each setting",
        repo.floors().await,
    );
    t.outcome(
        "a floor of a project that does not exist",
        repo.raise_floor(ProjectId::from_uuid(id(97)), floor(Some(1), None))
            .await,
    );
    Ok(())
}

/// The lifecycle script.
pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    registry(ctl, t).await?;
    requests(ctl, t).await?;
    ledger(ctl, t).await?;
    floors(ctl, t).await
}
