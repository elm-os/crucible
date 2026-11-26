//! Retention and deletion on this node (contract 13): the node's state over its WAL, the jobs, and what the
//! API and the read services take from them.
//!
//! The node timer keeps two snapshots of Postgres in memory, refreshed every 5 s: every project's retention
//! with its floors, and the pending-deletion set. Reads are clamped and filtered with them, so a retention
//! change or a deletion request hides its rows at once on the node that accepted it and within one refresh on
//! every other node.
//!
//! The shipper reads the deletion requests through [`ControlDeletions`] before it ships what they can select, so
//! that stage B drops what a request that completed while this node was down deleted (contract 13.3). It reads only
//! the requests made after the data it ships arrived, because a deletion selects data received before it.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crucible_api::{ProjectSettings, ProjectSettingsSource};
use crucible_control::{ControlStore, DeletionRequest, DeletionSelector, Node, TxError};
use crucible_env::{Clock, Env, TelemetryStore};
use crucible_export::ExportObjects;
use crucible_ingest::EventHorizon;
use crucible_jobs::Job;
use crucible_lifecycle::{
    LeaderSource, LeaderStores, LifecycleConfig, NodeState, RetentionSnapshot, Service, Subjects,
    WalLog, leader_jobs, node_job,
};
use crucible_normalize::{Tombstone, TombstoneSelector};
use crucible_plan::RetentionClamp;
use crucible_query::{Hidden, HiddenSource};
use crucible_records::{DeletionRequestId, Lsn, ProjectId, TableId, Timestamp};
use crucible_shipper::{DeletionRequests, DeletionsUnavailable, KnownDeletion};
use crucible_wal::Wal;
use futures::future::BoxFuture;

/// The version this node writes into the node registry: the release number with build metadata that says the node
/// reads deletion requests of kind `project`. Servers of 0.1.3 and earlier cannot, and register their bare release
/// number, so the leader holds those requests while a node registered by a bare release number is live (contract
/// 13.4); without the metadata this node would count as one of them.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+project-deletions");

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
    /// The object store and its key prefix, when the deployment has one: it holds media, which a completed
    /// deletion request then reports on, and the export files that a deletion removes.
    objects: Option<ExportObjects>,
    /// What the shipper reads the deletion requests through.
    deletions: Arc<ControlDeletions>,
    /// `ingest.max_event_age` as this node's ingest applies it, which bounds what a project deletion drops.
    max_event_age: Duration,
}

impl Lifecycle {
    /// The lifecycle of the node whose WAL is `wal`, over the control store. `objects` is the deployment's
    /// object store, when it has one, which stores media and export files. `max_event_age` is the
    /// `ingest.max_event_age` that this node's ingest applies.
    pub(crate) fn new(
        env: &Env,
        control: Arc<dyn ControlStore>,
        wal: Wal,
        objects: Option<ExportObjects>,
        max_event_age: Duration,
    ) -> Self {
        let node = Arc::new(NodeState::new(Arc::new(WalLog(wal.clone())), VERSION, ""));
        Self {
            service: Service::new(env, Arc::clone(&control), Arc::clone(&node)),
            node,
            stores: Arc::default(),
            objects,
            deletions: Arc::new(ControlDeletions {
                control,
                wal,
                clock: Arc::clone(&env.clock),
            }),
            max_event_age,
        }
    }

    /// The deletion requests as the shipper applies them.
    pub(crate) fn deletions(&self) -> Arc<dyn DeletionRequests> {
        Arc::clone(&self.deletions) as Arc<dyn DeletionRequests>
    }

    /// The node timer and the leader jobs of the `lifecycle` lease.
    pub(crate) fn jobs(&self) -> Vec<Job> {
        let mut config = LifecycleConfig::default();
        config.media = self.objects.is_some();
        config.max_event_age = self.max_event_age;
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
            exports: self.objects.clone(),
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

impl Lifecycle {
    /// The retention horizon that ingest rejects older spans by (contract 1.2).
    pub(crate) fn horizon(&self) -> Arc<dyn EventHorizon> {
        Arc::new(Horizon(Arc::clone(self.node.retention())))
    }
}

/// A project's retention horizon: the first instant of `span_core` that reads do not hide, from the node's
/// snapshot of every project's retention and floors. A span older than it would be hidden at once, so ingest
/// rejects it instead of acknowledging it.
#[derive(Debug)]
struct Horizon(Arc<RetentionSnapshot>);

impl EventHorizon for Horizon {
    fn horizon(&self, project: ProjectId, now: Timestamp) -> Option<Timestamp> {
        let (retention, floors) = self.0.get(project);
        RetentionClamp::new(retention, now)
            .with_floors(floors)
            .cutoff(TableId::SpanCore)
            .ok()
            .flatten()
    }
}

/// The deletion requests of the control store, as the shipper applies them (contract 13.3). Before it reads them
/// the node registers in the node registry, so that the deletion executor waits for this node's tombstone of any
/// request made after the read.
#[derive(Debug)]
pub(crate) struct ControlDeletions {
    control: Arc<dyn ControlStore>,
    wal: Wal,
    /// The node's clock, which its registration carries beside the database clock.
    clock: Arc<dyn Clock>,
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
                DeletionSelector::Project => TombstoneSelector::Project,
            },
        },
        state: request.state,
    }
}

impl DeletionRequests for ControlDeletions {
    fn configured(&self) -> bool {
        true
    }

