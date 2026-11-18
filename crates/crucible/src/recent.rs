//! The node's WAL as the recent-span index reads it (contract 8.6): the durable LSN for the read barrier, and
//! the re-read of one span by its locator through `Wal::read_at` and the normalizer.
//!
//! The record is stage A's. A locator's ordinal is the span's position among the spans stage B admitted from
//! its frame, so when stage B dropped a repeat earlier in the same frame the position is off by the repeats
//! before it. No route of milestone 1 re-reads a span; the ordinal is settled before one does.

use crucible_normalize::{IngestFrame, NormCtx, normalize};
use crucible_recent::{SpanRead, WalLocator, WalReadError, WalView};
use crucible_records::{Lsn, ProjectId, SpanRecord};
use crucible_wal::{Wal, WalFrame};
use crucible_wal_codec::{Record, RecordType};

#[derive(Debug)]
pub(crate) struct WalSpans {
    pub(crate) wal: Wal,
    pub(crate) ctx: NormCtx,
}

/// The spans stage A makes of an `IngestRequest` frame.
fn spans(wal: &Wal, frame: &WalFrame, ctx: &NormCtx) -> Result<Vec<SpanRecord>, WalReadError> {
    let failed = |reason: String| WalReadError::Failed(reason);
    let payload = frame.payload().map_err(|e| failed(e.to_string()))?;
    let record = Record::decode(RecordType::IngestRequest, &payload)
        .map_err(|e| failed(e.to_string()))?;
    let Record::IngestRequest(request) = record else {
        return Err(WalReadError::Gone);
    };
    let ingest = IngestFrame {
        wal_epoch: wal.epoch(),
        lsn: frame.lsn(),
        received_at: frame.received_at(),
        project_id: request.project_id,
        api_key_id: request.api_key_id,
        compat_dialect: request.compat_dialect,
        sdk_name: request.sdk_name,
        sdk_version: request.sdk_version,
        rejected: request.rejected,
        otlp: request.otlp,
    };
    Ok(normalize(&ingest, ctx)
        .map_err(|e| failed(e.to_string()))?
        .spans)
}

impl WalView for WalSpans {
    fn durable_lsn(&self) -> Lsn {
        self.wal.durable_lsn()
    }

    fn read_span(&self, project_id: ProjectId, locator: WalLocator) -> SpanRead {
        let (wal, ctx) = (self.wal.clone(), self.ctx);
        Box::pin(async move {
            let frame = wal
                .read_at(locator.segment, locator.offset)
                .await
                .map_err(|e| WalReadError::Failed(e.to_string()))?
                .filter(|frame| frame.record_type() == RecordType::IngestRequest)
                .ok_or(WalReadError::Gone)?;
            spans(&wal, &frame, &ctx)?
                .into_iter()
                .nth(locator.ordinal as usize)
                .filter(|span| span.core.project_id == project_id)
                .ok_or(WalReadError::Gone)
        })
    }
}
