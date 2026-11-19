//! The `crucible-jsonl` format (contract 12.8): one JSON object per span with exactly the top-level keys
//! `schema`, `span` and `facts`. `span` nests the whole record, both tiers with the wire rules of contract 1.3
//! and the MCP operation under `span.mcp`; `facts` holds the effective facts of the span and of its trace.
//!
//! - Bodies are embedded as JSON when they are valid JSON, as a string otherwise, and as a string whenever a cap
//!   cut them (they are listed in `truncated_fields`, or their `*_truncation` is `cap_exceeded`), so a cut body
//!   is never presented as complete. An empty body is the empty string.
//! - The store keeps `metadata` only as its keys and values (contract 3.1), so the object is rebuilt from them:
//!   a value that is JSON text of something other than a string is embedded, any other value is a string.
//! - Counts, sizes and LSNs are numbers saturated at 2^53 - 1, with `saturated: true` in `span` when one was;
//!   costs are USD numbers named `*_usd`; hashes, ids and times are strings.

use crucible_plan::{FactTable, Value};
use crucible_query::{ExportColumn, ExportFact, ExportMcp, ExportRecord};
use crucible_records::{
    PayloadField, ProjectScope, SpanCore, SpanId, SpanPayload, Timestamp, Truncation, Uuid,
};
use crucible_schema::Ty;

use crate::json::{self, Obj};

/// The value of `schema` in every line.
pub const SCHEMA: &str = "crucible.span/1";

/// Columns stored as `UInt8` that hold a flag.
const FLAGS: [&str; 7] = [
    "retracted",
    "value",
    "is_notification",
    "is_batch",
    "timed_out",
    "caller_fault",
    "content_captured",
];

/// Columns stored as `String` that hold JSON.
const JSON_TEXT: [&str; 4] = ["metadata", "corrected", "args", "result"];

/// Appends the value of one stored column (contract 1.3). Returns whether a number saturated. A value that does
/// not fit its type is `null`: the writer never fails.
pub(crate) fn column(out: &mut Vec<u8>, name: &str, ty: &Ty, value: &Value) -> bool {
    match (ty, value) {
        (_, Value::Null) => out.extend_from_slice(b"null"),
        (Ty::Nullable(inner) | Ty::Saf(_, inner), value) => return column(out, name, inner, value),
        (Ty::Uuid, Value::Uuid(id)) => uuid(out, *id),
        (Ty::FixedString(_), Value::Bytes(bytes)) if bytes.iter().all(|b| *b == 0) => {
            out.extend_from_slice(b"null");
        }
        (Ty::FixedString(_), Value::Bytes(bytes)) => {
            out.push(b'"');
            json::hex(out, bytes);
            out.push(b'"');
        }
        (Ty::DateTime | Ty::DateTime64, Value::Time(t)) => json::string(out, &t.to_string()),
        (Ty::Date, Value::Date(d)) => json::string(out, &d.to_string()),
        (Ty::Enum8(kind), Value::Enum(v)) => match kind.variants().iter().find(|(_, n)| n == v) {
            Some((name, _)) => json::string(out, name),
            None => return json::uint(out, u64::from(*v)),
        },
        (Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64, Value::UInt(v)) => {
            if FLAGS.contains(&name) {
                out.extend_from_slice(if *v != 0 { b"true" } else { b"false" });
            } else if name.ends_with("_hash") {
                json::string(out, &format!("{v:016x}"));
            } else {
                return json::uint(out, *v);
            }
        }
        (Ty::Int32 | Ty::Int64, Value::Int(v)) => return json::int(out, *v),
        (Ty::Float32 | Ty::Float64, Value::Float(v)) => json::float(out, *v),
        (Ty::String | Ty::LowCardinalityString, Value::String(s)) => {
            if JSON_TEXT.contains(&name) {
                json::body(out, s.as_bytes(), false);
            } else {
                json::string(out, s);
            }
        }
        (Ty::String | Ty::LowCardinalityString, Value::Bytes(bytes)) => json::text(out, bytes),
        (Ty::Array(inner), Value::Array(items)) => {
            let mut saturated = false;
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                saturated |= column(out, name, inner, item);
            }
            out.push(b']');
            return saturated;
        }
        (Ty::Tuple2(a, b), Value::Tuple2(x, y)) => {
            out.push(b'[');
            let saturated = column(out, name, a, x);
            out.push(b',');
            let saturated = column(out, name, b, y) | saturated;
            out.push(b']');
            return saturated;
        }
        _ => out.extend_from_slice(b"null"),
    }
    false
}

