//! `row_digest` (contract 3.1): xxh3-64 of the normalized row excluding the ingestion group.
//!
//! The contract fixes the hash and the excluded group but not the bytes that are hashed; this module fixes them as
//! encoding version [`DIGEST_ENCODING`]. The row is both tiers of the [`SpanRecord`], so two spans that differ only
//! in a payload body digest differently. The excluded ingestion group is `wal_epoch`, `lsn`, `batch_seq`,
//! `received_at`, `api_key_id`, `row_digest` itself and `event_bytes`.
//!
//! The encoding is the version byte, then every other field in declaration order, core tier first: integers
//! little-endian at their own width; `bool` and enums one byte (the stored value); timestamps as `i64`
//! microseconds; ids at their byte length; an optional value as a presence byte and the value; strings and bodies as
//! a `u64` length and the bytes; lists and maps as a `u64` count and the items; `truncated_fields` as a `u16`
//! bit set in [`crucible_records::PayloadField::ALL`] order.

use crucible_records::{
    NanoUsd, PayloadFields, SpanCore, SpanPayload, SpanRecord, Timestamp, UsageEntry, Uuid,
};
use xxhash_rust::xxh3::Xxh3;

/// The version of the encoding that [`row_digest`] hashes. A change to it changes digests, so it ships with a new
/// normalizer version.
pub const DIGEST_ENCODING: u8 = 1;

/// Where encoded bytes go.
trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for Xxh3 {
    fn put(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

#[cfg(test)]
impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

struct Encoder<'s, S: Sink>(&'s mut S);

impl<S: Sink> Encoder<'_, S> {
    fn u8(&mut self, v: u8) {
        self.0.put(&[v]);
    }

    fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    fn u16(&mut self, v: u16) {
        self.0.put(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.0.put(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.0.put(&v.to_le_bytes());
    }

    fn i64(&mut self, v: i64) {
        self.0.put(&v.to_le_bytes());
    }

    fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }

    fn time(&mut self, t: Timestamp) {
        self.i64(t.micros());
    }

    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.put(b);
    }

    fn strs(&mut self, list: &[Box<str>]) {
        self.len(list.len());
        for s in list {
            self.bytes(s.as_bytes());
        }
    }

    fn uuid(&mut self, id: Option<Uuid>) {
        match id {
            Some(id) => {
                self.u8(1);
                self.0.put(id.as_bytes());
            }
            None => self.u8(0),
        }
    }

    fn sha(&mut self, hash: Option<[u8; 32]>) {
        match hash {
            Some(hash) => {
                self.u8(1);
                self.0.put(&hash);
            }
            None => self.u8(0),
        }
    }

    fn nano(&mut self, cost: NanoUsd) {
        self.i64(cost.nanos());
    }

    fn fields(&mut self, set: PayloadFields) {
        let bits = set.iter().fold(0u16, |bits, f| bits | (1 << f as u8));
        self.u16(bits);
    }
}

