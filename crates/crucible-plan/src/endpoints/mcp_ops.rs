//! The MCP analytics over the raw rows of `mcp_ops` and `mcp_content` (contract 9.9): the error breakdown, the
//! client versions, users, sessions, the operation list and one operation. None of them is bound by the 300 ms
//! dashboard target. Caller-fault calls are left out unless the read includes them (contract 9.2).

use crucible_records::{ErrorClass, McpSide, ProjectScope, SpanId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{Column, McpContentCol, McpOpsCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::endpoints::mcp::{McpRead, and, side_pred};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{
    AggOutput, Aggregate, ArrivalBounds, Direction, FieldRange, Measure, Node, Order, PointLookup,
    Scan,
};
use crate::plan::QueryPlan;
use crate::pred::{CmpOp, Pred};

type O = McpOpsCol;

/// The largest page of the MCP lists: users, sessions, operations, the error breakdown and client versions.
pub const MCP_LIST_MAX_LIMIT: u32 = 200;
/// The most rows of one multi-round chain that the operation detail reads.
pub const MCP_CHAIN_LIMIT: u32 = 64;
/// The most rows of the other side of one operation that the operation detail reads: more than one only under
/// double capture.
pub const MCP_PEER_LIMIT: u32 = 8;

const ZERO_16: [u8; 16] = [0; 16];

fn ops_conditions(read: &McpRead<'_>) -> Result<Option<Vec<Pred>>, PlanError> {
    let Some(mut parts) = read.conditions(
        O::Side.column_ref(),
        O::ServerName.column_ref(),
        O::CaptureSource.column_ref(),
    )?
    else {
        return Ok(None);
    };
    if !read.include_caller_faults {
        parts.push(Pred::eq(O::CallerFault, 0u32));
    }
    Ok(Some(parts))
}

fn errors() -> Pred {
    Pred::cmp(
        O::ErrorClass,
        CmpOp::Ne,
        Literal::Enum(ErrorClass::None.value()),
    )
}

fn check_limit(limit: u32) -> Result<(), PlanError> {
    if limit == 0 || limit > MCP_LIST_MAX_LIMIT {
        Err(PlanError::Limit)
    } else {
        Ok(())
    }
}

fn raw(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    class: QueryClass,
    root: Node,
) -> Result<Option<QueryPlan>, PlanError> {
    clamp.plan(scope, window, caller.class(class), root)
}

/// The errors of a read by what they say, most frequent first, at most `limit + 1` rows: `server_name`,
/// `method`, `target_name`, `error_class`, `error_subclass`, `jsonrpc_code`, `error_message`, then `count()`
/// and `max(start_time)`, in class `analytics_raw`.
pub fn mcp_error_breakdown(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    limit: u32,
) -> Result<Option<QueryPlan>, PlanError> {
    check_limit(limit)?;
    let Some(mut parts) = ops_conditions(read)? else {
        return Ok(None);
    };
    parts.push(errors());
    let group = [
        O::ServerName,
        O::Method,
        O::TargetName,
        O::ErrorClass,
        O::ErrorSubclass,
        O::JsonrpcCode,
        O::ErrorMessage,
    ];
    let mut order = vec![
        (AggOutput::Measure(0), Direction::Desc),
        (AggOutput::Measure(1), Direction::Desc),
    ];
    order.extend((0..group.len()).map(|i| (AggOutput::Group(i), Direction::Asc)));
    let aggregate = Aggregate::new(
        O::TABLE,
        and(parts),
        group,
        vec![Measure::Count, Measure::Max(O::StartTime.column_ref())],
    )?
    .ordered(order)?
    .limited(limit + 1)?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    )
}

/// The self-reported client versions of a read, most calls first, at most `limit + 1` rows: `client_name`,
/// `client_version`, then `count()` and `max(start_time)`, in class `analytics_raw`.
pub fn mcp_client_versions(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    limit: u32,
) -> Result<Option<QueryPlan>, PlanError> {
    check_limit(limit)?;
    let Some(parts) = ops_conditions(read)? else {
        return Ok(None);
    };
    let aggregate = Aggregate::new(
        O::TABLE,
        and(parts),
        [O::ClientName, O::ClientVersion],
        vec![Measure::Count, Measure::Max(O::StartTime.column_ref())],
    )?
    .ordered(vec![
        (AggOutput::Measure(0), Direction::Desc),
        (AggOutput::Group(0), Direction::Asc),
        (AggOutput::Group(1), Direction::Asc),
    ])?
    .limited(limit + 1)?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    )
}