fn uuid(out: &mut Vec<u8>, id: Uuid) {
    if id == Uuid::NIL {
        out.extend_from_slice(b"null");
    } else {
        json::string(out, &id.to_string());
    }
}

fn time(obj: &mut Obj<'_>, key: &str, t: Timestamp) {
    obj.display(key, t);
}

fn span_id_or_null(obj: &mut Obj<'_>, key: &str, id: SpanId) {
    if id == SpanId::ZERO {
        obj.null(key);
    } else {
        obj.display(key, id);
    }
}

fn digest(obj: &mut Obj<'_>, key: &str, digest: Option<[u8; 32]>) {
    match digest {
        Some(bytes) => {
            let out = obj.key(key);
            out.push(b'"');
            json::hex(out, &bytes);
            out.push(b'"');
        }
        None => obj.null(key),
    }
}

/// Whether a payload field was cut by a cap and is opaque text.
fn opaque(core: &SpanCore, payload: &SpanPayload, field: PayloadField) -> bool {
    payload.truncated_fields.contains(field)
        || match field {
            PayloadField::Input => core.input_truncation == Truncation::CapExceeded,
            PayloadField::Output => core.output_truncation == Truncation::CapExceeded,
            _ => false,
        }
}

/// `metadata` rebuilt from its stored keys and values.
fn metadata(out: &mut Vec<u8>, payload: &SpanPayload, cut: bool) {
    let mut rebuilt = Vec::new();
    let mut obj = Obj::new(&mut rebuilt);
    for (key, value) in payload.metadata_keys.iter().zip(&payload.metadata_values) {
        let out = obj.key(key);
        match json::Json::parse(value.as_bytes()) {
            Some(json::Json::Str(_)) | None => json::string(out, value),
            Some(_) => json::minify(out, value.as_bytes()),
        }
    }
    obj.end();
    if cut {
        json::text(out, &rebuilt);
    } else {
        out.extend_from_slice(&rebuilt);
    }
}

