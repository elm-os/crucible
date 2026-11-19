//! The records the real normalizer makes of a case's exports (`docs/testing/sdk-conformance.md`, "Records").
//!
//! Every accepted export goes through `crucible-normalize`, stage A and stage B, as a frame of the shipper would.
//! The driver then checks the derived fields and the classification of a golden against the [`McpOpRecord`] and
//! the span record of each span, so that a golden passes only when the normalizer makes of the SDK's spans what
//! the contract says (contracts 9.1 and 9.2).

use std::collections::BTreeMap;

use crucible_normalize::{
    IdentityCtx, IngestFrame, NormCtx, PriceTable, ProjectSalt, StageB, normalize,
};
use crucible_records::{
    ApiKeyId, Lsn, McpOpRecord, PayloadCaps, ProjectId, SpanCore, SpanId, Timestamp, TraceId,
    WalEpoch,
};
use serde_json::Value as Json;

use crate::collector::{Event, RecordedSpan};

/// The salt of the suite's project: any fixed bytes, so that a subject an SDK sends is hashed as in a server.
const SALT: ProjectSalt = ProjectSalt::new([0xc0; 32]);

/// The records of one case, by span key.
#[derive(Debug, Default)]
pub(crate) struct Records {
    ops: BTreeMap<(TraceId, SpanId), McpOpRecord>,
    spans: BTreeMap<(TraceId, SpanId), SpanCore>,
    /// Exports the normalizer refused, with the reason.
    pub(crate) errors: Vec<String>,
}

impl Records {
    /// Normalizes every accepted export of a case's log.
    pub(crate) fn of(events: &[Event]) -> Self {
        let mut out = Self::default();
        let prices = match PriceTable::bundled() {
            Ok(prices) => prices,
            Err(e) => {
                out.errors.push(e.to_string());
                return out;
            }
        };
        let ctx = NormCtx::new(prices, PayloadCaps::DEFAULT);
        let exports = events.iter().filter_map(|event| match event {
            Event::Export(export) if export.refusal.is_none() => Some(export),
            _ => None,
        });
        for (at, export) in exports.enumerate() {
            let received_at = export
                .spans
                .iter()
                .map(|span| span.end)
                .max()
                .unwrap_or(Timestamp::EPOCH);
            let frame = IngestFrame {
                wal_epoch: WalEpoch::from_bytes([0xe0; 16]),
                lsn: Lsn::new(at as u64 + 1),
                received_at,
                project_id: ProjectId::from_bytes([0x01; 16]),
                api_key_id: ApiKeyId::from_bytes([0x02; 16]),
                compat_dialect: 0,
                sdk_name: "",
                sdk_version: "",
                rejected: &[],
                otlp: &export.body,
            };
            let mut normalized = match normalize(&frame, &ctx) {
                Ok(normalized) => normalized,
                Err(e) => {
                    out.errors.push(e.to_string());
                    continue;
                }
            };
            // Each export by itself: a span that an SDK emits twice is the binding's to report.
            if let Err(e) =
                StageB::default().admit(&mut normalized, &IdentityCtx::Salted(Some(&SALT)))
            {
                out.errors.push(e.to_string());
                continue;
            }
            for op in normalized.mcp_ops {
                let record = op.record;
                out.ops.insert((record.trace_id, record.span_id), record);
            }
            for span in normalized.spans {
                let core = span.core;
                out.spans.insert((core.trace_id, core.span_id), core);
            }
        }
        out
    }

    /// The MCP operation the normalizer made of `span`.
    pub(crate) fn op(&self, span: &RecordedSpan) -> Option<&McpOpRecord> {
        self.ops.get(&(span.trace_id, span.span_id))
    }

    /// The value of a derived field of the registry in the records of `span`: a JSON string or boolean, or
    /// `None` when the records have no such field or hold no value for it. The outer `None` is for a field
    /// that is no record field.
    pub(crate) fn field(&self, field: &str, span: &RecordedSpan) -> Option<Option<Json>> {
        let key = (span.trace_id, span.span_id);
        let text = |s: &str| (!s.is_empty()).then(|| Json::from(s));
        let of_op =
            |read: fn(&McpOpRecord) -> Option<Json>| Some(self.ops.get(&key).and_then(read));
        let of_span = |read: fn(&SpanCore) -> &str| {
            Some(self.spans.get(&key).and_then(|core| text(read(core))))
        };
        match field {
            "side" => of_op(|op| Some(Json::from(op.side.as_str()))),
            "capture_source" => of_op(|op| Some(Json::from(op.capture_source.as_str()))),
            "era" => of_op(|op| Some(Json::from(op.era.as_str()))),
            "transport" => of_op(|op| Some(Json::from(op.transport.as_str()))),
            "direction" => of_op(|op| Some(Json::from(op.direction.as_str()))),
            "is_notification" => of_op(|op| Some(Json::from(op.is_notification))),
            "identity_source" => of_op(|op| Some(Json::from(op.identity_source.as_str()))),
            "server_name" => {
                of_op(|op| (!op.server_name.is_empty()).then(|| Json::from(&*op.server_name)))
            }
            "server_version" => {
                of_op(|op| (!op.server_version.is_empty()).then(|| Json::from(&*op.server_version)))
            }
            "deploy_id" => {
                of_op(|op| (!op.deploy_id.is_empty()).then(|| Json::from(&*op.deploy_id)))
            }
            "service_name" => of_span(|core| &core.service_name),
            "observation_type" => Some(
                self.spans
                    .get(&key)
                    .map(|core| Json::from(core.observation_type.as_str())),
            ),
            "session_id" => of_span(|core| &core.session_id),
            "user_id" => of_span(|core| &core.user_id),
            "environment" => of_span(|core| &core.environment),
            "release" => of_span(|core| &core.release),
            _ => None,
        }
    }
}
