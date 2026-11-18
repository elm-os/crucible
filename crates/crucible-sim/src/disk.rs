//! `SimDisk`: an in-memory [`Disk`] with the fault model of contract section 2.4.
//!
//! - Every file has a current image (what reads see) and a durable image (what survives a crash). Blocks
//!   of 4,096 bytes written since the last successful sync are *dirty*.
//! - A crash keeps, per file, none of the dirty blocks, all of them, or a seeded subset (a torn write);
//!   the same holds for a pending change of the file's length.
//! - A failed sync, and a lying one, *drop* the dirty state: those blocks are lost at the next crash and
//!   no later sync rescues them, unless they are written again. This is what makes the failed-file rule
//!   necessary.
//! - Directory entries created, renamed or removed since the last `sync_dir` are pending; a crash keeps
//!   none of them, all of them, a seeded prefix or a seeded subset.
//! - Writes can fail after a prefix was written (EIO, short write), reads can fail or return one flipped
//!   bit, syncs can fail, lie or take seeded virtual time, and a capacity budget yields ENOSPC.
//!
//! Each fault type draws from its own forked stream, so switching one on never moves another's draws.
//! Every operation is an await point, is counted, and is fed to the run's [`Trace`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_env::{Disk, DiskError, DiskPath, Entry, File, FileOps, OpenMode, Rng, Root};

use crate::trace::Trace;

/// The granularity of torn writes.
pub const BLOCK: usize = 4096;

const PPM: u64 = 1_000_000;

/// Which faults a [`SimDisk`] injects. The default injects none.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiskFaults {
    /// Chance per write, in parts per million, of EIO after an unknown prefix was written.
    pub write_error_ppm: u32,
    /// Chance per write of a short write: a prefix is written and the call fails.
    pub short_write_ppm: u32,
    /// Chance per read of EIO.
    pub read_error_ppm: u32,
    /// Chance per non-empty read of one flipped bit in the returned bytes.
    pub bit_flip_ppm: u32,
    /// Chance per sync of EIO. The file's dirty state is dropped.
    pub sync_error_ppm: u32,
    /// Chance per sync of a lie: `Ok` is returned and the dirty state is dropped. Stress tier only.
    pub fsync_lie_ppm: u32,
    /// Sync latency in virtual microseconds, drawn uniformly from this inclusive range.
    pub sync_latency_micros: (u64, u64),
    /// Total bytes the disk holds; exceeding it yields ENOSPC. `None` is unlimited.
    pub capacity_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct Inode {
    /// What reads see.
    data: Vec<u8>,
    /// What survives a crash.
    durable: Vec<u8>,
    /// Blocks changed since the last successful sync.
    dirty: BTreeSet<usize>,
    /// Blocks whose dirty state a failed or lying sync dropped.
    dropped: BTreeSet<usize>,
}

fn block_range(block: usize, len: usize) -> std::ops::Range<usize> {
    let start = (block * BLOCK).min(len);
    start..((block + 1) * BLOCK).min(len)
}

impl Inode {
    fn touch(&mut self, start: usize, end: usize) {
        if start < end {
            for block in start / BLOCK..=(end - 1) / BLOCK {
                self.dropped.remove(&block);
                self.dirty.insert(block);
            }
        }
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) {
        let end = offset + bytes.len();
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[offset..end].copy_from_slice(bytes);
        self.touch(offset, end);
    }

    fn set_len(&mut self, len: usize) {
        let old = self.data.len();
        self.data.resize(len, 0);
        self.touch(old.min(len), old.max(len));
    }

    /// Copies one block of the current image into the durable image, as far as both reach.
    fn persist_block(&mut self, block: usize) {
        let range = block_range(block, self.data.len().min(self.durable.len()));
        self.durable[range.clone()].copy_from_slice(&self.data[range]);
    }

    fn sync(&mut self) {
        self.durable.resize(self.data.len(), 0);
        for block in std::mem::take(&mut self.dirty) {
            self.persist_block(block);
        }
    }

    fn drop_dirty(&mut self) {
        let dirty = std::mem::take(&mut self.dirty);
        self.dropped.extend(dirty);
    }

