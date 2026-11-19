//! The filter-values lookup (contract 8.7 and 8.8): the most frequent values of a metrics source's filter field
//! in a window.

use std::sync::Arc;

use crucible_plan::{
    FILTER_VALUES_MAX_LIMIT, FilterField, FilterValuesRequest, Value, filter_values,
};
use crucible_records::{Level, ObservationType, TableId, TimeWindow};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::service::QueryService;

/// A filter-values request: the lookup and its window.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilterValuesQuery {
    /// The window.
    pub window: TimeWindow,
    /// The source, the field, the prefix and the limit.
    pub request: FilterValuesRequest,
}

/// The values of a filter field, most frequent first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilterValues {
    /// Each value with the number of spans that carry it in the window.
    pub data: Vec<(Arc<str>, u64)>,
    /// Whether more values exist than `data` holds.
    pub truncated: bool,
    /// The response metadata.
    pub meta: Meta,
}

fn bad() -> QueryServiceError {
    QueryServiceError::Internal("the store returned a filter value of the wrong type".to_owned())
}

impl QueryService {
    /// The values of a filter field with their counts (contract 8.8). A field with a closed set of names
    /// (`level`, `observation_type`) is read whole and its prefix applied to the names here.
    pub async fn filter_values(
        &self,
        ctx: &ReadContext,
        query: FilterValuesQuery,
    ) -> Result<FilterValues, QueryServiceError> {
        let FilterValuesQuery { window, request } = query;
        let mut read = self.read(ctx);
        let consistency = self.consistency();
        let state = self.index.read_state(ctx.scope);
        let named = matches!(
            request.field,
            FilterField::Level | FilterField::ObservationType
        );
        let plan_request = FilterValuesRequest {
            limit: if named {
                FILTER_VALUES_MAX_LIMIT
            } else {
                request.limit
            },
            ..request.clone()
        };
        let plan = filter_values(ctx.scope, window, &read.clamp, ctx.caller, &plan_request);
        let mut data = Vec::new();
        for row in read.rows(plan).await? {
            let (value, count) = match row.values() {
                [value, Value::UInt(count)] => (value, *count),
                _ => return Err(bad()),
            };
            let value: Arc<str> = match (request.field, value) {
                (FilterField::Level, Value::Enum(v)) => {
                    Arc::from(Level::from_value(*v).ok_or_else(bad)?.as_str())
                }
                (FilterField::ObservationType, Value::Enum(v)) => {
                    Arc::from(ObservationType::from_value(*v).ok_or_else(bad)?.as_str())
                }
                (_, Value::String(text)) => Arc::from(text.as_str()),
                (_, Value::Bytes(bytes)) => Arc::from(String::from_utf8_lossy(bytes)),
                _ => return Err(bad()),
            };
            let prefixed = request
                .prefix
                .as_deref()
                .is_none_or(|prefix| value.starts_with(prefix));
            if prefixed {
                data.push((value, count));
            }
        }
        let truncated = data.len() > request.limit as usize;
        data.truncate(request.limit as usize);
        let meta = read.finish(read.now, consistency, state, &[TableId::SpanMetrics], false)?;
        Ok(FilterValues {
            data,
            truncated,
            meta,
        })
    }
}
