//! The payload viewer (contract 8.7 and 12.6): a byte range of one payload field of one span, with the field's
//! length. The store answers through `FieldRange`; a span it does not serve yet is re-read from its WAL frame
//! through the recent index (contract 8.6), so a payload is readable as soon as its span is. Also the bodies a
//! span's media references are found in (contract 14): its input, output and metadata, read the same way.

use crucible_env::QueryError;
use crucible_plan::{
    HiddenSelector, PayloadField, PayloadRangeRequest, Value, payload_range, span_bodies,
};
use crucible_records::{IoShape, SpanId, TableId, Timestamp, TraceId};
use crucible_schema::{Column, SpanCoreCol};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::decode::Fields;
use crate::service::QueryService;
use crate::spans::{SpanSource, TraceSpan};

/// The tables a payload read depends on, for the quarantine rule.
const PAYLOAD_TABLES: [TableId; 1] = [TableId::SpanPayload];

/// The length that reads a field to its end: no field is this long.
const WHOLE: u64 = 1 << 40;

/// A request for a byte range of one payload field of one span.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PayloadQuery {
    /// The span's trace.
    pub trace_id: TraceId,
    /// The span.
    pub span_id: SpanId,
    /// The span's `start_time`, which locates its payload row.
    pub start_time: Timestamp,
    /// The field.
    pub field: PayloadField,
    /// The first byte and the most bytes to read, at least one; `None` reads the whole field.
    pub range: Option<(u64, u64)>,
}

/// A byte range of one payload field.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PayloadBytes {
    /// The bytes of the range, cut at the end of the field: empty for a range that starts past it.
    pub bytes: Vec<u8>,
    /// The length of the whole field in bytes.
    pub total: u64,
    /// The response metadata.
    pub meta: Meta,
}

impl TraceSpan {
    /// How the span's input and output are structured, which tells the payload viewer how to show them. The
    /// recent index does not keep it, so a span the store does not serve yet has none.
    pub fn io_shape(&self) -> Option<IoShape> {
        let SpanSource::Store(row) = &self.source else {
            return None;
        };
        let at = SpanCoreCol::ALL
            .iter()
            .position(|c| *c == SpanCoreCol::IoShape)?;
        match row.values().get(at)? {
            Value::Enum(value) => IoShape::from_value(*value),
            _ => None,
        }
    }
}

/// The record field of a plan field: the same body.
const fn record_field(field: PayloadField) -> crucible_records::PayloadField {
    use crucible_records::PayloadField as R;
    match field {
        PayloadField::Input => R::Input,
        PayloadField::Output => R::Output,
        PayloadField::SystemInstructions => R::SystemInstructions,
        PayloadField::Attributes => R::Attributes,
        PayloadField::ResourceAttributes => R::ResourceAttributes,
        PayloadField::Events => R::Events,
        PayloadField::Links => R::Links,
        PayloadField::ToolDefinitions => R::ToolDefinitions,
        PayloadField::ToolCalls => R::ToolCalls,
    }
}

impl QueryService {
    /// A byte range of one payload field of one span (contract 8.7), from the store or, for a span the store
    /// does not serve yet, from its WAL frame. `not_found` only when the answer is whole and strong: no payload
    /// row (it expired under the payload retention, or was never stored), and nothing in the index. A miss that
    /// could be incomplete fails as `telemetry_store_unavailable`, because a byte response has no body to carry
    /// `meta`.
    pub async fn payload(
        &self,
        ctx: &ReadContext,
        query: PayloadQuery,
    ) -> Result<PayloadBytes, QueryServiceError> {
        let mut read = self.read(ctx, "payload");
        let scope = ctx.scope;
        let (offset, len) = query.range.unwrap_or((0, WHOLE));
        if len == 0 {
            return Err(QueryServiceError::Validation("range"));
        }
        let consistency = self.consistency();
        // A payload is read by its key, which no predicate can narrow: a trace under a pending deletion has
        // no payload to give (contract 13.4).
        let deleted = read.hidden.iter().any(|hidden| {
            matches!(&hidden.selector, HiddenSelector::Traces(ids) if ids.contains(&query.trace_id))
        });
        if deleted {
            return Err(QueryServiceError::NotFound);
        }
        let request = PayloadRangeRequest {
            trace_id: query.trace_id,
            span_id: query.span_id,
            start_time: query.start_time,
            field: query.field,
            offset,
            len,
        };
        let plan = payload_range(scope, &read.clamp, ctx.caller, request);
        if let Some(row) = read.rows(plan).await?.first() {
            let f = Fields(row.values());
            let (bytes, total) = (f.bytes(0)?.to_vec(), f.uint(1)?);
            let state = self.index.read_state(scope);
            let meta = read.finish(read.now, consistency, state, &PAYLOAD_TABLES, false)?;
            return Ok(PayloadBytes { bytes, total, meta });
        }

        let retained = read
            .cutoff(TableId::SpanPayload)?
            .is_none_or(|cutoff| query.start_time >= cutoff);
        // A failed re-read is a span the index cannot give: its frame is gone because the store serves it.
        let record = if retained {
            self.index
                .span_record(scope, query.trace_id, query.span_id)
                .await
                .ok()
                .flatten()
                .filter(|record| record.core.start_time == query.start_time)
                .filter(|record| {
                    let core = &record.core;
                    !read.hidden.iter().any(|hidden| {
                        hidden.hides(
                            core.trace_id,
                            &core.user_id,
                            &core.session_id,
                            core.ingestion.received_at,
                        )
                    })
                })
        } else {
            None
        };
        let state = self.index.read_state(scope);
        if let Some(record) = record {
            let field = record.payload.field(record_field(query.field));
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(field.len());
            let end = start
                .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
                .min(field.len());
            let meta = read.finish(read.now, consistency, state, &PAYLOAD_TABLES, true)?;
            return Ok(PayloadBytes {
                bytes: field[start..end].to_vec(),
                total: field.len() as u64,
                meta,
            });
        }
        let meta = read.finish(read.now, consistency, state, &PAYLOAD_TABLES, false)?;
        if meta.is_whole() {
            Err(QueryServiceError::NotFound)
        } else {
            Err(QueryError::Unavailable(
                "the payload may not be readable yet: the read is partial or lagging".to_owned(),
            )
            .into())
        }
    }
}

