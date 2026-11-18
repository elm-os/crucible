//! Normalization never panics on a body the scanner accepts, fails cleanly on one it refuses, and the records it
//! produces keep their invariants. Doubles include NaN and every bit pattern; caps are tiny so every cut path runs.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::{frame, generate, prices};
use crucible_normalize::{NormCtx, NormalizedFrame, normalize, row_digest};
use crucible_otlp::TraceRequest;
use crucible_records::{CostSource, Lsn, McpSide, NanoUsd, PayloadCaps, PayloadField, caps};
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message;

fn check(out: &NormalizedFrame, ctx: &NormCtx) -> Result<(), TestCaseError> {
    let summary = out.summary;
    prop_assert_eq!(
        out.spans.len() as u32 + summary.rejected_before_ack + summary.missing_ids,
        summary.spans
    );
    for r in &out.spans {
        let c = &r.core;
        prop_assert!(c.start_time.is_valid() && c.end_time.is_valid());
        prop_assert!(c.start_time <= c.end_time);
        prop_assert!(!c.trace_id.is_zero() && !c.span_id.is_zero());
        // A span without a parent is a root; a dialect's root flag makes one of a span that names a parent.
        prop_assert!(c.is_root || !c.parent_span_id.is_zero());
        for (value, cap) in [
            (&c.name, caps::SPAN_NAME_BYTES),
            (&c.status_message, caps::STATUS_MESSAGE_BYTES),
            (&c.service_name, caps::SERVICE_BYTES),
            (&c.scope_name, caps::SERVICE_BYTES),
            (&c.telemetry_sdk_name, caps::SDK_BYTES),
            (&c.user_id, caps::TRACE_ATTRIBUTE_BYTES),
            (&c.model_requested, caps::MODEL_BYTES),
            (&c.provider, caps::MODEL_BYTES),
            (&c.prompt_name, caps::PROMPT_NAME_BYTES),
            (&c.input_preview, caps::PREVIEW_BYTES),
            (&c.output_preview, caps::PREVIEW_BYTES),
        ] {
            prop_assert!(value.len() <= cap);
        }
        prop_assert!(c.finish_reasons.len() <= caps::FINISH_REASONS_MAX_ITEMS);
        prop_assert!(
            c.finish_reasons
                .iter()
                .all(|f| f.len() <= caps::FINISH_REASON_BYTES)
        );
        prop_assert!(c.usage.len() <= caps::USAGE_MAX_KEYS);
        let p = &r.payload;
        prop_assert!(c.total_cost_nusd >= NanoUsd::ZERO);
        prop_assert_eq!(
            c.cost_source == CostSource::Provided,
            !p.cost_provided.is_empty()
        );
        let priced = c.price_model_id.is_some() && c.price_tier_id.is_some();
        let unpriced = c.price_model_id.is_none() && c.price_tier_id.is_none();
        match c.cost_source {
            CostSource::Calculated => {
                prop_assert!(priced);
                prop_assert_eq!(
                    c.total_cost_nusd,
                    NanoUsd::saturating_sum(c.usage.iter().map(|e| e.cost))
                );
            }
            CostSource::None => {
                prop_assert!(unpriced);
                prop_assert_eq!(c.total_cost_nusd, NanoUsd::ZERO);
                prop_assert!(c.usage.iter().all(|e| e.cost == NanoUsd::ZERO));
            }
            CostSource::Provided => prop_assert!(unpriced),
        }
        for field in PayloadField::ALL {
            let stored = p.field(field);
            prop_assert!(stored.len() <= ctx.caps.field_cap(field));
            prop_assert!(
                core::str::from_utf8(stored).is_ok(),
                "{:?} cut inside a character",
                field
            );
        }
        let order_empty = PayloadField::SPAN_CUT_ORDER
            .iter()
            .all(|&f| p.field(f).is_empty());
        prop_assert!(p.stored_bytes() <= ctx.caps.max_span_bytes || order_empty);
        // The JSON the normalizer writes, and the bodies it takes only as JSON, are valid JSON unless a cap cut
        // them.
        for field in [
            PayloadField::Attributes,
            PayloadField::ResourceAttributes,
            PayloadField::Events,
            PayloadField::Links,
            PayloadField::SystemInstructions,
            PayloadField::ToolDefinitions,
        ] {
            let stored = p.field(field);
            if !stored.is_empty() && !p.truncated_fields.contains(field) {
                prop_assert!(
                    serde_json::from_slice::<serde_json::Value>(stored).is_ok(),
                    "{:?}",
                    field
                );
            }
        }
        prop_assert_eq!(c.row_digest, row_digest(r));
        prop_assert_eq!(c.ingestion.batch_seq, Lsn::ZERO);
        prop_assert!(!c.environment.is_empty());
        prop_assert_eq!(c.mcp_side, McpSide::None);
        prop_assert!(usize::from(c.compat_dialect) < crucible_compat::TABLES.len());
        prop_assert_eq!(
            c.source_format == crucible_records::SourceFormat::Otel,
            c.compat_dialect == 0
        );
        // Only stage B flags a usage aggregate.
        prop_assert!(!c.usage_aggregate);
    }
    Ok(())
}

fn tiny(max_field_bytes: usize, max_span_bytes: usize) -> NormCtx<'static> {
    NormCtx::new(
        prices(),
        PayloadCaps {
            max_field_bytes,
            max_span_bytes,
        },
    )
}

/// Normalizes `body` if the scanner accepts it, else expects a clean error.
fn exercise(body: &[u8], rejected: &[u8], hint: u8, ctx: &NormCtx) -> Result<(), TestCaseError> {
    let mut f = frame(body, rejected);
    f.compat_dialect = hint;
    match TraceRequest::scan(body, None) {
        Ok(_) => check(&normalize(&f, ctx).unwrap(), ctx),
        Err(_) => {
            prop_assert!(normalize(&f, ctx).is_err());
            Ok(())
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    #[test]
    fn random_requests(
        request in generate::request(false),
        rejected in vec(any::<u8>(), 0..3),
        hint in any::<u8>(),
        max_field in 0usize..64,
        max_span in 0usize..256,
    ) {
        exercise(&request.encode_to_vec(), &rejected, hint, &tiny(max_field, max_span))?;
    }

    /// Bodies with bytes overwritten, inserted or removed: whatever still scans normalizes.
    #[test]
    fn mutated_requests(
        request in generate::request(false),
        edits in vec((any::<prop::sample::Index>(), any::<u8>(), 0u8..3), 1..6),
        max_field in 0usize..64,
    ) {
        let mut body = request.encode_to_vec();
        for (at, byte, op) in edits {
            match op {
                0 if !body.is_empty() => {
                    let i = at.index(body.len());
                    body[i] = byte;
                }
                1 => body.insert(at.index(body.len() + 1), byte),
                2 if !body.is_empty() => {
                    body.remove(at.index(body.len()));
                }
                _ => {}
            }
        }
        exercise(&body, &[], 0, &tiny(max_field, 4 * max_field))?;
    }

    #[test]
    fn arbitrary_bytes(body in vec(any::<u8>(), 0..512)) {
        exercise(&body, &[], 0, &tiny(16, 64))?;
    }
}
