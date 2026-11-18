//! Write-ahead log (contract section 4): segments, group commit, recovery, replay and garbage collection,
//! over the `Disk`, `Clock`, `Spawner` and `Metrics` handles of an [`crucible_env::Env`].
//!
//! - [`Wal::open`] initialises or recovers the directory and returns a [`RecoveryReport`].
//! - [`Wal::append`] resolves with the frame's LSN only after the frame is durable (the ack rule of
//!   section 4.5, under [`Durability::Group`]).
//! - [`Wal::replay`] reads durable frames in LSN order.
//! - [`Wal::gc`] removes whole segments below the checkpoint watermark and outside replay retention.
//! - [`Wal::admission`] gives ingest the high-water, disk-floor and failed signals of section 4.8.
//!
//! The checkpoint watermark is taken only from `Checkpoint` frames that the log itself made durable or
//! recovered; no caller can supply one.

mod committer;
mod config;
mod error;
mod layout;
mod recovery;
mod replay;
mod scan;
mod wal;

pub use config::{
    DEFAULT_MAX_BYTES, DEFAULT_MIN_FREE_BYTES, DEFAULT_REPLAY_RETENTION, DEFAULT_SEGMENT_BYTES,
    Durability, GROUP_BYTES, MAX_REPLAY_RETENTION, Mutant, WalConfig,
};
pub use error::{AppendError, GcError, RecoveryError, ReplayError, SegmentHeaderIssue, WalGap};
pub use layout::SegmentMeta;
pub use recovery::{RecoveryReport, TailRepair, Watermark};
pub use replay::Replay;
pub use scan::WalFrame;
pub use wal::{Admission, GcReport, Wal};
