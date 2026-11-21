//! Exports (contracts 8.7, 11.1, 12.6, 12.8 and 13.4).
//!
//! - **Writers.** [`write_jsonl`] and [`write_otlp`] turn one exported span ([`crucible_query::ExportRecord`])
//!   into its line of the `crucible-jsonl` or `otlp-json` format. Both take the `ProjectScope` the reads were made
//!   with, and both are total: any record gives one line of valid JSON, or, for `otlp-json`, a counted skip.
//! - **Files.** [`ExportWriter`] gathers the lines of one file, gzip-compressed when the options ask for it, and
//!   its SHA-256; [`Pages`] reads the query service into it page by page. On the `exports` disk root each attempt
//!   writes a temporary file of its own and commits it atomically (`sync_data`, rename, `sync_dir`) before the
//!   row records it; [`sweep`] removes what no export needs. With an object store ([`ExportObjects`]) the file
//!   is an object under the project's key prefix instead, which appears whole at the commit and downloads
//!   through a presigned link from any node; [`sweep_objects`] removes what no export needs there.
//! - **Jobs.** [`job`] is the claimed consumer of `export_jobs`, [`sweep_job`] the per-node sweep.
//! - **Service.** [`Exports`] creates exports under the project's rate limit and the request's
//!   `Idempotency-Key`, reads them and opens committed files for download, for the API and the MCP server.
//! - **Deletion.** [`purge`] is the export side of a deletion request (contract 13.4), and
//!   [`ExportObjects::remove`] its object side once the expiry has committed.

mod files;
mod job;
mod json;
mod jsonl;
mod objects;
mod otlp;
mod service;
mod spec;
mod stream;

pub use files::{FilesError, Purged, Swept, purge, sweep};
pub use job::{
    EXPORT_LEASE, EXPORT_TTL, ExportRuntime, ExportState, SHIPPING_WAIT, SWEEP_EVERY, codes,
    expiry, job, sweep_job,
};
pub use jsonl::{SCHEMA, write_jsonl};
pub use objects::{DOWNLOAD_TTL, ExportObjects, PART_BYTES, sweep_objects};
pub use otlp::{Written, write_otlp};
pub use service::{
    CreateError, CreateExport, Created, Download, DownloadError, EXPORTS_PER_HOUR, ExportFile,
    Exports,
};
pub use spec::{MAX_TRACE_IDS, Options, Selector, Unreadable};
pub use stream::{CHUNK_BYTES, ExportWriter, Pages, Progress, Summary, WriteError};

/// The control plane's export types, for the crates that read exports through this one.
pub use crucible_control::{ExportFormat, ExportJob, TxError};