/// A user of the MCP analytics: the `user_id` from the identify hook and the hashed token subject, either of
/// which may be empty (contract 9.3).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct McpUserKey {
    /// `user_id`, empty when absent.
    pub user_id: String,
    /// `user_subject_hash`, all zero when absent.
    pub subject_hash: [u8; 16],
}

/// The users of a read, last seen first, at most `limit + 1` rows: `user_id`, `user_subject_hash`, then
/// `count()`, `min(start_time)`, `max(start_time)`, `uniq(group_key)` and `max(identity_source)`, in class
/// `analytics_raw`. Operations without either identity are left out.
pub fn mcp_users(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    limit: u32,
) -> Result<Option<QueryPlan>, PlanError> {
    check_limit(limit)?;
    let Some(mut parts) = ops_conditions(read)? else {
        return Ok(None);
    };
    parts.push(Pred::Or(vec![
        Pred::cmp(O::UserId, CmpOp::Ne, ""),
        Pred::cmp(
            O::UserSubjectHash,
            CmpOp::Ne,
            Literal::Bytes(ZERO_16.to_vec()),
        ),
    ]));
    let aggregate = Aggregate::new(
        O::TABLE,
        and(parts),
        [O::UserId, O::UserSubjectHash],
        vec![
            Measure::Count,
            Measure::Min(O::StartTime.column_ref()),
            Measure::Max(O::StartTime.column_ref()),
            Measure::Uniq(O::GroupKey.column_ref()),
            Measure::Max(O::IdentitySource.column_ref()),
        ],
    )?
    .ordered(vec![
        (AggOutput::Measure(2), Direction::Desc),
        (AggOutput::Group(0), Direction::Desc),
        (AggOutput::Group(1), Direction::Desc),
    ])?
    .limited(limit + 1)?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    )
}

/// The failed operations of a page of users: `user_id`, `user_subject_hash`, then `count()`, one row per user
/// of `users` with an error. The plan keeps every pair of the ids and hashes given; the caller matches the
/// page's own pairs.
pub fn mcp_user_errors(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    users: &[McpUserKey],
) -> Result<Option<QueryPlan>, PlanError> {
    let Some(mut parts) = ops_conditions(read)? else {
        return Ok(None);
    };
    if users.is_empty() {
        return Err(PlanError::Empty);
    }
    parts.push(errors());
    parts.push(Pred::In(
        O::UserId.column_ref(),
        users
            .iter()
            .map(|u| Literal::String(u.user_id.clone()))
            .collect(),
    ));
    parts.push(Pred::In(
        O::UserSubjectHash.column_ref(),
        users
            .iter()
            .map(|u| Literal::Bytes(u.subject_hash.to_vec()))
            .collect(),
    ));
    let aggregate = Aggregate::new(
        O::TABLE,
        and(parts),
        [O::UserId, O::UserSubjectHash],
        vec![Measure::Count],
    )?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    )
}

