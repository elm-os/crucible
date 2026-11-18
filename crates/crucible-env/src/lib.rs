//! Determinism seam: Clock, Rng, Spawner, Disk, Net and Metrics, bundled in [`Env`], and the store traits,
//! bundled in [`Stores`].
//!
//! Product crates take an [`Env`] and never touch ambient time, randomness, task spawning, the filesystem,
//! the network or the process environment (contract section 2). `crucible-env-prod` builds the production
//! `Env`; `crucible-sim` builds the simulated one. This crate itself is deterministic: it reads no clock, no
//! entropy and no file, and it needs no async runtime.
//!
//! # Construction
//!
//! A host builds its handles in two phases, then its components:
//!
//! 1. The core [`Env`], from the six primitive handles.
//! 2. The [`Stores`], each built over that `Env` (it is cloneable): a store opens its connections through
//!    `env.net`, takes its deadlines from `env.clock` and mints its query ids with `env.new_id()`. In
//!    simulation the stores are in-memory models that take the same `Env` for their virtual time and seeded
//!    faults.
//! 3. The product components, each of which receives the `Env` and the `Stores` it needs.
//!
//! Stores are not fields of `Env` because they are built from one.
//!
//! Every trait is object-safe because `Env` holds `Arc<dyn Trait>` handles. Generic conveniences (a typed
//! [`JoinHandle`], [`ClockExt::timeout`] over any future, [`Interval`]) are inherent methods on `Env` or
//! extension traits that box. Async methods are cancel-safe unless their documentation says otherwise.

mod asset;
mod child;
mod clock;
mod disk;
mod id;
pub mod metrics;
mod net;
mod rng;
mod spawner;
mod store;
#[cfg(test)]
mod testing;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

pub use asset::{Asset, AssetManifest, AssetSource, Encodings};
pub use child::{
    Child, ChildControl, ChildError, ChildInput, ChildOutput, ChildProcess, ChildSignal, ChildSpec,
    ExitStatus, StderrMode,
};
pub use clock::{Clock, ClockExt, Elapsed, Instant, Interval, MissedTicks};
pub use crucible_control::ControlStore;
pub use disk::{Disk, DiskError, DiskPath, Entry, File, FileOps, OpenMode, Root};
pub use id::{IdGen, new_id};
pub use metrics::{
    Counter, CounterDef, Gauge, GaugeDef, Histogram, HistogramDef, MemoryRecorder, Metrics,
};
pub use net::{Listener, Net, NetError, Stream, StreamIo, TlsRoots};
pub use rng::{ChaChaRng, Rng, RngAdapter};
pub use spawner::{
    Cancelled, JoinHandle, ShutdownSignal, ShutdownTrigger, Spawner, TaskAbort, shutdown_channel,
};
pub use store::{
    EngineFamily, InsertClass, InsertError, InsertSettings, MutationId, MutationStatus, QueryError,
    RetryReason, RowStream, StoreError, StoreFacts, StoreFlavour, StoreHealth, StoreVersion,
    TelemetryStore,
};

/// The environment handed to product code: one cloneable bundle of `Arc<dyn Trait>` handles, built once in
/// `main` (production) or by the simulation harness. The stores are the separate bundle [`Stores`].
#[derive(Clone, Debug)]
pub struct Env {
    /// Monotonic and wall-clock time.
    pub clock: Arc<dyn Clock>,
    /// The root random stream. Subsystems should `fork` a labelled child instead of sharing it.
    pub rng: Arc<dyn Rng>,
    /// Task spawning inside the host's shutdown scope.
    pub spawner: Arc<dyn Spawner>,
    /// Files under named roots.
    pub disk: Arc<dyn Disk>,
    /// TCP, DNS and TLS.
    pub net: Arc<dyn Net>,
    /// Metric handles.
    pub metrics: Arc<dyn Metrics>,
    ids: Arc<IdGen>,
    id_rng: Arc<dyn Rng>,
}

impl Env {
    /// Bundles the handles. The id generator state (see [`IdGen`]) is created here and shared by every clone,
    /// so all ids minted through one `Env` and its clones are strictly increasing. Ids draw from the fork
    /// `crucible-env/new_id` of `rng`, so minting an id never perturbs another consumer's stream.
    pub fn new(
        clock: Arc<dyn Clock>,
        rng: Arc<dyn Rng>,
        spawner: Arc<dyn Spawner>,
        disk: Arc<dyn Disk>,
        net: Arc<dyn Net>,
        metrics: Arc<dyn Metrics>,
    ) -> Self {
        let id_rng = rng.fork("crucible-env/new_id");
        Self {
            clock,
            rng,
            spawner,
            disk,
            net,
            metrics,
            ids: Arc::new(IdGen::new()),
            id_rng,
        }
    }

    /// Mints a UUIDv7 (contract 1.1), strictly greater than every id this `Env` minted before.
    pub fn new_id(&self) -> crucible_records::Uuid {
        new_id(&self.ids, self.clock.as_ref(), self.id_rng.as_ref())
    }

    /// Spawns a named task in the host's shutdown scope and returns a typed handle.
    pub fn spawn<T, F>(&self, name: &'static str, fut: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        spawner::spawn(self.spawner.as_ref(), name, fut)
    }

    /// Runs CPU-bound or blocking work off the async threads and returns a typed handle.
    pub fn spawn_blocking<T, F>(&self, name: &'static str, f: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        spawner::spawn_blocking(self.spawner.as_ref(), name, f)
    }

    /// Resolves when the host's shutdown scope is asked to stop.
    pub fn shutdown(&self) -> ShutdownSignal {
        self.spawner.shutdown()
    }

    /// Runs `fut` for at most `d`; the future is dropped on expiry.
    pub async fn timeout<F: Future>(&self, d: Duration, fut: F) -> Result<F::Output, Elapsed> {
        self.clock.timeout(d, fut).await
    }

    /// A periodic timer whose first tick is one `period` from now.
    pub fn interval(&self, period: Duration, missed: MissedTicks) -> Interval {
        Interval::new(Arc::clone(&self.clock), period, missed)
    }
}

/// The stores handed to product code alongside an [`Env`]: one cloneable bundle of `Arc<dyn Trait>` handles,
/// built once after the `Env` they run over (see the crate documentation). The later stores join it as their
/// traits land.
#[derive(Clone, Debug)]
pub struct Stores {
    /// Postgres, or its simulation model. `None` on a node that runs without a control plane: `serve` without
    /// Postgres still ingests and reads telemetry.
    pub control: Option<Arc<dyn ControlStore>>,
    /// ClickHouse, or its simulation model.
    pub telemetry: Arc<dyn TelemetryStore>,
}

impl Stores {
    /// Bundles the handles.
    pub fn new(telemetry: Arc<dyn TelemetryStore>) -> Self {
        Self {
            control: None,
            telemetry,
        }
    }

    /// The same bundle with the control store.
    #[must_use]
    pub fn with_control(mut self, control: Arc<dyn ControlStore>) -> Self {
        self.control = Some(control);
        self
    }
}
