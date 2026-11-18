//! The insert, deduplication and probe side of the simulated ClickHouse (contract section 2.6, `ChModel`).
//!
//! - Tables are the production `TableDef`s of `crucible-schema`. A block is decoded from its `EncodedBlock` and
//!   checked against its table: column names and header types, at least one row, one partition, the batch
//!   identity of its rows or of its marker row, and the ORDER BY sort. A block that fails is refused and
//!   recorded as a violation: the shipper must never send one.
//! - Parts are kept per table with their partition. A probe is answered by scanning the rows (plain tables) or
//!   the marker rows (aggregating tables) of the landed parts, never from the token.
//! - Each table has a deduplication log bounded by the count window of the DDL and, for the replicated
//!   flavours, by the age window. Window expiry is also a fault: a log entry may expire early, except while a
//!   copy of its insert is in flight, or within `probe_valid_for` of a probe of its pair that read absent.
//!   Those are the two assumptions of the ingest model (`docs/testing/formal-model.md`), which conformance
//!   rows C1 to C3 pin on real servers.
//! - Outcome faults: timeout before apply, timeout with the insert still in flight (it lands later, also
//!   after the node crashed), applied with the ack lost, a server error, too many parts, memory and
//!   concurrency rejections, an overload that sheds inserts after a timeout, a hang, an outage that lasts
//!   minutes, and poison pairs.
//!
//! The model may be stricter than the real system and is never more permissive. The plan interpreter, merges,
//! deletes and partition drops join with the read-side simulation; until then those methods answer `Internal`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_chfmt::{
    DecodedBlock, EncodedBlock, MAX_BLOCK_ROWS, Values, decode_block, decompress,
};
use crucible_env::{
    EngineFamily, InsertError, InsertSettings, MutationId, MutationStatus, QueryError, RetryReason,
    Rng, RowStream, StoreError, StoreFacts, StoreFlavour, StoreHealth, StoreVersion,
    TelemetryStore,
};
use crucible_plan::{BatchProbe, DeletePlan, QueryPlan};
use crucible_records::{BatchToken, PartitionId, ProjectId, TableId, Timestamp, Uuid};
use crucible_schema::{
    DEDUP_WINDOW, DEDUP_WINDOW_SECONDS, Granularity, MigrationStep, PROJECT_ID, TableDef,
    TableKind, Ty, route, table,
};

/// The seeded faults of the store. Rates are parts per million of the calls they apply to.
#[derive(Clone, Debug, Default)]
pub struct ChFaults {
    /// An insert times out and never lands.
    pub timeout_lost_ppm: u32,
    /// An insert times out while still in flight: it lands after a delay from `late_apply_micros`.
    pub timeout_late_ppm: u32,
    /// An insert lands and its ack is lost.
    pub ack_lost_ppm: u32,
    /// A server error (a 5xx with no known code): nothing lands, and the shipper cannot know that.
    pub server_error_ppm: u32,
    /// Code 252, too many parts.
    pub too_many_parts_ppm: u32,
    /// Code 241, memory.
    pub memory_ppm: u32,
    /// Code 202, too many simultaneous queries.
    pub too_many_queries_ppm: u32,
    /// Of the unknown outcomes, how many never answer at all, so that the caller's timeout fires.
    pub hang_ppm: u32,
    /// An insert that timed out in flight is the first sign of an overload: for a time drawn from
    /// `overload_micros` the store then sheds every insert with code 252, while probes still answer.
    pub overload_ppm: u32,
    /// How long an overload lasts, drawn uniformly from this range.
    pub overload_micros: (u64, u64),
    /// A probe fails.
    pub probe_error_ppm: u32,
    /// A pair is poison: the store refuses its block as a data error.
    pub poison_pair_ppm: u32,
    /// The first attempt of a poison pair is instead accepted and stays in flight.
    pub poison_first_in_flight_ppm: u32,
    /// A deduplication log entry that may expire does so, per entry and per store call.
    pub expire_ppm: u32,
    /// The answer delay, drawn uniformly from this range.
    pub latency_micros: (u64, u64),
    /// How long an insert in flight takes to land, drawn uniformly from this range.
    pub late_apply_micros: (u64, u64),
}

/// The batch identity a block carries in its rows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct BlockId {
    /// `wal_epoch`, or `marker_epoch` of the marker row.
    pub epoch: [u8; 16],
    /// `batch_seq`, or `marker_batch_seq` of the marker row.
    pub batch_seq: u64,
}

