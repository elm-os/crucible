//! The `portable` disk backend and backend selection (contract 2.4).
//!
//! # Threads
//!
//! Each root has one named writer thread (`disk-<root>`, short enough for the 15-byte thread names of
//! Linux). Every mutating operation on the root and on its files runs there, in submission order:
//! creating directories and files, `preallocate`, `append`,
//! `sync_data`, `truncate`, `sync_dir`, `rename` and `remove`. Read operations (`open` for reading,
//! `read_at`, `list`, `free_space`) run on the runtime's blocking pool so they never queue behind a sync.
//! An operation that was submitted runs to completion even if its future is dropped.
//!
//! `append` issues one positional write per buffer, in order; a failure part-way leaves the earlier buffers
//! (and possibly part of the failing one) written, which is the "unknown prefix" of the contract.
//!
//! # Durability, per platform
//!
//! | Operation | Linux | macOS | Windows |
//! |---|---|---|---|
//! | `sync_data` | `fdatasync(2)` | `fcntl(F_FULLFSYNC)`: the standard library issues it for `File::sync_data` on Apple targets, so the drive's write cache is flushed, which plain `fsync(2)` does not do there | `FlushFileBuffers` |
//! | `sync_dir` | `fsync(2)` on a descriptor of the directory | `fcntl(F_FULLFSYNC)` on a descriptor of the directory | Checks that the directory exists and returns `Ok`; see below |
//! | `preallocate` | `fallocate(2)` with mode 0: blocks are reserved and the length extended; unwritten ranges read as zeros | `fcntl(F_PREALLOCATE)`, then the length is extended | `SetFileInformationByHandle(FileAllocationInfo)`, then the length is extended |
//! | `free_space` | `statvfs(3)`: `f_bavail * f_frsize` | the same | `GetDiskFreeSpaceExW`: bytes available to the caller |
//!
//! - Linux: a filesystem without `fallocate` support answers `EOPNOTSUPP`; `preallocate` then only extends
//!   the length (a sparse file), which keeps the zero-read guarantee but reserves nothing, and logs a
//!   warning. A later `append` into such a file can therefore still fail with `NoSpace`.
//! - Windows: a directory handle cannot be flushed through the standard library without unsafe code, and
//!   NTFS commits directory updates through its own journal at a time the caller does not control. `sync_dir`
//!   is therefore not a durability barrier on Windows. Windows is a development platform for Crucible; the
//!   "no acknowledged span is lost" claim is made for Linux and macOS only.
//! - On every platform a failed `sync_data` leaves the page cache in an unknown state; `crucible_env::File`
//!   refuses the handle afterwards (the failed-file rule).
//!
//! # Backends
//!
//! `wal.backend = auto | portable | uring`. The io_uring backend arrives in a later milestone; until then
//! `auto` selects `portable` and reports why, and an explicit `uring` is refused. See [`select_backend`].

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::mpsc;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_env::{Disk, DiskError, DiskPath, Entry, File, FileOps, OpenMode, Root};
use futures::channel::oneshot;

/// The `wal.backend` setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WalBackend {
    /// Use io_uring when it is available, else `portable`.
    #[default]
    Auto,
    /// Standard file APIs on a writer thread per root.
    Portable,
    /// io_uring; startup fails when it is unavailable.
    Uring,
}

impl WalBackend {
    /// The setting's spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Portable => "portable",
            Self::Uring => "uring",
        }
    }
}

impl FromStr for WalBackend {
    type Err = BackendUnavailable;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "portable" => Ok(Self::Portable),
            "uring" => Ok(Self::Uring),
            _ => Err(BackendUnavailable {
                reason: "unknown_backend",
            }),
        }
    }
}

/// A disk backend that can actually be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiskBackend {
    /// [`PortableDisk`].
    Portable,
}

impl DiskBackend {
    /// The backend's name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Portable => "portable",
        }
    }
}

/// The outcome of [`select_backend`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BackendChoice {
    /// What the configuration asked for.
    pub requested: WalBackend,
    /// What will be used.
    pub backend: DiskBackend,
    /// Set when `auto` could not use io_uring: a short machine-readable reason, also exported as the
    /// `fallback_reason` label of `crucible_wal_backend_info`.
    pub fallback_reason: Option<&'static str>,
}

/// The requested backend cannot be used.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("wal backend unavailable: {reason}")]
pub struct BackendUnavailable {
    /// A short machine-readable reason.
    pub reason: &'static str,
}

/// Why io_uring cannot be used by this build. Once the backend exists this is also where the startup probe
/// (ENOSYS, EPERM, a failed O_DIRECT probe) will report.
const fn uring_unavailable() -> &'static str {
    if !cfg!(target_os = "linux") {
        "unsupported_platform"
    } else if !cfg!(feature = "wal-uring") {
        "feature_disabled"
    } else {
        "not_implemented"
    }
}

