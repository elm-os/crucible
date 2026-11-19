//! What the MCP analytics services share (contract 9.9): the request every read takes, the capture-source
//! selection, and the folding of rollup groups into calls, errors, caller-fault calls and latency.
//!
//! Every read first takes the presence of each server's capture sources on its side in the window
//! (`crucible_plan::mcp_presence`) and selects one class per server (`crucible_plan::McpSelection`, contract
//! 9.5); the data plans then carry that choice, so rows of two classes are never summed, and every response
//! names the choice per server. Under `include_caller_faults = false` (the default) caller-fault calls are left
//! out of calls, errors, latency and bytes, and are reported apart (contract 9.2).

use std::sync::Arc;

use crucible_plan::{
    McpSelection, Row, ServerSources, SourceChoice, Value, mcp_presence, mcp_series_step,
};
use crucible_records::{
    CaptureSource, ErrorClass, Histogram, McpSide, TableId, TimeWindow, Timestamp,
};

use crate::context::{Meta, QueryServiceError};
use crate::decode::Fields;
use crate::service::Read;

/// The parameters every MCP analytics read takes (contract 9.9).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpQuery {
    /// The window.
    pub window: TimeWindow,
    /// The side read: client or server.
    pub side: McpSide,
    /// `auto` or one capture-source class.
    pub source: SourceChoice,
    /// Whether caller-fault calls count in the measures.
    pub include_caller_faults: bool,
    /// Only this server.
    pub server: Option<String>,
}

/// The latency percentiles of a set of calls, in microseconds, from the histogram of contract 6.5; `None`
/// without calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Latency {
    /// The median.
    pub p50: Option<u64>,
    /// The 90th percentile.
    pub p90: Option<u64>,
    /// The 99th percentile.
    pub p99: Option<u64>,
}

impl Latency {
    pub(crate) fn of(histogram: &Histogram) -> Self {
        Self {
            p50: histogram.quantile(500_000),
            p90: histogram.quantile(900_000),
            p99: histogram.quantile(990_000),
        }
    }
}

/// The measures of a set of calls.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct McpTotals {
    /// Calls, without the caller-fault calls unless the request includes them.
    pub calls: u64,
    /// Calls with an error class other than `none`, under the same rule.
    pub errors: u64,
    /// Calls the caller caused (contract 9.2), counted whether or not they are included.
    pub caller_faults: u64,
    /// Latency percentiles of the calls counted.
    pub latency: Latency,
    /// Request bytes of the calls counted, where the read has them.
    pub request_bytes: u64,
    /// Response bytes of the calls counted, where the read has them.
    pub response_bytes: u64,
    /// When the last call was seen, of any kind.
    pub last_seen: Option<Timestamp>,
}

/// The rollup measures of one group while groups are folded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Tally {
    calls: u64,
    errors: u64,
    caller_faults: u64,
    histogram: Histogram,
    request_bytes: u64,
    response_bytes: u64,
    last_seen: Option<Timestamp>,
}

impl Default for Tally {
    fn default() -> Self {
        Self {
            calls: 0,
            errors: 0,
            caller_faults: 0,
            histogram: Histogram::new(),
            request_bytes: 0,
            response_bytes: 0,
            last_seen: None,
        }
    }
}

/// One rollup group: its error class, whether its calls are caller faults, and its measures.
#[derive(Clone, Debug)]
pub(crate) struct Group {
    pub(crate) error_class: Option<ErrorClass>,
    pub(crate) caller_fault: bool,
    pub(crate) count: u64,
    pub(crate) histogram: Histogram,
    pub(crate) request_bytes: u64,
    pub(crate) response_bytes: u64,
    pub(crate) last_seen: Option<Timestamp>,
}

impl Tally {
    /// Adds a group under the caller-fault rule of the request.
    pub(crate) fn add(&mut self, group: &Group, include_caller_faults: bool) {
        if let Some(seen) = group.last_seen {
            self.last_seen = Some(self.last_seen.map_or(seen, |last| last.max(seen)));
        }
        if group.caller_fault {
            self.caller_faults = self.caller_faults.saturating_add(group.count);
            if !include_caller_faults {
                return;
            }
        }
        self.calls = self.calls.saturating_add(group.count);
        if group.error_class.is_some_and(|c| c != ErrorClass::None) {
            self.errors = self.errors.saturating_add(group.count);
        }
        self.histogram.merge(&group.histogram);
        self.request_bytes = self.request_bytes.saturating_add(group.request_bytes);
        self.response_bytes = self.response_bytes.saturating_add(group.response_bytes);
    }

    pub(crate) fn totals(&self) -> McpTotals {
        McpTotals {
            calls: self.calls,
            errors: self.errors,
            caller_faults: self.caller_faults,
            latency: Latency::of(&self.histogram),
            request_bytes: self.request_bytes,
            response_bytes: self.response_bytes,
            last_seen: self.last_seen,
        }
    }
}

fn bad(what: &str) -> QueryServiceError {
    QueryServiceError::Internal(format!(
        "the store returned an MCP row without a valid {what}"
    ))
}

