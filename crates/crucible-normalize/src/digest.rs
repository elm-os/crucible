//! `row_digest` (contract 3.1): xxh3-64 of the normalized row excluding the ingestion group.
//!
//! The contract fixes the hash and the excluded group but not the bytes that are hashed; this module fixes them as
//! encoding version [`DIGEST_ENCODING`]. The row is both tiers of the [`SpanRecord`], so two spans that differ only
//! in a payload body digest differently. The excluded ingestion group is `wal_epoch`, `lsn`, `batch_seq`,
//! `received_at`, `api_key_id`, `row_digest` itself and `event_bytes`. `usage_aggregate` is excluded too: stage B
//! sets it from the LSN stream after the digest is taken, so a re-sent span keeps its digest.
//!
//! A span with an MCP operation or tool inventory entries digests those too ([`fold_mcp`]): the digest of such a
//! span is xxh3-64 of the span's own digest, a presence byte and the operation (every field of the record but its
//! ingestion group, then the raw user subject), then each inventory entry's tool name and schema hash behind a
//! one byte and a closing zero byte. A span without either keeps the digest of its row alone, so the encoding
//! version does not change for it.
//!
//! The encoding is the version byte, then every other field in declaration order, core tier first: integers
//! little-endian at their own width; `bool` and enums one byte (the stored value); timestamps as `i64`
//! microseconds; ids at their byte length; an optional value as a presence byte and the value; strings and bodies as
//! a `u64` length and the bytes; lists and maps as a `u64` count and the items; `truncated_fields` as a `u16`
//! bit set in [`crucible_records::PayloadField::ALL`] order.

use crucible_records::{
    McpOpRecord, NanoUsd, PayloadFields, SpanCore, SpanPayload, SpanRecord, Timestamp, UsageEntry,
    Uuid,
};
use xxhash_rust::xxh3::Xxh3;

