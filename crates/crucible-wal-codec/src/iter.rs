//! Frame scanning over the bytes of one segment, and the classification of how its data ends (contract
//! section 4.6).

use std::borrow::Cow;
use std::iter::FusedIterator;

use crucible_records::{Lsn, Timestamp};

use crate::compress::{CompressionError, load_payload};
use crate::crc::crc32c;
use crate::frame::{
    FRAME_HEADER_LEN, FrameHeader, FrameHeaderError, MAX_PAYLOAD, RecordType, frame_end,
    raw_payload_fields,
};

/// One valid frame of a segment: its header checksum, its payload checksum and every header field are valid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame<'a> {
    offset: usize,
    end_offset: usize,
    header: FrameHeader,
    stored: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Offset of the frame header in the segment.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Offset one past the frame's payload: where the next frame starts.
    pub fn end_offset(&self) -> usize {
        self.end_offset
    }

    /// The decoded header.
    pub fn header(&self) -> &FrameHeader {
        &self.header
    }

    /// The frame's LSN.
    pub fn lsn(&self) -> Lsn {
        self.header.lsn
    }

    /// The frame's record type.
    pub fn record_type(&self) -> RecordType {
        self.header.record_type
    }

    /// The frame's receive time.
    pub fn received_at(&self) -> Timestamp {
        self.header.received_at
    }

    /// The payload bytes as stored (compressed when the header says so).
    pub fn stored_payload(&self) -> &'a [u8] {
        self.stored
    }

    /// The payload: a slice of the segment when stored as it is, decompressed otherwise.
    pub fn payload(&self) -> Result<Cow<'a, [u8]>, CompressionError> {
        load_payload(self.header.compressed, self.stored)
    }
}

/// Why a segment's data ends cleanly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CleanEnd {
    /// Zero bytes where a frame magic is expected: the end of written data. Fewer than four remaining bytes
    /// count when all of them are zero.
    ZeroMagic,
    /// The previous frame ends exactly at the end of the segment.
    EndOfSegment,
}

/// Why the bytes at the end of the valid prefix are not a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TornReason {
    /// Fewer than 36 bytes remain.
    ShortHeader,
    /// The header checksum does not match.
    HeaderCrc,
    /// The header is valid and the payload runs past the end of the segment.
    ShortPayload,
    /// The header is valid and the payload checksum does not match.
    PayloadCrc,
}

/// How the data of a segment ends. `offset` is always the end of the valid prefix: every byte before it
/// belongs to a valid frame (or to the segment header), and no valid frame starts at it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegmentEnd {
    /// The end of written data.
    Clean {
        /// End of the valid prefix.
        offset: usize,
        /// Which clean ending.
        reason: CleanEnd,
    },
    /// An incomplete or checksum-failing frame. In the last segment recovery truncates from `offset`; whether
    /// a torn end is acceptable in an earlier segment is decided from LSNs by the log.
    Torn {
        /// End of the valid prefix.
        offset: usize,
        /// What is wrong with the bytes at `offset`.
        reason: TornReason,
    },
    /// Fatal corruption: a frame whose checksums are valid and whose header is not acceptable (or declares a
    /// payload above the cap, which no checksum can vouch for). Recovery must not truncate.
    Corrupt {
        /// End of the valid prefix, which is the offset of the offending frame.
        offset: usize,
        /// What is wrong with the header.
        error: FrameHeaderError,
    },
}

impl SegmentEnd {
    /// End of the valid prefix.
    pub fn offset(&self) -> usize {
        match *self {
            Self::Clean { offset, .. }
            | Self::Torn { offset, .. }
            | Self::Corrupt { offset, .. } => offset,
        }
    }
}

/// The result of looking for a frame at one offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scan<'a> {
    /// A valid frame. Its `end_offset` is above the scanned offset and at most the segment length.
    Frame(Frame<'a>),
    /// No valid frame starts here.
    End(SegmentEnd),
}