/// One landed block.
#[derive(Clone, Debug)]
pub struct Part {
    /// The partition.
    pub partition: PartitionId,
    /// The token the block landed under.
    pub token: BatchToken,
    /// The identity its rows carry.
    pub id: BlockId,
    /// The rows.
    pub block: Arc<DecodedBlock>,
    /// Virtual microseconds at which it landed.
    pub landed_at: u64,
}

/// A (batch, table, partition) pair as the store tells them apart.
pub type PairKey = (TableId, PartitionId, u64);

/// The last thing the store answered about a pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LastAnswer {
    /// An insert was acknowledged.
    InsertOk,
    /// An insert was answered with an unknown outcome, or not at all.
    InsertUnknown,
    /// An insert was rejected with a retryable error.
    InsertRetry,
    /// An insert was rejected as a data error.
    InsertPoison,
    /// A probe read present.
    ProbePresent,
    /// A probe read absent.
    ProbeAbsent,
}

/// Counters of the store.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ChStats {
    /// Insert calls.
    pub inserts: u64,
    /// Blocks that landed.
    pub landed: u64,
    /// Inserts the deduplication log absorbed.
    pub deduplicated: u64,
    /// Inserts answered with an unknown outcome.
    pub unknown: u64,
    /// Inserts that landed after their caller had given up.
    pub late: u64,
    /// Inserts rejected with a retryable error.
    pub rejected: u64,
    /// Inserts rejected as data errors.
    pub poisoned: u64,
    /// Probe calls.
    pub probes: u64,
    /// Probes that read present.
    pub probes_present: u64,
    /// Deduplication log entries that expired by the fault.
    pub expired: u64,
}

/// One call the store received, for directed tests.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Call {
    /// An insert.
    Insert {
        /// The token it carried.
        token: BatchToken,
        /// BLAKE3 of the compressed body.
        body: [u8; 32],
        /// The settings it carried.
        settings: Vec<(&'static str, String)>,
    },
    /// A probe.
    Probe {
        /// The pair it asked about.
        pair: PairKey,
    },
}

/// Decides the outcome of an insert ahead of the seeded faults: `Some` answers with that error and lands
/// nothing; `None` leaves the insert to the model. The arguments are the token and the number of this attempt
/// of its pair, from 1.
#[derive(Clone)]
pub struct Script(Arc<Decide>);

type Decide = dyn Fn(&BatchToken, u64) -> Option<InsertError> + Send + Sync;

impl Script {
    /// A script from a function.
    pub fn new(
        decide: impl Fn(&BatchToken, u64) -> Option<InsertError> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(decide))
    }
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Script")
    }
}

#[derive(Debug)]
struct DedupEntry {
    token: String,
    at: u64,
}

#[derive(Debug, Default)]
struct TableState {
    parts: Vec<Part>,
    dedup: VecDeque<DedupEntry>,
}

#[derive(Debug)]
struct Pending {
    due: u64,
    token: BatchToken,
    id: BlockId,
    block: Arc<DecodedBlock>,
}

#[derive(Debug)]
struct State {
    tables: BTreeMap<TableId, TableState>,
    pending: Vec<Pending>,
    outage_until: u64,
    overload_until: u64,
    faults: ChFaults,
    /// The last absent probe of each pair, for the expiry guard.
    absent_probes: BTreeMap<PairKey, u64>,
    /// Pairs of which the store accepted a block, whether or not the caller learned it.
    accepted: BTreeSet<PairKey>,
    attempts: BTreeMap<PairKey, u64>,
    last: BTreeMap<PairKey, LastAnswer>,
    /// Every LSN range a token ever named.
    ranges: BTreeSet<(u64, u64)>,
    violations: Vec<String>,
    stats: ChStats,
    script: Option<Script>,
    /// The calls received since a script was set.
    calls: Vec<Call>,
}

/// The simulated store. Clones share one state, which outlives every simulated host.
#[derive(Clone, Debug)]
pub struct ChModel {
    state: Arc<Mutex<State>>,
    origin: tokio::time::Instant,
    flavour: StoreFlavour,
    granularity: Granularity,
    /// How long an absent probe protects its pair's log entry: the shipper's `probe_valid_for`.
    probe_valid_for: Duration,
    salt: u64,
    outcome_rng: Arc<dyn Rng>,
    latency_rng: Arc<dyn Rng>,
    expire_rng: Arc<dyn Rng>,
    probe_rng: Arc<dyn Rng>,
}

