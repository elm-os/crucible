//! Typed parameters, row decoding and the error map.
//!
//! Every statement is sent with `query_typed`: parameter types travel with the statement, so nothing is
//! prepared by name and no session state is left behind (contract 2.7; transaction-mode poolers work).

use std::error::Error;

use bytes::{BufMut, BytesMut};
use crucible_control::TxError;
use crucible_records::{Timestamp, Uuid};
use tokio_postgres::Row;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};

/// Microseconds from the Unix epoch to the Postgres epoch, 2000-01-01T00:00:00Z.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

type Boxed = Box<dyn Error + Sync + Send>;

/// One statement parameter with its Postgres type.
#[derive(Debug)]
pub(crate) enum P<'a> {
    Uuid(Uuid),
    OptUuid(Option<Uuid>),
    Text(&'a str),
    Int(i64),
    OptInt(Option<i64>),
    OptText(Option<&'a str>),
    OptInt4(Option<u32>),
    Int4(u32),
    Bool(bool),
    Ts(Timestamp),
    OptTs(Option<Timestamp>),
    Bytes(&'a [u8]),
    OptBytes(Option<&'a [u8]>),
    TextArray(Vec<&'a str>),
}

impl P<'_> {
    pub(crate) fn ty(&self) -> Type {
        match self {
            Self::Uuid(_) | Self::OptUuid(_) => Type::UUID,
            Self::Text(_) | Self::OptText(_) => Type::TEXT,
            Self::Int(_) | Self::OptInt(_) => Type::INT8,
            Self::OptInt4(_) | Self::Int4(_) => Type::INT4,
            Self::Bool(_) => Type::BOOL,
            Self::Ts(_) | Self::OptTs(_) => Type::TIMESTAMPTZ,
            Self::Bytes(_) | Self::OptBytes(_) => Type::BYTEA,
            Self::TextArray(_) => Type::TEXT_ARRAY,
        }
    }
}

fn int4(value: u32) -> Result<i32, Boxed> {
    i32::try_from(value).map_err(|_| "a counter does not fit a Postgres integer".into())
}

fn put_ts(ts: Timestamp, out: &mut BytesMut) {
    out.put_i64(ts.micros().saturating_sub(PG_EPOCH_MICROS));
}

impl ToSql for P<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, Boxed> {
        match self {
            Self::Uuid(id) | Self::OptUuid(Some(id)) => out.put_slice(id.as_bytes()),
            Self::Text(text) | Self::OptText(Some(text)) => return text.to_sql(ty, out),
            Self::Int(value) | Self::OptInt(Some(value)) => return value.to_sql(ty, out),
            Self::Int4(value) => return int4(*value)?.to_sql(ty, out),
            Self::OptInt4(Some(value)) => return int4(*value)?.to_sql(ty, out),
            Self::Bool(value) => return value.to_sql(ty, out),
            Self::Ts(ts) | Self::OptTs(Some(ts)) => put_ts(*ts, out),
            Self::Bytes(bytes) | Self::OptBytes(Some(bytes)) => return bytes.to_sql(ty, out),
            Self::TextArray(items) => return items.to_sql(ty, out),
            Self::OptInt4(None)
            | Self::OptInt(None)
            | Self::OptText(None)
            | Self::OptTs(None)
            | Self::OptBytes(None)
            | Self::OptUuid(None) => {
                return Ok(IsNull::Yes);
            }
        }
        Ok(IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// A `uuid` column.
struct PgUuid(Uuid);

impl<'a> FromSql<'a> for PgUuid {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Boxed> {
        let bytes: [u8; 16] = raw.try_into().map_err(|_| "a uuid is 16 bytes")?;
        Ok(Self(Uuid::from_bytes(bytes)))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::UUID
    }
}

/// A `timestamptz` column.
struct PgTs(Timestamp);

impl<'a> FromSql<'a> for PgTs {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Boxed> {
        let bytes: [u8; 8] = raw.try_into().map_err(|_| "a timestamp is 8 bytes")?;
        let micros = i64::from_be_bytes(bytes);
        Ok(Self(Timestamp::from_micros(
            micros.saturating_add(PG_EPOCH_MICROS),
        )))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMESTAMPTZ
    }
}

fn invalid(what: &str, error: impl std::fmt::Display) -> TxError {
    TxError::Invalid(format!("{what}: {error}"))
}

/// Reads the columns of one row in order.
pub(crate) struct Cols<'a> {
    row: &'a Row,
    next: usize,
}

impl<'a> Cols<'a> {
    pub(crate) fn new(row: &'a Row) -> Self {
        Self { row, next: 0 }
    }

    fn take<T: FromSql<'a>>(&mut self) -> Result<T, TxError> {
        let at = self.next;
        self.next += 1;
        self.row
            .try_get(at)
            .map_err(|error| invalid(&format!("column {at}"), error))
    }

    pub(crate) fn uuid(&mut self) -> Result<Uuid, TxError> {
        Ok(self.take::<PgUuid>()?.0)
    }

    pub(crate) fn opt_uuid(&mut self) -> Result<Option<Uuid>, TxError> {
        Ok(self.take::<Option<PgUuid>>()?.map(|id| id.0))
    }

    pub(crate) fn text(&mut self) -> Result<String, TxError> {
        self.take()
    }

    pub(crate) fn opt_text(&mut self) -> Result<Option<String>, TxError> {
        self.take()
    }

    pub(crate) fn opt_i64(&mut self) -> Result<Option<i64>, TxError> {
        self.take()
    }

    pub(crate) fn bool(&mut self) -> Result<bool, TxError> {
        self.take()
    }

    pub(crate) fn bytes(&mut self) -> Result<Vec<u8>, TxError> {
        self.take()
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], TxError> {
        let bytes = self.bytes()?;
        <[u8; N]>::try_from(bytes.as_slice())
            .map_err(|_| invalid("bytes", format!("expected {N}, found {}", bytes.len())))
    }

    pub(crate) fn texts(&mut self) -> Result<Vec<String>, TxError> {
        self.take()
    }

    /// A `bigint` counter, which is never negative.
    pub(crate) fn u64(&mut self) -> Result<u64, TxError> {
        let value: i64 = self.take()?;
        u64::try_from(value).map_err(|error| invalid("counter", error))
    }

    /// An `integer` counter, which is never negative.
    pub(crate) fn u32(&mut self) -> Result<u32, TxError> {
        let value: i32 = self.take()?;
        u32::try_from(value).map_err(|error| invalid("counter", error))
    }

    pub(crate) fn opt_u32(&mut self) -> Result<Option<u32>, TxError> {
        let value: Option<i32> = self.take()?;
        value
            .map(|value| u32::try_from(value).map_err(|error| invalid("counter", error)))
            .transpose()
    }

    pub(crate) fn ts(&mut self) -> Result<Timestamp, TxError> {
        Ok(self.take::<PgTs>()?.0)
    }

    pub(crate) fn opt_ts(&mut self) -> Result<Option<Timestamp>, TxError> {
        Ok(self.take::<Option<PgTs>>()?.map(|ts| ts.0))
    }

    /// A text column that holds one of a closed set of values.
    pub(crate) fn parsed<T>(&mut self, parse: impl Fn(&str) -> Option<T>) -> Result<T, TxError> {
        let text = self.text()?;
        parse(&text).ok_or_else(|| invalid("stored value", format!("unknown {text:?}")))
    }
}

/// A counter as a `bigint` parameter.
pub(crate) fn int(value: u64) -> Result<P<'static>, TxError> {
    i64::try_from(value)
        .map(P::Int)
        .map_err(|error| invalid("counter", error))
}

/// What a failed statement means for the caller and for the connection.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) error: TxError,
    /// The connection can no longer be used.
    pub(crate) broken: bool,
}

/// Maps a client error (contract 2.7). `writes` says whether the statement could have changed anything: a
/// connection lost under a write leaves its outcome unknown.
pub(crate) fn failure(error: &tokio_postgres::Error, writes: bool) -> Failure {
    let Some(db) = error.as_db_error() else {
        return Failure {
            error: if writes {
                TxError::CommitUnknown
            } else {
                TxError::Unavailable(format!("the connection to Postgres failed: {error}"))
            },
            broken: true,
        };
    };
    let constraint = || db.constraint().unwrap_or_default().to_owned();
    let code = db.code();
    let (error, broken) = if *code == SqlState::T_R_SERIALIZATION_FAILURE
        || *code == SqlState::T_R_DEADLOCK_DETECTED
    {
        (TxError::SerializationFailure, false)
    } else if *code == SqlState::UNIQUE_VIOLATION {
        (TxError::UniqueViolation(constraint()), false)
    } else if *code == SqlState::FOREIGN_KEY_VIOLATION {
        (TxError::ForeignKeyViolation(constraint()), false)
    } else if *code == SqlState::QUERY_CANCELED {
        (
            TxError::Unavailable("the statement passed the 10 s statement timeout".to_owned()),
            false,
        )
    } else if code.code().starts_with("08")
        || code.code().starts_with("57P")
        || code.code().starts_with("53")
        || *code == SqlState::IDLE_IN_TRANSACTION_SESSION_TIMEOUT
    {
        // Connection exceptions, operator intervention (shutdown, failover) and resource exhaustion.
        (
            TxError::Unavailable(format!("Postgres refused the statement: {}", db.message())),
            true,
        )
    } else {
        (
            TxError::Invalid(format!("Postgres error {}: {}", code.code(), db.message())),
            false,
        )
    };
    Failure { error, broken }
}
