// -*- mode: Bluespec; -*-
//
// Formal model of the Crucible ingest protocol: WAL group commit and the ack rule (contract section 4),
// the insert protocol with sealed batches, tokens, the existence probe, quarantine and the checkpoint low
// watermark (section 5), and the recent-span index (section 8.6).
//
// The model is one node with one `wal_epoch` and one ClickHouse store. `docs/testing/formal-model.md` lists
// what is modelled, what is abstracted away, and how to run every check.

module ingest {
  // ---------------------------------------------------------------------------------------------------------
  // Parameters
  // ---------------------------------------------------------------------------------------------------------

  /// Span keys a client may send. A span routes to partition `span % 2` of every table.
  const SPANS: Set[int]
  /// Table ids. Every span yields one row per table.
  const TABLES: Set[int]
  /// Entries the store keeps in its deduplication log, per table.
  const WINDOW: int
  /// Frames per WAL segment.
  const SEG: int
  /// Entries the recent-span index holds before the gap rule drops the oldest.
  const RECENT_CAP: int
  /// Identical data failures of one pair before it is quarantined (3 in the contract).
  const POISON_LIMIT: int
  /// Bound on `IngestRequest` frames appended in one behaviour (first sends plus client retries).
  const MAX_FRAMES: int
  /// Bound on faults in one behaviour: crashes, lost insert acks and rejected inserts.
  const MAX_FAULTS: int
  /// Which protocol rule is broken: "none" for the contract, or one of the mutant names below.
  const MUTANT: str

  /// Mutant: the ack is sent after the append, before `sync_data`.
  pure val ackBeforeSync = MUTANT == "ack-before-sync"
  /// Mutant: an insert is sent before the batch's `BatchSeal` is durable.
  pure val sendBeforeSeal = MUTANT == "send-before-seal"
  /// Mutant: every attempt carries a new deduplication identity.
  pure val dropToken = MUTANT == "drop-token"
  /// Mutant: segment GC ignores the checkpoint watermark.
  pure val truncateEarly = MUTANT == "truncate-early"
  /// Mutant: a retry after an unknown outcome is sent without the existence probe.
  pure val noProbe = MUTANT == "no-probe-after-window-expiry"
  /// Assumption lifted: a deduplication log entry may expire while a copy of its insert is in flight.
  pure val expiresInFlight = MUTANT == "window-expires-in-flight"
  /// Assumption lifted: a deduplication log entry may expire between a probe that read "absent" and the resend.
  pure val expiresBeforeResend = MUTANT == "window-expires-before-resend"
  /// Narrow reading of section 5.5: after a known rejection the shipper resends without a probe, even when an
  /// earlier attempt of the pair had an unknown outcome.
  pure val retryWithoutReprobe = MUTANT == "retry-without-reprobe"

  // ---------------------------------------------------------------------------------------------------------
  // Types
  // ---------------------------------------------------------------------------------------------------------

  /// One row: the span carried by the `IngestRequest` frame at `lsn`.
  type Row = { lsn: int, span: int }
  /// Batch identity `(first_lsn, last_lsn)`; the epoch is constant in the model.
  type Batch = { first: int, last: int }
  /// A batch the shipper is working on. `sealLsn` is 0 until the `BatchSeal` frame is appended.
  type OpenBatch = { first: int, last: int, sealLsn: int }
  /// A (batch, table, partition) pair.
  type Pair = { first: int, last: int, table: int, part: int }
  /// The deduplication token. `gen` is always 0 under the contract; only the drop-token mutant varies it.
  type Token = { pair: Pair, gen: int }
  /// One INSERT carrying one block.
  type Insert = { id: int, tok: Token, rows: Set[Row] }

  type Frame =
    | Ingest(int)
    | Seal(Batch)
    | Ckpt(int)
    | Quar(Pair)

  /// What the shipper knows about a pair.
  type PairState =
    | Fresh      // never sent by this process
    | Sent(int)  // the insert with this id is awaiting its response; no earlier attempt is in doubt
    | Resent(int) // as `Sent`, after an earlier attempt of the pair had an unknown outcome
    | Unknown    // outcome unknown: timeout, reset, or inherited from before a crash
    | Absent     // the probe read "absent"; a send is allowed
    | Retry      // the store rejected the insert; nothing landed
    | Parked     // quarantined; resolved once the `Quarantine` frame is durable
    | Done       // confirmed present

  pure def partOf(span: int): int = span % 2
  pure def max(a: int, b: int): int = if (a > b) a else b
  pure def isIngest(f: Frame): bool = match f { | Ingest(_) => true | _ => false }
  pure def spanOf(f: Frame): int = match f { | Ingest(s) => s | _ => 0 }
  pure def isSent(st: PairState): bool = match st { | Sent(_) => true | Resent(_) => true | _ => false }

  // ---------------------------------------------------------------------------------------------------------
  // State
  // ---------------------------------------------------------------------------------------------------------

  // Client.
  var sent: Set[int]            // spans sent at least once
  var acked: Set[Row]           // every ack ever issued, with the LSN it was issued for

