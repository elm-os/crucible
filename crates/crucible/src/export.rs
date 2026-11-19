//! `crucible export`: one export written from the command line, read from the telemetry store under a fixed
//! `as_of` and written in either format of contract 12.8 to a file or to standard output, without an export job.
//!
//! The project must be one the deployment knows: a project of the control plane (slug or id) when Postgres is
//! configured, else a project that holds a key in `--keys-file`. Spans a running server has acknowledged but
//! not yet shipped to the store are not included; the export reads what the store holds at `as_of`.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Args;
use crucible_control::{ExportFormat, Repos as _};
use crucible_env::{Env, Stores, TelemetryStore as _};
use crucible_export::{CHUNK_BYTES, ExportWriter, MAX_TRACE_IDS, Options, Pages, Selector};
use crucible_plan::Caller;
use crucible_query::{ExportLimits, QueryService, ReadContext};
use crucible_recent::{RecentConfig, RecentIndex, SpanRead, WalLocator, WalReadError, WalView};
use crucible_records::{Lsn, ProjectId, ProjectScope, TimeWindow, Timestamp, TraceId, WalEpoch};
use crucible_schema::RetentionSettings;

use crate::ConfigArgs;
use crate::control::Control;

/// The options of `crucible export`.
#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    #[command(flatten)]
    pub(crate) config: ConfigArgs,
    /// The format: `crucible-jsonl` (every field, the MCP operation and the effective facts) or `otlp-json`
    /// (one OTLP JSON document per span, with the OTel GenAI keys).
    #[arg(long)]
    format: String,
    /// The project: its slug or id with a control plane, else the id of a project in --keys-file.
    #[arg(long)]
    project: String,
    /// The start of the window over span start times, RFC 3339 (inclusive).
    #[arg(long)]
    from: String,
    /// The end of the window, RFC 3339 (exclusive).
    #[arg(long)]
    to: String,
    /// Only spans of this trace (32 hexadecimal characters); repeat for several.
    #[arg(long = "trace-id")]
    trace_ids: Vec<String>,
    /// Only spans of this user.
    #[arg(long)]
    user_id: Option<String>,
    /// Only spans of this session.
    #[arg(long)]
    session_id: Option<String>,
    /// Compress the output with gzip.
    #[arg(long)]
    gzip: bool,
    /// With `otlp-json`, write spans that have a field cut by a size cap (as opaque text) instead of skipping them.
    #[arg(long)]
    keep_truncated: bool,
    /// Write to this file instead of standard output.
    #[arg(long, short)]
    output: Option<PathBuf>,
}

/// The index of a process that has no WAL: it never holds an entry, so it never re-reads a frame.
#[derive(Debug)]
struct NoWal;

impl WalView for NoWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::ZERO
    }

    fn read_span(&self, _project_id: ProjectId, _locator: WalLocator) -> SpanRead {
        Box::pin(async {
            Err(WalReadError::Failed(
                "this process has no write-ahead log".to_owned(),
            ))
        })
    }
}

fn format(text: &str) -> anyhow::Result<ExportFormat> {
    ExportFormat::parse(text)
        .with_context(|| format!("--format {text:?}: use crucible-jsonl or otlp-json"))
}

