//! Data bounds (contract 8.7 and 8.8): the first and last minute with data.

use crucible_plan::data_bounds;
use crucible_records::{McpSide, TableId, Timestamp};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::decode::Fields;
use crate::service::{QueryService, all_time};

/// The tables data bounds depend on, for the quarantine rule.
const BOUNDS_TABLES: [TableId; 2] = [TableId::SpanMetrics, TableId::McpToolRollup];

/// The first and last minute with data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinuteRange {
    /// The first minute.
    pub first: Timestamp,
    /// The last minute.
    pub last: Timestamp,
}

impl MinuteRange {
    fn union(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(a), Some(b)) => Some(Self {
                first: a.first.min(b.first),
                last: a.last.max(b.last),
            }),
            (a, b) => a.or(b),
        }
    }
}

/// Where a project's data begins and ends. A range is `None` when the project has no data of that kind inside
/// its retention.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DataBounds {
    /// Spans: the span rollup merged with the recent index.
    pub spans: Option<MinuteRange>,
    /// MCP operations captured on the client side.
    pub mcp_client: Option<MinuteRange>,
    /// MCP operations captured on the server side.
    pub mcp_server: Option<MinuteRange>,
    /// The first instant that has not expired, or `None` when the project keeps its data forever.
    pub retention_horizon: Option<Timestamp>,
    /// The response metadata.
    pub meta: Meta,
}

impl QueryService {
    /// Data bounds: `min` and `max` of the minute rollups, merged with the recent index, whose spans the
    /// rollups do not hold yet. Both sources are cut at the retention horizon.
    pub async fn data_bounds(&self, ctx: &ReadContext) -> Result<DataBounds, QueryServiceError> {
        let mut read = self.read(ctx);
        let consistency = self.consistency();
        let horizon = read.cutoff(TableId::SpanCore)?;
        let (recent, state) = self.index.time_bounds(ctx.scope);
        let recent = recent
            .filter(|(_, last)| horizon.is_none_or(|horizon| *last >= horizon))
            .map(|(first, last)| MinuteRange {
                first: horizon
                    .map_or(first, |horizon| first.max(horizon))
                    .floor_minute(),
                last: last.floor_minute(),
            });

        let plans = data_bounds(ctx.scope, all_time()?, &read.clamp, ctx.caller)?;
        let range = |f: &Fields<'_>, at: usize| -> Result<MinuteRange, QueryServiceError> {
            Ok(MinuteRange {
                first: f.time(at)?,
                last: f.time(at + 1)?,
            })
        };
        let mut spans = None;
        if let Some(row) = read.rows(Ok(plans.spans)).await?.first() {
            spans = Some(range(&Fields(row.values()), 0)?);
        }
        let (mut mcp_client, mut mcp_server) = (None, None);
        for row in read.rows(Ok(plans.mcp)).await? {
            let f = Fields(row.values());
            match McpSide::from_value(f.enum8(0)?) {
                Some(McpSide::Client) => mcp_client = Some(range(&f, 1)?),
                Some(McpSide::Server) => mcp_server = Some(range(&f, 1)?),
                _ => {}
            }
        }
        let meta = read.finish(
            read.now,
            consistency,
            state,
            &BOUNDS_TABLES,
            recent.is_some(),
        )?;
        Ok(DataBounds {
            spans: MinuteRange::union(spans, recent),
            mcp_client,
            mcp_server,
            retention_horizon: horizon,
            meta,
        })
    }
}
