//! Payload layouts of the eight record types (contract section 4.4).
//!
//! Decoded records borrow byte strings and text from the payload, so the large OTLP body of an
//! `IngestRequest` is never copied. Encoders and decoders enforce the same caps and value domains: a record
//! that encodes always decodes to an equal record, and a payload that decodes re-encodes to the same bytes.

use crucible_records::{
    ApiKeyId, AuthorKind, DeletionKind, DeletionRequestId, FactId, FactTargetKind, Lsn,
    PartitionId, ProjectId, ScoreDataType, ScoreSource, SpanId, TableId, TagOp, Timestamp, TraceId,
    Uuid,
};

use crate::frame::{MAX_PAYLOAD, RecordType};
use crate::wire::{PayloadError, Reader, check_cap, check_region, put_bytes, put_len};

/// The only payload version.
pub const PAYLOAD_VERSION: u8 = 1;

/// Largest canonical OTLP body of one request: 64 MiB minus the 64 KiB envelope reserve.
pub const MAX_OTLP_BYTES: usize = 67_043_328;
/// Cap on an `IngestRequest` payload without its OTLP bytes: the payload version, the envelope and the
/// length prefix of `otlp`.
pub const MAX_ENVELOPE_BYTES: usize = 65_536;
/// Cap on `sdk_name`.
pub const MAX_SDK_NAME_BYTES: usize = 128;
/// Cap on `sdk_version`.
pub const MAX_SDK_VERSION_BYTES: usize = 64;
/// Cap on `idempotency_key`.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// Cap on the facts of one `FactBatch`.
pub const MAX_FACTS: usize = 1_000;
/// Cap on the uncompressed payload of one `FactBatch`.
pub const MAX_FACT_BATCH_BYTES: usize = 16 * 1024 * 1024;
/// Cap on a fact's `key`.
pub const MAX_FACT_KEY_BYTES: usize = 256;
/// Cap on a score's `value_str`.
pub const MAX_SCORE_VALUE_BYTES: usize = 1_024;
/// Cap on a score's `comment` and on a correction's `note`.
pub const MAX_NOTE_BYTES: usize = 4_096;
/// Cap on a score's `metadata`.
pub const MAX_SCORE_METADATA_BYTES: usize = 16_384;
/// Cap on a correction's `corrected`.
pub const MAX_CORRECTED_BYTES: usize = 8 * 1024 * 1024;
/// Cap on the trace ids of one trace tombstone.
pub const MAX_TOMBSTONE_TRACES: usize = 10_000;
/// Cap on a quarantine's `reason`.
pub const MAX_QUARANTINE_REASON_BYTES: usize = 512;

/// For fields that the contract bounds only through the payload cap.
const UNCAPPED: usize = MAX_PAYLOAD as usize;

type Result<T> = core::result::Result<T, PayloadError>;

/// How an ingest request arrived.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum IngestTransport {
    /// OTLP over HTTP.
    Http = 1,
    /// OTLP over gRPC.
    Grpc = 2,
}

/// The encoding in which an ingest request body arrived, before transcoding to protobuf.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum ContentOrigin {
    /// Protobuf.
    Protobuf = 1,
    /// JSON, transcoded before the append.
    Json = 2,
}

/// One accepted OTLP export request: the envelope and the canonical protobuf body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IngestRequest<'a> {
    /// Project of the authenticated credential.
    pub project_id: ProjectId,
    /// The API key that authenticated the request.
    pub api_key_id: ApiKeyId,
    /// Request-level dialect hint.
    pub compat_dialect: u8,
    /// How the request arrived.
    pub transport: IngestTransport,
    /// The encoding in which the body arrived.
    pub content_origin: ContentOrigin,
    /// Flag bit 0: the request came through an admin import key.
    pub admin_import: bool,
    /// SDK name, at most 128 bytes.
    pub sdk_name: &'a str,
    /// SDK version, at most 64 bytes.
    pub sdk_version: &'a str,
    /// Idempotency key, at most 128 bytes; empty when absent.
    pub idempotency_key: &'a [u8],
    /// Spans in the body, in wire order.
    pub span_count: u32,
    /// Bitmap over span ordinals in wire order; a set bit marks a span rejected before the ack.
    pub rejected: &'a [u8],
    /// The canonical OTLP protobuf `ExportTraceServiceRequest`.
    pub otlp: &'a [u8],
}

