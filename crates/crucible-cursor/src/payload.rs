//! The payload: a versioned binary record with a fixed layout per kind, big-endian, at most
//! [`MAX_PAYLOAD_LEN`] bytes.
//!
//! | Bytes | Field |
//! |---|---|
//! | 0 | payload version, 1 |
//! | 1 | kind: 1 `page`, 2 `head` |
//! | 2 | route: 1 trace list, 2 span children, 3 span list, 4 session list, 5 user list, 6 a user's sessions |
//! | 3 | sort: 1 newest first, 2 oldest first |
//! | 4..20 | project id |
//! | 20..36 | filter hash |
//! | 36..44 | issued at, `i64` microseconds |
//! | 44..52 | expires at, `i64` microseconds |
//! | 52.. | the position (below) |
//!
//! | Kind and route | Position | Total |
//! |---|---|---|
//! | page, trace list | window `from` and `to`, `as_of`, then the last row's `start_time`, `trace_hash` (`u32`) and `trace_id` | 104 |
//! | head, trace list | window `from` and `to`, `as_of`, `wal_epoch`, LSN (`u64`) | 100 |
//! | page, span children | `trace_id`, parent `span_id`, then the last child's `start_time` and `span_id` | 92 |
//! | page, span list | window `from` and `to`, `as_of`, then the last row's `start_time` and `span_id` | 92 |
//! | page, session list, user list or a user's sessions | window `from` and `to`, the last row's `last_seen`, then its id: length (`u16`) and bytes | 78 to 590 |
//!
//! Decoding accepts exactly what encoding produces: another version, an unknown code, a head of any list but
//! the trace list, a window that is not `from < to`, an id longer than [`MAX_ID_LEN`], or a length other than
//! the layout's is an error. Values the layout cannot check (the hash, the times, the id's bytes) are carried
//! as they are; the MAC is what vouches for them.

use crucible_records::{Lsn, ProjectId, SpanId, TimeWindow, Timestamp, TraceId, WalEpoch};
use sha2::{Digest as _, Sha256};

use crate::CURSOR_TTL_MICROS;
use crate::ring::Binding;

/// The payload version this codec writes and reads.
pub const PAYLOAD_VERSION: u8 = 1;

/// The longest id that a page of a list keyed by a text id carries: the cap of `session_id` and `user_id`
/// (contract 3.1).
#[cfg(not(kani))]
pub const MAX_ID_LEN: usize = 512;
/// Under Kani the cap is the one that makes the longest id page as long as a trace page, so the proofs keep the
/// bounds they had before ids existed; every id length takes the same code path, and the property tests and
/// the fuzz target cover ids up to the real cap.
#[cfg(kani)]
pub const MAX_ID_LEN: usize = 26;

/// The longest payload: a page of a list keyed by a text id with an id of [`MAX_ID_LEN`] bytes, or a trace
/// page (104 bytes) when that is longer.
pub const MAX_PAYLOAD_LEN: usize = if 78 + MAX_ID_LEN > 104 {
    78 + MAX_ID_LEN
} else {
    104
};

/// The kind of a cursor (contract 12.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// The position after the last row of a page.
    Page,
    /// The newest position of a list, for live tail.
    Head,
}

impl Kind {
    const fn code(self) -> u8 {
        match self {
            Self::Page => 1,
            Self::Head => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Page),
            2 => Some(Self::Head),
            _ => None,
        }
    }
}

/// The list a cursor belongs to: the route id of contract 12.4. Codes are stored in cursors, so new routes
/// append.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Route {
    /// `GET projects/{id}/traces`.
    TraceList,
    /// `GET projects/{id}/traces/{trace_id}/spans`.
    SpanChildren,
    /// `GET projects/{id}/spans`.
    SpanList,
    /// `GET projects/{id}/sessions`.
    SessionList,
    /// `GET projects/{id}/users`.
    UserList,
    /// `GET projects/{id}/users/{user_id}/sessions`.
    UserSessions,
}

impl Route {
    const fn code(self) -> u8 {
        match self {
            Self::TraceList => 1,
            Self::SpanChildren => 2,
            Self::SpanList => 3,
            Self::SessionList => 4,
            Self::UserList => 5,
            Self::UserSessions => 6,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::TraceList),
            2 => Some(Self::SpanChildren),
            3 => Some(Self::SpanList),
            4 => Some(Self::SessionList),
            5 => Some(Self::UserList),
            6 => Some(Self::UserSessions),
            _ => None,
        }
    }
}

