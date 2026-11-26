//! The statements of retention and deletion (contracts 11.1, 11.7 and 13): the node registry, deletion
//! requests with their idempotency key and their per-node acknowledgements, the ledger of leader actions, and
//! the retention floors.

use std::time::Duration;

use anyhow::Result;
use crucible_control::{
    ActionKind, ActionState, AlertRule, AlertSeriesState, AlertStatus, Channel, ChannelKind,
    DeletionRequest, DeletionSelector, ExportFormat, JsonDoc, LifecycleAction, NewExport, Node,
    NodeAck, ProjectMembership, ProjectRole, RetentionFloor, SavedQuery, SealedProjectSalt, Secret,
    SqlActor, SqlRun, SqlRunKind, SqlVisibility,
};
use crucible_records::{
    AlertRuleId, ChannelId, DeletionRequestId, DeletionState, ExportJobId, Lsn, PartitionId,
    ProjectId, SavedQueryId, SqlRunId, TableId, Timestamp, TraceId, UserId, WalEpoch,
};
use crucible_schema::{Granularity, route};

use super::{Ctl, Transcript, api_key, at, id, project, user};

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
        // The node's clock, which the store keeps as given.
        node_clock: Some(at(i64::try_from(checkpoint).unwrap_or(0))),
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

fn nodes(nodes: &[Node]) -> Vec<(String, u64, Option<Timestamp>)> {
    nodes
        .iter()
        .map(|node| {
            (
                node.version.clone(),
                node.checkpoint_lsn.get(),
                node.node_clock,
            )
        })
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
    t.outcome(
        "every project's, by sequence",
        repo.requests_after(at(0), 0, 10).await.map(|r| shapes(&r)),
    );
    t.outcome(
        "every project's, one page",
        repo.requests_after(at(0), 0, 1).await.map(|r| shapes(&r)),
    );
    t.outcome(
        "every project's after the first",
        repo.requests_after(at(0), one, 10)
            .await
            .map(|r| shapes(&r)),
    );
    // Only the requests made after an instant: one made at that instant selects nothing received then.
    t.outcome(
        "every project's requested after the second",
        repo.requests_after(at(2), 0, 10).await.map(|r| shapes(&r)),
    );
    t.outcome(
        "every project's requested after the second, after the third",
        repo.requests_after(at(2), three, 10)
            .await
            .map(|r| shapes(&r)),
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
    t.outcome(
        "a completed request is still among every project's",
        repo.requests_after(at(0), 0, 10).await.map(|r| shapes(&r)),
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
    // A project deletion's purge is planned as a delete and may be issued as a drop.
    let as_drop = LifecycleAction {
        kind: ActionKind::Drop,
        ..issued.clone()
    };
    t.outcome("update the kind", repo.update_action(&as_drop).await);
    t.outcome(
        "the kind is stored",
        repo.request_actions(request_id)
            .await
            .map(|a| a.iter().map(|a| a.kind).collect::<Vec<_>>()),
    );
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

/// One row of each control-plane table that holds rows of a project, with ids from `n`: a membership, an API key,
/// a channel with its sealed secret and a second secret of the project that no channel names, an alert rule with
/// the state of a series, a saved query, a SQL run, an export, a retention floor and the salt.
async fn seed_project_rows(
    s: &dyn crucible_control::ControlStore,
    project: ProjectId,
    member: UserId,
    n: u64,
) -> Result<()> {
    s.memberships()
        .put_project(ProjectMembership {
            project_id: project,
            user_id: member,
            role: ProjectRole::Viewer,
        })
        .await?;
    s.api_keys().insert(&api_key(n, project)).await?;
    for k in [1, 2] {
        s.secrets()
            .insert(&Secret {
                id: id(n + k),
                scope: project.to_string(),
                kind: "channel".to_owned(),
                key_id: "master-0".to_owned(),
                nonce: vec![1; 24],
                ciphertext: vec![2; 32],
            })
            .await?;
    }
    s.channels()
        .insert(&Channel {
            id: ChannelId::from_uuid(id(n + 3)),
            project_id: project,
            name: "On call".to_owned(),
            kind: ChannelKind::Webhook,
            config: JsonDoc::empty(),
            secret_id: Some(id(n + 1)),
            enabled: true,
            disabled_reason: None,
            failures: 0,
            last_test: None,
            version: 1,
        })
        .await?;
    let rule = AlertRuleId::from_uuid(id(n + 4));
    s.alert_rules()
        .insert(&AlertRule {
            id: rule,
            project_id: project,
            name: "errors".to_owned(),
            enabled: true,
            spec: JsonDoc::empty(),
            interval_secs: 60,
            version: 1,
            created_at: at(0),
            updated_at: at(0),
            last_eval_seq: None,
            eval_failures: 0,
            skipped_ticks: 0,
        })
        .await?;
    s.alert_state()
        .put(&AlertSeriesState {
            rule_id: rule,
            series_key: String::new(),
            status: AlertStatus::Firing,
            since: at(0),
            last_rows_at: at(0),
            last_notified_at: None,
            value: Some(9),
        })
        .await?;
    s.saved_queries()
        .insert(&SavedQuery {
            id: SavedQueryId::from_uuid(id(n + 5)),
            project_id: project,
            owner: SqlActor::User(member),
            name: "errors".to_owned(),
            description: String::new(),
            visibility: SqlVisibility::Project,
            sql: "SELECT 1".to_owned(),
            parameters: Vec::new(),
            version: 1,
            created_at: at(0),
            updated_at: at(0),
        })
        .await?;
    s.sql_runs()
        .insert(&SqlRun {
            id: SqlRunId::from_uuid(id(n + 6)),
            project_id: project,
            actor: SqlActor::User(member),
            query_id: id(n + 7),
            kind: SqlRunKind::AdHoc,
            saved_query_id: None,
            sql: "SELECT 1".to_owned(),
            started_at: at(0),
            end: None,
        })
        .await?;
    s.export_jobs()
        .insert(&NewExport {
            id: ExportJobId::from_uuid(id(n + 8)),
            project_id: project,
            format: ExportFormat::CrucibleJsonl,
            selector: "{}".to_owned(),
            options: "{}".to_owned(),
            dedup_key: format!("export-{n}"),
            created_by: "user:1".to_owned(),
            created_at: at(0),
            as_of: at(0),
        })
        .await?;
    s.lifecycle()
        .raise_floor(
            project,
            RetentionFloor {
                retention: Some(at(0)),
                payload: None,
                mcp_content: None,
            },
        )
        .await?;
    s.project_salts()
        .insert_if_absent(&SealedProjectSalt {
            project_id: project,
            key_id: "master-0".to_owned(),
            nonce: vec![3; 24],
            ciphertext: vec![4; 48],
        })
        .await?;
    Ok(())
}

/// What a project seeded by [`seed_project_rows`] from `n` still holds, table by table.
async fn project_rows(
    s: &dyn crucible_control::ControlStore,
    project: ProjectId,
    n: u64,
) -> Result<Vec<(&'static str, usize)>, crucible_control::TxError> {
    let mut secrets = 0;
    for k in [1, 2] {
        secrets += usize::from(s.secrets().get(id(n + k)).await?.is_some());
    }
    let floors = s.lifecycle().floors().await?;
    Ok(vec![
        (
            "memberships",
            s.memberships().list_project(project).await?.len(),
        ),
        ("api_keys", s.api_keys().list(project).await?.len()),
        ("channels", s.channels().list(project).await?.len()),
        ("secrets", secrets),
        ("alert_rules", s.alert_rules().list(project).await?.len()),
        (
            "alert_state",
            s.alert_state()
                .list(AlertRuleId::from_uuid(id(n + 4)))
                .await?
                .len(),
        ),
        (
            "saved_queries",
            s.saved_queries().list(project).await?.len(),
        ),
        (
            "sql_runs",
            s.sql_runs().list(project, None, 10).await?.len(),
        ),
        (
            "export_jobs",
            s.export_jobs().list(project, 10).await?.len(),
        ),
        (
            "retention_floors",
            floors.iter().filter(|(of, _)| *of == project).count(),
        ),
        (
            "project_salts",
            usize::from(s.project_salts().get(project).await?.is_some()),
        ),
    ])
}

/// The purge of a deleted project's control-plane rows (contract 13.4): a live project keeps everything, a
/// deleted one loses every row of each table in one statement, its row and its deletion requests stay, and another
/// project keeps its rows.
async fn purge(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let repo = s.lifecycle();
    let (gone, kept) = (project(60), project(61));
    s.projects().insert(&gone).await?;
    s.projects().insert(&kept).await?;
    let member = user(62)?;
    s.users().insert(&member).await?;
    seed_project_rows(s, gone.id, member.id, 100).await?;
    seed_project_rows(s, kept.id, member.id, 120).await?;
    repo.insert_request(&request(140, gone.id, DeletionSelector::Project))
        .await?;
    t.outcome(
        "a live project keeps everything",
        repo.purge_project(gone.id).await,
    );
    let deleted = crucible_control::Project {
        deleted_at: Some(at(5)),
        version: 2,
        ..gone.clone()
    };
    t.outcome(
        "the project is marked deleted",
        s.projects().update_if_version(&deleted, 1).await,
    );
    t.outcome(
        "a deleted project loses its rows",
        repo.purge_project(gone.id).await,
    );
    t.outcome("again nothing is left", repo.purge_project(gone.id).await);
    t.outcome(
        "what the deleted project holds",
        project_rows(s, gone.id, 100).await,
    );
    t.outcome(
        "what the other project holds",
        project_rows(s, kept.id, 120).await,
    );
    t.outcome(
        "the deleted project's row stays",
        s.projects()
            .get(gone.id)
            .await
            .map(|row| row.map(|row| row.deleted_at.is_some())),
    );
    t.outcome(
        "its deletion request stays",
        repo.list_requests(gone.id, None, 10)
            .await
            .map(|requests| shapes(&requests)),
    );
    t.outcome(
        "a project that does not exist",
        repo.purge_project(ProjectId::from_uuid(id(98))).await,
    );
    Ok(())
}

/// The deleted projects without a project deletion request (contract 13.4): those deleted before deleting a
/// project made one. A live project and a deleted one with its project deletion are not among them; a deletion
/// of another kind does not count. Then every project deletion, which the purge sweep looks at.
async fn earlier(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let repo = s.lifecycle();
    let (bare, traced, live) = (project(71), project(70), project(72));
    for row in [&bare, &traced, &live] {
        s.projects().insert(row).await?;
    }
    for row in [&bare, &traced] {
        let deleted = crucible_control::Project {
            deleted_at: Some(at(7)),
            version: 2,
            ..row.clone()
        };
        s.projects().update_if_version(&deleted, 1).await?;
    }
    repo.insert_request(&request(
        150,
        traced.id,
        DeletionSelector::Traces(vec![TraceId::from_bytes([8; 16])]),
    ))
    .await?;
    t.outcome(
        "deleted without a project deletion, by id",
        repo.deleted_without_request().await,
    );
    repo.insert_request(&request(151, bare.id, DeletionSelector::Project))
        .await?;
    t.outcome(
        "one of them gets its project deletion",
        repo.deleted_without_request().await,
    );
    t.outcome(
        "the project deletions, by sequence",
        repo.project_requests().await.map(|requests| {
            requests
                .iter()
                .map(|request| (request.project_id, request.state))
                .collect::<Vec<_>>()
        }),
    );
    Ok(())
}

/// The lifecycle script.
pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    registry(ctl, t).await?;
    requests(ctl, t).await?;
    ledger(ctl, t).await?;
    floors(ctl, t).await?;
    purge(ctl, t).await?;
    earlier(ctl, t).await
}
