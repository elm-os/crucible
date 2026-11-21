//! Property and example tests of the public codec: frames of every record type round-trip through a segment,
//! and damaged bytes are detected or classified without a panic.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crucible_records::{
    ApiKeyId, AuthorKind, CivilDate, DeletionRequestId, FactId, FactTargetKind, Lsn, PartitionId,
    ProjectId, ScoreDataType, ScoreSource, SpanId, TableId, TagOp, Timestamp, TraceId, Uuid,
    WalEpoch,
};
use crucible_wal_codec::{
    BatchSeal, Checkpoint, CleanEnd, Compression, ContentOrigin, FRAME_HEADER_LEN, Fact, FactBatch,
    FactBody, FrameHeader, FrameHeaderError, FrameIter, IngestRequest, IngestTransport,
    MAX_ENVELOPE_BYTES, MAX_FACTS, MAX_PAYLOAD, MAX_TOMBSTONE_TRACES, PayloadError, PreparedFrame,
    Quarantine, QuarantinePair, Record, RecordType, SEGMENT_HEADER_LEN, Scan, ScoreBody,
    SegmentEnd, SegmentHeader, Tombstone, TombstoneSelector, TornReason, scan_frame,
};
use proptest::collection::vec;
use proptest::prelude::*;

// ---------------------------------------------------------------------------------------------------------
// Owned inputs. Records borrow from their payload, so strategies generate owned data and lend it out.
// ---------------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum OwnedBody {
    Score {
        data_type: ScoreDataType,
        value_bits: u64,
        value_str: String,
        comment: String,
        source: ScoreSource,
        metadata: String,
        config_id: Option<[u8; 16]>,
    },
    Tag(TagOp),
    Flag(bool),
    Correction {
        corrected: String,
        note: String,
    },
}

#[derive(Clone, Debug)]
struct OwnedFact {
    fact_id: [u8; 16],
    target_kind: FactTargetKind,
    trace_id: [u8; 16],
    span_id: [u8; 8],
    session_id: String,
    target_time: i64,
    key: String,
    version: u64,
    retracted: bool,
    created_at: i64,
    body: OwnedBody,
}

#[derive(Clone, Debug)]
enum OwnedSelector {
    Traces(Vec<[u8; 16]>),
    User(String),
    Session(String),
    Project,
}

#[derive(Clone, Debug)]
enum Owned {
    Ingest {
        project_id: [u8; 16],
        api_key_id: [u8; 16],
        compat_dialect: u8,
        grpc: bool,
        json: bool,
        admin_import: bool,
        sdk_name: String,
        sdk_version: String,
        idempotency_key: Vec<u8>,
        span_count: u32,
        rejected: Vec<u8>,
        otlp: Vec<u8>,
    },
    Native(Vec<u8>),
    Facts {
        project_id: [u8; 16],
        author_kind: AuthorKind,
        author_id: [u8; 16],
        facts: Vec<OwnedFact>,
    },
    Tombstone {
        deletion_request_id: [u8; 16],
        project_id: [u8; 16],
        requested_at: i64,
        deletion_seq: u64,
        selector: OwnedSelector,
    },
    Seal(BatchSeal),
    Checkpoint(Checkpoint),
    Quarantine {
        first_lsn: u64,
        last_lsn: u64,
        pairs: Vec<QuarantinePair>,
        error_code: i32,
        attempts: u16,
        quarantine_ref: String,
        reason: String,
    },
    Noop(Vec<u8>),
}

impl OwnedFact {
    fn fact(&self) -> Fact<'_> {
        Fact {
            fact_id: FactId::from_bytes(self.fact_id),
            target_kind: self.target_kind,
            trace_id: TraceId::from_bytes(self.trace_id),
            span_id: SpanId::from_bytes(self.span_id),
            session_id: &self.session_id,
            target_time: Timestamp::from_micros(self.target_time),
            key: &self.key,
            version: self.version,
            retracted: self.retracted,
            created_at: Timestamp::from_micros(self.created_at),
            body: match &self.body {
                OwnedBody::Score {
                    data_type,
                    value_bits,
                    value_str,
                    comment,
                    source,
                    metadata,
                    config_id,
                } => FactBody::Score(ScoreBody {
                    data_type: *data_type,
                    value_num: f64::from_bits(*value_bits),
                    value_str,
                    comment,
                    source: *source,
                    metadata,
                    config_id: config_id.map(Uuid::from_bytes),
                }),
                OwnedBody::Tag(op) => FactBody::Tag { op: *op },
                OwnedBody::Flag(value) => FactBody::Flag { value: *value },
                OwnedBody::Correction { corrected, note } => {
                    FactBody::Correction { corrected, note }
                }
            },
        }
    }
}

