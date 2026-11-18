//! Authentication and authorization (contract section 11).
//!
//! This crate owns minting of `ProjectScope` values: [`KeyVerifier::verify`] and [`KeyVerifier::verify_pair`]
//! are how a request obtains one, and [`authenticate`] reads the `Authorization` forms of contract 12.2 into
//! them. In milestone 1 the verifier is [`StaticKeys`], the project keys listed in configuration. Sessions,
//! roles, the key snapshot and the key feed arrive with the platform milestone.

mod credential;
mod key;
mod scope;

pub use credential::{Refusal, authenticate};
pub use key::{
    KeyConfigError, KeyPrincipal, KeyVerifier, MalformedKey, MalformedPublicKey, PairError,
    PublicKey, SecretKey, StaticKey, StaticKeys,
};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