fn ppm(rng: &dyn Rng, rate: u32) -> bool {
    rate > 0 && rng.next_u64() % 1_000_000 < u64::from(rate)
}

fn between(rng: &dyn Rng, (low, high): (u64, u64)) -> u64 {
    low + rng.next_u64() % (high.saturating_sub(low) + 1)
}

fn column<'a>(block: &'a DecodedBlock, name: &str) -> Option<&'a Values> {
    block
        .columns
        .iter()
        .find(|c| c.name == name)
        .map(|c| &c.values)
}

/// A `UInt64` cell.
pub fn u64_at(block: &DecodedBlock, name: &str, row: usize) -> Option<u64> {
    match column(block, name)? {
        Values::UInt64(v) => v.get(row).copied(),
        _ => None,
    }
}

/// A `UInt8` cell.
pub fn u8_at(block: &DecodedBlock, name: &str, row: usize) -> Option<u8> {
    match column(block, name)? {
        Values::UInt8(v) => v.get(row).copied(),
        _ => None,
    }
}

/// An `Enum8` cell.
pub fn i8_at(block: &DecodedBlock, name: &str, row: usize) -> Option<i8> {
    match column(block, name)? {
        Values::Int8(v) => v.get(row).copied(),
        _ => None,
    }
}

/// A `UUID` cell.
pub fn uuid_at(block: &DecodedBlock, name: &str, row: usize) -> Option<[u8; 16]> {
    match column(block, name)? {
        Values::Uuid(v) => v.get(row).copied(),
        _ => None,
    }
}

/// A `String` or `FixedString` cell.
pub fn bytes_at<'a>(block: &'a DecodedBlock, name: &str, row: usize) -> Option<&'a [u8]> {
    match column(block, name)? {
        Values::String(v) | Values::FixedString(_, v) => v.get(row).map(Vec::as_slice),
        _ => None,
    }
}

/// The instant of a time cell (`Date`, `DateTime` or `DateTime64`).
fn time_at(values: &Values, ty: &Ty, row: usize) -> Option<Timestamp> {
    let micros = match (ty.scalar(), values) {
        (Ty::Date, Values::UInt16(v)) => i64::from(*v.get(row)?) * 86_400_000_000,
        (Ty::DateTime, Values::UInt32(v)) => i64::from(*v.get(row)?) * 1_000_000,
        (Ty::DateTime64, Values::Int64(v)) => *v.get(row)?,
        _ => return None,
    };
    Some(Timestamp::from_micros(micros))
}

/// The order of two rows of one key column.
fn cell_order(values: &Values, a: usize, b: usize) -> Ordering {
    match values {
        Values::UInt8(v) => v[a].cmp(&v[b]),
        Values::UInt16(v) => v[a].cmp(&v[b]),
        Values::UInt32(v) => v[a].cmp(&v[b]),
        Values::UInt64(v) => v[a].cmp(&v[b]),
        Values::Int8(v) => v[a].cmp(&v[b]),
        Values::Int32(v) => v[a].cmp(&v[b]),
        Values::Int64(v) => v[a].cmp(&v[b]),
        Values::Uuid(v) => v[a].cmp(&v[b]),
        Values::String(v) | Values::FixedString(_, v) => v[a].cmp(&v[b]),
        _ => Ordering::Equal,
    }
}