fn encode<S: Sink>(record: &SpanRecord, sink: &mut S) {
    let SpanCore {
        project_id,
        trace_id,
        span_id,
        parent_span_id,
        is_root,
        is_app_root,
        kind,
        name,
        start_time,
        end_time,
        completion_start_time,
        status_code,
        status_message,
        service_name,
        service_version,
        scope_name,
        scope_version,
        telemetry_sdk_name,
        telemetry_sdk_language,
        telemetry_sdk_version,
        sdk_name,
        sdk_version,
        observation_type,
        level,
        environment,
        release,
        version,
        trace_name,
        user_id,
        session_id,
        trace_tags,
        trace_public,
        tags,
        model_requested,
        model_response,
        provider,
        finish_reasons,
        usage,
        input_tokens,
        output_tokens,
        total_tokens,
        total_cost_nusd,
        cost_source,
        price_model_id,
        price_tier_id,
        prompt_name,
        prompt_version,
        tool_names,
        tool_call_count,
        mcp_side,
        io_shape,
        input_preview,
        output_preview,
        input_bytes,
        output_bytes,
        input_truncation,
        output_truncation,
        input_sha256,
        output_sha256,
        source_format,
        compat_dialect,
        normalizer_version,
        clock_skew,
        // The ingestion group.
        ingestion: _,
        api_key_id: _,
        row_digest: _,
        event_bytes: _,
    } = &record.core;
    let SpanPayload {
        input,
        output,
        system_instructions,
        metadata,
        metadata_keys,
        metadata_values,
        metadata_kv,
        trace_metadata,
        attributes,
        resource_attributes,
        events,
        links,
        tool_definitions,
        tool_calls,
        model_parameters,
        usage_provided,
        cost_provided,
        trace_state,
        truncated_fields,
    } = &record.payload;

    let mut e = Encoder(sink);
    e.u8(DIGEST_ENCODING);
    e.0.put(project_id.uuid().as_bytes());
    e.0.put(trace_id.as_bytes());
    e.0.put(span_id.as_bytes());
    e.0.put(parent_span_id.as_bytes());
    e.bool(*is_root);
    e.bool(*is_app_root);
    e.u8(kind.value());
    e.bytes(name.as_bytes());
    e.time(*start_time);
    e.time(*end_time);
    match completion_start_time {
        Some(t) => {
            e.u8(1);
            e.time(*t);
        }
        None => e.u8(0),
    }
    e.u8(status_code.value());
    for s in [
        status_message,
        service_name,
        service_version,
        scope_name,
        scope_version,
        telemetry_sdk_name,
        telemetry_sdk_language,
        telemetry_sdk_version,
        sdk_name,
        sdk_version,
    ] {
        e.bytes(s.as_bytes());
    }
    e.u8(observation_type.value());
    e.u8(level.value());
    for s in [
        environment,
        release,
        version,
        trace_name,
        user_id,
        session_id,
    ] {
        e.bytes(s.as_bytes());
    }
    e.strs(trace_tags);
    e.u8(trace_public.value());
    e.strs(tags);
    for s in [model_requested, model_response, provider] {
        e.bytes(s.as_bytes());
    }
    e.strs(finish_reasons);
    e.len(usage.len());
    for UsageEntry { key, units, cost } in usage {
        e.bytes(key.as_bytes());
        e.u64(*units);
        e.nano(*cost);
    }
    e.u64(*input_tokens);
    e.u64(*output_tokens);
    e.u64(*total_tokens);
    e.nano(*total_cost_nusd);
    e.u8(cost_source.value());
    e.uuid(*price_model_id);
    e.uuid(*price_tier_id);
    e.bytes(prompt_name.as_bytes());
    e.bytes(prompt_version.as_bytes());
    e.strs(tool_names);
    e.u32(*tool_call_count);
    e.u8(mcp_side.value());
    e.u8(io_shape.value());
    e.bytes(input_preview.as_bytes());
    e.bytes(output_preview.as_bytes());
    e.u64(*input_bytes);
    e.u64(*output_bytes);
    e.u8(input_truncation.value());
    e.u8(output_truncation.value());
    e.sha(*input_sha256);
    e.sha(*output_sha256);
    e.u8(source_format.value());
    e.u8(*compat_dialect);
    e.u32(*normalizer_version);
    e.bool(*clock_skew);

    for body in [input, output, system_instructions, metadata] {
        e.bytes(body);
    }
    e.strs(metadata_keys);
    e.strs(metadata_values);
    e.strs(metadata_kv);
    for body in [
        trace_metadata,
        attributes,
        resource_attributes,
        events,
        links,
        tool_definitions,
        tool_calls,
        model_parameters,
    ] {
        e.bytes(body);
    }
    e.len(usage_provided.len());
    for (key, units) in usage_provided {
        e.bytes(key.as_bytes());
        e.u64(*units);
    }
    e.len(cost_provided.len());
    for (key, cost) in cost_provided {
        e.bytes(key.as_bytes());
        e.nano(*cost);
    }
    e.bytes(trace_state);
    e.fields(*truncated_fields);
}

/// The row digest of a normalized record.
pub fn row_digest(record: &SpanRecord) -> u64 {
    let mut hasher = Xxh3::new();
    encode(record, &mut hasher);
    hasher.digest()
}

