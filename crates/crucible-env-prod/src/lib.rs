//! Production implementations of the crucible-env traits.
//!
//! This is the one library crate allowed to touch ambient time, entropy, threads, the filesystem and the
//! network; the determinism lints are switched off for it here and nowhere else. Everything is built once,
//! in `main`, by [`build`], and reaches product code only as a [`crucible_env::Env`].

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod clock;
mod disk;
mod net;
mod rng;
mod spawner;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crucible_env::metrics::registry;
use crucible_env::{DiskError, Env, MemoryRecorder, Metrics, Root};

pub use clock::SystemClock;
pub use disk::{
    BackendChoice, BackendUnavailable, DiskBackend, PortableDisk, WalBackend, select_backend,
};
pub use net::TokioNet;
pub use rng::os_seeded_rng;
pub use spawner::TokioSpawner;

/// What [`build`] needs to know.
#[derive(Clone, Debug, Default)]
pub struct ProdConfig {
    /// The directory behind each disk root. A root that is not listed is unavailable.
    pub roots: BTreeMap<Root, PathBuf>,
    /// The `wal.backend` setting.
    pub wal_backend: WalBackend,
}

/// The production environment and the concrete handles `main` keeps for itself.
#[derive(Debug)]
pub struct Prod {
    /// The environment for product code.
    pub env: Env,
    /// The shutdown scope's owner: `main` requests shutdown and closes the scope through it.
    pub spawner: Arc<TokioSpawner>,
    /// The recorder behind `env.metrics`, for rendering `/metrics`.
    pub recorder: Arc<MemoryRecorder>,
    /// The disk backend in use and, for a fallback, why.
    pub wal_backend: BackendChoice,
}

/// Why the production environment could not be built.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The OS entropy source failed.
    #[error("os entropy unavailable: {0}")]
    Entropy(String),
    /// A disk root could not be prepared.
    #[error("disk root: {0}")]
    Disk(#[from] DiskError),
    /// The requested WAL backend cannot be used.
    #[error(transparent)]
    Backend(#[from] BackendUnavailable),
}

/// Builds the production [`Env`] on the given tokio runtime: OS clocks, a ChaCha20 stream seeded from OS
/// entropy, a shutdown scope on the runtime, the selected disk backend, tokio networking with rustls, and the
/// in-memory metrics recorder. Records the backend choice in `crucible_wal_backend_info`.
pub fn build(runtime: tokio::runtime::Handle, config: &ProdConfig) -> Result<Prod, BuildError> {
    let wal_backend = select_backend(config.wal_backend)?;
    let recorder = Arc::new(MemoryRecorder::new());
    recorder
        .gauge(
            &registry::WAL_BACKEND_INFO,
            &[
                wal_backend.requested.as_str(),
                wal_backend.backend.as_str(),
                wal_backend.fallback_reason.unwrap_or(""),
            ],
        )
        .set(1);
    if let Some(reason) = wal_backend.fallback_reason {
        tracing::info!(
            backend = wal_backend.backend.as_str(),
            reason,
            "wal backend fell back"
        );
    }

    let clock = Arc::new(SystemClock::new());
    let rng = Arc::new(os_seeded_rng().map_err(|e| BuildError::Entropy(e.to_string()))?);
    let spawner = Arc::new(TokioSpawner::new(runtime.clone()));
    let disk = Arc::new(PortableDisk::new(runtime, &config.roots)?);
    let net = Arc::new(TokioNet::new(clock.clone()));
    let env = Env::new(clock, rng, spawner.clone(), disk, net, recorder.clone());
    Ok(Prod {
        env,
        spawner,
        recorder,
        wal_backend,
    })
}
