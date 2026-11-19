//! Opaque tokens (contract 11.1 and 11.4): session tokens, CSRF tokens, one-time link tokens and OAuth `state`.
//! Each is 256 random bits from `Rng` written as unpadded base64url (43 characters). Only the SHA-256 of a
//! session, one-time or state token is stored.

use core::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crucible_control::TokenHash;
use crucible_env::Rng;
use sha2::{Digest, Sha256};

/// The length of a token's text.
pub const TOKEN_LEN: usize = 43;

/// A fresh token. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// 256 random bits from `rng`.
    pub fn new(rng: &dyn Rng) -> Self {
        let mut bytes = [0; 32];
        rng.fill_bytes(&mut bytes);
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// The token's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The SHA-256 of the token's text, as stored.
    pub fn hash(&self) -> TokenHash {
        hash(&self.0)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// The SHA-256 of a token's text.
pub fn hash(text: &str) -> TokenHash {
    Sha256::digest(text.as_bytes()).into()
}

/// Whether `text` has the shape of a token: 43 base64url characters. Presented text that does not is refused
/// before anything is looked up.
pub fn well_formed(text: &str) -> bool {
    text.len() == TOKEN_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// Every token is well formed, hashes to the SHA-256 of its text, and differs from the next one.
        #[test]
        fn tokens_are_43_base64url_characters(seed: u64) {
            let rng = ChaChaRng::from_u64(seed);
            let (a, b) = (Token::new(&rng), Token::new(&rng));
            prop_assert!(well_formed(a.as_str()) && well_formed(b.as_str()));
            prop_assert_ne!(&a, &b);
            prop_assert_eq!(a.hash(), hash(a.as_str()));
            prop_assert_eq!(format!("{a:?}"), "Token(..)");
            let mut shorter = a.as_str().to_owned();
            shorter.pop();
            prop_assert!(!well_formed(&shorter));
            let (padded, plus) = (format!("{shorter}="), format!("{shorter}+"));
            prop_assert!(!well_formed(&padded));
            prop_assert!(!well_formed(&plus));
        }
    }
}
