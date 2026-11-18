//! Passwords (contract 11.4): Argon2id with 64 MiB, 3 iterations and 1 lane, off the async threads and behind a
//! semaphore, with a dummy verification for unknown emails so that timing does not reveal which emails exist.

use std::sync::Arc;

use argon2::{Algorithm, Argon2, Params, PasswordHasher as _, PasswordVerifier as _, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use crucible_env::Env;
use tokio::sync::Semaphore;

/// The fewest characters a password may have.
pub const MIN_CHARS: usize = 12;
/// The most characters a password may have.
pub const MAX_CHARS: usize = 128;
/// The version of the hashing parameters that new hashes are made with. A login with a hash of another version
/// rehashes the password.
pub const PARAMS_VERSION: u32 = 1;
/// The length of a salt.
const SALT_LEN: usize = 16;
/// The length of a hash.
const HASH_LEN: usize = 32;

/// Argon2id costs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HashCost {
    /// Memory, in KiB.
    pub memory_kib: u32,
    /// Iterations.
    pub iterations: u32,
    /// Lanes.
    pub lanes: u32,
}

impl HashCost {
    /// The costs of contract 11.4: 64 MiB, 3 iterations, 1 lane.
    pub const CONTRACT: Self = Self {
        memory_kib: 64 * 1024,
        iterations: 3,
        lanes: 1,
    };
}

/// Why a password is refused as a new password. The messages say what a valid password is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordPolicy {
    /// Fewer than [`MIN_CHARS`] characters.
    #[error("the password must have at least 12 characters")]
    TooShort,
    /// More than [`MAX_CHARS`] characters.
    #[error("the password must have at most 128 characters")]
    TooLong,
}

/// Checks a new password: 12 to 128 characters, no composition rules.
pub fn check_policy(password: &str) -> Result<(), PasswordPolicy> {
    let chars = password.chars().count();
    if chars < MIN_CHARS {
        Err(PasswordPolicy::TooShort)
    } else if chars > MAX_CHARS {
        Err(PasswordPolicy::TooLong)
    } else {
        Ok(())
    }
}

/// Why hashing or verifying did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// Every hashing slot is taken; the request is answered with 429.
    #[error("too many password checks are running; try again in a second")]
    Busy,
    /// The hashing task was cancelled by shutdown.
    #[error("the server is shutting down")]
    Cancelled,
    /// The stored hash cannot be read, or the costs are invalid.
    #[error("password hashing failed: {0}")]
    Failed(String),
}

/// Hashes and verifies passwords.
#[derive(Debug, Clone)]
pub struct Passwords {
    env: Env,
    params: Params,
    permits: Arc<Semaphore>,
    /// A hash of no password, verified against when the email is unknown.
    dummy: String,
}

fn hasher(params: Params) -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

impl Passwords {
    /// Hashing with `cost`, at most `permits` at a time (the server passes min(cores, 4)).
    pub fn new(env: &Env, cost: HashCost, permits: usize) -> Result<Self, PasswordError> {
        let params = Params::new(cost.memory_kib, cost.iterations, cost.lanes, Some(HASH_LEN))
            .map_err(|error| PasswordError::Failed(error.to_string()))?;
        let mut salt = [0; SALT_LEN];
        env.rng.fill_bytes(&mut salt);
        let dummy = format!(
            "$argon2id$v=19$m={},t={},p={}${}${}",
            cost.memory_kib,
            cost.iterations,
            cost.lanes,
            STANDARD_NO_PAD.encode(salt),
            STANDARD_NO_PAD.encode([0; HASH_LEN]),
        );
        Ok(Self {
            env: env.clone(),
            params,
            permits: Arc::new(Semaphore::new(permits.max(1))),
            dummy,
        })
    }