use crate::mcp::{McpInventory, McpOp};

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

    fn hash16(&mut self, hash: Option<[u8; 16]>) {
        match hash {
            Some(hash) => {
                self.u8(1);
                self.0.put(&hash);
            }
            None => self.u8(0),
        }
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

/// Encodes an MCP operation: every field but the ingestion group, in declaration order, then the user subject.
fn encode_op<S: Sink>(op: &McpOp, e: &mut Encoder<'_, S>) {
    let McpOpRecord {
        project_id,
        trace_id,
        span_id,
        parent_span_id,
        transport_link_trace_id,
        transport_link_span_id,
        server_name,
        server_version,
        deploy_id,
        instance_id,
        side,
        capture_source,
        era,
        protocol_version,
        transport,
        method,
        target_kind,
        target_name,
        jsonrpc_id,
        direction,
        is_notification,
        is_batch,
        result_type,
        mrtr_round,
        mrtr_state_in,
        mrtr_state_out,
        task_id,
        subscription_id,
        start_time,
        duration_us,
        progress_count,
        cancel_source,
        timed_out,
        status,
        error_class,
        error_subclass,
        jsonrpc_code,
        http_status,
        caller_fault,
        error_type,
        error_message,
        client_name,
        client_version,
        client_capabilities_hash,
        oauth_client_id,
        user_subject_hash,
        user_issuer,
        user_id,
        tenant,
        identity_source,
        session_key,
        handle_id,
        group_key,
        group_key_kind,
        ip_prefix,
        user_agent,
        request_bytes,
        response_bytes,
        content_blocks,
        content_captured,
        content_sample_rate,
        content_truncation,
        args,
        result,
        args_bytes,
        result_bytes,
        ingestion: _,
        normalizer_version,
    } = &op.record;
    e.0.put(&project_id.to_bytes());
    e.0.put(trace_id.as_bytes());
    e.0.put(span_id.as_bytes());
    e.0.put(parent_span_id.as_bytes());
    e.0.put(transport_link_trace_id.as_bytes());
    e.0.put(transport_link_span_id.as_bytes());
    for s in [server_name, server_version, deploy_id, instance_id] {
        e.str(s);
    }
    e.u8(side.value());
    e.u8(capture_source.value());
    e.u8(era.value());
    e.str(protocol_version);
    e.u8(transport.value());
    e.str(method);
    e.u8(target_kind.value());
    e.str(target_name);
    e.str(jsonrpc_id);
    e.u8(direction.value());
    e.bool(*is_notification);
    e.bool(*is_batch);
    e.u8(result_type.value());
    e.u16(*mrtr_round);
    e.hash16(*mrtr_state_in);
    e.hash16(*mrtr_state_out);
    e.str(task_id);
    e.str(subscription_id);
    e.time(*start_time);
    e.u64(*duration_us);
    e.bool(progress_count.is_some());
    e.u32(progress_count.unwrap_or(0));
    e.u8(cancel_source.value());
    e.bool(*timed_out);
    e.u8(status.value());
    e.u8(error_class.value());
    e.u8(error_subclass.value());
    e.bool(jsonrpc_code.is_some());
    e.i64(i64::from(jsonrpc_code.unwrap_or(0)));
    e.bool(http_status.is_some());
    e.u16(http_status.unwrap_or(0));
    e.bool(*caller_fault);
    for s in [error_type, error_message, client_name, client_version] {
        e.str(s);
    }
    e.u64(*client_capabilities_hash);
    e.str(oauth_client_id);
    e.hash16(*user_subject_hash);
    e.str(user_issuer);
    e.str(user_id);
    e.str(tenant);
    e.u8(identity_source.value());
    e.hash16(*session_key);
    e.str(handle_id);
    e.str(group_key);
    e.u8(group_key_kind.value());
    e.str(ip_prefix);
    e.str(user_agent);
    e.u64(*request_bytes);
    e.u64(*response_bytes);
    e.len(content_blocks.len());
    for (kind, count) in content_blocks {
        e.str(kind);
        e.u32(*count);
    }
    e.bool(*content_captured);
    e.u32(content_sample_rate.to_bits());
    e.u8(content_truncation.value());
    e.bytes(args);
    e.bytes(result);
    e.u64(*args_bytes);
    e.u64(*result_bytes);
    e.u32(*normalizer_version);
    e.str(&op.subject);
}

/// Folds the MCP operation and the tool inventory entries of each span into its `row_digest`, so that a span
/// sent again with another operation is not an exact repeat. `ops` and `inventory` are in span order; a span
/// without either keeps its digest.
pub(crate) fn fold_mcp(spans: &mut [SpanRecord], ops: &[McpOp], inventory: &[McpInventory]) {
    let mut ops = ops.iter().peekable();
    let mut inventory = inventory.iter().peekable();
    for (at, span) in spans.iter_mut().enumerate() {
        let at = u32::try_from(at).unwrap_or(u32::MAX);
        let op = ops.next_if(|op| op.span == at);
        if op.is_none() && inventory.peek().is_none_or(|entry| entry.span != at) {
            continue;
        }
        let mut hasher = Xxh3::new();
        let mut e = Encoder(&mut hasher);
        e.u64(span.core.row_digest);
        e.bool(op.is_some());
        if let Some(op) = op {
            encode_op(op, &mut e);
        }
        while let Some(entry) = inventory.next_if(|entry| entry.span == at) {
            e.u8(1);
            e.str(&entry.record.tool_name);
            e.u64(entry.record.schema_hash);
        }
        e.u8(0);
        span.core.row_digest = hasher.digest();
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
        // Set by stage B from the LSN stream, after the digest is taken.
        usage_aggregate: _,
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
        let mutations: [fn(&mut SpanRecord); 8] = [
            |r| r.core.ingestion.wal_epoch = WalEpoch::from_bytes([9; 16]),
            |r| r.core.ingestion.lsn = Lsn::new(77),
            |r| r.core.ingestion.batch_seq = Lsn::new(70),
            |r| r.core.ingestion.received_at = Timestamp::from_micros(1),
            |r| r.core.api_key_id = ApiKeyId::from_bytes([8; 16]),
            |r| r.core.row_digest = 12345,
            |r| r.core.event_bytes = 999,
            |r| r.core.usage_aggregate = true,
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
