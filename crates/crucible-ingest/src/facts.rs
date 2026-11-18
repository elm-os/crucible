//! The fact write path before the ack (contracts 3.8, 4.4 and 4.8): validation, admission, `FactBatch` frames
//! through the WAL's group commit, and the acknowledgement after the fsync.

use crucible_env::metrics::registry;
use crucible_env::{Counter, Env};
use crucible_records::{
    AuthorKind, FactBody, FactId, FactTargetKind, Lsn, ProjectScope, SpanId, Timestamp, TraceId,
    Uuid, caps,
};
use crucible_wal_codec::{
    self as codec, FRAME_HEADER_LEN, MAX_FACT_BATCH_BYTES, MAX_FACTS, PreparedFrame, Record,
    RecordType,
};

use crate::reply::Reject;
use crate::service::Ingest;

/// One fact as a caller hands it in. The server adds the id, the version and the creation time.
#[derive(Clone, PartialEq, Debug)]
pub struct NewFact {
    /// What the fact is attached to.
    pub target_kind: FactTargetKind,
    /// The target trace: required for a trace or span target, zero for a session target.
    pub trace_id: TraceId,
    /// The target span: required for a span target, zero otherwise.
    pub span_id: SpanId,
    /// The target session: required for a session target, empty otherwise.
    pub session_id: Box<str>,
    /// `start_time` of the target, which routes the fact to its partition.
    pub target_time: Timestamp,
    /// What latest-wins resolves over.
    pub key: Box<str>,
    /// Whether the fact retracts its key.
    pub retracted: bool,
    /// The body, which names the fact table.
    pub body: FactBody,
}

/// Who writes a batch of facts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactAuthor {
    /// The kind of author.
    pub kind: AuthorKind,
    /// The user or API key.
    pub id: Uuid,
}

/// What the server assigned to an accepted fact, in the order of the write.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AcceptedFact {
    /// The fact's id.
    pub fact_id: FactId,
    /// The fact's version: the server's wall clock in microseconds at acceptance.
    pub version: u64,
    /// The fact's creation time.
    pub created_at: Timestamp,
    /// The LSN of the durable frame that holds the fact.
    pub lsn: Lsn,
}