/// A list whose rows are keyed by a text id and ordered by `(last_seen, id)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IdList {
    /// The session list.
    Sessions,
    /// The user list.
    Users,
    /// The sessions of one user.
    UserSessions,
}

impl IdList {
    /// The route of the list.
    pub const fn route(self) -> Route {
        match self {
            Self::Sessions => Route::SessionList,
            Self::Users => Route::UserList,
            Self::UserSessions => Route::UserSessions,
        }
    }
}

/// The id of the last row of a page of an [`IdList`]: at most [`MAX_ID_LEN`] bytes, held inline so that a
/// cursor stays a fixed-size value.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CursorId {
    len: u16,
    bytes: [u8; MAX_ID_LEN],
}

impl CursorId {
    /// The id, when it is at most [`MAX_ID_LEN`] bytes.
    pub fn new(id: &[u8]) -> Option<Self> {
        if id.len() > MAX_ID_LEN {
            return None;
        }
        let mut bytes = [0; MAX_ID_LEN];
        bytes[..id.len()].copy_from_slice(id);
        Some(Self {
            len: id.len() as u16,
            bytes,
        })
    }

    /// The id's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl core::fmt::Debug for CursorId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "CursorId({:?})",
            String::from_utf8_lossy(self.as_bytes())
        )
    }
}

/// The order of a list. Lists paginate only when sorted by time (contract 12.4). Codes are stored in cursors,
/// so new orders append.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sort {
    /// Newest first.
    NewestFirst,
    /// Oldest first.
    OldestFirst,
}

impl Sort {
    const fn code(self) -> u8 {
        match self {
            Self::NewestFirst => 1,
            Self::OldestFirst => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::NewestFirst),
            2 => Some(Self::OldestFirst),
            _ => None,
        }
    }
}

/// The filter hash of contract 12.4: the first 16 bytes of SHA-256 over the canonical form of a request's
/// filters. Each route defines its canonical form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FilterHash([u8; 16]);

impl FilterHash {
    /// The hash of a canonical filter form.
    pub fn of(canonical: &[u8]) -> Self {
        let digest = Sha256::digest(canonical);
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(bytes)
    }

    /// Wraps the stored bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The stored bytes.
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Where a cursor resumes, with the parts of its binding that only some lists have.
#[expect(
    clippy::large_enum_variant,
    reason = "an id page holds its id inline so that a cursor stays a fixed-size `Copy` value for the codec \
              and its proofs; a cursor lives for one request"
)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Position {
    /// The next page of the trace list.
    TracePage {
        /// The window of the list.
        window: TimeWindow,
        /// Fixed by the first page; later pages read only rows that arrived by then.
        as_of: Timestamp,
        /// The sort tuple `(start_time, trace_hash, trace_id)` of the last row.
        last: (Timestamp, u32, TraceId),
    },
    /// Live tail of the trace list: the newest position a response reflected.
    TraceHead {
        /// The window of the list.
        window: TimeWindow,
        /// The instant of the response that issued the cursor.
        as_of: Timestamp,
        /// The epoch of the WAL whose LSNs `lsn` counts: the issuing node's.
        wal_epoch: WalEpoch,
        /// The LSN that node's recent index had applied.
        lsn: Lsn,
    },
    /// The next page of the children of a span.
    ChildPage {
        /// The trace.
        trace_id: TraceId,
        /// The span whose children are listed.
        parent: SpanId,
        /// The sort tuple `(start_time, span_id)` of the last child.
        last: (Timestamp, SpanId),
    },
    /// The next page of the span list.
    SpanPage {
        /// The window of the list.
        window: TimeWindow,
        /// Fixed by the first page; later pages read only rows that arrived by then.
        as_of: Timestamp,
        /// The sort tuple `(start_time, span_id)` of the last row.
        last: (Timestamp, SpanId),
    },
    /// The next page of a list keyed by a text id.
    IdPage {
        /// The list.
        list: IdList,
        /// The window of the list.
        window: TimeWindow,
        /// The sort tuple `(last_seen, id)` of the last row.
        last: (Timestamp, CursorId),
    },
}

impl Position {
    /// The kind of a cursor at this position.
    pub const fn kind(&self) -> Kind {
        match self {
            Self::TracePage { .. }
            | Self::ChildPage { .. }
            | Self::SpanPage { .. }
            | Self::IdPage { .. } => Kind::Page,
            Self::TraceHead { .. } => Kind::Head,
        }
    }