    fn crash(&mut self, rng: &dyn Rng) {
        match rng.next_u64() % 4 {
            // Nothing that was not synced survives.
            0 => {}
            // Everything dirty reached the disk.
            1 => self.sync(),
            // Torn: the length change and each dirty block survive independently.
            _ => {
                if rng.next_u64().is_multiple_of(2) {
                    self.durable.resize(self.data.len(), 0);
                }
                for block in std::mem::take(&mut self.dirty) {
                    if rng.next_u64().is_multiple_of(2) {
                        self.persist_block(block);
                    }
                }
            }
        }
        self.dirty.clear();
        self.dropped.clear();
        self.data.clone_from(&self.durable);
    }
}

#[derive(Clone, Debug)]
enum DirOp {
    Link(DiskPath, u64),
    Unlink(DiskPath),
    Rename(DiskPath, DiskPath),
}

impl DirOp {
    fn dir(&self) -> DiskPath {
        match self {
            Self::Link(path, _) | Self::Unlink(path) | Self::Rename(_, path) => parent(path),
        }
    }

    fn apply(&self, names: &mut BTreeMap<DiskPath, u64>) {
        match self {
            Self::Link(path, inode) => {
                names.insert(path.clone(), *inode);
            }
            Self::Unlink(path) => {
                names.remove(path);
            }
            Self::Rename(from, to) => {
                if let Some(inode) = names.remove(from) {
                    names.insert(to.clone(), inode);
                }
            }
        }
    }
}

fn parent(path: &DiskPath) -> DiskPath {
    let rel = path.as_str();
    let dir = rel.rsplit_once('/').map_or("", |(dir, _)| dir);
    DiskPath::new(path.root(), dir).unwrap_or_else(|_| DiskPath::root_dir(path.root()))
}

fn file_name(path: &DiskPath) -> &str {
    let rel = path.as_str();
    rel.rsplit_once('/').map_or(rel, |(_, name)| name)
}

#[derive(Clone, Debug, Default)]
struct State {
    /// Bumped by every crash; handles of an earlier generation are dead.
    generation: u64,
    /// Set from the crash point until the crash is applied: no operation completes or has any effect.
    frozen: bool,
    inodes: BTreeMap<u64, Inode>,
    next_inode: u64,
    names: BTreeMap<DiskPath, u64>,
    durable_names: BTreeMap<DiskPath, u64>,
    pending_dir_ops: Vec<DirOp>,
    dirs: BTreeSet<DiskPath>,
    faults: DiskFaults,
    ops: u64,
    crash_at_op: Option<u64>,
}

impl State {
    fn used_bytes(&self) -> u64 {
        self.names
            .values()
            .filter_map(|inode| self.inodes.get(inode))
            .map(|inode| inode.data.len() as u64)
            .sum()
    }

    /// Whether growing a file from `old_len` to `new_len` fits the capacity budget.
    fn fits(&self, old_len: usize, new_len: usize) -> bool {
        let Some(capacity) = self.faults.capacity_bytes else {
            return true;
        };
        new_len <= old_len || self.used_bytes() + (new_len - old_len) as u64 <= capacity
    }

    fn dir_exists(&self, dir: &DiskPath) -> bool {
        dir.as_str().is_empty() || self.dirs.contains(dir)
    }

    fn dir_op(&mut self, op: DirOp) {
        op.apply(&mut self.names);
        self.pending_dir_ops.push(op);
    }
}

struct Streams {
    write_error: Arc<dyn Rng>,
    short_write: Arc<dyn Rng>,
    prefix: Arc<dyn Rng>,
    read_error: Arc<dyn Rng>,
    bit_flip: Arc<dyn Rng>,
    sync_error: Arc<dyn Rng>,
    fsync_lie: Arc<dyn Rng>,
    sync_latency: Arc<dyn Rng>,
    crash: Arc<dyn Rng>,
}

struct Shared {
    state: Mutex<State>,
    streams: Streams,
    trace: Arc<Trace>,
    crash_point: tokio::sync::Notify,
}

fn io_error(kind: std::io::ErrorKind, message: &str) -> DiskError {
    DiskError::Io {
        kind,
        message: message.to_owned(),
    }
}

