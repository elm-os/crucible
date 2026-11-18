//! Secret keys and their verification (contract 11.3).
//!
//! Milestone 1 verifies the project keys an operator lists in configuration ([`StaticKeys`]). The key
//! snapshot, the key feed, expiry and the Postgres outage modes of section 11.8 arrive with the control plane,
//! behind the same [`KeyVerifier`] seam.

use core::fmt;

use crucible_env::Rng;
use crucible_records::{ApiKeyId, ProjectId, ProjectScope};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::scope;

/// The prefix of a secret key.
const SECRET_PREFIX: &str = "csk_";
/// The base62 characters that follow the prefix.
const BODY_LEN: usize = 43;
/// The largest body: 2^256 - 1 in base62. Bodies have a fixed width and the alphabet `0-9A-Za-z` is in ASCII
/// order, so comparing two bodies as text compares their values.
const MAX_BODY: &[u8; BODY_LEN] = b"yhjskwdA6OZ1AL1YmHWZWm8LLG7HjnuCA2j5rOw8Xp1";
/// The length of each key's random salt.
const SALT_LEN: usize = 16;

/// Whether `text` is a secret key: `csk_` followed by 32 bytes in base62 (alphabet `0-9A-Za-z`, a fixed 43
/// characters, left-padded with `0`). Allocates nothing.
fn well_formed(text: &str) -> bool {
    let Some(body) = text.strip_prefix(SECRET_PREFIX) else {
        return false;
    };
    let body = body.as_bytes();
    body.len() == BODY_LEN
        && body.iter().all(u8::is_ascii_alphanumeric)
        && body <= MAX_BODY.as_slice()
}

/// A secret key in the contract's format. Its `Debug` output never shows the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey(String);

/// Text that is not a secret key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a secret key: expected `csk_` followed by 43 base62 characters encoding 32 bytes")]
pub struct MalformedKey;

impl SecretKey {
    /// Parses a secret key.
    pub fn parse(text: &str) -> Result<Self, MalformedKey> {
        if well_formed(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(MalformedKey)
        }
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(..)")
    }
}

/// The project principal of a verified key (contract 11.3): the project the request may act on and the key
/// that authenticated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPrincipal {
    /// The key's project.
    pub scope: ProjectScope,
    /// The key.
    pub api_key_id: ApiKeyId,
}

/// Verifies a secret key presented as a Bearer token.
pub trait KeyVerifier: fmt::Debug + Send + Sync {
    /// The principal of `secret`, or `None` when it is malformed, unknown or wrong.
    fn verify(&self, secret: &str) -> Option<KeyPrincipal>;
}

/// A project key from configuration.
#[derive(Debug, Clone)]
pub struct StaticKey {
    /// The key's id, recorded with every request it authenticates.
    pub id: ApiKeyId,
    /// The project the key belongs to.
    pub project_id: ProjectId,
    /// The secret.
    pub secret: SecretKey,
}

/// Why a set of configured keys is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyConfigError {
    /// The nil project id cannot own keys (contract 1.1).
    #[error("key {0} belongs to the nil project id")]
    NilProject(ApiKeyId),
    /// Two keys share a secret, so a request could not tell which one it presented.
    #[error("keys {0} and {1} have the same secret")]
    DuplicateSecret(ApiKeyId, ApiKeyId),
}

/// One configured key as it is kept: a salted hash instead of the secret.
struct Stored {
    id: ApiKeyId,
    project_id: ProjectId,
    salt: [u8; SALT_LEN],
    hash: [u8; 32],
}

/// The keys from configuration. Each secret is kept only as `SHA-256(salt || secret)` with its own random
/// 16-byte salt (contract 11.3); the secret itself is dropped.
pub struct StaticKeys {
    keys: Vec<Stored>,
}

impl fmt::Debug for StaticKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for key in &self.keys {
            list.entry(&(key.id, key.project_id));
        }
        list.finish()
    }
}

fn salted_hash(salt: &[u8; SALT_LEN], secret: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(secret);
    hasher.finalize().into()
}

