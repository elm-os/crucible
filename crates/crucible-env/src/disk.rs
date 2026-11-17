//! Files under named roots (contract 2.4).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;

/// A named directory tree. Every path is relative to one of these.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Root {
    /// WAL segments, the epoch file and the checkpoint hint.
    Wal,
    /// Quarantined batches.
    Quarantine,
    /// Export job output.
    Exports,
    /// UI assets served from a directory.
    Ui,
}

impl Root {
    /// The root's name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wal => "wal",
            Self::Quarantine => "quarantine",
            Self::Exports => "exports",
            Self::Ui => "ui",
        }
    }
}

/// A validated path relative to a [`Root`]: `/`-separated components, none of them empty, `.` or `..`, and
/// none containing `\`, `:` or NUL. It can therefore never leave its root on any platform. The empty path is
/// the root directory itself.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DiskPath {
    root: Root,
    rel: String,
}

impl DiskPath {
    /// Validates `rel` and builds the path.
    pub fn new(root: Root, rel: &str) -> Result<Self, DiskError> {
        let invalid = |reason| DiskError::InvalidPath {
            path: format!("{}:{rel}", root.name()),
            reason,
        };
        if !rel.is_empty() {
            for component in rel.split('/') {
                match component {
                    "" => return Err(invalid("empty component, leading or trailing slash")),
                    "." | ".." => return Err(invalid("dot component")),
                    c if c.contains(['\\', ':', '\0']) => {
                        return Err(invalid("reserved character"));
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            root,
            rel: rel.to_owned(),
        })
    }

    /// The root directory itself.
    pub const fn root_dir(root: Root) -> Self {
        Self {
            root,
            rel: String::new(),
        }
    }

    /// The path of `name` (one or more components) inside this directory.
    pub fn join(&self, name: &str) -> Result<Self, DiskError> {
        if self.rel.is_empty() {
            Self::new(self.root, name)
        } else {
            Self::new(self.root, &format!("{}/{name}", self.rel))
        }
    }

    /// The root this path is relative to.
    pub const fn root(&self) -> Root {
        self.root
    }

    /// The `/`-separated relative path; empty for the root directory.
    pub fn as_str(&self) -> &str {
        &self.rel
    }

    /// The path's components, in order.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.rel.split('/').filter(|c| !c.is_empty())
    }
}

impl fmt::Display for DiskPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.root.name(), self.rel)
    }
}

/// One directory entry. Directories are listed with `len` 0.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The entry's name within its directory.
    pub name: String,
    /// The file's length in bytes.
    pub len: u64,
}

/// How [`Disk::open`] opens a file. No mode truncates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenMode {
    /// Read only; the file must exist.
    Read,
    /// Read and write; the file is created when it does not exist.
    Write,
    /// Read and write; fails with [`DiskError::AlreadyExists`] if the entry exists.
    CreateNew,
}

/// A disk failure.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum DiskError {
    /// The entry, or the root, does not exist.
    #[error("not found")]
    NotFound,
    /// `CreateNew` found an existing entry.
    #[error("already exists")]
    AlreadyExists,
    /// The device, or the capacity budget, is full.
    #[error("no space left")]
    NoSpace,
    /// The path failed validation, or the operation does not accept it.
    #[error("invalid path {path}: {reason}")]
    InvalidPath {
        /// The offending path.
        path: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// An earlier `sync_data` on this handle failed or was cancelled; the handle refuses further use.
    #[error("file failed an earlier sync")]
    FileFailed,
    /// Any other IO error.
    #[error("io error ({kind:?}): {message}")]
    Io {
        /// The error's class.
        kind: std::io::ErrorKind,
        /// The backend's description.
        message: String,
    },
}

impl From<std::io::Error> for DiskError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound,
            std::io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => Self::NoSpace,
            kind => Self::Io {
                kind,
                message: e.to_string(),
            },
        }
    }
}

/// The filesystem. Operations on directory entries (`open` that creates, `rename`, `remove`,
/// `create_dir_all`) are durable only after [`Disk::sync_dir`] on the containing directory returns `Ok`.
///
/// Cancel safety: dropping a future of a mutating method leaves it unknown whether the operation happened,
/// exactly as if it had returned an error. Read methods are cancel-safe.
#[async_trait]
pub trait Disk: fmt::Debug + Send + Sync {
    /// Creates `dir` and its missing parents.
    async fn create_dir_all(&self, dir: &DiskPath) -> Result<(), DiskError>;

    /// The entries of `dir`, sorted by name (byte order).
    async fn list(&self, dir: &DiskPath) -> Result<Vec<Entry>, DiskError>;

    /// Opens a file.
    async fn open(&self, path: &DiskPath, mode: OpenMode) -> Result<File, DiskError>;

    /// Makes creates, renames and removes inside `dir` durable.
    async fn sync_dir(&self, dir: &DiskPath) -> Result<(), DiskError>;

    /// Renames within one root, replacing `to` if it exists.
    async fn rename(&self, from: &DiskPath, to: &DiskPath) -> Result<(), DiskError>;

