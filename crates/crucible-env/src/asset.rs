//! The UI's static files (contract 2.13).

use std::collections::BTreeMap;
use std::fmt;

use bytes::Bytes;

/// One file of the UI export.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Asset {
    /// The file's bytes.
    pub bytes: Bytes,
    /// The strong entity tag of `bytes`, quotes included. A precompressed variant's tag is derived from it by
    /// the serving code.
    pub etag: String,
    /// The media type, from the file's extension.
    pub content_type: &'static str,
    /// Precompressed variants, each decoding to `bytes`.
    pub encodings: Encodings,
}

/// The precompressed variants of an [`Asset`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Encodings {
    /// Brotli.
    pub br: Option<Bytes>,
    /// Gzip.
    pub gzip: Option<Bytes>,
}

/// What a source knows about its pages.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct AssetManifest {
    /// The page routes, each ending in `/` (`/`, `/traces/`), in byte order.
    pub routes: Vec<String>,
    /// For each HTML file, by path, the hash sources (`sha256-<base64>`) of its inline scripts, sorted and
    /// deduplicated: a page's `Content-Security-Policy` allows exactly these.
    pub csp_hashes: BTreeMap<String, Vec<String>>,
}

/// The files of the UI export: embedded at build time, read from a directory through `Disk`, or none. The
/// serving code is the same for every source.
///
/// Paths are relative, `/`-separated and have no leading slash (`traces/index.html`).
pub trait AssetSource: fmt::Debug + Send + Sync {
    /// The file at `path`, if the export has it.
    fn get(&self, path: &str) -> Option<Asset>;

    /// The export's routes and per-page CSP hashes.
    fn manifest(&self) -> &AssetManifest;
}