    /// Runs `f` on the blocking path, holding a slot.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, PasswordError> {
        let Ok(_permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            return Err(PasswordError::Busy);
        };
        self.env
            .spawn_blocking("argon2", f)
            .join()
            .await
            .map_err(|_| PasswordError::Cancelled)
    }

    /// The PHC string of `password` with a fresh salt.
    pub async fn hash(&self, password: &str) -> Result<String, PasswordError> {
        let mut salt = [0; SALT_LEN];
        self.env.rng.fill_bytes(&mut salt);
        let (params, password) = (self.params.clone(), password.to_owned());
        self.blocking(move || {
            hasher(params)
                .hash_password_with_salt(password.as_bytes(), &salt)
                .map(|hash| hash.to_string())
                .map_err(|error| PasswordError::Failed(error.to_string()))
        })
        .await?
    }

    /// Whether `password` matches the stored PHC string. Without a stored hash (an unknown email, or a user
    /// without a password) a dummy hash is verified instead, so the answer takes as long and is `false`.
    pub async fn verify(
        &self,
        password: &str,
        stored: Option<&str>,
    ) -> Result<bool, PasswordError> {
        let known = stored.is_some();
        let stored = stored.unwrap_or(&self.dummy).to_owned();
        let (params, password) = (self.params.clone(), password.to_owned());
        let matched = self
            .blocking(move || {
                hasher(params)
                    .verify_password(password.as_bytes(), stored.as_str())
                    .is_ok()
            })
            .await?;
        Ok(known && matched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small costs: the contract's 64 MiB crawl in debug builds.
    const TEST_COST: HashCost = HashCost {
        memory_kib: 64,
        iterations: 1,
        lanes: 1,
    };

    fn passwords(env: &Env, permits: usize) -> Passwords {
        Passwords::new(env, TEST_COST, permits).unwrap()
    }

    #[test]
    fn the_contract_costs_are_64_mib_3_iterations_1_lane() {
        assert_eq!(
            HashCost::CONTRACT,
            HashCost {
                memory_kib: 65_536,
                iterations: 3,
                lanes: 1
            }
        );
        crate::testing::sim(1, async |env| {
            let passwords = Passwords::new(&env, HashCost::CONTRACT, 4).unwrap();
            assert!(
                passwords
                    .dummy
                    .starts_with("$argon2id$v=19$m=65536,t=3,p=1$")
            );
        });
    }

    #[test]
    fn the_policy_counts_characters() {
        assert_eq!(check_policy("short"), Err(PasswordPolicy::TooShort));
        assert_eq!(check_policy(&"é".repeat(11)), Err(PasswordPolicy::TooShort));
        assert_eq!(check_policy(&"é".repeat(12)), Ok(()));
        assert_eq!(check_policy(&"x".repeat(128)), Ok(()));
        assert_eq!(check_policy(&"x".repeat(129)), Err(PasswordPolicy::TooLong));
    }

    #[test]
    fn a_hash_verifies_its_password_only() {
        crate::testing::sim(2, async |env| {
            let passwords = passwords(&env, 2);
            let hash = passwords.hash("correct horse battery").await.unwrap();
            assert!(hash.starts_with("$argon2id$v=19$m=64,t=1,p=1$"), "{hash}");
            assert!(
                passwords
                    .verify("correct horse battery", Some(&hash))
                    .await
                    .unwrap()
            );
            assert!(
                !passwords
                    .verify("correct horse battery!", Some(&hash))
                    .await
                    .unwrap()
            );
            // Unknown emails verify against the dummy hash and never match.
            assert!(
                !passwords
                    .verify("correct horse battery", None)
                    .await
                    .unwrap()
            );
            assert!(!passwords.verify("", None).await.unwrap());
            // Two hashes of one password differ by their salts.
            assert_ne!(hash, passwords.hash("correct horse battery").await.unwrap());
            // A stored hash that does not parse never matches.
            assert!(!passwords.verify("x", Some("not a hash")).await.unwrap());
        });
    }

    #[test]
    fn a_full_semaphore_answers_busy() {
        crate::testing::sim(3, async |env| {
            let passwords = passwords(&env, 1);
            let held = Arc::clone(&passwords.permits).try_acquire_owned().unwrap();
            assert_eq!(passwords.hash("x").await, Err(PasswordError::Busy));
            drop(held);
            assert!(passwords.hash("x").await.is_ok());
        });
    }
}