impl Owned {
    fn record(&self) -> Record<'_> {
        match self {
            Self::Ingest {
                project_id,
                api_key_id,
                compat_dialect,
                grpc,
                json,
                admin_import,
                sdk_name,
                sdk_version,
                idempotency_key,
                span_count,
                rejected,
                otlp,
            } => Record::IngestRequest(IngestRequest {
                project_id: ProjectId::from_bytes(*project_id),
                api_key_id: ApiKeyId::from_bytes(*api_key_id),
                compat_dialect: *compat_dialect,
                transport: if *grpc {
                    IngestTransport::Grpc
                } else {
                    IngestTransport::Http
                },
                content_origin: if *json {
                    ContentOrigin::Json
                } else {
                    ContentOrigin::Protobuf
                },
                admin_import: *admin_import,
                sdk_name,
                sdk_version,
                idempotency_key,
                span_count: *span_count,
                rejected,
                otlp,
            }),
            Self::Native(bytes) => Record::NativeBatch(bytes),
            Self::Facts {
                project_id,
                author_kind,
                author_id,
                facts,
            } => Record::FactBatch(FactBatch {
                project_id: ProjectId::from_bytes(*project_id),
                author_kind: *author_kind,
                author_id: Uuid::from_bytes(*author_id),
                facts: facts.iter().map(OwnedFact::fact).collect(),
            }),
            Self::Tombstone {
                deletion_request_id,
                project_id,
                requested_at,
                deletion_seq,
                selector,
            } => Record::Tombstone(Tombstone {
                deletion_request_id: DeletionRequestId::from_bytes(*deletion_request_id),
                project_id: ProjectId::from_bytes(*project_id),
                requested_at: Timestamp::from_micros(*requested_at),
                deletion_seq: *deletion_seq,
                selector: match selector {
                    OwnedSelector::Traces(traces) => TombstoneSelector::Traces(
                        traces.iter().copied().map(TraceId::from_bytes).collect(),
                    ),
                    OwnedSelector::User(id) => TombstoneSelector::User(id),
                    OwnedSelector::Session(id) => TombstoneSelector::Session(id),
                    OwnedSelector::Project => TombstoneSelector::Project,
                },
            }),
            Self::Seal(seal) => Record::BatchSeal(*seal),
            Self::Checkpoint(checkpoint) => Record::Checkpoint(*checkpoint),
            Self::Quarantine {
                first_lsn,
                last_lsn,
                pairs,
                error_code,
                attempts,
                quarantine_ref,
                reason,
            } => Record::Quarantine(Quarantine {
                first_lsn: Lsn::new(*first_lsn),
                last_lsn: Lsn::new(*last_lsn),
                pairs: pairs.clone(),
                error_code: *error_code,
                attempts: *attempts,
                quarantine_ref,
                reason,
            }),
            Self::Noop(bytes) => Record::Noop(bytes),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------------------------------------

/// Text of at most `max_chars` characters: mostly ASCII, sometimes any Unicode scalar (up to 4 bytes each).
fn text(max_chars: usize) -> impl Strategy<Value = String> {
    prop_oneof![
        3 => vec(proptest::char::range(' ', '~'), 0..=max_chars),
        1 => vec(any::<char>(), 0..=max_chars),
    ]
    .prop_map(|chars| chars.into_iter().collect())
}

fn bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        vec(any::<u8>(), 0..=max),
        // Compressible content, so that the compressed form is exercised.
        (any::<u8>(), 0..=max).prop_map(|(byte, len)| vec![byte; len]),
    ]
}

fn one_of<T: Copy + core::fmt::Debug + 'static>(all: &'static [T]) -> impl Strategy<Value = T> {
    proptest::sample::select(all)
}

fn body() -> impl Strategy<Value = OwnedBody> {
    prop_oneof![
        (
            one_of(ScoreDataType::ALL),
            any::<u64>(),
            text(64),
            text(64),
            one_of(ScoreSource::ALL),
            text(64),
            any::<Option<[u8; 16]>>(),
        )
            .prop_map(
                |(data_type, value_bits, value_str, comment, source, metadata, config_id)| {
                    OwnedBody::Score {
                        data_type,
                        value_bits,
                        value_str,
                        comment,
                        source,
                        metadata,
                        config_id,
                    }
                }
            ),
        one_of(TagOp::ALL).prop_map(OwnedBody::Tag),
        any::<bool>().prop_map(OwnedBody::Flag),
        (text(200), text(64))
            .prop_map(|(corrected, note)| OwnedBody::Correction { corrected, note }),
    ]
}

fn fact() -> impl Strategy<Value = OwnedFact> {
    (
        any::<([u8; 16], [u8; 16], [u8; 8])>(),
        one_of(FactTargetKind::ALL),
        text(32),
        any::<(i64, u64, bool, i64)>(),
        text(64),
        body(),
    )
        .prop_map(
            |(
                (fact_id, trace_id, span_id),
                target_kind,
                session_id,
                (target_time, version, retracted, created_at),
                key,
                body,
            )| OwnedFact {
                fact_id,
                target_kind,
                trace_id,
                span_id,
                session_id,
                target_time,
                key,
                version,
                retracted,
                created_at,
                body,
            },
        )
}

fn partition() -> impl Strategy<Value = PartitionId> {
    prop_oneof![
        Just(PartitionId::UNPARTITIONED),
        (0i64..47_482).prop_map(|days| PartitionId::date(CivilDate::from_day_number(days))),
        (1970i32..2100, 1u8..=12).prop_map(|(y, m)| PartitionId::month(y, m).unwrap()),
    ]
}

fn range() -> impl Strategy<Value = (u64, u64)> {
    (any::<u64>(), any::<u64>()).prop_map(|(a, b)| (a.min(b), a.max(b)))
}

fn owned() -> impl Strategy<Value = Owned> {
    let ingest = (
        any::<([u8; 16], [u8; 16], u8, bool, bool, bool)>(),
        text(32),
        text(16),
        bytes(128),
        any::<u32>(),
        bytes(32),
        bytes(600),
    )
        .prop_map(
            |(
                (project_id, api_key_id, compat_dialect, grpc, json, admin_import),
                sdk_name,
                sdk_version,
                idempotency_key,
                span_count,
                rejected,
                otlp,
            )| Owned::Ingest {
                project_id,
                api_key_id,
                compat_dialect,
                grpc,
                json,
                admin_import,
                sdk_name,
                sdk_version,
                idempotency_key,
                span_count,
                rejected,
                otlp,
            },
        );
    let facts = (
        any::<([u8; 16], [u8; 16])>(),
        one_of(AuthorKind::ALL),
        vec(fact(), 0..4),
    )
        .prop_map(
            |((project_id, author_id), author_kind, facts)| Owned::Facts {
                project_id,
                author_kind,
                author_id,
                facts,
            },
        );
    let selector = prop_oneof![
        vec(any::<[u8; 16]>(), 0..6).prop_map(OwnedSelector::Traces),
        text(40).prop_map(OwnedSelector::User),
        text(40).prop_map(OwnedSelector::Session),
        Just(OwnedSelector::Project),
    ];
    let tombstone = (any::<([u8; 16], [u8; 16], i64, u64)>(), selector).prop_map(
        |((deletion_request_id, project_id, requested_at, deletion_seq), selector)| {
            Owned::Tombstone {
                deletion_request_id,
                project_id,
                requested_at,
                deletion_seq,
                selector,
            }
        },
    );
    let seal = (range(), any::<(u32, u32, u32, u64)>()).prop_map(
        |((first, last), (frame_count, normalizer_version, price_table_version, deletion_seq))| {
            Owned::Seal(BatchSeal {
                first_lsn: Lsn::new(first),
                last_lsn: Lsn::new(last),
                frame_count,
                normalizer_version,
                price_table_version,
                deletion_seq,
            })
        },
    );
    let checkpoint = any::<(u64, u64, u64)>().prop_map(|(batch_seq, last_lsn, table_bitmap)| {
        Owned::Checkpoint(Checkpoint {
            batch_seq,
            last_lsn: Lsn::new(last_lsn),
            table_bitmap,
        })
    });
    let pair = (proptest::sample::select(&TableId::ALL[..]), partition()).prop_map(
        |(table_id, partition_id)| QuarantinePair {
            table_id,
            partition_id,
        },
    );
    let quarantine = (
        range(),
        vec(pair, 0..5),
        any::<(i32, u16)>(),
        text(40),
        text(120),
    )
        .prop_map(
            |((first_lsn, last_lsn), pairs, (error_code, attempts), quarantine_ref, reason)| {
                Owned::Quarantine {
                    first_lsn,
                    last_lsn,
                    pairs,
                    error_code,
                    attempts,
                    quarantine_ref,
                    reason,
                }
            },
        );
    prop_oneof![
        ingest,
        bytes(200).prop_map(Owned::Native),
        facts,
        tombstone,
        seal,
        checkpoint,
        quarantine,
        bytes(200).prop_map(Owned::Noop),
    ]
}

