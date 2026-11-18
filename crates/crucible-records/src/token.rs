//! Batch tokens (contract section 5.2): `{wal_epoch}:{first_lsn}-{last_lsn}:{table_id}:{partition_id}`.
//!
//! The format is injective: every field has a canonical text form and none contains `:`. [`BatchToken::parse`]
//! accepts canonical text only, so `parse(display(t)) == t` for every token and `display(parse(s)) == s` for
//! every accepted string. The canonical text is built without allocation ([`BatchToken::text`]), and `Display`
//! writes exactly those bytes.
//!
//! A partition id is canonical only while its year has four digits. With a longer year two partitions could
//! share a text (`PartitionId::month(202611, 1)` and the date partition of 2026-11-01 both read `20261101`), so
//! [`BatchToken::new`] refuses partitions whose year is outside 0 to 9999. Every partition of a valid
//! `Timestamp` is inside that range.

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
    /// The partition's year is outside 0 to 9999, so its id has no canonical text.
    #[error("a batch token needs a partition year from 0 to 9999")]
    Partition,
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
    /// A token, or [`TokenError::Range`] unless `1 <= first_lsn <= last_lsn`, or [`TokenError::Partition`] when
    /// the partition's year is outside 0 to 9999.
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
        let year = match (partition.as_date(), partition.as_month()) {
            (Some(date), _) => date.year(),
            (None, Some((year, _))) => year,
            (None, None) => 0,
        };
        if year < 0 || year > 9999 {
            return Err(TokenError::Partition);
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

    /// The canonical text, built without allocation.
    pub fn text(&self) -> TokenText {
        let mut text = TokenText {
            bytes: [0; MAX_TOKEN_LEN],
            len: 0,
        };
        for (i, byte) in self.wal_epoch.to_bytes().into_iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                text.push(b'-');
            }
            text.push(HEX[usize::from(byte >> 4)]);
            text.push(HEX[usize::from(byte & 0xf)]);
        }
        text.push(b':');
        text.push_decimal(self.first_lsn.get());
        text.push(b'-');
        text.push_decimal(self.last_lsn.get());
        text.push(b':');
        text.push_decimal(u64::from(self.table.id()));
        text.push(b':');
        match (self.partition.as_date(), self.partition.as_month()) {
            (Some(date), _) => {
                text.push_padded::<4>(date.year());
                text.push_padded::<2>(i32::from(date.month()));
                text.push_padded::<2>(i32::from(date.day()));
            }
            (None, Some((year, month))) => {
                text.push_padded::<4>(year);
                text.push_padded::<2>(i32::from(month));
            }
            (None, None) => b"all".iter().for_each(|&b| text.push(b)),
        }
        text
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
        if token.text().as_bytes() != s.as_bytes() {
            return Err(TokenError::Malformed);
        }
        Ok(token)
    }
}

/// Length of the longest canonical token: a 36-character epoch, two 20-digit LSNs, a 2-digit table id, an
/// 8-digit partition id and four separators.
pub const MAX_TOKEN_LEN: usize = 90;

const HEX: [u8; 16] = *b"0123456789abcdef";

/// The canonical text of a [`BatchToken`], built without allocation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TokenText {
    bytes: [u8; MAX_TOKEN_LEN],
    len: usize,
}

