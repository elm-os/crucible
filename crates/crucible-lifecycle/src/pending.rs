//! What a node keeps in memory between its 5 s refreshes: the pending-deletion set (contract 13.3 steps 1 and
//! 2, and 13.4) and the retention settings of every project with their floors (contract 13.1).
//!
//! Both are snapshots of Postgres that the accepting node also updates at once, before it answers the request
//! that changed them, so its own reads follow the change immediately and every other node's within one refresh.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use crucible_control::{ControlStore, DeletionRequest, DeletionSelector, TxError};
use crucible_plan::{Hidden, HiddenSelector, RetentionFloors};
use crucible_records::{DeletionRequestId, DeletionState, ProjectId, Timestamp, TraceId};
use crucible_schema::RetentionSettings;

use crate::retention::{floors, settings};

/// One deletion request that reads must already honour: its deletes have not been issued yet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PendingDeletion {
    /// The request.
    pub request_id: DeletionRequestId,
    /// The project.
    pub project_id: ProjectId,
    /// What is deleted.
    pub selector: DeletionSelector,
    /// Data received at or after this instant is kept.
    pub requested_at: Timestamp,
}

impl PendingDeletion {
    /// Whether the deletion hides a row of its project with these fields.
    pub fn hides(
        &self,
        trace_id: TraceId,
        user_id: &str,
        session_id: &str,
        received_at: Timestamp,
    ) -> bool {
        received_at < self.requested_at
            && match &self.selector {
                DeletionSelector::Traces(ids) => ids.contains(&trace_id),
                DeletionSelector::User { user_id: user, .. } => !user.is_empty() && user == user_id,
                DeletionSelector::Session(session) => !session.is_empty() && session == session_id,
                DeletionSelector::Project => true,
            }
    }
}

/// How long a request stays in the pending-deletion set after its deletes have finished: the longest
/// visibility grace of the recent-span index (10 s), with room for a node clock that is minutes off the
/// leader's.
pub const KEEP_AFTER_APPLIED: std::time::Duration = std::time::Duration::from_secs(600);
const KEEP_AFTER_APPLIED_MICROS: i64 = 600_000_000;

/// The pending-deletion set of one node.
#[derive(Debug, Default)]
pub struct PendingDeletions {
    requests: RwLock<Arc<BTreeMap<DeletionRequestId, PendingDeletion>>>,
}

fn pending(request: &DeletionRequest) -> PendingDeletion {
    PendingDeletion {
        request_id: request.id,
        project_id: request.project_id,
        selector: request.selector.clone(),
        requested_at: request.requested_at,
    }
}

