//! The telemetry store (contract 2.6): the only way product code reaches ClickHouse, apart from the SQL
//! workspace.
//!
//! `crucible-store-clickhouse` implements [`TelemetryStore`] over the real server and `crucible-sim` over an
//! in-memory model. Both return the same result shapes ([`QueryPlan::output`]) and the same typed errors, so the
//! shipper, the query layer and the lifecycle executors run unchanged over either.
//!
//! No method reads across projects. Rows leave the store only through [`TelemetryStore::execute`], which takes a
//! [`QueryPlan`] and so a `ProjectScope`. [`TelemetryStore::probe`] returns one boolean,
//! [`TelemetryStore::partition_projects`] returns project ids only, and there is no method that takes SQL text
//! and returns rows.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use crucible_chfmt::EncodedBlock;
use crucible_plan::{BatchProbe, DeletePlan, QueryPlan, Row, RowBlock};
use crucible_records::{BatchToken, PartitionId, ProjectId, TableId, Uuid};
use crucible_schema::MigrationStep;
use futures::stream::{BoxStream, Stream, StreamExt, TryStreamExt};

/// The deployment flavour of the store (contract 6.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StoreFlavour {
    /// One server.
    SingleNode,
    /// One shard with N replicas.
    Clustered,
    /// The managed service.
    Cloud,
}

/// The settings sent explicitly with every insert, never left to server or profile defaults (contract 5.4).
/// The token also names the table and the partition, so an insert cannot pair a block with the wrong ones.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InsertSettings {
    token: BatchToken,
    quorum_auto: bool,
}

impl InsertSettings {
    /// `max_execution_time` of an insert, in seconds.
    pub const MAX_EXECUTION_TIME_SECS: u32 = 60;

    /// The name of every setting an insert can carry, in the order of contract 5.4. `insert_quorum` is sent to
    /// the clustered flavour only.
    pub const NAMES: [&'static str; 9] = [
        "async_insert",
        "insert_deduplicate",
        "insert_deduplication_token",
        "max_insert_threads",
        "max_partitions_per_insert_block",
        "throw_on_max_partitions_per_insert_block",
        "input_format_native_allow_types_conversion",
        "max_execution_time",
        "insert_quorum",
    ];

    /// The settings of the insert of one (batch, table, partition) pair into a store of this flavour.
    pub const fn new(token: BatchToken, flavour: StoreFlavour) -> Self {
        Self {
            token,
            quorum_auto: matches!(flavour, StoreFlavour::Clustered),
        }
    }

    /// The deduplication token.
    pub const fn token(&self) -> BatchToken {
        self.token
    }

    /// The table the block is inserted into.
    pub const fn table(&self) -> TableId {
        self.token.table()
    }

    /// The one partition the block belongs to.
    pub const fn partition(&self) -> PartitionId {
        self.token.partition()
    }

    /// Every setting with its value, in the order of [`InsertSettings::NAMES`]. A store sends exactly these.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let [
            async_insert,
            dedup,
            token,
            threads,
            partitions,
            throw,
            convert,
            time,
            quorum,
        ] = Self::NAMES;
        let mut pairs = vec![
            (async_insert, "0".to_owned()),
            (dedup, "1".to_owned()),
            (token, self.token.text().as_str().to_owned()),
            (threads, "1".to_owned()),
            (partitions, "1".to_owned()),
            (throw, "1".to_owned()),
            (convert, "1".to_owned()),
            (time, Self::MAX_EXECUTION_TIME_SECS.to_string()),
        ];
        if self.quorum_auto {
            pairs.push((quorum, "auto".to_owned()));
        }
        pairs
    }
}

/// The class of a failed insert (contract 5.6): what the shipper does next.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum InsertClass {
    /// Nothing landed. Send the same token and the same bytes again after a backoff.
    Retry,
    /// The block may or may not have landed. Probe before any retry; insert only if absent.
    UnknownOutcome,
    /// The deployment is misconfigured. Stop shipping that table, alert the operator, keep the WAL.
    FatalConfig,
    /// The server refuses this block's data. After 3 identical failures the pair is quarantined.
    Poison,
}

