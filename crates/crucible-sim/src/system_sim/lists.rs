//! I7 on the read paths of the session and user lists: at the end of quiescence, after the last restart, every
//! project's session list, user list, the detail of each session and user, and the sessions of each user, read
//! through the real `QueryService`, equal a reference folded from the rows the store holds.
//!
//! The reference is the one the rollups are checked against (`system_oracle`), taken per session and per user:
//! the `span_core` rows on the UTC days of the window (the rollups select whole days), with the tokens and cost
//! of usage aggregates left out of the sums as the rollups leave them out, and, for session counts
//! and the sessions of a user, the `trace_heads` rows inside the window. Runs with a quarantine are skipped, as
//! the rollup check skips them, because a quarantined pair can hold back one table's rows and not another's.
//! The check reads only: it records no trace event and draws no randomness, and it runs last, so it changes
//! nothing else in the run.

use std::collections::{BTreeMap, BTreeSet};

use crucible_query::{
    IdCursor, QueryService, ReadContext, SessionListQuery, SessionSummary, UserListQuery,
    UserSessionsQuery, UserSummary,
};
use crucible_records::{TableId, TimeWindow, Timestamp};

use super::{
    Node, Permitted, START_BASE_MICROS, Shared, Violation, check_error, check_meta, project_id,
};
use crate::ch_model::{ChModel, bytes_at, i8_at, i64_at, u8_at, u64_at, uuid_at};
use crate::runtime::SIM_EPOCH_MICROS;

const STATUS_ERROR: i8 = 2;
const LEVEL_ERROR: i8 = 3;
/// The most users a session's rollup keeps (`groupUniqArrayArray(8)`).
const SESSION_USERS_KEPT: usize = 8;

/// What one session or one user must read as.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Expected {
    traces: u64,
    spans: u64,
    errors: u64,
    input: u64,
    output: u64,
    total: u64,
    cost: i64,
    first: i64,
    last: i64,
    users: BTreeSet<Vec<u8>>,
    sessions: BTreeSet<Vec<u8>>,
}

/// What one session of a user must read as: its distinct traces and the first and last head start.
#[derive(Clone, PartialEq, Eq, Debug)]
struct ExpectedSession {
    traces: BTreeSet<Vec<u8>>,
    first: i64,
    last: i64,
}

#[derive(Default)]
struct Reference {
    sessions: BTreeMap<Vec<u8>, Expected>,
    users: BTreeMap<Vec<u8>, Expected>,
    /// Per user, per session.
    user_sessions: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, ExpectedSession>>,
}

/// The window of every read: the one the trace list of the simulation reads.
fn window() -> Option<TimeWindow> {
    TimeWindow::new(
        Timestamp::from_micros(START_BASE_MICROS),
        Timestamp::from_micros(SIM_EPOCH_MICROS),
    )
    .ok()
}

fn on_days(window: TimeWindow, micros: i64) -> bool {
    let last = window.to().saturating_add_micros(-1).date();
    (window.from().date()..=last).contains(&Timestamp::from_micros(micros).date())
}

fn add(entry: &mut Expected, spans: &Expected) {
    if entry.spans == 0 {
        entry.first = spans.first;
        entry.last = spans.last;
    }
    entry.traces += spans.traces;
    entry.spans += spans.spans;
    entry.errors += spans.errors;
    entry.input += spans.input;
    entry.output += spans.output;
    entry.total += spans.total;
    entry.cost += spans.cost;
    entry.first = entry.first.min(spans.first);
    entry.last = entry.last.max(spans.last);
    entry.users.extend(spans.users.iter().cloned());
}

