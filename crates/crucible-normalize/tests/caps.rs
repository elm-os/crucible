//! Caps and truncation markers (contract 3.1) as stage A applies them: string caps on the core tier at mapping
//! time, item caps on lists, and the payload caps of the project snapshot after mapping and forward-fill.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use common::double::Double;
use common::{frame, int, prices, request, request_with, s, span, span_with, strings};
use crucible_normalize::{NormCtx, normalize, normalize_with};
use crucible_records::{PayloadCaps, PayloadField, Truncation, caps, truncate_str};
use opentelemetry_proto::tonic::trace::v1::span::{Event, Link};
use prost::Message;
use sha2::{Digest, Sha256};

fn capped(max_field_bytes: usize, max_span_bytes: usize) -> NormCtx<'static> {
    NormCtx::new(
        prices(),
        PayloadCaps {
            max_field_bytes,
            max_span_bytes,
        },
    )
}

fn names(fields: crucible_records::PayloadFields) -> Vec<&'static str> {
    fields.iter().map(PayloadField::name).collect()
}

#[test]
fn a_cut_body_keeps_its_size_hash_and_preview() {
    // 3-byte characters, so a cut at 10 bytes must back off to 9.
    let input = "\u{4e2d}".repeat(500);
    let body = request(vec![span_with(
        1,
        1,
        None,
        vec![s("gen_ai.input.messages", &input)],
    )]);
    let r = &normalize(&frame(&body, &[]), &capped(10, 1 << 20))
        .unwrap()
        .spans[0];
    assert_eq!(&r.payload.input[..], "\u{4e2d}".repeat(3).as_bytes());
    assert_eq!(r.core.input_bytes, input.len() as u64);
    assert_eq!(r.core.input_truncation, Truncation::CapExceeded);
    assert_eq!(
        r.core.input_sha256,
        Some(Sha256::digest(input.as_bytes()).into())
    );
    // The preview comes from the original, cut at 1,024 bytes on a character boundary.
    assert_eq!(
        &*r.core.input_preview,
        truncate_str(&input, caps::PREVIEW_BYTES)
    );
    assert_eq!(r.core.output_truncation, Truncation::None);
    assert_eq!(names(r.payload.truncated_fields), ["input"]);
}

#[test]
fn the_span_cap_cuts_in_the_fixed_order() {
    let mut sp = span_with(
        1,
        1,
        None,
        vec![
            s("gen_ai.input.messages", &"i".repeat(40)),
            s("gen_ai.output.messages", &"o".repeat(40)),
            s("extra", &"a".repeat(40)),
        ],
    );
    sp.events = vec![Event {
        time_unix_nano: 1,
        name: "e".repeat(40),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
    }];
    // Stored: input 40, output 40, attributes 52, resource attributes 46, events 106.
    let resource = vec![s("service.name", "svc"), s("host.name", &"h".repeat(30))];
    let body = request_with(resource, vec![sp]).encode_to_vec();
    let r = &normalize(&frame(&body, &[]), &capped(1 << 20, 100))
        .unwrap()
        .spans[0];
    let p = &r.payload;
    assert!(p.stored_bytes() <= 100);
    // Events go first, then the residual resource attributes, then the residual span attributes.
    assert!(p.events.is_empty());
    assert_eq!(
        names(p.truncated_fields),
        ["attributes", "resource_attributes", "events"]
    );
    assert_eq!((p.input.len(), p.output.len()), (40, 40));
    // A cut JSON field is a raw prefix and no longer valid JSON; `truncated_fields` says so.
    assert!(!p.attributes.is_empty());
    assert!(serde_json::from_slice::<serde_json::Value>(&p.attributes).is_err());
}