/// The MCP sessions of a read (contract 9.4), last seen first, at most `limit + 1` rows: `group_key`,
/// `group_key_kind`, then `count()`, `min(start_time)`, `max(start_time)` and `max(client_name)`, in class
/// `analytics_raw`. Operations without a group key (a process of unknown instance) are left out.
pub fn mcp_sessions(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    limit: u32,
) -> Result<Option<QueryPlan>, PlanError> {
    check_limit(limit)?;
    let Some(mut parts) = ops_conditions(read)? else {
        return Ok(None);
    };
    parts.push(Pred::cmp(O::GroupKey, CmpOp::Ne, ""));
    let aggregate = Aggregate::new(
        O::TABLE,
        and(parts),
        [O::GroupKey, O::GroupKeyKind],
        vec![
            Measure::Count,
            Measure::Min(O::StartTime.column_ref()),
            Measure::Max(O::StartTime.column_ref()),
            Measure::Max(O::ClientName.column_ref()),
        ],
    )?
    .ordered(vec![
        (AggOutput::Measure(2), Direction::Desc),
        (AggOutput::Group(0), Direction::Desc),
        (AggOutput::Group(1), Direction::Desc),
    ])?
    .limited(limit + 1)?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    )
}

/// The failed operations of a page of sessions: `group_key`, `group_key_kind`, then `count()`, one row per
/// session of `keys` with an error.
pub fn mcp_session_errors(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    keys: &[String],
) -> Result<Option<QueryPlan>, PlanError> {
    let Some(mut parts) = ops_conditions(read)? else {
        return Ok(None);
    };
    if keys.is_empty() {
        return Err(PlanError::Empty);
    }
    parts.push(errors());
    parts.push(Pred::In(
        O::GroupKey.column_ref(),
        keys.iter().cloned().map(Literal::String).collect(),
    ));
    let aggregate = Aggregate::new(
        O::TABLE,
        and(parts),
        [O::GroupKey, O::GroupKeyKind],
        vec![Measure::Count],
    )?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::AnalyticsRaw,
        Node::Aggregate(aggregate),
    )
}

/// What the operation list is narrowed to, beyond the read.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct McpOpsFilter {
    /// `target_name`: a tool or prompt name, or a resource template.
    pub target: Option<String>,
    /// `method`.
    pub method: Option<String>,
    /// The self-reported `client_name`.
    pub client: Option<String>,
    /// This error class only.
    pub error_class: Option<ErrorClass>,
    /// Failed operations only (an error class other than `none`).
    pub errors_only: bool,
    /// One MCP session: its `group_key`.
    pub session: Option<String>,
    /// One user: a `user_id`, or a subject hash in 32 hex characters, which matches either.
    pub user: Option<String>,
}

impl McpOpsFilter {
    fn conditions(&self) -> Vec<Pred> {
        let mut parts = Vec::new();
        if let Some(target) = &self.target {
            parts.push(Pred::eq(O::TargetName, target.as_str()));
        }
        if let Some(method) = &self.method {
            parts.push(Pred::eq(O::Method, method.as_str()));
        }
        if let Some(client) = &self.client {
            parts.push(Pred::eq(O::ClientName, client.as_str()));
        }
        if let Some(class) = self.error_class {
            parts.push(Pred::eq(O::ErrorClass, Literal::Enum(class.value())));
        }
        if self.errors_only {
            parts.push(errors());
        }
        if let Some(session) = &self.session {
            parts.push(Pred::eq(O::GroupKey, session.as_str()));
        }
        if let Some(user) = &self.user {
            let by_id = Pred::eq(O::UserId, user.as_str());
            parts.push(match subject_hash(user) {
                Some(hash) => Pred::Or(vec![
                    by_id,
                    Pred::eq(O::UserSubjectHash, Literal::Bytes(hash.to_vec())),
                ]),
                None => by_id,
            });
        }
        parts
    }
}

