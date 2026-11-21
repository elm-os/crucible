//! REST API data transfer objects with serde, OpenAPI and TypeScript derives.
//!
//! Every type here derives serde, the utoipa schema trait and the ts-rs trait from one struct (contract 1.3),
//! so the JSON the server writes, the OpenAPI document (`specs/openapi/crucible-v1.json`) and the TypeScript
//! types of the UI (`ui/src/api/generated/`) cannot disagree.
//!
//! Wire rules (contract 1.3):
//! - integers of 64 bits are [`SafeU64`] or [`SafeI64`]: JSON numbers at most 2^53 - 1, saturating there,
//!   with `saturated: true` on the enclosing object, and `number` in TypeScript, never `bigint`;
//! - costs are JSON numbers in USD named `*_usd`, from nano-USD through [`usd_from_nanos`];
//! - ids and times are strings: lowercase hex for trace and span ids, canonical UUIDs for Crucible ids, and
//!   RFC 3339 UTC with six fractional digits for times;
//! - enums are lowercase snake_case strings;
//! - request parameters reject unknown fields.

mod alerts;
mod dashboards;
mod data_bounds;
mod enums;
mod exports;
mod facts;
mod filters;
mod identity;
mod lifecycle;
mod mcp;
mod media;
mod meta;
mod metrics;
mod payload;
mod problem;
mod search;
mod sessions;
mod span_list;
mod spans;
mod sql;
mod trace_detail;
mod traces;
mod users;
mod wire;

pub use alerts::{
    AlertComparison, AlertEvent, AlertEventKind, AlertHistory, AlertHistoryParams,
    AlertNoDataPolicy, AlertPreview, AlertPreviewSeries, AlertRule, AlertRuleInput, AlertRuleList,
    AlertRulePatch, AlertSeries, AlertStatus, Channel, ChannelCreate, ChannelKind, ChannelList,
    ChannelPatch, ChannelTestResult,
};
pub use dashboards::{
    ChartForm, Dashboard, DashboardCreate, DashboardDuplicate, DashboardList, DashboardPatch,
    DashboardSummary, Widget, WidgetDisplay, WidgetInput, WidgetKind, WidgetLayout, WidgetPatch,
    WidgetQuery,
};
pub use data_bounds::{DataBounds, McpBounds, TimeRange};
pub use enums::{
    AuthorKind, Consistency, HeadKind, IoShape, Level, ObservationType, PartialReason,
    ScoreDataType, ScoreSource, StatusCode, TagOp,
};
pub use exports::{Export, ExportFormat, ExportList, ExportRequest, ExportState};
pub use facts::{
    Correction, CorrectionRequest, FactRef, FactWritten, Facts, FactsParams, FeedbackRequest,
    FlagName, FlagRequest, FlagValue, Score, ScoreValue, TagEdit, TagRequest, TraceFacts,
};
pub use filters::ListFilter;
pub use identity::{
    Account, AccountPatch, ApiKey, ApiKeyCreate, ApiKeyCreated, ApiKeyList, ApiKeyPatch,
    AuthMethod, AuthProvider, AuthProviderCreate, AuthProviderList, AuthProviderPatch,
    FeatureState, Features, Invitation, InvitationAccept, InvitationCreate, InvitationCreated,
    InvitationList, KeyScope, LoginRequest, MediaFeature, Org, OrgMember, OrgMemberList,
    OrgMemberPatch, OrgPatch, OrgRole, OrgSettings, PasswordChange, PasswordResetConfirm,
    PasswordResetRequest, Project, ProjectAccess, ProjectCreate, ProjectGrant, ProjectList,
    ProjectMember, ProjectMemberAdd, ProjectMemberList, ProjectMemberPatch, ProjectPatch,
    ProjectRole, ProviderButton, ProviderKind, ServerInfo, SessionInfo, SessionOrg, SessionProject,
    SessionTimes, SessionUser, SetupRequest, SignInOptions, TransferOwnership, UserStatus,
};
pub use lifecycle::{
    DeletionKind, DeletionPlace, DeletionRequest, DeletionRequestCreate, DeletionRequestList,
    DeletionState, Retention, RetentionHidden, RetentionUpdate,
};
pub use mcp::{
    CaptureClass, CaptureSource, Era, ErrorClass, ErrorSubclass, GroupKeyKind, IdentitySource,
    McpChainLink, McpClassCount, McpClientList, McpClientRow, McpClientVersionRow, McpCompare,
    McpCompareParams, McpContentPreview, McpDeployList, McpDeployRow, McpErrorGroupRow, McpErrors,
    McpLatency, McpListParams, McpOperation, McpOperationDetail, McpOperationPage, McpOperationRow,
    McpOperationsParams, McpParams, McpSeriesPoint, McpServerList, McpServerRow, McpServerSource,
    McpSessionList, McpSessionRow, McpSide, McpSideParam, McpStats, McpStatus, McpTargetErrorRow,
    McpTargetKind, McpToolDeltaRow, McpToolDetail, McpToolList, McpToolRow, McpUserList,
    McpUserRow, McpView, McpViewParams, ResultType, SourceParam, Transport,
};
pub use media::{
    Media, MediaCreate, MediaDisposition, MediaList, MediaListParams, MediaState, MediaUpload,
    MediaUploadUrl, MediaUse, SignedHeader, SpanMedia, SpanMediaList, SpanMediaParams,
};
pub use meta::Meta;
pub use metrics::{
    FillMode, FilterValue, FilterValues, FilterValuesParams, Granularity, MeasureRequest,
    MetricsFilter, MetricsPoint, MetricsQueryRequest, MetricsResponse, MetricsSchema,
    MetricsSeries, SchemaDimension, SchemaFilterField, SchemaLimits, SchemaMeasure, SeriesKey,
    SourceSchema,
};
pub use payload::{PayloadField, PayloadParams};
pub use problem::{FieldError, Problem, ProblemCode, ProblemLimit};
pub use search::{
    SearchField, SearchFragment, SearchHit, SearchMatch, SearchMode, SearchPage, SearchRequest,
};
pub use sessions::{EntityParams, SessionDetail, SessionItem, SessionListParams, SessionPage};
pub use span_list::{SpanListItem, SpanListPage, SpanListParams};
pub use spans::{SpanPage, SpanPageParams};
pub use sql::{
    SavedQuery, SavedQueryInput, SavedQueryList, SavedQueryParameter, SavedQueryParameterType,
    SavedQueryRunRequest, SavedQueryVisibility, SqlActor, SqlActorKind, SqlColumn, SqlExportFormat,
    SqlExportRequest, SqlLimits, SqlRelation, SqlResult, SqlRun, SqlRunKind, SqlRunList,
    SqlRunOutcome, SqlRunRequest, SqlRunStats, SqlSchema, SqlWorkspaceStatus,
};
pub use trace_detail::{Span, TraceDetail, TraceDetailParams};
pub use traces::{TraceListItem, TraceListPage, TraceListParams};
pub use users::{UserDetail, UserItem, UserListParams, UserPage, UserSessionItem, UserSessionPage};
pub use wire::{MAX_SAFE_INTEGER, SafeI64, SafeU64, Saturation, usd_from_nanos};

