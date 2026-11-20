//! Retention and deletion on this node (contract 13): the node's state over its WAL, the jobs, and what the
//! API and the read services take from them.
//!
//! The node timer keeps two snapshots of Postgres in memory, refreshed every 5 s: every project's retention
//! with its floors, and the pending-deletion set. Reads are clamped and filtered with them, so a retention
//! change or a deletion request hides its rows at once on the node that accepted it and within one refresh on
//! every other node.
//!
//! The shipper reads the deletion requests through [`ControlDeletions`] before it ships anything, so that stage B
//! drops what a request that completed while this node was down deleted (contract 13.3).

use std::sync::{Arc, OnceLock};

use crucible_api::{ProjectSettings, ProjectSettingsSource};
use crucible_control::{ControlStore, DeletionRequest, DeletionSelector, Node, TxError};
use crucible_env::{Env, TelemetryStore};
use crucible_jobs::Job;
use crucible_lifecycle::{
    LeaderSource, LeaderStores, LifecycleConfig, NodeState, Service, Subjects, WalLog, leader_jobs,
    node_job,
};
use crucible_normalize::{Tombstone, TombstoneSelector};
use crucible_query::{Hidden, HiddenSource};
use crucible_records::{DeletionRequestId, Lsn, ProjectId};
use crucible_shipper::{DeletionRequests, DeletionsUnavailable, KnownDeletion};
use crucible_wal::Wal;
use futures::future::BoxFuture;

/// The version this node writes into the node registry.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The deletion requests read in one statement.
const REQUESTS_PAGE: u32 = 500;

/// The lifecycle of this node. It exists when the node has a control plane.
#[derive(Clone, Debug)]
pub(crate) struct Lifecycle {
    node: Arc<NodeState>,
    /// What the API calls.
    pub(crate) service: Service,
    /// The telemetry store and the project salts, once the store is up. The leader jobs do nothing before
    /// that.
    stores: Arc<OnceLock<LeaderStores>>,
    /// What the shipper reads the deletion requests through.
    deletions: Arc<ControlDeletions>,
}

impl Lifecycle {
    /// The lifecycle of the node whose WAL is `wal`, over the control store.
    pub(crate) fn new(env: &Env, control: Arc<dyn ControlStore>, wal: Wal) -> Self {
        let node = Arc::new(NodeState::new(Arc::new(WalLog(wal.clone())), VERSION, ""));
        Self {
            service: Service::new(env, Arc::clone(&control), Arc::clone(&node)),
            node,
            stores: Arc::default(),
            deletions: Arc::new(ControlDeletions { control, wal }),
        }
    }

    /// The deletion requests as the shipper applies them.
    pub(crate) fn deletions(&self) -> Arc<dyn DeletionRequests> {
        Arc::clone(&self.deletions) as Arc<dyn DeletionRequests>
    }

    /// The node timer and the three leader jobs of the `lifecycle` lease.
    pub(crate) fn jobs(&self) -> Vec<Job> {
        let config = LifecycleConfig::default();
        let stores = Arc::clone(&self.stores);
        let source: LeaderSource = Arc::new(move || stores.get().cloned());
        let mut jobs = vec![node_job(Arc::clone(&self.node), &config)];
        jobs.extend(leader_jobs(&source, config));
        jobs
    }

    /// Gives the leader jobs the telemetry store, and the project salts that a deletion by user hashes its
    /// subject with. The first stores stay.
    pub(crate) fn store_is_up(
        &self,
        telemetry: Arc<dyn TelemetryStore>,
        subjects: Arc<dyn Subjects>,
    ) {
        let _ = self.stores.set(LeaderStores {
            telemetry,
            subjects,
        });
    }
}

impl ProjectSettingsSource for Lifecycle {
    fn settings(&self, project_id: ProjectId) -> ProjectSettings {
        let (retention, floors) = self.node.retention().get(project_id);
        ProjectSettings {
            retention,
            floors,
            ..ProjectSettings::default()
        }
    }
}

impl HiddenSource for Lifecycle {
    fn hidden(&self, project_id: ProjectId) -> Vec<Hidden> {
        self.node.pending().hidden(project_id)
    }
}

