//! What a durable frame yields: the records of an `IngestRequest` frame (stage A) or the facts of a `FactBatch`
//! frame, or the reason this binary cannot read it (contract 5.6, frame quarantine).
//!
//! Everything here is a pure function of the frame, the normalizer context and the shipper's configuration, so
//! a replay of a frame decides the same as the first read.

use crucible_normalize::{IngestFrame, NormCtx, NormalizedFrame, normalize};
use crucible_records::{
    CorrectionBody, Fact, FactBody, FactEnvelope, FlagBody, Ingestion, Lsn, ProjectId, ScoreBody,
    TableId, TagBody,
};
use crucible_schema::route;
use crucible_wal::{Wal, WalFrame};
use crucible_wal_codec::{self as codec, Record, RecordType};

use crate::config::ShipperConfig;

/// The rows a frame carries, before they are routed.
#[derive(Debug)]
pub(crate) enum Content {
    /// The records of an `IngestRequest` frame, before stage B.
    Spans(NormalizedFrame),
    /// The facts of a `FactBatch` frame.
    Facts(Vec<Fact>),
}

/// Why this binary cannot turn a durable frame into rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Unshippable {
    /// The frame's project, when its envelope decodes.
    pub(crate) project: Option<ProjectId>,
    /// Whether the frame is a `FactBatch`.
    pub(crate) facts: bool,
    /// The step that failed: `decode`, `normalize` or `route`.
    pub(crate) stage: &'static str,
    /// The error.
    pub(crate) reason: String,
}

fn fact_record(batch: &codec::FactBatch<'_>, fact: &codec::Fact<'_>, ingestion: Ingestion) -> Fact {
    let body = match fact.body {
        codec::FactBody::Score(score) => FactBody::Score(ScoreBody {
            data_type: score.data_type,
            value_num: score.value_num,
            value_str: score.value_str.into(),
            comment: score.comment.into(),
            source: score.source,
            metadata: bytes::Bytes::copy_from_slice(score.metadata.as_bytes()),
            config_id: score.config_id,
        }),
        codec::FactBody::Tag { op } => FactBody::Tag(TagBody { op }),
        codec::FactBody::Flag { value } => FactBody::Flag(FlagBody { value }),
        codec::FactBody::Correction { corrected, note } => FactBody::Correction(CorrectionBody {
            corrected: bytes::Bytes::copy_from_slice(corrected.as_bytes()),
            note: note.into(),
        }),
    };
    Fact {
        envelope: FactEnvelope {
            project_id: batch.project_id,
            fact_id: fact.fact_id,
            target_kind: fact.target_kind,
            trace_id: fact.trace_id,
            span_id: fact.span_id,
            session_id: fact.session_id.into(),
            target_time: fact.target_time,
            key: fact.key.into(),
            version: fact.version,
            retracted: fact.retracted,
            author_kind: batch.author_kind,
            author_id: batch.author_id,
            created_at: fact.created_at,
            ingestion,
        },
        body,
    }
}

/// Reads one `IngestRequest` or `FactBatch` frame: decodes it and, for a request, runs stage A. Every row must
/// route (contract 5.2), so that nothing after this step can fail for the frame's data.
pub(crate) fn read_frame(
    wal: &Wal,
    frame: &WalFrame,
    ctx: &NormCtx<'_>,
    config: &ShipperConfig,
) -> Result<Content, Unshippable> {
    let lsn = frame.lsn();
    let facts = frame.record_type() == RecordType::FactBatch;
    let fail = |project, stage, reason: String| Unshippable {
        project,
        facts,
        stage,
        reason,
    };
    let payload = frame
        .payload()
        .map_err(|e| fail(None, "decode", e.to_string()))?;
    let record = Record::decode(frame.record_type(), &payload)
        .map_err(|e| fail(None, "decode", e.to_string()))?;
    let injected = config.frame_fault.is_some_and(|f| f.fails(lsn.get()));
    let ingestion = Ingestion {
        wal_epoch: wal.epoch(),
        batch_seq: Lsn::ZERO,
        lsn,
        received_at: frame.received_at(),
    };
    match record {
        Record::IngestRequest(request) => {
            let project = Some(request.project_id);
            if injected {
                return Err(fail(project, "normalize", "injected fault".to_owned()));
            }
            let ingest = IngestFrame {
                wal_epoch: ingestion.wal_epoch,
                lsn,
                received_at: ingestion.received_at,
                project_id: request.project_id,
                api_key_id: request.api_key_id,
                compat_dialect: request.compat_dialect,
                sdk_name: request.sdk_name,
                sdk_version: request.sdk_version,
                rejected: request.rejected,
                otlp: request.otlp,
            };
            let normalized =
                normalize(&ingest, ctx).map_err(|e| fail(project, "normalize", e.to_string()))?;
            for span in &normalized.spans {
                route(TableId::SpanCore, config.granularity, span.core.start_time)
                    .map_err(|e| fail(project, "route", e.to_string()))?;
            }
            Ok(Content::Spans(normalized))
        }
        Record::FactBatch(batch) => {
            let project = Some(batch.project_id);
            if injected {
                return Err(fail(project, "normalize", "injected fault".to_owned()));
            }
            let facts: Vec<Fact> = batch
                .facts
                .iter()
                .map(|fact| fact_record(&batch, fact, ingestion))
                .collect();
            for fact in &facts {
                route(
                    fact.body.table(),
                    config.granularity,
                    fact.envelope.target_time,
                )
                .map_err(|e| fail(project, "route", e.to_string()))?;
            }
            Ok(Content::Facts(facts))
        }
        _ => Err(fail(
            None,
            "decode",
            "not a frame that carries rows".to_owned(),
        )),
    }
}