/// Checks a decoded block against its table and returns the batch identity its rows carry.
fn check_block(
    def: &TableDef,
    block: &DecodedBlock,
    token: &BatchToken,
    granularity: Granularity,
) -> Result<BlockId, String> {
    let rows = block.rows;
    if rows == 0 || rows > MAX_BLOCK_ROWS {
        return Err(format!("{rows} rows"));
    }
    if block.columns.len() != def.columns.len() {
        return Err(format!("{} columns", block.columns.len()));
    }
    for (sent, column) in block.columns.iter().zip(def.columns) {
        if sent.name != column.name || sent.ty != column.ty.wire().to_string() {
            return Err(format!(
                "column `{}` of type `{}` where `{}` is expected",
                sent.name, sent.ty, column.name
            ));
        }
    }
    if let Some(name) = def.partition.column() {
        let ty = &def.column(name).ok_or("no partition column")?.ty;
        let values = column(block, name).ok_or("no partition column")?;
        for row in 0..rows {
            let routed = time_at(values, ty, row)
                .and_then(|t| route(def.id, granularity, t).ok())
                .ok_or_else(|| format!("row {row} has no partition"))?;
            if routed != token.partition() {
                return Err(format!(
                    "row {row} belongs to partition {routed}, the insert names {}",
                    token.partition()
                ));
            }
        }
    } else if !token.partition().is_unpartitioned() {
        return Err("a partition for an unpartitioned table".to_owned());
    }
    let keys: Vec<&Values> = def
        .order_by
        .iter()
        .filter_map(|name| column(block, name))
        .collect();
    if keys.len() != def.order_by.len() {
        return Err("a key column is missing".to_owned());
    }
    let key_order = |a: usize, b: usize| {
        keys.iter()
            .map(|values| cell_order(values, a, b))
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    };
    if let Some(row) = (1..rows).find(|&row| key_order(row - 1, row).is_gt()) {
        return Err(format!("row {row} is out of ORDER BY order"));
    }
    let seq = token.batch_seq().get();
    let missing = || "an identity column is missing".to_owned();
    match def.kind {
        TableKind::Plain => {
            let epoch = uuid_at(block, "wal_epoch", 0).ok_or_else(missing)?;
            for row in 0..rows {
                let lsn = u64_at(block, "lsn", row).ok_or_else(missing)?;
                let in_range = lsn >= seq && lsn <= token.last_lsn().get();
                if u64_at(block, "batch_seq", row) != Some(seq)
                    || uuid_at(block, "wal_epoch", row) != Some(epoch)
                    || !in_range
                {
                    return Err(format!("row {row} is not of batch {seq}"));
                }
            }
            Ok(BlockId {
                epoch,
                batch_seq: seq,
            })
        }
        TableKind::Aggregating => {
            let mut marker = None;
            for row in 0..rows {
                let project = uuid_at(block, PROJECT_ID, row).ok_or_else(missing)?;
                let epoch = uuid_at(block, "marker_epoch", row).ok_or_else(missing)?;
                let marker_seq = u64_at(block, "marker_batch_seq", row).ok_or_else(missing)?;
                if project == [0; 16] {
                    if marker.replace(epoch).is_some() || marker_seq != seq {
                        return Err("more than one marker row, or a marker of another batch".into());
                    }
                } else if epoch != [0; 16] || marker_seq != 0 {
                    return Err(format!("data row {row} carries marker columns"));
                }
            }
            let epoch = marker.ok_or("no marker row")?;
            Ok(BlockId {
                epoch,
                batch_seq: seq,
            })
        }
    }
}

fn pair_key(token: &BatchToken) -> PairKey {
    (token.table(), token.partition(), token.batch_seq().get())
}

impl State {
    fn violate(&mut self, what: String) {
        if self.violations.len() < 16 {
            self.violations.push(what);
        }
    }

    /// Lands a block unless its token is in the table's deduplication log.
    fn apply(&mut self, at: u64, token: &BatchToken, id: BlockId, block: &Arc<DecodedBlock>) {
        let text = token.text().as_str().to_owned();
        let state = self.tables.entry(token.table()).or_default();
        if state.dedup.iter().any(|e| e.token == text) {
            self.stats.deduplicated += 1;
            return;
        }
        let twice = state
            .parts
            .iter()
            .any(|p| p.partition == token.partition() && p.id == id);
        state.parts.push(Part {
            partition: token.partition(),
            token: *token,
            id,
            block: Arc::clone(block),
            landed_at: at,
        });
        state.dedup.push_back(DedupEntry { token: text, at });
        while state.dedup.len() > DEDUP_WINDOW as usize {
            state.dedup.pop_front();
        }
        self.stats.landed += 1;
        if twice {
            self.violate(format!(
                "I2: a second block of batch {} landed in {} partition {}",
                id.batch_seq,
                token.table().name(),
                token.partition()
            ));
        }
    }
}

