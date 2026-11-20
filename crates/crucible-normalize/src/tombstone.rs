//! Deletion requests in stage B (contract 13.3, step 3).
//!
//! A deletion is point in time: it selects data of its project received before its `requested_at`, and data
//! received at or after that instant is new data and is kept. Stage B drops what a deletion selects from every frame
//! it admits once it knows the deletion: from the `Tombstone` frame in the log, or from the deletion requests that
//! the shipper reads from the control store before it ships, so that a replay after a restart never inserts what a
//! completed request deleted.
//!
//! What a deletion selects inside its project:
//!
//! | Selector | Spans | MCP operations | Facts |
//! |---|---|---|---|
//! | trace | by trace id | with their spans | by the target's trace id |
//! | user | by `user_id` | with their spans, and by their own `user_id` or `user_subject_hash` | none: a fact names no user |
//! | session | by `session_id` | with their spans | by the target's session id |
//! | project | all | all | all |
//!
//! The subject hash of an operation names the user when it is the keyed hash of the user (contract 9.3) under the
//! issuer the request names, or, when it names none, under the operation's own issuer. The deletion executor hashes
//! under every issuer the project's operations carry, and an operation's hash can only match under its own issuer.
//! A user or session deletion reaches facts and the other MCP rows of the user's traces through the executor, which
//! resolves the traces first; stage B cannot.
//!
//! [`Tombstones`] holds, per project and key, the latest `requested_at` of the deletions with that key, so matching
//! a span is a few lookups whatever the number of deletions: data received before the latest instant is selected by
//! at least one of them.

use crucible_records::{
    DeletionRequestId, DetMap, Fact, McpOpRecord, ProjectId, SpanCore, Timestamp, TraceId, det_map,
};

use crate::ProjectSalt;

/// What a deletion selects inside its project (contract 13.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TombstoneSelector {
    /// These traces.
    Traces(Vec<TraceId>),
    /// The spans of a user, and the MCP operations that name the user by `user_id` or by the hash of the user's
    /// subject.
    User {
        /// The `user_id`, which is also the subject that a hash covers.
        user_id: Box<str>,
        /// The token issuer of the subject hash; `None` for every issuer.
        issuer: Option<Box<str>>,
    },
    /// The spans of a session.
    Session(Box<str>),
    /// Everything of the project.
    Project,
}

/// A deletion request as stage B applies it: the `Tombstone` record of contract 4.4, with the issuer of a deletion by
/// user, which only the control store holds (contract 13.6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tombstone {
    /// The deletion request.
    pub request_id: DeletionRequestId,
    /// The project whose data is deleted.
    pub project_id: ProjectId,
    /// Data received before this instant is selected; data received at or after it is kept.
    pub requested_at: Timestamp,
    /// The deployment-wide sequence number of the request.
    pub deletion_seq: u64,
    /// What is deleted.
    pub selector: TombstoneSelector,
}

/// The latest `requested_at` of the deletions of one user.
#[derive(Debug, Default)]
struct UserCut {
    /// Over every deletion of the user: spans and operations match by `user_id` before it.
    latest: Option<Timestamp>,
    /// Over the deletions that name no issuer: subject hashes under the operation's own issuer match before it.
    every_issuer: Option<Timestamp>,
    /// Per named issuer, over the deletions that name it.
    issuers: Vec<(Box<str>, Timestamp)>,
}

/// The deletions of one project, by key.
#[derive(Debug, Default)]
struct Cuts {
    project: Option<Timestamp>,
    traces: DetMap<TraceId, Timestamp>,
    users: DetMap<Box<str>, UserCut>,
    sessions: DetMap<Box<str>, Timestamp>,
}

fn raise(cut: &mut Option<Timestamp>, at: Timestamp) {
    *cut = Some(cut.map_or(at, |before| before.max(at)));
}

fn raise_entry<K: core::hash::Hash + Eq>(map: &mut DetMap<K, Timestamp>, key: K, at: Timestamp) {
    let cut = map.entry(key).or_insert(at);
    *cut = (*cut).max(at);
}

