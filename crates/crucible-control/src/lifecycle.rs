//! Retention and deletion in the control plane (contracts 11.1, 11.7 and 13): the node registry, deletion
//! requests with their per-node tombstone acknowledgements, the ledger of leader actions on ClickHouse, and the
//! retention floors that keep lengthened retention from showing rows that were already hidden.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use crucible_records::{
    DeletionKind, DeletionRequestId, DeletionState, Lsn, PartitionId, ProjectId, TableId,
    Timestamp, TraceId, Uuid, WalEpoch,
};
use serde::{Deserialize, Serialize};

use crate::error::TxError;

/// The most trace ids one trace deletion names (contract 13.3).
pub const MAX_DELETION_TRACES: usize = 10_000;

text_enum! {
    /// What a leader action does to one partition of a ClickHouse table (contract 13.2).
    ActionKind {
        /// A lightweight `DELETE ... IN PARTITION` of one project's rows.
        Delete => "delete",
        /// `DROP PARTITION`.
        Drop => "drop",
        /// `APPLY DELETED MASK IN PARTITION`.
        Mask => "mask",
    }
}

text_enum! {
    /// Where a leader action stands. A delete goes `planned`, `issued`, `done`, `masked`, `verified`; a drop
    /// skips `masked`; a mask ends at `verified` once the partition carries no masked part.
    ActionState {
        /// Recorded, not issued yet.
        Planned => "planned",
        /// Recorded as issued under a fencing token; the store may or may not have it.
        Issued => "issued",
        /// The store reports the mutation finished.
        Done => "done",
        /// A mask application ran over the partition after the delete finished.
        Masked => "masked",
        /// The rows are gone: the count is zero and no part of the partition carries a delete mask.
        Verified => "verified",
        /// Given up after its attempts ran out; an operator must look.
        Failed => "failed",
    }
}

/// A node of the deployment (contract 11.1): one WAL directory.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Node {
    /// The node's WAL epoch.
    pub wal_epoch: WalEpoch,
    /// The database clock at its latest heartbeat. Written by the store, not by the node.
    pub heartbeat_at: Timestamp,
    /// The build version.
    pub version: String,
    /// The node's checkpoint watermark at the heartbeat.
    pub checkpoint_lsn: Lsn,
    /// Where peers reach the node; empty when it has no peer address.
    pub address: String,
}

/// What a deletion request removes inside its project (contract 13.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeletionSelector {
    /// These traces, at most [`MAX_DELETION_TRACES`].
    Traces(Vec<TraceId>),
    /// Everything of one user.
    User {
        /// The `user_id` of the spans.
        user_id: String,
        /// The token issuer whose subject hash also names the user on MCP rows.
        issuer: Option<String>,
    },
    /// Everything of one session.
    Session(String),
    /// Everything of the project: the purge that deleting the project starts (contract 13.4).
    Project,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectorDoc {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trace_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

impl DeletionSelector {
    /// The kind of request the selector belongs to.
    pub const fn kind(&self) -> DeletionKind {
        match self {
            Self::Traces(_) => DeletionKind::Trace,
            Self::User { .. } => DeletionKind::User,
            Self::Session(_) => DeletionKind::Session,
            Self::Project => DeletionKind::Project,
        }
    }

    /// The stored JSON document.
    pub fn to_text(&self) -> String {
        let doc = match self {
            Self::Project => SelectorDoc {
                trace_ids: Vec::new(),
                user_id: None,
                issuer: None,
                session_id: None,
            },
            Self::Traces(ids) => SelectorDoc {
                trace_ids: ids.iter().map(ToString::to_string).collect(),
                user_id: None,
                issuer: None,
                session_id: None,
            },
            Self::User { user_id, issuer } => SelectorDoc {
                trace_ids: Vec::new(),
                user_id: Some(user_id.clone()),
                issuer: issuer.clone(),
                session_id: None,
            },
            Self::Session(session_id) => SelectorDoc {
                trace_ids: Vec::new(),
                user_id: None,
                issuer: None,
                session_id: Some(session_id.clone()),
            },
        };
        serde_json::to_string(&doc).unwrap_or_default()
    }

    /// Reads the stored document of a request of `kind`.
    pub fn parse(kind: DeletionKind, text: &str) -> Result<Self, TxError> {
        let invalid = |what: &str| TxError::Invalid(format!("deletion selector: {what}"));
        let doc: SelectorDoc =
            serde_json::from_str(text).map_err(|error| invalid(&error.to_string()))?;
        match kind {
            DeletionKind::Trace => {
                let ids = doc
                    .trace_ids
                    .iter()
                    .map(|id| TraceId::parse(id))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| invalid("a trace id is not 32 hex characters"))?;
                if ids.is_empty() || ids.len() > MAX_DELETION_TRACES {
                    return Err(invalid("a trace deletion names 1 to 10,000 traces"));
                }
                Ok(Self::Traces(ids))
            }
            DeletionKind::User => Ok(Self::User {
                user_id: doc.user_id.ok_or_else(|| invalid("no user_id"))?,
                issuer: doc.issuer,
            }),
            DeletionKind::Session => Ok(Self::Session(
                doc.session_id.ok_or_else(|| invalid("no session_id"))?,
            )),
            DeletionKind::Project => {
                let names = !doc.trace_ids.is_empty()
                    || doc.user_id.is_some()
                    || doc.issuer.is_some()
                    || doc.session_id.is_some();
                if names {
                    return Err(invalid(
                        "a project deletion names nothing inside the project",
                    ));
                }
                Ok(Self::Project)
            }
        }
    }
}