  // Disk. Survives a crash, except the unsynced WAL tail.
  var wal: int -> Frame         // retained frames by LSN
  var lastLsn: int              // last appended LSN
  var durableLsn: int           // last LSN covered by a successful `sync_data`
  var gcThrough: int            // every segment at or below this LSN was removed
  var quarantine: Set[Pair]     // pairs copied under the quarantine root

  // Process memory. Lost by a crash.
  var up: bool
  var pending: Set[int]         // received, waiting for the committer
  var waiters: Set[Row]         // appended, waiting for the ack
  var open: Set[OpenBatch]      // batches formed and not yet checkpointed
  var formedThrough: int        // last LSN assigned to a batch
  var ckptThrough: int          // watermark of the last appended `Checkpoint`
  var pst: Pair -> PairState
  var fails: Pair -> int        // data failures per pair
  var recent: Set[int]          // recent-span index entries, by LSN
  var recentApplied: int        // stage B has applied every frame through this LSN
  var gapThrough: int           // upper LSN of the index gap (section 8.6); 0 when there is none

  // Network and store. Survive a crash of the node.
  var requests: Set[Insert]     // inserts sent and not yet processed by the store
  var replies: Set[int]         // insert ids whose success response is on its way back
  var nextMsg: int
  var chParts: Set[Insert]      // landed blocks; `id` keeps two landings of one block apart
  var dedup: Set[Token]         // deduplication log, all tables

  // Bounds and ghosts.
  var frames: int               // `IngestRequest` frames appended so far
  var faults: int               // faults injected so far
  var wire: Set[Batch]          // ghost: every batch identity that was ever put on the wire
  var prevWatermark: int        // ghost: the watermark before the last step
  var lastAction: str           // ghost: label of the last step

  // ---------------------------------------------------------------------------------------------------------
  // Derived state
  // ---------------------------------------------------------------------------------------------------------

  val lsns = wal.keys()
  val durable = lsns.filter(l => l <= durableLsn)

  /// The checkpoint low watermark: the highest durable `Checkpoint` frame. WAL records are authoritative.
  val watermark = durable.fold(0, (acc, l) => match wal.get(l) { | Ckpt(w) => max(acc, w) | _ => acc })

  /// Every durable `BatchSeal`, as (identity, LSN of the seal frame).
  val durableSeals: Set[OpenBatch] = durable.fold(Set(), (acc, l) => match wal.get(l) {
    | Seal(b) => acc.union(Set({ first: b.first, last: b.last, sealLsn: l }))
    | _ => acc
  })

  def quarDurable(k: Pair): bool = durable.exists(l => wal.get(l) == Quar(k))

  /// Rows of the retained `IngestRequest` frames in an LSN range. Control frames contribute no rows.
  def rowsIn(first: int, last: int): Set[Row] =
    lsns.filter(l => first <= l and l <= last and isIngest(wal.get(l)))
        .map(l => { lsn: l, span: spanOf(wal.get(l)) })

  def rowsOf(k: Pair): Set[Row] = rowsIn(k.first, k.last).filter(r => partOf(r.span) == k.part)

  /// The (table, partition) pairs of a batch that have at least one row.
  def pairsOf(first: int, last: int): Set[Pair] =
    tuples(TABLES, rowsIn(first, last).map(r => partOf(r.span)))
      .map(tp => { first: first, last: last, table: tp._1, part: tp._2 })

  def sealDurable(b: OpenBatch): bool = b.sealLsn > 0 and b.sealLsn <= durableLsn
  def isOpen(k: Pair): bool = open.exists(b => b.first == k.first and b.last == k.last)
  def sendable(k: Pair): bool =
    open.exists(b => b.first == k.first and b.last == k.last and (sendBeforeSeal or sealDurable(b)))

  def resolved(k: Pair): bool = or {
    pst.get(k) == Done,
    pst.get(k) == Parked and quarDurable(k),
  }

  /// The existence probe: keyed by `batch_seq`, table and partition, never by the token.
  def probePresent(k: Pair): bool =
    chParts.exists(p => p.tok.pair.first == k.first and p.tok.pair.table == k.table and p.tok.pair.part == k.part)

  /// Assumption (contract 6.2, conformance C1 and C3): the deduplication window outlasts one attempt. An entry
  /// stays in the log while a copy of its insert is in flight and while the shipper holds a probe-absent
  /// decision for its pair.
  def evictable(tok: Token): bool = and {
    expiresInFlight or not(requests.exists(m => m.tok == tok)),
    expiresBeforeResend or not(pst.keys().exists(k => k == tok.pair and pst.get(k) == Absent)),
  }

  def tableLog(t: int): Set[Token] = dedup.filter(tok => tok.pair.table == t)

  def landed(a: Row, t: int): bool = chParts.exists(p => p.tok.pair.table == t and p.rows.contains(a))
  def quarantined(a: Row, t: int): bool = quarantine.exists(k => and {
    k.table == t, k.part == partOf(a.span), k.first <= a.lsn, a.lsn <= k.last,
  })
  def inDurableWal(a: Row): bool = durable.contains(a.lsn) and wal.get(a.lsn) == Ingest(a.span)
  def inStore(a: Row): bool = TABLES.forall(t => landed(a, t) or quarantined(a, t))

  // ---------------------------------------------------------------------------------------------------------
  // Frame conditions
  // ---------------------------------------------------------------------------------------------------------