/// Why a retryable insert failed. The shipper also widens batches on `TooManyParts` and halves its concurrency
/// on `Memory`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RetryReason {
    /// A network error before the request was sent.
    NotSent,
    /// Server code 202: too many simultaneous queries.
    TooManyQueries,
    /// Server code 252: too many parts.
    TooManyParts,
    /// Server code 241: memory limit exceeded.
    Memory,
    /// Server code 243: not enough space.
    NoSpace,
    /// Server code 242: the table is read-only.
    ReadOnly,
}

/// A failed insert, classified by the store (contract 5.6). The server-code table is data in each store; the
/// actions (backoff, probe, alert, quarantine) belong to the shipper, so simulation exercises them.
///
/// A failure that fits no class must be reported as [`InsertError::UnknownOutcome`]: probing is always safe.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum InsertError {
    /// Nothing landed; retry with backoff.
    #[error("insert not applied ({reason:?}): {message}")]
    Retry {
        /// Why, for the shipper's pressure response.
        reason: RetryReason,
        /// Text for logs.
        message: String,
    },
    /// A timeout, a reset mid-request, or server code 159, 319 or 999: the outcome is unknown.
    #[error("insert outcome unknown: {message}")]
    UnknownOutcome {
        /// The server code, when the server answered.
        code: Option<u32>,
        /// Text for logs.
        message: String,
    },
    /// Server code 516, 497, 60 or 16: authentication, access or a missing table or column.
    #[error("insert refused by configuration (code {code}): {message}")]
    FatalConfig {
        /// The server code.
        code: u32,
        /// Text for logs.
        message: String,
    },
    /// Server code 53, 27, 117 or a similar data error. Two failures are identical when their codes are equal.
    #[error("insert refused for its data (code {code}): {message}")]
    Poison {
        /// The server code.
        code: u32,
        /// Text for logs.
        message: String,
    },
}

impl InsertError {
    /// The class.
    pub const fn class(&self) -> InsertClass {
        match self {
            Self::Retry { .. } => InsertClass::Retry,
            Self::UnknownOutcome { .. } => InsertClass::UnknownOutcome,
            Self::FatalConfig { .. } => InsertClass::FatalConfig,
            Self::Poison { .. } => InsertClass::Poison,
        }
    }
}

/// A failed read. The variants map one-to-one to the problem codes of contract 12.3; guardrail failures are
/// never retried (contract 8.4).
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum QueryError {
    /// The class time limit or the Rust-side deadline passed (server code 159).
    #[error("the query exceeded its time limit")]
    Timeout,
    /// The class memory limit was exceeded (server code 241).
    #[error("the query exceeded its memory limit")]
    MemoryExceeded,
    /// A row or byte guardrail was hit, or a semi-join produced more than `max_ids` ids (server codes 158, 307
    /// and 396).
    #[error("the query reads too much")]
    TooBroad,
    /// The store cannot be reached, or refuses queries for now.
    #[error("the telemetry store is unavailable: {0}")]
    Unavailable(String),
    /// Anything else, including a result that does not follow the plan's output schema.
    #[error("the telemetry store failed: {0}")]
    Internal(String),
}

impl QueryError {
    /// The problem code of contract 12.3.
    pub const fn problem_code(&self) -> &'static str {
        match self {
            Self::Timeout => "query_timeout",
            Self::MemoryExceeded => "query_memory_exceeded",
            Self::TooBroad => "query_too_broad",
            Self::Unavailable(_) => "telemetry_store_unavailable",
            Self::Internal(_) => "internal_error",
        }
    }
}

/// A failed probe, maintenance, migration or detection call.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum StoreError {
    /// The store cannot be reached or did not answer in time. For a statement that changes data the outcome is
    /// unknown; every such method is idempotent, so the caller repeats it later.
    #[error("the telemetry store is unavailable: {0}")]
    Unavailable(String),
    /// The server refused the statement.
    #[error("the telemetry store refused the statement (code {code}): {message}")]
    Rejected {
        /// The server code.
        code: u32,
        /// Text for logs.
        message: String,
    },
    /// A startup self-test ran and did not hold (contract 5.8).
    #[error("self-test failed: {0}")]
    SelfTest(String),
    /// Anything else.
    #[error("the telemetry store failed: {0}")]
    Internal(String),
}

impl StoreError {
    /// The problem code of contract 12.3 for a request that fails on this error.
    pub const fn problem_code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "telemetry_store_unavailable",
            Self::Rejected { .. } | Self::SelfTest(_) | Self::Internal(_) => "internal_error",
        }
    }
}