fn selector(args: &ExportArgs) -> anyhow::Result<Selector> {
    let time = |flag: &str, text: &str| {
        Timestamp::parse_rfc3339(text).with_context(|| {
            format!("--{flag} {text:?} is not an RFC 3339 time, for example 2026-11-17T09:30:00Z")
        })
    };
    let window = TimeWindow::new(time("from", &args.from)?, time("to", &args.to)?)
        .map_err(|_| anyhow::anyhow!("--from must be before --to"))?;
    anyhow::ensure!(
        args.trace_ids.len() <= MAX_TRACE_IDS,
        "at most {MAX_TRACE_IDS} --trace-id values; export the window, or split the list"
    );
    let trace_ids = args
        .trace_ids
        .iter()
        .map(|id| {
            TraceId::parse(id)
                .with_context(|| format!("--trace-id {id:?} is not 32 hexadecimal characters"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Selector {
        window,
        trace_ids,
        user_id: args.user_id.clone(),
        session_id: args.session_id.clone(),
    })
}

/// The scope and retention of the named project: from the control plane, else from the keys file.
async fn project(
    env: &Env,
    config: &crucible_config::Config,
    reference: &str,
) -> anyhow::Result<(ProjectScope, RetentionSettings)> {
    if let Some(postgres) = &config.postgres {
        let control = Control::new(env, postgres)?;
        let projects = control.store.projects();
        let found = match ProjectId::parse(reference) {
            Ok(id) => projects.get(id).await?,
            Err(_) => projects.get_by_slug(reference).await?,
        };
        let project = found.filter(|p| p.deleted_at.is_none()).with_context(|| {
            format!(
                "no project has the slug or id {reference:?}; the first project's slug is `default`, and Settings \
                 lists the others"
            )
        })?;
        let scope =
            crucible_auth::stored_project_scope(&project).context("the project id is nil")?;
        let retention = RetentionSettings {
            retention_days: project.retention_days,
            payload_retention_days: project.payload_retention_days,
            mcp_content_retention_days: project.mcp_content_retention_days,
        };
        return Ok((scope, retention));
    }
    let path = config.server.keys_file.as_ref().context(
        "name the deployment's projects: set postgres.url (--postgres-url, CRUCIBLE_POSTGRES_URL), or --keys-file \
         with a key of the project",
    )?;
    let keys = crate::keys::load(path, env)?;
    let id = ProjectId::parse(reference).with_context(|| {
        format!("--project {reference:?}: without a control plane, give the project's id")
    })?;
    let scope = keys
        .project_scope(id)
        .with_context(|| format!("no key in {} belongs to project {id}", path.display()))?;
    Ok((scope, RetentionSettings::default()))
}

/// Runs `crucible export`.
#[allow(clippy::disallowed_methods, clippy::disallowed_types)] // The binary writes the operator's file or standard output.
pub(crate) async fn run(args: &ExportArgs, config: crucible_config::Config) -> anyhow::Result<()> {
    let format = format(&args.format)?;
    let selector = selector(args)?;
    let clickhouse = config.clickhouse.as_ref().context(
        "crucible export reads the telemetry store: set clickhouse.url (--clickhouse-url, CRUCIBLE_CLICKHOUSE_URL)",
    )?;
    let prod = crucible_env_prod::build(
        tokio::runtime::Handle::current(),
        &crucible_env_prod::ProdConfig::default(),
    )
    .context("building the production environment")?;
    let env = &prod.env;
    let (scope, retention) = project(env, &config, &args.project).await?;
    let clickhouse = crate::store::ClickHouse::new(clickhouse, config.sql)?;
    let store = clickhouse.store(env);
    let facts = store.detect().await.map_err(|error| {
        anyhow::anyhow!("the telemetry store did not pass startup detection: {error}")
    })?;
    let (index, _writer) = RecentIndex::new(
        RecentConfig::new(1 << 20, facts.flavour),
        Arc::clone(&env.clock),
        Arc::new(NoWal),
    );
    let service = QueryService::new(
        Stores::new(Arc::new(store)),
        Arc::clone(&env.clock),
        index,
        WalEpoch::from_uuid(env.new_id()),
    );
    let ctx = ReadContext::new(scope, retention, Caller::Session);
    let query = selector.query(env.clock.wall_now());
    let options = Options {
        gzip: args.gzip,
        keep_truncated: args.keep_truncated,
    };
    let mut out: Box<dyn std::io::Write> = match &args.output {
        Some(path) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
        )),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut writer = ExportWriter::new(format, options);
    let mut pages = Pages::new(&service, &ctx, &query, ExportLimits::default());
    while pages
        .next(&mut writer)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?
    {
        if let Some(chunk) = writer.ready(CHUNK_BYTES) {
            out.write_all(&chunk).context("writing the export")?;
        }
    }
    let (rest, summary) = writer
        .finish()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    out.write_all(&rest).context("writing the export")?;
    out.flush().context("writing the export")?;
    writeln!(
        std::io::stderr().lock(),
        "exported {} spans of project {} ({} skipped for a cut field) as {}: {} bytes, sha256 {}",
        summary.spans,
        scope.project_id(),
        summary.skipped,
        format.as_str(),
        summary.bytes,
        summary.sha256
    )?;
    Ok(())
}