  action keepClient = all { sent' = sent, acked' = acked }
  action keepIntake = all { pending' = pending, waiters' = waiters }
  action keepBatches = all { open' = open, formedThrough' = formedThrough, ckptThrough' = ckptThrough }
  action keepPairs = all { pst' = pst, fails' = fails }
  action keepRecent = all { recent' = recent, recentApplied' = recentApplied, gapThrough' = gapThrough }
  action keepWal = all { wal' = wal, lastLsn' = lastLsn, durableLsn' = durableLsn, gcThrough' = gcThrough }
  action keepStore = all { chParts' = chParts, dedup' = dedup }
  action keepNet = all { requests' = requests, replies' = replies, nextMsg' = nextMsg }
  action keepMisc = all { up' = up, frames' = frames, faults' = faults, wire' = wire, quarantine' = quarantine }

  /// The committer appends one frame. Every frame of every type consumes one LSN.
  action appendFrame(f: Frame): bool = all {
    wal' = wal.put(lastLsn + 1, f),
    lastLsn' = lastLsn + 1,
    durableLsn' = durableLsn,
    gcThrough' = gcThrough,
  }

  // ---------------------------------------------------------------------------------------------------------
  // Intake: receive, append, sync, ack (section 4.5)
  // ---------------------------------------------------------------------------------------------------------

  /// A client sends a span for the first time.
  action Receive(s: int): bool = all {
    up, not(sent.contains(s)),
    sent' = sent.union(Set(s)), acked' = acked,
    pending' = pending.union(Set(s)), waiters' = waiters,
    keepBatches, keepPairs, keepRecent, keepWal, keepStore, keepNet, keepMisc,
    prevWatermark' = watermark, lastAction' = "Receive",
  }

  /// A client sends a span again: its ack never arrived (503, crash, or the response was lost).
  action ClientRetry(s: int): bool = all {
    up, sent.contains(s), not(pending.contains(s)), not(waiters.exists(w => w.span == s)),
    pending' = pending.union(Set(s)), waiters' = waiters,
    keepClient, keepBatches, keepPairs, keepRecent, keepWal, keepStore, keepNet, keepMisc,
    prevWatermark' = watermark, lastAction' = "ClientRetry",
  }

  /// The committer assigns the next LSN to a request and appends its frame.
  action Append(s: int): bool = all {
    up, pending.contains(s), frames < MAX_FRAMES,
    appendFrame(Ingest(s)),
    pending' = pending.exclude(Set(s)),
    waiters' = waiters.union(Set({ lsn: lastLsn + 1, span: s })),
    frames' = frames + 1,
    up' = up, faults' = faults, wire' = wire, quarantine' = quarantine,
    keepClient, keepBatches, keepPairs, keepRecent, keepStore, keepNet,
    prevWatermark' = watermark, lastAction' = "Append",
  }

  /// `sync_data` returns `Ok` for the group: everything appended is durable.
  action Sync: bool = all {
    up, durableLsn < lastLsn,
    durableLsn' = lastLsn, wal' = wal, lastLsn' = lastLsn, gcThrough' = gcThrough,
    keepClient, keepIntake, keepBatches, keepPairs, keepRecent, keepStore, keepNet, keepMisc,
    prevWatermark' = watermark, lastAction' = "Sync",
  }

  /// Ack rule: a request is acknowledged only after the sync of the group that holds its frame.
  action Ack(w: Row): bool = all {
    up, waiters.contains(w), ackBeforeSync or w.lsn <= durableLsn,
    acked' = acked.union(Set(w)), sent' = sent,
    waiters' = waiters.exclude(Set(w)), pending' = pending,
    keepBatches, keepPairs, keepRecent, keepWal, keepStore, keepNet, keepMisc,
    prevWatermark' = watermark, lastAction' = "Ack",
  }

  // ---------------------------------------------------------------------------------------------------------
  // Recent-span index (section 8.6)
  // ---------------------------------------------------------------------------------------------------------

  /// Stage B applies the next durable frame. When the index is full the oldest entry is dropped and the
  /// dropped range is recorded as a gap.
  action IndexApply: bool = {
    val l = recentApplied + 1
    val added = if (lsns.contains(l) and isIngest(wal.get(l))) recent.union(Set(l)) else recent
    val oldest = added.fold(l, (a, x) => if (x < a) x else a)
    val over = added.size() > RECENT_CAP
    all {
      up, recentApplied < durableLsn,
      recentApplied' = l,
      recent' = if (over) added.exclude(Set(oldest)) else added,
      gapThrough' = if (over) max(gapThrough, oldest) else gapThrough,
      keepClient, keepIntake, keepBatches, keepPairs, keepWal, keepStore, keepNet, keepMisc,
      prevWatermark' = watermark, lastAction' = "IndexApply",
    }
  }

  /// Eviction: only entries at or below the checkpoint watermark (the visibility grace is abstracted).
  action EvictRecent(l: int): bool = all {
    up, recent.contains(l), l <= watermark,
    recent' = recent.exclude(Set(l)), recentApplied' = recentApplied, gapThrough' = gapThrough,
    keepClient, keepIntake, keepBatches, keepPairs, keepWal, keepStore, keepNet, keepMisc,
    prevWatermark' = watermark, lastAction' = "EvictRecent",
  }

  // ---------------------------------------------------------------------------------------------------------
  // Shipper: batches, inserts, probe, quarantine, checkpoint (section 5)
  // ---------------------------------------------------------------------------------------------------------

  /// The shipper picks the boundary of the next batch. Ranges are adjacent; a batch holds at least one
  /// `IngestRequest` frame. One batch at a time waits for its seal.
  action FormBatch(last: int): bool = {
    val first = formedThrough + 1
    val ps = pairsOf(first, last)
    all {
      up, first <= last, last <= lastLsn,
      ps != Set(),
      not(open.exists(b => b.sealLsn == 0)),
      open' = open.union(Set({ first: first, last: last, sealLsn: 0 })),
      formedThrough' = last, ckptThrough' = ckptThrough,
      pst' = ps.fold(pst, (m, k) => m.put(k, Fresh)),
      fails' = ps.fold(fails, (m, k) => m.put(k, 0)),
      keepClient, keepIntake, keepRecent, keepWal, keepStore, keepNet, keepMisc,
      prevWatermark' = watermark, lastAction' = "FormBatch",
    }
  }

  /// The `BatchSeal` frame is appended. It must be synced before any insert of the batch is sent.
  action PersistBatchBoundary(b: OpenBatch): bool = all {
    up, open.contains(b), b.sealLsn == 0,
    appendFrame(Seal({ first: b.first, last: b.last })),
    open' = open.exclude(Set(b)).union(Set({ ...b, sealLsn: lastLsn + 1 })),
    formedThrough' = formedThrough, ckptThrough' = ckptThrough,
    keepClient, keepIntake, keepPairs, keepRecent, keepStore, keepNet, keepMisc,
    prevWatermark' = watermark, lastAction' = "PersistBatchBoundary",
  }

  /// One INSERT with one block for one pair. Allowed for a new pair, after a known rejection, and after a
  /// probe that read "absent"; never while the outcome of an earlier attempt is unknown. An attempt that
  /// follows an unknown outcome is marked `Resent`: the earlier attempt may still land.
  action SendInsert(k: Pair): bool = {
    val st = pst.get(k)
    val tok = { pair: k, gen: if (dropToken) nextMsg else 0 }
    all {
      up, pst.keys().contains(k), sendable(k),
      st == Fresh or st == Absent or st == Retry or (noProbe and st == Unknown),
      requests' = requests.union(Set({ id: nextMsg, tok: tok, rows: rowsOf(k) })),
      replies' = replies, nextMsg' = nextMsg + 1,
      pst' = pst.set(k, if (st == Fresh or st == Retry) Sent(nextMsg) else Resent(nextMsg)), fails' = fails,
      wire' = wire.union(Set({ first: k.first, last: k.last })),
      up' = up, frames' = frames, faults' = faults, quarantine' = quarantine,
      keepClient, keepIntake, keepBatches, keepRecent, keepWal, keepStore,
      prevWatermark' = watermark, lastAction' = "SendInsert",
    }
  }

  /// The store processes an insert. A token found in the deduplication log is acknowledged without writing;
  /// otherwise the block lands whole and the token is logged.
  action ChApply(m: Insert): bool = {
    val known = dedup.contains(m.tok)
    all {
      requests.contains(m),
      known or tableLog(m.tok.pair.table).size() < WINDOW,
      requests' = requests.exclude(Set(m)),
      replies' = if (up) replies.union(Set(m.id)) else replies,
      nextMsg' = nextMsg,
      chParts' = if (known) chParts else chParts.union(Set(m)),
      dedup' = dedup.union(Set(m.tok)),
      keepClient, keepIntake, keepBatches, keepPairs, keepRecent, keepWal, keepMisc,
      prevWatermark' = watermark, lastAction' = if (known) "ChApply.deduplicated" else "ChApply.landed",
    }
  }

  /// The shipper receives the success response.
  action ChAckDeliver(k: Pair): bool = {
    val st = pst.get(k)
    all {
      up, pst.keys().contains(k),
      replies.exists(id => st == Sent(id) or st == Resent(id)),
      replies' = replies.filter(id => st != Sent(id) and st != Resent(id)),
      requests' = requests, nextMsg' = nextMsg,
      pst' = pst.set(k, Done), fails' = fails,
      keepClient, keepIntake, keepBatches, keepRecent, keepWal, keepStore, keepMisc,
      prevWatermark' = watermark, lastAction' = "ChAckDeliver",
    }
  }

  /// Timeout or reset: the shipper stops waiting. The insert may have landed, may still land, or may never
  /// land; its response, if any, is discarded.
  action ChAckLost(k: Pair): bool = {
    val st = pst.get(k)
    all {
      up, pst.keys().contains(k), faults < MAX_FAULTS,
      isSent(st),
      replies' = replies.filter(id => st != Sent(id) and st != Resent(id)),
      requests' = requests, nextMsg' = nextMsg,
      pst' = pst.set(k, Unknown), fails' = fails,
      faults' = faults + 1, up' = up, frames' = frames, wire' = wire, quarantine' = quarantine,
      keepClient, keepIntake, keepBatches, keepRecent, keepWal, keepStore,
      prevWatermark' = watermark, lastAction' = "ChAckLost",
    }
  }

  /// The store rejects an insert with an error: this attempt did not land. `poison` selects the data-error
  /// class, which counts towards quarantine; otherwise the error is the retry-with-backoff class. The shipper
  /// may resend at once only when no earlier attempt of the pair is in doubt; otherwise the pair's outcome is
  /// unknown again and the probe runs first.
  action ChReject(m: Insert, poison: bool): bool = {
    val k = m.tok.pair
    val clean = up and pst.keys().contains(k) and pst.get(k) == Sent(m.id)
    val doubted = up and pst.keys().contains(k) and pst.get(k) == Resent(m.id)
    val waiting = clean or doubted
    all {
      requests.contains(m), faults < MAX_FAULTS,
      requests' = requests.exclude(Set(m)), replies' = replies, nextMsg' = nextMsg,
      pst' = if (clean or (doubted and retryWithoutReprobe)) pst.set(k, Retry)
             else if (doubted) pst.set(k, Unknown)
             else pst,
      fails' = if (waiting and poison) fails.set(k, fails.get(k) + 1) else fails,
      faults' = faults + 1, up' = up, frames' = frames, wire' = wire, quarantine' = quarantine,
      keepClient, keepIntake, keepBatches, keepRecent, keepWal, keepStore,
      prevWatermark' = watermark, lastAction' = if (poison) "ChReject.poison" else "ChReject.retryable",
    }
  }

  /// Quarantine (section 5.6): the pair's frames are copied under the quarantine root and a `Quarantine`
  /// frame is appended. The pair counts as resolved once that frame is durable. A pair with an attempt in
  /// doubt is quarantined only after the probe read "absent".
  action QuarantinePair(k: Pair): bool = all {
    up, pst.keys().contains(k), isOpen(k),
    pst.get(k) == Retry or pst.get(k) == Absent, fails.get(k) >= POISON_LIMIT,
    appendFrame(Quar(k)),
    quarantine' = quarantine.union(Set(k)),
    pst' = pst.set(k, Parked), fails' = fails,
    up' = up, frames' = frames, faults' = faults, wire' = wire,
    keepClient, keepIntake, keepBatches, keepRecent, keepStore, keepNet,
    prevWatermark' = watermark,
    lastAction' = if (pst.get(k) == Absent) "QuarantinePair.afterProbe" else "QuarantinePair",
  }

  /// An entry leaves the deduplication log (by age or by count).
  action WindowExpire(tok: Token): bool = all {
    dedup.contains(tok), evictable(tok),
    dedup' = dedup.exclude(Set(tok)), chParts' = chParts,
    keepClient, keepIntake, keepBatches, keepPairs, keepRecent, keepWal, keepNet, keepMisc,
    prevWatermark' = watermark, lastAction' = "WindowExpire",
  }

  /// The existence probe resolves an unknown outcome.
  action ExistenceProbe(k: Pair): bool = all {
    up, pst.keys().contains(k), isOpen(k), pst.get(k) == Unknown,
    pst' = pst.set(k, if (probePresent(k)) Done else Absent), fails' = fails,
    keepClient, keepIntake, keepBatches, keepRecent, keepWal, keepStore, keepNet, keepMisc,
    prevWatermark' = watermark,
    lastAction' = if (probePresent(k)) "ExistenceProbe.present" else "ExistenceProbe.absent",
  }

  /// The `Checkpoint` frame is appended when the oldest open batch is sealed and every pair of it is
  /// resolved. Batches are checkpointed in order, so the frame is a low watermark.
  action AdvanceCheckpoint(b: OpenBatch): bool = {
    val ps = pairsOf(b.first, b.last)
    all {
      up, open.contains(b), b.first == ckptThrough + 1, sealDurable(b),
      ps.forall(k => pst.keys().contains(k) and resolved(k)),
      appendFrame(Ckpt(b.last)),
      open' = open.exclude(Set(b)), formedThrough' = formedThrough, ckptThrough' = b.last,
      pst' = pst.keys().exclude(ps).mapBy(k => pst.get(k)),
      fails' = fails.keys().exclude(ps).mapBy(k => fails.get(k)),
      keepClient, keepIntake, keepRecent, keepStore, keepNet, keepMisc,
      prevWatermark' = watermark, lastAction' = "AdvanceCheckpoint",
    }
  }

  /// Segment GC (section 4.7): the oldest segment is removed when it is full and durable (so it is not the
  /// current segment), lies wholly at or below the durable watermark, and no recent-index entry references
  /// it. Replay retention only delays this step.
  action TruncateWal: bool = {
    val hi = gcThrough + SEG
    all {
      up, hi <= lastLsn, hi <= durableLsn,
      truncateEarly or hi <= watermark,
      not(recent.exists(l => l <= hi)),
      wal' = lsns.filter(l => l > hi).mapBy(l => wal.get(l)),
      gcThrough' = hi, lastLsn' = lastLsn, durableLsn' = durableLsn,
      keepClient, keepIntake, keepBatches, keepPairs, keepRecent, keepStore, keepNet, keepMisc,
      prevWatermark' = watermark, lastAction' = "TruncateWal",
    }
  }

  // ---------------------------------------------------------------------------------------------------------
  // Crash and recovery (sections 4.6 and 5.9)
  // ---------------------------------------------------------------------------------------------------------

  /// The process dies. Every frame through `keep` survives: all synced frames, plus any prefix of the unsynced
  /// tail. Memory is lost. Inserts in flight stay in flight, so their outcome is unknown.
  action Crash(keep: int): bool = all {
    up, faults < MAX_FAULTS,
    durableLsn <= keep, keep <= lastLsn,
    wal' = lsns.filter(l => l <= keep).mapBy(l => wal.get(l)),
    lastLsn' = keep, durableLsn' = keep, gcThrough' = gcThrough,
    up' = false, faults' = faults + 1, frames' = frames, wire' = wire, quarantine' = quarantine,
    pending' = Set(), waiters' = Set(),
    open' = Set(), formedThrough' = 0, ckptThrough' = 0,
    pst' = Map(), fails' = Map(),
    recent' = Set(), recentApplied' = 0, gapThrough' = 0,
    requests' = requests, replies' = Set(), nextMsg' = nextMsg,
    keepClient, keepStore,
    prevWatermark' = watermark, lastAction' = "Crash",
  }

  /// Recovery (section 4.6), as one step: it reads only durable state, so a crash inside it changes nothing.
  /// Sealed batches above the watermark come back with their recorded boundaries and every pair in the
  /// unknown state (probe first), except pairs named by a durable `Quarantine` frame. The unsealed tail is
  /// left for `FormBatch` to seal with fresh boundaries. The recent index is rebuilt from the frames above the
  /// watermark.
  action Recover: bool = {
    val sealed = durableSeals.filter(b => b.last > watermark)
    val ps = sealed.map(b => pairsOf(b.first, b.last)).flatten()
    val above = rowsIn(watermark + 1, lastLsn).map(r => r.lsn)
    val kept = above.filter(l => above.filter(m => m > l).size() < RECENT_CAP)
    all {
      not(up),
      up' = true, frames' = frames, faults' = faults, wire' = wire, quarantine' = quarantine,
      open' = sealed,
      formedThrough' = durableSeals.fold(0, (acc, b) => max(acc, b.last)),
      ckptThrough' = watermark,
      pst' = ps.mapBy(k => if (quarDurable(k)) Parked else Unknown),
      fails' = ps.mapBy(k => 0),
      recent' = kept,
      recentApplied' = lastLsn,
      gapThrough' = above.exclude(kept).fold(0, (acc, l) => max(acc, l)),
      keepClient, keepIntake, keepWal, keepStore, keepNet,
      prevWatermark' = watermark, lastAction' = "Recover",
    }
  }

  // ---------------------------------------------------------------------------------------------------------
  // Initial state and next-state relation
  // ---------------------------------------------------------------------------------------------------------

  action init = all {
    sent' = Set(), acked' = Set(),
    wal' = Map(), lastLsn' = 0, durableLsn' = 0, gcThrough' = 0, quarantine' = Set(),
    up' = true, pending' = Set(), waiters' = Set(),
    open' = Set(), formedThrough' = 0, ckptThrough' = 0, pst' = Map(), fails' = Map(),
    recent' = Set(), recentApplied' = 0, gapThrough' = 0,
    requests' = Set(), replies' = Set(), nextMsg' = 1, chParts' = Set(), dedup' = Set(),
    frames' = 0, faults' = 0, wire' = Set(), prevWatermark' = 0, lastAction' = "Init",
  }

  // Parameter-free forms of the actions that take an argument; the fairness assumptions refer to them.
  action someReceive = { nondet s = oneOf(SPANS) Receive(s) }
  action someClientRetry = { nondet s = oneOf(SPANS) ClientRetry(s) }
  action someAppend = { nondet s = oneOf(pending) Append(s) }
  action someAck = { nondet w = oneOf(waiters) Ack(w) }
  action someEvictRecent = { nondet l = oneOf(recent) EvictRecent(l) }
  action someFormBatch = { nondet last = oneOf((formedThrough + 1).to(lastLsn)) FormBatch(last) }
  action somePersistBatchBoundary = { nondet b = oneOf(open) PersistBatchBoundary(b) }
  action someSendInsert = { nondet k = oneOf(pst.keys()) SendInsert(k) }
  action someChApply = { nondet m = oneOf(requests) ChApply(m) }
  action someChAckDeliver = { nondet k = oneOf(pst.keys()) ChAckDeliver(k) }
  action someChAckLost = { nondet k = oneOf(pst.keys()) ChAckLost(k) }
  action someChReject = {
    nondet m = oneOf(requests)
    nondet poison = oneOf(Set(true, false))
    ChReject(m, poison)
  }
  action someQuarantinePair = { nondet k = oneOf(pst.keys()) QuarantinePair(k) }
  action someWindowExpire = { nondet tok = oneOf(dedup) WindowExpire(tok) }
  action someExistenceProbe = { nondet k = oneOf(pst.keys()) ExistenceProbe(k) }
  action someAdvanceCheckpoint = { nondet b = oneOf(open) AdvanceCheckpoint(b) }
  action someCrash = { nondet keep = oneOf(durableLsn.to(lastLsn)) Crash(keep) }

  action step = any {
    someReceive, someClientRetry, someAppend, Sync, someAck,
    IndexApply, someEvictRecent,
    someFormBatch, somePersistBatchBoundary, someSendInsert,
    someChApply, someChAckDeliver, someChAckLost, someChReject, someQuarantinePair,
    someWindowExpire, someExistenceProbe, someAdvanceCheckpoint, TruncateWal,
    someCrash, Recover,
  }


  // ---------------------------------------------------------------------------------------------------------
  // Safety
  // ---------------------------------------------------------------------------------------------------------

  /// I1: an ack is issued only for a frame that a successful `sync_data` covers.
  val AckOnlyAfterSync = acked.forall(a => a.lsn <= durableLsn)

  /// I1: every acked span is in the durable WAL, or for every table in ClickHouse or in quarantine. Holding
  /// in every reachable state means it holds across crash and recovery.
  val NoAckedLoss = acked.forall(a => inDurableWal(a) or inStore(a))

  /// I2: a (batch, table, partition) lands at most once, and no row lands twice in a table.
  val NoDuplicateRows = chParts.forall(p => chParts.forall(q => or {
    p.id == q.id,
    p.tok.pair.table != q.tok.pair.table,
    and {
      not(p.tok.pair.first == q.tok.pair.first and p.tok.pair.part == q.tok.pair.part),
      p.rows.intersect(q.rows) == Set(),
    },
  }))

  /// I2: the checkpoint watermark never moves backwards.
  val CheckpointMonotonic = watermark >= prevWatermark

  /// I1 and I13: nothing above the checkpoint watermark is ever removed from the WAL.
  val TruncateOnlyBelowCheckpoint = gcThrough <= watermark

  /// I2: a batch's LSN range never changes. Every identity that was ever put on the wire and every durable
  /// seal are pairwise equal or disjoint, across crash and recovery.
  val BatchIdentityStable = {
    val ids = wire.union(durableSeals.map(b => { first: b.first, last: b.last }))
    ids.forall(a => ids.forall(b => a == b or a.last < b.first or b.last < a.first))
  }

  /// A read may answer `partial: true` for a span inside an open index gap (section 8.6).
  def inOpenGap(a: Row): bool = a.lsn <= gapThrough and a.lsn > watermark
  /// A span with a quarantined pair is not served by ClickHouse until an operator requeues it.
  def hasQuarantinedPair(a: Row): bool = TABLES.exists(t => quarantined(a, t))

  /// I3 as the contract states it: once stage B has applied through the ack's LSN (what the read barrier
  /// waits for), the span is in the recent index or in ClickHouse, unless it lies in an open gap.
  val ReadYourAckStrict = up implies acked.forall(a => recentApplied >= a.lsn implies or {
    recent.contains(a.lsn),
    TABLES.forall(t => landed(a, t)),
    inOpenGap(a),
  })

  /// I3 with the one exception the model found: a span with a quarantined pair.
  val ReadYourAck = up implies acked.forall(a => recentApplied >= a.lsn implies or {
    recent.contains(a.lsn),
    TABLES.forall(t => landed(a, t)),
    inOpenGap(a),
    hasQuarantinedPair(a),
  })

  val Safety = and {
    AckOnlyAfterSync, NoAckedLoss, NoDuplicateRows, CheckpointMonotonic,
    TruncateOnlyBelowCheckpoint, BatchIdentityStable, ReadYourAck,
  }

  // ---------------------------------------------------------------------------------------------------------
  // Liveness
  // ---------------------------------------------------------------------------------------------------------

  val vars = (
    sent, acked, wal, lastLsn, durableLsn, gcThrough, quarantine, up, pending, waiters, open, formedThrough,
    ckptThrough, pst, fails, recent, recentApplied, gapThrough, requests, replies, nextMsg, chParts, dedup,
    frames, faults, wire, prevWatermark, lastAction
  )

  /// Fairness assumptions. Faults are finite (`MAX_FAULTS`), and every step that makes progress is weakly
  /// fair: if it stays enabled it eventually happens. Client steps and faults carry no fairness.
  temporal Fairness = and {
    weakFair(Sync, vars), weakFair(IndexApply, vars), weakFair(someEvictRecent, vars),
    weakFair(someFormBatch, vars), weakFair(somePersistBatchBoundary, vars), weakFair(someSendInsert, vars),
    weakFair(someChApply, vars), weakFair(someChAckDeliver, vars), weakFair(someQuarantinePair, vars),
    weakFair(someExistenceProbe, vars), weakFair(someAdvanceCheckpoint, vars), weakFair(TruncateWal, vars),
    weakFair(Recover, vars), weakFair(someWindowExpire, vars),
  }

  def spanAcked(s: int): bool = acked.exists(a => a.span == s)
  def spanStored(s: int): bool = acked.forall(a => a.span == s implies inStore(a))

  /// I1 liveness: every acked span eventually lands in ClickHouse (or in quarantine).
  temporal AckedEventuallyLands =
    Fairness implies SPANS.forall(s => always(spanAcked(s) implies eventually(spanStored(s))))

  /// I13 liveness: a full segment that lies at or below the watermark is eventually removed.
  temporal CoveredEventuallyTruncated =
    Fairness implies always(eventually(gcThrough + SEG > watermark))

  /// Progress without temporal logic. While the node is up and work is outstanding, a step that makes
  /// progress is enabled; with the fairness assumptions above this rules out a stuck protocol.
  val workOutstanding = or {
    acked.exists(a => not(inStore(a))),
    gcThrough + SEG <= watermark,
  }
  val progressEnabled = or {
    durableLsn < lastLsn,
    recentApplied < durableLsn,
    recent.exists(l => l <= watermark),
    not(open.exists(b => b.sealLsn == 0)) and
      (formedThrough + 1).to(lastLsn).exists(last => pairsOf(formedThrough + 1, last) != Set()),
    open.exists(b => b.sealLsn == 0),
    pst.keys().exists(k => sendable(k) and (pst.get(k) == Fresh or pst.get(k) == Absent or pst.get(k) == Retry)),
    pst.keys().exists(k => isOpen(k) and pst.get(k) == Unknown),
    pst.keys().exists(k => replies.exists(id => pst.get(k) == Sent(id) or pst.get(k) == Resent(id))),
    requests.exists(m => dedup.contains(m.tok) or tableLog(m.tok.pair.table).size() < WINDOW),
    requests.exists(m => and {
      not(dedup.contains(m.tok)),
      tableLog(m.tok.pair.table).exists(tok => evictable(tok)),
    }),
    open.exists(b => and {
      b.first == ckptThrough + 1, sealDurable(b),
      pairsOf(b.first, b.last).forall(k => pst.keys().contains(k) and resolved(k)),
    }),
    gcThrough + SEG <= watermark and not(recent.exists(l => l <= gcThrough + SEG)),
  }
  val NoStuckState = (up and workOutstanding) implies progressEnabled

  // ---------------------------------------------------------------------------------------------------------
  // Witnesses: the simulation must reach these, or a clean run proves little
  // ---------------------------------------------------------------------------------------------------------

  val reachedCheckpoint = watermark > 0
  val reachedTruncate = gcThrough > 0
  val reachedQuarantine = quarantine != Set()
  val reachedQuarantineAfterProbe = lastAction == "QuarantinePair.afterProbe"
  val reachedProbePresent = lastAction == "ExistenceProbe.present"
  val reachedProbeAbsent = lastAction == "ExistenceProbe.absent"
  val reachedDeduplicated = lastAction == "ChApply.deduplicated"
  val reachedWindowExpire = lastAction == "WindowExpire"
  val reachedReship = lastAction == "Recover" and open != Set()
  val reachedGap = gapThrough > 0
}

// The contract, on the instance the simulator explores.
module ingest_ok {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "none"
  ).*
}

// A smaller instance for bounded model checking.
module ingest_small {
  import ingest(
    SPANS = Set(1, 2), TABLES = Set(1), WINDOW = 1, SEG = 2, RECENT_CAP = 2, POISON_LIMIT = 1,
    MAX_FRAMES = 2, MAX_FAULTS = 2, MUTANT = "none"
  ).*
}

// Mutants. Each breaks one rule of the contract and must violate the property named beside it.

// Violates AckOnlyAfterSync (and then NoAckedLoss after a crash).
module mutant_ack_before_sync {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "ack-before-sync"
  ).*
}

// Violates BatchIdentityStable (and then NoDuplicateRows).
module mutant_send_before_seal {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "send-before-seal"
  ).*
}

// Violates NoDuplicateRows.
module mutant_drop_token {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "drop-token"
  ).*
}

