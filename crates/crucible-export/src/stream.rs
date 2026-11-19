//! From pages of spans to the bytes of an export file: each span becomes its line in the export's format, the
//! lines are gzip-compressed when the options ask for it, and every byte handed out enters the file's SHA-256.

use std::io::Write as _;

use bytes::Bytes;
use crucible_control::ExportFormat;
use crucible_query::{
    ExportLimits, ExportQuery, ExportRecord, QueryService, QueryServiceError, ReadContext,
};
use crucible_records::ProjectScope;
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

use crate::jsonl::write_jsonl;
use crate::otlp::{Written, write_otlp};
use crate::spec::Options;

/// The bytes a writer gathers before it hands a chunk out.
pub const CHUNK_BYTES: usize = 1 << 20;

/// Why an export could not be written.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A read of the export failed.
    #[error("reading the export failed: {0}")]
    Read(#[from] QueryServiceError),
    /// The compressor failed.
    #[error("compressing the export failed: {0}")]
    Encode(#[from] std::io::Error),
    /// Where the bytes go refused them.
    #[error("{0}")]
    Sink(String),
}

/// What a finished export holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Summary {
    /// Spans written.
    pub spans: u64,
    /// Spans the format left out (`otlp-json` skips spans with a cut field unless asked to keep them).
    pub skipped: u64,
    /// The file's length in bytes.
    pub bytes: u64,
    /// SHA-256 of the file, lowercase hex.
    pub sha256: String,
}

/// How far a writer has come.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Progress {
    /// Spans written so far.
    pub spans: u64,
    /// Spans left out so far.
    pub skipped: u64,
    /// Bytes handed out so far.
    pub bytes: u64,
}

/// The writer of one export file.
pub struct ExportWriter {
    format: ExportFormat,
    keep_truncated: bool,
    line: Vec<u8>,
    plain: Vec<u8>,
    gzip: Option<GzEncoder<Vec<u8>>>,
    sha: Sha256,
    progress: Progress,
}

impl std::fmt::Debug for ExportWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportWriter")
            .field("format", &self.format)
            .field("gzip", &self.gzip.is_some())
            .field("progress", &self.progress)
            .finish_non_exhaustive()
    }
}

impl ExportWriter {
    /// A writer of `format` with `options`.
    pub fn new(format: ExportFormat, options: Options) -> Self {
        Self {
            format,
            keep_truncated: options.keep_truncated,
            line: Vec::new(),
            plain: Vec::new(),
            gzip: options
                .gzip
                .then(|| GzEncoder::new(Vec::new(), Compression::default())),
            sha: Sha256::new(),
            progress: Progress::default(),
        }
    }

    /// Writes one span of the export, read under `scope`.
    pub fn push(&mut self, scope: ProjectScope, record: &ExportRecord) -> Result<(), WriteError> {
        self.line.clear();
        let written = match self.format {
            ExportFormat::CrucibleJsonl => {
                write_jsonl(scope, record, &mut self.line);
                Written::Line
            }
            ExportFormat::OtlpJson => write_otlp(scope, record, self.keep_truncated, &mut self.line),
        };
        match written {
            Written::Skipped => self.progress.skipped += 1,
            Written::Line => {
                self.progress.spans += 1;
                match &mut self.gzip {
                    Some(encoder) => encoder.write_all(&self.line)?,
                    None => self.plain.extend_from_slice(&self.line),
                }
            }
        }
        Ok(())
    }

    /// How far the writer has come.
    pub const fn progress(&self) -> Progress {
        self.progress
    }

    fn hand_out(&mut self, chunk: Vec<u8>) -> Bytes {
        self.sha.update(&chunk);
        self.progress.bytes += chunk.len() as u64;
        Bytes::from(chunk)
    }

    /// The bytes ready to be written, when there are at least `at_least` of them.
    pub fn ready(&mut self, at_least: usize) -> Option<Bytes> {
        let buffer = match &mut self.gzip {
            Some(encoder) => encoder.get_mut(),
            None => &mut self.plain,
        };
        if buffer.is_empty() || buffer.len() < at_least {
            return None;
        }
        let chunk = std::mem::take(buffer);
        Some(self.hand_out(chunk))
    }

    /// Ends the file: the last bytes, and what the file holds.
    pub fn finish(mut self) -> Result<(Bytes, Summary), WriteError> {
        let rest = match self.gzip.take() {
            Some(encoder) => encoder.finish()?,
            None => std::mem::take(&mut self.plain),
        };
        let rest = self.hand_out(rest);
        let summary = Summary {
            spans: self.progress.spans,
            skipped: self.progress.skipped,
            bytes: self.progress.bytes,
            sha256: self
                .sha
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        };
        Ok((rest, summary))
    }
}

/// Reads every page of the export and writes it, handing each chunk of at least [`CHUNK_BYTES`] to `sink` with
/// the progress so far. The caller ends the file with [`ExportWriter::finish`].
pub async fn write_pages<S>(
    service: &QueryService,
    ctx: &ReadContext,
    query: &ExportQuery,
    limits: ExportLimits,
    writer: &mut ExportWriter,
    mut sink: S,
) -> Result<(), WriteError>
where
    S: AsyncFnMut(Bytes, Progress) -> Result<(), WriteError>,
{
    let mut after = None;
    loop {
        let page = service.export_page(ctx, query, after, limits).await?;
        for record in &page.records {
            writer.push(ctx.scope(), record)?;
        }
        if let Some(chunk) = writer.ready(CHUNK_BYTES) {
            sink(chunk, writer.progress()).await?;
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(()),
        }
    }
}
