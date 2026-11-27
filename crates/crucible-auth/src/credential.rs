//! The credential of an `Authorization` header (contract 12.2): a secret key as `Bearer`, or a public and
//! secret key pair as `Basic`, and why one is refused.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::GeneralPurpose;
use base64::engine::general_purpose::PAD_INDIFFERENT;

use crate::db_keys::ScopeGap;
use crate::key::{
    KEY_LEN, KeyPrincipal, KeyVerifier, PUBLIC_PREFIX, PairError, SECRET_PREFIX, well_formed,
};

/// The standard base64 alphabet of Basic credentials (RFC 7617), with or without padding.
const BASE64: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, PAD_INDIFFERENT);

/// The longest decoded Basic credential that is read: room for a public key, `:` and a secret key, and for
/// a mistaken pair of similar length, so that its refusal can say what is wrong.
const BASIC_MAX: usize = 4 * KEY_LEN;

/// The longest base64 text that decodes to at most [`BASIC_MAX`] bytes.
const BASIC_ENCODED_MAX: usize = BASIC_MAX.div_ceil(3) * 4;

/// Why a request's credential is refused. Each message says how to fix the request; none carries key
/// material, so a refusal never echoes a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The request has no `Authorization` header.
    #[error(
        "send the project's secret key as `Authorization: Bearer <secret key>`, or its public and secret key \
         pair as `Authorization: Basic base64(<public key>:<secret key>)`"
    )]
    Missing,
    /// A scheme other than Bearer and Basic, or no credential after the scheme.
    #[error(
        "the Authorization header must be `Bearer <secret key>` or `Basic base64(<public key>:<secret key>)`"
    )]
    Malformed,
    /// A Basic credential that is not base64 text of `<public key>:<secret key>`.
    #[error(
        "the Basic credential must be the base64 encoding of `<public key>:<secret key>`, with the public key \
         (cpk_...) as the username and the secret key (csk_...) as the password"
    )]
    MalformedBasic,
    /// A public key without its secret key: Bearer with a public key, or Basic with an empty password.
    #[error(
        "a public key alone is not accepted here; send its secret key too, as \
         `Basic base64(<public key>:<secret key>)` or `Bearer <secret key>`"
    )]
    PublicKeyOnly,
    /// Basic with an empty username.
    #[error(
        "the Basic username is empty; it must be the public key (cpk_...) shown together with the secret key"
    )]
    MissingPublicKey,
    /// The secret key is not in the key format.
    #[error(
        "the secret key is not a Crucible secret key, which starts with `csk_` and has 43 more characters; in \
         Basic auth the secret key is the password and the public key (cpk_...) the username"
    )]
    MalformedSecret,
    /// A well-formed secret key that this server does not accept.
    #[error(
        "this server does not accept the secret key; check that it was copied whole and that the server lists it"
    )]
    UnknownSecret,
    /// The server cannot check the key now: Postgres is unreachable and the key is not in the server's snapshot
    /// (contract 11.8). Answered with 503 and `Retry-After`.
    #[error(
        "the server cannot check this key right now because its control plane is unreachable; try again shortly"
    )]
    Unavailable,
    /// The secret key is valid and the public key is not its pair's.
    #[error(
        "the public key does not belong to the same project as the secret key; use the key pair shown together \
         in Settings"
    )]
    PairMismatch,
    /// The key is valid and lacks the scope this route needs (contract 11.3). Answered with 403.
    #[error("{0}")]
    MissingScope(ScopeGap),
}

/// Verifies the value of an `Authorization` header (contract 12.2):
///
/// - `Bearer <secret key>`, verified by [`KeyVerifier::verify`]; a public key there is refused;
/// - `Basic base64(<public key>:<secret key>)`, split at the first `:`, verified by
///   [`KeyVerifier::verify_pair`]: the secret finds the key, and the public key must be that key's. An empty
///   username or password is refused.
///
/// Scheme names ignore case and the credential ignores surrounding spaces. Decoding happens on the stack, so
/// nothing is allocated.
pub fn authenticate(keys: &dyn KeyVerifier, authorization: &str) -> Result<KeyPrincipal, Refusal> {
    let (scheme, credential) = authorization
        .trim()
        .split_once(' ')
        .ok_or(Refusal::Malformed)?;
    let credential = credential.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        bearer(keys, credential)
    } else if scheme.eq_ignore_ascii_case("basic") {
        basic(keys, credential)
    } else {
        Err(Refusal::Malformed)
    }
}