    fn requested_after(
        &self,
        received_at: Timestamp,
    ) -> BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>> {
        Box::pin(async move {
            let repo = self.control.lifecycle();
            repo.heartbeat(&Node {
                wal_epoch: self.wal.epoch(),
                // The store writes its own clock.
                heartbeat_at: crucible_records::Timestamp::from_micros(0),
                version: VERSION.to_owned(),
                checkpoint_lsn: self.wal.watermark().map_or(Lsn::ZERO, |w| w.last_lsn()),
                address: String::new(),
                node_clock: Some(self.clock.wall_now()),
            })
            .await
            .map_err(unavailable)?;
            let mut requests = Vec::new();
            let mut after = 0;
            loop {
                let page = repo
                    .requests_after(received_at, after, REQUESTS_PAGE)
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
    use crucible_query::RetentionFloors;
    use crucible_records::{DeletionState, TraceId};
    use crucible_schema::RetentionSettings;
    use crucible_sim::pg_model::{PgFaults, PgModel};

    use super::*;

    const PROJECT: ProjectId = ProjectId::from_bytes([4; 16]);

    /// A project deletion drops only what ingest can no longer write to, so the node gives its jobs the
    /// `ingest.max_event_age` its ingest applies, and the defaults agree for whatever builds them alone.
    #[test]
    fn the_jobs_and_ingest_share_the_maximum_event_age() {
        assert_eq!(
            LifecycleConfig::default().max_event_age,
            crucible_ingest::DEFAULT_MAX_EVENT_AGE
        );
        assert_eq!(
            crucible_ingest::IngestConfig::default().max_event_age,
            crucible_ingest::DEFAULT_MAX_EVENT_AGE
        );
    }

    /// The horizon is the read cut-off of `span_core`: the start of the UTC day `retention_days` back, never
    /// below a floor that an earlier, shorter retention set, and none for a project that keeps everything.
    #[test]
    fn the_horizon_is_the_read_cut_off_of_spans() {
        let snapshot = Arc::new(RetentionSnapshot::default());
        let horizon = Horizon(Arc::clone(&snapshot));
        let (kept, ninety, floored) = (
            ProjectId::from_bytes([1; 16]),
            ProjectId::from_bytes([2; 16]),
            ProjectId::from_bytes([3; 16]),
        );
        let now = Timestamp::parse_rfc3339("2026-11-19T18:30:00Z").unwrap();
        let days = |n| RetentionSettings {
            retention_days: Some(n),
            ..RetentionSettings::default()
        };
        snapshot.set(ninety, days(90), RetentionFloors::default());
        let floor = Timestamp::parse_rfc3339("2026-10-01T00:00:00Z").unwrap();
        snapshot.set(
            floored,
            days(90),
            RetentionFloors {
                retention: Some(floor),
                ..RetentionFloors::default()
            },
        );
        assert_eq!(horizon.horizon(kept, now), None);
        assert_eq!(
            horizon.horizon(ninety, now),
            Some(Timestamp::parse_rfc3339("2026-08-21T00:00:00Z").unwrap())
        );
        assert_eq!(horizon.horizon(floored, now), Some(floor));
    }

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
                clock: Arc::clone(&host.env.clock),
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
                (4, DeletionSelector::Project, DeletionState::Logged),
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
            let all = deletions
                .requested_after(Timestamp::from_micros(0))
                .await
                .unwrap();
            // A request that completed while the node was down still applies to what the node never shipped.
            let states: Vec<DeletionState> = all.iter().map(|known| known.state).collect();
            assert_eq!(
                states,
                [
                    DeletionState::Completed,
                    DeletionState::Pending,
                    DeletionState::Failed,
                    DeletionState::Logged
                ]
            );
            assert_eq!(
                all[1].tombstone.selector,
                TombstoneSelector::User {
                    user_id: "ada".into(),
                    issuer: Some("https://issuer.example".into()),
                }
            );
            // Deleting a project selects every frame of it received before, as its tombstone does.
            assert_eq!(all[3].tombstone.selector, TombstoneSelector::Project);
            // The node registered before it read, so the executor waits for it from here on, with its own clock
            // and a version that reads project deletions.
            let live = repo.live_nodes(Duration::from_secs(60)).await.unwrap();
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].wal_epoch, deletions.wal.epoch());
            assert!(
                live[0]
                    .clock_skew()
                    .is_some_and(|skew| skew.abs() < 1_000_000),
                "{:?}",
                live[0]
            );
            assert!(live[0].reads_project_requests(), "{}", live[0].version);
            assert!(live[0].version.starts_with(env!("CARGO_PKG_VERSION")));
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
            let all = deletions
                .requested_after(Timestamp::from_micros(0))
                .await
                .unwrap();
            let seqs: Vec<u64> = all
                .iter()
                .map(|known| known.tombstone.deletion_seq)
                .collect();
            assert_eq!(seqs.len(), count as usize);
            assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
        });
    }

    /// A request made at or before an instant selects nothing received then, so the shipper does not read it, on
    /// any page.
    #[test]
    fn only_the_requests_made_after_the_instant_are_read() {
        node(async |deletions, model| {
            let repo = model.lifecycle();
            let count = REQUESTS_PAGE + 3;
            for n in 1..=count {
                let selector = DeletionSelector::Session(format!("s-{n}"));
                repo.insert_request(&request(n, selector, DeletionState::Completed))
                    .await
                    .unwrap();
            }
            // Each request was made `n` microseconds after the epoch.
            let read = deletions
                .requested_after(Timestamp::from_micros(2))
                .await
                .unwrap();
            let made: Vec<i64> = read
                .iter()
                .map(|known| known.tombstone.requested_at.micros())
                .collect();
            assert_eq!(made.len(), count as usize - 2);
            assert!(made.iter().all(|&micros| micros > 2), "{made:?}");
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
                clock: Arc::clone(&host.env.clock),
            };
            let error = deletions
                .requested_after(Timestamp::from_micros(0))
                .await
                .unwrap_err();
            assert!(
                error.0.contains("check that Postgres is reachable"),
                "{error}"
            );
        });
    }
}