impl ChModel {
    /// An empty store. Its fault streams are forks of `rng`. `probe_valid_for` is the shipper's setting: the
    /// expiry fault honours an absent probe for that long.
    pub fn new(
        rng: &dyn Rng,
        flavour: StoreFlavour,
        granularity: Granularity,
        probe_valid_for: Duration,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                tables: BTreeMap::new(),
                pending: Vec::new(),
                outage_until: 0,
                overload_until: 0,
                faults: ChFaults::default(),
                absent_probes: BTreeMap::new(),
                accepted: BTreeSet::new(),
                attempts: BTreeMap::new(),
                last: BTreeMap::new(),
                ranges: BTreeSet::new(),
                violations: Vec::new(),
                stats: ChStats::default(),
                script: None,
                calls: Vec::new(),
            })),
            origin: tokio::time::Instant::now(),
            flavour,
            granularity,
            probe_valid_for,
            salt: rng.fork("ch/poison").next_u64(),
            outcome_rng: rng.fork("ch/outcome"),
            latency_rng: rng.fork("ch/latency"),
            expire_rng: rng.fork("ch/expire"),
            probe_rng: rng.fork("ch/probe"),
        }
    }

    fn now(&self) -> u64 {
        let elapsed = tokio::time::Instant::now().saturating_duration_since(self.origin);
        u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
    }

    /// Brings the state to `now`: lands the inserts in flight that are due, then expires log entries.
    fn settled(&self) -> MutexGuard<'_, State> {
        let now = self.now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut due: Vec<Pending> = Vec::new();
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut state.pending) {
            if pending.due <= now {
                due.push(pending);
            } else {
                waiting.push(pending);
            }
        }
        state.pending = waiting;
        due.sort_by_key(|p| p.due);
        for pending in due {
            state.stats.late += 1;
            state.apply(pending.due, &pending.token, pending.id, &pending.block);
        }

        let valid_for = u64::try_from(self.probe_valid_for.as_micros()).unwrap_or(u64::MAX);
        let age_window = match self.flavour {
            StoreFlavour::SingleNode => None,
            StoreFlavour::Clustered | StoreFlavour::Cloud => {
                Some(u64::from(DEDUP_WINDOW_SECONDS) * 1_000_000)
            }
        };
        let State {
            tables,
            pending,
            absent_probes,
            faults,
            stats,
            ..
        } = &mut *state;
        for (table_id, table_state) in tables.iter_mut() {
            table_state.dedup.retain(|entry| {
                if age_window.is_some_and(|window| now.saturating_sub(entry.at) > window) {
                    return false;
                }
                if faults.expire_ppm == 0 {
                    return true;
                }
                let Ok(token) = BatchToken::parse(&entry.token) else {
                    return true;
                };
                let in_flight = pending
                    .iter()
                    .any(|p| p.token.text().as_str() == entry.token);
                let key = (*table_id, token.partition(), token.batch_seq().get());
                let probe_holds = absent_probes
                    .get(&key)
                    .is_some_and(|&at| now < at.saturating_add(valid_for));
                if in_flight || probe_holds || !ppm(self.expire_rng.as_ref(), faults.expire_ppm) {
                    return true;
                }
                stats.expired += 1;
                false
            });
        }
        state
    }

    /// Replaces the faults.
    pub fn set_faults(&self, faults: ChFaults) {
        self.settled().faults = faults;
    }

    /// Sets the script that decides insert outcomes ahead of the faults, and starts recording calls.
    pub fn set_script(&self, script: Script) {
        self.settled().script = Some(script);
    }

    /// The calls received since [`ChModel::set_script`], in order.
    pub fn calls(&self) -> Vec<Call> {
        self.settled().calls.clone()
    }

    /// Makes the store unreachable for `duration` from now. Inserts already in flight still land.
    pub fn start_outage(&self, duration: Duration) {
        let until = self
            .now()
            .saturating_add(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX));
        self.settled().outage_until = until;
    }

    /// Ends an outage and an overload.
    pub fn end_outage(&self) {
        let mut state = self.settled();
        state.outage_until = 0;
        state.overload_until = 0;
    }

    /// When the last insert in flight lands, in virtual microseconds; zero when none is in flight.
    pub fn in_flight_until(&self) -> u64 {
        self.settled()
            .pending
            .iter()
            .map(|p| p.due)
            .max()
            .unwrap_or(0)
    }

    /// The landed parts of a table, in landing order.
    pub fn parts(&self, table_id: TableId) -> Vec<Part> {
        self.settled()
            .tables
            .get(&table_id)
            .map(|t| t.parts.clone())
            .unwrap_or_default()
    }

    /// What the model itself found wrong: a malformed block, or a second block of one pair.
    pub fn violations(&self) -> Vec<String> {
        self.settled().violations.clone()
    }

    /// The counters.
    pub fn stats(&self) -> ChStats {
        self.settled().stats
    }

    /// Whether the store ever accepted a block of the pair, and its last answer about the pair.
    pub fn pair_history(&self, key: &PairKey) -> (bool, Option<LastAnswer>) {
        let state = self.settled();
        (state.accepted.contains(key), state.last.get(key).copied())
    }

    /// Two LSN ranges that tokens named and that overlap without being equal, if any: two batch identities
    /// over the same frames have been on the wire.
    pub fn overlapping_ranges(&self) -> Option<((u64, u64), (u64, u64))> {
        let state = self.settled();
        let mut previous: Option<(u64, u64)> = None;
        for &range in &state.ranges {
            if let Some(before) = previous
                && range.0 <= before.1
            {
                return Some((before, range));
            }
            previous = Some(range);
        }
        None
    }

    /// A digest of every landed part, for comparing the store before and after a restart.
    pub fn digest(&self) -> String {
        let state = self.settled();
        let mut hasher = blake3::Hasher::new();
        for (table_id, table_state) in &state.tables {
            hasher.update(&[table_id.id()]);
            for part in &table_state.parts {
                hasher.update(part.token.text().as_bytes());
                hasher.update(&(part.block.rows as u64).to_le_bytes());
                hasher.update(&part.landed_at.to_le_bytes());
            }
        }
        hasher.finalize().to_hex().to_string()
    }

    fn is_poison(&self, key: &PairKey, rate: u32) -> bool {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.salt.to_le_bytes());
        hasher.update(&[key.0.id()]);
        hasher.update(key.1.to_string().as_bytes());
        hasher.update(&key.2.to_le_bytes());
        let mut word = [0u8; 8];
        word.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
        rate > 0 && u64::from_le_bytes(word) % 1_000_000 < u64::from(rate)
    }

    fn decode(
        &self,
        block: &EncodedBlock,
        token: &BatchToken,
    ) -> Result<(BlockId, DecodedBlock), String> {
        let native = decompress(block.compressed()).map_err(|e| e.to_string())?;
        let (decoded, used) = decode_block(&native).map_err(|e| e.to_string())?;
        if used != native.len() || decoded.rows != block.rows() {
            return Err("trailing bytes or a wrong row count".to_owned());
        }
        let id = check_block(table(token.table()), &decoded, token, self.granularity)?;
        Ok((id, decoded))
    }
}