/// The span object. Returns nothing; `saturated` is set inside it when a number was cut to 2^53 - 1.
fn span(out: &mut Vec<u8>, scope: ProjectScope, record: &ExportRecord) {
    let (core, payload) = (&record.span.core, &record.span.payload);
    let mut saturated = false;
    let mut obj = Obj::new(out);
    obj.display("project_id", scope.project_id());
    obj.display("trace_id", core.trace_id);
    obj.display("span_id", core.span_id);
    span_id_or_null(&mut obj, "parent_span_id", core.parent_span_id);
    obj.bool("is_root", core.is_root);
    obj.bool("is_app_root", core.is_app_root);
    obj.str("kind", core.kind.as_str());
    obj.str("name", &core.name);
    time(&mut obj, "start_time", core.start_time);
    time(&mut obj, "end_time", core.end_time);
    saturated |= json::uint(obj.key("duration_us"), core.duration_us());
    match core.completion_start_time {
        Some(t) => time(&mut obj, "completion_start_time", t),
        None => obj.null("completion_start_time"),
    }
    obj.str("status_code", core.status_code.as_str());
    obj.str("status_message", &core.status_message);
    for (key, value) in [
        ("service_name", &core.service_name),
        ("service_version", &core.service_version),
        ("scope_name", &core.scope_name),
        ("scope_version", &core.scope_version),
        ("telemetry_sdk_name", &core.telemetry_sdk_name),
        ("telemetry_sdk_language", &core.telemetry_sdk_language),
        ("telemetry_sdk_version", &core.telemetry_sdk_version),
        ("sdk_name", &core.sdk_name),
        ("sdk_version", &core.sdk_version),
    ] {
        obj.str(key, value);
    }
    obj.str("observation_type", core.observation_type.as_str());
    obj.str("level", core.level.as_str());
    for (key, value) in [
        ("environment", &core.environment),
        ("release", &core.release),
        ("version", &core.version),
        ("trace_name", &core.trace_name),
        ("user_id", &core.user_id),
        ("session_id", &core.session_id),
    ] {
        obj.str(key, value);
    }
    obj.strs("trace_tags", &core.trace_tags);
    obj.str("trace_public", core.trace_public.as_str());
    obj.strs("tags", &core.tags);
    obj.str("model_requested", &core.model_requested);
    obj.str("model_response", &core.model_response);
    obj.str("provider", &core.provider);
    obj.strs("finish_reasons", &core.finish_reasons);
    {
        let mut usage = Obj::new(obj.key("usage"));
        for entry in &core.usage {
            saturated |= json::uint(usage.key(&entry.key), entry.units);
        }
        usage.end();
    }
    {
        let out = obj.key("cost_usd");
        out.push(b'[');
        for (i, entry) in core.usage.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            json::usd(out, entry.cost.nanos());
        }
        out.push(b']');
    }
    saturated |= json::uint(obj.key("input_tokens"), core.input_tokens);
    saturated |= json::uint(obj.key("output_tokens"), core.output_tokens);
    saturated |= json::uint(obj.key("total_tokens"), core.total_tokens);
    json::usd(obj.key("total_cost_usd"), core.total_cost_nusd.nanos());
    obj.str("cost_source", core.cost_source.as_str());
    uuid(
        obj.key("price_model_id"),
        core.price_model_id.unwrap_or(Uuid::NIL),
    );
    uuid(
        obj.key("price_tier_id"),
        core.price_tier_id.unwrap_or(Uuid::NIL),
    );
    obj.bool("usage_aggregate", core.usage_aggregate);
    obj.str("prompt_name", &core.prompt_name);
    obj.str("prompt_version", &core.prompt_version);
    obj.strs("tool_names", &core.tool_names);
    json::uint(obj.key("tool_call_count"), u64::from(core.tool_call_count));
    obj.str("mcp_side", core.mcp_side.as_str());
    obj.str("io_shape", core.io_shape.as_str());
    obj.str("input_preview", &core.input_preview);
    obj.str("output_preview", &core.output_preview);
    saturated |= json::uint(obj.key("input_bytes"), core.input_bytes);
    saturated |= json::uint(obj.key("output_bytes"), core.output_bytes);
    obj.str("input_truncation", core.input_truncation.as_str());
    obj.str("output_truncation", core.output_truncation.as_str());
    digest(&mut obj, "input_sha256", core.input_sha256);
    digest(&mut obj, "output_sha256", core.output_sha256);
    obj.str("source_format", core.source_format.as_str());
    json::uint(obj.key("compat_dialect"), u64::from(core.compat_dialect));
    json::uint(
        obj.key("normalizer_version"),
        u64::from(core.normalizer_version),
    );
    obj.bool("clock_skew", core.clock_skew);
    obj.display("wal_epoch", core.ingestion.wal_epoch);
    saturated |= json::uint(obj.key("lsn"), core.ingestion.lsn.get());
    saturated |= json::uint(obj.key("batch_seq"), core.ingestion.batch_seq.get());
    time(&mut obj, "received_at", core.ingestion.received_at);
    obj.display("api_key_id", core.api_key_id);
    obj.str("row_digest", &format!("{:016x}", core.row_digest));
    json::uint(obj.key("event_bytes"), u64::from(core.event_bytes));
    obj.bool("payload_stored", record.payload_stored);

    let cut = |field| opaque(core, payload, field);
    json::body(obj.key("input"), &payload.input, cut(PayloadField::Input));
    json::body(
        obj.key("output"),
        &payload.output,
        cut(PayloadField::Output),
    );
    json::body(
        obj.key("system_instructions"),
        &payload.system_instructions,
        cut(PayloadField::SystemInstructions),
    );
    metadata(obj.key("metadata"), payload, cut(PayloadField::Metadata));
    obj.strs("metadata_keys", &payload.metadata_keys);
    obj.strs("metadata_values", &payload.metadata_values);
    obj.strs("metadata_kv", &payload.metadata_kv);
    for (key, field) in [
        ("trace_metadata", PayloadField::TraceMetadata),
        ("attributes", PayloadField::Attributes),
        ("resource_attributes", PayloadField::ResourceAttributes),
        ("events", PayloadField::Events),
        ("links", PayloadField::Links),
        ("tool_definitions", PayloadField::ToolDefinitions),
        ("tool_calls", PayloadField::ToolCalls),
        ("model_parameters", PayloadField::ModelParameters),
    ] {
        json::body(obj.key(key), payload.field(field), cut(field));
    }
    {
        let mut usage = Obj::new(obj.key("usage_provided"));
        for (key, units) in &payload.usage_provided {
            saturated |= json::uint(usage.key(key), *units);
        }
        usage.end();
    }
    {
        let mut cost = Obj::new(obj.key("cost_provided_usd"));
        for (key, nanos) in &payload.cost_provided {
            json::usd(cost.key(key), nanos.nanos());
        }
        cost.end();
    }
    json::text(obj.key("trace_state"), &payload.trace_state);
    let truncated: Vec<&str> = PayloadField::ALL
        .into_iter()
        .filter(|f| payload.truncated_fields.contains(*f))
        .map(PayloadField::name)
        .collect();
    obj.strs("truncated_fields", &truncated);
    if let Some(mcp) = &record.mcp {
        saturated |= mcp_object(obj.key("mcp"), mcp);
    }
    if saturated {
        obj.bool("saturated", true);
    }
    obj.end();
}

