//! What the API asks of the lifecycle (contract 12.6 and 13): a project's retention settings and its deletion
//! requests.
//!
//! - A retention change is one transaction that compares the project's version, raises the project's floors
//!   to what the old settings had hidden, and writes the new settings with a higher `config_version`. The
//!   accepting node's snapshot follows before the call returns, so its reads hide expired rows at once.
//! - A deletion request is a Postgres row and a `Tombstone` frame in the accepting node's WAL: the call
//!   returns only after the frame is durable (contract 4.5), with the request already in the node's
//!   pending-deletion set (contract 13.3, step 1).

use std::sync::Arc;

use crucible_control::{
    ControlStore, DeletionRequest, DeletionSelector, MAX_DELETION_TRACES, Project, RetentionFloor,
    TxError, transaction,
};
use crucible_env::Env;
use crucible_plan::RetentionFloors;
use crucible_records::{DeletionRequestId, DeletionState, ProjectId, ProjectScope, Timestamp};
use crucible_schema::{RetentionBinding, RetentionSettings, retention_cutoff};

use crate::node::{AckError, LogError, NodeState};
use crate::retention::{floors, settings};

/// The longest retention a project can set, in days.
pub const MAX_RETENTION_DAYS: u32 = 36_500;
/// The longest `user_id`, `issuer` or `session_id` a deletion names, in bytes.
pub const MAX_SELECTOR_BYTES: usize = 512;
/// The most deletion requests one list page holds.
pub const MAX_DELETION_PAGE: u32 = 100;
/// How far ahead of this node's clock another node's may be when it hides rows by retention: the floor of a
/// retention change is taken this far ahead, so that it covers what any node had hidden.
const CLOCK_SKEW_MICROS: i64 = 600_000_000;
/// How often a transaction that lost a write race is run again.
const RETRIES: u32 = 3;

/// A project's retention as the API shows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Retention {
    /// The settings.
    pub settings: RetentionSettings,
    /// What earlier settings had hidden and stays hidden.
    pub floors: RetentionFloors,
    /// The project's version, for `If-Match`.
    pub version: u64,
}

/// A deletion request to create.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewDeletion {
    /// What to delete.
    pub selector: DeletionSelector,
    /// Who asks: `user:<id>` or `key:<id>`.
    pub requested_by: String,
    /// The `Idempotency-Key` and the hash of the request body under it.
    pub idempotency: Option<(String, String)>,
}

/// An accepted deletion request.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Accepted {
    /// The request row.
    pub request: DeletionRequest,
    /// Whether the row was made by an earlier call under the same idempotency key.
    pub replayed: bool,
}

