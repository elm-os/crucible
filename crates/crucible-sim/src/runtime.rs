//! The deterministic runtime: one thread, a paused virtual clock, and every random stream forked from the
//! run seed.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_env::{
    ChaChaRng, Clock, Env, Instant, Listener, MemoryRecorder, Net, NetError, Rng, ShutdownSignal,
    ShutdownTrigger, Spawner, Stream, TaskAbort, TlsRoots, shutdown_channel,
};
use crucible_records::Timestamp;
use futures::future::BoxFuture;

use crate::disk::SimDisk;
use crate::trace::Trace;

/// The wall-clock time at virtual time zero: 2027-01-15T08:00:00Z.
pub const SIM_EPOCH_MICROS: i64 = 1_800_000_000_000_000;

/// The virtual clock of one host: monotonic time is the runtime's paused clock, shared by every host;
/// wall time is a fixed epoch plus the monotonic time, a per-host offset and the steps applied so far.
#[derive(Debug)]
pub struct SimClock {
    origin: tokio::time::Instant,
    wall_offset_micros: AtomicI64,
}

impl SimClock {
    fn new(origin: tokio::time::Instant, wall_offset_micros: i64) -> Self {
        Self {
            origin,
            wall_offset_micros: AtomicI64::new(wall_offset_micros),
        }
    }

    /// Steps this host's wall clock forward or backward. The monotonic clock is not affected.
    pub fn step_wall(&self, micros: i64) {
        self.wall_offset_micros.fetch_add(micros, Ordering::SeqCst);
    }
}

impl Clock for SimClock {
    fn now(&self) -> Instant {
        let elapsed = tokio::time::Instant::now().saturating_duration_since(self.origin);
        Instant::from_nanos(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
    }

    fn wall_now(&self) -> Timestamp {
        let elapsed = i64::try_from(self.now().nanos() / 1_000).unwrap_or(i64::MAX);
        let offset = self.wall_offset_micros.load(Ordering::SeqCst);
        Timestamp::from_micros(
            SIM_EPOCH_MICROS
                .saturating_add(elapsed)
                .saturating_add(offset),
        )
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'static, ()> {
        let at = self.origin + Duration::from_nanos(deadline.nanos());
        Box::pin(tokio::time::sleep_until(at))
    }
}

/// The task scope of one simulated host. Tasks run on the single runtime thread; blocking closures run
/// inline. A crash aborts every task at its current await point.
#[derive(Debug)]
pub struct SimSpawner {
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    trigger: ShutdownTrigger,
    signal: ShutdownSignal,
}

#[derive(Debug)]
struct SimAbort(tokio::task::AbortHandle);

impl TaskAbort for SimAbort {
    fn abort(&self) {
        self.0.abort();
    }
}

#[derive(Debug)]
struct Finished;

impl TaskAbort for Finished {
    fn abort(&self) {}
}

impl SimSpawner {
    fn new() -> Self {
        let (trigger, signal) = shutdown_channel();
        Self {
            tasks: Mutex::new(Vec::new()),
            trigger,
            signal,
        }
    }

    /// Asks the scope to stop: resolves every [`ShutdownSignal`].
    pub fn request_shutdown(&self) {
        self.trigger.trigger();
    }

    /// Aborts every task of the scope and waits until each one has been dropped.
    pub async fn abort_all(&self) {
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            // An aborted task reports cancellation; a finished one its unit value. Either way it is gone.
            let _gone = task.await;
        }
    }
}

impl Spawner for SimSpawner {
    fn spawn_boxed(&self, _name: &'static str, fut: BoxFuture<'static, ()>) -> Box<dyn TaskAbort> {
        let task = tokio::spawn(fut);
        let abort = task.abort_handle();
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
        Box::new(SimAbort(abort))
    }

    fn spawn_blocking_boxed(
        &self,
        _name: &'static str,
        f: Box<dyn FnOnce() + Send>,
    ) -> Box<dyn TaskAbort> {
        f();
        Box::new(Finished)
    }

    fn shutdown(&self) -> ShutdownSignal {
        self.signal.clone()
    }
}

/// A network that refuses everything. The simulated network joins with the full harness.
#[derive(Debug)]
struct NoNet;

fn no_net() -> NetError {
    NetError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this simulation has no network",
    ))
}

