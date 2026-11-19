//! The MCP analytics over the raw rows (contract 9.9): the error breakdown, client versions, users, sessions,
//! the operation list and one operation, from `mcp_ops` and `mcp_content`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crucible_plan::{
    CaptureClass, MCP_OPERATION_COLUMNS, McpContentField, McpOperationsRequest, McpOpsFilter,
    McpRead, McpSelection, McpUserKey, Value, mcp_client_versions, mcp_error_breakdown,
    mcp_operation, mcp_operation_chain, mcp_operation_content, mcp_operation_peers,
    mcp_operations, mcp_session_errors, mcp_sessions, mcp_user_errors, mcp_users,
};
use crucible_records::{
    CaptureSource, Era, ErrorClass, ErrorSubclass, GroupKeyKind, IdentitySource, McpSide,
    McpStatus, McpTargetKind, ResultType, SpanId, TableId, Timestamp, TraceId, Transport,
};
use crucible_schema::{Column, McpOpsCol};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::decode::Fields;
use crate::mcp::{CaptureSources, McpQuery, fields};
use crate::service::QueryService;

/// The bytes of each captured content field that the operation detail returns; the rest is read through the
/// payload route.
pub const MCP_CONTENT_PREVIEW: u64 = 16 * 1024;

fn bad(what: &str) -> QueryServiceError {
    QueryServiceError::Internal(format!(
        "the store returned an MCP operation without a valid {what}"
    ))
}

fn read_for<'a>(query: &'a McpQuery, selection: &'a McpSelection) -> McpRead<'a> {
    McpRead {
        side: query.side,
        selection,
        include_caller_faults: query.include_caller_faults,
        server: query.server.as_deref(),
    }
}

/// One group of the error breakdown.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpErrorGroup {
    /// The server.
    pub server_name: Arc<str>,
    /// The method.
    pub method: Arc<str>,
    /// The tool or prompt name, or the resource template.
    pub target_name: Arc<str>,
    /// The error class.
    pub error_class: ErrorClass,
    /// The subclass an SDK annotated.
    pub error_subclass: ErrorSubclass,
    /// The JSON-RPC error code, when the error had one.
    pub jsonrpc_code: Option<i64>,
    /// The scrubbed error message.
    pub error_message: Arc<str>,
    /// The failed calls.
    pub calls: u64,
    /// When the last of them started.
    pub last_seen: Timestamp,
}

/// The error breakdown of a side (the raw section of `errors`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpErrorBreakdown {
    /// The groups, most calls first.
    pub groups: Vec<McpErrorGroup>,
    /// Whether more groups exist than `groups` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// One self-reported client version.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpClientVersion {
    /// The self-reported client name.
    pub client_name: Arc<str>,
    /// The self-reported client version.
    pub client_version: Arc<str>,
    /// Its calls.
    pub calls: u64,
    /// When its last call started.
    pub last_seen: Timestamp,
}

/// The client versions of a side (the raw section of `clients`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpClientVersions {
    /// The versions, most calls first.
    pub versions: Vec<McpClientVersion>,
    /// Whether more versions exist than `versions` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// One user.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpUser {
    /// The identify hook's user id and the hashed token subject.
    pub key: McpUserKey,
    /// Its calls.
    pub calls: u64,
    /// Its failed calls.
    pub errors: u64,
    /// Its distinct MCP sessions.
    pub sessions: u64,
    /// How the identity was established, at its strongest.
    pub identity_source: IdentitySource,
    /// First seen.
    pub first_seen: Timestamp,
    /// Last seen.
    pub last_seen: Timestamp,
}

/// The users of a side.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpUsers {
    /// The users, last seen first.
    pub users: Vec<McpUser>,
    /// Whether more users exist than `users` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// One MCP session: the operations of one group key (contract 9.4).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpSession {
    /// The group key.
    pub group_key: Arc<str>,
    /// What the group key is.
    pub kind: GroupKeyKind,
    /// Its calls.
    pub calls: u64,
    /// Its failed calls.
    pub errors: u64,
    /// A self-reported client name of the session.
    pub client_name: Arc<str>,
    /// First seen.
    pub first_seen: Timestamp,
    /// Last seen.
    pub last_seen: Timestamp,
}

/// The MCP sessions of a side.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpSessions {
    /// The sessions, last seen first.
    pub sessions: Vec<McpSession>,
    /// Whether more sessions exist than `sessions` holds.
    pub truncated: bool,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata.
    pub meta: Meta,
}