impl TokenText {
    /// The text as ASCII bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        // Every byte written is ASCII.
        core::str::from_utf8(self.as_bytes()).unwrap_or_default()
    }

    fn push(&mut self, byte: u8) {
        if let Some(slot) = self.bytes.get_mut(self.len) {
            *slot = byte;
            self.len += 1;
        }
    }

    /// Unpadded decimal.
    fn push_decimal(&mut self, value: u64) {
        // Least significant digit first; a u64 has at most 20 digits.
        let mut digits = [0u8; 20];
        let mut count = 0;
        let mut rest = value;
        while let Some(slot) = digits.get_mut(count) {
            *slot = b'0' + (rest % 10) as u8;
            count += 1;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        for &digit in digits.get(..count).unwrap_or_default().iter().rev() {
            self.push(digit);
        }
    }

    /// Exactly `N` digits, zero-padded; [`BatchToken::new`] guarantees the value fits.
    fn push_padded<const N: usize>(&mut self, value: i32) {
        let mut digits = [0u8; N];
        let mut rest = value.unsigned_abs();
        for slot in digits.iter_mut().rev() {
            *slot = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        digits.into_iter().for_each(|digit| self.push(digit));
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
        f.write_str(self.text().as_str())
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
        assert_eq!(t.text().as_str(), s);
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

    #[test]
    fn refuses_partitions_without_a_four_digit_year() {
        let token = |p| BatchToken::new(epoch(), Lsn::new(1), Lsn::new(2), TableId::FactScores, p);
        // These two would otherwise share the text `20261101`.
        let date = PartitionId::date(CivilDate::new(2026, 11, 1).unwrap());
        let month = PartitionId::month(202_611, 1).unwrap();
        assert_eq!(month.to_string(), date.to_string());
        assert!(token(date).is_ok());
        assert_eq!(token(month), Err(TokenError::Partition));
        let far = PartitionId::date(CivilDate::new(10_000, 1, 1).unwrap());
        assert_eq!(token(far), Err(TokenError::Partition));
        let negative = PartitionId::month(-1, 12).unwrap();
        assert_eq!(token(negative), Err(TokenError::Partition));
        let first = PartitionId::date(CivilDate::new(0, 1, 1).unwrap());
        assert_eq!(
            token(first).unwrap().to_string().rsplit(':').next(),
            Some("00000101")
        );
    }

    fn arb_partition() -> impl Strategy<Value = PartitionId> {
        prop_oneof![
            Just(PartitionId::UNPARTITIONED),
            (0i64..47_482).prop_map(|d| PartitionId::date(CivilDate::from_day_number(d))),
            (1970i32..2100, 1u8..=12).prop_filter_map("month", |(y, m)| PartitionId::month(y, m)),
            // Every four-digit year.
            (-719_528i64..2_932_897).prop_map(|d| PartitionId::date(CivilDate::from_day_number(d))),
            (0i32..=9999, 1u8..=12).prop_filter_map("month", |(y, m)| PartitionId::month(y, m)),
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
            // The allocation-free text is the contract format of the fields' own text forms.
            let fields = format!("{}:{first}-{last}:{table}:{partition}", WalEpoch::from_bytes(epoch));
            let text = t.text();
            prop_assert_eq!(text.as_str(), fields.as_str());
        }

        #[test]
        fn lsns_of_every_length_round_trip(first in any::<u64>(), last in any::<u64>()) {
            let (first, last) = (first.min(last).max(1), first.max(last).max(1));
            let t = BatchToken::new(epoch(), Lsn::new(first), Lsn::new(last), TableId::SpanCore, PartitionId::UNPARTITIONED)
                .unwrap();
            prop_assert_eq!(BatchToken::parse(t.text().as_str()), Ok(t));
        }
    }
}

#[cfg(kani)]
mod proofs {
    //! Injectivity of the batch token (contract 4.9), proved on the canonical text that `Display` writes.
    //!
    //! `token_text_is_injective` takes two tokens and shows that equal texts imply equal tokens. The epoch, the
    //! table and the partition are unconstrained: any 16 epoch bytes, any table, any four-digit year with any
    //! two-digit month and day (a superset of the valid dates, built without the calendar check), or `all`. Both
    //! LSNs range over `u16`, which covers one- to five-digit decimals and so every way the variable-length
    //! fields and the separators can line up. The partition collision described in the module documentation is
    //! the kind of defect this proof rules out.
    //!
    //! Decimal formatting of full-range `u64` LSNs is left to the property tests (`round_trips`,
    //! `lsns_of_every_length_round_trip`): `parse(text(t)) == t` there, which implies injectivity on every
    //! tested token. Two full-range decimal conversions did not finish within 12 minutes of solver time, which
    //! the shared Kani CI job cannot afford.

    use super::*;
    use crate::time::CivilDate;

    fn any_token() -> BatchToken {
        let first = u64::from(kani::any::<u16>());
        let last = u64::from(kani::any::<u16>());
        let table: u8 = kani::any();
        kani::assume((1..=18).contains(&table));
        let year: i32 = kani::any();
        let month: u8 = kani::any();
        let day: u8 = kani::any();
        kani::assume(
            (0..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day),
        );
        let partition = match kani::any::<u8>() {
            0 => PartitionId::UNPARTITIONED,
            1 => PartitionId::month(year, month).unwrap_or(PartitionId::UNPARTITIONED),
            _ => PartitionId::date(CivilDate::from_fields_unchecked(year, month, day)),
        };
        let token = BatchToken::new(
            WalEpoch::from_bytes(kani::any()),
            Lsn::new(first),
            Lsn::new(last),
            TableId::from_id(table).unwrap_or(TableId::SpanCore),
            partition,
        );
        kani::assume(token.is_ok());
        token.unwrap_or_else(|_| unreachable!())
    }

    #[kani::proof]
    #[kani::unwind(21)]
    fn token_text_is_injective() {
        let a = any_token();
        let b = any_token();
        if a.text() == b.text() {
            assert!(a == b);
        }
    }
}