/// A mutation a store started: a lightweight delete or a mask application on one table. The name is the
/// store's own; callers only keep it and pass it back to [`TelemetryStore::mutation_status`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct MutationId {
    table: TableId,
    name: String,
}

impl MutationId {
    /// The id of a mutation of `table`. Called by stores, and by executors that kept an id across a restart.
    pub const fn new(table: TableId, name: String) -> Self {
        Self { table, name }
    }

    /// The table the mutation runs on.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The store's name for the mutation.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Where a mutation stands.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum MutationStatus {
    /// Not finished on every part yet.
    Pending,
    /// Finished. A store also answers `Done` for a mutation it no longer lists: only finished mutations and
    /// mutations of dropped partitions leave the list.
    Done,
    /// The store gave up on it.
    Failed {
        /// The server code of the latest failure.
        code: u32,
        /// Text for logs and the operator alert.
        reason: String,
    },
}

/// A server version: the first two components of `version()`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StoreVersion {
    /// The year component.
    pub major: u32,
    /// The release within the year.
    pub minor: u32,
}

impl StoreVersion {
    /// The oldest version ingest accepts (contract 5.8).
    pub const MINIMUM: Self = Self {
        major: 26,
        minor: 3,
    };
}

/// The engine family of a table as it exists in the store.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EngineFamily {
    /// `MergeTree` or `AggregatingMergeTree`.
    MergeTree,
    /// The `Replicated*` engines.
    Replicated,
    /// The managed service's `Shared*` engines.
    Shared,
}

/// What startup detection found (contract 5.8). A node refuses ingest when `version` is below
/// [`StoreVersion::MINIMUM`], when `missing_settings` is not empty, or when either text-search field is false.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StoreFacts {
    /// `version()`.
    pub version: StoreVersion,
    /// Single node, clustered or Cloud.
    pub flavour: StoreFlavour,
    /// The engine family of every Crucible table that exists, in `TableId` order. A table that has not been
    /// migrated yet is absent.
    pub engines: Vec<(TableId, EngineFamily)>,
    /// The settings a store sends on any call that the server does not know (`system.settings`,
    /// `system.merge_tree_settings`), sorted. Empty when every one is present.
    pub missing_settings: Vec<String>,
    /// Whether the text-search functions that plans render exist.
    pub text_search_functions: bool,
    /// Whether the text index type is available.
    pub text_index: bool,
    /// The server setting `distributed_ddl_use_initial_user_and_roles`, which the clustered flavour's
    /// `ON CLUSTER` text-index DDL needs on every host. `None` for the other flavours.
    pub ddl_initial_user_and_roles: Option<bool>,
}

/// Whether the store answers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StoreHealth {
    /// The store answered a trivial statement.
    Healthy,
    /// It did not.
    Unavailable {
        /// Text for logs and the readiness endpoint.
        reason: String,
    },
}

/// The result of a read: [`RowBlock`]s that follow the plan's [`QueryPlan::output`].
///
/// - Every block holds at least one row; the result is the concatenation of the blocks; an empty result is a
///   stream that ends at once. Block boundaries carry no meaning and differ between stores.
/// - The first `Err` is terminal: the rows before it are a prefix of the result, and the stream yields nothing
///   after it.
/// - Dropping the stream cancels the query (contract 8.5). The store does the same on the class deadline, which
///   ends the stream with [`QueryError::Timeout`]. [`TelemetryStore::kill`] with [`RowStream::query_id`] is the
///   fallback for a caller that cannot drop the stream.
pub struct RowStream {
    query_id: Uuid,
    blocks: BoxStream<'static, Result<RowBlock, QueryError>>,
}

impl RowStream {
    /// Wraps a store's block stream. Called by `TelemetryStore` implementations.
    pub fn new(
        query_id: Uuid,
        blocks: impl Stream<Item = Result<RowBlock, QueryError>> + Send + 'static,
    ) -> Self {
        Self {
            query_id,
            blocks: blocks.boxed(),
        }
    }

    /// The id the statement carries (contract 8.3).
    pub const fn query_id(&self) -> Uuid {
        self.query_id
    }