impl<'a> IngestRequest<'a> {
    /// Whether the span at `ordinal` (wire order) was rejected before the ack and must be skipped: bit
    /// `ordinal % 8`, least significant first, of byte `ordinal / 8`. Ordinals beyond the bitmap are not
    /// rejected.
    pub fn is_rejected(&self, ordinal: u32) -> bool {
        self.rejected
            .get((ordinal / 8) as usize)
            .is_some_and(|byte| byte & (1 << (ordinal % 8)) != 0)
    }

    fn encode(&self, start: usize, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&self.project_id.to_bytes());
        out.extend_from_slice(&self.api_key_id.to_bytes());
        out.push(self.compat_dialect);
        out.push(self.transport as u8);
        out.push(self.content_origin as u8);
        out.push(u8::from(self.admin_import));
        let (name, version) = (self.sdk_name.as_bytes(), self.sdk_version.as_bytes());
        put_bytes(out, "sdk_name", name, MAX_SDK_NAME_BYTES)?;
        put_bytes(out, "sdk_version", version, MAX_SDK_VERSION_BYTES)?;
        let key = self.idempotency_key;
        put_bytes(out, "idempotency_key", key, MAX_IDEMPOTENCY_KEY_BYTES)?;
        out.extend_from_slice(&self.span_count.to_le_bytes());
        put_bytes(out, "rejected", self.rejected, MAX_ENVELOPE_BYTES)?;
        put_len(out, "otlp", self.otlp.len(), MAX_OTLP_BYTES)?;
        check_region(out, start, "envelope", MAX_ENVELOPE_BYTES)?;
        out.extend_from_slice(self.otlp);
        Ok(())
    }

    fn decode(r: &mut Reader<'a>) -> Result<Self> {
        let project_id = ProjectId::from_bytes(r.array("project_id")?);
        let api_key_id = ApiKeyId::from_bytes(r.array("api_key_id")?);
        let compat_dialect = r.u8("compat_dialect")?;
        let transport = match r.u8("transport")? {
            1 => IngestTransport::Http,
            2 => IngestTransport::Grpc,
            _ => return Err(PayloadError::InvalidValue("transport")),
        };
        let content_origin = match r.u8("content_origin")? {
            1 => ContentOrigin::Protobuf,
            2 => ContentOrigin::Json,
            _ => return Err(PayloadError::InvalidValue("content_origin")),
        };
        // Only flag bit 0 is defined, so the flags byte is 0 or 1.
        let admin_import = r.bool("flags")?;
        let sdk_name = r.str("sdk_name", MAX_SDK_NAME_BYTES)?;
        let sdk_version = r.str("sdk_version", MAX_SDK_VERSION_BYTES)?;
        let idempotency_key = r.bytes("idempotency_key", MAX_IDEMPOTENCY_KEY_BYTES)?;
        let span_count = r.u32("span_count")?;
        let rejected = r.bytes("rejected", MAX_ENVELOPE_BYTES)?;
        // The envelope region includes the four-byte length prefix of `otlp`, read next.
        let envelope = r.consumed().saturating_add(4);
        check_cap("envelope", envelope, MAX_ENVELOPE_BYTES)?;
        let otlp = r.bytes("otlp", MAX_OTLP_BYTES)?;
        Ok(Self {
            project_id,
            api_key_id,
            compat_dialect,
            transport,
            content_origin,
            admin_import,
            sdk_name,
            sdk_version,
            idempotency_key,
            span_count,
            rejected,
            otlp,
        })
    }
}