/// A subject hash written as 32 hex characters, the wire form of contract 1.3.
pub fn subject_hash(text: &str) -> Option<[u8; 16]> {
    if text.len() != 32 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// One page of the operation list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpOperationsRequest {
    /// What the list is narrowed to.
    pub filter: McpOpsFilter,
    /// The page size, 1 to [`MCP_LIST_MAX_LIMIT`].
    pub limit: u32,
    /// The sort tuple `(start_time, span_id)` of the last row of the previous page.
    pub cursor: Option<(Timestamp, SpanId)>,
    /// Fixed by the first page: only rows with `received_at <= as_of` are read, so pages do not shift.
    pub as_of: Timestamp,
}

/// The columns of a row of the operation list, in order.
pub const MCP_OPERATION_COLUMNS: [McpOpsCol; 32] = [
    O::TraceId,
    O::SpanId,
    O::ParentSpanId,
    O::StartTime,
    O::DurationUs,
    O::ServerName,
    O::ServerVersion,
    O::DeployId,
    O::CaptureSource,
    O::Era,
    O::Transport,
    O::Method,
    O::TargetKind,
    O::TargetName,
    O::Status,
    O::ErrorClass,
    O::ErrorSubclass,
    O::CallerFault,
    O::JsonrpcCode,
    O::ErrorType,
    O::ErrorMessage,
    O::ClientName,
    O::ClientVersion,
    O::UserId,
    O::UserSubjectHash,
    O::IdentitySource,
    O::GroupKey,
    O::GroupKeyKind,
    O::RequestBytes,
    O::ResponseBytes,
    O::ContentCaptured,
    O::ResultType,
];

/// The operation list: the [`MCP_OPERATION_COLUMNS`] of the read's operations, newest first, ordered by
/// `(start_minute, start_time, span_id)` descending along the sort key, at most `limit + 1` rows, in class
/// `list`.
pub fn mcp_operations(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    read: &McpRead<'_>,
    request: &McpOperationsRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    check_limit(request.limit)?;
    let Some(mut parts) = ops_conditions(read)? else {
        return Ok(None);
    };
    parts.extend(request.filter.conditions());
    let mut scan = Scan::new(O::TABLE, MCP_OPERATION_COLUMNS, and(parts))?
        .ordered(Order::by(
            [O::StartMinute, O::StartTime, O::SpanId],
            Direction::Desc,
        ))?
        .limited(request.limit + 1)?
        .arrived(ArrivalBounds {
            as_of: Some(request.as_of),
            since: None,
        })?;
    if let Some((start_time, span_id)) = request.cursor {
        scan = scan.after(vec![
            start_time.floor_minute().into(),
            start_time.into(),
            Literal::from(span_id),
        ])?;
    }
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::List,
        Node::Scan(scan),
    )
}

/// The key columns of one operation in `mcp_ops` and `mcp_content`, after `project_id`.
fn operation_key(side: McpSide, trace_id: TraceId, span_id: SpanId) -> Vec<Literal> {
    vec![
        Literal::Enum(side.value()),
        trace_id.trace_hash().into(),
        trace_id.into(),
        span_id.into(),
    ]
}

/// One operation: every column of `mcp_ops` (in `McpOpsCol::ALL` order) for the span of `side`, at most one
/// row, in class `point`.
pub fn mcp_operation(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    side: McpSide,
    (trace_id, span_id): (TraceId, SpanId),
) -> Result<Option<QueryPlan>, PlanError> {
    side_pred(O::Side, side)?;
    let lookup = PointLookup::new(
        O::TABLE,
        [O::Side, O::TraceHash, O::TraceId, O::SpanId],
        vec![operation_key(side, trace_id, span_id)],
        O::ALL.iter().copied(),
    )?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::Point,
        Node::PointLookup(lookup),
    )
}

/// A captured content field of an operation (contract 3.5).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum McpContentField {
    /// The tool call's arguments.
    Args,
    /// The tool call's result.
    Result,
}

