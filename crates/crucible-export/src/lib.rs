//! Exports (contracts 8.7, 11.1, 12.6, 12.8 and 13.4).
//!
//! - **Writers.** [`write_jsonl`] and [`write_otlp`] turn one exported span ([`crucible_query::ExportRecord`])
//!   into its line of the `crucible-jsonl` or `otlp-json` format. Both take the `ProjectScope` the reads were made
//!   with, and both are total: any record gives one line of valid JSON, or, for `otlp-json`, a counted skip.
//! - **Files.** [`ExportWriter`] gathers the lines of one file, gzip-compressed when the options ask for it, and
//!   its SHA-256; [`write_pages`] pages the query service into it.

mod json;
mod jsonl;
mod otlp;
mod spec;
mod stream;

pub use jsonl::{SCHEMA, write_jsonl};
pub use otlp::{Written, write_otlp};
pub use spec::{MAX_TRACE_IDS, Options, Selector, Unreadable};
pub use stream::{CHUNK_BYTES, ExportWriter, Progress, Summary, WriteError, write_pages};