/// The reference of one project, folded from the store's parts. `None` when a row does not read.
fn reference(model: &ChModel, project: [u8; 16], window: TimeWindow) -> Option<Reference> {
    let mut out = Reference::default();
    for part in model.parts(TableId::SpanCore) {
        let block = &part.block;
        for row in 0..block.rows {
            let start = i64_at(block, "start_time", row)?;
            if uuid_at(block, "project_id", row)? != project || !on_days(window, start) {
                continue;
            }
            let user = bytes_at(block, "user_id", row)?.to_vec();
            // As in the rollups, token and cost sums leave out the spans that are usage aggregates.
            let counted = u8_at(block, "usage_aggregate", row)? == 0;
            let tokens = |name: &str| u64_at(block, name, row).map(|v| if counted { v } else { 0 });
            let one = Expected {
                traces: u64::from(u8_at(block, "is_root", row)? == 1),
                spans: 1,
                errors: u64::from(
                    i8_at(block, "status_code", row)? == STATUS_ERROR
                        || i8_at(block, "level", row)? == LEVEL_ERROR,
                ),
                input: tokens("input_tokens")?,
                output: tokens("output_tokens")?,
                total: tokens("total_tokens")?,
                cost: if counted {
                    i64_at(block, "total_cost_nusd", row)?
                } else {
                    0
                },
                first: start,
                last: i64_at(block, "end_time", row)?,
                users: if user.is_empty() {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([user.clone()])
                },
                sessions: BTreeSet::new(),
            };
            let session = bytes_at(block, "session_id", row)?.to_vec();
            if !session.is_empty() {
                add(out.sessions.entry(session).or_default(), &one);
            }
            if !user.is_empty() {
                let without_users = Expected {
                    users: BTreeSet::new(),
                    ..one
                };
                add(out.users.entry(user).or_default(), &without_users);
            }
        }
    }
    for part in model.parts(TableId::TraceHeads) {
        let block = &part.block;
        for row in 0..block.rows {
            let start = i64_at(block, "start_time", row)?;
            if uuid_at(block, "project_id", row)? != project
                || !window.contains(Timestamp::from_micros(start))
            {
                continue;
            }
            let (user, session) = (
                bytes_at(block, "user_id", row)?.to_vec(),
                bytes_at(block, "session_id", row)?.to_vec(),
            );
            if user.is_empty() || session.is_empty() {
                continue;
            }
            let trace = bytes_at(block, "trace_id", row)?.to_vec();
            if let Some(expected) = out.users.get_mut(&user) {
                expected.sessions.insert(session.clone());
            }
            let entry = out
                .user_sessions
                .entry(user)
                .or_default()
                .entry(session)
                .or_insert_with(|| ExpectedSession {
                    traces: BTreeSet::new(),
                    first: start,
                    last: start,
                });
            entry.traces.insert(trace);
            entry.first = entry.first.min(start);
            entry.last = entry.last.max(start);
        }
    }
    Some(out)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The measures a read gives, in the reference's terms. Users are compared on their own.
fn read_as(
    traces: u64,
    spans: u64,
    errors: u64,
    tokens: (u64, u64, u64),
    cost: i64,
    seen: (Timestamp, Timestamp),
) -> Expected {
    Expected {
        traces,
        spans,
        errors,
        input: tokens.0,
        output: tokens.1,
        total: tokens.2,
        cost,
        first: seen.0.micros(),
        last: seen.1.micros(),
        users: BTreeSet::new(),
        sessions: BTreeSet::new(),
    }
}

fn session_measures(s: &SessionSummary) -> Expected {
    read_as(
        s.trace_count,
        s.span_count,
        s.error_count,
        (s.input_tokens, s.output_tokens, s.total_tokens),
        s.total_cost_nusd.nanos(),
        (s.first_seen, s.last_seen),
    )
}

fn user_measures(u: &UserSummary) -> Expected {
    read_as(
        u.trace_count,
        u.span_count,
        u.error_count,
        (u.input_tokens, u.output_tokens, u.total_tokens),
        u.total_cost_nusd.nanos(),
        (u.first_seen, u.last_seen),
    )
}

fn measures_only(expected: &Expected) -> Expected {
    Expected {
        users: BTreeSet::new(),
        sessions: BTreeSet::new(),
        ..expected.clone()
    }
}

struct Reader<'a> {
    shared: &'a Shared,
    service: &'a QueryService,
    ctx: ReadContext,
    window: TimeWindow,
    permitted: Permitted,
}