/// Facts written by one author in one API call.
#[derive(Clone, PartialEq, Debug)]
pub struct FactBatch<'a> {
    /// Project of the facts.
    pub project_id: ProjectId,
    /// Kind of the author.
    pub author_kind: AuthorKind,
    /// Id of the author.
    pub author_id: Uuid,
    /// At most 1,000 facts.
    pub facts: Vec<Fact<'a>>,
}

/// One fact (contract section 3.8). Its table code on the wire follows from the body.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Fact<'a> {
    /// Server-generated id.
    pub fact_id: FactId,
    /// What the fact is about.
    pub target_kind: FactTargetKind,
    /// Target trace; zero when unused.
    pub trace_id: TraceId,
    /// Target span; zero when unused.
    pub span_id: SpanId,
    /// Target session; empty when unused.
    pub session_id: &'a str,
    /// Start time of the target.
    pub target_time: Timestamp,
    /// What latest-wins resolves over; at most 256 bytes.
    pub key: &'a str,
    /// Server microsecond timestamp at acceptance.
    pub version: u64,
    /// Whether this row retracts the key.
    pub retracted: bool,
    /// Creation time.
    pub created_at: Timestamp,
    /// The body, which also selects the fact table.
    pub body: FactBody<'a>,
}

/// Body of a fact. The wire table codes are 1 score, 2 tag, 3 flag, 4 correction.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FactBody<'a> {
    /// A score (`fact_scores`).
    Score(ScoreBody<'a>),
    /// A tag edit (`fact_tags`).
    Tag {
        /// Add or remove.
        op: TagOp,
    },
    /// A flag edit (`fact_flags`).
    Flag {
        /// The flag's value.
        value: bool,
    },
    /// A correction (`fact_corrections`).
    Correction {
        /// Corrected output as JSON text, at most 8 MiB.
        corrected: &'a str,
        /// Note, at most 4 KiB.
        note: &'a str,
    },
}

/// Body of a score fact.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ScoreBody<'a> {
    /// Data type of the score.
    pub data_type: ScoreDataType,
    /// Numeric value, stored as its IEEE 754 bits.
    pub value_num: f64,
    /// String value, at most 1 KiB.
    pub value_str: &'a str,
    /// Comment, at most 4 KiB.
    pub comment: &'a str,
    /// Where the score came from.
    pub source: ScoreSource,
    /// Metadata as JSON text, at most 16 KiB.
    pub metadata: &'a str,
    /// Score configuration, when one applies. Encoded as a presence byte (0 or 1), then the id when present.
    pub config_id: Option<Uuid>,
}

/// Smallest encoded fact: every fixed field, three empty length prefixes and a one-byte body.
const MIN_FACT_BYTES: usize = 80;