    /// The list this position belongs to.
    pub const fn route(&self) -> Route {
        match self {
            Self::TracePage { .. } | Self::TraceHead { .. } => Route::TraceList,
            Self::ChildPage { .. } => Route::SpanChildren,
            Self::SpanPage { .. } => Route::SpanList,
            Self::IdPage { list, .. } => list.route(),
        }
    }

    /// The window of the list, for lists that have one.
    pub const fn window(&self) -> Option<TimeWindow> {
        match self {
            Self::TracePage { window, .. }
            | Self::TraceHead { window, .. }
            | Self::SpanPage { window, .. }
            | Self::IdPage { window, .. } => Some(*window),
            Self::ChildPage { .. } => None,
        }
    }
}

/// A cursor's payload, decoded: what it is bound to, where it resumes, and when it was issued and expires.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Cursor {
    /// The project of the request that issued it.
    pub project_id: ProjectId,
    /// The filters of that request.
    pub filter: FilterHash,
    /// The sort of that request.
    pub sort: Sort,
    /// Where the next request resumes.
    pub position: Position,
    /// When it was issued.
    pub issued_at: Timestamp,
    /// The first instant at which it no longer opens.
    pub expires_at: Timestamp,
}

/// Why a payload did not decode.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, thiserror::Error)]
pub enum DecodeError {
    /// Shorter or longer than its kind's layout.
    #[error("the payload's length does not match its layout")]
    Length,
    /// A version this codec does not read.
    #[error("payload version {0} is not supported")]
    Version(u8),
    /// An unknown code, the pair head and span children, or a window that is not `from < to`.
    #[error("a payload value is outside its domain")]
    Value,
}

impl Cursor {
    /// A cursor issued at `issued_at`, expiring [`CURSOR_TTL_MICROS`] later (contract 12.4).
    pub const fn new(
        project_id: ProjectId,
        filter: FilterHash,
        sort: Sort,
        position: Position,
        issued_at: Timestamp,
    ) -> Self {
        Self {
            project_id,
            filter,
            sort,
            position,
            issued_at,
            expires_at: issued_at.saturating_add_micros(CURSOR_TTL_MICROS),
        }
    }

    /// What the cursor is bound to: a request with exactly this binding accepts it.
    pub const fn binding(&self) -> Binding {
        Binding {
            project_id: self.project_id,
            route: self.position.route(),
            kind: self.position.kind(),
            filter: self.filter,
            sort: self.sort,
            window: self.position.window(),
        }
    }

    /// The binary payload.
    pub fn encode(&self) -> Payload {
        let mut w = Writer {
            bytes: [0; MAX_PAYLOAD_LEN],
            len: 0,
        };
        w.put(&[
            PAYLOAD_VERSION,
            self.position.kind().code(),
            self.position.route().code(),
            self.sort.code(),
        ]);
        w.put(&self.project_id.to_bytes());
        w.put(&self.filter.0);
        w.time(self.issued_at);
        w.time(self.expires_at);
        match self.position {
            Position::TracePage {
                window,
                as_of,
                last: (start, hash, trace),
            } => {
                w.window(window);
                w.time(as_of);
                w.time(start);
                w.put(&hash.to_be_bytes());
                w.put(trace.as_bytes());
            }
            Position::TraceHead {
                window,
                as_of,
                wal_epoch,
                lsn,
            } => {
                w.window(window);
                w.time(as_of);
                w.put(&wal_epoch.to_bytes());
                w.put(&lsn.get().to_be_bytes());
            }
            Position::ChildPage {
                trace_id,
                parent,
                last: (start, span),
            } => {
                w.put(trace_id.as_bytes());
                w.put(parent.as_bytes());
                w.time(start);
                w.put(span.as_bytes());
            }
            Position::SpanPage {
                window,
                as_of,
                last: (start, span),
            } => {
                w.window(window);
                w.time(as_of);
                w.time(start);
                w.put(span.as_bytes());
            }
            Position::IdPage {
                window,
                last: (last_seen, id),
                ..
            } => {
                w.window(window);
                w.time(last_seen);
                w.put(&id.len.to_be_bytes());
                w.put(id.as_bytes());
            }
        }
        Payload {
            bytes: w.bytes,
            len: w.len,
        }
    }

