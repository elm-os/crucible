//! Cursors, as values. `crucible-cursor` (milestone 2) will encode each of these as the signed
//! `v1.<kid>.<payload>.<mac>` string of contract 12.4 and add the route, filter hash, sort and expiry. This
//! module is the seam: the services mint and accept these values and check what a value can check (project,
//! trace and window), and the codec will produce them with `from_parts` only after it verified the MAC.

use crucible_records::{Lsn, ProjectId, ProjectScope, SpanId, TimeWindow, Timestamp, TraceId};

use crate::context::QueryServiceError;

/// The sort tuple of the trace list: `(start_time, trace_hash, trace_id)` of a head.
pub type HeadKey = (Timestamp, u32, TraceId);

/// A cursor of kind `page` for the trace list: the sort tuple of the last row and the `as_of` the first page
/// fixed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PageCursor {
    project_id: ProjectId,
    window: TimeWindow,
    as_of: Timestamp,
    last: HeadKey,
}

impl PageCursor {
    /// A cursor from its decoded parts, for the cursor codec.
    pub const fn from_parts(
        project_id: ProjectId,
        window: TimeWindow,
        as_of: Timestamp,
        last: HeadKey,
    ) -> Self {
        Self {
            project_id,
            window,
            as_of,
            last,
        }
    }

    /// The parts, for the cursor codec: project, window, `as_of` and the last sort tuple.
    pub const fn parts(&self) -> (ProjectId, TimeWindow, Timestamp, HeadKey) {
        (self.project_id, self.window, self.as_of, self.last)
    }

    pub(crate) fn check(
        &self,
        scope: ProjectScope,
        window: TimeWindow,
    ) -> Result<(Timestamp, HeadKey), QueryServiceError> {
        if (self.project_id, self.window) == (scope.project_id(), window) {
            Ok((self.as_of, self.last))
        } else {
            Err(QueryServiceError::CursorInvalid)
        }
    }
}

/// A cursor of kind `head` for live tail: the newest position (`as_of`, and the LSN stage B had applied) of the
/// response that issued it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HeadCursor {
    project_id: ProjectId,
    window: TimeWindow,
    as_of: Timestamp,
    lsn: Lsn,
}

impl HeadCursor {
    /// A cursor from its decoded parts, for the cursor codec.
    pub const fn from_parts(
        project_id: ProjectId,
        window: TimeWindow,
        as_of: Timestamp,
        lsn: Lsn,
    ) -> Self {
        Self {
            project_id,
            window,
            as_of,
            lsn,
        }
    }

    /// The parts, for the cursor codec: project, window, `as_of` and the applied LSN.
    pub const fn parts(&self) -> (ProjectId, TimeWindow, Timestamp, Lsn) {
        (self.project_id, self.window, self.as_of, self.lsn)
    }

    pub(crate) fn check(
        &self,
        scope: ProjectScope,
        window: TimeWindow,
    ) -> Result<(Timestamp, Lsn), QueryServiceError> {
        if (self.project_id, self.window) == (scope.project_id(), window) {
            Ok((self.as_of, self.lsn))
        } else {
            Err(QueryServiceError::CursorInvalid)
        }
    }
}

/// A cursor for the children of one span: the `(start_time, span_id)` of the last child returned.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChildCursor {
    project_id: ProjectId,
    trace_id: TraceId,
    parent: SpanId,
    last: (Timestamp, SpanId),
}

impl ChildCursor {
    /// A cursor from its decoded parts, for the cursor codec.
    pub const fn from_parts(
        project_id: ProjectId,
        trace_id: TraceId,
        parent: SpanId,
        last: (Timestamp, SpanId),
    ) -> Self {
        Self {
            project_id,
            trace_id,
            parent,
            last,
        }
    }

    /// The parts, for the cursor codec: project, trace, parent and the last sort tuple.
    pub const fn parts(&self) -> (ProjectId, TraceId, SpanId, (Timestamp, SpanId)) {
        (self.project_id, self.trace_id, self.parent, self.last)
    }

    pub(crate) fn check(
        &self,
        scope: ProjectScope,
        trace_id: TraceId,
        parent: SpanId,
    ) -> Result<(Timestamp, SpanId), QueryServiceError> {
        if (self.project_id, self.trace_id, self.parent) == (scope.project_id(), trace_id, parent) {
            Ok(self.last)
        } else {
            Err(QueryServiceError::CursorInvalid)
        }
    }
}
