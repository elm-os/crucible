//! Authentication and authorization (contract section 11).
//!
//! In the walking skeleton this crate only owns minting of `ProjectScope` values. Credentials, sessions, roles
//! and the key snapshot arrive with the platform milestone.

mod scope;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
