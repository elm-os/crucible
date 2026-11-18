//! The one place that builds a [`BatchProbe`] (contract 5.5). `clippy.toml` disallows the constructor everywhere
//! else, and the CI allow-attribute guard admits the allow below in this file only.

use crucible_plan::{BatchProbe, PlanError};
use crucible_records::BatchToken;

/// The existence probe of the pair a token names: its table, partition, epoch and batch sequence number.
#[allow(clippy::disallowed_methods)]
pub(crate) fn batch_probe(token: &BatchToken) -> Result<BatchProbe, PlanError> {
    BatchProbe::for_shipper(
        token.table(),
        token.partition(),
        token.wal_epoch(),
        token.batch_seq(),
    )
}