/// The refusal of a secret that did not verify: [`Refusal::Unavailable`] when the verifier cannot decide now,
/// [`Refusal::MissingScope`] when its key is valid and lacks the verifier's scope.
fn undecided(keys: &dyn KeyVerifier, secret: &str) -> Refusal {
    if keys.retry_after(secret).is_some() {
        Refusal::Unavailable
    } else if let Some(gap) = keys.missing_scope(secret) {
        Refusal::MissingScope(gap)
    } else {
        Refusal::UnknownSecret
    }
}

fn bearer(keys: &dyn KeyVerifier, token: &str) -> Result<KeyPrincipal, Refusal> {
    if token.starts_with(PUBLIC_PREFIX) {
        return Err(Refusal::PublicKeyOnly);
    }
    if !well_formed(token, SECRET_PREFIX) {
        return Err(Refusal::MalformedSecret);
    }
    keys.verify(token).ok_or_else(|| undecided(keys, token))
}

fn basic(keys: &dyn KeyVerifier, encoded: &str) -> Result<KeyPrincipal, Refusal> {
    if encoded.len() > BASIC_ENCODED_MAX {
        return Err(Refusal::MalformedBasic);
    }
    let mut buffer = [0; BASIC_MAX];
    let len = BASE64
        .decode_slice(encoded, &mut buffer)
        .map_err(|_| Refusal::MalformedBasic)?;
    let decoded = buffer
        .get(..len)
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .ok_or(Refusal::MalformedBasic)?;
    let (public, secret) = decoded.split_once(':').ok_or(Refusal::MalformedBasic)?;
    if public.is_empty() {
        return Err(Refusal::MissingPublicKey);
    }
    if secret.is_empty() {
        return Err(Refusal::PublicKeyOnly);
    }
    if !well_formed(secret, SECRET_PREFIX) {
        return Err(Refusal::MalformedSecret);
    }
    keys.verify_pair(public, secret).map_err(|e| match e {
        PairError::Secret => undecided(keys, secret),
        PairError::Mismatch => Refusal::PairMismatch,
    })
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;
    use crucible_records::{ApiKeyId, ProjectId};
    use proptest::prelude::*;

    use super::*;
    use crate::{PublicKey, SecretKey, StaticKey, StaticKeys};

    fn public(n: char) -> String {
        format!("cpk_{}", n.to_string().repeat(43))
    }

    fn secret(n: char) -> String {
        format!("csk_{}", n.to_string().repeat(43))
    }

    /// Keys 1 and 2 of project 1, and key 3 of project 2.
    fn keys() -> StaticKeys {
        let key = |n: u8, project: u8| {
            let digit = char::from(b'0' + n);
            StaticKey {
                id: ApiKeyId::from_bytes([n; 16]),
                project_id: ProjectId::from_bytes([project; 16]),
                public_key: PublicKey::parse(&public(digit)).unwrap(),
                secret: SecretKey::parse(&secret(digit)).unwrap(),
            }
        };
        StaticKeys::new(
            &ChaChaRng::from_u64(3),
            vec![key(1, 1), key(2, 1), key(3, 2)],
        )
        .unwrap()
    }

    fn basic(credential: &str) -> String {
        format!("Basic {}", BASE64.encode(credential))
    }

    fn key_of(result: Result<KeyPrincipal, Refusal>) -> Result<u8, Refusal> {
        result.map(|principal| principal.api_key_id.to_bytes()[0])
    }

    #[test]
    fn bearer_and_basic_find_the_same_key() {
        let keys = keys();
        let check = |value: &str| key_of(authenticate(&keys, value));
        assert_eq!(check(&format!("Bearer {}", secret('1'))), Ok(1));
        assert_eq!(check(&format!("bearer  {} ", secret('2'))), Ok(2));
        assert_eq!(
            check(&basic(&format!("{}:{}", public('1'), secret('1')))),
            Ok(1)
        );
        assert_eq!(
            check(&basic(&format!("{}:{}", public('3'), secret('3')))),
            Ok(3)
        );
        // Padding is optional, and the scheme ignores case.
        let pair = format!("{}:{}", public('2'), secret('2'));
        let unpadded = BASE64.encode(&pair).trim_end_matches('=').to_owned();
        assert_ne!(unpadded, BASE64.encode(&pair), "the pair needs padding");
        assert_eq!(check(&format!("BASIC {unpadded}")), Ok(2));
    }

    #[test]
    fn every_refusal_is_typed() {
        let keys = keys();
        let check = |value: &str| key_of(authenticate(&keys, value));
        let (p1, s1, s2, p3) = (public('1'), secret('1'), secret('2'), public('3'));
        let unknown = secret('9');
        let long = "A".repeat(BASIC_ENCODED_MAX + 4);
        for (value, refusal) in [
            // The pair must belong together, whether the other key is in the same project or not.
            (basic(&format!("{p1}:{s2}")), Refusal::PairMismatch),
            (basic(&format!("{p3}:{s1}")), Refusal::PairMismatch),
            (
                basic(&format!("pk-upstream-key:{s1}")),
                Refusal::PairMismatch,
            ),
            // A public key alone.
            (format!("Bearer {p1}"), Refusal::PublicKeyOnly),
            (basic(&format!("{p1}:")), Refusal::PublicKeyOnly),
            // Empty parts.
            (basic(&format!(":{s1}")), Refusal::MissingPublicKey),
            (basic(":"), Refusal::MissingPublicKey),
            // Malformed credentials.
            (basic(&format!("{p1}{s1}")), Refusal::MalformedBasic),
            ("Basic not*base64".to_owned(), Refusal::MalformedBasic),
            (
                format!("Basic {}", BASE64.encode([b'a', b':', 0xff])),
                Refusal::MalformedBasic,
            ),
            (format!("Basic {long}"), Refusal::MalformedBasic),
            ("Basic".to_owned(), Refusal::Malformed),
            ("Bearer".to_owned(), Refusal::Malformed),
            (s1.clone(), Refusal::Malformed),
            (format!("Token {s1}"), Refusal::Malformed),
            // Secrets that are not keys, or not this server's.
            (basic(&format!("{s1}:{p1}")), Refusal::MalformedSecret),
            (
                basic(&format!("{p1}:sk-upstream-key")),
                Refusal::MalformedSecret,
            ),
            (format!("Bearer {}", &s1[..20]), Refusal::MalformedSecret),
            (format!("Bearer {unknown}"), Refusal::UnknownSecret),
            (basic(&format!("{p1}:{unknown}")), Refusal::UnknownSecret),
        ] {
            assert_eq!(check(&value), Err(refusal), "{value}");
        }
    }

    #[test]
    fn no_refusal_names_a_key() {
        let messages = [
            Refusal::Missing,
            Refusal::Malformed,
            Refusal::MalformedBasic,
            Refusal::PublicKeyOnly,
            Refusal::MissingPublicKey,
            Refusal::MalformedSecret,
            Refusal::UnknownSecret,
            Refusal::PairMismatch,
            Refusal::MissingScope(ScopeGap {
                needed: crucible_control::KeyScope::Sql,
                has: crate::Scopes::DEFAULT,
            }),
        ]
        .map(|refusal| refusal.to_string());
        for message in &messages {
            for prefix in ["cpk_", "csk_"] {
                for (at, _) in message.match_indices(prefix) {
                    let rest = &message[at + prefix.len()..];
                    assert!(
                        !rest.starts_with(|c: char| c.is_ascii_alphanumeric()),
                        "{message}"
                    );
                }
            }
        }
        assert_eq!(
            messages[7],
            "the public key does not belong to the same project as the secret key; use the key pair shown \
             together in Settings"
        );
    }

    proptest! {
        /// Arbitrary header values and Basic credentials of arbitrary bytes are refused without a panic.
        #[test]
        fn arbitrary_credentials_are_refused(
            value in "\\PC{0,200}",
            raw in prop::collection::vec(any::<u8>(), 0..2 * BASIC_MAX),
        ) {
            let keys = keys();
            prop_assert!(authenticate(&keys, &value).is_err());
            let encoded = format!("Basic {}", BASE64.encode(&raw));
            prop_assert!(authenticate(&keys, &encoded).is_err());
        }

        /// A Basic pair of any two of the keys' public keys and secrets verifies exactly when both are one
        /// key's, and is otherwise a mismatch.
        #[test]
        fn basic_pairs_verify_exactly_when_they_belong_together(p in 1..4u8, s in 1..4u8) {
            let keys = keys();
            let (p, s) = (char::from(b'0' + p), char::from(b'0' + s));
            let result = key_of(authenticate(&keys, &basic(&format!("{}:{}", public(p), secret(s)))));
            if p == s {
                prop_assert_eq!(result, Ok(s as u8 - b'0'));
            } else {
                prop_assert_eq!(result, Err(Refusal::PairMismatch));
            }
        }
    }
}
