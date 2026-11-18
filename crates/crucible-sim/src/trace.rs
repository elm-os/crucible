//! The event trace of a run and its hash (invariant I14: one seed gives one trace hash).

use std::sync::{Mutex, PoisonError};

/// A running BLAKE3 hash over every event of a run, in order.
#[derive(Debug, Default)]
pub struct Trace {
    state: Mutex<(blake3::Hasher, u64)>,
}

impl Trace {
    /// Creates an empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one event: a label and its numeric fields.
    pub fn event(&self, label: &str, fields: &[u64]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (hasher, count) = &mut *state;
        hasher.update(&(label.len() as u64).to_le_bytes());
        hasher.update(label.as_bytes());
        hasher.update(&(fields.len() as u64).to_le_bytes());
        for field in fields {
            hasher.update(&field.to_le_bytes());
        }
        *count += 1;
    }

    /// Events recorded so far.
    pub fn len(&self) -> u64 {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).1
    }

    /// Whether no event was recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The hash of the events recorded so far, in hex.
    pub fn hash(&self) -> String {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0.finalize().to_hex().to_string()
    }
}