/// One operation of the list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpOperationRow {
    /// The trace.
    pub trace_id: TraceId,
    /// The operation's span.
    pub span_id: SpanId,
    /// Its parent span; zero when it has none.
    pub parent_span_id: SpanId,
    /// Start.
    pub start_time: Timestamp,
    /// Duration in microseconds.
    pub duration_us: u64,
    /// The server.
    pub server_name: Arc<str>,
    /// The server's version.
    pub server_version: Arc<str>,
    /// The deploy.
    pub deploy_id: Arc<str>,
    /// Who observed it.
    pub capture_source: CaptureSource,
    /// The protocol era.
    pub era: Era,
    /// The transport.
    pub transport: Transport,
    /// The method.
    pub method: Arc<str>,
    /// What the method targets.
    pub target_kind: McpTargetKind,
    /// The tool or prompt name, or the resource template.
    pub target_name: Arc<str>,
    /// The outcome.
    pub status: McpStatus,
    /// The error class.
    pub error_class: ErrorClass,
    /// The subclass an SDK annotated.
    pub error_subclass: ErrorSubclass,
    /// Whether the caller caused the error.
    pub caller_fault: bool,
    /// The JSON-RPC error code.
    pub jsonrpc_code: Option<i64>,
    /// The semantic-convention error type.
    pub error_type: Arc<str>,
    /// The scrubbed error message.
    pub error_message: Arc<str>,
    /// The self-reported client name.
    pub client_name: Arc<str>,
    /// The self-reported client version.
    pub client_version: Arc<str>,
    /// The identify hook's user id.
    pub user_id: Arc<str>,
    /// The hashed token subject.
    pub user_subject_hash: Option<[u8; 16]>,
    /// How the identity was established.
    pub identity_source: IdentitySource,
    /// The MCP session.
    pub group_key: Arc<str>,
    /// What the group key is.
    pub group_key_kind: GroupKeyKind,
    /// Request bytes; 0 when unknown.
    pub request_bytes: u64,
    /// Response bytes; 0 when unknown.
    pub response_bytes: u64,
    /// Whether arguments and results were captured.
    pub content_captured: bool,
    /// What the result was.
    pub result_type: ResultType,
}

fn decoded<T>(value: Option<T>, what: &str) -> Result<T, QueryServiceError> {
    value.ok_or_else(|| bad(what))
}

impl McpOperationRow {
    /// A row whose first columns are [`MCP_OPERATION_COLUMNS`], at the positions `at` gives.
    fn decode(f: &Fields<'_>, at: impl Fn(McpOpsCol) -> usize) -> Result<Self, QueryServiceError> {
        use McpOpsCol as C;
        let e = |c: C| f.enum8(at(c));
        Ok(Self {
            trace_id: f.trace_id(at(C::TraceId))?,
            span_id: f.span_id(at(C::SpanId))?,
            parent_span_id: f.span_id(at(C::ParentSpanId))?,
            start_time: f.time(at(C::StartTime))?,
            duration_us: f.uint(at(C::DurationUs))?,
            server_name: f.text(at(C::ServerName))?,
            server_version: f.text(at(C::ServerVersion))?,
            deploy_id: f.text(at(C::DeployId))?,
            capture_source: f.capture_source(at(C::CaptureSource))?,
            era: decoded(Era::from_value(e(C::Era)?), "era")?,
            transport: decoded(Transport::from_value(e(C::Transport)?), "transport")?,
            method: f.text(at(C::Method))?,
            target_kind: decoded(McpTargetKind::from_value(e(C::TargetKind)?), "target kind")?,
            target_name: f.text(at(C::TargetName))?,
            status: decoded(McpStatus::from_value(e(C::Status)?), "status")?,
            error_class: f.error_class(at(C::ErrorClass))?,
            error_subclass: decoded(ErrorSubclass::from_value(e(C::ErrorSubclass)?), "subclass")?,
            caller_fault: f.uint(at(C::CallerFault))? != 0,
            jsonrpc_code: f.opt_int(at(C::JsonrpcCode))?,
            error_type: f.text(at(C::ErrorType))?,
            error_message: f.text(at(C::ErrorMessage))?,
            client_name: f.text(at(C::ClientName))?,
            client_version: f.text(at(C::ClientVersion))?,
            user_id: f.text(at(C::UserId))?,
            user_subject_hash: f.hash16(at(C::UserSubjectHash))?,
            identity_source: decoded(
                IdentitySource::from_value(e(C::IdentitySource)?),
                "identity source",
            )?,
            group_key: f.text(at(C::GroupKey))?,
            group_key_kind: decoded(GroupKeyKind::from_value(e(C::GroupKeyKind)?), "group kind")?,
            request_bytes: f.uint(at(C::RequestBytes))?,
            response_bytes: f.uint(at(C::ResponseBytes))?,
            content_captured: f.uint(at(C::ContentCaptured))? != 0,
            result_type: decoded(ResultType::from_value(e(C::ResultType)?), "result type")?,
        })
    }
}

