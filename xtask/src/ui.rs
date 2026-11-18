//! `cargo xtask ui compress`: the precompressed variants of a UI export.
//!
//! For each file whose type compresses, writes `<file>.br` (brotli, quality 11, window 2^22) and `<file>.gz`
//! (gzip, level 9, no file name, modification time 0, OS byte 255) beside it when the variant is smaller,
//! and removes a variant left from an earlier run when it is not. The output depends only on the input bytes,
//! so a second run changes nothing and every machine writes the same files.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Text formats. Fonts and images are compressed already.
const COMPRESSIBLE: [&str; 9] = [
    "html",
    "txt",
    "js",
    "mjs",
    "css",
    "json",
    "map",
    "svg",
    "webmanifest",
];

/// What one run did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Compressible files seen.
    pub files: usize,
    /// Their total size.
    pub bytes: u64,
    /// Brotli variants written, and their total size.
    pub br: (usize, u64),
    /// Gzip variants written, and their total size.
    pub gz: (usize, u64),
}

/// Compresses every compressible file under `dir`, in path order.
pub fn compress_dir(dir: &Path) -> io::Result<Summary> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    files.sort();
    let mut summary = Summary::default();
    for file in files.iter().filter(|file| is_compressible(file)) {
        let data = fs::read(file)?;
        summary.files += 1;
        summary.bytes += data.len() as u64;
        for (ext, variant, count) in [
            ("br", brotli(&data)?, &mut summary.br),
            ("gz", gzip(&data)?, &mut summary.gz),
        ] {
            let path = with_extension(file, ext);
            if variant.len() < data.len() {
                fs::write(&path, &variant)?;
                count.0 += 1;
                count.1 += variant.len() as u64;
            } else if path.exists() {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(summary)
}

/// Whether `path` is a compressible file, as opposed to a variant or an already compressed format.
pub fn is_compressible(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| COMPRESSIBLE.contains(&ext))
}

/// `path` with `.ext` appended to its full name.
pub fn with_extension(path: &Path, ext: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// Brotli at quality 11 with a 4 MiB window.
pub fn brotli(data: &[u8]) -> io::Result<Vec<u8>> {
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 22,
        ..Default::default()
    };
    let mut out = Vec::new();
    brotli::BrotliCompress(&mut &data[..], &mut out, &params)?;
    Ok(out)
}

/// Gzip at level 9 with a fixed header: no name, modification time 0, OS byte 255 (unknown).
pub fn gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(Vec::new(), flate2::Compression::best());
    encoder.write_all(data)?;
    encoder.finish()
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/crucible-ui-assets/tests/fixtures/out")
    }

    fn fixture_files() -> Vec<PathBuf> {
        let mut files = Vec::new();
        walk(&fixture(), &mut files).unwrap();
        files.sort();
        files
    }

    #[test]
    fn variants_decode_to_the_input_and_never_change() {
        let data = fs::read(fixture().join("traces/index.html")).unwrap();
        let br = brotli(&data).unwrap();
        let gz = gzip(&data).unwrap();
        assert_eq!(br, brotli(&data).unwrap());
        assert_eq!(gz, gzip(&data).unwrap());
        assert!(br.len() < gz.len() && gz.len() < data.len());

        let mut decoded = Vec::new();
        brotli::BrotliDecompress(&mut &br[..], &mut decoded).unwrap();
        assert_eq!(decoded, data);
        decoded.clear();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, data);
        // Magic, deflate, no flags, mtime 0, best-compression flag, OS unknown.
        assert_eq!(gz[..10], [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 2, 255]);
    }

    /// The committed fixture variants are exactly what this step writes today, so a run on any machine
    /// reproduces them byte for byte. A compressor upgrade may change them; then regenerate them.
    #[test]
    fn the_committed_fixture_variants_are_reproduced() {
        const REGENERATE: &str = "regenerate with `cargo xtask ui compress crates/crucible-ui-assets/tests/fixtures/out`";
        let files = fixture_files();
        let mut checked = 0;
        for file in files.iter().filter(|file| is_compressible(file)) {
            let data = fs::read(file).unwrap();
            for (ext, expected) in [("br", brotli(&data).unwrap()), ("gz", gzip(&data).unwrap())] {
                let path = with_extension(file, ext);
                if expected.len() < data.len() {
                    let committed = fs::read(&path).unwrap_or_default();
                    assert!(
                        committed == expected,
                        "{} differs; {REGENERATE}",
                        path.display()
                    );
                    checked += 1;
                } else {
                    assert!(
                        !path.exists(),
                        "{} should not exist; {REGENERATE}",
                        path.display()
                    );
                }
            }
        }
        assert!(checked >= 20, "only {checked} variants checked");
        // No variant without its file, and none for a font.
        for file in &files {
            let name = file.to_string_lossy();
            if let Some(base) = name
                .strip_suffix(".br")
                .or_else(|| name.strip_suffix(".gz"))
            {
                assert!(is_compressible(Path::new(base)), "{name}");
                assert!(Path::new(base).exists(), "{name}");
            }
        }
    }
}
