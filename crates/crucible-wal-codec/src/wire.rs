//! Primitives of the payload codec (contract section 4.4): fixed-width little-endian integers, `uuid` as 16
//! bytes, `bytes` as a `u32` length then the bytes, `str` as `bytes` holding UTF-8, `list<T>` as a `u32`
//! count then the items.

/// Why a payload was refused, by a decoder or by an encoder asked to write a value the format cannot hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PayloadError {
    /// The payload ends inside the named field.
    #[error("payload ends inside `{0}`")]
    Truncated(&'static str),
    /// Bytes remain after the last field.
    #[error("{0} trailing bytes after the payload")]
    TrailingBytes(usize),
    /// The payload version is not 1.
    #[error("unsupported payload version {0}")]
    UnsupportedVersion(u8),
    /// The named field holds a value outside its domain.
    #[error("invalid value in `{0}`")]
    InvalidValue(&'static str),
    /// The named field is not valid UTF-8.
    #[error("`{0}` is not valid UTF-8")]
    InvalidUtf8(&'static str),
    /// The named field, list or region is larger than its cap (bytes, or items for a list).
    #[error("`{field}` has length {len}, above its cap of {max}")]
    TooLong {
        /// The field, list or region.
        field: &'static str,
        /// Its length.
        len: usize,
        /// Its cap.
        max: usize,
    },
}

type Result<T> = core::result::Result<T, PayloadError>;

pub(crate) fn check_cap(field: &'static str, len: usize, max: usize) -> Result<()> {
    if len > max {
        return Err(PayloadError::TooLong { field, len, max });
    }
    Ok(())
}

/// A cursor over a payload. Every read is bounds-checked and borrows from the payload.
pub(crate) struct Reader<'a> {
    total: usize,
    rest: &'a [u8],
}

macro_rules! read_int {
    ($($name:ident -> $ty:ty;)+) => {$(
        pub(crate) fn $name(&mut self, field: &'static str) -> Result<$ty> {
            self.array(field).map(<$ty>::from_le_bytes)
        }
    )+};
}

impl<'a> Reader<'a> {
    pub(crate) fn new(payload: &'a [u8]) -> Self {
        Self {
            total: payload.len(),
            rest: payload,
        }
    }

    /// Bytes read so far.
    pub(crate) fn consumed(&self) -> usize {
        self.total.saturating_sub(self.rest.len())
    }

    /// Everything not read yet.
    pub(crate) fn take_rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.rest)
    }

    pub(crate) fn array<const N: usize>(&mut self, field: &'static str) -> Result<[u8; N]> {
        let (head, rest) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or(PayloadError::Truncated(field))?;
        self.rest = rest;
        Ok(*head)
    }

    read_int! {
        u8 -> u8;
        u16 -> u16;
        u32 -> u32;
        u64 -> u64;
        i32 -> i32;
        i64 -> i64;
    }

    /// A byte that must be 0 or 1.
    pub(crate) fn bool(&mut self, field: &'static str) -> Result<bool> {
        match self.u8(field)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PayloadError::InvalidValue(field)),
        }
    }

    pub(crate) fn bytes(&mut self, field: &'static str, max: usize) -> Result<&'a [u8]> {
        let len = self.u32(field)? as usize;
        check_cap(field, len, max)?;
        let (head, rest) = self
            .rest
            .split_at_checked(len)
            .ok_or(PayloadError::Truncated(field))?;
        self.rest = rest;
        Ok(head)
    }

    pub(crate) fn str(&mut self, field: &'static str, max: usize) -> Result<&'a str> {
        let bytes = self.bytes(field, max)?;
        core::str::from_utf8(bytes).map_err(|_| PayloadError::InvalidUtf8(field))
    }

    /// A list count. It is checked against the cap and against the bytes that remain (each item takes at
    /// least `min_item` bytes), so the count is safe to allocate for.
    pub(crate) fn count(
        &mut self,
        field: &'static str,
        max: usize,
        min_item: usize,
    ) -> Result<usize> {
        let count = self.u32(field)? as usize;
        check_cap(field, count, max)?;
        if count > self.rest.len() / min_item.max(1) {
            return Err(PayloadError::Truncated(field));
        }
        Ok(count)
    }

    pub(crate) fn finish(self) -> Result<()> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(PayloadError::TrailingBytes(self.rest.len()))
        }
    }
}

/// Appends a `u32` length or count.
pub(crate) fn put_len(
    out: &mut Vec<u8>,
    field: &'static str,
    len: usize,
    max: usize,
) -> Result<()> {
    check_cap(field, len, max)?;
    let len = u32::try_from(len).map_err(|_| PayloadError::TooLong { field, len, max })?;
    out.extend_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Appends `bytes` (also used for `str`).
pub(crate) fn put_bytes(
    out: &mut Vec<u8>,
    field: &'static str,
    bytes: &[u8],
    max: usize,
) -> Result<()> {
    put_len(out, field, bytes.len(), max)?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// Checks the region of `out` that starts at `start` against its cap.
pub(crate) fn check_region(
    out: &[u8],
    start: usize,
    field: &'static str,
    max: usize,
) -> Result<()> {
    check_cap(field, out.len().saturating_sub(start), max)
}