#[derive(Clone, Debug)]
struct OwnedFrame {
    record: Owned,
    lsn: u64,
    received_at: i64,
    lz4: bool,
}

fn frame() -> impl Strategy<Value = OwnedFrame> {
    (owned(), any::<u64>(), any::<i64>(), any::<bool>()).prop_map(
        |(record, lsn, received_at, lz4)| OwnedFrame {
            record,
            lsn,
            received_at,
            lz4,
        },
    )
}

// ---------------------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------------------

fn segment_header() -> SegmentHeader {
    SegmentHeader {
        wal_epoch: WalEpoch::from_bytes([7; 16]),
        segment_no: 1,
        first_lsn: Lsn::FIRST,
        created_at: Timestamp::from_micros(1),
    }
}

/// A segment holding `frames`, and the offset at which each frame starts plus the end of the last one.
fn build_segment(frames: &[OwnedFrame]) -> (Vec<u8>, Vec<usize>) {
    let mut segment = segment_header().encode().to_vec();
    let mut offsets = vec![segment.len()];
    for frame in frames {
        let payload = frame.record.record().encode().unwrap();
        let compression = if frame.lz4 {
            Compression::Lz4
        } else {
            Compression::None
        };
        let record_type = frame.record.record().record_type();
        let prepared = PreparedFrame::new(record_type, payload, compression).unwrap();
        let before = segment.len();
        prepared.write_to(
            Lsn::new(frame.lsn),
            Timestamp::from_micros(frame.received_at),
            &mut segment,
        );
        assert_eq!(segment.len() - before, prepared.encoded_len());
        offsets.push(segment.len());
    }
    (segment, offsets)
}

/// Checks that the first `count` frames of `segment` are exactly `frames[..count]` at `offsets`.
fn check_prefix(
    iter: &mut FrameIter<'_>,
    frames: &[OwnedFrame],
    offsets: &[usize],
    count: usize,
) -> Result<(), TestCaseError> {
    for (index, expected) in frames.iter().enumerate().take(count) {
        prop_assert_eq!(iter.offset(), offsets[index]);
        let frame = iter.next().expect("a frame").expect("no corruption");
        prop_assert_eq!(frame.offset(), offsets[index]);
        prop_assert_eq!(frame.end_offset(), offsets[index + 1]);
        prop_assert_eq!(frame.lsn(), Lsn::new(expected.lsn));
        prop_assert_eq!(
            frame.received_at(),
            Timestamp::from_micros(expected.received_at)
        );
        prop_assert_eq!(iter.last_lsn(), Some(Lsn::new(expected.lsn)));
        let record = expected.record.record();
        prop_assert_eq!(frame.record_type(), record.record_type());
        let payload = frame.payload().expect("payload loads");
        // Scores carry an f64 that may be NaN, so equality is checked on the canonical bytes as well.
        let decoded = Record::decode(frame.record_type(), &payload).expect("payload decodes");
        prop_assert_eq!(decoded.encode().unwrap(), record.encode().unwrap());
        prop_assert_eq!(payload.into_owned(), record.encode().unwrap());
    }
    Ok(())
}

fn check_round_trip(frames: &[OwnedFrame], zero_tail: usize) -> Result<(), TestCaseError> {
    let (mut segment, offsets) = build_segment(frames);
    let data_end = segment.len();
    segment.resize(data_end + zero_tail, 0);
    prop_assert_eq!(
        SegmentHeader::decode(segment.first_chunk().unwrap()),
        Ok(segment_header())
    );
    let mut iter = FrameIter::new(&segment, SEGMENT_HEADER_LEN);
    check_prefix(&mut iter, frames, &offsets, frames.len())?;
    prop_assert!(iter.next().is_none());
    let reason = if zero_tail == 0 {
        CleanEnd::EndOfSegment
    } else {
        CleanEnd::ZeroMagic
    };
    prop_assert_eq!(
        iter.end(),
        Some(SegmentEnd::Clean {
            offset: data_end,
            reason
        })
    );
    prop_assert_eq!(iter.offset(), data_end);
    prop_assert!(iter.next().is_none());
    Ok(())
}