/// The typed values of the MCP rows, beyond those `Fields` reads.
impl Fields<'_> {
    pub(crate) fn histogram(&self, index: usize) -> Result<Histogram, QueryServiceError> {
        let Some(Value::Tuple2(keys, counts)) = self.0.get(index) else {
            return Err(bad("histogram"));
        };
        let (Value::Array(keys), Value::Array(counts)) = (&**keys, &**counts) else {
            return Err(bad("histogram"));
        };
        let keys: Option<Vec<u16>> = keys
            .iter()
            .map(|k| match k {
                Value::UInt(k) => u16::try_from(*k).ok(),
                _ => None,
            })
            .collect();
        let counts: Option<Vec<u64>> = counts
            .iter()
            .map(|c| match c {
                Value::UInt(c) => Some(*c),
                _ => None,
            })
            .collect();
        keys.zip(counts)
            .and_then(|(keys, counts)| Histogram::from_sum_map(&keys, &counts))
            .ok_or_else(|| bad("histogram"))
    }

    pub(crate) fn error_class(&self, index: usize) -> Result<ErrorClass, QueryServiceError> {
        ErrorClass::from_value(self.enum8(index)?).ok_or_else(|| bad("error class"))
    }

    pub(crate) fn capture_source(&self, index: usize) -> Result<CaptureSource, QueryServiceError> {
        CaptureSource::from_value(self.enum8(index)?).ok_or_else(|| bad("capture source"))
    }

    /// A time that a zero-filled or empty group leaves null.
    pub(crate) fn opt_time(&self, index: usize) -> Result<Option<Timestamp>, QueryServiceError> {
        match self.0.get(index) {
            Some(Value::Null) => Ok(None),
            _ => self.time(index).map(Some),
        }
    }

    /// A `Nullable(Int32)` value.
    pub(crate) fn opt_int(&self, index: usize) -> Result<Option<i64>, QueryServiceError> {
        match self.0.get(index) {
            Some(Value::Null) => Ok(None),
            Some(Value::Int(v)) => Ok(Some(*v)),
            _ => Err(bad("integer")),
        }
    }

    /// A `FixedString(16)` value; all zero means absent.
    pub(crate) fn hash16(&self, index: usize) -> Result<Option<[u8; 16]>, QueryServiceError> {
        let bytes: [u8; 16] = self
            .bytes(index)?
            .try_into()
            .map_err(|_| bad("16-byte hash"))?;
        Ok((bytes != [0; 16]).then_some(bytes))
    }
}

/// A rollup group of the plans that group by `error_class` and `caller_fault` after `dims` dimensions, with
/// `sum(count)` first among the measures. The optional measures are read at the positions given.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupLayout {
    pub(crate) dims: usize,
    pub(crate) histogram: Option<usize>,
    pub(crate) bytes: Option<usize>,
    pub(crate) last_seen: Option<usize>,
}

impl GroupLayout {
    /// The group of a row; positions of the optional measures count from the first measure.
    pub(crate) fn group(&self, f: &Fields<'_>) -> Result<Group, QueryServiceError> {
        let measures = self.dims + 2;
        let at = |offset: usize| measures + offset;
        Ok(Group {
            error_class: Some(f.error_class(self.dims)?),
            caller_fault: f.uint(self.dims + 1)? != 0,
            count: f.uint(measures)?,
            histogram: match self.histogram {
                Some(offset) => f.histogram(at(offset))?,
                None => Histogram::new(),
            },
            request_bytes: match self.bytes {
                Some(offset) => f.uint(at(offset))?,
                None => 0,
            },
            response_bytes: match self.bytes {
                Some(offset) => f.uint(at(offset + 1))?,
                None => 0,
            },
            last_seen: match self.last_seen {
                Some(offset) => f.opt_time(at(offset))?,
                None => None,
            },
        })
    }
}

/// The capture source each server is read from, as a response names it.
pub type CaptureSources = Vec<ServerSources>;

impl Read<'_> {
    /// The selection of a read: the presence of each server's capture sources on the side in the window, and
    /// the class chosen for each (contract 9.5).
    pub(crate) async fn mcp_selection(
        &mut self,
        query: &McpQuery,
    ) -> Result<McpSelection, QueryServiceError> {
        let plan = mcp_presence(
            self.ctx.scope,
            query.window,
            &self.clamp,
            self.ctx.caller,
            query.side,
            query.server.as_deref(),
        );
        let rows = self.rows(plan).await?;
        let presence = rows
            .iter()
            .map(|row| {
                let f = Fields(row.values());
                Ok((f.text(0)?.to_string(), f.capture_source(1)?))
            })
            .collect::<Result<Vec<_>, QueryServiceError>>()?;
        Ok(McpSelection::select(query.source, presence))
    }

    /// The `meta` of an MCP read over `tables`, with the consistency taken when the read began. The recent
    /// index holds no MCP rows, so an unavailable store fails the read.
    pub(crate) fn mcp_meta(
        &self,
        consistency: crucible_recent::Consistency,
        tables: &[TableId],
    ) -> Result<Meta, QueryServiceError> {
        let state = self.service.index.read_state(self.ctx.scope);
        self.finish(self.now, consistency, state, tables, false)
    }
}

/// The selection of a read whose plans read each server's own capture sources (deploys): the same choice
/// over the classes those rows have.
pub(crate) fn select_among(
    choice: SourceChoice,
    rows: impl IntoIterator<Item = (Arc<str>, CaptureSource)>,
) -> McpSelection {
    McpSelection::select(
        choice,
        rows.into_iter()
            .map(|(server, source)| (server.to_string(), source)),
    )
}

/// The buckets of an MCP time series over `window`: the step and the first instant of each bucket.
pub(crate) fn series_buckets(window: TimeWindow) -> (crucible_plan::Step, Vec<Timestamp>) {
    let step = mcp_series_step(window);
    let (first, count) = step.buckets(window);
    let width = step.seconds().saturating_mul(1_000_000);
    let buckets = (0..count)
        .map(|i| {
            let offset = i64::try_from(i).unwrap_or(i64::MAX).saturating_mul(width);
            first.saturating_add_micros(offset)
        })
        .collect();
    (step, buckets)
}

/// The rows of a plan, read by position.
pub(crate) fn fields(row: &Row) -> Fields<'_> {
    Fields(row.values())
}
