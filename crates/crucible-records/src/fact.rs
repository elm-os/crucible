//! Facts (contract 3.8): later-written, append-only, versioned rows. They are never updated or deleted in place; a
//! retraction is a new row. Latest-wins resolution per `(target, key)` picks the greatest `(version, fact_id)`.
//!
//! The comment fact lives in Postgres (contract 11.1) and has no record here.

use std::cmp::Ordering;
use std::collections::BTreeMap;

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

/// What a fact is attached to. Latest-wins resolves per `(target, key)`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FactTarget {
    /// The project.
    pub project_id: ProjectId,
    /// The kind of target.
    pub target_kind: FactTargetKind,
    /// The target trace, or zero.
    pub trace_id: TraceId,
    /// The target span, or zero.
    pub span_id: SpanId,
    /// The target session, or empty.
    pub session_id: Box<str>,
}

impl FactEnvelope {
    /// The target of the fact.
    pub fn target(&self) -> FactTarget {
        FactTarget {
            project_id: self.project_id,
            target_kind: self.target_kind,
            trace_id: self.trace_id,
            span_id: self.span_id,
            session_id: self.session_id.clone(),
        }
    }

    /// The fact's place in [`fact_order`] and whether it retracts.
    pub const fn version(&self) -> FactVersion {
        FactVersion {
            version: self.version,
            fact_id: self.fact_id,
            retracted: self.retracted,
        }
    }
}

/// What resolution reads of one fact row: its place in [`fact_order`] and whether it is a retraction.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FactVersion {
    /// The row's `version`.
    pub version: u64,
    /// The row's `fact_id`.
    pub fact_id: FactId,
    /// Whether the row retracts its key.
    pub retracted: bool,
}

/// The one comparator facts resolve by (contract 3.8): `version` first, then `fact_id` in RFC 9562 byte order,
/// which is time order for UUIDv7. The Rust fold, the store model and the differential check all use it; no SQL
/// ordering of UUIDs ever picks a winner.
pub fn fact_order(a: (u64, FactId), b: (u64, FactId)) -> Ordering {
    a.0.cmp(&b.0)
        .then_with(|| a.1.to_bytes().cmp(&b.1.to_bytes()))
}

/// The reference fold of invariant I4: per key, the row that is greatest under [`fact_order`]; a key whose
/// greatest row is a retraction has no value and is absent from the result. `K` is whatever `(target, key)`
/// the caller resolves over. The result is a function of the set of rows: it does not depend on their order,
/// and a row that appears twice counts once.
pub fn fold_facts<K: Ord, T>(
    rows: impl IntoIterator<Item = (K, FactVersion, T)>,
) -> BTreeMap<K, T> {
    let mut latest: BTreeMap<K, (FactVersion, T)> = BTreeMap::new();
    for (key, version, row) in rows {
        let at = (version.version, version.fact_id);
        match latest.get_mut(&key) {
            Some(held) if fact_order(at, (held.0.version, held.0.fact_id)).is_le() => {}
            Some(held) => *held = (version, row),
            None => {
                latest.insert(key, (version, row));
            }
        }
    }
    latest
        .into_iter()
        .filter(|(_, (version, _))| !version.retracted)
        .map(|(key, (_, row))| (key, row))
        .collect()
}