/// A frame with arbitrary header bytes and valid checksums, for headers that the encoder refuses to write.
fn raw_frame(magic: &[u8; 4], version: u8, code: u8, flags: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(magic);
    frame.push(version);
    frame.push(code);
    frame.extend_from_slice(&flags.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&9u64.to_le_bytes());
    frame.extend_from_slice(&5i64.to_le_bytes());
    frame.extend_from_slice(&crc32c::crc32c(payload).to_le_bytes());
    let crc = crc32c::crc32c(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn end_of(segment: &[u8]) -> (usize, SegmentEnd) {
    let mut iter = FrameIter::new(segment, 0);
    let frames = iter.by_ref().filter(Result::is_ok).count();
    (frames, iter.end().unwrap())
}

// ---------------------------------------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------------------------------------

proptest! {
    #[test]
    fn frames_of_every_record_type_round_trip(frames in vec(frame(), 0..6), zero_tail in 0usize..8) {
        check_round_trip(&frames, zero_tail)?;
    }

    #[test]
    fn records_round_trip_by_value(record in owned()) {
        let record = record.record();
        let payload = record.encode().unwrap();
        let decoded = Record::decode(record.record_type(), &payload).unwrap();
        prop_assert_eq!(&decoded.encode().unwrap(), &payload);
        // Structural equality holds whenever no score holds a NaN.
        let has_nan = matches!(&record, Record::FactBatch(batch) if batch.facts.iter().any(
            |fact| matches!(fact.body, FactBody::Score(score) if score.value_num.is_nan())
        ));
        if !has_nan {
            prop_assert_eq!(decoded, record);
        }
    }

    /// A single flipped bit anywhere in the frames is caught by a checksum: the frames before the damaged
    /// one are intact and the damaged one is the torn end.
    #[test]
    fn a_flipped_bit_is_a_torn_end_at_the_damaged_frame(
        frames in vec(frame(), 1..5), position in any::<prop::sample::Index>(), bit in 0u8..8,
    ) {
        let (mut segment, offsets) = build_segment(&frames);
        let at = SEGMENT_HEADER_LEN + position.index(segment.len() - SEGMENT_HEADER_LEN);
        segment[at] ^= 1 << bit;
        let damaged = offsets.iter().rposition(|&start| start <= at).unwrap();
        let mut iter = FrameIter::new(&segment, SEGMENT_HEADER_LEN);
        check_prefix(&mut iter, &frames, &offsets, damaged)?;
        prop_assert!(iter.next().is_none());
        let reason = if at - offsets[damaged] < FRAME_HEADER_LEN {
            TornReason::HeaderCrc
        } else {
            TornReason::PayloadCrc
        };
        prop_assert_eq!(iter.end(), Some(SegmentEnd::Torn { offset: offsets[damaged], reason }));
    }

    /// A segment cut anywhere ends cleanly at a frame boundary or is torn at the cut frame.
    #[test]
    fn a_truncated_segment_is_torn_at_the_cut_frame(
        frames in vec(frame(), 1..5), position in any::<prop::sample::Index>(),
    ) {
        let (mut segment, offsets) = build_segment(&frames);
        let cut = SEGMENT_HEADER_LEN + position.index(segment.len() - SEGMENT_HEADER_LEN);
        segment.truncate(cut);
        let whole = offsets.iter().rposition(|&start| start <= cut).unwrap();
        let mut iter = FrameIter::new(&segment, SEGMENT_HEADER_LEN);
        check_prefix(&mut iter, &frames, &offsets, whole)?;
        prop_assert!(iter.next().is_none());
        let expected = if cut == offsets[whole] {
            SegmentEnd::Clean { offset: cut, reason: CleanEnd::EndOfSegment }
        } else if cut - offsets[whole] < FRAME_HEADER_LEN {
            SegmentEnd::Torn { offset: offsets[whole], reason: TornReason::ShortHeader }
        } else {
            SegmentEnd::Torn { offset: offsets[whole], reason: TornReason::ShortPayload }
        };
        prop_assert_eq!(iter.end(), Some(expected));
    }

    /// Arbitrary damage (overwritten, zeroed or inserted bytes): frames before the first changed byte come
    /// back identical, offsets only move forward, and the scan ends with a classification.
    #[test]
    fn arbitrary_damage_never_panics_and_keeps_the_intact_prefix(
        frames in vec(frame(), 1..5), position in any::<prop::sample::Index>(),
        junk in vec(any::<u8>(), 1..40), mode in 0u8..3,
    ) {
        let (pristine, offsets) = build_segment(&frames);
        let mut segment = pristine.clone();
        let at = SEGMENT_HEADER_LEN + position.index(segment.len() - SEGMENT_HEADER_LEN);
        match mode {
            0 => for (slot, byte) in segment[at..].iter_mut().zip(&junk) { *slot = *byte; },
            1 => segment[at..].fill(0),
            _ => { segment.splice(at..at, junk.iter().copied()); }
        }
        let first_change = pristine.iter().zip(&segment).position(|(a, b)| a != b).unwrap_or(pristine.len());
        let intact = offsets.iter().skip(1).take_while(|&&end| end <= first_change).count();
        let mut iter = FrameIter::new(&segment, SEGMENT_HEADER_LEN);
        check_prefix(&mut iter, &frames, &offsets, intact)?;
        let mut last = iter.offset();
        let mut steps = 0;
        for item in iter.by_ref() {
            steps += 1;
            prop_assert!(steps <= segment.len());
            match item {
                Ok(frame) => {
                    prop_assert!(frame.offset() == last && frame.end_offset() > last);
                    prop_assert!(frame.end_offset() <= segment.len());
                    last = frame.end_offset();
                    if let Ok(payload) = frame.payload() {
                        let _ = Record::decode(frame.record_type(), &payload);
                    }
                }
                Err(corruption) => prop_assert_eq!(corruption.offset, last),
            }
        }
        prop_assert_eq!(iter.end().map(|end| end.offset()), Some(last));
        prop_assert!(iter.next().is_none());
    }

    #[test]
    fn arbitrary_bytes_never_panic(data in vec(any::<u8>(), 0..300), offset in 0usize..320) {
        let mut iter = FrameIter::new(&data, offset);
        prop_assert!(iter.by_ref().count() <= data.len());
        prop_assert!(iter.end().is_some());
        let _ = scan_frame(&data, offset);
        for record_type in RecordType::ALL {
            // The codec is canonical: whatever decodes re-encodes to the same bytes.
            if let Ok(record) = Record::decode(record_type, &data) {
                prop_assert_eq!(record.encode().unwrap(), data.clone());
            }
        }
    }

    /// A damaged payload is refused or decodes to some record; a strict prefix and an extension of a
    /// versioned payload are always refused.
    #[test]
    fn damaged_payloads_are_refused_or_decoded(
        record in owned(), position in any::<prop::sample::Index>(), byte in any::<u8>(),
        extra in vec(any::<u8>(), 1..8),
    ) {
        let record = record.record();
        let record_type = record.record_type();
        let payload = record.encode().unwrap();
        let opaque = matches!(record_type, RecordType::NativeBatch | RecordType::Noop);
        if !payload.is_empty() {
            let at = position.index(payload.len());
            let mut changed = payload.clone();
            changed[at] = byte;
            if let Ok(decoded) = Record::decode(record_type, &changed) {
                prop_assert_eq!(decoded.encode().unwrap(), changed);
            }
            prop_assert_eq!(Record::decode(record_type, &payload[..at]).is_ok(), opaque);
        }
        let mut longer = payload.clone();
        longer.extend_from_slice(&extra);
        let result = Record::decode(record_type, &longer);
        if opaque {
            prop_assert!(result.is_ok());
        } else {
            prop_assert_eq!(result.err(), Some(PayloadError::TrailingBytes(extra.len())));
        }
    }

    #[test]
    fn header_bit_flips_and_round_trips(
        code in 1u8..=8, compressed in any::<bool>(), payload_len in 0u32..=MAX_PAYLOAD,
        lsn in any::<u64>(), received_at in any::<i64>(), payload_crc32c in any::<u32>(),
        bit in 0usize..FRAME_HEADER_LEN * 8,
    ) {
        let header = FrameHeader {
            record_type: RecordType::from_code(code).unwrap(),
            compressed,
            payload_len,
            lsn: Lsn::new(lsn),
            received_at: Timestamp::from_micros(received_at),
            payload_crc32c,
        };
        let mut bytes = header.encode().unwrap();
        prop_assert_eq!(FrameHeader::decode(&bytes), Ok(header));
        bytes[bit / 8] ^= 1 << (bit % 8);
        prop_assert_eq!(FrameHeader::decode(&bytes), Err(FrameHeaderError::HeaderCrcMismatch));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000_000))]

    /// The one-off large run: `cargo test --release -p crucible-wal-codec --test codec -- --ignored`.
    #[test]
    #[ignore = "one million cases; run on demand"]
    fn one_million_frames_round_trip(frame in frame(), zero_tail in 0usize..8) {
        check_round_trip(core::slice::from_ref(&frame), zero_tail)?;
    }
}

