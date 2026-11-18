//! Authentication and authorization (contract section 11).
//!
//! This crate owns minting of `ProjectScope` values: [`KeyVerifier::verify`] is how a request obtains one. In
//! milestone 1 the verifier is [`StaticKeys`], the project keys listed in configuration. Sessions, roles, the
//! key snapshot and the key feed arrive with the platform milestone.

mod key;
mod scope;

pub use key::{
    KeyConfigError, KeyPrincipal, KeyVerifier, MalformedKey, SecretKey, StaticKey, StaticKeys,
};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
