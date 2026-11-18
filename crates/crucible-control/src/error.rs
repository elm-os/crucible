//! The typed errors of the control store (contract 2.7).

/// Why a control-store call failed. Every repository method, a transaction's commit and the generic
/// [`transaction`](crate::transaction) wrapper answer with this one type, so a caller's retry policy is the same
/// for a single statement and for a transaction.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum TxError {
    /// The transaction lost a write-write race under snapshot isolation. Nothing was applied; run it again.
    #[error("serialization failure; retry the transaction")]
    SerializationFailure,
    /// A unique constraint refused the write. Carries the constraint name, for example `users_email_key`.
    #[error("unique constraint {0} violated")]
    UniqueViolation(String),
    /// A foreign-key constraint refused the write. Carries the constraint name.
    #[error("foreign-key constraint {0} violated")]
    ForeignKeyViolation(String),
    /// The connection was lost while a commit (or a single writing statement) was in flight: it may or may
    /// not have been applied. Read the state back before retrying anything that is not idempotent.
    #[error("the connection was lost during commit; the outcome is unknown")]
    CommitUnknown,
    /// A fenced write found that the lease's fencing token has changed (contract 11.7). The caller is no longer
    /// the leader and must stop.
    #[error("the lease is no longer held under this fencing token")]
    Fenced,
    /// Postgres cannot be reached, the pool is exhausted, or a time limit passed. Nothing was applied.
    #[error("the control plane is unavailable: {0}")]
    Unavailable(String),
    /// A stored row or an argument does not fit its type. This is a defect or a corrupted row, never a
    /// condition to retry.
    #[error("invalid control-plane data: {0}")]
    Invalid(String),
}

impl TxError {
    /// Whether running the same transaction again can succeed without any other change.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::SerializationFailure | Self::Unavailable(_))
    }
}