/// What the store does with one insert.
enum Outcome {
    Ack,
    AckLost,
    Late(u64),
    Lost,
    Retry(RetryReason),
    Poison,
}

fn unknown(message: &str) -> InsertError {
    InsertError::UnknownOutcome {
        code: None,
        message: message.to_owned(),
    }
}

fn internal<T>() -> Result<T, StoreError> {
    Err(StoreError::Internal(
        "the store model has no read or maintenance side yet".to_owned(),
    ))
}

#[async_trait]
impl TelemetryStore for ChModel {
    async fn insert_block(
        &self,
        block: EncodedBlock,
        settings: InsertSettings,
    ) -> Result<(), InsertError> {
        let token = settings.token();
        let key = pair_key(&token);
        let (reply, latency, hang) = {
            let mut state = self.settled();
            let now = self.now();
            state.stats.inserts += 1;
            state
                .ranges
                .insert((token.first_lsn().get(), token.last_lsn().get()));
            let faults = state.faults.clone();
            let latency = between(self.latency_rng.as_ref(), faults.latency_micros);
            let rng = self.outcome_rng.as_ref();
            let attempt = {
                let attempts = state.attempts.entry(key).or_default();
                *attempts += 1;
                *attempts
            };
            let scripted = state.script.clone().and_then(|script| {
                state.calls.push(Call::Insert {
                    token,
                    body: *blake3::hash(block.compressed()).as_bytes(),
                    settings: settings.pairs(),
                });
                (script.0)(&token, attempt)
            });
            if let Some(error) = scripted {
                let answer = match &error {
                    InsertError::Poison { .. } => LastAnswer::InsertPoison,
                    InsertError::UnknownOutcome { .. } => LastAnswer::InsertUnknown,
                    _ => LastAnswer::InsertRetry,
                };
                state.last.insert(key, answer);
                return Err(error);
            }
            let outcome = if now < state.outage_until {
                if ppm(rng, 200_000) {
                    Outcome::Lost
                } else {
                    Outcome::Retry(RetryReason::NotSent)
                }
            } else if now < state.overload_until {
                Outcome::Retry(RetryReason::TooManyParts)
            } else if self.is_poison(&key, faults.poison_pair_ppm) {
                if attempt == 1 && ppm(rng, faults.poison_first_in_flight_ppm) {
                    Outcome::Late(between(rng, faults.late_apply_micros))
                } else {
                    Outcome::Poison
                }
            } else {
                let draw = u32::try_from(rng.next_u64() % 1_000_000).unwrap_or(0);
                let mut threshold = 0u32;
                let mut hit = |rate: u32| {
                    threshold = threshold.saturating_add(rate);
                    draw < threshold
                };
                if hit(faults.timeout_lost_ppm) || hit(faults.server_error_ppm) {
                    Outcome::Lost
                } else if hit(faults.timeout_late_ppm) {
                    Outcome::Late(between(rng, faults.late_apply_micros))
                } else if hit(faults.ack_lost_ppm) {
                    Outcome::AckLost
                } else if hit(faults.too_many_parts_ppm) {
                    Outcome::Retry(RetryReason::TooManyParts)
                } else if hit(faults.memory_ppm) {
                    Outcome::Retry(RetryReason::Memory)
                } else if hit(faults.too_many_queries_ppm) {
                    Outcome::Retry(RetryReason::TooManyQueries)
                } else {
                    Outcome::Ack
                }
            };
            let accepts = matches!(outcome, Outcome::Ack | Outcome::AckLost | Outcome::Late(_));
            let decoded = if accepts {
                match self.decode(&block, &token) {
                    Ok((id, decoded)) => Some((id, Arc::new(decoded))),
                    Err(why) => {
                        state.violate(format!(
                            "malformed block for {}: {why}",
                            token.text().as_str()
                        ));
                        None
                    }
                }
            } else {
                None
            };
            let (reply, answer) = match (outcome, decoded) {
                (Outcome::Ack | Outcome::AckLost | Outcome::Late(_), None)
                | (Outcome::Poison, _) => {
                    state.stats.poisoned += 1;
                    let refused = InsertError::Poison {
                        code: 27,
                        message: "cannot parse input".to_owned(),
                    };
                    (Err(refused), LastAnswer::InsertPoison)
                }
                (Outcome::Ack, Some((id, decoded))) => {
                    state.accepted.insert(key);
                    state.apply(now, &token, id, &decoded);
                    (Ok(()), LastAnswer::InsertOk)
                }
                (Outcome::AckLost, Some((id, decoded))) => {
                    state.accepted.insert(key);
                    state.apply(now, &token, id, &decoded);
                    (Err(unknown("connection reset")), LastAnswer::InsertUnknown)
                }
                (Outcome::Late(delay), Some((id, block))) => {
                    if ppm(rng, faults.overload_ppm) {
                        state.overload_until =
                            now.saturating_add(between(rng, faults.overload_micros));
                    }
                    state.accepted.insert(key);
                    state.pending.push(Pending {
                        due: now.saturating_add(delay),
                        token,
                        id,
                        block,
                    });
                    (Err(unknown("timeout")), LastAnswer::InsertUnknown)
                }
                (Outcome::Lost, _) => (Err(unknown("timeout")), LastAnswer::InsertUnknown),
                (Outcome::Retry(reason), _) => {
                    state.stats.rejected += 1;
                    let rejected = InsertError::Retry {
                        reason,
                        message: "rejected".to_owned(),
                    };
                    (Err(rejected), LastAnswer::InsertRetry)
                }
            };
            state.last.insert(key, answer);
            let is_unknown = answer == LastAnswer::InsertUnknown;
            if is_unknown {
                state.stats.unknown += 1;
            }
            (reply, latency, is_unknown && ppm(rng, faults.hang_ppm))
        };
        tokio::time::sleep(Duration::from_micros(latency)).await;
        if hang {
            futures::future::pending::<()>().await;
        }
        reply
    }