/// Whether data received at `received_at` is before a cut.
fn before(cut: Option<Timestamp>, received_at: Timestamp) -> bool {
    cut.is_some_and(|cut| received_at < cut)
}

/// The deletions stage B knows, indexed for matching. Inserting a deletion twice changes nothing.
#[derive(Debug, Default)]
pub struct Tombstones {
    projects: DetMap<ProjectId, Cuts>,
    highest_seq: u64,
    count: usize,
}

impl Tombstones {
    /// No deletion.
    pub fn new() -> Self {
        Self {
            projects: det_map(),
            highest_seq: 0,
            count: 0,
        }
    }

    /// Learns a deletion.
    pub fn insert(&mut self, tombstone: &Tombstone) {
        let at = tombstone.requested_at;
        let cuts = self.projects.entry(tombstone.project_id).or_default();
        match &tombstone.selector {
            TombstoneSelector::Project => raise(&mut cuts.project, at),
            TombstoneSelector::Traces(ids) => {
                for id in ids {
                    raise_entry(&mut cuts.traces, *id, at);
                }
            }
            TombstoneSelector::Session(session) if !session.is_empty() => {
                raise_entry(&mut cuts.sessions, session.clone(), at);
            }
            TombstoneSelector::User { user_id, issuer } if !user_id.is_empty() => {
                let cut = cuts.users.entry(user_id.clone()).or_default();
                raise(&mut cut.latest, at);
                match issuer {
                    None => raise(&mut cut.every_issuer, at),
                    Some(issuer) => match cut.issuers.iter_mut().find(|(i, _)| i == issuer) {
                        Some((_, named)) => *named = (*named).max(at),
                        None => cut.issuers.push((issuer.clone(), at)),
                    },
                }
            }
            // An empty user or session names nothing, as the API refuses it.
            TombstoneSelector::Session(_) | TombstoneSelector::User { .. } => {}
        }
        self.highest_seq = self.highest_seq.max(tombstone.deletion_seq);
        self.count += 1;
    }

    /// Whether no deletion is known.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The highest deletion sequence number known: what a `BatchSeal` records (contract 4.4).
    pub fn highest_seq(&self) -> u64 {
        self.highest_seq
    }

    /// Whether a deletion selects the span: its project's, received before the deletion, and of the deletion's
    /// trace, user or session, or of any when the whole project is deleted.
    pub fn selects_span(&self, core: &SpanCore) -> bool {
        let Some(cuts) = self.projects.get(&core.project_id) else {
            return false;
        };
        let at = core.ingestion.received_at;
        before(cuts.project, at)
            || before(cuts.traces.get(&core.trace_id).copied(), at)
            || (!core.user_id.is_empty()
                && before(
                    cuts.users.get(&*core.user_id).and_then(|cut| cut.latest),
                    at,
                ))
            || (!core.session_id.is_empty()
                && before(cuts.sessions.get(&*core.session_id).copied(), at))
    }

    /// Whether a deletion by user selects the MCP operation by its own identity: its `user_id`, or its
    /// `user_subject_hash` under the issuer the deletion names, or under the operation's own issuer when the
    /// deletion names none. `salt` is the project's salt that hashed the subject. The span of the operation is
    /// matched by [`Tombstones::selects_span`].
    pub fn selects_op(&self, op: &McpOpRecord, salt: Option<&ProjectSalt>) -> bool {
        self.op_selected(op, salt, false)
    }

    /// As [`Tombstones::selects_op`]; with `every_issuer`, a deletion that names an issuer is taken as one that
    /// names none (the seeded defect of the simulation suite).
    pub(crate) fn op_selected(
        &self,
        op: &McpOpRecord,
        salt: Option<&ProjectSalt>,
        every_issuer: bool,
    ) -> bool {
        let Some(cuts) = self.projects.get(&op.project_id) else {
            return false;
        };
        let at = op.ingestion.received_at;
        if before(cuts.project, at) || before(cuts.traces.get(&op.trace_id).copied(), at) {
            return true;
        }
        if cuts.users.is_empty() {
            return false;
        }
        if !op.user_id.is_empty()
            && before(cuts.users.get(&*op.user_id).and_then(|cut| cut.latest), at)
        {
            return true;
        }
        let (Some(hash), Some(salt)) = (op.user_subject_hash, salt) else {
            return false;
        };
        cuts.users.iter().any(|(user, cut)| {
            let own = || salt.subject_hash(&op.user_issuer, user) == hash;
            (before(cut.every_issuer, at) && own())
                || cut.issuers.iter().any(|(issuer, named)| {
                    at < *named
                        && if every_issuer {
                            own()
                        } else {
                            salt.subject_hash(issuer, user) == hash
                        }
                })
        })
    }