    /// Removes a file or an empty directory.
    async fn remove(&self, path: &DiskPath) -> Result<(), DiskError>;

    /// Bytes available to this process on the root's filesystem.
    async fn free_space(&self, root: Root) -> Result<u64, DiskError>;
}

/// The per-file operations a [`Disk`] backend implements. Product code uses [`File`], which adds the
/// failed-file rule on top.
#[async_trait]
pub trait FileOps: fmt::Debug + Send + Sync {
    /// See [`File::preallocate`].
    async fn preallocate(&self, len: u64) -> Result<(), DiskError>;
    /// See [`File::append`].
    async fn append(&self, offset: u64, bufs: Vec<Bytes>) -> Result<(), DiskError>;
    /// See [`File::read_at`].
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes, DiskError>;
    /// See [`File::sync_data`].
    async fn sync_data(&self) -> Result<(), DiskError>;
    /// See [`File::truncate`].
    async fn truncate(&self, len: u64) -> Result<(), DiskError>;
}

/// An open file.
///
/// The failed-file rule of contract 2.4 is enforced here for every backend: once [`File::sync_data`] has
/// returned an error, or its future was dropped before completing (the outcome is then unknown, and an OS
/// reports a writeback error only once), every later call on this handle returns
/// [`DiskError::FileFailed`]. The caller must abandon the file; it must not sync again, on this handle or a
/// fresh one, and assume its earlier writes are durable.
#[derive(Debug)]
pub struct File {
    ops: Box<dyn FileOps>,
    failed: AtomicBool,
}

/// Marks the file failed unless the sync completed with `Ok`.
struct SyncGuard<'a> {
    failed: &'a AtomicBool,
    ok: bool,
}

impl Drop for SyncGuard<'_> {
    fn drop(&mut self) {
        if !self.ok {
            self.failed.store(true, Ordering::SeqCst);
        }
    }
}

impl File {
    /// Wraps a backend's file. Called by `Disk` implementations.
    pub fn new(ops: Box<dyn FileOps>) -> Self {
        Self {
            ops,
            failed: AtomicBool::new(false),
        }
    }

    /// Whether the failed-file rule has tripped.
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<(), DiskError> {
        if self.is_failed() {
            Err(DiskError::FileFailed)
        } else {
            Ok(())
        }
    }

    /// Reserves space so that the file is at least `len` bytes long; never shrinks. Reads of unwritten
    /// ranges return zeros.
    pub async fn preallocate(&self, len: u64) -> Result<(), DiskError> {
        self.check()?;
        self.ops.preallocate(len).await
    }

    /// Writes `bufs` back to back starting at `offset`, extending the file as needed. On error, or when the
    /// future is dropped, an unknown prefix may have been written.
    pub async fn append(&self, offset: u64, bufs: Vec<Bytes>) -> Result<(), DiskError> {
        self.check()?;
        self.ops.append(offset, bufs).await
    }

    /// Reads up to `len` bytes at `offset`. The result is shorter than `len` only at end of file.
    pub async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes, DiskError> {
        self.check()?;
        self.ops.read_at(offset, len).await
    }

    /// Makes every byte written before the call durable. Durability holds only after `Ok`. Not cancel-safe:
    /// an error or a dropped future fails the file for good (see the type's documentation).
    pub async fn sync_data(&self) -> Result<(), DiskError> {
        self.check()?;
        let mut guard = SyncGuard {
            failed: &self.failed,
            ok: false,
        };
        let result = self.ops.sync_data().await;
        guard.ok = result.is_ok();
        result
    }