impl<'a> Fact<'a> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.push(match self.body {
            FactBody::Score(_) => 1,
            FactBody::Tag { .. } => 2,
            FactBody::Flag { .. } => 3,
            FactBody::Correction { .. } => 4,
        });
        out.extend_from_slice(&self.fact_id.to_bytes());
        out.push(self.target_kind.value());
        out.extend_from_slice(self.trace_id.as_bytes());
        out.extend_from_slice(self.span_id.as_bytes());
        put_bytes(out, "session_id", self.session_id.as_bytes(), UNCAPPED)?;
        out.extend_from_slice(&self.target_time.micros().to_le_bytes());
        put_bytes(out, "key", self.key.as_bytes(), MAX_FACT_KEY_BYTES)?;
        out.extend_from_slice(&self.version.to_le_bytes());
        out.push(u8::from(self.retracted));
        out.extend_from_slice(&self.created_at.micros().to_le_bytes());
        let mut body = Vec::new();
        match self.body {
            FactBody::Score(score) => {
                body.push(score.data_type.value());
                body.extend_from_slice(&score.value_num.to_bits().to_le_bytes());
                let value = score.value_str.as_bytes();
                put_bytes(&mut body, "value_str", value, MAX_SCORE_VALUE_BYTES)?;
                put_bytes(
                    &mut body,
                    "comment",
                    score.comment.as_bytes(),
                    MAX_NOTE_BYTES,
                )?;
                body.push(score.source.value());
                let metadata = score.metadata.as_bytes();
                put_bytes(&mut body, "metadata", metadata, MAX_SCORE_METADATA_BYTES)?;
                match score.config_id {
                    None => body.push(0),
                    Some(id) => {
                        body.push(1);
                        body.extend_from_slice(id.as_bytes());
                    }
                }
            }
            FactBody::Tag { op } => body.push(op.value()),
            FactBody::Flag { value } => body.push(u8::from(value)),
            FactBody::Correction { corrected, note } => {
                put_bytes(
                    &mut body,
                    "corrected",
                    corrected.as_bytes(),
                    MAX_CORRECTED_BYTES,
                )?;
                put_bytes(&mut body, "note", note.as_bytes(), MAX_NOTE_BYTES)?;
            }
        }
        put_bytes(out, "body", &body, UNCAPPED)
    }

    fn decode(r: &mut Reader<'a>) -> Result<Self> {
        let invalid = PayloadError::InvalidValue;
        let table = r.u8("table")?;
        let fact_id = FactId::from_bytes(r.array("fact_id")?);
        let target_kind =
            FactTargetKind::from_value(r.u8("target_kind")?).ok_or(invalid("target_kind"))?;
        let trace_id = TraceId::from_bytes(r.array("trace_id")?);
        let span_id = SpanId::from_bytes(r.array("span_id")?);
        let session_id = r.str("session_id", UNCAPPED)?;
        let target_time = Timestamp::from_micros(r.i64("target_time")?);
        let key = r.str("key", MAX_FACT_KEY_BYTES)?;
        let version = r.u64("version")?;
        let retracted = r.bool("retracted")?;
        let created_at = Timestamp::from_micros(r.i64("created_at")?);
        let mut b = Reader::new(r.bytes("body", UNCAPPED)?);
        let body = match table {
            1 => FactBody::Score(ScoreBody {
                data_type: ScoreDataType::from_value(b.u8("data_type")?)
                    .ok_or(invalid("data_type"))?,
                value_num: f64::from_bits(b.u64("value_num")?),
                value_str: b.str("value_str", MAX_SCORE_VALUE_BYTES)?,
                comment: b.str("comment", MAX_NOTE_BYTES)?,
                source: ScoreSource::from_value(b.u8("source")?).ok_or(invalid("source"))?,
                metadata: b.str("metadata", MAX_SCORE_METADATA_BYTES)?,
                config_id: if b.bool("config_id")? {
                    Some(Uuid::from_bytes(b.array("config_id")?))
                } else {
                    None
                },
            }),
            2 => FactBody::Tag {
                op: TagOp::from_value(b.u8("op")?).ok_or(invalid("op"))?,
            },
            3 => FactBody::Flag {
                value: b.bool("value")?,
            },
            4 => FactBody::Correction {
                corrected: b.str("corrected", MAX_CORRECTED_BYTES)?,
                note: b.str("note", MAX_NOTE_BYTES)?,
            },
            _ => return Err(invalid("table")),
        };
        b.finish()?;
        Ok(Self {
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
        })
    }
}