/// The deletion requests of the control store, as the shipper applies them (contract 13.3). Before it reads them
/// the node registers in the node registry, so that the deletion executor waits for this node's tombstone of any
/// request made after the read.
#[derive(Debug)]
pub(crate) struct ControlDeletions {
    control: Arc<dyn ControlStore>,
    wal: Wal,
}

fn unavailable(error: TxError) -> DeletionsUnavailable {
    DeletionsUnavailable(format!(
        "the control store did not answer ({error}); check that Postgres is reachable"
    ))
}

/// A request as the shipper applies it.
fn known(request: &DeletionRequest) -> KnownDeletion {
    KnownDeletion {
        tombstone: Tombstone {
            request_id: request.id,
            project_id: request.project_id,
            requested_at: request.requested_at,
            deletion_seq: request.deletion_seq,
            selector: match &request.selector {
                DeletionSelector::Traces(ids) => TombstoneSelector::Traces(ids.clone()),
                DeletionSelector::User { user_id, issuer } => TombstoneSelector::User {
                    user_id: user_id.as_str().into(),
                    issuer: issuer.as_deref().map(Into::into),
                },
                DeletionSelector::Session(session_id) => {
                    TombstoneSelector::Session(session_id.as_str().into())
                }
            },
        },
        state: request.state,
    }
}

impl DeletionRequests for ControlDeletions {
    fn configured(&self) -> bool {
        true
    }

    fn all(&self) -> BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>> {
        Box::pin(async move {
            let repo = self.control.lifecycle();
            repo.heartbeat(&Node {
                wal_epoch: self.wal.epoch(),
                // The store writes its own clock.
                heartbeat_at: crucible_records::Timestamp::from_micros(0),
                version: VERSION.to_owned(),
                checkpoint_lsn: self.wal.watermark().map_or(Lsn::ZERO, |w| w.last_lsn()),
                address: String::new(),
            })
            .await
            .map_err(unavailable)?;
            let mut requests = Vec::new();
            let mut after = 0;
            loop {
                let page = repo
                    .requests_after(after, REQUESTS_PAGE)
                    .await
                    .map_err(unavailable)?;
                requests.extend(page.iter().map(known));
                match page.last() {
                    Some(last) if page.len() == REQUESTS_PAGE as usize => {
                        after = last.deletion_seq;
                    }
                    _ => return Ok(requests),
                }
            }
        })
    }

