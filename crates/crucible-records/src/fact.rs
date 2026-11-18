//! Facts (contract 3.8): later-written, append-only, versioned rows. They are never updated or deleted in place; a
//! retraction is a new row. Latest-wins resolution per `(target, key)` picks the greatest `(version, fact_id)`.
//!
//! The comment fact lives in Postgres (contract 11.1) and has no record here.

use bytes::Bytes;

use crate::enums::{AuthorKind, FactTargetKind, ScoreDataType, ScoreSource, TagOp};
use crate::id::{FactId, ProjectId, SpanId, TraceId, Uuid};
use crate::ingestion::Ingestion;
use crate::table::TableId;
use crate::time::Timestamp;

/// One fact: the envelope every fact carries and the body of its table.
#[derive(Clone, PartialEq, Debug)]
pub struct Fact {
    /// Target, key, version and authorship.
    pub envelope: FactEnvelope,
    /// The table-specific fields.
    pub body: FactBody,
}

/// The fields every fact carries.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FactEnvelope {
    /// From the credential only.
    pub project_id: ProjectId,
    /// Server-generated UUIDv7.
    pub fact_id: FactId,
    /// What the fact is attached to.
    pub target_kind: FactTargetKind,
    /// The target trace; [`TraceId::ZERO`] when the target kind does not use it.
    pub trace_id: TraceId,
    /// The target span; [`SpanId::ZERO`] when the target kind does not use it.
    pub span_id: SpanId,
    /// The target session; empty when the target kind does not use it.
    pub session_id: Box<str>,
    /// `start_time` of the target (a trace's earliest known start, a session's first seen); aligns pruning and
    /// retention with spans.
    pub target_time: Timestamp,
    /// What latest-wins resolves over; [`crate::caps::FACT_KEY_BYTES`].
    pub key: Box<str>,
    /// Server microsecond timestamp at acceptance; ties are broken by the greater `fact_id`.
    pub version: u64,
    /// A retracted latest fact leaves its key without a value.
    pub retracted: bool,
    /// Who wrote the fact.
    pub author_kind: AuthorKind,
    /// The user or API key that wrote the fact.
    pub author_id: Uuid,
    /// When the fact was created.
    pub created_at: Timestamp,
    /// Where and when the WAL accepted the fact.
    pub ingestion: Ingestion,
}

/// The body of a fact; its variant names the table.
#[derive(Clone, PartialEq, Debug)]
pub enum FactBody {
    /// `fact_scores` (v0 exposes scores as feedback).
    Score(ScoreBody),
    /// `fact_tags`.
    Tag(TagBody),
    /// `fact_flags`.
    Flag(FlagBody),
    /// `fact_corrections`.
    Correction(CorrectionBody),
}

impl FactBody {
    /// The table the fact is written to.
    pub const fn table(&self) -> TableId {
        match self {
            Self::Score(_) => TableId::FactScores,
            Self::Tag(_) => TableId::FactTags,
            Self::Flag(_) => TableId::FactFlags,
            Self::Correction(_) => TableId::FactCorrections,
        }
    }
}

/// A score. Its key is the score name, plus `#` and the client `score_id` when one is given.
#[derive(Clone, PartialEq, Debug)]
pub struct ScoreBody {
    /// How to read the value.
    pub data_type: ScoreDataType,
    /// Numeric value.
    pub value_num: f64,
    /// String value; [`crate::caps::SCORE_VALUE_STR_BYTES`].
    pub value_str: Box<str>,
    /// [`crate::caps::SCORE_COMMENT_BYTES`].
    pub comment: Box<str>,
    /// Where the score came from.
    pub source: ScoreSource,
    /// JSON; [`crate::caps::SCORE_METADATA_BYTES`].
    pub metadata: Bytes,
    /// The score configuration used.
    pub config_id: Option<Uuid>,
}

/// A tag edit. Its key is the tag.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TagBody {
    /// Add or remove.
    pub op: TagOp,
}

/// A flag edit. Its key is `bookmarked` or `public`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FlagBody {
    /// The flag's new value.
    pub value: bool,
}

/// A correction. Its key is `output`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct CorrectionBody {
    /// The corrected value, JSON; [`crate::caps::CORRECTION_BYTES`].
    pub corrected: Bytes,
    /// [`crate::caps::CORRECTION_NOTE_BYTES`].
    pub note: Box<str>,
}

impl FlagBody {
    /// The key of the bookmark flag.
    pub const BOOKMARKED: &'static str = "bookmarked";
    /// The key of the public flag.
    pub const PUBLIC: &'static str = "public";
}

impl CorrectionBody {
    /// The key of every correction.
    pub const KEY: &'static str = "output";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_name_their_tables() {
        let score = FactBody::Score(ScoreBody {
            data_type: ScoreDataType::Numeric,
            value_num: 0.5,
            value_str: "".into(),
            comment: "".into(),
            source: ScoreSource::Api,
            metadata: Bytes::new(),
            config_id: None,
        });
        assert_eq!(score.table(), TableId::FactScores);
        assert_eq!(
            FactBody::Tag(TagBody { op: TagOp::Add }).table(),
            TableId::FactTags
        );
        assert_eq!(
            FactBody::Flag(FlagBody { value: true }).table(),
            TableId::FactFlags
        );
        let correction = FactBody::Correction(CorrectionBody {
            corrected: Bytes::from_static(b"{}"),
            note: "".into(),
        });
        assert_eq!(correction.table(), TableId::FactCorrections);
    }
}