impl<'a> FactBatch<'a> {
    fn encode(&self, start: usize, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&self.project_id.to_bytes());
        out.push(self.author_kind.value());
        out.extend_from_slice(self.author_id.as_bytes());
        put_len(out, "facts", self.facts.len(), MAX_FACTS)?;
        for fact in &self.facts {
            fact.encode(out)?;
            check_region(out, start, "fact_batch", MAX_FACT_BATCH_BYTES)?;
        }
        Ok(())
    }

    fn decode(r: &mut Reader<'a>, payload_len: usize) -> Result<Self> {
        check_cap("fact_batch", payload_len, MAX_FACT_BATCH_BYTES)?;
        let project_id = ProjectId::from_bytes(r.array("project_id")?);
        let author_kind = AuthorKind::from_value(r.u8("author_kind")?)
            .ok_or(PayloadError::InvalidValue("author_kind"))?;
        let author_id = Uuid::from_bytes(r.array("author_id")?);
        let count = r.count("facts", MAX_FACTS, MIN_FACT_BYTES)?;
        let mut facts = Vec::with_capacity(count);
        for _ in 0..count {
            facts.push(Fact::decode(r)?);
        }
        Ok(Self {
            project_id,
            author_kind,
            author_id,
            facts,
        })
    }
}

/// A deletion request.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tombstone<'a> {
    /// The deletion request.
    pub deletion_request_id: DeletionRequestId,
    /// Project whose data is deleted.
    pub project_id: ProjectId,
    /// When the deletion was requested.
    pub requested_at: Timestamp,
    /// Deployment-wide deletion sequence.
    pub deletion_seq: u64,
    /// What to delete; also selects the `kind` byte.
    pub selector: TombstoneSelector<'a>,
}

/// What a tombstone deletes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TombstoneSelector<'a> {
    /// These traces (at most 10,000).
    Traces(Vec<TraceId>),
    /// Everything of this user.
    User(&'a str),
    /// This session.
    Session(&'a str),
    /// The whole project.
    Project,
}

impl TombstoneSelector<'_> {
    /// The deletion kind this selector belongs to.
    pub fn kind(&self) -> DeletionKind {
        match self {
            Self::Traces(_) => DeletionKind::Trace,
            Self::User(_) => DeletionKind::User,
            Self::Session(_) => DeletionKind::Session,
            Self::Project => DeletionKind::Project,
        }
    }
}

impl<'a> Tombstone<'a> {
    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&self.deletion_request_id.to_bytes());
        out.extend_from_slice(&self.project_id.to_bytes());
        out.push(self.selector.kind().value());
        out.extend_from_slice(&self.requested_at.micros().to_le_bytes());
        out.extend_from_slice(&self.deletion_seq.to_le_bytes());
        match &self.selector {
            TombstoneSelector::Traces(traces) => {
                put_len(out, "selector", traces.len(), MAX_TOMBSTONE_TRACES)?;
                for trace in traces {
                    out.extend_from_slice(trace.as_bytes());
                }
                Ok(())
            }
            TombstoneSelector::User(id) | TombstoneSelector::Session(id) => {
                put_bytes(out, "selector", id.as_bytes(), UNCAPPED)
            }
            TombstoneSelector::Project => Ok(()),
        }
    }

    fn decode(r: &mut Reader<'a>) -> Result<Self> {
        let deletion_request_id = DeletionRequestId::from_bytes(r.array("deletion_request_id")?);
        let project_id = ProjectId::from_bytes(r.array("project_id")?);
        let kind =
            DeletionKind::from_value(r.u8("kind")?).ok_or(PayloadError::InvalidValue("kind"))?;
        let requested_at = Timestamp::from_micros(r.i64("requested_at")?);
        let deletion_seq = r.u64("deletion_seq")?;
        let selector = match kind {
            DeletionKind::Trace => {
                let count = r.count("selector", MAX_TOMBSTONE_TRACES, 16)?;
                let mut traces = Vec::with_capacity(count);
                for _ in 0..count {
                    traces.push(TraceId::from_bytes(r.array("selector")?));
                }
                TombstoneSelector::Traces(traces)
            }
            DeletionKind::User => TombstoneSelector::User(r.str("selector", UNCAPPED)?),
            DeletionKind::Session => TombstoneSelector::Session(r.str("selector", UNCAPPED)?),
            DeletionKind::Project => TombstoneSelector::Project,
        };
        Ok(Self {
            deletion_request_id,
            project_id,
            requested_at,
            deletion_seq,
            selector,
        })
    }
}