#[test]
fn forward_filled_metadata_is_capped_per_span() {
    let metadata = format!(r#"{{"k":"{}"}}"#, "m".repeat(64));
    let body = request(vec![
        span_with(1, 1, None, vec![s("t.trace_metadata", &metadata)]),
        span(1, 2, Some(1)),
    ]);
    let out = normalize_with(&[&Double::NATIVE], &frame(&body, &[]), &capped(16, 1 << 20)).unwrap();
    for r in &out.spans {
        assert_eq!(&r.payload.trace_metadata[..], &metadata.as_bytes()[..16]);
        assert_eq!(names(r.payload.truncated_fields), ["trace_metadata"]);
    }
}

#[test]
fn core_strings_are_cut_on_character_boundaries() {
    let long = "\u{e9}".repeat(2_000);
    let mut sp = span_with(
        1,
        1,
        None,
        vec![
            s("user.id", &long),
            s("gen_ai.request.model", &long),
            s("gen_ai.prompt.name", &long),
        ],
    );
    sp.name = long.clone();
    sp.status = Some(opentelemetry_proto::tonic::trace::v1::Status {
        message: "x".repeat(10_000),
        code: 2,
    });
    let req = request_with(
        vec![s("service.name", &long), s("telemetry.sdk.name", &long)],
        vec![sp],
    );
    let body = req.encode_to_vec();
    let mut f = frame(&body, &[]);
    f.sdk_name = &long;
    let r = &normalize(&f, &NormCtx::new(prices(), PayloadCaps::DEFAULT))
        .unwrap()
        .spans[0];
    let c = &r.core;
    for (value, cap) in [
        (&c.name, caps::SPAN_NAME_BYTES),
        (&c.user_id, caps::TRACE_ATTRIBUTE_BYTES),
        (&c.model_requested, caps::MODEL_BYTES),
        (&c.prompt_name, caps::PROMPT_NAME_BYTES),
        (&c.service_name, caps::SERVICE_BYTES),
        (&c.telemetry_sdk_name, caps::SDK_BYTES),
        (&c.sdk_name, caps::SDK_BYTES),
    ] {
        assert_eq!(&**value, truncate_str(&long, cap));
    }
    assert_eq!(c.status_message.len(), caps::STATUS_MESSAGE_BYTES);
}

#[test]
fn lists_keep_their_first_items() {
    let reasons: Vec<String> = (0..20)
        .map(|i| format!("{i:02}{}", "r".repeat(100)))
        .collect();
    let refs: Vec<&str> = reasons.iter().map(String::as_str).collect();
    let mut sp = span_with(
        1,
        1,
        None,
        vec![strings("gen_ai.response.finish_reasons", &refs)],
    );
    sp.events = (0..200)
        .map(|i| Event {
            time_unix_nano: i,
            name: format!("e{i}"),
            attributes: vec![int("i", i64::try_from(i).unwrap())],
            dropped_attributes_count: 0,
        })
        .collect();
    sp.links = (0..40)
        .map(|i| Link {
            trace_id: vec![i; 16],
            span_id: vec![i; 8],
            ..Link::default()
        })
        .collect();
    let body = request(vec![sp]);
    let r = &normalize(
        &frame(&body, &[]),
        &NormCtx::new(prices(), PayloadCaps::DEFAULT),
    )
    .unwrap()
    .spans[0];
    let c = &r.core;
    assert_eq!(c.finish_reasons.len(), caps::FINISH_REASONS_MAX_ITEMS);
    assert!(
        c.finish_reasons
            .iter()
            .all(|f| f.len() == caps::FINISH_REASON_BYTES)
    );
    assert!(c.finish_reasons[0].starts_with("00") && c.finish_reasons[15].starts_with("15"));
    let events: Vec<serde_json::Value> = serde_json::from_slice(&r.payload.events).unwrap();
    assert_eq!(events.len(), caps::EVENTS_MAX_ITEMS);
    assert_eq!(events[127]["name"], "e127");
    let links: Vec<serde_json::Value> = serde_json::from_slice(&r.payload.links).unwrap();
    assert_eq!(links.len(), caps::LINKS_MAX_ITEMS);
    // Item caps keep valid JSON, so they are not truncation.
    assert!(r.payload.truncated_fields.is_empty());
}
