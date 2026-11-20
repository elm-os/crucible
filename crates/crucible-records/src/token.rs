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

    /// The canonical text, built without allocation: the fields of the token's key, in order, joined by the
    /// separators, with the numbers in unpadded decimal.
    pub fn text(&self) -> TokenText {
        let key = self.key();
        let mut text = TokenText {
            bytes: [0; MAX_TOKEN_LEN],
            len: 0,
        };
        text.extend(&key.epoch);
        text.push(b':');
        text.push_decimal(key.first_lsn);
        text.push(b'-');
        text.push_decimal(key.last_lsn);
        text.push(b':');
        text.push_decimal(u64::from(key.table));
        text.push(b':');
        text.extend(key.partition.get(..key.partition_len).unwrap_or_default());
        text
    }

    /// The fixed-width fields that [`BatchToken::text`] writes: the epoch's text, the LSNs and the table id as
    /// numbers, and the partition id's text with its length. The injectivity proof is stated on this form.
    fn key(&self) -> TokenKey {
        let mut epoch = [0u8; 36];
        let mut at = 0;
        let mut put = |byte: u8| {
            if let Some(slot) = epoch.get_mut(at) {
                *slot = byte;
                at += 1;
            }
        };
        for (i, byte) in self.wal_epoch.to_bytes().into_iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                put(b'-');
            }
            put(HEX[usize::from(byte >> 4)]);
            put(HEX[usize::from(byte & 0xf)]);
        }
        let mut partition = [0u8; 8];
        let partition_len = match (self.partition.as_date(), self.partition.as_month()) {
            (Some(date), _) => {
                write_padded(&mut partition[0..4], date.year());
                write_padded(&mut partition[4..6], i32::from(date.month()));
                write_padded(&mut partition[6..8], i32::from(date.day()));
                8
            }
            (None, Some((year, month))) => {
                write_padded(&mut partition[0..4], year);
                write_padded(&mut partition[4..6], i32::from(month));
                6
            }
            (None, None) => {
                partition[0..3].copy_from_slice(b"all");
                3
            }
        };
        TokenKey {
            epoch,
            first_lsn: self.first_lsn.get(),
            last_lsn: self.last_lsn.get(),
            table: self.table.id(),
            partition,
            partition_len,
        }
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

/// See [`BatchToken::key`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TokenKey {
    epoch: [u8; 36],
    first_lsn: u64,
    last_lsn: u64,
    table: u8,
    partition: [u8; 8],
    partition_len: usize,
}

/// Writes `value` as exactly `out.len()` digits, zero-padded; [`BatchToken::new`] guarantees it fits.
fn write_padded(out: &mut [u8], value: i32) {
    let mut rest = value.unsigned_abs();
    for slot in out.iter_mut().rev() {
        *slot = b'0' + (rest % 10) as u8;
        rest /= 10;
    }
}

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

    fn extend(&mut self, bytes: &[u8]) {
        bytes.iter().for_each(|&b| self.push(b));
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
    //! Injectivity of the batch token (contract 4.9), proved on the token's key ([`BatchToken::key`]): the
    //! fixed-width fields that `text`, and so `Display`, writes in order. They are the epoch's 36-character text,
    //! the two LSNs and the table id as numbers, and the partition id's own text with its length.
    //!
    //! `token_key_is_injective` takes two tokens that [`BatchToken::new`] accepts and shows that equal keys imply
    //! equal tokens. Nothing else is constrained: any epoch, any `u64` LSNs, any table, and a month or date
    //! partition of any `i32` year with any month from 1 to 12 and day from 1 to 31 (a superset of the valid
    //! dates, built without the calendar check), or `all`. Because the key holds the partition's text rather
    //! than its variant, the collision described in the module documentation is a counterexample whenever
    //! `new` admits it: removing the year check from `new` makes this proof fail.
    //!
    //! What the key leaves out is how `text` renders the numbers and joins the fields: unpadded decimals, which
    //! hold only digits, between `:` and `-` separators that no field's text contains. The property tests
    //! `round_trips` and `lsns_of_every_length_round_trip` check `parse(text(t)) == t` for that part. A proof on
    //! the joined text was tried and is out of reach for the shared Kani CI job: the solver did not finish two
    //! full-range `u64` decimal conversions in 12 minutes, and with 16-bit LSNs the joined text (every byte after
    //! a variable-length field sits at a symbolic offset) was still unfinished after 6 minutes.

    use super::*;
    use crate::time::CivilDate;

    fn any_token() -> BatchToken {
        let table: u8 = kani::any();
        kani::assume((1..=19).contains(&table));
        let year: i32 = kani::any();
        let month: u8 = kani::any();
        let day: u8 = kani::any();
        kani::assume((1..=12).contains(&month) && (1..=31).contains(&day));
        let partition = match kani::any::<u8>() {
            0 => PartitionId::UNPARTITIONED,
            1 => PartitionId::month(year, month).unwrap_or(PartitionId::UNPARTITIONED),
            _ => PartitionId::date(CivilDate::from_fields_unchecked(year, month, day)),
        };
        let token = BatchToken::new(
            WalEpoch::from_bytes(kani::any()),
            Lsn::new(kani::any()),
            Lsn::new(kani::any()),
            TableId::from_id(table).unwrap_or(TableId::SpanCore),
            partition,
        );
        kani::assume(token.is_ok());
        token.unwrap_or_else(|_| unreachable!())
    }

    #[kani::proof]
    #[kani::unwind(37)]
    fn token_key_is_injective() {
        let a = any_token();
        let b = any_token();
        if a.key() == b.key() {
            assert!(a == b);
        }
    }
}
