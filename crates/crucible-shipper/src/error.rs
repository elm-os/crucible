//! Why the shipper stops.

use crucible_normalize::PriceTableError;
use crucible_plan::PlanError;
use crucible_records::{Lsn, TokenError};
use crucible_wal::AppendError;

use crate::value::BuildError;

/// A failure the shipper cannot ship past. It stops, the WAL keeps every frame above the watermark, and a
/// restart resumes from there. None of these is an insert outcome: those are handled per contract 5.6.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ShipError {
    /// The WAL is in its `failed` state or its committer has stopped.
    #[error("the wal no longer accepts appends")]
    WalFailed,
    /// The WAL refused a control record.
    #[error("the wal refused a control record: {0}")]
    Append(AppendError),
    /// A frame above the watermark was reclaimed, which garbage collection never does.
    #[error("frame {0} above the watermark was reclaimed")]
    Reclaimed(Lsn),
    /// A durable frame does not decode or normalize, although ingest validated it before the append.
    #[error("frame {lsn} cannot be normalized: {reason}")]
    Frame {
        /// The frame.
        lsn: Lsn,
        /// Why.
        reason: String,
    },
    /// One frame alone exceeds a block, which the admission caps rule out (contract 5.1).
    #[error("frame {0} alone exceeds one block")]
    FrameTooLarge(Lsn),
    /// A blocking job of the CPU budget ended without its result: it panicked, or the runtime dropped it while
    /// shutting down. A restart does the work again.
    #[error("the {stage} job of frame {lsn} ended without a result")]
    JobLost {
        /// The job's stage in the CPU budget: `stage_a` or `encode`.
        stage: &'static str,
        /// The frame, or the first frame of the batch whose block it encoded.
        lsn: Lsn,
    },
    /// The seals above the watermark are not adjacent ranges.
    #[error("recovered seals are not adjacent: expected a seal from {expected}, found {found}")]
    SealChain {
        /// Where the next seal had to start.
        expected: Lsn,
        /// Where it starts.
        found: Lsn,
    },
    /// A block could not be built from its rows.
    #[error(transparent)]
    Build(#[from] BuildError),
    /// A batch identity has no token.
    #[error("no token for the batch: {0}")]
    Token(#[from] TokenError),
    /// A pair has no probe.
    #[error("no probe for the pair: {0}")]
    Probe(#[from] PlanError),
    /// The bundled price table does not load. A test of `crucible-normalize` loads it, so no build reaches this.
    #[error(transparent)]
    Prices(#[from] PriceTableError),
}