fn hit(rng: &dyn Rng, ppm: u32) -> bool {
    ppm > 0 && rng.next_u64() % PPM < u64::from(ppm)
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts one operation: an await point, the crash point check and the trace record. Returns the
    /// generation the operation runs in.
    async fn begin(&self, op: &'static str, a: u64, b: u64) -> u64 {
        tokio::task::yield_now().await;
        let generation = {
            let mut state = self.lock();
            if !state.frozen {
                state.ops += 1;
                if state.crash_at_op == Some(state.ops) {
                    state.frozen = true;
                    state.crash_at_op = None;
                    self.crash_point.notify_one();
                }
            }
            if state.frozen {
                None
            } else {
                self.trace.event(op, &[state.ops, a, b]);
                Some(state.generation)
            }
        };
        match generation {
            Some(generation) => generation,
            // The host is crashing: this operation never completes. Its task is dropped by the crash.
            None => futures::future::pending().await,
        }
    }

    /// The live state for an operation that began in `generation`; never returns once the disk is frozen.
    async fn live(&self, generation: u64) -> Result<MutexGuard<'_, State>, DiskError> {
        let frozen = {
            let state = self.lock();
            if state.generation != generation {
                return Err(io_error(
                    std::io::ErrorKind::BrokenPipe,
                    "handle predates a crash",
                ));
            }
            state.frozen
        };
        if frozen {
            futures::future::pending::<()>().await;
        }
        Ok(self.lock())
    }
}

/// The simulated disk. Cloning gives another handle to the same disk.
#[derive(Clone)]
pub struct SimDisk {
    shared: Arc<Shared>,
}

impl fmt::Debug for SimDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.shared.lock();
        f.debug_struct("SimDisk")
            .field("generation", &state.generation)
            .field("files", &state.names.len())
            .field("ops", &state.ops)
            .finish_non_exhaustive()
    }
}

impl SimDisk {
    /// Creates an empty disk without faults. Its fault and crash streams are forks of `rng` under
    /// `disk/<fault>` labels.
    pub fn new(rng: &dyn Rng, trace: Arc<Trace>) -> Self {
        Self::with_state(rng, trace, State::default())
    }