// ---------------------------------------------------------------------------------------------------------
// Examples
// ---------------------------------------------------------------------------------------------------------

#[test]
fn frame_header_layout() {
    let header = FrameHeader {
        record_type: RecordType::BatchSeal,
        compressed: true,
        payload_len: 0x0102_0304,
        lsn: Lsn::new(0x1122_3344_5566_7788),
        received_at: Timestamp::from_micros(-2),
        payload_crc32c: 0xAABB_CCDD,
    };
    let bytes = header.encode().unwrap();
    assert_eq!(&bytes[0..4], b"CWF1");
    assert_eq!(bytes[4], 1);
    assert_eq!(bytes[5], 5);
    assert_eq!(&bytes[6..8], &[1, 0]);
    assert_eq!(&bytes[8..12], &[4, 3, 2, 1]);
    assert_eq!(
        &bytes[12..20],
        &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
    );
    assert_eq!(&bytes[20..28], &(-2i64).to_le_bytes());
    assert_eq!(&bytes[28..32], &[0xDD, 0xCC, 0xBB, 0xAA]);
    assert_eq!(&bytes[32..36], &crc32c::crc32c(&bytes[..32]).to_le_bytes());
    assert_eq!(FrameHeader::decode(&bytes), Ok(header));
}

#[test]
fn frame_header_typed_errors() {
    let decode = |frame: Vec<u8>| FrameHeader::decode(frame.first_chunk().unwrap());
    assert_eq!(
        decode(raw_frame(b"CWF2", 1, 1, 0, b"")),
        Err(FrameHeaderError::BadMagic)
    );
    for version in [0, 2, 255] {
        assert_eq!(
            decode(raw_frame(b"CWF1", version, 1, 0, b"")),
            Err(FrameHeaderError::UnsupportedVersion(version))
        );
    }
    for code in [0, 9, 255] {
        assert_eq!(
            decode(raw_frame(b"CWF1", 1, code, 0, b"")),
            Err(FrameHeaderError::UnknownRecordType(code))
        );
    }
    for flags in [2, 3, 0x8000] {
        assert_eq!(
            decode(raw_frame(b"CWF1", 1, 1, flags, b"")),
            Err(FrameHeaderError::ReservedFlag(flags))
        );
    }
    let mut oversized = raw_frame(b"CWF1", 1, 8, 0, b"");
    oversized[8..12].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
    assert_eq!(
        decode(oversized.clone()),
        Err(FrameHeaderError::HeaderCrcMismatch)
    );
    let crc = crc32c::crc32c(&oversized[..32]);
    oversized[32..36].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        decode(oversized),
        Err(FrameHeaderError::PayloadTooLarge(MAX_PAYLOAD + 1))
    );
    let too_large = FrameHeader {
        record_type: RecordType::Noop,
        compressed: false,
        payload_len: MAX_PAYLOAD + 1,
        lsn: Lsn::FIRST,
        received_at: Timestamp::EPOCH,
        payload_crc32c: 0,
    };
    assert_eq!(
        too_large.encode(),
        Err(FrameHeaderError::PayloadTooLarge(MAX_PAYLOAD + 1))
    );
    let at_cap = FrameHeader {
        payload_len: MAX_PAYLOAD,
        ..too_large
    };
    assert_eq!(FrameHeader::decode(&at_cap.encode().unwrap()), Ok(at_cap));
}

/// An unacceptable header is fatal when both checksums are valid and a torn tail when the payload is not
/// intact (contract sections 4.3 and 4.6).
#[test]
fn unacceptable_headers_are_fatal_only_with_both_checksums_valid() {
    let good = raw_frame(b"CWF1", 1, 8, 0, b"pad");
    let cases: [(Vec<u8>, FrameHeaderError); 4] = [
        (
            raw_frame(b"CWF1", 1, 9, 0, b"abc"),
            FrameHeaderError::UnknownRecordType(9),
        ),
        (
            raw_frame(b"CWF1", 2, 1, 0, b"abc"),
            FrameHeaderError::UnsupportedVersion(2),
        ),
        (
            raw_frame(b"CWF1", 1, 1, 2, b"abc"),
            FrameHeaderError::ReservedFlag(2),
        ),
        (
            raw_frame(b"XXXX", 1, 1, 0, b"abc"),
            FrameHeaderError::BadMagic,
        ),
    ];
    for (bad, error) in cases {
        let mut segment = good.clone();
        segment.extend_from_slice(&bad);
        let offset = good.len();
        assert_eq!(end_of(&segment), (1, SegmentEnd::Corrupt { offset, error }));
        let mut iter = FrameIter::new(&segment, 0);
        assert!(iter.next().unwrap().is_ok());
        let corruption = iter.next().unwrap().unwrap_err();
        assert_eq!((corruption.offset, corruption.error), (offset, error));
        assert!(iter.next().is_none());

        let mut flipped = segment.clone();
        *flipped.last_mut().unwrap() ^= 1;
        let reason = TornReason::PayloadCrc;
        assert_eq!(end_of(&flipped), (1, SegmentEnd::Torn { offset, reason }));

        let short = &segment[..segment.len() - 1];
        let reason = TornReason::ShortPayload;
        assert_eq!(end_of(short), (1, SegmentEnd::Torn { offset, reason }));
    }
}