/// Reads `batch_seq`, `first_lsn` and `last_lsn`, which open both `BatchSeal` and `Quarantine`. `batch_seq`
/// must equal `first_lsn` (contract section 5.1) and the range must not be empty.
fn decode_range(r: &mut Reader<'_>) -> Result<(Lsn, Lsn)> {
    let batch_seq = r.u64("batch_seq")?;
    let first_lsn = r.u64("first_lsn")?;
    let last_lsn = r.u64("last_lsn")?;
    if batch_seq != first_lsn {
        return Err(PayloadError::InvalidValue("batch_seq"));
    }
    if first_lsn > last_lsn {
        return Err(PayloadError::InvalidValue("last_lsn"));
    }
    Ok((Lsn::new(first_lsn), Lsn::new(last_lsn)))
}

fn encode_range(first_lsn: Lsn, last_lsn: Lsn, out: &mut Vec<u8>) -> Result<()> {
    if first_lsn > last_lsn {
        return Err(PayloadError::InvalidValue("last_lsn"));
    }
    // `batch_seq`, then `first_lsn`: the same value.
    out.extend_from_slice(&first_lsn.get().to_le_bytes());
    out.extend_from_slice(&first_lsn.get().to_le_bytes());
    out.extend_from_slice(&last_lsn.get().to_le_bytes());
    Ok(())
}

/// The durable boundaries of a batch and the versions that pin its replay (contract sections 5.1 and 5.3).
/// The wire `batch_seq` always equals `first_lsn`, so it is not a separate field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BatchSeal {
    /// First LSN of the batch; also its `batch_seq`.
    pub first_lsn: Lsn,
    /// Last LSN of the batch, at or above `first_lsn`.
    pub last_lsn: Lsn,
    /// Frames in the range.
    pub frame_count: u32,
    /// Normalizer version that sealed the batch.
    pub normalizer_version: u32,
    /// Price table version the batch must be normalized with.
    pub price_table_version: u32,
    /// Highest tombstone sequence applied while normalizing the range.
    pub deletion_seq: u64,
}

impl BatchSeal {
    /// The batch sequence number, which is the first LSN.
    pub fn batch_seq(&self) -> u64 {
        self.first_lsn.get()
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        encode_range(self.first_lsn, self.last_lsn, out)?;
        out.extend_from_slice(&self.frame_count.to_le_bytes());
        out.extend_from_slice(&self.normalizer_version.to_le_bytes());
        out.extend_from_slice(&self.price_table_version.to_le_bytes());
        out.extend_from_slice(&self.deletion_seq.to_le_bytes());
        Ok(())
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let (first_lsn, last_lsn) = decode_range(r)?;
        Ok(Self {
            first_lsn,
            last_lsn,
            frame_count: r.u32("frame_count")?,
            normalizer_version: r.u32("normalizer_version")?,
            price_table_version: r.u32("price_table_version")?,
            deletion_seq: r.u64("deletion_seq")?,
        })
    }
}

/// The low watermark: every frame with an LSN at or below `last_lsn` is resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checkpoint {
    /// `batch_seq` of the last resolved batch.
    pub batch_seq: u64,
    /// The watermark.
    pub last_lsn: Lsn,
    /// Bit `table_id` set for every table; kept for format stability.
    pub table_bitmap: u64,
}

impl Checkpoint {
    /// The bitmap with the bit of every known table set.
    pub const ALL_TABLES: u64 = {
        let mut bitmap = 0u64;
        let mut i = 0;
        while i < TableId::ALL.len() {
            bitmap |= 1 << TableId::ALL[i].id();
            i += 1;
        }
        bitmap
    };

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.batch_seq.to_le_bytes());
        out.extend_from_slice(&self.last_lsn.get().to_le_bytes());
        out.extend_from_slice(&self.table_bitmap.to_le_bytes());
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            batch_seq: r.u64("batch_seq")?,
            last_lsn: Lsn::new(r.u64("last_lsn")?),
            table_bitmap: r.u64("table_bitmap")?,
        })
    }
}