/// Why a fact write is refused. Nothing of a refused write is acknowledged; after `Unavailable` some of its
/// frames may be durable, and a retry writes new facts that win over them.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum FactWriteError {
    /// A fact is not well formed (`validation_failed`).
    #[error("fact {index}: {reason}")]
    Invalid {
        /// The position of the fact in the write.
        index: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A fact is above a size cap (`payload_too_large`).
    #[error("fact {index}: {reason}")]
    TooLarge {
        /// The position of the fact in the write.
        index: usize,
        /// The cap it exceeds.
        reason: &'static str,
    },
    /// The in-flight byte budget or the project's byte rate is exhausted (429).
    #[error("{detail}")]
    Throttled {
        /// Seconds for `Retry-After`.
        retry_after_s: u32,
        /// The setting that refused the write.
        detail: &'static str,
    },
    /// The WAL cannot take the write, or failed to make it durable (`ingest_unavailable`, 503).
    #[error("{detail}")]
    Unavailable {
        /// Seconds for `Retry-After`.
        retry_after_s: u32,
        /// The condition that refused the write.
        detail: &'static str,
    },
    /// A failure that the write did not cause.
    #[error("{0}")]
    Internal(&'static str),
}

impl FactWriteError {
    const OUTCOMES: [&'static str; 5] = [
        "validation_failed",
        "payload_too_large",
        "rate_limited",
        "unavailable",
        "internal",
    ];

    const fn outcome(&self) -> usize {
        match self {
            Self::Invalid { .. } => 0,
            Self::TooLarge { .. } => 1,
            Self::Throttled { .. } => 2,
            Self::Unavailable { .. } => 3,
            Self::Internal(_) => 4,
        }
    }
}

impl From<Reject> for FactWriteError {
    fn from(reject: Reject) -> Self {
        match reject {
            Reject::TooManyRequests {
                retry_after_s,
                detail,
            } => Self::Throttled {
                retry_after_s,
                detail,
            },
            Reject::Unavailable {
                retry_after_s,
                detail,
            } => Self::Unavailable {
                retry_after_s,
                detail,
            },
            Reject::Internal(detail) => Self::Internal(detail),
            _ => Self::Internal("the WAL refused the fact frame"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct FactMetrics {
    accepted: Counter,
    refused: [Counter; FactWriteError::OUTCOMES.len()],
    facts: Counter,
}

impl FactMetrics {
    pub(crate) fn new(env: &Env) -> Self {
        let m = env.metrics.as_ref();
        let writes = |outcome| m.counter(&registry::INGEST_FACT_WRITES, &[outcome]);
        Self {
            accepted: writes("accepted"),
            refused: FactWriteError::OUTCOMES.map(writes),
            facts: m.counter(&registry::INGEST_FACTS, &[]),
        }
    }
}

/// The body as the WAL codec takes it, or the reason it does not fit.
fn wire_body(body: &FactBody) -> Result<codec::FactBody<'_>, (bool, &'static str)> {
    fn text<'a>(bytes: &'a [u8], reason: &'static str) -> Result<&'a str, (bool, &'static str)> {
        std::str::from_utf8(bytes).map_err(|_| (false, reason))
    }
    let large = |reason| (true, reason);
    Ok(match body {
        FactBody::Score(score) => {
            if score.value_str.len() > caps::SCORE_VALUE_STR_BYTES {
                return Err(large("value_str is above 1 KiB"));
            }
            if score.comment.len() > caps::SCORE_COMMENT_BYTES {
                return Err(large("comment is above 4 KiB"));
            }
            if score.metadata.len() > caps::SCORE_METADATA_BYTES {
                return Err(large("metadata is above 16 KiB"));
            }
            codec::FactBody::Score(codec::ScoreBody {
                data_type: score.data_type,
                value_num: score.value_num,
                value_str: &score.value_str,
                comment: &score.comment,
                source: score.source,
                metadata: text(&score.metadata, "metadata is not UTF-8")?,
                config_id: score.config_id,
            })
        }
        FactBody::Tag(tag) => codec::FactBody::Tag { op: tag.op },
        FactBody::Flag(flag) => codec::FactBody::Flag { value: flag.value },
        FactBody::Correction(correction) => {
            if correction.corrected.len() > caps::CORRECTION_BYTES {
                return Err(large("corrected is above 8 MiB"));
            }
            if correction.note.len() > caps::CORRECTION_NOTE_BYTES {
                return Err(large("note is above 4 KiB"));
            }
            codec::FactBody::Correction {
                corrected: text(&correction.corrected, "corrected is not UTF-8")?,
                note: &correction.note,
            }
        }
    })
}

/// Checks one fact (contract 3.8) and returns its wire body and its encoded size, give or take a few bytes.
fn check(fact: &NewFact) -> Result<(codec::FactBody<'_>, usize), (bool, &'static str)> {
    let invalid = |reason| Err((false, reason));
    if fact.key.is_empty() {
        return invalid("key is empty");
    }
    if fact.key.len() > caps::FACT_KEY_BYTES {
        return Err((true, "key is above 256 bytes"));
    }
    if fact.session_id.len() > caps::TRACE_ATTRIBUTE_BYTES {
        return Err((true, "session_id is above 512 bytes"));
    }
    let (trace, span, session) = (
        fact.trace_id != TraceId::ZERO,
        fact.span_id != SpanId::ZERO,
        !fact.session_id.is_empty(),
    );
    let fits = match fact.target_kind {
        FactTargetKind::Trace => trace && !span && !session,
        FactTargetKind::Span => trace && span && !session,
        FactTargetKind::Session => !trace && !span && session,
    };
    if !fits {
        return invalid("the ids do not fit the target kind");
    }
    // A time outside the valid range routes to no partition (contract 1.2).
    if !fact.target_time.is_valid() {
        return invalid("target_time is outside the valid range");
    }
    let body = wire_body(&fact.body)?;
    let body_len = match &fact.body {
        FactBody::Score(s) => s.value_str.len() + s.comment.len() + s.metadata.len() + 48,
        FactBody::Tag(_) | FactBody::Flag(_) => 1,
        FactBody::Correction(c) => c.corrected.len() + c.note.len() + 8,
    };
    Ok((body, 96 + fact.session_id.len() + fact.key.len() + body_len))
}

impl Ingest {
    /// Writes facts of one author to the project of `scope` (contract 3.8): validates them, assigns each its
    /// `fact_id`, `version` and `created_at`, admits the write under the in-flight budget and the decision table
    /// of contract 4.8, appends it as `FactBatch` frames (several when it holds more than 1,000 facts or 16 MiB)
    /// and returns once every frame is durable. The result lists what the server assigned, in the order of
    /// `facts`.
    pub async fn append_facts(
        &self,
        scope: &ProjectScope,
        author: FactAuthor,
        facts: &[NewFact],
    ) -> Result<Vec<AcceptedFact>, FactWriteError> {
        let result = self.write_facts(scope, author, facts).await;
        let metrics = &self.inner.facts;
        match &result {
            Ok(accepted) => {
                metrics.accepted.add(1);
                metrics.facts.add(accepted.len() as u64);
            }
            Err(e) => {
                if let Some(counter) = metrics.refused.get(e.outcome()) {
                    counter.add(1);
                }
            }
        }
        result
    }

    async fn write_facts(
        &self,
        scope: &ProjectScope,
        author: FactAuthor,
        facts: &[NewFact],
    ) -> Result<Vec<AcceptedFact>, FactWriteError> {
        let inner = &self.inner;
        let project_id = scope.project_id();
        let mut bodies = Vec::with_capacity(facts.len());
        let mut total = 0usize;
        for (index, fact) in facts.iter().enumerate() {
            let (body, len) = check(fact).map_err(|(large, reason)| {
                if large {
                    FactWriteError::TooLarge { index, reason }
                } else {
                    FactWriteError::Invalid { index, reason }
                }
            })?;
            total = total.saturating_add(len);
            bodies.push((body, len));
        }
        // The payload and its frame are held until the frame is durable.
        let held = inner
            .admission
            .admit(project_id, 2 * total as u64 + FRAME_HEADER_LEN as u64)?;

        let now = inner.env.clock.wall_now();
        let version = u64::try_from(now.micros()).unwrap_or(0);
        let mut accepted = Vec::with_capacity(facts.len());
        let mut at = 0;
        while at < facts.len() {
            // The frame takes facts while it stays within both caps; the margin covers the size estimate.
            let mut end = at;
            let mut bytes = 64usize;
            while end < facts.len()
                && end - at < MAX_FACTS
                && (end == at || bytes + bodies[end].1 <= MAX_FACT_BATCH_BYTES - 4_096)
            {
                bytes += bodies[end].1;
                end += 1;
            }
            let ids: Vec<FactId> = (at..end)
                .map(|_| FactId::from_uuid(inner.env.new_id()))
                .collect();
            let batch = codec::FactBatch {
                project_id,
                author_kind: author.kind,
                author_id: author.id,
                facts: (at..end)
                    .zip(&ids)
                    .map(|(i, &fact_id)| codec::Fact {
                        fact_id,
                        target_kind: facts[i].target_kind,
                        trace_id: facts[i].trace_id,
                        span_id: facts[i].span_id,
                        session_id: &facts[i].session_id,
                        target_time: facts[i].target_time,
                        key: &facts[i].key,
                        version,
                        retracted: facts[i].retracted,
                        created_at: now,
                        body: bodies[i].0,
                    })
                    .collect(),
            };
            let payload =
                Record::FactBatch(batch)
                    .encode()
                    .map_err(|_| FactWriteError::TooLarge {
                        index: at,
                        reason: "the fact does not fit a fact frame of 16 MiB",
                    })?;
            let frame = PreparedFrame::new(RecordType::FactBatch, payload, inner.compression)
                .map_err(|_| FactWriteError::Internal("the facts do not fit a WAL frame"))?;
            let lsn = inner
                .admission
                .wal
                .append(frame)
                .await
                .map_err(|e| inner.append_failed(e))?;
            accepted.extend(ids.into_iter().map(|fact_id| AcceptedFact {
                fact_id,
                version,
                created_at: now,
                lsn,
            }));
            at = end;
        }
        drop(held);
        Ok(accepted)
    }
}
