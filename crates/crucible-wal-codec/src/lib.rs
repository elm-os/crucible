//! Pure write-ahead-log segment and frame codec (contract section 4): the segment header, the `EPOCH` and
//! `CHECKPOINT` directory entries, the frame header, the payload layouts of the eight record types, the
//! compression rule, and a frame scanner that classifies how a segment's data ends.
//!
//! This crate is pure: no `Env`, no IO. LSN continuity across frames and segments is checked by the log
//! (`crucible-wal`), which reads [`Frame::lsn`], [`FrameIter::last_lsn`] and [`SegmentHeader::first_lsn`].

mod compress;
mod crc;
mod frame;
mod iter;
mod payload;
#[cfg(kani)]
mod proofs;
mod segment;
mod wire;

pub use compress::{Compression, CompressionError, PreparedFrame, load_payload};
pub use frame::{
    FRAME_HEADER_LEN, FRAME_HEADER_VERSION, FRAME_MAGIC, FrameHeader, FrameHeaderError,
    MAX_PAYLOAD, RecordType, frame_end,
};
pub use iter::{
    CleanEnd, Frame, FrameCorruption, FrameIter, Scan, SegmentEnd, TornReason, scan_frame,
};
pub use payload::{
    BatchSeal, Checkpoint, ContentOrigin, Fact, FactBatch, FactBody, IngestRequest,
    IngestTransport, MAX_CORRECTED_BYTES, MAX_ENVELOPE_BYTES, MAX_FACT_BATCH_BYTES,
    MAX_FACT_KEY_BYTES, MAX_FACTS, MAX_IDEMPOTENCY_KEY_BYTES, MAX_NOTE_BYTES, MAX_OTLP_BYTES,
    MAX_QUARANTINE_REASON_BYTES, MAX_SCORE_METADATA_BYTES, MAX_SCORE_VALUE_BYTES,
    MAX_SDK_NAME_BYTES, MAX_SDK_VERSION_BYTES, MAX_TOMBSTONE_TRACES, PAYLOAD_VERSION, Quarantine,
    QuarantinePair, Record, ScoreBody, Tombstone, TombstoneSelector,
};
pub use segment::{
    CHECKPOINT_HINT_LEN, CheckpointHint, EPOCH_ENTRY_LEN, EntryError, SEGMENT_FORMAT_VERSION,
    SEGMENT_HEADER_LEN, SEGMENT_MAGIC, SegmentHeader, SegmentHeaderError, decode_epoch,
    encode_epoch,
};
pub use wire::PayloadError;
