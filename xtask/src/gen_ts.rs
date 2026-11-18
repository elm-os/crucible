//! `cargo xtask gen-ts`: the TypeScript types of the REST API, from the DTOs of `crucible-api-types`.
//!
//! Writes one file per type to `ui/src/api/generated/`, after removing what an earlier run wrote, so a type
//! that no longer exists leaves no file behind. The output depends only on the Rust types; CI regenerates it
//! and fails on any difference from the committed files (`.github/workflows/drift.yml`).

use std::fs;
use std::path::{Path, PathBuf};

use ts_rs::{Config, ExportError, TS, TypeVisitor};

/// The output directory, relative to the repository root.
pub const OUT_DIR: &str = "ui/src/api/generated";

/// Exports each visited type with the types it uses.
struct Export<'a> {
    cfg: &'a Config,
    result: Result<(), ExportError>,
}

impl TypeVisitor for Export<'_> {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        if self.result.is_ok() {
            self.result = T::export_all(self.cfg);
        }
    }
}

/// The repository root: the parent of this crate's directory.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Regenerates the types and returns the directory and the number of files written.
pub fn generate() -> Result<(PathBuf, usize), String> {
    let dir = repository_root().join(OUT_DIR);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("removing {}: {e}", dir.display()))?;
    }
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let cfg = Config::new().with_out_dir(&dir);
    let mut export = Export {
        cfg: &cfg,
        result: Ok(()),
    };
    crucible_api_types::visit_exported(&mut export);
    export.result.map_err(|e| format!("exporting: {e}"))?;
    let files = fs::read_dir(&dir)
        .map_err(|e| format!("listing {}: {e}", dir.display()))?
        .count();
    Ok((dir, files))
}