    fn with_state(rng: &dyn Rng, trace: Arc<Trace>, state: State) -> Self {
        let streams = Streams {
            write_error: rng.fork("disk/write_error"),
            short_write: rng.fork("disk/short_write"),
            prefix: rng.fork("disk/written_prefix"),
            read_error: rng.fork("disk/read_error"),
            bit_flip: rng.fork("disk/bit_flip"),
            sync_error: rng.fork("disk/sync_error"),
            fsync_lie: rng.fork("disk/fsync_lie"),
            sync_latency: rng.fork("disk/sync_latency"),
            crash: rng.fork("disk/crash"),
        };
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                streams,
                trace,
                crash_point: tokio::sync::Notify::new(),
            }),
        }
    }

    /// An independent copy of the disk as it is now, without faults, with its own streams (forks of `rng`)
    /// and its own trace. Used to compute what an uninterrupted recovery yields.
    pub fn fork(&self, rng: &dyn Rng) -> Self {
        let mut state = self.shared.lock().clone();
        state.faults = DiskFaults::default();
        state.crash_at_op = None;
        state.frozen = false;
        Self::with_state(rng, Arc::new(Trace::new()), state)
    }

    /// Replaces the fault configuration.
    pub fn set_faults(&self, faults: DiskFaults) {
        self.shared.lock().faults = faults;
    }

    /// The current fault configuration.
    pub fn faults(&self) -> DiskFaults {
        self.shared.lock().faults.clone()
    }

    /// Operations started so far.
    pub fn ops(&self) -> u64 {
        self.shared.lock().ops
    }

    /// Arms a crash point: the `n`-th operation from now (1 is the next) freezes the disk instead of
    /// running, and [`SimDisk::crash_point_reached`] resolves.
    pub fn arm_crash_after_ops(&self, n: u64) {
        let mut state = self.shared.lock();
        state.crash_at_op = Some(state.ops + n.max(1));
    }

    /// Removes an armed crash point.
    pub fn disarm_crash(&self) {
        self.shared.lock().crash_at_op = None;
    }

    /// Resolves when an armed crash point was reached. The disk is then frozen.
    pub async fn crash_point_reached(&self) {
        // A permit left by an earlier crash point wakes this once; the frozen flag decides.
        while !self.shared.lock().frozen {
            self.shared.crash_point.notified().await;
        }
    }

    /// Freezes the disk: from now until [`SimDisk::crash`], no operation completes or has any effect.
    pub fn freeze(&self) {
        self.shared.lock().frozen = true;
    }

    /// Applies a crash: every file falls back to its durable image plus a seeded part of its dirty
    /// blocks, a seeded part of the pending directory operations survives, and every open handle dies.
    /// The disk is usable again afterwards. Abort the host's tasks first (see `Host::crash`).
    pub fn crash(&self) {
        let rng = self.shared.streams.crash.as_ref();
        let mut state = self.shared.lock();
        let pending = std::mem::take(&mut state.pending_dir_ops);
        // None of the pending directory operations survive, all of them, a prefix, or any subset.
        let total = pending.len();
        let kept: Vec<bool> = match rng.next_u64() % 4 {
            0 => vec![false; total],
            1 => vec![true; total],
            2 => {
                let prefix = (rng.next_u64() % (total as u64 + 1)) as usize;
                (0..total).map(|i| i < prefix).collect()
            }
            _ => (0..total).map(|_| rng.next_u64().is_multiple_of(2)).collect(),
        };
        let mut names = std::mem::take(&mut state.durable_names);
        for (op, _) in pending.iter().zip(&kept).filter(|(_, kept)| **kept) {
            op.apply(&mut names);
        }
        let live: BTreeSet<u64> = names.values().copied().collect();
        state.inodes.retain(|id, _| live.contains(id));
        for inode in state.inodes.values_mut() {
            inode.crash(rng);
        }
        state.names.clone_from(&names);
        state.durable_names = names;
        state.generation += 1;
        state.frozen = false;
        state.crash_at_op = None;
        self.shared
            .trace
            .event("disk.crash", &[state.generation, total as u64]);
    }

    /// The current image of a file, for assertions.
    pub fn read_file(&self, path: &DiskPath) -> Option<Vec<u8>> {
        let state = self.shared.lock();
        let inode = state.names.get(path)?;
        state.inodes.get(inode).map(|inode| inode.data.clone())
    }

    /// Overwrites bytes of a file in both its current and its durable image, for tests that plant damage.
    /// Returns false when the file does not exist.
    pub fn patch_file(&self, path: &DiskPath, offset: usize, bytes: &[u8]) -> bool {
        let mut state = self.shared.lock();
        let Some(id) = state.names.get(path).copied() else {
            return false;
        };
        let Some(inode) = state.inodes.get_mut(&id) else {
            return false;
        };
        let end = offset + bytes.len();
        for image in [&mut inode.data, &mut inode.durable] {
            if image.len() < end {
                image.resize(end, 0);
            }
            image[offset..end].copy_from_slice(bytes);
        }
        true
    }

    /// The names in a directory, sorted, for assertions.
    pub fn names(&self, dir: &DiskPath) -> Vec<String> {
        let state = self.shared.lock();
        state
            .names
            .keys()
            .filter(|path| parent(path) == *dir)
            .map(|path| file_name(path).to_owned())
            .collect()
    }
}

#[async_trait]
impl Disk for SimDisk {
    async fn create_dir_all(&self, dir: &DiskPath) -> Result<(), DiskError> {
        let generation = self.shared.begin("disk.mkdir", 0, 0).await;
        let mut state = self.shared.live(generation).await?;
        // Directory creation is durable at once: losing a directory is not part of the fault model.
        let mut at = dir.clone();
        while !at.as_str().is_empty() {
            state.dirs.insert(at.clone());
            at = parent(&at);
        }
        Ok(())
    }

