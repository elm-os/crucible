//! Secrets at rest (contract 11.2): XChaCha20-Poly1305 under a subkey of the master key, with a random nonce
//! from `Rng` and associated data that binds each sealed value to where it is stored.

use core::fmt;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use crucible_control::SealedProjectSalt;
use crucible_env::Rng;
use crucible_records::ProjectId;

/// The length of a nonce.
const NONCE_LEN: usize = 24;

/// Seals and opens secrets with one subkey of the master key.
pub struct Sealer {
    key_id: String,
    cipher: XChaCha20Poly1305,
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// A sealed value: the nonce, then the ciphertext with its tag.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The id of the master key that sealed it.
    pub key_id: String,
    /// The nonce.
    pub nonce: Vec<u8>,
    /// The ciphertext with its tag.
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// The associated data that binds a sealed project salt to its project. Salts sealed since the first version of
/// `project_salts` carry exactly these bytes.
fn salt_aad(project: ProjectId) -> Vec<u8> {
    format!("{}\0project_salt", project.uuid()).into_bytes()
}

/// A sealed value that does not open: another key sealed it, its associated data differs, or it was altered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "a sealed secret does not open: it was sealed under another master key or altered; restore the master key \
     it was sealed with"
)]
pub struct Unsealable;

impl Sealer {
    /// A sealer over `key`, a 32-byte subkey of the master key named `key_id`.
    pub fn new(key_id: &str, key: [u8; 32]) -> Self {
        Self {
            key_id: key_id.to_owned(),
            cipher: XChaCha20Poly1305::new(&key.into()),
        }
    }

    /// The id of the master key this sealer seals under.
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Seals `plaintext` bound to `aad`, under a fresh nonce from `rng`.
    pub fn seal(&self, rng: &dyn Rng, aad: &[u8], plaintext: &[u8]) -> Sealed {
        let mut nonce = [0; NONCE_LEN];
        rng.fill_bytes(&mut nonce);
        let payload = Payload {
            msg: plaintext,
            aad,
        };
        // Encryption fails only for messages above 256 GiB.
        let ciphertext = self
            .cipher
            .encrypt(&XNonce::from(nonce), payload)
            .unwrap_or_default();
        Sealed {
            key_id: self.key_id.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        }
    }

    /// Opens a sealed value bound to `aad`.
    pub fn open(&self, sealed: &Sealed, aad: &[u8]) -> Result<Vec<u8>, Unsealable> {
        if sealed.key_id != self.key_id {
            return Err(Unsealable);
        }
        let nonce: [u8; NONCE_LEN] = sealed.nonce.as_slice().try_into().map_err(|_| Unsealable)?;
        let payload = Payload {
            msg: &sealed.ciphertext,
            aad,
        };
        self.cipher
            .decrypt(&XNonce::from(nonce), payload)
            .map_err(|_| Unsealable)
    }

    /// Seals into one byte string, the nonce first, for a column that holds the whole sealed value (the PKCE
    /// verifier of a sign-in flow).
    pub fn seal_bytes(&self, rng: &dyn Rng, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let sealed = self.seal(rng, aad, plaintext);
        [sealed.nonce, sealed.ciphertext].concat()
    }

    /// A new salt for `project` (contract 9.3): 32 random bytes from `rng`, sealed and bound to the project, as
    /// its `project_salts` row holds it. A project gets its salt once, when it is created, and never another.
    pub fn new_project_salt(&self, rng: &dyn Rng, project: ProjectId) -> SealedProjectSalt {
        let mut salt = [0u8; 32];
        rng.fill_bytes(&mut salt);
        let sealed = self.seal(rng, &salt_aad(project), &salt);
        SealedProjectSalt {
            project_id: project,
            key_id: sealed.key_id,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        }
    }

    /// The 32 bytes of a stored project salt.
    pub fn open_project_salt(&self, row: &SealedProjectSalt) -> Result<[u8; 32], Unsealable> {
        let sealed = Sealed {
            key_id: row.key_id.clone(),
            nonce: row.nonce.clone(),
            ciphertext: row.ciphertext.clone(),
        };
        self.open(&sealed, &salt_aad(row.project_id))?
            .try_into()
            .map_err(|_| Unsealable)
    }

    /// Opens what [`Sealer::seal_bytes`] sealed.
    pub fn open_bytes(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, Unsealable> {
        let (nonce, ciphertext) = sealed.split_at_checked(NONCE_LEN).ok_or(Unsealable)?;
        self.open(
            &Sealed {
                key_id: self.key_id.clone(),
                nonce: nonce.to_vec(),
                ciphertext: ciphertext.to_vec(),
            },
            aad,
        )
    }
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// A sealed value opens to its plaintext under the same key and associated data, and under nothing else.
        #[test]
        fn sealed_values_open_only_where_they_were_sealed(
            seed: u64,
            key: [u8; 32],
            other_key: [u8; 32],
            aad in prop::collection::vec(any::<u8>(), 0..40),
            other_aad in prop::collection::vec(any::<u8>(), 0..40),
            plaintext in prop::collection::vec(any::<u8>(), 0..200),
            flip in any::<prop::sample::Index>(),
        ) {
            let rng = ChaChaRng::from_u64(seed);
            let sealer = Sealer::new("master-0", key);
            let sealed = sealer.seal(&rng, &aad, &plaintext);
            prop_assert_eq!(sealed.nonce.len(), NONCE_LEN);
            prop_assert_eq!(sealer.open(&sealed, &aad), Ok(plaintext.clone()));
            if other_aad != aad {
                prop_assert_eq!(sealer.open(&sealed, &other_aad), Err(Unsealable));
            }
            if other_key != key {
                let other = Sealer::new("master-0", other_key);
                prop_assert_eq!(other.open(&sealed, &aad), Err(Unsealable));
            }
            prop_assert_eq!(Sealer::new("master-1", key).open(&sealed, &aad), Err(Unsealable));
            let mut altered = sealed.clone();
            let at = flip.index(altered.ciphertext.len());
            altered.ciphertext[at] ^= 1;
            prop_assert_eq!(sealer.open(&altered, &aad), Err(Unsealable));

            let bytes = sealer.seal_bytes(&rng, &aad, &plaintext);
            prop_assert_eq!(sealer.open_bytes(&aad, &bytes), Ok(plaintext.clone()));
            prop_assert_eq!(sealer.open_bytes(&aad, &bytes[..NONCE_LEN - 1]), Err(Unsealable));
        }
    }

    #[test]
    fn two_seals_of_one_value_differ_and_nothing_prints_the_key() {
        let rng = ChaChaRng::from_u64(7);
        let sealer = Sealer::new("master-0", [9; 32]);
        let (a, b) = (sealer.seal(&rng, b"x", b"v"), sealer.seal(&rng, b"x", b"v"));
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
        assert!(!format!("{sealer:?}{a:?}").contains("9, 9"));
    }
}