/// Resolves `wal.backend`: `portable` is always available, `auto` falls back to `portable` with a reason,
/// and an explicit `uring` fails while that backend is unavailable.
pub fn select_backend(requested: WalBackend) -> Result<BackendChoice, BackendUnavailable> {
    match requested {
        WalBackend::Portable => Ok(BackendChoice {
            requested,
            backend: DiskBackend::Portable,
            fallback_reason: None,
        }),
        WalBackend::Auto => Ok(BackendChoice {
            requested,
            backend: DiskBackend::Portable,
            fallback_reason: Some(uring_unavailable()),
        }),
        WalBackend::Uring => Err(BackendUnavailable {
            reason: uring_unavailable(),
        }),
    }
}

type Job = Box<dyn FnOnce() + Send>;

struct RootState {
    dir: PathBuf,
    writer: mpsc::Sender<Job>,
}

/// The portable [`Disk`] backend; see the module documentation.
pub struct PortableDisk {
    runtime: tokio::runtime::Handle,
    roots: BTreeMap<Root, RootState>,
}

impl fmt::Debug for PortableDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let roots: BTreeMap<_, _> = self
            .roots
            .iter()
            .map(|(root, state)| (root.name(), &state.dir))
            .collect();
        f.debug_struct("PortableDisk")
            .field("roots", &roots)
            .finish()
    }
}

fn worker_gone() -> DiskError {
    DiskError::Io {
        kind: io::ErrorKind::BrokenPipe,
        message: "disk worker is gone".to_owned(),
    }
}

/// Runs `f` on a root's writer thread.
async fn on_writer<T, F>(writer: &mpsc::Sender<Job>, f: F) -> Result<T, DiskError>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let job: Job = Box::new(move || {
        // The caller may have stopped waiting; the operation still ran.
        let _unobserved = tx.send(f());
    });
    writer.send(job).map_err(|_| worker_gone())?;
    Ok(rx.await.map_err(|_| worker_gone())??)
}

/// Runs a read-only `f` on the runtime's blocking pool.
async fn off_thread<T, F>(runtime: &tokio::runtime::Handle, f: F) -> Result<T, DiskError>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    Ok(runtime
        .spawn_blocking(f)
        .await
        .map_err(|_| worker_gone())??)
}

impl PortableDisk {
    /// Creates each root directory if needed and starts its writer thread. Blocking; call it at startup.
    pub fn new(
        runtime: tokio::runtime::Handle,
        roots: &BTreeMap<Root, PathBuf>,
    ) -> Result<Self, DiskError> {
        let mut states = BTreeMap::new();
        for (root, dir) in roots {
            fs::create_dir_all(dir)?;
            let (writer, jobs) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name(format!("disk-{}", root.name()))
                .spawn(move || {
                    // Ends when the disk, and every file opened from it, has been dropped.
                    for job in jobs {
                        job();
                    }
                })?;
            states.insert(
                *root,
                RootState {
                    dir: dir.clone(),
                    writer,
                },
            );
        }
        Ok(Self {
            runtime,
            roots: states,
        })
    }

    fn root(&self, root: Root) -> Result<&RootState, DiskError> {
        self.roots.get(&root).ok_or(DiskError::NotFound)
    }

    fn locate(&self, path: &DiskPath) -> Result<(&RootState, PathBuf), DiskError> {
        let state = self.root(path.root())?;
        let mut full = state.dir.clone();
        full.extend(path.components());
        Ok((state, full))
    }
}

#[async_trait]
impl Disk for PortableDisk {
    async fn create_dir_all(&self, dir: &DiskPath) -> Result<(), DiskError> {
        let (state, full) = self.locate(dir)?;
        on_writer(&state.writer, move || fs::create_dir_all(full)).await
    }

    async fn list(&self, dir: &DiskPath) -> Result<Vec<Entry>, DiskError> {
        let (_, full) = self.locate(dir)?;
        off_thread(&self.runtime, move || {
            let mut entries = Vec::new();
            for entry in fs::read_dir(full)? {
                let entry = entry?;
                let name = entry.file_name().into_string().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "file name is not UTF-8")
                })?;
                let metadata = entry.metadata()?;
                entries.push(Entry {
                    name,
                    len: if metadata.is_file() {
                        metadata.len()
                    } else {
                        0
                    },
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(entries)
        })
        .await
    }

    async fn open(&self, path: &DiskPath, mode: OpenMode) -> Result<File, DiskError> {
        let (state, full) = self.locate(path)?;
        let file = match mode {
            OpenMode::Read => off_thread(&self.runtime, move || fs::File::open(full)).await?,
            OpenMode::Write => {
                let open = move || {
                    fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(full)
                };
                on_writer(&state.writer, open).await?
            }
            OpenMode::CreateNew => {
                let open = move || {
                    fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(full)
                };
                on_writer(&state.writer, open).await?
            }
        };
        Ok(File::new(Box::new(PortableFile {
            runtime: self.runtime.clone(),
            file: Arc::new(file),
            writer: state.writer.clone(),
        })))
    }

    async fn sync_dir(&self, dir: &DiskPath) -> Result<(), DiskError> {
        let (state, full) = self.locate(dir)?;
        on_writer(&state.writer, move || sync_dir(&full)).await
    }