/// A deletion request (contracts 11.1 and 13.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeletionRequest {
    /// The id.
    pub id: DeletionRequestId,
    /// The project whose data is deleted.
    pub project_id: ProjectId,
    /// What is deleted.
    pub selector: DeletionSelector,
    /// Where the request stands.
    pub state: DeletionState,
    /// The point in time of the delete: data received at or after it is kept.
    pub requested_at: Timestamp,
    /// The deployment-wide sequence number. The store assigns it on insert; the value given to
    /// [`LifecycleRepo::insert_request`] is ignored.
    pub deletion_seq: u64,
    /// Who asked: `user:<id>` or `key:<id>`.
    pub requested_by: String,
    /// The `Idempotency-Key` of the request, unique per project and requester.
    pub idempotency_key: Option<String>,
    /// The hash of the request body, hex, which a replay under the same key must match.
    pub request_hash: String,
    /// The per-place status document, written by the executor.
    pub places: String,
    /// How many times a step of the request failed.
    pub attempts: u32,
    /// Why the request failed, for the operator.
    pub error: Option<String>,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

impl DeletionRequest {
    /// The project deletion that deleting a project makes, in the same transaction (contract 13.4): pending, at
    /// the instant the project was marked deleted.
    pub fn project(
        id: DeletionRequestId,
        project_id: ProjectId,
        requested_at: Timestamp,
        requested_by: String,
    ) -> Self {
        Self {
            id,
            project_id,
            selector: DeletionSelector::Project,
            state: DeletionState::Pending,
            requested_at,
            deletion_seq: 0,
            requested_by,
            idempotency_key: None,
            request_hash: String::new(),
            places: "{}".to_owned(),
            attempts: 0,
            error: None,
            updated_at: requested_at,
        }
    }
}

/// How many rows of each table [`LifecycleRepo::purge_project`] removed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PurgedRows {
    /// `project_memberships`.
    pub memberships: u64,
    /// `alert_rules`; their `alert_state` and `alert_events` rows go with them.
    pub alert_rules: u64,
    /// `channels`.
    pub channels: u64,
    /// `secrets`: the sealed secrets of the project's channels.
    pub secrets: u64,
    /// `saved_queries`.
    pub saved_queries: u64,
    /// `sql_runs`.
    pub sql_runs: u64,
    /// `export_jobs`.
    pub export_jobs: u64,
    /// `api_keys`, every one revoked when the project was deleted.
    pub api_keys: u64,
    /// `retention_floors`.
    pub retention_floors: u64,
    /// `project_salts`.
    pub salts: u64,
}

/// One node's acknowledgement of a deletion request (contract 13.3, steps 2 and 4).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeAck {
    /// The request.
    pub request_id: DeletionRequestId,
    /// The node.
    pub wal_epoch: WalEpoch,
    /// The LSN of the node's durable `Tombstone` frame.
    pub tombstone_lsn: Lsn,
    /// When the node's checkpoint watermark reached the tombstone.
    pub passed_at: Option<Timestamp>,
}

