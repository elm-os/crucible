//! The node's WAL as the lifecycle uses it (contract 4.4 and 4.5): a `Tombstone` frame is appended through
//! `Wal::append`, which resolves only after the frame's group is durable.

use async_trait::async_trait;
use crucible_control::{DeletionRequest, DeletionSelector};
use crucible_records::{Lsn, WalEpoch};
use crucible_wal::Wal;
use crucible_wal_codec::{Compression, PreparedFrame, Record, Tombstone, TombstoneSelector};

use crate::node::{LogError, NodeLog};

/// [`NodeLog`] over the node's WAL.
#[derive(Clone, Debug)]
pub struct WalLog(pub Wal);

/// The `Tombstone` record of a request. A user tombstone carries the `user_id`; the issuer of a user
/// deletion is read from Postgres by whoever needs it (contract 4.4 has no field for it).
pub fn tombstone(request: &DeletionRequest) -> Tombstone<'_> {
    Tombstone {
        deletion_request_id: request.id,
        project_id: request.project_id,
        requested_at: request.requested_at,
        deletion_seq: request.deletion_seq,
        selector: match &request.selector {
            DeletionSelector::Traces(ids) => TombstoneSelector::Traces(ids.clone()),
            DeletionSelector::User { user_id, .. } => TombstoneSelector::User(user_id),
            DeletionSelector::Session(session_id) => TombstoneSelector::Session(session_id),
        },
    }
}

#[async_trait]
impl NodeLog for WalLog {
    fn epoch(&self) -> WalEpoch {
        self.0.epoch()
    }

    async fn append_tombstone(&self, request: &DeletionRequest) -> Result<Lsn, LogError> {
        let record = Record::Tombstone(tombstone(request));
        let payload = record.encode().map_err(|e| LogError(e.to_string()))?;
        let frame = PreparedFrame::new(record.record_type(), payload, Compression::Lz4)
            .map_err(|e| LogError(e.to_string()))?;
        self.0
            .append(frame)
            .await
            .map_err(|e| LogError(e.to_string()))
    }

    fn watermark(&self) -> Lsn {
        self.0
            .watermark()
            .map_or(Lsn::ZERO, |watermark| watermark.last_lsn())
    }
}
