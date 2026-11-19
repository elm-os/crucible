//! `create_export`: starts an export of the project's spans in a window to a file (contract 12.8). It answers at
//! once with the queued export, which `get_export` follows; the REST API's download path serves the file.

use crucible_api_types::{self as wire, ProblemCode};
use crucible_export::{
    CreateError, CreateExport as Create, Created, EXPORTS_PER_HOUR, ExportFormat, ExportJob,
    Exports, Options, Selector, TxError,
};
use crucible_records::TraceId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::tools::{Call, Check, Tool};

pub(crate) struct CreateExport;

/// The format of an export.
#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Format {
    /// One JSON object per span with the keys `schema`, `span` and `facts`: every field, the MCP operation and
    /// the effective facts.
    CrucibleJsonl,
    /// One OTLP JSON document per span, with the OTel GenAI keys.
    OtlpJson,
}

/// What to export.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The format.
    format: Format,
    /// Start of the window over span start times, RFC 3339 (inclusive).
    from: String,
    /// End of the window, RFC 3339 (exclusive).
    to: String,
    /// Only spans of these traces, 32 hexadecimal characters each, at most 10,000.
    #[serde(default)]
    trace_ids: Vec<String>,
    /// Only spans of this user.
    #[serde(default)]
    user_id: Option<String>,
    /// Only spans of this session.
    #[serde(default)]
    session_id: Option<String>,
    /// Compress the file with gzip.
    #[serde(default)]
    gzip: bool,
}

/// An export: where it is in its life, and once it is ready, its size, checksum and download path.
#[derive(Serialize, JsonSchema)]
pub(crate) struct ExportView {
    /// The export id; pass it to get_export.
    id: String,
    /// `crucible-jsonl` or `otlp-json`.
    format: String,
    /// `queued`, `running`, `ready`, `failed` or `expired`.
    state: String,
    /// Start of the window, RFC 3339.
    from: String,
    /// End of the window, RFC 3339.
    to: String,
    /// Spans written so far, or in all once it is ready.
    spans: u64,
    /// Spans the format left out (spans with a cut field, in `otlp-json`).
    skipped: u64,
    /// Bytes written so far, or the file's length once it is ready.
    bytes: u64,
    /// SHA-256 of the file, lowercase hex, once it is ready.
    checksum: Option<String>,
    /// When the file stops being downloadable, RFC 3339.
    expires_at: Option<String>,
    /// Why a failed export ended without a file; `query_timeout`, `query_memory_exceeded`, `query_too_broad`
    /// and `export_too_large` mean: narrow the window or the selection.
    error_code: Option<String>,
    /// The REST path of the download (GET with the same key), once it is ready.
    download_path: Option<String>,
    /// When it was created, RFC 3339.
    created_at: String,
}

/// The view of an export at the service's clock.
pub(crate) fn view(exports: &Exports, job: &ExportJob) -> ExportView {
    let dto = crucible_api::export_dto(job, exports.now());
    ExportView {
        id: dto.id,
        format: match dto.format {
            wire::ExportFormat::CrucibleJsonl => "crucible-jsonl",
            wire::ExportFormat::OtlpJson => "otlp-json",
        }
        .to_owned(),
        state: match dto.state {
            wire::ExportState::Queued => "queued",
            wire::ExportState::Running => "running",
            wire::ExportState::Ready => "ready",
            wire::ExportState::Failed => "failed",
            wire::ExportState::Expired => "expired",
        }
        .to_owned(),
        from: dto.from,
        to: dto.to,
        spans: dto.spans.get(),
        skipped: dto.skipped.get(),
        bytes: dto.bytes.get(),
        checksum: dto.checksum,
        expires_at: dto.expires_at,
        error_code: dto.error_code,
        download_path: dto.download_path,
        created_at: dto.created_at,
    }
}

/// The export service, or the refusal that names the setting it needs.
pub(crate) fn service<'a>(call: &Call<'a>) -> Result<&'a Exports, ToolError> {
    call.exports.ok_or_else(|| {
        ToolError::new(
            ProblemCode::FeatureDisabled,
            "exports need the control plane, which holds them; set postgres.url (CRUCIBLE_POSTGRES_URL) on the server",
        )
    })
}

pub(crate) fn store_error(error: &TxError) -> ToolError {
    match error {
        TxError::Unavailable(_) | TxError::CommitUnknown | TxError::SerializationFailure => {
            ToolError::new(
                ProblemCode::ControlPlaneUnavailable,
                "the control plane (Postgres) is unavailable; try again shortly",
            )
        }
        _ => ToolError::new(ProblemCode::InternalError, "internal error"),
    }
}

impl Tool for CreateExport {
    const NAME: &'static str = "create_export";
    const DESCRIPTION: &'static str = "Starts an export of the project's spans that start in a window \
        (`from`, `to`), optionally only some traces, one user or one session, to a file in the crucible-jsonl \
        format (every field, the MCP operation and the effective facts) or the otlp-json format (OTLP JSON with the \
        OTel GenAI keys). It answers at once with the queued export; call get_export with its id until the state is \
        `ready`, then download the file from `download_path` over the REST API with the same key. A project may \
        create 5 exports per hour.";
    const READ_ONLY: bool = false;
    type Args = Args;
    type Output = ExportView;

    async fn call(call: &Call<'_>, args: Args) -> Result<ExportView, ToolError> {
        let mut check = Check::default();
        let window = check.window(&args.from, &args.to);
        if args.trace_ids.len() > crucible_export::MAX_TRACE_IDS {
            check.error(
                "trace_ids",
                "too_many",
                "at most 10,000 trace ids; export the window instead",
            );
        }
        let trace_ids: Vec<TraceId> = args
            .trace_ids
            .iter()
            .enumerate()
            .filter_map(|(at, id)| check.trace_id(&format!("trace_ids/{at}"), id))
            .collect();
        check.finish()?;
        let Some(window) = window else {
            return Err(ToolError::new(
                ProblemCode::ValidationFailed,
                "the window is invalid; give `from` before `to`, each an RFC 3339 time",
            ));
        };
        let exports = service(call)?;
        let request = Create {
            format: match args.format {
                Format::CrucibleJsonl => ExportFormat::CrucibleJsonl,
                Format::OtlpJson => ExportFormat::OtlpJson,
            },
            selector: Selector {
                window,
                trace_ids,
                user_id: args.user_id,
                session_id: args.session_id,
            },
            options: Options {
                gzip: args.gzip,
                keep_truncated: false,
            },
            created_by: format!("api_key:{}", call.key),
            idempotency_key: None,
        };
        let created = exports.create(call.ctx.scope(), request).await.map_err(|error| match error {
            CreateError::RateLimited { retry_after_s } => ToolError::new(
                ProblemCode::RateLimited,
                format!(
                    "a project may create {EXPORTS_PER_HOUR} exports per hour; try again in {retry_after_s} s"
                ),
            )
            .with_limit("exports_per_hour", EXPORTS_PER_HOUR as u32),
            CreateError::KeyReused => ToolError::new(
                ProblemCode::IdempotencyKeyReused,
                "the request repeats an earlier one with other arguments; call create_export again",
            ),
            CreateError::Store(error) => store_error(&error),
        })?;
        let job = match created {
            Created::New(job) | Created::Replay(job) => job,
        };
        Ok(view(exports, &job))
    }
}