    /// Sets the file's length. Durable after the next `sync_data`.
    pub async fn truncate(&self, len: u64) -> Result<(), DiskError> {
        self.check()?;
        self.ops.truncate(len).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    use futures::FutureExt;
    use futures::executor::block_on;
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn paths_accept_plain_relative_components() {
        let path = DiskPath::new(Root::Wal, "segments/0000000001.wal").unwrap();
        assert_eq!(path.root(), Root::Wal);
        assert_eq!(path.as_str(), "segments/0000000001.wal");
        assert_eq!(
            path.components().collect::<Vec<_>>(),
            ["segments", "0000000001.wal"]
        );
        assert_eq!(path.to_string(), "wal:segments/0000000001.wal");
        assert_eq!(
            DiskPath::new(Root::Ui, "").unwrap(),
            DiskPath::root_dir(Root::Ui)
        );
        assert_eq!(DiskPath::root_dir(Root::Ui).components().count(), 0);
    }

    #[test]
    fn join_validates_and_nests() {
        let root = DiskPath::root_dir(Root::Exports);
        let dir = root.join("jobs").unwrap();
        assert_eq!(dir.join("a.csv").unwrap().as_str(), "jobs/a.csv");
        assert!(dir.join("../escape").is_err());
        assert!(dir.join("").is_err());
    }

    #[test]
    fn paths_that_could_leave_the_root_are_refused() {
        for bad in [
            "/etc/passwd",
            "..",
            "a/../b",
            "./a",
            "a//b",
            "a/",
            "a\\b",
            "C:/x",
            "a/c:d",
            "a\0b",
            ".",
        ] {
            let err = DiskPath::new(Root::Wal, bad).unwrap_err();
            assert!(
                matches!(err, DiskError::InvalidPath { .. }),
                "{bad:?} gave {err:?}"
            );
        }
    }

    proptest! {
        #[test]
        fn accepted_paths_have_only_ordinary_components(rel in r"[a-z./\\:]{0,12}") {
            if let Ok(path) = DiskPath::new(Root::Wal, &rel) {
                prop_assert_eq!(path.components().collect::<Vec<_>>().join("/"), rel);
                for c in path.components() {
                    prop_assert!(c != "." && c != ".." && !c.contains(['\\', ':']));
                }
            }
        }
    }

    #[test]
    fn io_errors_map_to_their_classes() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            DiskError::from(Error::from(ErrorKind::NotFound)),
            DiskError::NotFound
        );
        assert_eq!(
            DiskError::from(Error::from(ErrorKind::AlreadyExists)),
            DiskError::AlreadyExists
        );
        assert_eq!(
            DiskError::from(Error::from(ErrorKind::StorageFull)),
            DiskError::NoSpace
        );
        assert!(matches!(
            DiskError::from(Error::from(ErrorKind::PermissionDenied)),
            DiskError::Io {
                kind: ErrorKind::PermissionDenied,
                ..
            }
        ));
    }

    /// A file whose sync fails on call number `fail_on` (0-based) and never completes when `hang_sync` is
    /// set.
    #[derive(Debug, Default)]
    struct ScriptedFile {
        syncs: AtomicU32,
        fail_on: Option<u32>,
        hang_sync: bool,
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl FileOps for ScriptedFile {
        async fn preallocate(&self, _len: u64) -> Result<(), DiskError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn append(&self, _offset: u64, _bufs: Vec<Bytes>) -> Result<(), DiskError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn read_at(&self, _offset: u64, _len: usize) -> Result<Bytes, DiskError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Bytes::new())
        }
        async fn sync_data(&self) -> Result<(), DiskError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.hang_sync {
                futures::future::pending::<()>().await;
            }
            if Some(self.syncs.fetch_add(1, Ordering::SeqCst)) == self.fail_on {
                return Err(DiskError::Io {
                    kind: std::io::ErrorKind::Other,
                    message: "EIO".into(),
                });
            }
            Ok(())
        }
        async fn truncate(&self, _len: u64) -> Result<(), DiskError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn assert_refuses_everything(file: &File, calls: &AtomicU32) {
        let before = calls.load(Ordering::SeqCst);
        assert!(file.is_failed());
        assert_eq!(block_on(file.sync_data()), Err(DiskError::FileFailed));
        assert_eq!(
            block_on(file.append(0, vec![Bytes::from_static(b"x")])),
            Err(DiskError::FileFailed)
        );
        assert_eq!(block_on(file.read_at(0, 1)), Err(DiskError::FileFailed));
        assert_eq!(block_on(file.truncate(0)), Err(DiskError::FileFailed));
        assert_eq!(block_on(file.preallocate(1)), Err(DiskError::FileFailed));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before,
            "a failed file must not reach the backend"
        );
    }

    #[test]
    fn a_healthy_file_passes_calls_through() {
        let calls = Arc::new(AtomicU32::new(0));
        let file = File::new(Box::new(ScriptedFile {
            calls: Arc::clone(&calls),
            ..Default::default()
        }));
        block_on(async {
            file.preallocate(8).await.unwrap();
            file.append(0, vec![Bytes::from_static(b"ab")])
                .await
                .unwrap();
            file.sync_data().await.unwrap();
            file.sync_data().await.unwrap();
            file.truncate(1).await.unwrap();
            assert_eq!(file.read_at(0, 1).await.unwrap(), Bytes::new());
        });
        assert!(!file.is_failed());
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn a_failed_sync_fails_the_file_for_good() {
        let calls = Arc::new(AtomicU32::new(0));
        let ops = ScriptedFile {
            fail_on: Some(1),
            calls: Arc::clone(&calls),
            ..Default::default()
        };
        let file = File::new(Box::new(ops));
        assert_eq!(block_on(file.sync_data()), Ok(()));
        assert!(matches!(
            block_on(file.sync_data()),
            Err(DiskError::Io { .. })
        ));
        // The backend would report success on a retry; the wrapper must not let the caller see that.
        assert_refuses_everything(&file, &calls);
    }

    #[test]
    fn a_cancelled_sync_fails_the_file() {
        let calls = Arc::new(AtomicU32::new(0));
        let ops = ScriptedFile {
            hang_sync: true,
            calls: Arc::clone(&calls),
            ..Default::default()
        };
        let file = File::new(Box::new(ops));
        assert!(file.sync_data().now_or_never().is_none());
        assert_refuses_everything(&file, &calls);
    }
}