    async fn list(&self, dir: &DiskPath) -> Result<Vec<Entry>, DiskError> {
        let generation = self.shared.begin("disk.list", 0, 0).await;
        let state = self.shared.live(generation).await?;
        if !state.dir_exists(dir) {
            return Err(DiskError::NotFound);
        }
        let files = state
            .names
            .iter()
            .filter(|(path, _)| parent(path) == *dir)
            .map(|(path, inode)| Entry {
                name: file_name(path).to_owned(),
                len: state.inodes.get(inode).map_or(0, |i| i.data.len() as u64),
            });
        let dirs = state
            .dirs
            .iter()
            .filter(|path| parent(path) == *dir)
            .map(|path| Entry {
                name: file_name(path).to_owned(),
                len: 0,
            });
        let mut entries: Vec<Entry> = files.chain(dirs).collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    async fn open(&self, path: &DiskPath, mode: OpenMode) -> Result<File, DiskError> {
        let generation = self.shared.begin("disk.open", mode as u64, 0).await;
        let mut state = self.shared.live(generation).await?;
        if path.as_str().is_empty() || state.dirs.contains(path) {
            return Err(DiskError::InvalidPath {
                path: path.to_string(),
                reason: "is a directory",
            });
        }
        let existing = state.names.get(path).copied();
        let inode = match (existing, mode) {
            (Some(_), OpenMode::CreateNew) => return Err(DiskError::AlreadyExists),
            (Some(inode), _) => inode,
            (None, OpenMode::Read) => return Err(DiskError::NotFound),
            (None, _) => {
                if !state.dir_exists(&parent(path)) {
                    return Err(DiskError::NotFound);
                }
                state.next_inode += 1;
                let inode = state.next_inode;
                state.inodes.insert(inode, Inode::default());
                state.dir_op(DirOp::Link(path.clone(), inode));
                inode
            }
        };
        Ok(File::new(Box::new(SimFile {
            shared: Arc::clone(&self.shared),
            inode,
            generation,
            writable: mode != OpenMode::Read,
        })))
    }

    async fn sync_dir(&self, dir: &DiskPath) -> Result<(), DiskError> {
        let generation = self.shared.begin("disk.sync_dir", 0, 0).await;
        let mut state = self.shared.live(generation).await?;
        if !state.dir_exists(dir) {
            return Err(DiskError::NotFound);
        }
        let pending = std::mem::take(&mut state.pending_dir_ops);
        let (synced, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|op| op.dir() == *dir);
        for op in &synced {
            op.apply(&mut state.durable_names);
        }
        state.pending_dir_ops = rest;
        Ok(())
    }

    async fn rename(&self, from: &DiskPath, to: &DiskPath) -> Result<(), DiskError> {
        let generation = self.shared.begin("disk.rename", 0, 0).await;
        let mut state = self.shared.live(generation).await?;
        if from.root() != to.root() {
            return Err(DiskError::InvalidPath {
                path: to.to_string(),
                reason: "rename across roots",
            });
        }
        if !state.names.contains_key(from) {
            return Err(DiskError::NotFound);
        }
        state.dir_op(DirOp::Rename(from.clone(), to.clone()));
        Ok(())
    }

    async fn remove(&self, path: &DiskPath) -> Result<(), DiskError> {
        let generation = self.shared.begin("disk.remove", 0, 0).await;
        let mut state = self.shared.live(generation).await?;
        if state.dirs.remove(path) {
            return Ok(());
        }
        if !state.names.contains_key(path) {
            return Err(DiskError::NotFound);
        }
        state.dir_op(DirOp::Unlink(path.clone()));
        Ok(())
    }

    async fn free_space(&self, _root: Root) -> Result<u64, DiskError> {
        let generation = self.shared.begin("disk.free_space", 0, 0).await;
        let state = self.shared.live(generation).await?;
        Ok(match state.faults.capacity_bytes {
            Some(capacity) => capacity.saturating_sub(state.used_bytes()),
            None => u64::MAX / 2,
        })
    }
}

struct SimFile {
    shared: Arc<Shared>,
    inode: u64,
    generation: u64,
    writable: bool,
}

impl fmt::Debug for SimFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimFile")
            .field("inode", &self.inode)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl SimFile {
    async fn begin(&self, op: &'static str, a: u64, b: u64) -> Result<(), DiskError> {
        let generation = self.shared.begin(op, a, b).await;
        if generation == self.generation {
            Ok(())
        } else {
            Err(io_error(
                std::io::ErrorKind::BrokenPipe,
                "handle predates a crash",
            ))
        }
    }

    fn read_only() -> DiskError {
        io_error(
            std::io::ErrorKind::PermissionDenied,
            "file is open read-only",
        )
    }
}

fn inode(state: &mut State, id: u64) -> Result<&mut Inode, DiskError> {
    state.inodes.get_mut(&id).ok_or(DiskError::NotFound)
}

#[async_trait]
impl FileOps for SimFile {
    async fn preallocate(&self, len: u64) -> Result<(), DiskError> {
        self.begin("file.preallocate", self.inode, len).await?;
        if !self.writable {
            return Err(Self::read_only());
        }
        let mut state = self.shared.live(self.generation).await?;
        let len = len as usize;
        let old = inode(&mut state, self.inode)?.data.len();
        if len > old {
            if !state.fits(old, len) {
                return Err(DiskError::NoSpace);
            }
            inode(&mut state, self.inode)?.set_len(len);
        }
        Ok(())
    }