fn position(columns: &[McpOpsCol], column: McpOpsCol) -> usize {
    columns
        .iter()
        .position(|c| *c == column)
        .unwrap_or(usize::MAX)
}

/// One page of the operation list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpOperationsQuery {
    /// The request.
    pub query: McpQuery,
    /// What the list is narrowed to.
    pub filter: McpOpsFilter,
    /// The page size.
    pub limit: u32,
    /// The position after the previous page and the `as_of` its first page fixed, if this is a later page.
    pub cursor: Option<McpOperationCursor>,
}

/// Where the next page of the operation list starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct McpOperationCursor {
    /// Fixed by the first page.
    pub as_of: Timestamp,
    /// The sort tuple `(start_time, span_id)` of the last row.
    pub last: (Timestamp, SpanId),
}

/// One page of the operation list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpOperationsPage {
    /// The operations, newest first.
    pub operations: Vec<McpOperationRow>,
    /// Where the next page starts, if there is one.
    pub next: Option<McpOperationCursor>,
    /// The capture source of each server.
    pub capture_sources: CaptureSources,
    /// The response metadata; its `as_of` is the one the first page fixed.
    pub meta: Meta,
}

/// The first bytes of a captured content field.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpContent {
    /// The first [`MCP_CONTENT_PREVIEW`] bytes, as text; a character cut at the end is replaced.
    pub preview: Arc<str>,
    /// The stored length of the field in bytes.
    pub bytes: u64,
}

/// One link of a multi-round exchange (contract 9.6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpChainLink {
    /// The operation's span.
    pub span_id: SpanId,
    /// Start.
    pub start_time: Timestamp,
    /// The round: the sender's count when it sent one, else the position in the chain, from 1.
    pub round: u16,
    /// The method.
    pub method: Arc<str>,
    /// The target.
    pub target_name: Arc<str>,
    /// What the result was.
    pub result_type: ResultType,
}

/// One operation in detail.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpOperationDetail {
    /// The operation; `None` only when the answer is not whole (`meta` says why), so it may yet appear.
    pub operation: Option<McpOperationFull>,
    /// The response metadata.
    pub meta: Meta,
}

/// Everything known about one operation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpOperationFull {
    /// The columns the list shows.
    pub row: McpOperationRow,
    /// The side.
    pub side: McpSide,
    /// The instance of the server.
    pub instance_id: Arc<str>,
    /// The protocol version.
    pub protocol_version: Arc<str>,
    /// The JSON-RPC id.
    pub jsonrpc_id: Arc<str>,
    /// The HTTP status, when the transport is HTTP.
    pub http_status: Option<u64>,
    /// The OAuth client id.
    pub oauth_client_id: Arc<str>,
    /// The tenant.
    pub tenant: Arc<str>,
    /// Captured arguments, when present and not expired.
    pub args: Option<McpContent>,
    /// The captured result, when present and not expired.
    pub result: Option<McpContent>,
    /// The other side's row of the call in the same trace, when captured; under double capture the one of the
    /// best-ranked capture source.
    pub other_side: Option<McpOperationRow>,
    /// The multi-round exchange the operation belongs to, oldest first; empty when it is not part of one.
    pub chain: Vec<McpChainLink>,
}