impl StaticKeys {
    /// Hashes each key's secret with a fresh salt drawn from `rng`, which should be a fork labelled for this
    /// use. Refuses a key of the nil project and two keys with one secret.
    pub fn new(rng: &dyn Rng, keys: Vec<StaticKey>) -> Result<Self, KeyConfigError> {
        for (index, key) in keys.iter().enumerate() {
            if key.project_id.is_nil() {
                return Err(KeyConfigError::NilProject(key.id));
            }
            if let Some(earlier) = keys[..index].iter().find(|k| k.secret == key.secret) {
                return Err(KeyConfigError::DuplicateSecret(earlier.id, key.id));
            }
        }
        let keys = keys
            .into_iter()
            .map(|key| {
                let mut salt = [0; SALT_LEN];
                rng.fill_bytes(&mut salt);
                Stored {
                    id: key.id,
                    project_id: key.project_id,
                    hash: salted_hash(&salt, key.secret.0.as_bytes()),
                    salt,
                }
            })
            .collect();
        Ok(Self { keys })
    }

    /// The number of keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no key is configured, so that every request is refused.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl KeyVerifier for StaticKeys {
    /// Parses the secret, then hashes it with every key's salt and compares in constant time. Every key is
    /// checked, so the time taken does not depend on which key matches. Allocates nothing.
    fn verify(&self, secret: &str) -> Option<KeyPrincipal> {
        if !well_formed(secret) {
            return None;
        }
        let mut found = None;
        for key in &self.keys {
            let hash = salted_hash(&key.salt, secret.as_bytes());
            if bool::from(hash.as_slice().ct_eq(key.hash.as_slice())) {
                found = Some(key);
            }
        }
        let key = found?;
        // `new` refused the nil project, the only id `mint` refuses.
        let scope = scope::mint(key.project_id).ok()?;
        Some(KeyPrincipal {
            scope,
            api_key_id: key.id,
        })
    }
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;
    use proptest::prelude::*;

    use super::*;

    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

    /// The base62 body of a 256-bit number given as four big-endian 64-bit limbs.
    fn encode(mut limbs: [u64; 4]) -> String {
        let mut digits = [b'0'; BODY_LEN];
        for digit in digits.iter_mut().rev() {
            let mut rest = 0u128;
            for limb in &mut limbs {
                let value = (rest << 64) | u128::from(*limb);
                *limb = (value / 62) as u64;
                rest = value % 62;
            }
            *digit = ALPHABET[rest as usize];
        }
        assert_eq!(limbs, [0; 4], "the value needs more than 43 digits");
        String::from_utf8(digits.to_vec()).unwrap()
    }

    /// The value of a base62 body, or `None` when it does not fit in 256 bits.
    fn decode(body: &[u8]) -> Option<[u64; 4]> {
        let mut limbs = [0u64; 4];
        for c in body {
            let mut carry = ALPHABET.iter().position(|a| a == c)? as u128;
            for limb in limbs.iter_mut().rev() {
                let value = u128::from(*limb) * 62 + carry;
                *limb = value as u64;
                carry = value >> 64;
            }
            if carry != 0 {
                return None;
            }
        }
        Some(limbs)
    }

    fn key_id(n: u8) -> ApiKeyId {
        ApiKeyId::from_bytes([n; 16])
    }

    fn project(n: u8) -> ProjectId {
        ProjectId::from_bytes([n; 16])
    }

    fn secret(limbs: [u64; 4]) -> SecretKey {
        SecretKey::parse(&format!("csk_{}", encode(limbs))).unwrap()
    }

    #[test]
    fn the_largest_body_is_two_to_the_256_minus_one() {
        assert_eq!(decode(MAX_BODY), Some([u64::MAX; 4]));
        assert_eq!(encode([u64::MAX; 4]).as_bytes(), MAX_BODY);
        assert_eq!(encode([0; 4]), "0".repeat(BODY_LEN));
    }

    #[test]
    fn the_format_is_exact() {
        let max = format!("csk_{}", core::str::from_utf8(MAX_BODY).unwrap());
        let zero = format!("csk_{}", "0".repeat(BODY_LEN));
        assert!(well_formed(&max) && well_formed(&zero));
        let above_max = max.replace("Xp1", "Xp2");
        assert_eq!(decode(&above_max.as_bytes()[4..]), None);
        let short = &zero[..zero.len() - 1];
        let long = format!("{zero}0");
        let public = zero.replace("csk_", "cpk_");
        let upper = zero.replace("csk_", "CSK_");
        let symbol = zero.replacen('0', "-", 1);
        let space = format!(" {zero}");
        for bad in [
            &above_max, short, &long, &public, &upper, &symbol, &space, "", "csk_",
        ] {
            assert!(!well_formed(bad), "{bad:?}");
            assert_eq!(SecretKey::parse(bad), Err(MalformedKey));
        }
        assert_eq!(
            format!("{:?}", SecretKey::parse(&zero).unwrap()),
            "SecretKey(..)"
        );
    }