/// One (table, partition) pair of a batch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct QuarantinePair {
    /// The table.
    pub table_id: TableId,
    /// The partition.
    pub partition_id: PartitionId,
}

/// Pairs of a batch that were moved to quarantine and count as resolved (contract section 5.6). The wire
/// `batch_seq` always equals `first_lsn`, so it is not a separate field.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Quarantine<'a> {
    /// First LSN of the batch; also its `batch_seq`.
    pub first_lsn: Lsn,
    /// Last LSN of the batch, at or above `first_lsn`.
    pub last_lsn: Lsn,
    /// The failing pairs.
    pub pairs: Vec<QuarantinePair>,
    /// ClickHouse error code of the failure.
    pub error_code: i32,
    /// Attempts made before quarantining.
    pub attempts: u16,
    /// Directory name under the quarantine root.
    pub quarantine_ref: &'a str,
    /// Human-readable reason, at most 512 bytes.
    pub reason: &'a str,
}

/// Smallest encoded pair: the table id and an empty partition id.
const MIN_PAIR_BYTES: usize = 6;
/// Longest partition id, `YYYYMMDD`.
const MAX_PARTITION_ID_BYTES: usize = 8;

impl<'a> Quarantine<'a> {
    /// The batch sequence number, which is the first LSN.
    pub fn batch_seq(&self) -> u64 {
        self.first_lsn.get()
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        encode_range(self.first_lsn, self.last_lsn, out)?;
        put_len(out, "pairs", self.pairs.len(), UNCAPPED)?;
        for pair in &self.pairs {
            out.extend_from_slice(&u16::from(pair.table_id.id()).to_le_bytes());
            let partition = pair.partition_id.to_string();
            put_bytes(
                out,
                "partition_id",
                partition.as_bytes(),
                MAX_PARTITION_ID_BYTES,
            )?;
        }
        out.extend_from_slice(&self.error_code.to_le_bytes());
        out.extend_from_slice(&self.attempts.to_le_bytes());
        put_bytes(
            out,
            "quarantine_ref",
            self.quarantine_ref.as_bytes(),
            UNCAPPED,
        )?;
        let reason = self.reason.as_bytes();
        put_bytes(out, "reason", reason, MAX_QUARANTINE_REASON_BYTES)
    }

    fn decode(r: &mut Reader<'a>) -> Result<Self> {
        let (first_lsn, last_lsn) = decode_range(r)?;
        let count = r.count("pairs", UNCAPPED, MIN_PAIR_BYTES)?;
        let mut pairs = Vec::with_capacity(count);
        for _ in 0..count {
            let table_id = u8::try_from(r.u16("table_id")?)
                .ok()
                .and_then(TableId::from_id)
                .ok_or(PayloadError::InvalidValue("table_id"))?;
            let partition_id = PartitionId::parse(r.str("partition_id", MAX_PARTITION_ID_BYTES)?)
                .map_err(|_| PayloadError::InvalidValue("partition_id"))?;
            pairs.push(QuarantinePair {
                table_id,
                partition_id,
            });
        }
        Ok(Self {
            first_lsn,
            last_lsn,
            pairs,
            error_code: r.i32("error_code")?,
            attempts: r.u16("attempts")?,
            quarantine_ref: r.str("quarantine_ref", UNCAPPED)?,
            reason: r.str("reason", MAX_QUARANTINE_REASON_BYTES)?,
        })
    }
}