impl QueryService {
    /// The error breakdown of a side (the raw section of contract 9.9 `errors`): failed calls by server,
    /// method, target, class, subclass, JSON-RPC code and message, from `mcp_ops`.
    pub async fn mcp_error_breakdown(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
        limit: u32,
    ) -> Result<McpErrorBreakdown, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let selection = read.mcp_selection(query).await?;
        let plan = mcp_error_breakdown(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &read_for(query, &selection),
            limit,
        );
        let mut groups = Vec::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            groups.push(McpErrorGroup {
                server_name: f.text(0)?,
                method: f.text(1)?,
                target_name: f.text(2)?,
                error_class: f.error_class(3)?,
                error_subclass: decoded(ErrorSubclass::from_value(f.enum8(4)?), "subclass")?,
                jsonrpc_code: f.opt_int(5)?,
                error_message: f.text(6)?,
                calls: f.uint(7)?,
                last_seen: f.time(8)?,
            });
        }
        let truncated = groups.len() > limit as usize;
        groups.truncate(limit as usize);
        let meta = read.mcp_meta(consistency, &[TableId::McpToolRollup, TableId::McpOps])?;
        Ok(McpErrorBreakdown {
            groups,
            truncated,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// The self-reported client versions of a side (the raw section of contract 9.9 `clients`), from
    /// `mcp_ops`.
    pub async fn mcp_client_versions(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
        limit: u32,
    ) -> Result<McpClientVersions, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let selection = read.mcp_selection(query).await?;
        let plan = mcp_client_versions(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &read_for(query, &selection),
            limit,
        );
        let mut versions = Vec::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            versions.push(McpClientVersion {
                client_name: f.text(0)?,
                client_version: f.text(1)?,
                calls: f.uint(2)?,
                last_seen: f.time(3)?,
            });
        }
        let truncated = versions.len() > limit as usize;
        versions.truncate(limit as usize);
        let meta = read.mcp_meta(consistency, &[TableId::McpToolRollup, TableId::McpOps])?;
        Ok(McpClientVersions {
            versions,
            truncated,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// The users of a side (contract 9.9 `users`): calls, failed calls, sessions and first and last seen per
    /// user id or subject hash, from `mcp_ops`, last seen first.
    pub async fn mcp_users(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
        limit: u32,
    ) -> Result<McpUsers, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let selection = read.mcp_selection(query).await?;
        let mcp = read_for(query, &selection);
        let plan = mcp_users(ctx.scope, query.window, &read.clamp, ctx.caller, &mcp, limit);
        let mut users = Vec::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            users.push(McpUser {
                key: McpUserKey {
                    user_id: f.text(0)?.to_string(),
                    subject_hash: f.hash16(1)?.unwrap_or([0; 16]),
                },
                calls: f.uint(2)?,
                errors: 0,
                first_seen: f.time(3)?,
                last_seen: f.time(4)?,
                sessions: f.uint(5)?,
                identity_source: decoded(
                    IdentitySource::from_value(f.enum8(6)?),
                    "identity source",
                )?,
            });
        }
        let truncated = users.len() > limit as usize;
        users.truncate(limit as usize);
        if !users.is_empty() {
            let keys: Vec<McpUserKey> = users.iter().map(|u| u.key.clone()).collect();
            let plan = mcp_user_errors(ctx.scope, query.window, &read.clamp, ctx.caller, &mcp, &keys);
            let mut errors: BTreeMap<McpUserKey, u64> = BTreeMap::new();
            for row in read.rows(plan).await? {
                let f = fields(&row);
                let key = McpUserKey {
                    user_id: f.text(0)?.to_string(),
                    subject_hash: f.hash16(1)?.unwrap_or([0; 16]),
                };
                errors.insert(key, f.uint(2)?);
            }
            for user in &mut users {
                user.errors = errors.get(&user.key).copied().unwrap_or(0);
            }
        }
        let meta = read.mcp_meta(consistency, &[TableId::McpToolRollup, TableId::McpOps])?;
        Ok(McpUsers {
            users,
            truncated,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// The MCP sessions of a side (contract 9.9 `sessions`): calls, failed calls and first and last seen per
    /// group key with its kind, from `mcp_ops`, last seen first.
    pub async fn mcp_sessions(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
        limit: u32,
    ) -> Result<McpSessions, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let selection = read.mcp_selection(query).await?;
        let mcp = read_for(query, &selection);
        let plan = mcp_sessions(ctx.scope, query.window, &read.clamp, ctx.caller, &mcp, limit);
        let mut sessions = Vec::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            sessions.push(McpSession {
                group_key: f.text(0)?,
                kind: decoded(GroupKeyKind::from_value(f.enum8(1)?), "group kind")?,
                calls: f.uint(2)?,
                errors: 0,
                first_seen: f.time(3)?,
                last_seen: f.time(4)?,
                client_name: f.text(5)?,
            });
        }
        let truncated = sessions.len() > limit as usize;
        sessions.truncate(limit as usize);
        if !sessions.is_empty() {
            let keys: Vec<String> = sessions.iter().map(|s| s.group_key.to_string()).collect();
            let plan =
                mcp_session_errors(ctx.scope, query.window, &read.clamp, ctx.caller, &mcp, &keys);
            let mut errors: BTreeMap<(Arc<str>, u8), u64> = BTreeMap::new();
            for row in read.rows(plan).await? {
                let f = fields(&row);
                errors.insert((f.text(0)?, f.enum8(1)?), f.uint(2)?);
            }
            for session in &mut sessions {
                let key = (Arc::clone(&session.group_key), session.kind.value());
                session.errors = errors.get(&key).copied().unwrap_or(0);
            }
        }
        let meta = read.mcp_meta(consistency, &[TableId::McpToolRollup, TableId::McpOps])?;
        Ok(McpSessions {
            sessions,
            truncated,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// One page of the operation list (contract 9.9 `operations`), newest first, from `mcp_ops`.
    pub async fn mcp_operations(
        &self,
        ctx: &ReadContext,
        page: &McpOperationsQuery,
    ) -> Result<McpOperationsPage, QueryServiceError> {
        let query = &page.query;
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let as_of = page.cursor.map_or(read.now, |cursor| cursor.as_of);
        let selection = read.mcp_selection(query).await?;
        let request = McpOperationsRequest {
            filter: page.filter.clone(),
            limit: page.limit,
            cursor: page.cursor.map(|cursor| cursor.last),
            as_of,
        };
        let plan = mcp_operations(
            ctx.scope,
            query.window,
            &read.clamp,
            ctx.caller,
            &read_for(query, &selection),
            &request,
        );
        let mut operations = Vec::new();
        for row in read.rows(plan).await? {
            let f = fields(&row);
            operations.push(McpOperationRow::decode(&f, |c| {
                position(&MCP_OPERATION_COLUMNS, c)
            })?);
        }
        let more = operations.len() > page.limit as usize;
        operations.truncate(page.limit as usize);
        let next = match (more, operations.last()) {
            (true, Some(last)) => Some(McpOperationCursor {
                as_of,
                last: (last.start_time, last.span_id),
            }),
            _ => None,
        };
        let mut meta = read.mcp_meta(consistency, &[TableId::McpToolRollup, TableId::McpOps])?;
        meta.as_of = as_of;
        Ok(McpOperationsPage {
            operations,
            next,
            capture_sources: selection.servers().to_vec(),
            meta,
        })
    }

    /// One operation (contract 9.9 `operations/{trace_id}/{span_id}`): its row of `mcp_ops`, the first bytes
    /// of its captured content from `mcp_content`, the other side's row of the call in the same trace, and
    /// the multi-round exchange it belongs to.
    pub async fn mcp_operation(
        &self,
        ctx: &ReadContext,
        query: &McpQuery,
        (trace_id, span_id): (TraceId, SpanId),
    ) -> Result<McpOperationDetail, QueryServiceError> {
        let consistency = self.consistency();
        let mut read = self.read(ctx);
        let (scope, caller, window) = (ctx.scope, ctx.caller, query.window);
        let plan = mcp_operation(
            scope,
            window,
            &read.clamp,
            caller,
            query.side,
            (trace_id, span_id),
        );
        let found = read.rows(plan).await?;
        let tables = [TableId::McpOps, TableId::McpContent];
        let Some(row) = found.first() else {
            let meta = read.mcp_meta(consistency, &tables)?;
            return if meta.is_whole() {
                Err(QueryServiceError::NotFound)
            } else {
                Ok(McpOperationDetail {
                    operation: None,
                    meta,
                })
            };
        };
        let f = fields(row);
        let all = McpOpsCol::ALL;
        let at = |c: McpOpsCol| position(all, c);
        let op = McpOperationRow::decode(&f, at)?;
        let http_status = match f.0.get(at(McpOpsCol::HttpStatus)) {
            Some(Value::UInt(status)) => Some(*status),
            _ => None,
        };
        let (instance_id, protocol_version, jsonrpc_id) = (
            f.text(at(McpOpsCol::InstanceId))?,
            f.text(at(McpOpsCol::ProtocolVersion))?,
            f.text(at(McpOpsCol::JsonrpcId))?,
        );
        let (oauth_client_id, tenant) = (
            f.text(at(McpOpsCol::OauthClientId))?,
            f.text(at(McpOpsCol::Tenant))?,
        );
        let (state_in, state_out) = (
            f.hash16(at(McpOpsCol::MrtrStateIn))?,
            f.hash16(at(McpOpsCol::MrtrStateOut))?,
        );

        let mut content = [None, None];
        if op.content_captured {
            for (slot, field) in content
                .iter_mut()
                .zip([McpContentField::Args, McpContentField::Result])
            {
                let key = (query.side, trace_id, span_id);
                let plan = mcp_operation_content(
                    scope,
                    window,
                    &read.clamp,
                    caller,
                    key,
                    field,
                    MCP_CONTENT_PREVIEW,
                );
                if let Some(row) = read.rows(plan).await?.first() {
                    let f = fields(row);
                    *slot = Some(McpContent {
                        preview: f.text(0)?,
                        bytes: f.uint(1)?,
                    });
                }
            }
        }

        let other = match query.side {
            McpSide::Server => McpSide::Client,
            _ => McpSide::Server,
        };
        let plan = mcp_operation_peers(
            scope,
            window,
            &read.clamp,
            caller,
            other,
            (trace_id, span_id, op.parent_span_id),
        );
        let mut peers = Vec::new();
        for row in read.rows(plan).await? {
            peers.push(McpOperationRow::decode(&fields(&row), at)?);
        }
        peers.sort_by_key(|p| (CaptureClass::of(p.capture_source), p.start_time));
        let other_side = peers.into_iter().next();

        let mut chain = Vec::new();
        if state_in.is_some() || state_out.is_some() {
            let plan = mcp_operation_chain(
                scope,
                window,
                &read.clamp,
                caller,
                query.side,
                (trace_id, op.capture_source),
            );
            let mut links = Vec::new();
            for row in read.rows(plan).await? {
                let f = fields(&row);
                links.push(Link {
                    span_id: f.span_id(0)?,
                    start_time: f.time(1)?,
                    round: f.uint(2)?,
                    state_in: f.hash16(3)?,
                    state_out: f.hash16(4)?,
                    method: f.text(5)?,
                    target_name: f.text(6)?,
                    result_type: decoded(ResultType::from_value(f.enum8(7)?), "result type")?,
                });
            }
            chain = chain_of(&links, span_id);
        }
        let meta = read.mcp_meta(consistency, &tables)?;
        let [args, result] = content;
        Ok(McpOperationDetail {
            operation: Some(McpOperationFull {
                row: op,
                side: query.side,
                instance_id,
                protocol_version,
                jsonrpc_id,
                http_status,
                oauth_client_id,
                tenant,
                args,
                result,
                other_side,
                chain,
            }),
            meta,
        })
    }
}

/// One multi-round row while the chain is linked.
#[derive(Clone, Debug)]
struct Link {
    span_id: SpanId,
    start_time: Timestamp,
    round: u64,
    state_in: Option<[u8; 16]>,
    state_out: Option<[u8; 16]>,
    method: Arc<str>,
    target_name: Arc<str>,
    result_type: ResultType,
}

/// The chain through `span_id` (contract 9.6): an operation whose `mrtr_state_in` equals another's
/// `mrtr_state_out` continues that exchange. Followed backwards and forwards from the operation, each step
/// to the earliest match, never revisiting a row; the rounds are the senders' where given, else the positions
/// from 1.
fn chain_of(links: &[Link], span_id: SpanId) -> Vec<McpChainLink> {
    let Some(start) = links.iter().position(|l| l.span_id == span_id) else {
        return Vec::new();
    };
    let unvisited = |order: &[usize], matches: &dyn Fn(&Link) -> bool| {
        links
            .iter()
            .enumerate()
            .find(|(i, l)| matches(l) && !order.contains(i))
            .map(|(i, _)| i)
    };
    let mut order = vec![start];
    let mut at = start;
    while let Some(state) = links.get(at).and_then(|l| l.state_in) {
        let Some(previous) = unvisited(&order, &|l| l.state_out == Some(state)) else {
            break;
        };
        order.insert(0, previous);
        at = previous;
    }
    let mut at = start;
    while let Some(state) = links.get(at).and_then(|l| l.state_out) {
        let Some(next) = unvisited(&order, &|l| l.state_in == Some(state)) else {
            break;
        };
        order.push(next);
        at = next;
    }
    if order.len() < 2 {
        return Vec::new();
    }
    order
        .into_iter()
        .enumerate()
        .filter_map(|(i, at)| {
            let link = links.get(at)?;
            let round = match u16::try_from(link.round) {
                Ok(round) if round > 0 => round,
                _ => u16::try_from(i + 1).unwrap_or(u16::MAX),
            };
            Some(McpChainLink {
                span_id: link.span_id,
                start_time: link.start_time,
                round,
                method: Arc::clone(&link.method),
                target_name: Arc::clone(&link.target_name),
                result_type: link.result_type,
            })
        })
        .collect()
}
