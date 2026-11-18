//! A test double of [`SpanMapper`] (contract 10.2 allows them). It reads `t.*` span attributes straight into the
//! draft, so the stage A rules that run after mapping (forward-fill, defaults, cost, caps) can be tested on fields
//! that dialect 0's table has no source for.
//!
//! - `t.trace_name`, `t.user_id`, `t.session_id`, `t.environment`, `t.release`, `t.version`: strings.
//! - `t.trace_tags`: an array of strings. `t.trace_metadata`: a string body. `t.trace_public`: a bool.
//! - `t.level`: a level name. `t.usage.<key>`: an integer. `t.cost.<key>`: decimal USD text or a double.
//!   `t.cost_nanos.<key>`: a nano-USD integer, taken as is (negative values included).
//! - `t.model_requested`, `t.model_response`: strings. `t.model_parameters`: a string body.

use bytes::Bytes;
use crucible_compat::Confidence;
use crucible_normalize::{Hints, MapCtx, SpanDraft, SpanMapper};
use crucible_otlp::{AnyValueView, ResourceView, ScopeView, SpanView};
use crucible_records::{Level, NanoUsd, TracePublic};

#[derive(Debug, Clone, Copy)]
pub struct Double {
    /// The dialect id the double claims.
    pub id: u8,
    /// What `detect` returns.
    pub confidence: Option<u8>,
}

impl Double {
    /// A double for dialect 0 that detects nothing.
    pub const NATIVE: Self = Self {
        id: 0,
        confidence: None,
    };
}

impl SpanMapper for Double {
    fn dialect(&self) -> u8 {
        self.id
    }

    fn detect(
        &self,
        _resource: ResourceView<'_>,
        _scope: ScopeView<'_>,
        _span: &SpanView<'_>,
        _hints: &Hints,
    ) -> Option<Confidence> {
        self.confidence.map(Confidence)
    }

    fn map(&self, _ctx: &MapCtx<'_>, span: &SpanView<'_>, out: &mut SpanDraft) {
        // The double marks every span it maps, so tests can see which mapper ran.
        out.record.core.prompt_version = format!("double {}", self.id).into();
        for (key, value) in span.attributes() {
            let (core, payload) = (&mut out.record.core, &mut out.record.payload);
            match (key, value) {
                ("t.trace_name", AnyValueView::String(s)) => core.trace_name = s.into(),
                ("t.user_id", AnyValueView::String(s)) => core.user_id = s.into(),
                ("t.session_id", AnyValueView::String(s)) => core.session_id = s.into(),
                ("t.environment", AnyValueView::String(s)) => core.environment = s.into(),
                ("t.release", AnyValueView::String(s)) => core.release = s.into(),
                ("t.version", AnyValueView::String(s)) => core.version = s.into(),
                ("t.trace_tags", AnyValueView::Array(items)) => {
                    core.trace_tags = items
                        .filter_map(|v| match v {
                            AnyValueView::String(s) => Some(s.into()),
                            _ => None,
                        })
                        .collect();
                }
                ("t.trace_metadata", AnyValueView::String(s)) => {
                    payload.trace_metadata = Bytes::copy_from_slice(s.as_bytes());
                }
                ("t.trace_public", AnyValueView::Bool(b)) => {
                    core.trace_public = if b {
                        TracePublic::Public
                    } else {
                        TracePublic::Private
                    };
                }
                ("t.level", AnyValueView::String(s)) => out.level = Level::parse(s),
                ("t.model_requested", AnyValueView::String(s)) => core.model_requested = s.into(),
                ("t.model_response", AnyValueView::String(s)) => core.model_response = s.into(),
                ("t.model_parameters", AnyValueView::String(s)) => {
                    payload.model_parameters = Bytes::copy_from_slice(s.as_bytes());
                }
                (k, AnyValueView::Int(i)) if k.starts_with("t.usage.") => {
                    let units = u64::try_from(i).unwrap_or(0);
                    payload
                        .usage_provided
                        .push((k["t.usage.".len()..].into(), units));
                }
                (k, AnyValueView::String(s)) if k.starts_with("t.cost.") => {
                    let cost = NanoUsd::from_usd_decimal(s).unwrap_or(NanoUsd::ZERO);
                    payload
                        .cost_provided
                        .push((k["t.cost.".len()..].into(), cost));
                }
                (k, AnyValueView::Double(d)) if k.starts_with("t.cost.") => {
                    let cost = NanoUsd::from_usd_f64(d);
                    payload
                        .cost_provided
                        .push((k["t.cost.".len()..].into(), cost));
                }
                (k, AnyValueView::Int(i)) if k.starts_with("t.cost_nanos.") => {
                    let cost = NanoUsd::from_nanos(i);
                    payload
                        .cost_provided
                        .push((k["t.cost_nanos.".len()..].into(), cost));
                }
                _ => {}
            }
        }
    }
}