    async fn rename(&self, from: &DiskPath, to: &DiskPath) -> Result<(), DiskError> {
        if from.root() != to.root() {
            return Err(DiskError::InvalidPath {
                path: to.to_string(),
                reason: "rename across roots",
            });
        }
        let (state, from) = self.locate(from)?;
        let (_, to) = self.locate(to)?;
        on_writer(&state.writer, move || fs::rename(from, to)).await
    }

    async fn remove(&self, path: &DiskPath) -> Result<(), DiskError> {
        let (state, full) = self.locate(path)?;
        on_writer(&state.writer, move || {
            if fs::symlink_metadata(&full)?.is_dir() {
                fs::remove_dir(full)
            } else {
                fs::remove_file(full)
            }
        })
        .await
    }

    async fn free_space(&self, root: Root) -> Result<u64, DiskError> {
        let dir = self.root(root)?.dir.clone();
        off_thread(&self.runtime, move || fs4::available_space(dir)).await
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(dir: &Path) -> io::Result<()> {
    // Not a durability barrier on this platform; see the module documentation.
    if fs::metadata(dir)?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::from(io::ErrorKind::NotADirectory))
    }
}

#[cfg(unix)]
fn pread(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(unix)]
fn pwrite(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

#[cfg(windows)]
fn pread(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(windows)]
fn pwrite(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

fn write_all_at(file: &fs::File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match pwrite(file, buf, offset) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reads until `len` bytes or end of file.
fn read_full_at(file: &fs::File, offset: u64, len: usize) -> io::Result<Bytes> {
    let mut buf = vec![0; len];
    let mut filled = 0;
    while filled < len {
        match pread(file, &mut buf[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(Bytes::from(buf))
}

fn preallocate(file: &fs::File, len: u64) -> io::Result<()> {
    match fs4::FileExt::allocate(file, len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            tracing::warn!("filesystem cannot reserve space; preallocate only extends the file");
        }
        Err(e) => return Err(e),
    }
    // The reservation call may leave the length short when the blocks were already allocated.
    if file.metadata()?.len() < len {
        file.set_len(len)
    } else {
        Ok(())
    }
}

struct PortableFile {
    runtime: tokio::runtime::Handle,
    file: Arc<fs::File>,
    writer: mpsc::Sender<Job>,
}

impl fmt::Debug for PortableFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortableFile")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl FileOps for PortableFile {
    async fn preallocate(&self, len: u64) -> Result<(), DiskError> {
        let file = Arc::clone(&self.file);
        on_writer(&self.writer, move || preallocate(&file, len)).await
    }

    async fn append(&self, offset: u64, bufs: Vec<Bytes>) -> Result<(), DiskError> {
        let file = Arc::clone(&self.file);
        on_writer(&self.writer, move || {
            let mut at = offset;
            for buf in &bufs {
                write_all_at(&file, buf, at)?;
                at += buf.len() as u64;
            }
            Ok(())
        })
        .await
    }

    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes, DiskError> {
        let file = Arc::clone(&self.file);
        off_thread(&self.runtime, move || read_full_at(&file, offset, len)).await
    }

    async fn sync_data(&self) -> Result<(), DiskError> {
        let file = Arc::clone(&self.file);
        on_writer(&self.writer, move || file.sync_data()).await
    }

    async fn truncate(&self, len: u64) -> Result<(), DiskError> {
        let file = Arc::clone(&self.file);
        on_writer(&self.writer, move || file.set_len(len)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_is_always_selected_without_a_reason() {
        let choice = select_backend(WalBackend::Portable).unwrap();
        assert_eq!(choice.backend, DiskBackend::Portable);
        assert_eq!(choice.fallback_reason, None);
    }

    #[test]
    fn auto_falls_back_to_portable_and_says_why() {
        let choice = select_backend(WalBackend::Auto).unwrap();
        assert_eq!(
            (choice.requested, choice.backend),
            (WalBackend::Auto, DiskBackend::Portable)
        );
        let expected = if !cfg!(target_os = "linux") {
            "unsupported_platform"
        } else if cfg!(feature = "wal-uring") {
            "not_implemented"
        } else {
            "feature_disabled"
        };
        assert_eq!(choice.fallback_reason, Some(expected));
    }

    #[test]
    fn an_explicit_uring_request_is_refused_while_unavailable() {
        let err = select_backend(WalBackend::Uring).unwrap_err();
        assert_eq!(
            Some(err.reason),
            select_backend(WalBackend::Auto).unwrap().fallback_reason
        );
    }

    #[test]
    fn settings_parse_and_print() {
        for backend in [WalBackend::Auto, WalBackend::Portable, WalBackend::Uring] {
            assert_eq!(backend.as_str().parse::<WalBackend>(), Ok(backend));
        }
        assert!("io_uring".parse::<WalBackend>().is_err());
        assert_eq!(WalBackend::default(), WalBackend::Auto);
    }
}
