//! The project salts of a node with Postgres (contracts 9.3, 11.1 and 11.2): the shipper's `ProjectSalts` hook
//! over the `project_salts` rows of the control store, and the lifecycle's `Subjects`, which hashes the user
//! of a deletion by user the way stage B hashed the subjects it stored (contract 13.3).
//!
//! A salt is 32 random bytes, sealed under the master key with associated data that binds it to its project. It
//! is created by the first frame of the project that carries a user subject: insert if absent, then read again,
//! so that two nodes that race agree on the row that won. A salt that does not open is never replaced: a new
//! salt would give every later row of the project another `user_subject_hash`.

use std::sync::Arc;

use crucible_auth::{Sealed, Sealer};
use crucible_control::{ControlStore, SealedProjectSalt, TxError};
use crucible_env::{Env, Rng};
use crucible_lifecycle::Subjects;
use crucible_normalize::ProjectSalt;
use crucible_records::ProjectId;
use crucible_shipper::{ProjectSalts, SaltUnavailable};
use futures::future::BoxFuture;

/// The salts of the control store.
pub(crate) struct StoreSalts {
    rng: Arc<dyn Rng>,
    store: Arc<dyn ControlStore>,
    sealer: Sealer,
}

impl std::fmt::Debug for StoreSalts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreSalts").finish_non_exhaustive()
    }
}

/// The associated data that binds a sealed salt to its project.
fn salt_aad(project: ProjectId) -> Vec<u8> {
    format!("{}\0project_salt", project.uuid()).into_bytes()
}

fn unavailable(error: &TxError) -> SaltUnavailable {
    SaltUnavailable(error.to_string())
}

impl StoreSalts {
    /// The salts in `store`, sealed by `sealer`.
    pub(crate) fn new(env: &Env, store: Arc<dyn ControlStore>, sealer: Sealer) -> Self {
        Self {
            rng: env.rng.fork("project-salts"),
            store,
            sealer,
        }
    }

    async fn read_or_create(&self, project: ProjectId) -> Result<ProjectSalt, SaltUnavailable> {
        let repo = self.store.project_salts();
        let aad = salt_aad(project);
        let mut stored = repo.get(project).await.map_err(|e| unavailable(&e))?;
        if stored.is_none() {
            let mut salt = [0u8; 32];
            self.rng.fill_bytes(&mut salt);
            let sealed = self.sealer.seal(self.rng.as_ref(), &aad, &salt);
            let row = SealedProjectSalt {
                project_id: project,
                key_id: sealed.key_id,
                nonce: sealed.nonce,
                ciphertext: sealed.ciphertext,
            };
            // Whether this insert won or another node's did, the salt is the stored row.
            repo.insert_if_absent(&row)
                .await
                .map_err(|e| unavailable(&e))?;
            stored = repo.get(project).await.map_err(|e| unavailable(&e))?;
        }
        let stored = stored.ok_or_else(|| {
            SaltUnavailable("the salt was not there after it was stored".to_owned())
        })?;
        self.open(stored, &aad)
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
        stored
            .map(|stored| self.open(stored, &salt_aad(project)))
            .transpose()
    }

    fn open(&self, stored: SealedProjectSalt, aad: &[u8]) -> Result<ProjectSalt, SaltUnavailable> {
        let sealed = Sealed {
            key_id: stored.key_id,
            nonce: stored.nonce,
            ciphertext: stored.ciphertext,
        };
        let salt = self
            .sealer
            .open(&sealed, aad)
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .ok_or_else(|| {
                SaltUnavailable(
                    "the stored salt does not open: it was sealed under another master key or altered; restore \
                     the master key it was sealed with"
                        .to_owned(),
                )
            })?;
        Ok(ProjectSalt::new(salt))
    }
}

impl ProjectSalts for StoreSalts {
    fn configured(&self) -> bool {
        true
    }

    fn salt(&self, project: ProjectId) -> BoxFuture<'_, Result<ProjectSalt, SaltUnavailable>> {
        Box::pin(self.read_or_create(project))
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

    use crucible_control::Repos;
    use crucible_sim::pg_model::{PgFaults, PgModel};

    use super::*;

    const PROJECT: ProjectId = ProjectId::from_bytes([4; 16]);

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

    #[test]
    fn a_salt_is_created_once_and_read_back_by_every_node_and_after_a_restart() {
        sim(PgFaults::default(), async |env, model| {
            let (a, b) = (salts(&env, &model, [9; 32]), salts(&env, &model, [9; 32]));
            // Two nodes race for the first salt of the project: both get the row that won.
            let (first, second) = futures::join!(a.salt(PROJECT), b.salt(PROJECT));
            let first = first.unwrap();
            assert_eq!(first.as_bytes(), second.unwrap().as_bytes());
            // A node that starts later reads the same salt, so the same subject hashes the same.
            let restarted = salts(&env, &model, [9; 32]).salt(PROJECT).await.unwrap();
            assert_eq!(
                restarted.subject_hash("https://issuer.example", "user-1"),
                first.subject_hash("https://issuer.example", "user-1")
            );
            // Another project has another salt.
            let other = a.salt(ProjectId::from_bytes([5; 16])).await.unwrap();
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
            let first = salts(&env, &model, [9; 32]).salt(PROJECT).await.unwrap();
            let before = model.project_salts().get(PROJECT).await.unwrap();
            let error = salts(&env, &model, [8; 32])
                .salt(PROJECT)
                .await
                .unwrap_err();
            assert!(error.0.contains("does not open"), "{error}");
            assert_eq!(model.project_salts().get(PROJECT).await.unwrap(), before);
            let again = salts(&env, &model, [9; 32]).salt(PROJECT).await.unwrap();
            assert_eq!(again.as_bytes(), first.as_bytes());
        });
    }

    /// A deletion by user hashes the user as stage B hashed the subjects it stored, never creates a salt for
    /// a project that has none, and says how to fix a salt that does not open.
    #[test]
    fn a_deletion_hashes_with_the_stored_salt_and_never_creates_one() {
        sim(PgFaults::default(), async |env, model| {
            let issuers = [
                "https://a.example".to_owned(),
                "https://b.example".to_owned(),
            ];
            let lifecycle = salts(&env, &model, [9; 32]);
            assert_eq!(lifecycle.hashes(PROJECT, &issuers, "ada").await, Ok(None));
            assert!(model.project_salts().get(PROJECT).await.unwrap().is_none());
            let stored = salts(&env, &model, [9; 32]).salt(PROJECT).await.unwrap();
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

    #[test]
    fn a_store_that_does_not_answer_gives_no_salt() {
        let faults = PgFaults {
            unavailable_ppm: 1_000_000,
            ..PgFaults::default()
        };
        sim(faults, async |env, model| {
            let error = salts(&env, &model, [9; 32])
                .salt(PROJECT)
                .await
                .unwrap_err();
            assert!(!error.0.is_empty());
            model.set_faults(PgFaults::default());
            assert!(model.project_salts().get(PROJECT).await.unwrap().is_none());
        });
    }
}