impl PendingDeletions {
    fn snapshot(&self) -> Arc<BTreeMap<DeletionRequestId, PendingDeletion>> {
        Arc::clone(&self.requests.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Adds a request this node has just accepted, before the request is answered.
    pub fn add(&self, request: &DeletionRequest) {
        let mut requests = self
            .requests
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut next = (**requests).clone();
        next.insert(request.id, pending(request));
        *requests = Arc::new(next);
    }

    /// Replaces the set with the open requests of Postgres, read at `now` on this node's clock. A request
    /// stays in the set until its deletes have finished (`applied`), when the store itself hides its rows,
    /// and for [`KEEP_AFTER_APPLIED`] more: the recent-span index can still hold its spans for the
    /// visibility grace after their checkpoint.
    pub fn replace(&self, open: &[DeletionRequest], now: Timestamp) {
        let lingers = |request: &DeletionRequest| {
            request.state == DeletionState::Applied
                && request
                    .updated_at
                    .saturating_add_micros(KEEP_AFTER_APPLIED_MICROS)
                    > now
        };
        let next = open
            .iter()
            .filter(|r| {
                matches!(r.state, DeletionState::Pending | DeletionState::Logged) || lingers(r)
            })
            .map(|r| (r.id, pending(r)))
            .collect();
        *self
            .requests
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
    }

    /// The pending deletions of a project, ordered by request id.
    pub fn of(&self, project_id: ProjectId) -> Vec<PendingDeletion> {
        self.snapshot()
            .values()
            .filter(|pending| pending.project_id == project_id)
            .cloned()
            .collect()
    }

    /// The pending deletions of a project as the plans of its reads carry them (contract 13.4). A project
    /// deletion carries nothing: a deleted project's reads are refused before a plan is built.
    pub fn hidden(&self, project_id: ProjectId) -> Vec<Hidden> {
        self.snapshot()
            .values()
            .filter(|pending| pending.project_id == project_id)
            .filter_map(|pending| {
                let selector = match &pending.selector {
                    DeletionSelector::Traces(ids) => HiddenSelector::Traces(ids.clone()),
                    DeletionSelector::User { user_id, .. } => HiddenSelector::User(user_id.clone()),
                    DeletionSelector::Session(session_id) => {
                        HiddenSelector::Session(session_id.clone())
                    }
                    DeletionSelector::Project => return None,
                };
                Some(Hidden {
                    selector,
                    requested_at: pending.requested_at,
                })
            })
            .collect()
    }

    /// How many requests are pending on this node.
    pub fn len(&self) -> usize {
        self.snapshot().len()
    }

    /// Whether no request is pending.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The retention of every project as this node last read it.
#[derive(Debug, Default)]
pub struct RetentionSnapshot {
    projects: RwLock<BTreeMap<ProjectId, (RetentionSettings, RetentionFloors)>>,
}

impl RetentionSnapshot {
    /// The settings and floors of a project; a project the node has not read keeps everything.
    pub fn get(&self, project_id: ProjectId) -> (RetentionSettings, RetentionFloors) {
        self.projects
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&project_id)
            .copied()
            .unwrap_or_default()
    }

    /// Sets one project's retention, on the node that has just changed it.
    pub fn set(&self, project_id: ProjectId, retention: RetentionSettings, floor: RetentionFloors) {
        self.projects
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(project_id, (retention, floor));
    }

    /// Reads every project's retention from Postgres.
    pub async fn refresh(&self, control: &dyn ControlStore) -> Result<(), TxError> {
        let projects = control.projects().list().await?;
        let stored: BTreeMap<ProjectId, _> =
            control.lifecycle().floors().await?.into_iter().collect();
        let next = projects
            .iter()
            .map(|project| {
                let floor = stored.get(&project.id).copied().unwrap_or_default();
                (project.id, (settings(project), floors(floor)))
            })
            .collect();
        *self
            .projects
            .write()
            .unwrap_or_else(PoisonError::into_inner) = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(n: u8, selector: DeletionSelector, state: DeletionState) -> DeletionRequest {
        DeletionRequest {
            id: DeletionRequestId::from_bytes([n; 16]),
            project_id: ProjectId::from_bytes([1; 16]),
            selector,
            state,
            requested_at: Timestamp::from_micros(100),
            deletion_seq: u64::from(n),
            requested_by: "user:x".to_owned(),
            idempotency_key: None,
            request_hash: String::new(),
            places: "{}".to_owned(),
            attempts: 0,
            error: None,
            updated_at: Timestamp::EPOCH,
        }
    }

    #[test]
    fn a_pending_deletion_hides_what_it_selects_and_only_what_arrived_before_it() {
        let trace = TraceId::from_bytes([7; 16]);
        let before = Timestamp::from_micros(99);
        let at = Timestamp::from_micros(100);
        let traces = pending(&request(
            1,
            DeletionSelector::Traces(vec![trace]),
            DeletionState::Pending,
        ));
        assert!(traces.hides(trace, "", "", before));
        assert!(!traces.hides(trace, "", "", at));
        assert!(!traces.hides(TraceId::from_bytes([8; 16]), "", "", before));
        let user = DeletionSelector::User {
            user_id: "ada".to_owned(),
            issuer: None,
        };
        let user = pending(&request(2, user, DeletionState::Pending));
        assert!(user.hides(trace, "ada", "s", before));
        assert!(!user.hides(trace, "bob", "ada", before));
        let session = pending(&request(
            3,
            DeletionSelector::Session("s".to_owned()),
            DeletionState::Logged,
        ));
        assert!(session.hides(trace, "ada", "s", before));
        assert!(!session.hides(trace, "s", "", before));
    }

    /// A project deletion hides every row of the project received before it. Reads of a deleted project are
    /// refused before any plan is built, so it adds nothing to a plan's hidden set.
    #[test]
    fn a_project_deletion_hides_the_whole_project_and_adds_nothing_to_plans() {
        let project = pending(&request(
            4,
            DeletionSelector::Project,
            DeletionState::Pending,
        ));
        let trace = TraceId::from_bytes([7; 16]);
        assert!(project.hides(trace, "", "", Timestamp::from_micros(99)));
        assert!(project.hides(trace, "ada", "s", Timestamp::from_micros(99)));
        assert!(!project.hides(trace, "ada", "s", Timestamp::from_micros(100)));
        let set = PendingDeletions::default();
        set.add(&request(
            4,
            DeletionSelector::Project,
            DeletionState::Pending,
        ));
        set.add(&request(
            5,
            DeletionSelector::Session("s".to_owned()),
            DeletionState::Pending,
        ));
        let id = ProjectId::from_bytes([1; 16]);
        assert_eq!(set.of(id).len(), 2);
        assert_eq!(set.hidden(id).len(), 1);
    }

    #[test]
    fn the_set_holds_a_request_until_its_deletes_have_finished() {
        let set = PendingDeletions::default();
        let project = ProjectId::from_bytes([1; 16]);
        let mine = request(
            1,
            DeletionSelector::Session("s".to_owned()),
            DeletionState::Pending,
        );
        set.add(&mine);
        assert_eq!(set.of(project).len(), 1);
        assert!(set.of(ProjectId::from_bytes([2; 16])).is_empty());
        let logged = request(
            2,
            DeletionSelector::Session("t".to_owned()),
            DeletionState::Logged,
        );
        let applied = request(
            3,
            DeletionSelector::Session("u".to_owned()),
            DeletionState::Applied,
        );
        // The applied request last changed at the epoch: it lingers for ten minutes, then leaves the set.
        let open = [mine.clone(), logged, applied];
        set.replace(&open, Timestamp::from_micros(599_999_999));
        assert_eq!(set.len(), 3);
        set.replace(&open, Timestamp::from_micros(600_000_000));
        assert_eq!(set.len(), 2);
        set.replace(&[], Timestamp::EPOCH);
        assert!(set.is_empty());
    }

    #[test]
    fn an_unread_project_keeps_everything() {
        let snapshot = RetentionSnapshot::default();
        let project = ProjectId::from_bytes([1; 16]);
        assert_eq!(snapshot.get(project), Default::default());
        let retention = RetentionSettings {
            retention_days: Some(30),
            ..RetentionSettings::default()
        };
        snapshot.set(project, retention, RetentionFloors::default());
        assert_eq!(snapshot.get(project).0.retention_days, Some(30));
    }
}