/// The bodies of one span that can reference media (contract 14): its input and output as stored, and its
/// metadata as key and value pairs, each value as stored (JSON text for a value that is not a string).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SpanBodies {
    /// The input body.
    pub input: Vec<u8>,
    /// The output body.
    pub output: Vec<u8>,
    /// The metadata's keys and values, in order.
    pub metadata: Vec<(String, String)>,
}

impl QueryService {
    /// The bodies of one span that can reference media, from the store or, for a span the store does not serve
    /// yet, from its WAL frame. `not_found` only when the answer is whole and strong, as for [`Self::payload`].
    pub async fn span_bodies(
        &self,
        ctx: &ReadContext,
        trace_id: TraceId,
        span_id: SpanId,
        start_time: Timestamp,
    ) -> Result<SpanBodies, QueryServiceError> {
        let mut read = self.read(ctx, "span_bodies");
        let scope = ctx.scope;
        let consistency = self.consistency();
        let deleted = read.hidden.iter().any(|hidden| {
            matches!(&hidden.selector, HiddenSelector::Traces(ids) if ids.contains(&trace_id))
        });
        if deleted {
            return Err(QueryServiceError::NotFound);
        }
        let plan = span_bodies(
            scope,
            &read.clamp,
            ctx.caller,
            trace_id,
            span_id,
            start_time,
        );
        // A span sent twice has a row per copy: the newest one is the span.
        let mut newest: Option<(u64, SpanBodies)> = None;
        for row in read.rows(plan).await? {
            let f = Fields(row.values());
            let lsn = f.uint(4)?;
            if newest.as_ref().is_some_and(|(held, _)| *held >= lsn) {
                continue;
            }
            let (keys, values) = (f.texts(2)?, f.texts(3)?);
            if keys.len() != values.len() {
                return Err(QueryServiceError::Internal(
                    "the store returned metadata keys and values of different lengths".to_owned(),
                ));
            }
            let metadata = keys
                .iter()
                .zip(&values)
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            let bodies = SpanBodies {
                input: f.bytes(0)?.to_vec(),
                output: f.bytes(1)?.to_vec(),
                metadata,
            };
            newest = Some((lsn, bodies));
        }
        if let Some((_, bodies)) = newest {
            let state = self.index.read_state(scope);
            read.finish(read.now, consistency, state, &PAYLOAD_TABLES, false)?;
            return Ok(bodies);
        }

        let retained = read
            .cutoff(TableId::SpanPayload)?
            .is_none_or(|cutoff| start_time >= cutoff);
        // A failed re-read is a span the index cannot give: its frame is gone because the store serves it.
        let record = if retained {
            self.index
                .span_record(scope, trace_id, span_id)
                .await
                .ok()
                .flatten()
                .filter(|record| record.core.start_time == start_time)
                .filter(|record| {
                    let core = &record.core;
                    !read.hidden.iter().any(|hidden| {
                        hidden.hides(
                            core.trace_id,
                            &core.user_id,
                            &core.session_id,
                            core.ingestion.received_at,
                        )
                    })
                })
        } else {
            None
        };
        let state = self.index.read_state(scope);
        if let Some(record) = record {
            read.finish(read.now, consistency, state, &PAYLOAD_TABLES, true)?;
            let payload = &record.payload;
            return Ok(SpanBodies {
                input: payload.input.to_vec(),
                output: payload.output.to_vec(),
                metadata: payload
                    .metadata_keys
                    .iter()
                    .zip(&payload.metadata_values)
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
            });
        }
        let meta = read.finish(read.now, consistency, state, &PAYLOAD_TABLES, false)?;
        if meta.is_whole() {
            Err(QueryServiceError::NotFound)
        } else {
            Err(QueryError::Unavailable(
                "the span may not be readable yet: the read is partial or lagging".to_owned(),
            )
            .into())
        }
    }
}
