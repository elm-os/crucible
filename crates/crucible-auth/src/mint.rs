//! New API keys (contract 11.3): a public key `cpk_` and a secret key `csk_`, each followed by 32 random bytes in
//! base62 (alphabet `0-9A-Za-z`, a fixed 43 characters, left-padded with `0`). The secret is shown once; only a
//! salted SHA-256 of it is stored, with a lookup tag that lets a Bearer secret find its row without the public key.

use core::fmt;

use crucible_env::Rng;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::{Digest, Sha256};

/// The base62 alphabet, in ASCII order.
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// The characters of a key body.
const BODY_LEN: usize = 43;

/// The base62 text of a 256-bit big-endian number, 43 characters left-padded with `0`.
fn base62(mut value: [u8; 32]) -> String {
    let mut digits = [b'0'; BODY_LEN];
    for digit in digits.iter_mut().rev() {
        let mut rest = 0u32;
        for byte in &mut value {
            let current = (rest << 8) | u32::from(*byte);
            *byte = (current / 62) as u8;
            rest = current % 62;
        }
        *digit = ALPHABET[rest as usize];
    }
    digits.iter().map(|&b| char::from(b)).collect()
}

/// `SHA-256(salt || secret key string)` (contract 11.3).
pub fn secret_hash(salt: &[u8; 16], secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(secret.as_bytes());
    hasher.finalize().into()
}

/// The lookup subkey: the first 8 bytes of `HMAC-SHA256(lookup subkey, secret key string)` find a Bearer
/// secret's row (contract 11.3). The binary derives the subkey from the master key.
#[derive(Clone)]
pub struct LookupKey([u8; 32]);

impl fmt::Debug for LookupKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LookupKey(..)")
    }
}

impl LookupKey {
    /// The subkey.
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    /// The lookup tag of a secret key string. Allocates nothing.
    pub fn tag(&self, secret: &str) -> [u8; 8] {
        // HMAC pads a key shorter than the block with zeros, so this is HMAC under the 32-byte subkey.
        let mut block = [0; 64];
        block[..32].copy_from_slice(&self.0);
        let mut mac = Hmac::<Sha256>::new(&block.into());
        mac.update(secret.as_bytes());
        let full = mac.finalize().into_bytes();
        let mut tag = [0; 8];
        tag.copy_from_slice(&full[..8]);
        tag
    }
}

/// A new key pair: the strings the user sees once, and what is stored.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedKey {
    /// `cpk_...`; stored.
    pub public_key: String,
    /// `csk_...`; shown once, never stored.
    pub secret: String,
    /// 16 random bytes.
    pub salt: [u8; 16],
    /// `SHA-256(salt || secret)`.
    pub hash: [u8; 32],
    /// The lookup tag of the secret.
    pub lookup_tag: [u8; 8],
}

impl fmt::Debug for MintedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintedKey")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// Mints a key pair from `rng`.
pub fn mint(rng: &dyn Rng, lookup: &LookupKey) -> MintedKey {
    let mut public = [0; 32];
    rng.fill_bytes(&mut public);
    let mut secret = [0; 32];
    rng.fill_bytes(&mut secret);
    let mut salt = [0; 16];
    rng.fill_bytes(&mut salt);
    let secret = format!("csk_{}", base62(secret));
    MintedKey {
        public_key: format!("cpk_{}", base62(public)),
        hash: secret_hash(&salt, &secret),
        lookup_tag: lookup.tag(&secret),
        secret,
        salt,
    }
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;
    use proptest::prelude::*;

    use super::*;
    use crate::SecretKey;

    #[test]
    fn base62_is_fixed_width_and_in_order() {
        assert_eq!(base62([0; 32]), "0".repeat(43));
        let mut one = [0; 32];
        one[31] = 1;
        assert_eq!(base62(one), format!("{}1", "0".repeat(42)));
        one[31] = 62;
        assert_eq!(base62(one), format!("{}10", "0".repeat(41)));
        assert_eq!(
            base62([0xff; 32]),
            "yhjskwdA6OZ1AL1YmHWZWm8LLG7HjnuCA2j5rOw8Xp1"
        );
    }

    #[test]
    fn the_lookup_tag_is_hmac_sha256_truncated() {
        let tag = LookupKey::new([1; 32]).tag("csk_x");
        let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&[1; 32]).unwrap();
        mac.update(b"csk_x");
        assert_eq!(tag[..], mac.finalize().into_bytes()[..8]);
        assert_ne!(tag, LookupKey::new([2; 32]).tag("csk_x"));
        assert_eq!(format!("{:?}", LookupKey::new([1; 32])), "LookupKey(..)");
    }

    proptest! {
        /// Minted keys have the contract's format, hash to their stored hash, and differ from each other.
        #[test]
        fn minted_keys_are_well_formed(seed: u64, bytes: [u8; 32]) {
            let rng = ChaChaRng::from_u64(seed);
            let lookup = LookupKey::new([3; 32]);
            let (a, b) = (mint(&rng, &lookup), mint(&rng, &lookup));
            prop_assert!(SecretKey::parse(&a.secret).is_ok());
            prop_assert!(a.public_key.starts_with("cpk_") && a.public_key.len() == 47);
            prop_assert!(a.public_key[4..].bytes().all(|c| c.is_ascii_alphanumeric()));
            prop_assert_eq!(a.hash, secret_hash(&a.salt, &a.secret));
            prop_assert_eq!(a.lookup_tag, lookup.tag(&a.secret));
            prop_assert_ne!(&a.secret, &b.secret);
            prop_assert_ne!(&a.public_key, &b.public_key);
            let shown = format!("{a:?}");
            prop_assert!(!shown.contains(&a.secret));
            let body = base62(bytes);
            prop_assert_eq!(body.len(), 43);
            let secret = format!("csk_{body}");
            prop_assert!(SecretKey::parse(&secret).is_ok());
        }
    }
}
