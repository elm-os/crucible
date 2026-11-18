//! Raw statements for the conformance suite, which needs scratch tables, server-side fault setups and reads
//! of system tables that no `TelemetryStore` method offers. Behind the `test-support` feature, which product
//! builds never enable: product code reaches ClickHouse through the trait only.

use std::time::Duration;

use bytes::Bytes;
use crucible_env::{InsertError, QueryError, StoreError};
use crucible_records::Uuid;
use crucible_schema::ServiceIdentity;

use crate::codes::Failure;
use crate::render::Statement;
use crate::store::{CHANGE_DEADLINE, ClickHouseStore, LOOKUP_SECS, Reading};

/// A streaming read of raw bytes, on the same path as `TelemetryStore::execute`.
pub struct RawRead {
    query_id: Uuid,
    reading: Reading,
}

impl std::fmt::Debug for RawRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawRead")
            .field("query_id", &self.query_id)
            .finish_non_exhaustive()
    }
}

impl RawRead {
    /// The id the statement carries.
    pub const fn query_id(&self) -> Uuid {
        self.query_id
    }

    /// The next chunk; `None` at the end. The first poll sends the statement.
    pub async fn next(&mut self) -> Option<Result<Bytes, QueryError>> {
        self.reading.next().await
    }

    /// Reads to the end and returns the number of bytes, or the error that ended the read.
    pub async fn drain(mut self) -> Result<usize, QueryError> {
        let mut bytes = 0;
        while let Some(chunk) = self.next().await {
            bytes += chunk?.len();
        }
        Ok(bytes)
    }
}

impl ClickHouseStore {
    /// Runs a statement that returns nothing, with the statement's own settings.
    pub async fn raw_command(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
    ) -> Result<(), StoreError> {
        self.command(identity, statement, &[], CHANGE_DEADLINE)
            .await
            .map_err(Failure::into_store)
    }

    /// Runs a short read and returns its tab-separated answer.
    pub async fn raw_text(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
    ) -> Result<String, StoreError> {
        self.text(identity, statement, LOOKUP_SECS)
            .await
            .map_err(Failure::into_store)
    }

    /// Sends one insert as `crucible_ingest` on the path of `insert_block`: `sql` with `body` as its
    /// pre-compressed Native block and exactly `settings`.
    pub async fn raw_insert(
        &self,
        sql: &str,
        body: Bytes,
        settings: &[(&str, String)],
    ) -> Result<(), InsertError> {
        self.insert(sql.to_owned(), body, settings)
            .await
            .map_err(Failure::into_insert)
    }

    /// Starts a streaming read on the path of `execute`, with the statement's own settings.
    pub fn raw_read(
        &self,
        identity: ServiceIdentity,
        statement: &Statement,
        deadline: Duration,
        kill_on_drop: bool,
    ) -> Result<RawRead, QueryError> {
        let (query_id, reading) = self.read(
            identity,
            statement,
            &[],
            "RowBinary",
            deadline,
            kill_on_drop,
        )?;
        Ok(RawRead { query_id, reading })
    }
}
