//! The synchronization primitives of the index. Unit tests of this crate swap in Shuttle's, so that every lock
//! and atomic operation is a point where Shuttle may switch threads; every other build uses `std`.

#[cfg(test)]
pub(crate) use shuttle::sync::{
    Mutex, RwLock,
    atomic::{AtomicU64, Ordering},
};
#[cfg(not(test))]
pub(crate) use std::sync::{
    Mutex, RwLock,
    atomic::{AtomicU64, Ordering},
};