/// The bytes [`row_digest`] hashes.
#[cfg(test)]
pub(crate) fn canonical_row(record: &SpanRecord) -> Vec<u8> {
    let mut out = Vec::new();
    encode(record, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use crucible_records::{
        ApiKeyId, CostSource, Lsn, PayloadField, ProjectId, SpanId, TraceId, TracePublic, WalEpoch,
    };
    use xxhash_rust::xxh3::xxh3_64;

    use super::*;
    use crate::test_support::record;

    #[test]
    fn the_digest_is_xxh3_of_the_canonical_row() {
        let r = record();
        let row = canonical_row(&r);
        assert_eq!(row[0], DIGEST_ENCODING);
        assert_eq!(row_digest(&r), xxh3_64(&row));
    }

    #[test]
    fn the_ingestion_group_does_not_change_the_digest() {
        let base = record();
        let digest = row_digest(&base);
        let mutations: [fn(&mut SpanRecord); 7] = [
            |r| r.core.ingestion.wal_epoch = WalEpoch::from_bytes([9; 16]),
            |r| r.core.ingestion.lsn = Lsn::new(77),
            |r| r.core.ingestion.batch_seq = Lsn::new(70),
            |r| r.core.ingestion.received_at = Timestamp::from_micros(1),
            |r| r.core.api_key_id = ApiKeyId::from_bytes([8; 16]),
            |r| r.core.row_digest = 12345,
            |r| r.core.event_bytes = 999,
        ];
        for mutate in mutations {
            let mut r = base.clone();
            mutate(&mut r);
            assert_eq!(row_digest(&r), digest);
        }
    }

    #[test]
    fn every_other_field_changes_the_digest() {
        let base = record();
        let digest = row_digest(&base);
        let mutations: [fn(&mut SpanRecord); 18] = [
            |r| r.core.project_id = ProjectId::from_bytes([7; 16]),
            |r| r.core.trace_id = TraceId::from_bytes([7; 16]),
            |r| r.core.span_id = SpanId::from_bytes([7; 8]),
            |r| r.core.is_root = !r.core.is_root,
            |r| r.core.name = "other".into(),
            |r| r.core.end_time = Timestamp::from_micros(r.core.end_time.micros() + 1),
            |r| r.core.completion_start_time = Some(r.core.start_time),
            |r| r.core.user_id = "u".into(),
            |r| r.core.trace_tags = vec!["t".into()],
            |r| r.core.trace_public = TracePublic::Public,
            |r| r.core.normalizer_version += 1,
            |r| r.core.clock_skew = !r.core.clock_skew,
            |r| r.core.input_sha256 = Some([0; 32]),
            |r| r.payload.input = Bytes::from_static(b"in"),
            |r| r.payload.attributes = Bytes::from_static(b"{}"),
            |r| r.payload.usage_provided = vec![("input".into(), 1)],
            |r| r.payload.trace_state = Bytes::from_static(b"k=v"),
            |r| r.payload.truncated_fields.insert(PayloadField::Links),
        ];
        for (i, mutate) in mutations.into_iter().enumerate() {
            let mut r = base.clone();
            mutate(&mut r);
            assert_ne!(row_digest(&r), digest, "mutation {i}");
        }
    }

    /// The cost fields that calculated pricing sets are part of the digest.
    #[test]
    fn calculated_cost_fields_change_the_digest() {
        let mut base = record();
        base.core.usage = vec![UsageEntry {
            key: "input".into(),
            units: 1,
            cost: NanoUsd::ZERO,
        }];
        let digest = row_digest(&base);
        let mutations: [fn(&mut SpanRecord); 5] = [
            |r| r.core.usage[0].cost = NanoUsd::from_nanos(1),
            |r| r.core.total_cost_nusd = NanoUsd::from_nanos(1),
            |r| r.core.cost_source = CostSource::Calculated,
            |r| r.core.price_model_id = Some(Uuid::from_bytes([7; 16])),
            |r| r.core.price_tier_id = Some(Uuid::from_bytes([7; 16])),
        ];
        for (i, mutate) in mutations.into_iter().enumerate() {
            let mut r = base.clone();
            mutate(&mut r);
            assert_ne!(row_digest(&r), digest, "mutation {i}");
        }
    }

    /// Moving bytes between adjacent strings changes the digest: lengths delimit every value.
    #[test]
    fn values_are_delimited() {
        let mut a = record();
        a.core.service_name = "ab".into();
        a.core.service_version = "".into();
        let mut b = record();
        b.core.service_name = "a".into();
        b.core.service_version = "b".into();
        assert_ne!(row_digest(&a), row_digest(&b));
    }
}