/// The first `len` bytes of a captured content field of one operation, then the field's stored length in
/// bytes, from `mcp_content`, in class `point`. No row when nothing was captured or the content expired.
pub fn mcp_operation_content(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    (side, trace_id, span_id): (McpSide, TraceId, SpanId),
    field: McpContentField,
    len: u64,
) -> Result<Option<QueryPlan>, PlanError> {
    side_pred(McpContentCol::Side, side)?;
    let column = match field {
        McpContentField::Args => McpContentCol::Args,
        McpContentField::Result => McpContentCol::Result,
    };
    let range = FieldRange::new(
        McpContentCol::TABLE,
        [
            McpContentCol::Side,
            McpContentCol::TraceHash,
            McpContentCol::TraceId,
            McpContentCol::SpanId,
        ],
        operation_key(side, trace_id, span_id),
        column,
        0,
        len,
    )?
    .with_total();
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::Point,
        Node::FieldRange(range),
    )
}

/// The other side of one operation in its trace (contract 9.1): every column of `mcp_ops` for the rows of
/// `other` whose span is the operation's parent or whose parent is the operation's span. More than one row
/// only under double capture, at most [`MCP_PEER_LIMIT`].
pub fn mcp_operation_peers(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    other: McpSide,
    (trace_id, span_id, parent): (TraceId, SpanId, SpanId),
) -> Result<Option<QueryPlan>, PlanError> {
    let mut related = vec![Pred::eq(O::ParentSpanId, span_id)];
    if parent != SpanId::ZERO {
        related.push(Pred::eq(O::SpanId, parent));
    }
    let pred = Pred::And(vec![
        side_pred(O::Side, other)?,
        Pred::eq(O::TraceHash, trace_id.trace_hash()),
        Pred::eq(O::TraceId, trace_id),
        and_or(related),
    ]);
    let scan = Scan::new(O::TABLE, O::ALL.iter().copied(), pred)?
        .ordered(Order::by([O::StartTime, O::SpanId], Direction::Asc))?
        .limited(MCP_PEER_LIMIT)?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::Point,
        Node::Scan(scan),
    )
}

fn and_or(mut parts: Vec<Pred>) -> Pred {
    match parts.len() {
        1 => parts.swap_remove(0),
        _ => Pred::Or(parts),
    }
}

/// The columns of a row of a multi-round chain, in order.
pub const MCP_CHAIN_COLUMNS: [McpOpsCol; 8] = [
    O::SpanId,
    O::StartTime,
    O::MrtrRound,
    O::MrtrStateIn,
    O::MrtrStateOut,
    O::Method,
    O::TargetName,
    O::ResultType,
];

/// The multi-round operations of one trace (contract 9.6): the [`MCP_CHAIN_COLUMNS`] of the rows of `side`
/// and `capture_source` that carry a state hash, oldest first, at most [`MCP_CHAIN_LIMIT`]. Chains never cross
/// capture sources; the service links them by their hashes.
pub fn mcp_operation_chain(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    side: McpSide,
    (trace_id, capture_source): (TraceId, crucible_records::CaptureSource),
) -> Result<Option<QueryPlan>, PlanError> {
    let zero = || Literal::Bytes(ZERO_16.to_vec());
    let pred = Pred::And(vec![
        side_pred(O::Side, side)?,
        Pred::eq(O::TraceHash, trace_id.trace_hash()),
        Pred::eq(O::TraceId, trace_id),
        Pred::eq(O::CaptureSource, Literal::Enum(capture_source.value())),
        Pred::Or(vec![
            Pred::cmp(O::MrtrStateIn, CmpOp::Ne, zero()),
            Pred::cmp(O::MrtrStateOut, CmpOp::Ne, zero()),
        ]),
    ]);
    let scan = Scan::new(O::TABLE, MCP_CHAIN_COLUMNS, pred)?
        .ordered(Order::by([O::StartTime, O::SpanId], Direction::Asc))?
        .limited(MCP_CHAIN_LIMIT)?;
    raw(
        scope,
        window,
        clamp,
        caller,
        QueryClass::Point,
        Node::Scan(scan),
    )
}