    fn request(
        &self,
        project: ProjectId,
        id: DeletionRequestId,
    ) -> BoxFuture<'_, Result<Option<KnownDeletion>, DeletionsUnavailable>> {
        Box::pin(async move {
            let request = self
                .control
                .lifecycle()
                .request(project, id)
                .await
                .map_err(unavailable)?;
            Ok(request.as_ref().map(known))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::Duration;

    use crucible_control::{JsonDoc, Project, Repos};
    use crucible_records::{DeletionState, TraceId};
    use crucible_sim::pg_model::{PgFaults, PgModel};

    use super::*;

    const PROJECT: ProjectId = ProjectId::from_bytes([4; 16]);

    fn request(n: u32, selector: DeletionSelector, state: DeletionState) -> DeletionRequest {
        let mut id = [0; 16];
        id[..4].copy_from_slice(&n.to_be_bytes());
        id[6] = 0x70;
        DeletionRequest {
            id: DeletionRequestId::from_bytes(id),
            project_id: PROJECT,
            selector,
            state,
            requested_at: crucible_records::Timestamp::from_micros(i64::from(n)),
            deletion_seq: 0,
            requested_by: "user:test".to_owned(),
            idempotency_key: None,
            request_hash: "00".to_owned(),
            places: "{}".to_owned(),
            attempts: 0,
            error: None,
            updated_at: crucible_records::Timestamp::from_micros(0),
        }
    }

    /// Runs `body` with a node's WAL and a control store that holds one project.
    fn node<T>(body: impl AsyncFnOnce(ControlDeletions, PgModel) -> T + 'static) -> T {
        crucible_sim::run(29, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let (wal, _) = crucible_wal::Wal::open(&host.env, crucible_wal::WalConfig::default())
                .await
                .unwrap();
            let model = PgModel::new(
                host.clock.clone(),
                sim.rng("pg").as_ref(),
                PgFaults::default(),
            );
            model
                .projects()
                .insert(&Project {
                    id: PROJECT,
                    name: "Web".to_owned(),
                    slug: "web".to_owned(),
                    settings: JsonDoc::empty(),
                    retention_days: None,
                    payload_retention_days: None,
                    mcp_content_retention_days: None,
                    config_version: 1,
                    version: 1,
                    deleted_at: None,
                })
                .await
                .unwrap();
            let deletions = ControlDeletions {
                control: Arc::new(model.clone()),
                wal,
            };
            body(deletions, model).await
        })
    }

    #[test]
    fn the_shipper_gets_every_request_in_every_state_after_the_node_registered() {
        node(async |deletions, model| {
            let repo = model.lifecycle();
            let user = DeletionSelector::User {
                user_id: "ada".to_owned(),
                issuer: Some("https://issuer.example".to_owned()),
            };
            let trace = DeletionSelector::Traces(vec![TraceId::from_bytes([7; 16])]);
            for (n, selector, state) in [
                (1, trace, DeletionState::Completed),
                (2, user, DeletionState::Pending),
                (
                    3,
                    DeletionSelector::Session("s-1".to_owned()),
                    DeletionState::Failed,
                ),
            ] {
                repo.insert_request(&request(n, selector, state))
                    .await
                    .unwrap();
            }
            assert!(
                repo.live_nodes(Duration::from_secs(60))
                    .await
                    .unwrap()
                    .is_empty()
            );
            let all = deletions.all().await.unwrap();
            // A request that completed while the node was down still applies to what the node never shipped.
            let states: Vec<DeletionState> = all.iter().map(|known| known.state).collect();
            assert_eq!(
                states,
                [
                    DeletionState::Completed,
                    DeletionState::Pending,
                    DeletionState::Failed
                ]
            );
            assert_eq!(
                all[1].tombstone.selector,
                TombstoneSelector::User {
                    user_id: "ada".into(),
                    issuer: Some("https://issuer.example".into()),
                }
            );
            // The node registered before it read, so the executor waits for it from here on.
            let live = repo.live_nodes(Duration::from_secs(60)).await.unwrap();
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].wal_epoch, deletions.wal.epoch());
            // A tombstone's request is found by its id, with its issuer.
            let found = deletions
                .request(PROJECT, all[1].tombstone.request_id)
                .await
                .unwrap();
            assert_eq!(found.as_ref(), Some(&all[1]));
            let other = ProjectId::from_bytes([5; 16]);
            assert_eq!(
                deletions
                    .request(other, all[1].tombstone.request_id)
                    .await
                    .unwrap(),
                None
            );
        });
    }

    #[test]
    fn every_request_is_read_across_pages() {
        node(async |deletions, model| {
            let repo = model.lifecycle();
            let count = REQUESTS_PAGE + 3;
            for n in 1..=count {
                let selector = DeletionSelector::Session(format!("s-{n}"));
                repo.insert_request(&request(n, selector, DeletionState::Completed))
                    .await
                    .unwrap();
            }
            let all = deletions.all().await.unwrap();
            let seqs: Vec<u64> = all
                .iter()
                .map(|known| known.tombstone.deletion_seq)
                .collect();
            assert_eq!(seqs.len(), count as usize);
            assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
        });
    }

    #[test]
    fn a_control_store_that_does_not_answer_says_how_to_fix_it() {
        crucible_sim::run(31, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let (wal, _) = crucible_wal::Wal::open(&host.env, crucible_wal::WalConfig::default())
                .await
                .unwrap();
            let faults = PgFaults {
                unavailable_ppm: 1_000_000,
                ..PgFaults::default()
            };
            let model = PgModel::new(host.clock.clone(), sim.rng("pg").as_ref(), faults);
            let deletions = ControlDeletions {
                control: Arc::new(model),
                wal,
            };
            let error = deletions.all().await.unwrap_err();
            assert!(
                error.0.contains("check that Postgres is reachable"),
                "{error}"
            );
        });
    }
}
