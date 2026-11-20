//! Directed tests of stage B's deletion rules on one node with the real WAL and the real shipper (contract 13.3,
//! step 3): the frames after a `Tombstone` frame ship nothing the deletion selects that arrived before the
//! deletion's `requested_at`, and what arrived at or after it is kept; and the deletion requests of the control
//! store apply before anything ships, also those that completed while the node was down and so left no tombstone
//! in its log.
//!
//! Each scenario also runs with the seeded defect it guards against, which must let the deleted spans through.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ops::Range;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_env::{StoreFlavour, Stores};
use crucible_normalize::{Tombstone, TombstoneSelector};
use crucible_plan::Value;
use crucible_records::{
    DeletionRequestId, DeletionState, Lsn, ProjectId, TableId, Timestamp, TraceId,
};
use crucible_schema::{Granularity, table};
use crucible_shipper::{
    DeletionRequests, DeletionsUnavailable, Hooks, KnownDeletion, Mutant, Shipper, ShipperConfig,
};
use crucible_sim::ch_model::ChModel;
use crucible_sim::ship_sim::{SpanSpec, request_payload};
use crucible_sim::{Host, SIM_EPOCH_MICROS, run};
use crucible_wal::{RecoveryReport, Wal, WalConfig};
use crucible_wal_codec::{self as codec, Compression, PreparedFrame, Record, RecordType};
use futures::future::BoxFuture;

const PROJECT: [u8; 16] = [1; 16];

/// The deletion requests of a control store: a list the test fills, whose first reads may fail.
#[derive(Debug, Default)]
struct Requests {
    known: Mutex<Vec<KnownDeletion>>,
    /// Reads of every request that fail before one answers.
    failures: AtomicU32,
    /// Reads of every request that were answered.
    answered: AtomicU32,
}

impl DeletionRequests for Requests {
    fn configured(&self) -> bool {
        true
    }

    fn all(&self) -> BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>> {
        let failed = self
            .failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if !failed {
            self.answered.fetch_add(1, Ordering::SeqCst);
        }
        let known = self.known.lock().unwrap().clone();
        Box::pin(async move {
            if failed {
                Err(DeletionsUnavailable(
                    "the control store did not answer".to_owned(),
                ))
            } else {
                Ok(known)
            }
        })
    }

    fn request(
        &self,
        project: ProjectId,
        id: DeletionRequestId,
    ) -> BoxFuture<'_, Result<Option<KnownDeletion>, DeletionsUnavailable>> {
        let found = self
            .known
            .lock()
            .unwrap()
            .iter()
            .find(|known| known.tombstone.project_id == project && known.tombstone.request_id == id)
            .cloned();
        Box::pin(async move { Ok(found) })
    }
}

struct Node {
    host: Host,
    wal: Wal,
    report: RecoveryReport,
    ch: ChModel,
    requests: Arc<Requests>,
}

impl Node {
    async fn sleep(&self, d: Duration) {
        self.host.env.clock.sleep(d).await;
    }

    fn now(&self) -> Timestamp {
        self.host.env.clock.wall_now()
    }

    /// Starts the shipper, with a seeded defect when one is given.
    fn start(&self, mutant: Option<Mutant>) {
        let config = ShipperConfig {
            linger: Duration::from_millis(10),
            tail_poll: Duration::from_millis(1),
            backoff_min: Duration::from_millis(5),
            backoff_max: Duration::from_millis(50),
            mutant,
            ..ShipperConfig::default()
        };
        let hooks = Hooks {
            deletions: Arc::clone(&self.requests) as Arc<dyn DeletionRequests>,
            ..Hooks::default()
        };
        let stores = Stores::new(Arc::new(self.ch.clone()));
        let shipper = Shipper::new(
            &self.host.env,
            &stores,
            self.wal.clone(),
            &self.report,
            config,
            hooks,
        )
        .unwrap();
        drop(self.host.env.spawn("shipper", async move {
            Box::pin(shipper.run()).await.unwrap();
        }));
    }