    /// Decodes a binary payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader(bytes);
        let version = r.u8()?;
        if version != PAYLOAD_VERSION {
            return Err(DecodeError::Version(version));
        }
        let kind = Kind::from_code(r.u8()?).ok_or(DecodeError::Value)?;
        let route = Route::from_code(r.u8()?).ok_or(DecodeError::Value)?;
        let sort = Sort::from_code(r.u8()?).ok_or(DecodeError::Value)?;
        let project_id = ProjectId::from_bytes(r.array()?);
        let filter = FilterHash(r.array()?);
        let issued_at = r.time()?;
        let expires_at = r.time()?;
        let position = match (kind, route) {
            (Kind::Page, Route::TraceList) => Position::TracePage {
                window: r.window()?,
                as_of: r.time()?,
                last: (
                    r.time()?,
                    u32::from_be_bytes(r.array()?),
                    TraceId::from_bytes(r.array()?),
                ),
            },
            (Kind::Head, Route::TraceList) => Position::TraceHead {
                window: r.window()?,
                as_of: r.time()?,
                wal_epoch: WalEpoch::from_bytes(r.array()?),
                lsn: Lsn::new(u64::from_be_bytes(r.array()?)),
            },
            (Kind::Page, Route::SpanChildren) => Position::ChildPage {
                trace_id: TraceId::from_bytes(r.array()?),
                parent: SpanId::from_bytes(r.array()?),
                last: (r.time()?, SpanId::from_bytes(r.array()?)),
            },
            (Kind::Page, Route::SpanList) => Position::SpanPage {
                window: r.window()?,
                as_of: r.time()?,
                last: (r.time()?, SpanId::from_bytes(r.array()?)),
            },
            (Kind::Page, Route::SessionList) => r.id_page(IdList::Sessions)?,
            (Kind::Page, Route::UserList) => r.id_page(IdList::Users)?,
            (Kind::Page, Route::UserSessions) => r.id_page(IdList::UserSessions)?,
            (
                Kind::Head,
                Route::SpanChildren
                | Route::SpanList
                | Route::SessionList
                | Route::UserList
                | Route::UserSessions,
            ) => return Err(DecodeError::Value),
        };
        if !r.0.is_empty() {
            return Err(DecodeError::Length);
        }
        Ok(Self {
            project_id,
            filter,
            sort,
            position,
            issued_at,
            expires_at,
        })
    }
}

/// An encoded payload, held without allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Payload {
    bytes: [u8; MAX_PAYLOAD_LEN],
    len: usize,
}

impl Payload {
    /// The payload's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Appends to a payload. Every layout fits [`MAX_PAYLOAD_LEN`].
struct Writer {
    bytes: [u8; MAX_PAYLOAD_LEN],
    len: usize,
}

impl Writer {
    fn put(&mut self, data: &[u8]) {
        let end = self.len + data.len();
        self.bytes[self.len..end].copy_from_slice(data);
        self.len = end;
    }

    fn time(&mut self, t: Timestamp) {
        self.put(&t.micros().to_be_bytes());
    }

    fn window(&mut self, window: TimeWindow) {
        self.time(window.from());
        self.time(window.to());
    }
}

/// Reads a payload; every read fails once it is exhausted.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let (head, rest) = self.0.split_first_chunk::<N>().ok_or(DecodeError::Length)?;
        self.0 = rest;
        Ok(*head)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(u8::from_be_bytes(self.array()?))
    }

    fn time(&mut self) -> Result<Timestamp, DecodeError> {
        Ok(Timestamp::from_micros(i64::from_be_bytes(self.array()?)))
    }

    fn window(&mut self) -> Result<TimeWindow, DecodeError> {
        TimeWindow::new(self.time()?, self.time()?).map_err(|_| DecodeError::Value)
    }

    fn id_page(&mut self, list: IdList) -> Result<Position, DecodeError> {
        let window = self.window()?;
        let last_seen = self.time()?;
        let len = usize::from(u16::from_be_bytes(self.array()?));
        if len > MAX_ID_LEN {
            return Err(DecodeError::Value);
        }
        let (id, rest) = self.0.split_at_checked(len).ok_or(DecodeError::Length)?;
        self.0 = rest;
        let id = CursorId::new(id).ok_or(DecodeError::Value)?;
        Ok(Position::IdPage {
            list,
            window,
            last: (last_seen, id),
        })
    }
}
