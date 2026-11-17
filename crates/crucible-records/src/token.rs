//! Batch tokens (contract section 5.2): `{wal_epoch}:{first_lsn}-{last_lsn}:{table_id}:{partition_id}`.
//!
//! The format is injective: every field has a canonical text form and none contains `:`. [`BatchToken::parse`]
//! accepts canonical text only, so `parse(display(t)) == t` for every token and `display(parse(s)) == s` for
//! every accepted string.

use core::fmt;

use crate::id::WalEpoch;
use crate::lsn::Lsn;
use crate::table::{PartitionId, TableId};

/// A token that is out of range or not in canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The range does not satisfy `1 <= first_lsn <= last_lsn`.
    #[error("a batch range needs 1 <= first_lsn <= last_lsn")]
    Range,
    /// The text is not a canonical token.
    #[error("malformed batch token")]
    Malformed,
}

/// The deduplication token of one insert: one (batch, table, partition) pair.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BatchToken {
    wal_epoch: WalEpoch,
    first_lsn: Lsn,
    last_lsn: Lsn,
    table: TableId,
    partition: PartitionId,
}

impl BatchToken {
    /// A token, or [`TokenError::Range`] unless `1 <= first_lsn <= last_lsn`.
    pub const fn new(
        wal_epoch: WalEpoch,
        first_lsn: Lsn,
        last_lsn: Lsn,
        table: TableId,
        partition: PartitionId,
    ) -> Result<Self, TokenError> {
        if first_lsn.get() == 0 || first_lsn.get() > last_lsn.get() {
            return Err(TokenError::Range);
        }
        Ok(Self {
            wal_epoch,
            first_lsn,
            last_lsn,
            table,
            partition,
        })
    }

    /// The WAL epoch.
    pub const fn wal_epoch(&self) -> WalEpoch {
        self.wal_epoch
    }

    /// First LSN of the batch.
    pub const fn first_lsn(&self) -> Lsn {
        self.first_lsn
    }

    /// Last LSN of the batch.
    pub const fn last_lsn(&self) -> Lsn {
        self.last_lsn
    }

    /// `batch_seq`, which is the first LSN.
    pub const fn batch_seq(&self) -> Lsn {
        self.first_lsn
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The partition.
    pub const fn partition(&self) -> PartitionId {
        self.partition
    }

    /// Parses canonical token text.
    pub fn parse(s: &str) -> Result<Self, TokenError> {
        let mut parts = s.split(':');
        let (Some(epoch), Some(range), Some(table), Some(partition), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(TokenError::Malformed);
        };
        let wal_epoch = WalEpoch::parse(epoch).map_err(|_| TokenError::Malformed)?;
        let (first, last) = range.split_once('-').ok_or(TokenError::Malformed)?;
        let first_lsn = Lsn::new(decimal(first)?);
        let last_lsn = Lsn::new(decimal(last)?);
        let table_id = u8::try_from(decimal(table)?).map_err(|_| TokenError::Malformed)?;
        let table = TableId::from_id(table_id).ok_or(TokenError::Malformed)?;
        let partition = PartitionId::parse(partition).map_err(|_| TokenError::Malformed)?;
        let token = Self::new(wal_epoch, first_lsn, last_lsn, table, partition)?;
        // Reject non-canonical spellings (for example an uppercase epoch).
        if token.to_string() != s {
            return Err(TokenError::Malformed);
        }
        Ok(token)
    }
}

/// An unpadded decimal: digits only, no sign, no leading zero.
fn decimal(s: &str) -> Result<u64, TokenError> {
    let canonical =
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    if !canonical {
        return Err(TokenError::Malformed);
    }
    s.parse::<u64>().map_err(|_| TokenError::Malformed)
}

impl fmt::Display for BatchToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}-{}:{}:{}",
            self.wal_epoch,
            self.first_lsn,
            self.last_lsn,
            self.table.id(),
            self.partition
        )
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::time::CivilDate;

    fn epoch() -> WalEpoch {
        WalEpoch::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b").unwrap()
    }

    #[test]
    fn matches_the_contract_example() {
        let p = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
        let t = BatchToken::new(
            epoch(),
            Lsn::new(1_048_577),
            Lsn::new(1_051_000),
            TableId::SpanCore,
            p,
        )
        .unwrap();
        let s = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:1048577-1051000:1:20261117";
        assert_eq!(t.to_string(), s);
        assert_eq!(BatchToken::parse(s), Ok(t));
        assert_eq!(t.batch_seq(), Lsn::new(1_048_577));
    }

    #[test]
    fn rejects_bad_ranges_and_spellings() {
        let p = PartitionId::UNPARTITIONED;
        assert_eq!(
            BatchToken::new(epoch(), Lsn::ZERO, Lsn::new(3), TableId::SpanCore, p),
            Err(TokenError::Range)
        );
        assert_eq!(
            BatchToken::new(epoch(), Lsn::new(4), Lsn::new(3), TableId::SpanCore, p),
            Err(TokenError::Range)
        );
        for bad in [
            "0193A1B2-7C3D-7E4F-8A5B-6C7D8E9F0A1B:1-2:1:all",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:01-2:1:all",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:1-2:01:all",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:1-2:19:all",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:1-2:1:all:x",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:1:1:all",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:2-1:1:all",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:+1-2:1:all",
        ] {
            assert!(BatchToken::parse(bad).is_err(), "{bad}");
        }
    }

    fn arb_partition() -> impl Strategy<Value = PartitionId> {
        prop_oneof![
            Just(PartitionId::UNPARTITIONED),
            (0i64..47_482).prop_map(|d| PartitionId::date(CivilDate::from_day_number(d))),
            (1970i32..2100, 1u8..=12).prop_filter_map("month", |(y, m)| PartitionId::month(y, m)),
        ]
    }

    proptest! {
        #[test]
        fn round_trips(
            epoch in any::<[u8; 16]>(),
            first in 1u64..u64::MAX,
            span in 0u64..1_000_000,
            table in 1u8..=18,
            partition in arb_partition(),
        ) {
            let last = first.saturating_add(span);
            let t = BatchToken::new(
                WalEpoch::from_bytes(epoch),
                Lsn::new(first),
                Lsn::new(last),
                TableId::from_id(table).unwrap(),
                partition,
            )
            .unwrap();
            prop_assert_eq!(BatchToken::parse(&t.to_string()), Ok(t));
        }
    }
}