    /// Appends one request with the spans `spans` of the trace `tag` and returns its LSN once durable.
    async fn ingest(&self, tag: u8, spans: Range<u8>) -> Lsn {
        let specs: Vec<SpanSpec> = spans
            .map(|i| {
                (
                    [tag; 16],
                    [tag, i, 1, 1, 1, 1, 1, 1],
                    SIM_EPOCH_MICROS + i64::from(i),
                    i == 0,
                )
            })
            .collect();
        let payload = request_payload(PROJECT, &specs, 0).unwrap();
        let frame = PreparedFrame::new(RecordType::IngestRequest, payload, Compression::Lz4);
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    /// The control store's request to delete the trace `tag`, requested at `requested_at`.
    fn deletion(tag: u8, requested_at: Timestamp, state: DeletionState) -> KnownDeletion {
        KnownDeletion {
            tombstone: Tombstone {
                request_id: DeletionRequestId::from_bytes([tag; 16]),
                project_id: ProjectId::from_bytes(PROJECT),
                requested_at,
                deletion_seq: u64::from(tag),
                selector: TombstoneSelector::Traces(vec![TraceId::from_bytes([tag; 16])]),
            },
            state,
        }
    }

    /// Appends the `Tombstone` frame of a deletion of the trace `tag` requested at `requested_at`.
    async fn tombstone(&self, tag: u8, requested_at: Timestamp) -> Lsn {
        let record = Record::Tombstone(codec::Tombstone {
            deletion_request_id: DeletionRequestId::from_bytes([tag; 16]),
            project_id: ProjectId::from_bytes(PROJECT),
            requested_at,
            deletion_seq: u64::from(tag),
            selector: codec::TombstoneSelector::Traces(vec![TraceId::from_bytes([tag; 16])]),
        });
        let frame = PreparedFrame::new(
            record.record_type(),
            record.encode().unwrap(),
            Compression::Lz4,
        );
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    /// The spans of the trace `tag` that the store holds in `span_core`.
    fn held(&self, tag: u8) -> usize {
        let at = table(TableId::SpanCore)
            .columns
            .iter()
            .position(|column| column.name == "trace_id")
            .unwrap();
        self.ch
            .held_rows(TableId::SpanCore)
            .iter()
            .filter(|(_, _, cells)| cells[at] == Value::Bytes(vec![tag; 16]))
            .count()
    }
}

/// One node: the real WAL on a simulated disk, the store model and a control store's deletion requests. The test
/// starts the shipper.
fn directed<F, Fut, T>(body: F) -> T
where
    F: FnOnce(Node) -> Fut,
    Fut: Future<Output = T>,
{
    run(13, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let ch = ChModel::new(
            sim.rng("store").as_ref(),
            StoreFlavour::SingleNode,
            Granularity::Day,
            Duration::from_secs(60),
        );
        let (wal, report) = Box::pin(Wal::open(&host.env, WalConfig::default()))
            .await
            .unwrap();
        body(Node {
            host,
            wal,
            report,
            ch,
            requests: Arc::default(),
        })
        .await
    })
}

/// A deletion accepted on a node whose clock runs ahead of this one: spans that reach this node after the
/// tombstone can still have arrived before `requested_at`, and the deletion selects them. Returns what trace 1 and
/// trace 2 hold after the tombstone, and trace 1 after `requested_at`.
fn after_a_tombstone(mutant: Option<Mutant>) -> ((usize, usize), usize) {
    directed(|node| async move {
        node.start(mutant);
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        assert_eq!(node.held(1), 2);

        let requested_at = node.now().saturating_add_micros(10_000_000);
        node.tombstone(1, requested_at).await;
        node.ingest(1, 2..4).await;
        node.ingest(2, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        let after_tombstone = (node.held(1), node.held(2));

        // Spans that arrive at or after `requested_at` are new data.
        node.sleep(Duration::from_secs(10)).await;
        node.ingest(1, 4..5).await;
        node.sleep(Duration::from_secs(1)).await;
        (after_tombstone, node.held(1))
    })
}

#[test]
fn the_frames_after_a_tombstone_ship_nothing_it_selects_that_arrived_before_it() {
    // The two spans of trace 1 that arrived after the tombstone are deleted; trace 2 is not.
    assert_eq!(after_a_tombstone(None), ((2, 2), 3));
    assert_eq!(
        after_a_tombstone(Some(Mutant::SkipTombstoneFilter)),
        ((4, 2), 5)
    );
}

/// The frames of traces 3 and 4 are in the log and were never shipped, and a request deleted trace 3 while the
/// node was down: it completed without this node, which never logged its tombstone. Returns what traces 3 and 4
/// hold once the shipper has run.
fn completed_while_down(mutant: Option<Mutant>) -> (usize, usize) {
    directed(|node| async move {
        node.ingest(3, 0..3).await;
        node.ingest(4, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        let deletion = Node::deletion(3, node.now(), DeletionState::Completed);
        node.requests.known.lock().unwrap().push(deletion);
        node.start(mutant);
        node.sleep(Duration::from_secs(2)).await;
        (node.held(3), node.held(4))
    })
}

#[test]
fn a_request_that_completed_while_the_node_was_down_applies_before_anything_ships() {
    assert_eq!(completed_while_down(None), (0, 2));
    // Reading only the open requests misses it, and the shipper inserts what it deleted.
    assert_eq!(
        completed_while_down(Some(Mutant::SeedOnlyOpenRequests)),
        (3, 2)
    );
    assert_eq!(
        completed_while_down(Some(Mutant::SkipTombstoneFilter)),
        (3, 2)
    );
}

#[test]
fn nothing_ships_until_the_deletion_requests_can_be_read() {
    let (before, after, answered) = directed(|node| async move {
        node.requests.failures.store(4, Ordering::SeqCst);
        node.ingest(5, 0..2).await;
        node.ingest(6, 0..2).await;
        let deletion = Node::deletion(
            5,
            node.now().saturating_add_micros(1),
            DeletionState::Pending,
        );
        node.requests.known.lock().unwrap().push(deletion);
        node.start(None);
        // The reads fail for a while: trace 6, which no request deletes, does not ship either.
        node.sleep(Duration::from_millis(12)).await;
        let before = (node.held(5), node.held(6));
        node.sleep(Duration::from_secs(2)).await;
        let after = (node.held(5), node.held(6));
        (before, after, node.requests.answered.load(Ordering::SeqCst))
    });
    assert_eq!(before, (0, 0));
    assert_eq!(after, (0, 2));
    assert_eq!(answered, 1);
}