    /// Reads the stream to its end and returns every row, or the error that ended it.
    pub async fn rows(self) -> Result<Vec<Row>, QueryError> {
        self.blocks
            .try_fold(Vec::new(), |mut rows, block| async move {
                rows.extend(block.into_rows());
                Ok(rows)
            })
            .await
    }
}

impl Stream for RowStream {
    type Item = Result<RowBlock, QueryError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.blocks.poll_next_unpin(cx)
    }
}

impl fmt::Debug for RowStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RowStream")
            .field("query_id", &self.query_id)
            .finish_non_exhaustive()
    }
}

/// The telemetry store. Each method runs as the ClickHouse identity named in its documentation (contract 7).
///
/// Retry, backoff and batching sit above this trait. Futures are cancel-safe in the sense that dropping one
/// leaves the store usable; a dropped call that changes data has an unknown outcome, which every caller handles
/// the way its method documents.
#[async_trait]
pub trait TelemetryStore: fmt::Debug + Send + Sync {
    /// Inserts one Native block into the table and partition that `settings.token()` names, with exactly the
    /// settings of `settings` (contract 5). Runs as `crucible_ingest`. A block that spans partitions is
    /// refused. A dropped future is an unknown outcome: probe before any retry.
    async fn insert_block(
        &self,
        block: EncodedBlock,
        settings: InsertSettings,
    ) -> Result<(), InsertError>;

    /// Runs a typed plan as the identity of its query class (contract 8.4) and returns its rows, which follow
    /// [`QueryPlan::output`].
    async fn execute(&self, plan: QueryPlan) -> Result<RowStream, QueryError>;

    /// Whether the block of one (batch, table, partition) pair is present (contract 5.5). Runs as
    /// `crucible_ingest`. This is the one read without a project predicate, and it returns nothing else.
    async fn probe(&self, probe: BatchProbe) -> Result<bool, StoreError>;

    /// Starts a lightweight `DELETE ... IN PARTITION` with a typed predicate (contract 13.2). Runs as
    /// `crucible_maint`. The plan always carries a project predicate or the marker predicate. The matching rows
    /// are hidden from reads when this returns.
    async fn delete(&self, plan: DeletePlan) -> Result<MutationId, StoreError>;

    /// Drops one partition of a table, marker rows included. Runs as `crucible_maint`. Dropping a partition that
    /// does not exist succeeds.
    async fn drop_partition(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<(), StoreError>;

    /// Starts `APPLY DELETED MASK IN PARTITION`, which physically removes the partition's deleted rows. Runs as
    /// `crucible_maint`.
    async fn apply_deleted_mask(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<MutationId, StoreError>;

    /// Where a mutation stands. Runs as `crucible_maint`.
    async fn mutation_status(&self, id: &MutationId) -> Result<MutationStatus, StoreError>;

    /// The distinct projects with visible rows in a partition, ascending, without the nil marker project.
    /// Runs as `crucible_maint`. Markers never keep a partition alive.
    async fn partition_projects(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError>;

    /// The number of parts of a partition that still carry a delete mask. Runs as `crucible_maint`.
    async fn masked_parts(&self, table: TableId, partition: PartitionId)
    -> Result<u64, StoreError>;

    /// Applies one schema or access step, statement by statement, with the step's settings. Runs as
    /// `crucible_admin`. Idempotent: a step that failed part-way is applied again.
    async fn apply_migration(&self, step: &MigrationStep) -> Result<(), StoreError>;

    /// Startup detection (contract 5.8). Runs as `crucible_maint`.
    async fn detect(&self) -> Result<StoreFacts, StoreError>;

    /// The deduplication self-test (contract 5.8): inserts one block twice with one token into
    /// `dedup_selftest` and reads back exactly one row. Runs as `crucible_ingest`. Anything but one row is
    /// [`StoreError::SelfTest`].
    async fn dedup_selftest(&self) -> Result<(), StoreError>;

    /// `KILL QUERY` by id (contract 8.5). Runs as `crucible_maint`. Killing a query that has ended succeeds.
    async fn kill(&self, query_id: Uuid) -> Result<(), StoreError>;

    /// Whether the store answers. Runs as `crucible_maint`. Never fails: an unreachable store is
    /// [`StoreHealth::Unavailable`].
    async fn health(&self) -> StoreHealth;
}