/// One leader action on ClickHouse, recorded under the fencing token before it is issued (contract 11.7).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LifecycleAction {
    /// The id.
    pub id: Uuid,
    /// What makes a repeated plan the same action: unique.
    pub key: String,
    /// What the action does.
    pub kind: ActionKind,
    /// The table.
    pub table: TableId,
    /// The partition.
    pub partition: PartitionId,
    /// The project whose rows a delete removes.
    pub project_id: Option<ProjectId>,
    /// The deletion request the action belongs to; `None` for retention and mask actions.
    pub request_id: Option<DeletionRequestId>,
    /// What the executor needs to build the action again: a JSON document.
    pub detail: String,
    /// Where the action stands.
    pub state: ActionState,
    /// The fencing token under which the action was last recorded as issued; 0 before that.
    pub fencing_token: u64,
    /// The store's name of the mutation, once the store has answered.
    pub mutation: Option<String>,
    /// How many times the action was issued.
    pub attempts: u32,
    /// When the action was planned.
    pub created_at: Timestamp,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

/// The cut-offs a project's reads never go below again (contract 13.1: lengthening retention never shows data
/// that was already hidden). One instant per retention setting; `None` when nothing was ever hidden.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RetentionFloor {
    /// The floor of the tables bound to `retention_days`.
    pub retention: Option<Timestamp>,
    /// The floor of the tables bound to `payload_retention_days`.
    pub payload: Option<Timestamp>,
    /// The floor of the tables bound to `mcp_content_retention_days`.
    pub mcp_content: Option<Timestamp>,
}

impl RetentionFloor {
    /// The later of two floors, per setting.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self {
            retention: self.retention.max(other.retention),
            payload: self.payload.max(other.payload),
            mcp_content: self.mcp_content.max(other.mcp_content),
        }
    }
}

/// Retention and deletion rows.
#[async_trait]
pub trait LifecycleRepo: Send + Sync {
    /// Registers the node or refreshes its row: the heartbeat becomes the database clock. `node.heartbeat_at`
    /// is ignored.
    async fn heartbeat(&self, node: &Node) -> Result<(), TxError>;
    /// The nodes whose heartbeat is within `within` of the database clock, ordered by epoch.
    async fn live_nodes(&self, within: Duration) -> Result<Vec<Node>, TxError>;