    /// Whether a deletion selects the fact: its project's, received before the deletion, and on a target of the
    /// deletion's traces or session, or of any when the whole project is deleted. A fact names no user.
    pub fn selects_fact(&self, fact: &Fact) -> bool {
        let envelope = &fact.envelope;
        let Some(cuts) = self.projects.get(&envelope.project_id) else {
            return false;
        };
        let at = envelope.ingestion.received_at;
        before(cuts.project, at)
            || (!envelope.trace_id.is_zero()
                && before(cuts.traces.get(&envelope.trace_id).copied(), at))
            || (!envelope.session_id.is_empty()
                && before(cuts.sessions.get(&*envelope.session_id).copied(), at))
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::{
        AuthorKind, FactBody, FactEnvelope, FactId, FactTargetKind, FlagBody, Ingestion, Lsn,
        SpanId, Uuid, WalEpoch,
    };
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;
    use crate::test_support::record;

    const ISSUERS: [&str; 2] = ["https://a.example", "https://b.example"];

    fn project(p: u8) -> ProjectId {
        ProjectId::from_bytes([p + 1; 16])
    }

    fn trace(t: u8) -> TraceId {
        TraceId::from_bytes([t + 1; 16])
    }

    fn name(prefix: &str, n: u8) -> Box<str> {
        format!("{prefix}-{n}").into()
    }

    fn salt(p: u8) -> ProjectSalt {
        ProjectSalt::new([p + 9; 32])
    }

    fn ingestion(received_at: i64) -> Ingestion {
        Ingestion {
            wal_epoch: WalEpoch::from_bytes([4; 16]),
            batch_seq: Lsn::ZERO,
            lsn: Lsn::new(1),
            received_at: Timestamp::from_micros(received_at),
        }
    }

    /// A deletion as the tests describe it: (project, kind, key, issuer, requested_at, seq), from small domains.
    type TombDesc = (u8, u8, u8, Option<u8>, i64, u64);

    fn tombstone(&(p, kind, key, issuer, at, seq): &TombDesc) -> Tombstone {
        Tombstone {
            request_id: DeletionRequestId::from_bytes([seq as u8; 16]),
            project_id: project(p),
            requested_at: Timestamp::from_micros(at),
            deletion_seq: seq,
            selector: match kind {
                0 => TombstoneSelector::Traces(vec![trace(key), trace(key + 1)]),
                1 => TombstoneSelector::User {
                    user_id: name("user", key),
                    issuer: issuer.map(|i| ISSUERS[usize::from(i)].into()),
                },
                2 => TombstoneSelector::Session(name("session", key)),
                _ => TombstoneSelector::Project,
            },
        }
    }

    fn tomb_desc() -> impl Strategy<Value = TombDesc> {
        (
            0u8..2,
            0u8..4,
            0u8..3,
            prop::option::of(0u8..2),
            0i64..10,
            0u64..50,
        )
    }

    /// A span: (project, trace, user, session, received_at); `None` for an empty user or session.
    type SpanDesc = (u8, u8, Option<u8>, Option<u8>, i64);

    fn span(&(p, t, user, session, at): &SpanDesc) -> SpanCore {
        let mut core = record().core;
        core.project_id = project(p);
        core.trace_id = trace(t);
        core.user_id = user.map_or_else(|| "".into(), |u| name("user", u));
        core.session_id = session.map_or_else(|| "".into(), |s| name("session", s));
        core.ingestion = ingestion(at);
        core
    }

    /// An operation: (project, trace, user id, subject with its issuer, received_at).
    type OpDesc = (u8, u8, Option<u8>, Option<(u8, u8)>, i64);

    fn op(&(p, t, user, subject, at): &OpDesc) -> McpOpRecord {
        let mut core = record().core;
        core.project_id = project(p);
        core.trace_id = trace(t);
        core.ingestion = ingestion(at);
        let mut op = crate::test_support::op_record(&core);
        op.user_id = user.map_or_else(|| "".into(), |u| name("user", u));
        if let Some((issuer, subject)) = subject {
            let issuer = ISSUERS[usize::from(issuer)];
            op.user_issuer = issuer.into();
            op.user_subject_hash = Some(salt(p).subject_hash(issuer, &name("user", subject)));
        }
        op
    }

    /// A fact: (project, trace or none, session or none, received_at).
    type FactDesc = (u8, Option<u8>, Option<u8>, i64);

    fn fact(&(p, t, session, at): &FactDesc) -> Fact {
        Fact {
            envelope: FactEnvelope {
                project_id: project(p),
                fact_id: FactId::from_bytes([3; 16]),
                target_kind: if session.is_some() {
                    FactTargetKind::Session
                } else {
                    FactTargetKind::Trace
                },
                trace_id: t.map_or(TraceId::ZERO, trace),
                span_id: SpanId::ZERO,
                session_id: session.map_or_else(|| "".into(), |s| name("session", s)),
                target_time: Timestamp::from_micros(0),
                key: "k".into(),
                version: 1,
                retracted: false,
                author_kind: AuthorKind::User,
                author_id: Uuid::from_bytes([5; 16]),
                created_at: Timestamp::from_micros(0),
                ingestion: ingestion(at),
            },
            body: FactBody::Flag(FlagBody { value: true }),
        }
    }

    /// The linear model: does any one deletion select the span, by the table of the module documentation?
    fn model_span(tombs: &[TombDesc], s: &SpanDesc) -> bool {
        let &(p, t, user, session, at) = s;
        tombs.iter().any(|&(tp, kind, key, _, requested, _)| {
            tp == p
                && at < requested
                && match kind {
                    0 => t == key || t == key + 1,
                    1 => user == Some(key),
                    2 => session == Some(key),
                    _ => true,
                }
        })
    }

    fn model_op(tombs: &[TombDesc], o: &OpDesc) -> bool {
        let &(p, t, user, subject, at) = o;
        tombs.iter().any(|&(tp, kind, key, issuer, requested, _)| {
            tp == p
                && at < requested
                && match kind {
                    0 => t == key || t == key + 1,
                    1 => {
                        user == Some(key)
                            || subject.is_some_and(|(own, subject)| {
                                subject == key && issuer.is_none_or(|named| named == own)
                            })
                    }
                    2 => false,
                    _ => true,
                }
        })
    }

    fn model_fact(tombs: &[TombDesc], f: &FactDesc) -> bool {
        let &(p, t, session, at) = f;
        tombs.iter().any(|&(tp, kind, key, _, requested, _)| {
            tp == p
                && at < requested
                && match kind {
                    0 => t.is_some_and(|t| t == key || t == key + 1),
                    1 => false,
                    2 => session == Some(key),
                    _ => true,
                }
        })
    }

    fn set(tombs: &[TombDesc]) -> Tombstones {
        let mut set = Tombstones::new();
        for desc in tombs {
            set.insert(&tombstone(desc));
        }
        set
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// The indexed set selects exactly what one of its deletions selects, for spans, operations and facts.
        #[test]
        fn the_set_selects_what_one_of_its_deletions_selects(
            tombs in vec(tomb_desc(), 0..8),
            spans in vec((0u8..2, 0u8..5, prop::option::of(0u8..3), prop::option::of(0u8..3), 0i64..12), 0..12),
            ops in vec((0u8..2, 0u8..5, prop::option::of(0u8..3), prop::option::of((0u8..2, 0u8..3)), 0i64..12), 0..12),
            facts in vec((0u8..2, prop::option::of(0u8..5), prop::option::of(0u8..3), 0i64..12), 0..12),
        ) {
            let set = set(&tombs);
            prop_assert_eq!(set.is_empty(), tombs.is_empty());
            prop_assert_eq!(set.highest_seq(), tombs.iter().map(|t| t.5).max().unwrap_or(0));
            for s in &spans {
                prop_assert_eq!(set.selects_span(&span(s)), model_span(&tombs, s), "{:?}", s);
            }
            for o in &ops {
                let record = op(o);
                prop_assert_eq!(set.selects_op(&record, Some(&salt(o.0))), model_op(&tombs, o), "{:?}", o);
                // Without the salt no hash can be matched; the user id and the trace still can.
                let unsalted = model_op(&tombs, &(o.0, o.1, o.2, None, o.4));
                prop_assert_eq!(set.selects_op(&record, None), unsalted, "{:?}", o);
            }
            for f in &facts {
                prop_assert_eq!(set.selects_fact(&fact(f)), model_fact(&tombs, f), "{:?}", f);
            }
        }

        /// More deletions never select less: a replay that knows more deletions drops more, never fewer
        /// (contract 5.3).
        #[test]
        fn more_deletions_select_more(
            tombs in vec(tomb_desc(), 0..8),
            extra in vec(tomb_desc(), 0..4),
            spans in vec((0u8..2, 0u8..5, prop::option::of(0u8..3), prop::option::of(0u8..3), 0i64..12), 0..12),
        ) {
            let fewer = set(&tombs);
            let all: Vec<TombDesc> = tombs.iter().chain(&extra).copied().collect();
            let more = set(&all);
            for s in &spans {
                prop_assert!(!fewer.selects_span(&span(s)) || more.selects_span(&span(s)));
            }
        }
    }

    #[test]
    fn a_deletion_keeps_what_arrived_at_or_after_it() {
        let set = set(&[(0, 0, 0, None, 5, 1)]);
        assert!(set.selects_span(&span(&(0, 0, None, None, 4))));
        assert!(!set.selects_span(&span(&(0, 0, None, None, 5))));
        assert!(!set.selects_span(&span(&(0, 0, None, None, 6))));
        // Another project with the same trace id is another trace.
        assert!(!set.selects_span(&span(&(1, 0, None, None, 4))));
    }

    #[test]
    fn a_named_issuer_selects_only_the_hashes_made_under_it() {
        // A deletion of user-1 under issuer a: the operation whose subject user-1 came from issuer b is another
        // identity, which the deletion keeps.
        let set = set(&[(0, 1, 1, Some(0), 9, 1)]);
        let under = |issuer: u8| op(&(0, 4, None, Some((issuer, 1)), 3));
        assert!(set.selects_op(&under(0), Some(&salt(0))));
        assert!(!set.selects_op(&under(1), Some(&salt(0))));
        // The seeded defect takes the issuer as any.
        assert!(set.op_selected(&under(1), Some(&salt(0)), true));
        // Without an issuer, the subject is the user's under any issuer.
        let any = set_of(1, None);
        assert!(any.selects_op(&under(0), Some(&salt(0))));
        assert!(any.selects_op(&under(1), Some(&salt(0))));
    }

    fn set_of(user: u8, issuer: Option<u8>) -> Tombstones {
        set(&[(0, 1, user, issuer, 9, 1)])
    }

    #[test]
    fn an_empty_user_or_session_names_nothing() {
        let mut set = Tombstones::new();
        for selector in [
            TombstoneSelector::User {
                user_id: "".into(),
                issuer: None,
            },
            TombstoneSelector::Session("".into()),
        ] {
            set.insert(&Tombstone {
                request_id: DeletionRequestId::from_bytes([1; 16]),
                project_id: project(0),
                requested_at: Timestamp::from_micros(9),
                deletion_seq: 1,
                selector,
            });
        }
        assert!(!set.selects_span(&span(&(0, 0, None, None, 1))));
        assert!(!set.selects_op(&op(&(0, 0, None, None, 1)), None));
        assert!(!set.selects_fact(&fact(&(0, None, None, 1))));
    }
}
