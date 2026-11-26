//! The project salts of a node with Postgres (contracts 9.3, 11.1 and 11.2): the shipper's `ProjectSalts` hook
//! over the `project_salts` rows of the control store, and the lifecycle's `Subjects`, which hashes the user
//! of a deletion by user the way stage B hashed the subjects it stored (contract 13.3).
//!
//! A salt is 32 random bytes, sealed under the master key and bound to its project (`Sealer::new_project_salt`).
//! It is created in the transaction that creates its project: project creation, and first-owner setup (the
//! setup page and `crucible admin init`) for the default project.
//!
//! [`StoreSalts::run`] loads every salt when the node starts, retrying until Postgres answers, and again every
//! [`RELOAD_EVERY`]. That picks up the projects that another node or `crucible admin init` created, and lets go of
//! deleted ones. The hook answers a project whose salt it has loaded from memory, so the shipper never waits on
//! Postgres for it.
//!
//! A project whose salt is not loaded yet (before the first load, or created since the last one) is read on the
//! first frame that needs it, as before salts were loaded. A project created before salts came with their
//! project gets one then: insert if absent, then read again, so that two nodes that race agree on the row that
//! won. While Postgres does not answer, that frame waits, which can therefore happen only before the project's
//! salt is first loaded. A stored salt that does not open is never loaded and never replaced (a new salt would
//! give every later row of the project another `user_subject_hash`): the project's frames wait until the master
//! key it was sealed with is restored. A deleted project never gets a salt: its frames go on without one, and
//! stage B drops their user subjects.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crucible_auth::Sealer;
use crucible_control::{ControlStore, SealedProjectSalt, TxError};
use crucible_env::{Env, Rng};
use crucible_lifecycle::Subjects;
use crucible_normalize::ProjectSalt;
use crucible_records::ProjectId;
use crucible_shipper::{ProjectSalts, SaltUnavailable};
use futures::future::{self, BoxFuture};

/// How often the loaded salts are brought up to date with the control store.
pub(crate) const RELOAD_EVERY: Duration = Duration::from_secs(5);

/// The salts of the control store, and the ones loaded so far.
pub(crate) struct StoreSalts {
    rng: Arc<dyn Rng>,
    store: Arc<dyn ControlStore>,
    sealer: Sealer,
    loaded: Mutex<BTreeMap<ProjectId, ProjectSalt>>,
}

impl std::fmt::Debug for StoreSalts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSalts").finish_non_exhaustive()
    }
}

fn unavailable(error: &TxError) -> SaltUnavailable {
    SaltUnavailable(error.to_string())
}

impl StoreSalts {
    /// The salts in `store`, sealed by `sealer`. None is loaded until [`StoreSalts::run`] or a frame asks.
    pub(crate) fn new(env: &Env, store: Arc<dyn ControlStore>, sealer: Sealer) -> Self {
        Self {
            rng: env.rng.fork("project-salts"),
            store,
            sealer,
            loaded: Mutex::new(BTreeMap::new()),
        }
    }

    fn loaded(&self) -> MutexGuard<'_, BTreeMap<ProjectId, ProjectSalt>> {
        self.loaded.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open(&self, row: &SealedProjectSalt) -> Result<ProjectSalt, SaltUnavailable> {
        self.sealer
            .open_project_salt(row)
            .map(ProjectSalt::new)
            .map_err(|_| {
                SaltUnavailable(
                    "the stored salt does not open: it was sealed under another master key or altered; restore \
                     the master key it was sealed with"
                        .to_owned(),
                )
            })
    }

    /// Loads every salt of the control store in one read and returns how many are loaded. A salt loaded before
    /// stays as it is, since a salt never changes; the salt of a project whose row is gone goes; a salt that
    /// does not open stays out, so that its project takes the path of a salt that is not loaded.
    pub(crate) async fn load(&self) -> Result<usize, TxError> {
        let rows = self.store.project_salts().list().await?;
        let before = self.loaded().clone();
        let next: BTreeMap<ProjectId, ProjectSalt> = rows
            .iter()
            .filter_map(|row| {
                let salt = match before.get(&row.project_id) {
                    Some(salt) => *salt,
                    None => self.open(row).ok()?,
                };
                Some((row.project_id, salt))
            })
            .collect();
        let count = next.len();
        *self.loaded() = next;
        Ok(count)
    }