#[async_trait]
impl Net for NoNet {
    async fn listen(&self, _addr: SocketAddr) -> Result<Box<dyn Listener>, NetError> {
        Err(no_net())
    }

    async fn connect(&self, _addr: SocketAddr, _deadline: Instant) -> Result<Stream, NetError> {
        Err(no_net())
    }

    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetError> {
        Err(no_net())
    }

    async fn tls_connect(
        &self,
        _stream: Stream,
        _server_name: &str,
        _roots: &TlsRoots,
    ) -> Result<Stream, NetError> {
        Err(no_net())
    }
}

/// One simulated process: an [`Env`] whose tasks live in one scope, so that a crash can abort them all.
#[derive(Debug)]
pub struct Host {
    /// The environment product code runs in.
    pub env: Env,
    /// The host's clock, for wall-clock steps.
    pub clock: Arc<SimClock>,
    /// The host's task scope.
    pub spawner: Arc<SimSpawner>,
    /// The recorder behind `env.metrics`.
    pub recorder: Arc<MemoryRecorder>,
    disk: SimDisk,
}

impl Host {
    /// Crashes the host: freezes its disk so that no operation in flight completes, aborts every task at
    /// its await point and waits until they are dropped, then applies the crash to the disk. The caller
    /// drops what it still holds of the application and starts a new host on the same disk.
    pub async fn crash(self) {
        self.disk.freeze();
        self.spawner.abort_all().await;
        self.disk.crash();
    }

    /// Stops the host without a crash: aborts every task and leaves the disk as it is.
    pub async fn stop(self) {
        self.spawner.abort_all().await;
    }
}

/// One simulation run: the seed, the root random stream and the event trace.
#[derive(Debug)]
pub struct Sim {
    seed: u64,
    origin: tokio::time::Instant,
    rng: Arc<dyn Rng>,
    trace: Arc<Trace>,
}

impl Sim {
    /// The run seed.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// A random stream that is a function of the run seed and `label` only. Every subsystem and every
    /// fault type takes its own label; forking one label twice yields the same stream twice.
    pub fn rng(&self, label: &str) -> Arc<dyn Rng> {
        self.rng.fork(label)
    }

    /// The run's event trace.
    pub fn trace(&self) -> &Arc<Trace> {
        &self.trace
    }

    /// Virtual time since the start of the run.
    pub fn elapsed(&self) -> Duration {
        tokio::time::Instant::now().saturating_duration_since(self.origin)
    }

    /// Creates an empty disk whose fault streams are forks of the run seed under `label`.
    pub fn disk(&self, label: &str) -> SimDisk {
        SimDisk::new(self.rng(label).as_ref(), Arc::clone(&self.trace))
    }

    /// Starts a host on `disk`. `name` labels its random stream, so each start of a host takes a new
    /// name (for example `node-1/boot-3`). `wall_offset_micros` is the host's wall-clock skew.
    pub fn host(&self, name: &str, disk: &SimDisk, wall_offset_micros: i64) -> Host {
        let clock = Arc::new(SimClock::new(self.origin, wall_offset_micros));
        let spawner = Arc::new(SimSpawner::new());
        let recorder = Arc::new(MemoryRecorder::new());
        let env = Env::new(
            clock.clone(),
            self.rng(&format!("host/{name}")),
            spawner.clone(),
            Arc::new(disk.clone()),
            Arc::new(NoNet),
            recorder.clone(),
        );
        Host {
            env,
            clock,
            spawner,
            recorder,
            disk: disk.clone(),
        }
    }
}

/// Runs `body` on a fresh single-threaded runtime with a paused clock: virtual time advances only when
/// every task is idle. Everything random must come from [`Sim::rng`], so the run is a function of `seed`.
#[allow(clippy::panic)]
pub fn run<F, Fut>(seed: u64, body: F) -> Fut::Output
where
    F: FnOnce(Sim) -> Fut,
    Fut: Future,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap_or_else(|e| panic!("building the simulation runtime: {e}"));
    runtime.block_on(async {
        let sim = Sim {
            seed,
            origin: tokio::time::Instant::now(),
            rng: Arc::new(ChaChaRng::from_u64(seed)),
            trace: Arc::new(Trace::new()),
        };
        body(sim).await
    })
}