/// Looks for a frame at `offset` in `segment` (the bytes of a whole segment, header included, so offsets are
/// file offsets). An offset beyond the segment reads as a short header at the segment's end.
pub fn scan_frame(segment: &[u8], offset: usize) -> Scan<'_> {
    let offset = offset.min(segment.len());
    let rest = segment.get(offset..).unwrap_or_default();
    let torn = |reason| Scan::End(SegmentEnd::Torn { offset, reason });
    if rest.is_empty() {
        let reason = CleanEnd::EndOfSegment;
        return Scan::End(SegmentEnd::Clean { offset, reason });
    }
    if rest.iter().take(4).all(|&b| b == 0) {
        let reason = CleanEnd::ZeroMagic;
        return Scan::End(SegmentEnd::Clean { offset, reason });
    }
    let Some(head) = rest.first_chunk::<FRAME_HEADER_LEN>() else {
        return torn(TornReason::ShortHeader);
    };
    let decoded = FrameHeader::decode(head);
    if decoded == Err(FrameHeaderError::HeaderCrcMismatch) {
        return torn(TornReason::HeaderCrc);
    }
    // The header checksum is valid. A semantic error is fatal only when the payload checksum is valid too
    // (contract section 4.3), so the payload is located from the raw length field before the verdict.
    let (payload_len, payload_crc32c) = raw_payload_fields(head);
    if payload_len > MAX_PAYLOAD {
        // No writer produces this and no checksum can vouch for a payload that cannot be read.
        let error = decoded
            .err()
            .unwrap_or(FrameHeaderError::PayloadTooLarge(payload_len));
        return Scan::End(SegmentEnd::Corrupt { offset, error });
    }
    let located = frame_end(offset, payload_len, segment.len()).and_then(|end_offset| {
        let stored = rest.get(FRAME_HEADER_LEN..)?.get(..payload_len as usize)?;
        Some((end_offset, stored))
    });
    let Some((end_offset, stored)) = located else {
        return torn(TornReason::ShortPayload);
    };
    if crc32c(stored) != payload_crc32c {
        return torn(TornReason::PayloadCrc);
    }
    match decoded {
        Ok(header) => Scan::Frame(Frame {
            offset,
            end_offset,
            header,
            stored,
        }),
        Err(error) => Scan::End(SegmentEnd::Corrupt { offset, error }),
    }
}

/// Fatal corruption met by [`FrameIter`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("corrupt frame at offset {offset}: {error}")]
pub struct FrameCorruption {
    /// Offset of the offending frame.
    pub offset: usize,
    /// What is wrong with its header.
    pub error: FrameHeaderError,
}

/// Iterates the valid frames of a segment in order.
///
/// Every call to `next` either yields a frame and moves [`FrameIter::offset`] forward, or ends the iteration
/// and records why in [`FrameIter::end`]. Fatal corruption is yielded once as an `Err` before the iterator
/// ends; a clean end and a torn tail end it directly.
#[derive(Clone, Debug)]
pub struct FrameIter<'a> {
    segment: &'a [u8],
    offset: usize,
    last_lsn: Option<Lsn>,
    end: Option<SegmentEnd>,
}

impl<'a> FrameIter<'a> {
    /// Scans `segment` (the bytes of a whole segment) from `offset`, normally
    /// [`SEGMENT_HEADER_LEN`](crate::SEGMENT_HEADER_LEN).
    pub fn new(segment: &'a [u8], offset: usize) -> Self {
        Self {
            segment,
            offset: offset.min(segment.len()),
            last_lsn: None,
            end: None,
        }
    }

    /// End of the valid data seen so far: the offset at which the next frame is expected.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// LSN of the last frame yielded.
    pub fn last_lsn(&self) -> Option<Lsn> {
        self.last_lsn
    }

    /// How the data ends, once the iteration has ended.
    pub fn end(&self) -> Option<SegmentEnd> {
        self.end
    }
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = Result<Frame<'a>, FrameCorruption>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.end.is_some() {
            return None;
        }
        match scan_frame(self.segment, self.offset) {
            Scan::Frame(frame) => {
                self.offset = frame.end_offset;
                self.last_lsn = Some(frame.header.lsn);
                Some(Ok(frame))
            }
            Scan::End(end) => {
                self.end = Some(end);
                match end {
                    SegmentEnd::Corrupt { offset, error } => {
                        Some(Err(FrameCorruption { offset, error }))
                    }
                    _ => None,
                }
            }
        }
    }
}

impl FusedIterator for FrameIter<'_> {}