    async fn append(&self, offset: u64, bufs: Vec<Bytes>) -> Result<(), DiskError> {
        let total: usize = bufs.iter().map(Bytes::len).sum();
        self.begin("file.append", offset, total as u64).await?;
        if !self.writable {
            return Err(Self::read_only());
        }
        let streams = &self.shared.streams;
        let mut state = self.shared.live(self.generation).await?;
        let fault = if hit(streams.write_error.as_ref(), state.faults.write_error_ppm) {
            Some(io_error(std::io::ErrorKind::Other, "EIO on write"))
        } else if hit(streams.short_write.as_ref(), state.faults.short_write_ppm) {
            Some(io_error(std::io::ErrorKind::WriteZero, "short write"))
        } else {
            None
        };
        let offset = offset as usize;
        let old = inode(&mut state, self.inode)?.data.len();
        if !state.fits(old, offset + total) {
            return Err(DiskError::NoSpace);
        }
        // A failed write leaves an unknown prefix behind.
        let written = match &fault {
            Some(_) => (streams.prefix.next_u64() % (total as u64 + 1)) as usize,
            None => total,
        };
        let file = inode(&mut state, self.inode)?;
        let mut at = offset;
        let mut left = written;
        for buf in &bufs {
            let take = buf.len().min(left);
            file.write(at, &buf[..take]);
            at += take;
            left -= take;
        }
        fault.map_or(Ok(()), Err)
    }

    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes, DiskError> {
        self.begin("file.read_at", offset, len as u64).await?;
        let streams = &self.shared.streams;
        let mut state = self.shared.live(self.generation).await?;
        if hit(streams.read_error.as_ref(), state.faults.read_error_ppm) {
            return Err(io_error(std::io::ErrorKind::Other, "EIO on read"));
        }
        let flip = state.faults.bit_flip_ppm;
        let data = &inode(&mut state, self.inode)?.data;
        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(len).min(data.len());
        let mut out = data[start..end].to_vec();
        if !out.is_empty() && hit(streams.bit_flip.as_ref(), flip) {
            let bit = streams.bit_flip.next_u64() % (out.len() as u64 * 8);
            out[(bit / 8) as usize] ^= 1 << (bit % 8);
        }
        Ok(Bytes::from(out))
    }

    async fn sync_data(&self) -> Result<(), DiskError> {
        self.begin("file.sync_data", self.inode, 0).await?;
        let streams = &self.shared.streams;
        let (low, high) = self.shared.lock().faults.sync_latency_micros;
        if high > 0 {
            let span = high.saturating_sub(low) + 1;
            let micros = low + streams.sync_latency.next_u64() % span;
            tokio::time::sleep(Duration::from_micros(micros)).await;
        }
        let mut state = self.shared.live(self.generation).await?;
        let error = hit(streams.sync_error.as_ref(), state.faults.sync_error_ppm);
        let lie = !error && hit(streams.fsync_lie.as_ref(), state.faults.fsync_lie_ppm);
        let file = inode(&mut state, self.inode)?;
        if error || lie {
            file.drop_dirty();
        } else {
            file.sync();
        }
        if error {
            Err(io_error(std::io::ErrorKind::Other, "EIO on sync"))
        } else {
            Ok(())
        }
    }

    async fn truncate(&self, len: u64) -> Result<(), DiskError> {
        self.begin("file.truncate", self.inode, len).await?;
        if !self.writable {
            return Err(Self::read_only());
        }
        let mut state = self.shared.live(self.generation).await?;
        let len = len as usize;
        let old = inode(&mut state, self.inode)?.data.len();
        if !state.fits(old, len) {
            return Err(DiskError::NoSpace);
        }
        inode(&mut state, self.inode)?.set_len(len);
        Ok(())
    }
}