    /// Loads the salts when the node starts, retrying until the control store answers, then again every
    /// [`RELOAD_EVERY`], until the node shuts down.
    pub(crate) async fn run(self: Arc<Self>, env: Env) {
        let mut shutdown = env.shutdown();
        let mut first = true;
        loop {
            let loaded = tokio::select! {
                biased;
                () = &mut shutdown => return,
                loaded = self.load() => loaded,
            };
            match loaded {
                Ok(count) if first => {
                    first = false;
                    tracing::info!(count, "the project salts are loaded");
                }
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, "loading the project salts failed; retrying"),
            }
            tokio::select! {
                biased;
                () = &mut shutdown => return,
                () = env.clock.sleep(RELOAD_EVERY) => {}
            }
        }
    }

    /// The path of a salt that is not loaded: read the project's row, create it for a live project that has
    /// none, and keep what was read.
    async fn read_or_create(
        &self,
        project: ProjectId,
    ) -> Result<Option<ProjectSalt>, SaltUnavailable> {
        let repo = self.store.project_salts();
        let mut stored = repo.get(project).await.map_err(|e| unavailable(&e))?;
        if stored.is_none() {
            // A project created before salts came with their projects has none yet; a deleted one gets none.
            let live = self
                .store
                .projects()
                .get(project)
                .await
                .map_err(|e| unavailable(&e))?
                .is_some_and(|row| row.deleted_at.is_none());
            if !live {
                return Ok(None);
            }
            let row = self.sealer.new_project_salt(self.rng.as_ref(), project);
            match repo.insert_if_absent(&row).await {
                // Whether this insert won or another node's did, the salt is the stored row.
                Ok(_) => {}
                // The project's row went between the two reads.
                Err(TxError::ForeignKeyViolation(_)) => return Ok(None),
                Err(error) => return Err(unavailable(&error)),
            }
            stored = repo.get(project).await.map_err(|e| unavailable(&e))?;
        }
        let stored = stored.ok_or_else(|| {
            SaltUnavailable("the salt was not there after it was stored".to_owned())
        })?;
        let salt = self.open(&stored)?;
        self.loaded().insert(project, salt);
        Ok(Some(salt))
    }

    /// The salt of a project when it has one. Unlike the shipper's read, this never creates one: a project
    /// without a salt has no row that carries a subject hash.
    async fn read(&self, project: ProjectId) -> Result<Option<ProjectSalt>, SaltUnavailable> {
        let stored = self
            .store
            .project_salts()
            .get(project)
            .await
            .map_err(|e| unavailable(&e))?;
        stored.map(|stored| self.open(&stored)).transpose()
    }
}

impl ProjectSalts for StoreSalts {
    fn configured(&self) -> bool {
        true
    }

    fn salt(
        &self,
        project: ProjectId,
    ) -> BoxFuture<'_, Result<Option<ProjectSalt>, SaltUnavailable>> {
        let known = self.loaded().get(&project).copied();
        match known {
            Some(salt) => Box::pin(future::ready(Ok(Some(salt)))),
            None => Box::pin(self.read_or_create(project)),
        }
    }
}