/// The decoded payload of a frame.
#[derive(Clone, PartialEq, Debug)]
pub enum Record<'a> {
    /// One accepted OTLP export request.
    IngestRequest(IngestRequest<'a>),
    /// Reserved. The payload is opaque: its layout is not frozen, and readers skip the frame.
    NativeBatch(&'a [u8]),
    /// Facts written through the API.
    FactBatch(FactBatch<'a>),
    /// A deletion request.
    Tombstone(Tombstone<'a>),
    /// The durable boundaries of a batch.
    BatchSeal(BatchSeal),
    /// The low watermark.
    Checkpoint(Checkpoint),
    /// Pairs of a batch that were moved to quarantine.
    Quarantine(Quarantine<'a>),
    /// Padding: arbitrary bytes, ignored.
    Noop(&'a [u8]),
}

impl<'a> Record<'a> {
    /// The record type code of this payload.
    pub fn record_type(&self) -> RecordType {
        match self {
            Self::IngestRequest(_) => RecordType::IngestRequest,
            Self::NativeBatch(_) => RecordType::NativeBatch,
            Self::FactBatch(_) => RecordType::FactBatch,
            Self::Tombstone(_) => RecordType::Tombstone,
            Self::BatchSeal(_) => RecordType::BatchSeal,
            Self::Checkpoint(_) => RecordType::Checkpoint,
            Self::Quarantine(_) => RecordType::Quarantine,
            Self::Noop(_) => RecordType::Noop,
        }
    }

    /// Encodes the uncompressed payload. Fails when a field exceeds its cap, a batch range is empty, or the
    /// payload exceeds [`MAX_PAYLOAD`].
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.encode_into(&mut out)?;
        Ok(out)
    }

    /// Appends the uncompressed payload to `out`. On error `out` holds a partial payload.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<()> {
        let start = out.len();
        if is_versioned(self.record_type()) {
            out.push(PAYLOAD_VERSION);
        }
        match self {
            Self::IngestRequest(request) => request.encode(start, out)?,
            Self::FactBatch(batch) => batch.encode(start, out)?,
            Self::Tombstone(tombstone) => tombstone.encode(out)?,
            Self::BatchSeal(seal) => seal.encode(out)?,
            Self::Checkpoint(checkpoint) => checkpoint.encode(out),
            Self::Quarantine(quarantine) => quarantine.encode(out)?,
            Self::NativeBatch(bytes) | Self::Noop(bytes) => out.extend_from_slice(bytes),
        }
        check_region(out, start, "payload", MAX_PAYLOAD as usize)
    }

    /// Decodes the uncompressed payload of a frame of type `record_type`. Trailing bytes, an unknown payload
    /// version, a value outside its domain and a field above its cap are typed errors.
    pub fn decode(record_type: RecordType, payload: &'a [u8]) -> Result<Self> {
        check_cap("payload", payload.len(), MAX_PAYLOAD as usize)?;
        let mut r = Reader::new(payload);
        if is_versioned(record_type) {
            let version = r.u8("payload_version")?;
            if version != PAYLOAD_VERSION {
                return Err(PayloadError::UnsupportedVersion(version));
            }
        }
        let record = match record_type {
            RecordType::IngestRequest => Self::IngestRequest(IngestRequest::decode(&mut r)?),
            RecordType::NativeBatch => Self::NativeBatch(r.take_rest()),
            RecordType::FactBatch => Self::FactBatch(FactBatch::decode(&mut r, payload.len())?),
            RecordType::Tombstone => Self::Tombstone(Tombstone::decode(&mut r)?),
            RecordType::BatchSeal => Self::BatchSeal(BatchSeal::decode(&mut r)?),
            RecordType::Checkpoint => Self::Checkpoint(Checkpoint::decode(&mut r)?),
            RecordType::Quarantine => Self::Quarantine(Quarantine::decode(&mut r)?),
            RecordType::Noop => Self::Noop(r.take_rest()),
        };
        r.finish()?;
        Ok(record)
    }
}

/// `NativeBatch` (layout not frozen) and `Noop` (arbitrary bytes) are opaque; every other payload starts
/// with the payload version.
fn is_versioned(record_type: RecordType) -> bool {
    !matches!(record_type, RecordType::NativeBatch | RecordType::Noop)
}