    async fn execute(&self, _plan: QueryPlan) -> Result<RowStream, QueryError> {
        Err(QueryError::Internal(
            "the store model has no plan interpreter yet".to_owned(),
        ))
    }

    async fn probe(&self, probe: BatchProbe) -> Result<bool, StoreError> {
        let (reply, latency) = {
            let mut state = self.settled();
            let now = self.now();
            state.stats.probes += 1;
            let latency = between(self.latency_rng.as_ref(), state.faults.latency_micros);
            let failed = now < state.outage_until
                || ppm(self.probe_rng.as_ref(), state.faults.probe_error_ppm);
            if failed {
                (
                    Err(StoreError::Unavailable("no answer".to_owned())),
                    latency,
                )
            } else {
                let id = BlockId {
                    epoch: probe.epoch().to_bytes(),
                    batch_seq: probe.batch_seq().get(),
                };
                // The scan a server runs: the rows of plain tables, the marker rows of aggregating ones.
                let aggregating = table(probe.table()).kind == TableKind::Aggregating;
                let present = state.tables.get(&probe.table()).is_some_and(|t| {
                    t.parts
                        .iter()
                        .filter(|p| p.partition == probe.partition())
                        .any(|p| {
                            (0..p.block.rows).any(|row| {
                                if aggregating {
                                    uuid_at(&p.block, PROJECT_ID, row) == Some([0; 16])
                                        && uuid_at(&p.block, "marker_epoch", row) == Some(id.epoch)
                                        && u64_at(&p.block, "marker_batch_seq", row)
                                            == Some(id.batch_seq)
                                } else {
                                    uuid_at(&p.block, "wal_epoch", row) == Some(id.epoch)
                                        && u64_at(&p.block, "batch_seq", row) == Some(id.batch_seq)
                                }
                            })
                        })
                });
                let key = (probe.table(), probe.partition(), id.batch_seq);
                if state.script.is_some() {
                    state.calls.push(Call::Probe { pair: key });
                }
                if present {
                    state.stats.probes_present += 1;
                    state.last.insert(key, LastAnswer::ProbePresent);
                } else {
                    state.absent_probes.insert(key, now);
                    state.last.insert(key, LastAnswer::ProbeAbsent);
                }
                (Ok(present), latency)
            }
        };
        tokio::time::sleep(Duration::from_micros(latency)).await;
        reply
    }