/// Effective tags (contract 3.8): the ingest tags, plus the tags whose latest edit is `add`, minus the tags whose
/// latest edit is `remove`, distinct and in byte order. `latest` is the resolved tag edits of the target, as
/// [`fold_facts`] returns them.
pub fn effective_tags<'a>(
    ingest: impl IntoIterator<Item = &'a str>,
    latest: impl IntoIterator<Item = (&'a str, TagOp)>,
) -> Vec<&'a str> {
    let mut tags: std::collections::BTreeSet<&str> = ingest.into_iter().collect();
    for (tag, op) in latest {
        match op {
            TagOp::Add => tags.insert(tag),
            TagOp::Remove => tags.remove(tag),
        };
    }
    tags.into_iter().collect()
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
    mod resolution {
        use proptest::prelude::*;

        use super::super::*;

        /// (key, version, id low byte, id millisecond, retracted) from small domains, so that keys, versions and
        /// ids collide.
        type Row = (u8, u64, u8, u64, bool);

        fn id(ms: u64, low: u8) -> FactId {
            FactId::from_uuid(Uuid::from_v7_parts(ms, u128::from(low)))
        }

        fn rows(descs: &[Row]) -> Vec<(u8, FactVersion, Row)> {
            descs
                .iter()
                .map(|&row| {
                    let (key, version, low, ms, retracted) = row;
                    let version = FactVersion {
                        version,
                        fact_id: id(ms, low),
                        retracted,
                    };
                    (key, version, row)
                })
                .collect()
        }

        /// The fold, written the slow way: per key the maximum under a sort.
        fn naive(descs: &[Row]) -> BTreeMap<u8, Row> {
            let mut all = rows(descs);
            all.sort_by(|a, b| {
                a.0.cmp(&b.0).then_with(|| {
                    fact_order((a.1.version, a.1.fact_id), (b.1.version, b.1.fact_id))
                })
            });
            let mut out = BTreeMap::new();
            for (key, version, row) in all {
                if version.retracted {
                    out.remove(&key);
                } else {
                    out.insert(key, row);
                }
            }
            out
        }

        #[test]
        fn version_wins_then_the_id_in_byte_order() {
            let (early, late) = (id(1_000, 9), id(2_000, 1));
            assert!(fact_order((1, late), (2, early)).is_lt());
            assert!(fact_order((2, early), (2, late)).is_lt());
            assert!(fact_order((2, late), (2, late)).is_eq());
            // Byte order, not the order of the two 64-bit halves swapped: the ids differ only in their first
            // half, where the UUIDv7 time lives.
            assert!(early.to_bytes()[..8] < late.to_bytes()[..8]);
            assert!(early.to_bytes()[8..] > late.to_bytes()[8..]);
        }

        #[test]
        fn a_retraction_leaves_its_key_without_a_value() {
            let folded = fold_facts(rows(&[
                (1, 5, 0, 10, false),
                (1, 6, 0, 11, true),
                (2, 1, 0, 12, false),
            ]));
            assert_eq!(folded.keys().copied().collect::<Vec<_>>(), [2]);
            // An older write after the retraction in arrival order does not bring the value back.
            let folded = fold_facts(rows(&[(1, 6, 0, 11, true), (1, 5, 0, 10, false)]));
            assert!(folded.is_empty());
        }

        #[test]
        fn effective_tags_are_ingest_plus_added_minus_removed() {
            let tags = effective_tags(
                ["b", "a", "c"],
                [
                    ("c", TagOp::Remove),
                    ("d", TagOp::Add),
                    ("a", TagOp::Add),
                    ("z", TagOp::Remove),
                ],
            );
            assert_eq!(tags, ["a", "b", "d"]);
        }

        proptest! {
            #[test]
            fn the_order_is_total_and_agrees_with_uuid_v7_time(
                a in (0u64..4, 0u64..4, any::<u8>()),
                b in (0u64..4, 0u64..4, any::<u8>()),
                c in (0u64..4, 0u64..4, any::<u8>()),
            ) {
                let at = |(version, ms, low): (u64, u64, u8)| (version, id(ms, low));
                let (a, b, c) = (at(a), at(b), at(c));
                prop_assert_eq!(fact_order(a, b), fact_order(b, a).reverse());
                if fact_order(a, b).is_le() && fact_order(b, c).is_le() {
                    prop_assert!(fact_order(a, c).is_le());
                }
                prop_assert_eq!(fact_order(a, b).is_eq(), a == b);
                if a.0 == b.0 && a.1.uuid().v7_parts().map(|p| p.0) < b.1.uuid().v7_parts().map(|p| p.0) {
                    prop_assert!(fact_order(a, b).is_lt());
                }
            }

            #[test]
            fn the_fold_is_the_naive_fold_under_every_interleaving(
                descs in proptest::collection::vec((0u8..4, 0u64..4, 0u8..3, 0u64..3, any::<bool>()), 0..24),
                seed in any::<u64>(),
            ) {
                // Rows that tie in (version, fact_id) are one fact: keep one of each.
                let mut distinct: Vec<Row> = Vec::new();
                for row in descs {
                    if !distinct.iter().any(|d| (d.0, d.1, d.2, d.3) == (row.0, row.1, row.2, row.3)) {
                        distinct.push(row);
                    }
                }
                let expected = naive(&distinct);
                prop_assert_eq!(&fold_facts(rows(&distinct)), &expected);
                // Any arrival order, and any row arriving twice (a part merged late, a block read again).
                let mut shuffled = distinct.clone();
                let mut state = seed;
                for i in (1..shuffled.len()).rev() {
                    state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                    shuffled.swap(i, (state >> 33) as usize % (i + 1));
                }
                let twice: Vec<Row> = shuffled.iter().chain(distinct.iter()).copied().collect();
                prop_assert_eq!(&fold_facts(rows(&shuffled)), &expected);
                prop_assert_eq!(&fold_facts(rows(&twice)), &expected);
            }
        }
    }
}
