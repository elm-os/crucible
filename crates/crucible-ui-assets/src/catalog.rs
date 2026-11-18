//! The in-memory catalog of an export's files.

use std::collections::BTreeMap;
use std::fmt;

use bytes::{Bytes, BytesMut};
use crucible_env::{
    Asset, AssetManifest, AssetSource, Disk, DiskError, DiskPath, Encodings, OpenMode, Root,
};
use sha2::{Digest, Sha256};

use crate::csp;

/// The largest single read when loading a directory.
const READ_CHUNK: usize = 4 << 20;

/// An export's files, each with its ETag, media type and variants, plus the manifest computed over them.
#[derive(Clone, Default)]
pub struct Catalog {
    assets: BTreeMap<String, Asset>,
    manifest: AssetManifest,
}

impl fmt::Debug for Catalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Catalog")
            .field("files", &self.assets.len())
            .field("routes", &self.manifest.routes)
            .finish()
    }
}

/// The export written by `pnpm --filter crucible-ui build`, embedded at compile time.
#[cfg(feature = "embed-ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/out/"]
struct Export;

impl Catalog {
    /// The source compiled into this build: the embedded export with the `embed-ui` feature.
    #[cfg(feature = "embed-ui")]
    pub fn built_in() -> Self {
        Self::from_embed::<Export>()
    }

    /// The source compiled into this build: none, because the `embed-ui` feature is off. Every page answers
    /// 404 with a note on how to build the UI.
    #[cfg(not(feature = "embed-ui"))]
    pub fn built_in() -> Self {
        Self::default()
    }

    /// Loads the export in the `ui` disk root (`--ui-dir`). Fails with [`DiskError::NotFound`] when the root
    /// has no `index.html`, before reading anything else: a mistyped directory may be a whole source tree.
    pub async fn from_dir(disk: &dyn Disk) -> Result<Self, DiskError> {
        let root = DiskPath::root_dir(Root::Ui);
        disk.open(&root.join("index.html")?, OpenMode::Read).await?;
        let mut files = Vec::new();
        let mut dirs = vec![root];
        while let Some(dir) = dirs.pop() {
            for entry in disk.list(&dir).await? {
                let path = dir.join(&entry.name)?;
                // Directories are listed with length 0, and so are empty files, which cannot be listed.
                if entry.len == 0 && disk.list(&path).await.is_ok() {
                    dirs.push(path);
                } else {
                    let bytes = read(disk, &path, entry.len).await?;
                    files.push((path.as_str().to_owned(), bytes));
                }
            }
        }
        Ok(Self::from_files(files))
    }

    /// Builds the catalog of an embedded folder.
    #[cfg(any(feature = "embed-ui", test))]
    pub(crate) fn from_embed<E: rust_embed::RustEmbed>() -> Self {
        Self::from_files(E::iter().filter_map(|name| {
            let bytes = match E::get(&name)?.data {
                std::borrow::Cow::Borrowed(data) => Bytes::from_static(data),
                std::borrow::Cow::Owned(data) => Bytes::from(data),
            };
            Some((name.into_owned(), bytes))
        }))
    }

    /// Builds the catalog of `(path, bytes)` pairs. A file `F.br` or `F.gz` beside a file `F` is that file's
    /// brotli or gzip variant and is not an asset of its own.
    pub(crate) fn from_files(files: impl IntoIterator<Item = (String, Bytes)>) -> Self {
        let files: BTreeMap<String, Bytes> = files.into_iter().collect();
        let is_variant = |path: &str| {
            [".br", ".gz"].iter().any(|ext| {
                path.strip_suffix(ext)
                    .is_some_and(|base| files.contains_key(base))
            })
        };
        let mut catalog = Self::default();
        for (path, bytes) in &files {
            if is_variant(path) {
                continue;
            }
            let content_type = content_type(path);
            if content_type.starts_with("text/html") {
                let hashes = csp::script_hashes(bytes);
                catalog.manifest.csp_hashes.insert(path.clone(), hashes);
                if let Some(route) = route(path) {
                    catalog.manifest.routes.push(route);
                }
            }
            let asset = Asset {
                bytes: bytes.clone(),
                etag: etag(bytes),
                content_type,
                encodings: Encodings {
                    br: files.get(&format!("{path}.br")).cloned(),
                    gzip: files.get(&format!("{path}.gz")).cloned(),
                },
            };
            catalog.assets.insert(path.clone(), asset);
        }
        catalog.manifest.routes.sort();
        catalog
    }
}

impl AssetSource for Catalog {
    fn get(&self, path: &str) -> Option<Asset> {
        self.assets.get(path).cloned()
    }

    fn manifest(&self) -> &AssetManifest {
        &self.manifest
    }
}

/// Reads a whole file. `len` is the length the directory listing gave; the loop also covers a file that grew.
async fn read(disk: &dyn Disk, path: &DiskPath, len: u64) -> Result<Bytes, DiskError> {
    let file = disk.open(path, OpenMode::Read).await?;
    let mut bytes = BytesMut::new();
    // One more byte than listed, so that a file of the listed length ends in one short read.
    let mut want =
        usize::try_from(len).map_or(READ_CHUNK, |len| len.saturating_add(1).min(READ_CHUNK));
    loop {
        let chunk = file.read_at(bytes.len() as u64, want).await?;
        bytes.extend_from_slice(&chunk);
        if chunk.len() < want {
            return Ok(bytes.freeze());
        }
        want = READ_CHUNK;
    }
}