/// The MCP operation: its `mcp_ops` columns, then the captured-content columns the operation row lacks.
/// `content_block_types` and `content_block_counts` become the `content_blocks` map of contract 3.5.
fn mcp_object(out: &mut Vec<u8>, mcp: &ExportMcp) -> bool {
    let mut saturated = false;
    let mut obj = Obj::new(out);
    let find = |name: &str| mcp.op.iter().find(|c| c.name == name);
    for col in &mcp.op {
        match col.name {
            "content_block_counts" => {}
            "content_block_types" => {
                let counts = match find("content_block_counts").map(|c| &c.value) {
                    Some(Value::Array(counts)) => counts.as_slice(),
                    _ => &[],
                };
                let mut blocks = Obj::new(obj.key("content_blocks"));
                if let Value::Array(types) = &col.value {
                    for (kind, count) in types.iter().zip(counts) {
                        let name = match kind {
                            Value::String(s) => s.clone(),
                            Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                            _ => String::new(),
                        };
                        let count = match count {
                            Value::UInt(n) => *n,
                            _ => 0,
                        };
                        json::uint(blocks.key(&name), count);
                    }
                }
                blocks.end();
            }
            name => saturated |= column(obj.key(name), name, &col.ty, &col.value),
        }
    }
    for col in mcp.content.iter().flatten() {
        if find(col.name).is_none() {
            saturated |= column(obj.key(col.name), col.name, &col.ty, &col.value);
        }
    }
    obj.end();
    saturated
}

/// The name of a fact table, as `facts[].table` writes it.
fn table_name(table: FactTable) -> &'static str {
    table.table().name()
}

/// The envelope members of a fact, in this order; the body columns follow `table` in their table's order.
const ENVELOPE: [&str; 12] = [
    "fact_id",
    "target_kind",
    "trace_id",
    "span_id",
    "session_id",
    "target_time",
    "key",
    "version",
    "retracted",
    "author_kind",
    "author_id",
    "created_at",
];

fn fact(out: &mut Vec<u8>, scope: ProjectScope, fact: &ExportFact) {
    let mut obj = Obj::new(out);
    let write = |obj: &mut Obj<'_>, col: &ExportColumn| {
        column(obj.key(col.name), col.name, &col.ty, &col.value);
    };
    for name in ENVELOPE {
        if name == "target_kind" {
            obj.display("project_id", scope.project_id());
        }
        if let Some(col) = fact.columns.iter().find(|c| c.name == name) {
            write(&mut obj, col);
        }
    }
    obj.str("table", table_name(fact.table));
    for col in fact.columns.iter().filter(|c| !ENVELOPE.contains(&c.name)) {
        write(&mut obj, col);
    }
    obj.end();
}

/// Appends the line of one span: the `schema`, `span` and `facts` keys and a newline. The project id comes from
/// `scope`, which the export's reads were made with.
pub fn write_jsonl(scope: ProjectScope, record: &ExportRecord, out: &mut Vec<u8>) {
    let mut line = Obj::new(out);
    line.str("schema", SCHEMA);
    span(line.key("span"), scope, record);
    let facts = line.key("facts");
    facts.push(b'[');
    for (i, f) in record.facts.iter().enumerate() {
        if i > 0 {
            facts.push(b',');
        }
        fact(facts, scope, f);
    }
    facts.push(b']');
    line.end();
    out.push(b'\n');
}
