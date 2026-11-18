//! Open-loop OTLP load generator and fsync-honesty probe for the performance rig (`docs/testing/perf.md`).
//!
//! - [`Pool`] pre-encodes OTLP/HTTP protobuf requests of 1 KiB or 10 KiB spans; each request is a copy of a
//!   template with its identity patched in, so every span of a run carries the run id and a unique sequence
//!   number.
//! - [`run`] sends them open loop: request `i` is due at `start + i / rate` whatever the responses do, and
//!   its latency is measured from that intended time, so a stalled server cannot hide behind a stalled
//!   client (coordinated omission).
//! - [`AckLedger`] records which requests were acknowledged; [`SequenceReader`] and [`check_zero_loss`] are
//!   the seam for the zero-loss check that a store-backed reader will close in a later wave.
//! - [`probe`] measures write-plus-fdatasync latency through `Env`'s disk and classifies the volume's
//!   flushes as honest or suspicious.
//! - [`stub`] is a minimal OTLP/HTTP server for calibrating the generator.
//!
//! The library reaches time, the network and the disk only through [`crucible_env::Env`]; the binary's
//! `main` builds the production `Env`.

mod ledger;
mod pool;
mod probe;
mod run;
pub mod stub;

pub use ledger::{AckLedger, LossCheck, SequenceReader, check_zero_loss, verify};
pub use pool::{
    MAX_BODY_BYTES, MAX_SPANS_PER_REQUEST, Pool, PoolConfig, PoolError, RUN_ID_KEY, SEQ_KEY,
    SpanSize, SpanSizeError,
};
pub use probe::{
    BYTES_PER_SIZE, Check, DIR_SYNC_FLOOR_NS, DirSync, Distribution, FLUSH_FLOOR_NS,
    MAX_DEVICE_BYTES_PER_S, Medians, PROBE_SCHEMA, ProbeReport, Verdict, WARMUP_ITERATIONS,
    WRITE_SIZES, WriteSize, classify, probe,
};
pub use run::{
    ConnectError, ErrorCounts, GeneratorCpu, Lane, MAX_REQUESTS, Outcome, RUN_SCHEMA, ResolveError,
    RunConfig, RunError, RunOutcome, RunReport, SPIN_WINDOW, Schedule, Summary, TRACES_PATH,
    offset_nanos, resolve, run, scheduled_requests,
};