#[test]
fn an_oversized_length_with_a_valid_header_checksum_is_fatal() {
    let mut frame = raw_frame(b"CWF1", 1, 8, 0, b"");
    frame[8..12].copy_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
    let crc = crc32c::crc32c(&frame[..32]);
    frame[32..36].copy_from_slice(&crc.to_le_bytes());
    let error = FrameHeaderError::PayloadTooLarge(MAX_PAYLOAD + 1);
    assert_eq!(
        end_of(&frame),
        (0, SegmentEnd::Corrupt { offset: 0, error })
    );
}

#[test]
fn clean_ends_and_short_tails() {
    let frame = raw_frame(b"CWF1", 1, 8, 0, b"");
    assert_eq!(frame.len(), FRAME_HEADER_LEN);
    let clean = |offset, reason| SegmentEnd::Clean { offset, reason };
    assert_eq!(end_of(&frame), (1, clean(36, CleanEnd::EndOfSegment)));
    assert_eq!(end_of(&[]), (0, clean(0, CleanEnd::EndOfSegment)));
    for zeros in 1..=5 {
        let mut segment = frame.clone();
        segment.resize(36 + zeros, 0);
        assert_eq!(end_of(&segment), (1, clean(36, CleanEnd::ZeroMagic)));
    }
    // Zero magic ends the data even when later bytes are not zero.
    let mut segment = frame.clone();
    segment.extend_from_slice(&[0, 0, 0, 0, 9, 9, 9]);
    assert_eq!(end_of(&segment), (1, clean(36, CleanEnd::ZeroMagic)));
    let mut segment = frame.clone();
    segment.extend_from_slice(&[0, 0, 1]);
    let reason = TornReason::ShortHeader;
    assert_eq!(
        end_of(&segment),
        (1, SegmentEnd::Torn { offset: 36, reason })
    );
    // An offset beyond the segment reads as its end.
    assert!(matches!(
        scan_frame(&frame, 99),
        Scan::End(SegmentEnd::Clean {
            offset: 36,
            reason: CleanEnd::EndOfSegment
        })
    ));
}

#[test]
fn payload_layouts_match_the_contract() {
    let seal = Record::BatchSeal(BatchSeal {
        first_lsn: Lsn::new(3),
        last_lsn: Lsn::new(4),
        frame_count: 2,
        normalizer_version: 5,
        price_table_version: 6,
        deletion_seq: 7,
    });
    let mut expected = vec![1u8];
    for value in [3u64, 3, 4] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    for value in [2u32, 5, 6] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    expected.extend_from_slice(&7u64.to_le_bytes());
    assert_eq!(seal.encode().unwrap(), expected);

    let checkpoint = Record::Checkpoint(Checkpoint {
        batch_seq: 1,
        last_lsn: Lsn::new(2),
        table_bitmap: Checkpoint::ALL_TABLES,
    });
    assert_eq!(Checkpoint::ALL_TABLES, 0xF_FFFE);
    let mut expected = vec![1u8];
    for value in [1u64, 2, 0xF_FFFE] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(checkpoint.encode().unwrap(), expected);

    let request = IngestRequest {
        project_id: ProjectId::from_bytes([0x11; 16]),
        api_key_id: ApiKeyId::from_bytes([0x22; 16]),
        compat_dialect: 3,
        transport: IngestTransport::Grpc,
        content_origin: ContentOrigin::Json,
        admin_import: true,
        sdk_name: "sdk",
        sdk_version: "1.0",
        idempotency_key: b"k",
        span_count: 9,
        rejected: &[0b0000_0101, 0b1],
        otlp: b"otlp",
    };
    let mut expected = vec![1u8];
    expected.extend_from_slice(&[0x11; 16]);
    expected.extend_from_slice(&[0x22; 16]);
    expected.extend_from_slice(&[3, 2, 2, 1]);
    expected.extend_from_slice(b"\x03\0\0\0sdk\x03\0\0\x001.0\x01\0\0\0k");
    expected.extend_from_slice(&9u32.to_le_bytes());
    expected.extend_from_slice(&[2, 0, 0, 0, 0b101, 1]);
    expected.extend_from_slice(b"\x04\0\0\0otlp");
    assert_eq!(Record::IngestRequest(request).encode().unwrap(), expected);
    let rejected: Vec<u32> = (0..20).filter(|&i| request.is_rejected(i)).collect();
    assert_eq!(rejected, [0, 2, 8]);

    let quarantine = Record::Quarantine(Quarantine {
        first_lsn: Lsn::new(1),
        last_lsn: Lsn::new(1),
        pairs: vec![QuarantinePair {
            table_id: TableId::from_id(2).unwrap(),
            partition_id: PartitionId::parse("202611").unwrap(),
        }],
        error_code: -1,
        attempts: 3,
        quarantine_ref: "q",
        reason: "r",
    });
    let mut expected = vec![1u8];
    for value in [1u64, 1, 1] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    expected.extend_from_slice(b"\x01\0\0\0\x02\0\x06\0\0\x00202611");
    expected.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 3, 0]);
    expected.extend_from_slice(b"\x01\0\0\0q\x01\0\0\0r");
    assert_eq!(quarantine.encode().unwrap(), expected);

    let tombstone = Record::Tombstone(Tombstone {
        deletion_request_id: DeletionRequestId::from_bytes([1; 16]),
        project_id: ProjectId::from_bytes([2; 16]),
        requested_at: Timestamp::from_micros(3),
        deletion_seq: 4,
        selector: TombstoneSelector::Traces(vec![TraceId::from_bytes([5; 16])]),
    });
    let mut expected = vec![1u8];
    expected.extend_from_slice(&[1; 16]);
    expected.extend_from_slice(&[2; 16]);
    expected.push(1);
    expected.extend_from_slice(&3i64.to_le_bytes());
    expected.extend_from_slice(&4u64.to_le_bytes());
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(&[5; 16]);
    assert_eq!(tombstone.encode().unwrap(), expected);

    let facts = Record::FactBatch(FactBatch {
        project_id: ProjectId::from_bytes([1; 16]),
        author_kind: AuthorKind::ApiKey,
        author_id: Uuid::from_bytes([2; 16]),
        facts: vec![Fact {
            fact_id: FactId::from_bytes([3; 16]),
            target_kind: FactTargetKind::Span,
            trace_id: TraceId::from_bytes([4; 16]),
            span_id: SpanId::from_bytes([5; 8]),
            session_id: "",
            target_time: Timestamp::from_micros(6),
            key: "tag",
            version: 7,
            retracted: true,
            created_at: Timestamp::from_micros(8),
            body: FactBody::Tag { op: TagOp::Remove },
        }],
    });
    let mut expected = vec![1u8];
    expected.extend_from_slice(&[1; 16]);
    expected.push(2);
    expected.extend_from_slice(&[2; 16]);
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.push(2);
    expected.extend_from_slice(&[3; 16]);
    expected.push(2);
    expected.extend_from_slice(&[4; 16]);
    expected.extend_from_slice(&[5; 8]);
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&6i64.to_le_bytes());
    expected.extend_from_slice(b"\x03\0\0\0tag");
    expected.extend_from_slice(&7u64.to_le_bytes());
    expected.push(1);
    expected.extend_from_slice(&8i64.to_le_bytes());
    expected.extend_from_slice(&[1, 0, 0, 0, 2]);
    assert_eq!(facts.encode().unwrap(), expected);

    assert_eq!(Record::Noop(b"xy").encode().unwrap(), b"xy");
    assert_eq!(Record::NativeBatch(b"").encode().unwrap(), b"");
}

