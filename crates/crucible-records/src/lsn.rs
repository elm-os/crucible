//! Log sequence numbers (contract section 4.3).

use core::fmt;

/// Log sequence number. It starts at 1 in a WAL epoch and increases by exactly 1 per frame of any type, across
/// segments. [`Lsn::ZERO`] means "before the first frame".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Lsn(u64);

impl Lsn {
    /// Before the first frame.
    pub const ZERO: Self = Self(0);

    /// The first frame of an epoch.
    pub const FIRST: Self = Self(1);

    /// Wraps a raw value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw value.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next LSN, or `None` on overflow.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

/// Unpadded decimal, the form used in batch tokens.
impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