/// Why the lifecycle refused or failed a call.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ServiceError {
    /// A value is not acceptable; `reason` says what to send instead.
    #[error("{field}: {reason}")]
    Invalid {
        /// The member of the request body.
        field: &'static str,
        /// What is wrong and what is accepted.
        reason: String,
    },
    /// The project changed since the caller read it.
    #[error("the project is at version {current}")]
    VersionMismatch {
        /// The project's version now.
        current: u64,
    },
    /// The project does not exist any more.
    #[error("the project does not exist")]
    NotFound,
    /// The idempotency key was used for another request.
    #[error("the idempotency key was used for another request")]
    KeyReused,
    /// The request row exists and its tombstone is not durable on this node yet. The node's timer appends it
    /// within seconds; the caller may also send the request again under the same idempotency key.
    #[error(transparent)]
    Log(#[from] LogError),
    /// The control store failed.
    #[error(transparent)]
    Store(#[from] TxError),
}

/// The lifecycle of one node, for the API.
#[derive(Clone, Debug)]
pub struct Service {
    env: Env,
    control: Arc<dyn ControlStore>,
    node: Arc<NodeState>,
}

fn invalid(field: &'static str, reason: impl Into<String>) -> ServiceError {
    ServiceError::Invalid {
        field,
        reason: reason.into(),
    }
}

/// Checks retention settings (contract 13.1): each is 1 to 36,500 days or unset, and the payload and
/// MCP-content settings are at most `retention_days`.
pub fn check_retention(new: &RetentionSettings) -> Result<(), ServiceError> {
    let fields = [
        ("retention_days", new.retention_days),
        ("payload_retention_days", new.payload_retention_days),
        ("mcp_content_retention_days", new.mcp_content_retention_days),
    ];
    for (field, days) in fields {
        if days.is_some_and(|days| !(1..=MAX_RETENTION_DAYS).contains(&days)) {
            return Err(invalid(
                field,
                format!("send 1 to {MAX_RETENTION_DAYS} days, or null to keep the data forever"),
            ));
        }
    }
    for (field, days) in &fields[1..] {
        if let (Some(days), Some(all)) = (days, new.retention_days) {
            if *days > all {
                return Err(invalid(
                    field,
                    format!(
                        "at most retention_days ({all}); raise retention_days or lower this value"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Checks a deletion selector (contract 13.3) and returns it in canonical form: trace ids sorted and unique.
pub fn check_selector(selector: DeletionSelector) -> Result<DeletionSelector, ServiceError> {
    let text = |field: &'static str, value: &str| {
        if value.is_empty() || value.len() > MAX_SELECTOR_BYTES {
            Err(invalid(
                field,
                format!("send 1 to {MAX_SELECTOR_BYTES} bytes"),
            ))
        } else {
            Ok(())
        }
    };
    match selector {
        DeletionSelector::Traces(mut ids) => {
            ids.sort_unstable();
            ids.dedup();
            if ids.is_empty() || ids.len() > MAX_DELETION_TRACES {
                return Err(invalid(
                    "trace_ids",
                    format!(
                        "send 1 to {MAX_DELETION_TRACES} trace ids; split a longer list into several requests"
                    ),
                ));
            }
            if ids.iter().any(|id| id.is_zero()) {
                return Err(invalid("trace_ids", "the all-zero trace id names no trace"));
            }
            Ok(DeletionSelector::Traces(ids))
        }
        DeletionSelector::User { user_id, issuer } => {
            text("user_id", &user_id)?;
            if let Some(issuer) = &issuer {
                text("issuer", issuer)?;
            }
            Ok(DeletionSelector::User { user_id, issuer })
        }
        DeletionSelector::Session(session_id) => {
            text("session_id", &session_id)?;
            Ok(DeletionSelector::Session(session_id))
        }
    }
}

/// What the settings had hidden at `now`, per setting.
fn hidden(old: &RetentionSettings, now: Timestamp) -> RetentionFloor {
    let cutoff = |binding| {
        old.days(binding)
            .and_then(|days| retention_cutoff(now, days).ok())
            .map(|day| day.start())
    };
    RetentionFloor {
        retention: cutoff(RetentionBinding::RetentionDays),
        payload: cutoff(RetentionBinding::PayloadRetentionDays),
        mcp_content: cutoff(RetentionBinding::McpContentRetentionDays),
    }
}

fn live(project: Option<Project>) -> Result<Project, ServiceError> {
    project
        .filter(|project| project.deleted_at.is_none())
        .ok_or(ServiceError::NotFound)
}

impl Service {
    /// The lifecycle of the node `node` over the control store.
    pub fn new(env: &Env, control: Arc<dyn ControlStore>, node: Arc<NodeState>) -> Self {
        Self {
            env: env.clone(),
            control,
            node,
        }
    }

    /// The node this service runs on.
    pub fn node(&self) -> &Arc<NodeState> {
        &self.node
    }

    async fn floor_of(&self, project_id: ProjectId) -> Result<RetentionFloor, TxError> {
        Ok(self
            .control
            .lifecycle()
            .floors()
            .await?
            .into_iter()
            .find(|(id, _)| *id == project_id)
            .map_or_else(RetentionFloor::default, |(_, floor)| floor))
    }

    /// A project's retention.
    pub async fn retention(&self, scope: ProjectScope) -> Result<Retention, ServiceError> {
        let project_id = scope.project_id();
        let project = live(self.control.projects().get(project_id).await?)?;
        Ok(Retention {
            settings: settings(&project),
            floors: floors(self.floor_of(project_id).await?),
            version: project.version,
        })
    }

    /// Replaces a project's retention settings if the project is at version `expected`. Shortening applies to
    /// existing data at once; lengthening never shows what was already hidden (contract 13.1).
    pub async fn set_retention(
        &self,
        scope: ProjectScope,
        expected: u64,
        new: RetentionSettings,
    ) -> Result<Retention, ServiceError> {
        check_retention(&new)?;
        let project_id = scope.project_id();
        let now = self
            .env
            .clock
            .wall_now()
            .saturating_add_micros(CLOCK_SKEW_MICROS);
        let mut attempt = 0;
        let written = loop {
            attempt += 1;
            let result = transaction(self.control.as_ref(), |tx| {
                Box::pin(async move {
                    let project = match live(tx.projects().get(project_id).await?) {
                        Ok(project) => project,
                        Err(refusal) => return Ok(Err(refusal)),
                    };
                    if project.version != expected {
                        return Ok(Err(ServiceError::VersionMismatch {
                            current: project.version,
                        }));
                    }
                    tx.lifecycle()
                        .raise_floor(project_id, hidden(&settings(&project), now))
                        .await?;
                    let next = Project {
                        retention_days: new.retention_days,
                        payload_retention_days: new.payload_retention_days,
                        mcp_content_retention_days: new.mcp_content_retention_days,
                        config_version: project.config_version + 1,
                        ..project
                    };
                    if !tx.projects().update_if_version(&next, expected).await? {
                        return Ok(Err(ServiceError::VersionMismatch { current: expected }));
                    }
                    Ok(Ok(()))
                })
            })
            .await;
            match result {
                Err(TxError::SerializationFailure) if attempt < RETRIES => {}
                other => break other,
            }
        };
        match written {
            Ok(Ok(())) => {}
            Ok(Err(refusal)) => return Err(refusal),
            // The commit may have been applied: what is stored decides.
            Err(TxError::CommitUnknown) => {
                let now = self.retention(scope).await?;
                if now.settings != new || now.version != expected + 1 {
                    return Err(ServiceError::Store(TxError::CommitUnknown));
                }
            }
            Err(error) => return Err(error.into()),
        }
        let floor = floors(self.floor_of(project_id).await?);
        self.node.retention().set(project_id, new, floor);
        Ok(Retention {
            settings: new,
            floors: floor,
            version: expected + 1,
        })
    }

    /// Records this node's tombstone for a request that reads must already honour.
    async fn log(&self, request: &DeletionRequest) -> Result<(), ServiceError> {
        self.node.pending().add(request);
        match self.node.acknowledge(self.control.as_ref(), request).await {
            // The tombstone is durable; the node's timer records the acknowledgement that Postgres did not
            // take.
            Ok(()) | Err(AckError::Store(_)) => Ok(()),
            Err(AckError::Log(error)) => Err(error.into()),
        }
    }

    async fn replay(
        &self,
        existing: DeletionRequest,
        hash: &str,
    ) -> Result<Accepted, ServiceError> {
        if existing.request_hash != hash {
            return Err(ServiceError::KeyReused);
        }
        if matches!(
            existing.state,
            DeletionState::Pending | DeletionState::Logged
        ) {
            let acks = self.control.lifecycle().acks(existing.id).await?;
            if !acks.iter().any(|ack| ack.wal_epoch == self.node.epoch()) {
                self.log(&existing).await?;
            }
        }
        Ok(Accepted {
            request: existing,
            replayed: true,
        })
    }

    /// Creates a deletion request (contract 13.3, step 1) and makes its tombstone durable on this node.
    pub async fn request_deletion(
        &self,
        scope: ProjectScope,
        new: NewDeletion,
    ) -> Result<Accepted, ServiceError> {
        let project_id = scope.project_id();
        let selector = check_selector(new.selector)?;
        let repo = self.control.lifecycle();
        let (key, hash) = match &new.idempotency {
            Some((key, hash)) => (Some(key.as_str()), hash.as_str()),
            None => (None, ""),
        };
        if let Some(key) = key {
            if let Some(existing) = repo
                .request_by_key(project_id, &new.requested_by, key)
                .await?
            {
                return self.replay(existing, hash).await;
            }
        }
        let now = self.env.clock.wall_now();
        let mut request = DeletionRequest {
            id: DeletionRequestId::from_uuid(self.env.new_id()),
            project_id,
            selector,
            state: DeletionState::Pending,
            requested_at: now,
            deletion_seq: 0,
            requested_by: new.requested_by.clone(),
            idempotency_key: key.map(str::to_owned),
            request_hash: hash.to_owned(),
            places: "{}".to_owned(),
            attempts: 0,
            error: None,
            updated_at: now,
        };
        match repo.insert_request(&request).await {
            Ok(seq) => request.deletion_seq = seq,
            Err(TxError::UniqueViolation(constraint))
                if constraint == "deletion_requests_idempotency_key" =>
            {
                // Another call under the same key won the race.
                let existing = match key {
                    Some(key) => {
                        repo.request_by_key(project_id, &new.requested_by, key)
                            .await?
                    }
                    None => None,
                };
                return match existing {
                    Some(existing) => self.replay(existing, hash).await,
                    None => Err(ServiceError::KeyReused),
                };
            }
            Err(TxError::ForeignKeyViolation(_)) => return Err(ServiceError::NotFound),
            Err(TxError::CommitUnknown) => {
                // The row may be there: what is stored decides.
                match repo.request(project_id, request.id).await? {
                    Some(stored) => request = stored,
                    None => return Err(ServiceError::Store(TxError::CommitUnknown)),
                }
            }
            Err(error) => return Err(error.into()),
        }
        self.log(&request).await?;
        Ok(Accepted {
            request,
            replayed: false,
        })
    }

    /// One deletion request of a project.
    pub async fn deletion(
        &self,
        scope: ProjectScope,
        id: DeletionRequestId,
    ) -> Result<Option<DeletionRequest>, ServiceError> {
        Ok(self
            .control
            .lifecycle()
            .request(scope.project_id(), id)
            .await?)
    }

    /// The deletion requests of a project with a sequence number below `before`, newest first.
    pub async fn deletions(
        &self,
        scope: ProjectScope,
        before: Option<u64>,
        limit: u32,
    ) -> Result<Vec<DeletionRequest>, ServiceError> {
        Ok(self
            .control
            .lifecycle()
            .list_requests(
                scope.project_id(),
                before,
                limit.clamp(1, MAX_DELETION_PAGE),
            )
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::TraceId;

    use super::*;

    fn days(retention: Option<u32>, payload: Option<u32>, mcp: Option<u32>) -> RetentionSettings {
        RetentionSettings {
            retention_days: retention,
            payload_retention_days: payload,
            mcp_content_retention_days: mcp,
        }
    }

    fn field(result: Result<(), ServiceError>) -> Option<&'static str> {
        match result {
            Err(ServiceError::Invalid { field, .. }) => Some(field),
            _ => None,
        }
    }

    #[test]
    fn retention_settings_are_bounded_and_capped_by_retention_days() {
        assert_eq!(check_retention(&days(None, None, None)), Ok(()));
        assert_eq!(check_retention(&days(Some(30), Some(30), Some(1))), Ok(()));
        assert_eq!(check_retention(&days(None, Some(7), Some(7))), Ok(()));
        assert_eq!(
            field(check_retention(&days(Some(0), None, None))),
            Some("retention_days")
        );
        assert_eq!(
            field(check_retention(&days(
                Some(MAX_RETENTION_DAYS + 1),
                None,
                None
            ))),
            Some("retention_days")
        );
        assert_eq!(
            field(check_retention(&days(Some(30), Some(31), None))),
            Some("payload_retention_days")
        );
        assert_eq!(
            field(check_retention(&days(Some(30), None, Some(31)))),
            Some("mcp_content_retention_days")
        );
        let refusal = check_retention(&days(Some(30), Some(31), None)).unwrap_err();
        assert_eq!(
            refusal.to_string(),
            "payload_retention_days: at most retention_days (30); raise retention_days or lower this value"
        );
    }

    #[test]
    fn selectors_are_checked_and_trace_ids_made_canonical() {
        let trace = |n: u8| TraceId::from_bytes([n; 16]);
        assert_eq!(
            check_selector(DeletionSelector::Traces(vec![trace(2), trace(1), trace(2)])),
            Ok(DeletionSelector::Traces(vec![trace(1), trace(2)]))
        );
        assert!(check_selector(DeletionSelector::Traces(Vec::new())).is_err());
        assert!(check_selector(DeletionSelector::Traces(vec![TraceId::ZERO])).is_err());
        let many: Vec<TraceId> = (0..=MAX_DELETION_TRACES as u32)
            .map(|n| {
                let mut bytes = [1; 16];
                bytes[..4].copy_from_slice(&n.to_be_bytes());
                TraceId::from_bytes(bytes)
            })
            .collect();
        assert!(check_selector(DeletionSelector::Traces(many)).is_err());
        let user = |user_id: &str, issuer: Option<&str>| DeletionSelector::User {
            user_id: user_id.to_owned(),
            issuer: issuer.map(str::to_owned),
        };
        assert!(check_selector(user("ada", Some("https://issuer.example"))).is_ok());
        assert!(check_selector(user("", None)).is_err());
        assert!(check_selector(user("ada", Some(""))).is_err());
        assert!(check_selector(DeletionSelector::Session("x".repeat(513))).is_err());
        assert!(check_selector(DeletionSelector::Session("s-1".to_owned())).is_ok());
    }

    #[test]
    fn the_floor_of_a_change_is_what_the_old_settings_had_hidden() {
        let day = 86_400_000_000;
        let now = Timestamp::from_micros(100 * day + 5);
        let floor = hidden(&days(Some(30), Some(7), None), now);
        assert_eq!(floor.retention, Some(Timestamp::from_micros(70 * day)));
        assert_eq!(floor.payload, Some(Timestamp::from_micros(93 * day)));
        // The MCP-content setting was unset, so `retention_days` bound it.
        assert_eq!(floor.mcp_content, Some(Timestamp::from_micros(70 * day)));
        assert_eq!(
            hidden(&days(None, None, None), now),
            RetentionFloor::default()
        );
    }
}