    #[test]
    fn the_stored_hash_is_sha256_of_the_salt_then_the_secret() {
        let salt: [u8; SALT_LEN] = core::array::from_fn(|i| i as u8);
        let secret = format!("csk_{}1", "0".repeat(BODY_LEN - 1));
        let hex: String = salted_hash(&salt, secret.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "7beda4c0fd471b7171d5cefc3e8492f4af080c6659ae739bb022a5da207615f2"
        );
    }

    #[test]
    fn configuration_refuses_the_nil_project_and_shared_secrets() {
        let rng = ChaChaRng::from_u64(1);
        let key = |id, project_id, s| StaticKey {
            id: key_id(id),
            project_id: project(project_id),
            secret: secret([0, 0, 0, s]),
        };
        assert_eq!(
            StaticKeys::new(&rng, vec![key(1, 1, 1), key(2, 0, 2)]).unwrap_err(),
            KeyConfigError::NilProject(key_id(2))
        );
        assert_eq!(
            StaticKeys::new(&rng, vec![key(1, 1, 1), key(2, 2, 2), key(3, 3, 1)]).unwrap_err(),
            KeyConfigError::DuplicateSecret(key_id(1), key_id(3))
        );
        let keys = StaticKeys::new(&rng, vec![key(1, 1, 1), key(2, 1, 2)]).unwrap();
        assert_eq!(keys.len(), 2);
        // Debug output names keys and projects and nothing derived from a secret.
        assert_eq!(
            format!("{keys:?}"),
            format!(
                "[({:?}, {:?}), ({:?}, {:?})]",
                key_id(1),
                project(1),
                key_id(2),
                project(1)
            )
        );
        let empty = StaticKeys::new(&rng, Vec::new()).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.verify(&format!("csk_{}", encode([0, 0, 0, 1]))), None);
    }

    fn limbs() -> impl Strategy<Value = [u64; 4]> {
        prop::array::uniform4(any::<u64>())
    }

    proptest! {
        /// Every configured secret verifies to its own key and project; a secret that differs in one
        /// character, an unknown secret and malformed text verify to nothing.
        #[test]
        fn secrets_verify_to_their_own_key(
            seed: u64,
            secrets in prop::collection::btree_set(limbs(), 1..6),
            unknown in limbs(),
            flip in any::<prop::sample::Index>(),
        ) {
            let secrets: Vec<SecretKey> = secrets.into_iter().map(secret).collect();
            let configured: Vec<StaticKey> = secrets
                .iter()
                .enumerate()
                .map(|(i, secret)| StaticKey {
                    id: key_id(i as u8 + 1),
                    project_id: project(i as u8 % 2 + 1),
                    secret: secret.clone(),
                })
                .collect();
            let keys = StaticKeys::new(&ChaChaRng::from_u64(seed), configured.clone()).unwrap();
            for key in &configured {
                let principal = keys.verify(&key.secret.0).unwrap();
                prop_assert_eq!(principal.api_key_id, key.id);
                prop_assert_eq!(principal.scope.project_id(), key.project_id);

                let mut changed = key.secret.0.clone().into_bytes();
                let at = 4 + flip.index(BODY_LEN);
                changed[at] = if changed[at] == b'0' { b'1' } else { b'0' };
                let changed = String::from_utf8(changed).unwrap();
                if !configured.iter().any(|k| k.secret.0 == changed) {
                    prop_assert_eq!(keys.verify(&changed), None);
                }
                prop_assert_eq!(keys.verify(&key.secret.0[1..]), None);
                prop_assert_eq!(keys.verify(&key.secret.0.replace("csk_", "cpk_")), None);
            }
            let unknown = secret(unknown);
            if !configured.iter().any(|k| k.secret == unknown) {
                prop_assert_eq!(keys.verify(&unknown.0), None);
            }
        }

        /// Base62 bodies of 256-bit values are well formed and decode to the value.
        #[test]
        fn every_256_bit_value_is_a_well_formed_body(value in limbs()) {
            let body = encode(value);
            let key = format!("csk_{body}");
            prop_assert!(well_formed(&key));
            prop_assert_eq!(decode(body.as_bytes()), Some(value));
        }
    }
}