#[test]
fn payload_typed_errors() {
    let seal = Record::BatchSeal(BatchSeal {
        first_lsn: Lsn::new(3),
        last_lsn: Lsn::new(4),
        frame_count: 2,
        normalizer_version: 5,
        price_table_version: 6,
        deletion_seq: 7,
    });
    let good = seal.encode().unwrap();
    let decode = |bytes: &[u8]| Record::decode(RecordType::BatchSeal, bytes).err();

    let mut bytes = good.clone();
    bytes[0] = 2;
    assert_eq!(decode(&bytes), Some(PayloadError::UnsupportedVersion(2)));
    bytes[0] = 0;
    assert_eq!(decode(&bytes), Some(PayloadError::UnsupportedVersion(0)));
    let mut bytes = good.clone();
    bytes.push(0);
    assert_eq!(decode(&bytes), Some(PayloadError::TrailingBytes(1)));
    assert_eq!(
        decode(&[]),
        Some(PayloadError::Truncated("payload_version"))
    );
    assert_eq!(
        decode(&good[..good.len() - 1]),
        Some(PayloadError::Truncated("deletion_seq"))
    );
    // `batch_seq` must equal `first_lsn`, and the range must not be empty.
    let mut bytes = good.clone();
    bytes[1] = 9;
    assert_eq!(
        decode(&bytes),
        Some(PayloadError::InvalidValue("batch_seq"))
    );
    let mut bytes = good.clone();
    bytes[17] = 2;
    assert_eq!(decode(&bytes), Some(PayloadError::InvalidValue("last_lsn")));
    let empty_range = Record::BatchSeal(BatchSeal {
        first_lsn: Lsn::new(5),
        last_lsn: Lsn::new(4),
        frame_count: 0,
        normalizer_version: 0,
        price_table_version: 0,
        deletion_seq: 0,
    });
    assert_eq!(
        empty_range.encode(),
        Err(PayloadError::InvalidValue("last_lsn"))
    );

    let request = IngestRequest {
        project_id: ProjectId::from_bytes([1; 16]),
        api_key_id: ApiKeyId::from_bytes([2; 16]),
        compat_dialect: 0,
        transport: IngestTransport::Http,
        content_origin: ContentOrigin::Protobuf,
        admin_import: false,
        sdk_name: "",
        sdk_version: "",
        idempotency_key: b"",
        span_count: 0,
        rejected: b"",
        otlp: b"",
    };
    let encode = |request: IngestRequest<'_>| Record::IngestRequest(request).encode().err();
    let long = "x".repeat(129);
    let too_long = |field, len, max| Some(PayloadError::TooLong { field, len, max });
    assert_eq!(
        encode(IngestRequest {
            sdk_name: &long,
            ..request
        }),
        too_long("sdk_name", 129, 128)
    );
    assert_eq!(
        encode(IngestRequest {
            sdk_version: &long[..65],
            ..request
        }),
        too_long("sdk_version", 65, 64)
    );
    assert_eq!(
        encode(IngestRequest {
            idempotency_key: long.as_bytes(),
            ..request
        }),
        too_long("idempotency_key", 129, 128)
    );
    // The envelope cap counts the payload version and the length prefix of `otlp`.
    let fixed = Record::IngestRequest(request).encode().unwrap().len();
    let rejected = vec![0u8; MAX_ENVELOPE_BYTES - fixed];
    let at_cap = IngestRequest {
        rejected: &rejected,
        otlp: b"body",
        ..request
    };
    let payload = Record::IngestRequest(at_cap).encode().unwrap();
    assert_eq!(payload.len(), MAX_ENVELOPE_BYTES + 4);
    assert!(Record::decode(RecordType::IngestRequest, &payload).is_ok());
    let rejected = vec![0u8; MAX_ENVELOPE_BYTES - fixed + 1];
    assert_eq!(
        encode(IngestRequest {
            rejected: &rejected,
            ..request
        }),
        too_long("envelope", MAX_ENVELOPE_BYTES + 1, MAX_ENVELOPE_BYTES)
    );
    // The same payload written by hand is refused by the decoder too.
    let mut over = payload.clone();
    let rejected_len_at = fixed - 8;
    let new_len = (MAX_ENVELOPE_BYTES - fixed + 1) as u32;
    over[rejected_len_at..rejected_len_at + 4].copy_from_slice(&new_len.to_le_bytes());
    over.insert(rejected_len_at + 4, 0);
    assert_eq!(
        Record::decode(RecordType::IngestRequest, &over).err(),
        too_long("envelope", MAX_ENVELOPE_BYTES + 1, MAX_ENVELOPE_BYTES)
    );

    let ingest = Record::IngestRequest(request).encode().unwrap();
    let decode = |bytes: &[u8]| Record::decode(RecordType::IngestRequest, bytes).err();
    for (index, field) in [(34, "transport"), (35, "content_origin"), (36, "flags")] {
        for value in [0u8, 3, 255] {
            if (index, value) == (36, 0) {
                continue;
            }
            let mut bytes = ingest.clone();
            bytes[index] = value;
            assert_eq!(decode(&bytes), Some(PayloadError::InvalidValue(field)));
        }
    }
    let mut bytes = ingest.clone();
    bytes[37] = 1;
    bytes.insert(41, 0xFF);
    assert_eq!(decode(&bytes), Some(PayloadError::InvalidUtf8("sdk_name")));

    // List counts are checked against their caps and against the bytes that remain.
    let traces = vec![TraceId::from_bytes([0; 16]); MAX_TOMBSTONE_TRACES + 1];
    let tombstone = |selector| {
        Record::Tombstone(Tombstone {
            deletion_request_id: DeletionRequestId::from_bytes([1; 16]),
            project_id: ProjectId::from_bytes([2; 16]),
            requested_at: Timestamp::EPOCH,
            deletion_seq: 0,
            selector,
        })
    };
    assert_eq!(
        tombstone(TombstoneSelector::Traces(traces.clone()))
            .encode()
            .err(),
        too_long("selector", MAX_TOMBSTONE_TRACES + 1, MAX_TOMBSTONE_TRACES)
    );
    let mut bytes = tombstone(TombstoneSelector::Traces(traces[..1].to_vec()))
        .encode()
        .unwrap();
    let count_at = bytes.len() - 20;
    bytes[count_at..count_at + 4].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(
        Record::decode(RecordType::Tombstone, &bytes).err(),
        Some(PayloadError::Truncated("selector"))
    );
    bytes[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        Record::decode(RecordType::Tombstone, &bytes).err(),
        too_long("selector", u32::MAX as usize, MAX_TOMBSTONE_TRACES)
    );
    let mut bytes = tombstone(TombstoneSelector::Project).encode().unwrap();
    bytes[33] = 5;
    assert_eq!(
        Record::decode(RecordType::Tombstone, &bytes).err(),
        Some(PayloadError::InvalidValue("kind"))
    );

    let fact = Fact {
        fact_id: FactId::from_bytes([3; 16]),
        target_kind: FactTargetKind::Trace,
        trace_id: TraceId::from_bytes([4; 16]),
        span_id: SpanId::ZERO,
        session_id: "",
        target_time: Timestamp::EPOCH,
        key: "bookmarked",
        version: 1,
        retracted: false,
        created_at: Timestamp::EPOCH,
        body: FactBody::Flag { value: true },
    };
    let batch = |facts| {
        Record::FactBatch(FactBatch {
            project_id: ProjectId::from_bytes([1; 16]),
            author_kind: AuthorKind::User,
            author_id: Uuid::from_bytes([2; 16]),
            facts,
        })
    };
    assert_eq!(
        batch(vec![fact; MAX_FACTS + 1]).encode().err(),
        too_long("facts", MAX_FACTS + 1, MAX_FACTS)
    );
    assert!(batch(vec![fact; MAX_FACTS]).encode().is_ok());
    let long_key = "k".repeat(257);
    assert_eq!(
        batch(vec![Fact {
            key: &long_key,
            ..fact
        }])
        .encode()
        .err(),
        too_long("key", 257, 256)
    );
    let bytes = batch(vec![fact]).encode().unwrap();
    let decode = |bytes: &[u8]| Record::decode(RecordType::FactBatch, bytes).err();
    let mut bad = bytes.clone();
    bad[38] = 5;
    assert_eq!(decode(&bad), Some(PayloadError::InvalidValue("table")));
    // A tag body where the table says flag: the body value is outside the flag's domain.
    let mut bad = bytes.clone();
    *bad.last_mut().unwrap() = 2;
    assert_eq!(decode(&bad), Some(PayloadError::InvalidValue("value")));
    // Trailing bytes inside a fact body are refused as well.
    let mut bad = bytes.clone();
    let body_len_at = bad.len() - 5;
    bad[body_len_at] = 2;
    bad.push(0);
    assert_eq!(decode(&bad), Some(PayloadError::TrailingBytes(1)));
}