    async fn delete(&self, _plan: DeletePlan) -> Result<MutationId, StoreError> {
        internal()
    }

    async fn drop_partition(
        &self,
        _table: TableId,
        _partition: PartitionId,
    ) -> Result<(), StoreError> {
        internal()
    }

    async fn apply_deleted_mask(
        &self,
        _table: TableId,
        _partition: PartitionId,
    ) -> Result<MutationId, StoreError> {
        internal()
    }

    async fn mutation_status(&self, _id: &MutationId) -> Result<MutationStatus, StoreError> {
        internal()
    }

    async fn partition_projects(
        &self,
        _table: TableId,
        _partition: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError> {
        internal()
    }

    async fn masked_parts(
        &self,
        _table: TableId,
        _partition: PartitionId,
    ) -> Result<u64, StoreError> {
        internal()
    }

    async fn apply_migration(&self, _step: &MigrationStep) -> Result<(), StoreError> {
        internal()
    }

    async fn detect(&self) -> Result<StoreFacts, StoreError> {
        let family = match self.flavour {
            StoreFlavour::SingleNode => EngineFamily::MergeTree,
            StoreFlavour::Clustered => EngineFamily::Replicated,
            StoreFlavour::Cloud => EngineFamily::Shared,
        };
        Ok(StoreFacts {
            version: StoreVersion::MINIMUM,
            flavour: self.flavour,
            engines: TableId::ALL.iter().map(|&id| (id, family)).collect(),
            missing_settings: Vec::new(),
            text_search_functions: true,
            text_index: true,
            ddl_initial_user_and_roles: matches!(self.flavour, StoreFlavour::Clustered)
                .then_some(true),
        })
    }

    async fn dedup_selftest(&self) -> Result<(), StoreError> {
        Ok(())
    }

    async fn kill(&self, _query_id: Uuid) -> Result<(), StoreError> {
        Ok(())
    }

    async fn health(&self) -> StoreHealth {
        let state = self.settled();
        if self.now() < state.outage_until {
            StoreHealth::Unavailable {
                reason: "outage".to_owned(),
            }
        } else {
            StoreHealth::Healthy
        }
    }
}
