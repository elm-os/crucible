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

mod data_bounds;
mod enums;
mod facts;
mod meta;
mod metrics;
mod payload;
mod problem;
mod search;
mod spans;
mod sql;
mod trace_detail;
mod traces;
mod wire;

pub use data_bounds::{DataBounds, McpBounds, TimeRange};
pub use enums::{
    AuthorKind, Consistency, HeadKind, IoShape, Level, ObservationType, PartialReason,
    ScoreDataType, ScoreSource, StatusCode, TagOp,
};
pub use facts::{
    Correction, CorrectionRequest, FactRef, FactWritten, Facts, FactsParams, FeedbackRequest,
    FlagName, FlagRequest, FlagValue, Score, ScoreValue, TagEdit, TagRequest, TraceFacts,
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
pub use spans::{SpanPage, SpanPageParams};
pub use sql::{
    SavedQuery, SavedQueryInput, SavedQueryList, SavedQueryParameter, SavedQueryParameterType,
    SavedQueryRunRequest, SavedQueryVisibility, SqlActor, SqlActorKind, SqlColumn, SqlExportFormat,
    SqlExportRequest, SqlLimits, SqlRelation, SqlResult, SqlRun, SqlRunKind, SqlRunList,
    SqlRunOutcome, SqlRunRequest, SqlRunStats, SqlSchema, SqlWorkspaceStatus,
};
pub use trace_detail::{Span, TraceDetail, TraceDetailParams};
pub use traces::{TraceListItem, TraceListPage, TraceListParams};
pub use wire::{MAX_SAFE_INTEGER, SafeI64, SafeU64, Saturation, usd_from_nanos};

/// Visits every type the TypeScript client is generated from; `cargo xtask gen-ts` exports each with the
/// types it uses. A new request, parameter or response type joins this list at its sorted place; the order does
/// not change the output, one file per type.
pub fn visit_exported(visitor: &mut impl ts_rs::TypeVisitor) {
    visitor.visit::<CorrectionRequest>();
    visitor.visit::<DataBounds>();
    visitor.visit::<FactWritten>();
    visitor.visit::<Facts>();
    visitor.visit::<FactsParams>();
    visitor.visit::<FeedbackRequest>();
    visitor.visit::<FilterValues>();
    visitor.visit::<FilterValuesParams>();
    visitor.visit::<FlagRequest>();
    visitor.visit::<MetricsQueryRequest>();
    visitor.visit::<MetricsResponse>();
    visitor.visit::<MetricsSchema>();
    visitor.visit::<PayloadParams>();
    visitor.visit::<Problem>();
    visitor.visit::<SavedQuery>();
    visitor.visit::<SavedQueryInput>();
    visitor.visit::<SavedQueryList>();
    visitor.visit::<SavedQueryRunRequest>();
    visitor.visit::<SearchPage>();
    visitor.visit::<SearchRequest>();
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
