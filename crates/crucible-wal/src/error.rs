//! Typed errors of the log.

use crucible_env::DiskError;
use crucible_records::Lsn;
use crucible_wal_codec::{
    CompressionError, EntryError, FrameHeaderError, PayloadError, SegmentHeaderError,
};

/// A break in the LSN sequence (contract section 4.6). The node refuses to start ingest and reports
/// `wal_gap`; nothing is truncated.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum WalGap {
    /// A segment number is missing between two segments.
    #[error("segment {expected} is missing (found {found})")]
    MissingSegment {
        /// The number that should follow.
        expected: u64,
        /// The number found instead.
        found: u64,
    },
    /// A segment's `first_lsn` is below its predecessor's, or is zero.
    #[error("segment {segment_no} starts at lsn {first_lsn}, before its predecessor")]
    FirstLsnRegressed {
        /// The offending segment.
        segment_no: u64,
        /// Its `first_lsn`.
        first_lsn: Lsn,
    },
    /// An earlier segment's valid prefix ends below the next segment's `first_lsn` minus 1.
    #[error(
        "segment {segment_no} is valid through lsn {valid_through}, the next starts at {next_first_lsn}"
    )]
    ShortSegment {
        /// The short segment.
        segment_no: u64,
        /// Last LSN of its valid prefix.
        valid_through: Lsn,
        /// `first_lsn` of the next segment.
        next_first_lsn: Lsn,
    },
    /// A frame's LSN is not its predecessor's plus 1.
    #[error("segment {segment_no} offset {offset}: expected lsn {expected}, found {found}")]
    LsnMismatch {
        /// The segment.
        segment_no: u64,
        /// Offset of the frame.
        offset: u64,
        /// The LSN that should be there.
        expected: Lsn,
        /// The LSN that is there.
        found: Lsn,
    },
    /// The oldest segment starts above the checkpoint watermark plus 1: frames that were never resolved
    /// are gone.
    #[error("oldest segment starts at lsn {oldest_first_lsn}, above watermark {watermark} plus 1")]
    MissingPrefix {
        /// `first_lsn` of the oldest segment.
        oldest_first_lsn: Lsn,
        /// The highest checkpoint watermark found (zero when there is none).
        watermark: Lsn,
    },
}

/// Why the log could not be opened.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum RecoveryError {
    /// A disk operation failed. Nothing was decided from it; a restart may succeed.
    #[error("disk: {0}")]
    Disk(#[from] DiskError),
    /// The settings cannot be used.
    #[error("invalid wal config: {0}")]
    InvalidConfig(&'static str),
    /// Segments exist and `EPOCH` does not.
    #[error("the wal directory has segments and no EPOCH entry")]
    MissingEpoch,
    /// Segments exist and `EPOCH` is not valid.
    #[error("the wal directory has segments and an invalid EPOCH entry: {0}")]
    InvalidEpoch(EntryError),
    /// A segment header is unreadable and the segment is not an unfinished creation.
    #[error("segment {segment_no}: bad header: {reason}")]
    BadSegmentHeader {
        /// The segment.
        segment_no: u64,
        /// What is wrong.
        reason: SegmentHeaderIssue,
    },
    /// A break in the LSN sequence.
    #[error("wal gap: {0}")]
    Gap(#[from] WalGap),
    /// A frame with valid checksums and an unacceptable header.
    #[error("segment {segment_no} offset {offset}: corrupt frame: {error}")]
    CorruptFrame {
        /// The segment.
        segment_no: u64,
        /// Offset of the frame.
        offset: u64,
        /// What is wrong with the header.
        error: FrameHeaderError,
    },
    /// A `BatchSeal` or `Checkpoint` frame with valid checksums whose payload does not decode, or whose
    /// range reaches its own LSN.
    #[error("lsn {lsn}: bad control record: {reason}")]
    BadRecord {
        /// The frame.
        lsn: Lsn,
        /// What is wrong.
        reason: String,
    },
    /// Two reads of the torn tail disagreed, so nothing was truncated. A restart may succeed.
    #[error("segment {segment_no} offset {offset}: reads of the torn tail disagree")]
    UnstableRead {
        /// The segment.
        segment_no: u64,
        /// Offset of the tail.
        offset: u64,
    },
}

impl RecoveryError {
    /// Whether the directory itself is damaged, as opposed to an operation that may succeed after a restart.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, Self::Disk(_) | Self::UnstableRead { .. })
    }

    /// The stable code for health output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Disk(_) | Self::UnstableRead { .. } => "wal_io",
            Self::InvalidConfig(_) => "wal_config",
            Self::MissingEpoch | Self::InvalidEpoch(_) => "wal_epoch",
            Self::BadSegmentHeader { .. } => "wal_segment_header",
            Self::Gap(_) => "wal_gap",
            Self::CorruptFrame { .. } | Self::BadRecord { .. } => "wal_corrupt",
        }
    }
}

/// What is wrong with a segment header.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum SegmentHeaderIssue {
    /// The header does not decode.
    #[error("{0}")]
    Decode(#[from] SegmentHeaderError),
    /// The header is all zeros.
    #[error("header was never written")]
    Unwritten,
    /// The file is shorter than a header and not zero.
    #[error("file is shorter than a header")]
    Short,
    /// The header names another epoch than `EPOCH`.
    #[error("epoch differs from EPOCH")]
    EpochMismatch,
    /// The header's segment number differs from the file name's.
    #[error("segment number differs from the file name")]
    NumberMismatch,
}

/// Why an append was refused or failed. No frame of a failed append is acknowledged.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum AppendError {
    /// The frame cannot fit in an empty segment.
    #[error("frame of {len} bytes exceeds the segment capacity of {max}")]
    TooLarge {
        /// Encoded frame length.
        len: u64,
        /// Largest frame a segment holds.
        max: u64,
    },
    /// A `Checkpoint` payload that does not decode, or whose watermark is above the durable LSN.
    #[error("invalid checkpoint record")]
    InvalidCheckpoint,
    /// The frame's group failed to append or sync. The caller answers 503 and the client may retry.
    #[error("wal group failed")]
    Unavailable,
    /// The WAL is in the `failed` state until restart.
    #[error("wal failed")]
    Failed,
    /// The committer has stopped.
    #[error("wal closed")]
    Closed,
}

/// Why a replay read failed.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ReplayError {
    /// A disk operation failed.
    #[error("disk: {0}")]
    Disk(#[from] DiskError),
    /// The requested LSN is below the oldest retained segment.
    #[error("lsn {lsn} was reclaimed; the oldest retained lsn is {oldest}")]
    Reclaimed {
        /// The LSN asked for.
        lsn: Lsn,
        /// First LSN of the oldest retained segment.
        oldest: Lsn,
    },
    /// A durable frame could not be read back intact.
    #[error("segment {segment_no} offset {offset}: durable frame {lsn} is unreadable")]
    Unreadable {
        /// The segment.
        segment_no: u64,
        /// Offset at which the frame was expected.
        offset: u64,
        /// The frame's LSN.
        lsn: Lsn,
    },
}

/// Why garbage collection failed. Segments already removed stay removed.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("wal gc: {0}")]
pub struct GcError(#[from] pub DiskError);

pub(crate) fn bad_payload(lsn: Lsn, e: &PayloadError) -> RecoveryError {
    RecoveryError::BadRecord {
        lsn,
        reason: e.to_string(),
    }
}

pub(crate) fn bad_compression(lsn: Lsn, e: CompressionError) -> RecoveryError {
    RecoveryError::BadRecord {
        lsn,
        reason: e.to_string(),
    }
}