/// The strong ETag of `bytes`: the first 16 bytes of their SHA-256, in lowercase hex, quoted.
fn etag(bytes: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let hex: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("\"{hex}\"")
}

/// The route an HTML file serves: `index.html` is `/`, `R/index.html` is `/R/`.
fn route(path: &str) -> Option<String> {
    if path == "index.html" {
        return Some("/".to_owned());
    }
    path.strip_suffix("/index.html")
        .map(|dir| format!("/{dir}/"))
}

/// The media type for a file name. `.txt` must be `text/plain`: the client router treats any other type for
/// an RSC payload as a reason to fall back to a full page load.
fn content_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(rust_embed::RustEmbed)]
    #[folder = "tests/fixtures/out/"]
    struct Fixture;

    fn files(paths: &[&str]) -> Catalog {
        Catalog::from_files(
            paths
                .iter()
                .map(|path| ((*path).to_owned(), Bytes::from(path.as_bytes().to_vec()))),
        )
    }

    #[test]
    fn variants_pair_with_their_file_and_are_not_assets() {
        let catalog = files(&[
            "a.js", "a.js.br", "a.js.gz", "b.css.gz", "c.tar.gz", "c.tar",
        ]);
        let a = catalog.get("a.js").unwrap();
        assert_eq!(a.encodings.br.as_deref(), Some(&b"a.js.br"[..]));
        assert_eq!(a.encodings.gzip.as_deref(), Some(&b"a.js.gz"[..]));
        assert!(catalog.get("a.js.br").is_none());
        assert!(catalog.get("a.js.gz").is_none());
        // Without its base file a `.gz` is an ordinary file.
        assert_eq!(
            catalog.get("b.css.gz").unwrap().content_type,
            "application/octet-stream"
        );
        let tar = catalog.get("c.tar").unwrap();
        assert_eq!(tar.encodings.gzip.as_deref(), Some(&b"c.tar.gz"[..]));
        assert!(tar.encodings.br.is_none());
    }

    #[test]
    fn etags_are_strong_hashes_of_the_bytes() {
        let catalog = Catalog::from_embed::<Fixture>();
        let index = catalog.get("index.html").unwrap();
        assert_eq!(index.etag, etag(&index.bytes));
        assert_eq!(index.etag.len(), 34);
        assert!(index.etag.starts_with('"') && index.etag.ends_with('"'));
        assert!(
            index.etag[1..33]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(index.etag, catalog.get("404.html").unwrap().etag);
        // SHA-256 of the empty input starts e3b0c442 98fc1c14 9afbf4c8 996fb924.
        assert_eq!(etag(b""), "\"e3b0c44298fc1c149afbf4c8996fb924\"");
    }

    #[test]
    fn the_manifest_lists_routes_and_page_hashes() {
        let catalog = Catalog::from_embed::<Fixture>();
        let manifest = catalog.manifest();
        assert_eq!(
            manifest.routes,
            ["/", "/home/", "/mcp/servers/", "/traces/"]
        );
        assert_eq!(
            manifest.csp_hashes.keys().collect::<Vec<_>>(),
            [
                "404.html",
                "home/index.html",
                "index.html",
                "mcp/servers/index.html",
                "traces/index.html"
            ]
        );
        assert!(manifest.csp_hashes.values().all(|hashes| hashes.len() == 3));
    }

    #[test]
    fn media_types_follow_the_extension() {
        for (path, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("traces/index.txt", "text/plain; charset=utf-8"),
            (
                "traces/__next.!KHNoZWxsKQ/traces/__PAGE__.txt",
                "text/plain; charset=utf-8",
            ),
            ("_next/static/chunks/a.js", "text/javascript; charset=utf-8"),
            ("_next/static/chunks/a.css", "text/css; charset=utf-8"),
            ("fonts/a.woff2", "font/woff2"),
            ("icon.SVG", "image/svg+xml"),
            (
                "__next.!KHNoZWxsKQ/traces/__PAGE__",
                "application/octet-stream",
            ),
            ("LICENSE", "application/octet-stream"),
        ] {
            assert_eq!(content_type(path), expected, "{path}");
        }
    }

    #[test]
    fn the_default_catalog_is_empty() {
        let catalog = Catalog::default();
        assert!(catalog.get("index.html").is_none());
        assert_eq!(catalog.manifest(), &AssetManifest::default());
    }

    /// Runs only in an `embed-ui` build, which needs a built `ui/out`.
    #[cfg(feature = "embed-ui")]
    #[test]
    fn the_embedded_export_has_every_page_and_its_hashes() {
        let catalog = Catalog::built_in();
        assert!(catalog.get("404.html").is_some());
        let manifest = catalog.manifest();
        assert!(manifest.routes.len() >= 13, "{:?}", manifest.routes);
        for route in &manifest.routes {
            let page = format!("{}index.html", &route[1..]);
            assert!(catalog.get(&page).is_some(), "{page}");
            assert!(!manifest.csp_hashes[&page].is_empty(), "{page}");
        }
    }
}