// Violates TruncateOnlyBelowCheckpoint (and then NoAckedLoss).
module mutant_truncate_early {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "truncate-early"
  ).*
}

// Violates NoDuplicateRows.
module mutant_no_probe {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "no-probe-after-window-expiry"
  ).*
}

// Not protocol mutants: one half of the deduplication-window assumption is lifted. Each violates
// NoDuplicateRows, which shows that the token does not cover an attempt that outlives the window.
module assumption_window_in_flight {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "window-expires-in-flight"
  ).*
}

module assumption_window_before_resend {
  import ingest(
    SPANS = Set(1, 2, 3), TABLES = Set(1, 2), WINDOW = 2, SEG = 2, RECENT_CAP = 3, POISON_LIMIT = 1,
    MAX_FRAMES = 4, MAX_FAULTS = 3, MUTANT = "window-expires-before-resend"
  ).*
}

// The smallest instance with every action reachable, for exhaustive checking and the temporal properties.
module ingest_tiny {
  import ingest(
    SPANS = Set(1), TABLES = Set(1), WINDOW = 1, SEG = 1, RECENT_CAP = 1, POISON_LIMIT = 1,
    MAX_FRAMES = 1, MAX_FAULTS = 3, MUTANT = "none"
  ).*
}

// Not a protocol mutant: the narrow reading of the probe rule, on the tiny instance. Violates NoDuplicateRows.
// The trace needs a lost ack, a rejection and a window expiry in one order, so it is checked exhaustively.
module reading_retry_without_reprobe {
  import ingest(
    SPANS = Set(1), TABLES = Set(1), WINDOW = 1, SEG = 1, RECENT_CAP = 1, POISON_LIMIT = 1,
    MAX_FRAMES = 1, MAX_FAULTS = 3, MUTANT = "retry-without-reprobe"
  ).*
}