impl Reader<'_> {
    fn wrong(&self, what: String) {
        self.shared.ledger().violate(Violation::Aggregate(what));
    }

    /// Every session of the list, by id; `None` when a read failed or was not whole (already a violation).
    async fn sessions(&self) -> Option<BTreeMap<Vec<u8>, SessionSummary>> {
        let mut out = BTreeMap::new();
        let mut cursor: Option<IdCursor> = None;
        loop {
            let query = SessionListQuery {
                window: self.window,
                limit: Some(200),
                cursor: cursor.take(),
            };
            let page = match self.service.session_list(&self.ctx, query).await {
                Ok(page) => page,
                Err(error) => {
                    check_error(self.shared, &error, self.permitted);
                    return None;
                }
            };
            if !check_meta(self.shared, &page.meta, self.permitted) {
                return None;
            }
            for session in page.data {
                out.insert(session.session_id.as_bytes().to_vec(), session);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Some(out);
            }
        }
    }

    async fn users(&self) -> Option<BTreeMap<Vec<u8>, UserSummary>> {
        let mut out = BTreeMap::new();
        let mut cursor: Option<IdCursor> = None;
        loop {
            let query = UserListQuery {
                window: self.window,
                limit: Some(200),
                cursor: cursor.take(),
            };
            let page = match self.service.user_list(&self.ctx, query).await {
                Ok(page) => page,
                Err(error) => {
                    check_error(self.shared, &error, self.permitted);
                    return None;
                }
            };
            if !check_meta(self.shared, &page.meta, self.permitted) {
                return None;
            }
            for user in page.data {
                out.insert(user.user_id.as_bytes().to_vec(), user);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Some(out);
            }
        }
    }

    async fn check_sessions(&self, reference: &Reference) {
        let Some(listed) = self.sessions().await else {
            return;
        };
        let (expected, found): (BTreeSet<&Vec<u8>>, BTreeSet<&Vec<u8>>) =
            (reference.sessions.keys().collect(), listed.keys().collect());
        if expected != found {
            self.wrong(format!(
                "the session list holds {} sessions, the reference {}",
                found.len(),
                expected.len()
            ));
            return;
        }
        for (id, session) in &listed {
            let Some(want) = reference.sessions.get(id) else {
                continue;
            };
            if session_measures(session) != measures_only(want) {
                self.wrong(format!(
                    "session {} reads {:?}, the reference is {:?}",
                    text(id),
                    session_measures(session),
                    measures_only(want)
                ));
                continue;
            }
            let users: BTreeSet<Vec<u8>> = session
                .user_ids
                .iter()
                .map(|user| user.as_bytes().to_vec())
                .collect();
            let kept = if want.users.len() <= SESSION_USERS_KEPT {
                users == want.users
            } else {
                users.len() == SESSION_USERS_KEPT && users.is_subset(&want.users)
            };
            if !kept {
                self.wrong(format!(
                    "session {} names users {users:?}, the reference {:?}",
                    text(id),
                    want.users
                ));
            }
            match self
                .service
                .session(&self.ctx, self.window, &text(id))
                .await
            {
                Ok(detail) => {
                    if check_meta(self.shared, &detail.meta, self.permitted)
                        && detail.session.as_ref() != Some(session)
                    {
                        self.wrong(format!(
                            "session {} reads differently alone than in the list",
                            text(id)
                        ));
                    }
                }
                Err(error) => {
                    check_error(self.shared, &error, self.permitted);
                }
            }
        }
    }

    async fn check_users(&self, reference: &Reference) {
        let Some(listed) = self.users().await else {
            return;
        };
        let (expected, found): (BTreeSet<&Vec<u8>>, BTreeSet<&Vec<u8>>) =
            (reference.users.keys().collect(), listed.keys().collect());
        if expected != found {
            self.wrong(format!(
                "the user list holds {} users, the reference {}",
                found.len(),
                expected.len()
            ));
            return;
        }
        for (id, user) in &listed {
            let Some(want) = reference.users.get(id) else {
                continue;
            };
            let sessions = want.sessions.len() as u64;
            if user_measures(user) != measures_only(want) || user.session_count != sessions {
                self.wrong(format!(
                    "user {} reads {:?} with {} sessions, the reference is {:?} with {sessions}",
                    text(id),
                    user_measures(user),
                    user.session_count,
                    measures_only(want)
                ));
                continue;
            }
            match self.service.user(&self.ctx, self.window, &text(id)).await {
                Ok(detail) => {
                    if check_meta(self.shared, &detail.meta, self.permitted)
                        && detail.user.as_ref() != Some(user)
                    {
                        self.wrong(format!(
                            "user {} reads differently alone than in the list",
                            text(id)
                        ));
                    }
                }
                Err(error) => {
                    check_error(self.shared, &error, self.permitted);
                }
            }
            self.check_user_sessions(id, reference).await;
        }
    }

    async fn check_user_sessions(&self, user: &[u8], reference: &Reference) {
        let empty = BTreeMap::new();
        let want = reference.user_sessions.get(user).unwrap_or(&empty);
        let mut found = BTreeMap::new();
        let mut cursor: Option<IdCursor> = None;
        loop {
            let query = UserSessionsQuery {
                window: self.window,
                user_id: text(user),
                limit: Some(200),
                cursor: cursor.take(),
            };
            let page = match self.service.user_sessions(&self.ctx, query).await {
                Ok(page) => page,
                Err(error) => {
                    check_error(self.shared, &error, self.permitted);
                    return;
                }
            };
            if !check_meta(self.shared, &page.meta, self.permitted) {
                return;
            }
            for session in page.data {
                let id = session.session_id.as_bytes().to_vec();
                let read = (
                    session.trace_count,
                    session.first_trace_at.micros(),
                    session.last_trace_at.micros(),
                );
                found.insert(id, read);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        let expected: BTreeMap<Vec<u8>, (u64, i64, i64)> = want
            .iter()
            .map(|(id, s)| (id.clone(), (s.traces.len() as u64, s.first, s.last)))
            .collect();
        if found != expected {
            self.wrong(format!(
                "the sessions of user {} read {found:?}, the reference is {expected:?}",
                text(user)
            ));
        }
    }
}

/// I7 on the read paths of sessions and users, for every project.
pub(super) async fn check(shared: &Shared, node: &Node, model: &ChModel) {
    if shared.ledger().stats.quarantines > 0 {
        return;
    }
    let Some(window) = window() else {
        return;
    };
    for project in 0..shared.plan.projects {
        let Some(ctx) = shared.context(project) else {
            return;
        };
        let Some(reference) = reference(model, project_id(project).to_bytes(), window) else {
            shared.ledger().violate(Violation::Aggregate(
                "a row of span_core or trace_heads does not read".to_owned(),
            ));
            return;
        };
        let reader = Reader {
            shared,
            service: &node.query,
            ctx,
            window,
            permitted: Permitted::quiescent(false, "rollup lists"),
        };
        reader.check_sessions(&reference).await;
        reader.check_users(&reference).await;
    }
}