#[test]
fn compressed_frames_decode_through_the_iterator() {
    let otlp = vec![b'a'; 100_000];
    let request = Record::IngestRequest(IngestRequest {
        project_id: ProjectId::from_bytes([1; 16]),
        api_key_id: ApiKeyId::from_bytes([2; 16]),
        compat_dialect: 0,
        transport: IngestTransport::Http,
        content_origin: ContentOrigin::Protobuf,
        admin_import: false,
        sdk_name: "sdk",
        sdk_version: "1",
        idempotency_key: b"",
        span_count: 1,
        rejected: b"",
        otlp: &otlp,
    });
    let prepared = PreparedFrame::new(
        RecordType::IngestRequest,
        request.encode().unwrap(),
        Compression::Lz4,
    )
    .unwrap();
    assert!(prepared.is_compressed());
    assert!(prepared.encoded_len() < 2_000);
    let mut segment = Vec::new();
    prepared.write_to(Lsn::FIRST, Timestamp::from_micros(42), &mut segment);
    let mut iter = FrameIter::new(&segment, 0);
    let frame = iter.next().unwrap().unwrap();
    assert!(frame.header().compressed);
    assert!(frame.header().payload_matches(frame.stored_payload()));
    let payload = frame.payload().unwrap();
    assert_eq!(
        Record::decode(frame.record_type(), &payload).unwrap(),
        request
    );
    assert!(iter.next().is_none());
}