/// The subject hashes of a deletion by user (contract 13.3), with the keyed hash that stage B computes.
impl Subjects for StoreSalts {
    fn hashes<'a>(
        &'a self,
        project_id: ProjectId,
        issuers: &'a [String],
        subject: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<[u8; 16]>>, String>> {
        Box::pin(async move {
            let salt = self.read(project_id).await.map_err(|error| error.0)?;
            Ok(salt.map(|salt| {
                issuers
                    .iter()
                    .map(|issuer| salt.subject_hash(issuer, subject))
                    .collect()
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use crucible_auth::csrf::Origins;
    use crucible_auth::password::HashCost;
    use crucible_auth::service::{Identity, IdentityConfig};
    use crucible_auth::{DbKeys, LookupKey, OutageMode};
    use crucible_control::{JsonDoc, KeySnapshot, Project, Repos};
    use crucible_sim::pg_model::{PgFaults, PgModel};

    use super::*;

    const PROJECT: ProjectId = ProjectId::from_bytes([4; 16]);
    const OTHER: ProjectId = ProjectId::from_bytes([5; 16]);

    /// Runs `body` on a simulated host with an empty `PgModel`.
    fn sim<T>(faults: PgFaults, body: impl AsyncFnOnce(Env, PgModel) -> T + 'static) -> T {
        crucible_sim::run(23, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let model = PgModel::new(host.clock.clone(), sim.rng("pg").as_ref(), faults);
            body(host.env.clone(), model).await
        })
    }

    fn salts(env: &Env, model: &PgModel, key: [u8; 32]) -> StoreSalts {
        StoreSalts::new(env, Arc::new(model.clone()), Sealer::new("master-0", key))
    }

    /// A project's row as a version before this one created it: without a salt.
    async fn bare_project(model: &PgModel, id: ProjectId) {
        let project = Project {
            id,
            name: format!("Project {id}"),
            slug: format!("project-{id}"),
            settings: JsonDoc::empty(),
            retention_days: None,
            payload_retention_days: None,
            mcp_content_retention_days: None,
            config_version: 1,
            version: 1,
            deleted_at: None,
        };
        model.projects().insert(&project).await.unwrap();
    }

    /// Identity over `model`, sealing with the key the hook opens with.
    fn identity(env: &Env, model: &PgModel, key: [u8; 32]) -> Identity {
        let mut config = IdentityConfig::new(Origins::default(), None);
        config.hash_cost = HashCost {
            memory_kib: 64,
            iterations: 1,
            lanes: 1,
        };
        let lookup = LookupKey::new([5; 32]);
        let keys = Arc::new(DbKeys::new(env, lookup.clone(), OutageMode::default()));
        keys.load(&KeySnapshot {
            max_seq: 0,
            keys: Vec::new(),
        });
        let sealer = Sealer::new("master-0", key);
        Identity::new(env, Arc::new(model.clone()), sealer, lookup, keys, config).unwrap()
    }

    fn down() -> PgFaults {
        PgFaults {
            unavailable_ppm: 1_000_000,
            ..PgFaults::default()
        }
    }

    #[test]
    fn a_salt_is_created_once_and_read_back_by_every_node_and_after_a_restart() {
        sim(PgFaults::default(), async |env, model| {
            bare_project(&model, PROJECT).await;
            bare_project(&model, OTHER).await;
            let (a, b) = (salts(&env, &model, [9; 32]), salts(&env, &model, [9; 32]));
            // Two nodes race for the first salt of a project that has none: both get the row that won.
            let (first, second) = futures::join!(a.salt(PROJECT), b.salt(PROJECT));
            let first = first.unwrap().unwrap();
            assert_eq!(first.as_bytes(), second.unwrap().unwrap().as_bytes());
            // A node that starts later reads the same salt, so the same subject hashes the same.
            let restarted = salts(&env, &model, [9; 32])
                .salt(PROJECT)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                restarted.subject_hash("https://issuer.example", "user-1"),
                first.subject_hash("https://issuer.example", "user-1")
            );
            // Another project has another salt.
            let other = a.salt(OTHER).await.unwrap().unwrap();
            assert_ne!(other.as_bytes(), first.as_bytes());
            // The row holds the salt sealed, not the salt.
            let row = model.project_salts().get(PROJECT).await.unwrap().unwrap();
            assert_eq!(row.key_id, "master-0");
            assert!(!row.ciphertext.windows(32).any(|w| w == first.as_bytes()));
            assert!(!format!("{row:?}{a:?}").contains("ciphertext"));
        });
    }

    #[test]
    fn a_salt_that_does_not_open_is_reported_and_never_replaced() {
        sim(PgFaults::default(), async |env, model| {
            bare_project(&model, PROJECT).await;
            let first = salts(&env, &model, [9; 32])
                .salt(PROJECT)
                .await
                .unwrap()
                .unwrap();
            let before = model.project_salts().get(PROJECT).await.unwrap();
            let other_key = salts(&env, &model, [8; 32]);
            let error = other_key.salt(PROJECT).await.unwrap_err();
            assert!(error.0.contains("does not open"), "{error}");
            // A load leaves it out, so the frame keeps waiting rather than taking another salt.
            assert_eq!(other_key.load().await.unwrap(), 0);
            assert!(other_key.salt(PROJECT).await.is_err());
            assert_eq!(model.project_salts().get(PROJECT).await.unwrap(), before);
            let again = salts(&env, &model, [9; 32])
                .salt(PROJECT)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(again.as_bytes(), first.as_bytes());
        });
    }

    /// A deletion by user hashes the user as stage B hashed the subjects it stored, never creates a salt for
    /// a project that has none, and says how to fix a salt that does not open.
    #[test]
    fn a_deletion_hashes_with_the_stored_salt_and_never_creates_one() {
        sim(PgFaults::default(), async |env, model| {
            bare_project(&model, PROJECT).await;
            let issuers = [
                "https://a.example".to_owned(),
                "https://b.example".to_owned(),
            ];
            let lifecycle = salts(&env, &model, [9; 32]);
            assert_eq!(lifecycle.hashes(PROJECT, &issuers, "ada").await, Ok(None));
            assert!(model.project_salts().get(PROJECT).await.unwrap().is_none());
            let stored = salts(&env, &model, [9; 32])
                .salt(PROJECT)
                .await
                .unwrap()
                .unwrap();
            let expected: Vec<[u8; 16]> = issuers
                .iter()
                .map(|issuer| stored.subject_hash(issuer, "ada"))
                .collect();
            assert_eq!(
                lifecycle.hashes(PROJECT, &issuers, "ada").await,
                Ok(Some(expected))
            );
            let error = salts(&env, &model, [8; 32])
                .hashes(PROJECT, &issuers, "ada")
                .await
                .unwrap_err();
            assert!(error.contains("restore the master key"), "{error}");
        });
    }

    /// Before its salt is loaded, a project's frame waits while the store does not answer.
    #[test]
    fn a_store_that_does_not_answer_gives_no_salt_before_the_first_load() {
        sim(down(), async |env, model| {
            let hook = salts(&env, &model, [9; 32]);
            assert!(!hook.salt(PROJECT).await.unwrap_err().0.is_empty());
            assert!(hook.load().await.is_err());
            model.set_faults(PgFaults::default());
            assert!(model.project_salts().get(PROJECT).await.unwrap().is_none());
        });
    }

    /// Contract 9.3: a project made through identity has its salt from the start. The hook reads that salt and
    /// stores none of its own, and once it has loaded it, answers without a call to the store.
    #[test]
    fn the_salt_identity_made_is_the_one_the_hook_answers() {
        sim(PgFaults::default(), async |env, model| {
            let made = identity(&env, &model, [9; 32])
                .create_project("Agents", "agents")
                .await
                .unwrap();
            let row = model.project_salts().get(made.id).await.unwrap();
            assert!(row.is_some(), "the project has its salt before any frame");
            let hook = salts(&env, &model, [9; 32]);
            let read = hook.salt(made.id).await.unwrap().unwrap();
            assert_eq!(model.project_salts().get(made.id).await.unwrap(), row);
            let reloaded = salts(&env, &model, [9; 32]);
            assert_eq!(reloaded.load().await.unwrap(), 1);
            model.set_faults(down());
            let answered = reloaded.salt(made.id).await.unwrap().unwrap();
            assert_eq!(answered.as_bytes(), read.as_bytes());
        });
    }

    /// The loop loads every salt at startup and again after each change: a project made after the first load
    /// is answered once the next load ran, and while the store is down a loaded project never waits. A deleted
    /// project leaves the loaded salts and never gets a new one.
    #[test]
    fn loaded_salts_follow_the_store_and_never_wait_on_it() {
        sim(PgFaults::default(), async |env, model| {
            let maker = identity(&env, &model, [9; 32]);
            let first = maker.create_project("First", "first").await.unwrap();
            let hook = Arc::new(salts(&env, &model, [9; 32]));
            drop(env.spawn("project-salts", Arc::clone(&hook).run(env.clone())));
            env.clock.sleep(Duration::from_millis(10)).await;
            let second = maker.create_project("Second", "second").await.unwrap();
            env.clock.sleep(RELOAD_EVERY).await;
            model.set_faults(down());
            for project in [first.id, second.id] {
                assert!(hook.salt(project).await.unwrap().is_some(), "{project}");
            }
            // A project whose salt is not loaded waits while the store is down.
            assert!(hook.salt(PROJECT).await.is_err());
            model.set_faults(PgFaults::default());

            // Project deletion: the project is deleted, then its salt.
            let by = crucible_records::UserId::from_bytes([1; 16]);
            maker
                .delete_project(first.id, first.version, by)
                .await
                .unwrap();
            assert!(model.project_salts().delete(first.id).await.unwrap());
            env.clock.sleep(RELOAD_EVERY).await;
            assert_eq!(hook.salt(first.id).await.unwrap(), None);
            assert!(model.project_salts().get(first.id).await.unwrap().is_none());
            // A project whose row is gone gets none either.
            assert_eq!(hook.salt(PROJECT).await.unwrap(), None);
            assert!(model.project_salts().get(PROJECT).await.unwrap().is_none());
        });
    }
}