/// Visits every type the TypeScript client is generated from; `cargo xtask gen-ts` exports each with the
/// types it uses. A new request, parameter or response type joins this list at its sorted place; the order does
/// not change the output, one file per type.
pub fn visit_exported(visitor: &mut impl ts_rs::TypeVisitor) {
    visitor.visit::<Account>();
    visitor.visit::<AccountPatch>();
    visitor.visit::<AlertHistory>();
    visitor.visit::<AlertHistoryParams>();
    visitor.visit::<AlertPreview>();
    visitor.visit::<AlertRule>();
    visitor.visit::<AlertRuleInput>();
    visitor.visit::<AlertRuleList>();
    visitor.visit::<AlertRulePatch>();
    visitor.visit::<ApiKeyCreate>();
    visitor.visit::<ApiKeyCreated>();
    visitor.visit::<ApiKeyList>();
    visitor.visit::<ApiKeyPatch>();
    visitor.visit::<AuthProviderCreate>();
    visitor.visit::<AuthProviderList>();
    visitor.visit::<AuthProviderPatch>();
    visitor.visit::<Channel>();
    visitor.visit::<ChannelCreate>();
    visitor.visit::<ChannelList>();
    visitor.visit::<ChannelPatch>();
    visitor.visit::<ChannelTestResult>();
    visitor.visit::<CorrectionRequest>();
    visitor.visit::<Dashboard>();
    visitor.visit::<DashboardCreate>();
    visitor.visit::<DashboardDuplicate>();
    visitor.visit::<DashboardList>();
    visitor.visit::<DashboardPatch>();
    visitor.visit::<DataBounds>();
    visitor.visit::<DeletionRequest>();
    visitor.visit::<DeletionRequestCreate>();
    visitor.visit::<DeletionRequestList>();
    visitor.visit::<EntityParams>();
    visitor.visit::<Export>();
    visitor.visit::<ExportList>();
    visitor.visit::<ExportRequest>();
    visitor.visit::<FactWritten>();
    visitor.visit::<Facts>();
    visitor.visit::<FactsParams>();
    visitor.visit::<FeedbackRequest>();
    visitor.visit::<FilterValues>();
    visitor.visit::<FilterValuesParams>();
    visitor.visit::<FlagRequest>();
    visitor.visit::<InvitationAccept>();
    visitor.visit::<InvitationCreate>();
    visitor.visit::<InvitationCreated>();
    visitor.visit::<InvitationList>();
    visitor.visit::<LoginRequest>();
    visitor.visit::<McpClientList>();
    visitor.visit::<McpCompare>();
    visitor.visit::<McpCompareParams>();
    visitor.visit::<McpDeployList>();
    visitor.visit::<McpErrors>();
    visitor.visit::<McpListParams>();
    visitor.visit::<McpOperationDetail>();
    visitor.visit::<McpOperationPage>();
    visitor.visit::<McpOperationsParams>();
    visitor.visit::<McpParams>();
    visitor.visit::<McpServerList>();
    visitor.visit::<McpSessionList>();
    visitor.visit::<McpToolDetail>();
    visitor.visit::<McpToolList>();
    visitor.visit::<McpUserList>();
    visitor.visit::<McpViewParams>();
    visitor.visit::<Media>();
    visitor.visit::<MediaCreate>();
    visitor.visit::<MediaList>();
    visitor.visit::<MediaListParams>();
    visitor.visit::<MediaUpload>();
    visitor.visit::<MetricsQueryRequest>();
    visitor.visit::<MetricsResponse>();
    visitor.visit::<MetricsSchema>();
    visitor.visit::<Org>();
    visitor.visit::<OrgMemberList>();
    visitor.visit::<OrgMemberPatch>();
    visitor.visit::<OrgPatch>();
    visitor.visit::<PasswordChange>();
    visitor.visit::<PasswordResetConfirm>();
    visitor.visit::<PasswordResetRequest>();
    visitor.visit::<PayloadParams>();
    visitor.visit::<Problem>();
    visitor.visit::<SavedQuery>();
    visitor.visit::<SavedQueryInput>();
    visitor.visit::<SavedQueryList>();
    visitor.visit::<SavedQueryRunRequest>();
    visitor.visit::<ProjectCreate>();
    visitor.visit::<ProjectList>();
    visitor.visit::<ProjectMemberAdd>();
    visitor.visit::<ProjectMemberList>();
    visitor.visit::<ProjectMemberPatch>();
    visitor.visit::<Retention>();
    visitor.visit::<RetentionUpdate>();
    visitor.visit::<ProjectPatch>();
    visitor.visit::<SearchPage>();
    visitor.visit::<SearchRequest>();
    visitor.visit::<SessionDetail>();
    visitor.visit::<SessionInfo>();
    visitor.visit::<SessionListParams>();
    visitor.visit::<SessionPage>();
    visitor.visit::<SetupRequest>();
    visitor.visit::<SignInOptions>();
    visitor.visit::<SpanListPage>();
    visitor.visit::<SpanListParams>();
    visitor.visit::<SpanMediaList>();
    visitor.visit::<SpanMediaParams>();
    visitor.visit::<SpanPage>();
    visitor.visit::<SpanPageParams>();
    visitor.visit::<SqlExportRequest>();
    visitor.visit::<SqlResult>();
    visitor.visit::<SqlRunList>();
    visitor.visit::<SqlRunRequest>();
    visitor.visit::<SqlSchema>();
    visitor.visit::<TagRequest>();
    visitor.visit::<TraceDetail>();
    visitor.visit::<TraceDetailParams>();
    visitor.visit::<TraceListPage>();
    visitor.visit::<TraceListParams>();
    visitor.visit::<TransferOwnership>();
    visitor.visit::<UserDetail>();
    visitor.visit::<UserListParams>();
    visitor.visit::<UserPage>();
    visitor.visit::<UserSessionPage>();
    visitor.visit::<WidgetInput>();
    visitor.visit::<WidgetPatch>();
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ts_rs::{Config, TS, TypeVisitor};

    use super::*;

    /// The declaration of every exported type and of every type they use, by name.
    #[derive(Default)]
    struct Declarations {
        cfg: Config,
        found: BTreeMap<String, String>,
    }

    impl TypeVisitor for Declarations {
        fn visit<T: TS + 'static + ?Sized>(&mut self) {
            if T::output_path().is_none() || self.found.contains_key(&T::ident(&self.cfg)) {
                return;
            }
            self.found.insert(T::ident(&self.cfg), T::decl(&self.cfg));
            T::visit_dependencies(self);
        }
    }

    fn declarations() -> BTreeMap<String, String> {
        let mut visitor = Declarations::default();
        visit_exported(&mut visitor);
        visitor.found
    }

    #[test]
    fn no_exported_type_uses_bigint() {
        let found = declarations();
        assert!(found.contains_key("SafeU64") && found.contains_key("TraceListItem"));
        for (name, decl) in &found {
            assert!(!decl.contains("bigint"), "{name} uses bigint: {decl}");
        }
        assert_eq!(found["SafeU64"], "type SafeU64 = number;");
    }

    #[test]
    fn absent_members_are_optional_and_null_members_are_required() {
        let found = declarations();
        assert!(
            found["Meta"].contains("partial_reason?: PartialReason"),
            "{}",
            found["Meta"]
        );
        let page = &found["TraceListPage"];
        assert!(page.contains("next_cursor: string | null"), "{page}");
        assert!(page.contains("new_count?: number"), "{page}");
        let params = &found["TraceListParams"];
        assert!(
            params.contains("from: string") && params.contains("limit?: number"),
            "{params}"
        );
    }
}