    /// Creates a request and returns its `deletion_seq`. A taken idempotency key of the same project and
    /// requester is `UniqueViolation("deletion_requests_idempotency_key")`.
    async fn insert_request(&self, request: &DeletionRequest) -> Result<u64, TxError>;
    /// The request of a project with this id.
    async fn request(
        &self,
        project_id: ProjectId,
        id: DeletionRequestId,
    ) -> Result<Option<DeletionRequest>, TxError>;
    /// The request a requester made in a project under an idempotency key.
    async fn request_by_key(
        &self,
        project_id: ProjectId,
        requested_by: &str,
        key: &str,
    ) -> Result<Option<DeletionRequest>, TxError>;
    /// The requests of a project with a sequence number below `before`, newest first, at most `limit`.
    async fn list_requests(
        &self,
        project_id: ProjectId,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<DeletionRequest>, TxError>;
    /// Every request that is neither completed nor failed, ordered by sequence number.
    async fn open_requests(&self) -> Result<Vec<DeletionRequest>, TxError>;
    /// The requests of every project, in every state, requested after `requested_after` and with a sequence number
    /// above `after`, ordered by sequence number, at most `limit`: what a node applies before it ships data received
    /// at or after `requested_after` (contract 13.3), page by page.
    async fn requests_after(
        &self,
        requested_after: Timestamp,
        after: u64,
        limit: u32,
    ) -> Result<Vec<DeletionRequest>, TxError>;
    /// Writes the request's state, places, attempts, error and update time. `false` when there is no such
    /// request.
    async fn update_request(&self, request: &DeletionRequest) -> Result<bool, TxError>;
    /// Deletes, in one statement, the control-plane rows of a deleted project once its telemetry is purged
    /// (contract 13.4): its memberships, alert rules (with their state and events), channels and their sealed
    /// secrets (every `channel` secret scoped to the project), saved queries, SQL runs, export jobs, API keys,
    /// retention floor and salt. The project's row, its deletion requests and their acknowledgements and
    /// actions stay: a node that returns reads those requests before it ships. A project whose row is not marked
    /// deleted keeps everything, and the counts are zero.
    async fn purge_project(&self, project_id: ProjectId) -> Result<PurgedRows, TxError>;

    /// Records a node's tombstone. `false` when the node had already recorded one for the request; the stored
    /// row is not changed.
    async fn record_ack(&self, ack: &NodeAck) -> Result<bool, TxError>;
    /// Marks that the node's checkpoint has passed its tombstone. `false` when there is no such
    /// acknowledgement or it was already marked.
    async fn mark_passed(
        &self,
        request_id: DeletionRequestId,
        wal_epoch: WalEpoch,
        at: Timestamp,
    ) -> Result<bool, TxError>;
    /// The acknowledgements of a request, ordered by epoch.
    async fn acks(&self, request_id: DeletionRequestId) -> Result<Vec<NodeAck>, TxError>;
    /// The acknowledgements of a node, ordered by request id.
    async fn node_acks(&self, wal_epoch: WalEpoch) -> Result<Vec<NodeAck>, TxError>;

    /// Records a planned action. `false` when an action with this key exists; the stored row is not changed.
    async fn plan_action(&self, action: &LifecycleAction) -> Result<bool, TxError>;
    /// Every action that is neither verified nor failed, ordered by creation time then id.
    async fn open_actions(&self) -> Result<Vec<LifecycleAction>, TxError>;
    /// The actions of a deletion request, ordered by creation time then id.
    async fn request_actions(
        &self,
        request_id: DeletionRequestId,
    ) -> Result<Vec<LifecycleAction>, TxError>;
    /// Writes the action's state, fencing token, mutation, attempts and update time. `false` when there is no
    /// such action.
    async fn update_action(&self, action: &LifecycleAction) -> Result<bool, TxError>;
    /// Deletes the verified actions without a request that last changed before `before`; returns how many.
    async fn prune_actions(&self, before: Timestamp) -> Result<u64, TxError>;

    /// The floor of every project that has one, ordered by project id.
    async fn floors(&self) -> Result<Vec<(ProjectId, RetentionFloor)>, TxError>;
    /// Raises a project's floor: each setting keeps the later of the stored and the given instant.
    async fn raise_floor(
        &self,
        project_id: ProjectId,
        floor: RetentionFloor,
    ) -> Result<(), TxError>;
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn selectors_round_trip_and_refuse_what_their_kind_does_not_hold() {
        let traces = DeletionSelector::Traces(vec![TraceId::from_bytes([7; 16])]);
        assert_eq!(
            traces.to_text(),
            r#"{"trace_ids":["07070707070707070707070707070707"]}"#
        );
        let user = DeletionSelector::User {
            user_id: "ada".to_owned(),
            issuer: Some("https://issuer.example".to_owned()),
        };
        let session = DeletionSelector::Session("s-1".to_owned());
        let project = DeletionSelector::Project;
        assert_eq!(project.kind(), DeletionKind::Project);
        assert_eq!(project.to_text(), "{}");
        for selector in [&traces, &user, &session, &project] {
            assert_eq!(
                DeletionSelector::parse(selector.kind(), &selector.to_text()).as_ref(),
                Ok(selector)
            );
        }
        assert!(DeletionSelector::parse(DeletionKind::Trace, &user.to_text()).is_err());
        assert!(DeletionSelector::parse(DeletionKind::User, &session.to_text()).is_err());
        assert!(DeletionSelector::parse(DeletionKind::Session, &traces.to_text()).is_err());
        // A whole project names nothing inside it.
        for named in [&traces, &user, &session] {
            assert!(DeletionSelector::parse(DeletionKind::Project, &named.to_text()).is_err());
        }
        assert!(DeletionSelector::parse(DeletionKind::Trace, &project.to_text()).is_err());
        assert!(DeletionSelector::parse(DeletionKind::Trace, r#"{"trace_ids":["zz"]}"#).is_err());
        assert!(DeletionSelector::parse(DeletionKind::Session, r#"{"other":1}"#).is_err());
    }

    proptest! {
        #[test]
        fn a_raised_floor_never_goes_back(a in 0i64..1 << 50, b in 0i64..1 << 50) {
            let floor = |micros: i64| RetentionFloor {
                retention: Some(Timestamp::from_micros(micros)),
                payload: None,
                mcp_content: Some(Timestamp::from_micros(micros / 2)),
            };
            let raised = floor(a).max(floor(b));
            prop_assert_eq!(raised.retention, Some(Timestamp::from_micros(a.max(b))));
            prop_assert_eq!(raised.payload, None);
            prop_assert_eq!(raised.max(floor(a)), raised);
        }
    }
}
