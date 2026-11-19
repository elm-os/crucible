//! Authentication and authorization (contract section 11).
//!
//! This crate owns minting of `ProjectScope` values: [`KeyVerifier::verify`] and [`KeyVerifier::verify_pair`]
//! are how a request obtains one, and [`authenticate`] reads the `Authorization` forms of contract 12.2 into
//! them. Without a control plane the verifier is [`StaticKeys`], the project keys listed in configuration; with
//! one it is [`DbKeys`], the Postgres key snapshot kept by the key feed. The control plane's sessions, roles,
//! invitations and provider sign-in are in [`service`].

mod credential;
mod key;
mod scope;

// The control plane (milestone 4).
pub mod cookie;
pub mod csrf;
mod db_keys;
pub mod http;
mod mint;
mod next_path;
pub mod oauth;
pub mod password;
pub mod roles;
mod seal;
pub mod service;
pub mod token;

pub use db_keys::{
    DbKeys, KeyGrant, KeyVerdict, OUTAGE_AFTER, OutageMode, POLL_EVERY, RETRY_AFTER, ScopedKeys,
    Scopes, StoreFeed, run_feed, run_last_used,
};
pub use mint::{LookupKey, MintedKey, mint, secret_hash};
pub use next_path::safe_next;
pub use scope::job_scope;
pub use seal::{Sealed, Sealer, Unsealable};

pub use credential::{Refusal, authenticate};
pub use key::{
    KeyConfigError, KeyPrincipal, KeyVerifier, MalformedKey, MalformedPublicKey, PairError,
    PublicKey, SecretKey, StaticKey, StaticKeys,
};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod testing {
    use crucible_env::Env;

    /// Runs `body` on a simulated host with seed `seed`.
    pub(crate) fn sim<T: 'static>(seed: u64, body: impl AsyncFnOnce(Env) -> T + 'static) -> T {
        crucible_sim::run(seed, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            body(host.env.clone()).await
        })
    }
}
