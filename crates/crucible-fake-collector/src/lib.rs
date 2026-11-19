//! The SDK conformance suite's collector and driver (`docs/testing/sdk-conformance.md`).
//!
//! - [`Collector`] is an OTLP/HTTP receiver on a loopback port. It decodes protobuf and JSON bodies, gzip or not,
//!   with the same scanner and transcoder as ingest (`crucible-otlp`), and records every request and span in one
//!   ordered log, together with the checkpoints runners post.
//! - [`Suite`] loads and cross-checks `conformance/`: constants, the key registry, scenarios, goldens and the
//!   scrub rules. [`spec`] resolves each case into the run spec a runner plays, and checks every resolved case
//!   against the transport and lifecycle rules of both protocol revisions.
//! - [`run_suite`] runs every case through a [`Runner`], evaluates the collected spans against the golden
//!   ([`golden::evaluate`]) and returns the matrix [`Report`].
//! - [`SelfTestRunner`] emits the spans an SDK that follows the contract would emit, directly with
//!   `opentelemetry-proto`, so the suite runs before any SDK exists.
//!
//! The library reaches time, the network and task spawning only through [`crucible_env::Env`]. The binary,
//! `crucible-sdk-driver`, builds the production `Env`, reads the suite and manifest files, spawns runner
//! processes and writes the reports.

pub mod collector;
mod driver;
pub mod golden;
mod hash;
mod pattern;
pub mod registry;
pub mod scrub;
mod selftest;
pub mod spec;
pub mod suite;
pub mod value;

pub use collector::{Collector, Event, Export, RecordedSpan};
pub use driver::{
    CaseResult, DriverError, Options, Report, RunOutcome, Runner, RunnerSummary, Status, run_suite,
};
pub use hash::{mrtr_hash, session_hash};
pub use selftest::{Mutant, REQUEST_STATE, SESSION_ID, SelfTestOptions, SelfTestRunner};
pub use spec::{ResponseEvent, RunSpec, RunnerEvent};
pub use suite::{Manifest, Suite, SuiteError};
