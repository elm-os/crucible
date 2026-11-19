//! The CPU budget for blocking work: one configured total of permits, a share of it per stage, and a first-come
//! pool of permits per share (Kani obligations in this module).
//!
//! A blocking job takes a permit before it is spawned. When the job ends, the permit passes straight to the job
//! that has waited longest, which runs next on the same thread, so a permit never idles while a job waits and a
//! waiting job holds no thread. With nobody waiting the permit is free again. The jobs of a stage that run at once
//! therefore never exceed its share.

use std::collections::VecDeque;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Each stage's share of the CPU budget's total (`cpu.permits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuShares {
    /// Ingest's inflate, transcode, scan and frame compression.
    pub ingest_prepare: u32,
}

impl CpuShares {
    /// The shares of `total` permits, each at least 1. Ingest's prepare stage is the only one that takes permits
    /// so far, so it has the whole total; the shipper's stages will take their shares from the same total.
    pub const fn of(total: u32) -> Self {
        Self {
            ingest_prepare: if total == 0 { 1 } else { total },
        }
    }
}

/// A blocking job of a [`PermitPool`].
pub type Job = Box<dyn FnOnce() + Send>;

/// A first-come pool of permits for blocking jobs. [`PermitPool::offer`] gives a job a free permit, or else queues
/// it; the queue is served first come, first served, by the permits that ending jobs pass on.
pub struct PermitPool {
    total: u32,
    queue: Mutex<Queue<Job>>,
}

/// The pool's accounting, the kernel the proofs check. `J` is a waiting job: a [`Job`] in the pool, a plain value
/// in the proofs.
#[derive(Debug)]
struct Queue<J> {
    /// Permits that no job holds.
    free: u32,
    /// The waiting jobs in arrival order, each with its ticket.
    waiting: VecDeque<(u64, J)>,
    /// The next waiting job's ticket.
    next: u64,
}

impl<J> Queue<J> {
    const fn new(total: u32) -> Self {
        Self {
            free: total,
            waiting: VecDeque::new(),
            next: 0,
        }
    }

    /// Gives `job` a free permit and returns it to run now, or else queues it last and returns its ticket. A free
    /// permit means that no job waits, so a newcomer never passes a waiting job.
    fn offer(&mut self, job: J) -> Result<J, u64> {
        if self.free > 0 {
            self.free -= 1;
            return Ok(job);
        }
        let ticket = self.next;
        self.next = ticket.wrapping_add(1);
        self.waiting.push_back((ticket, job));
        Err(ticket)
    }

    /// A job that held a permit ended: the permit passes to the job that has waited longest, returned to run
    /// next on the same thread, or else it is free again.
    fn pass_on(&mut self) -> Option<J> {
        match self.waiting.pop_front() {
            Some((_, job)) => Some(job),
            None => {
                self.free += 1;
                None
            }
        }
    }

    /// The job of `ticket` is no longer wanted: it leaves the queue if it still waits.
    fn withdraw(&mut self, ticket: u64) {
        self.waiting.retain(|(t, _)| *t != ticket);
    }
}

/// A permit of a [`PermitPool`], taken for a job. [`Permit::run`] runs the job, and then every job the permit
/// passes to.
pub struct Permit {
    pool: Arc<PermitPool>,
    /// The job to run next; `None` once the permit is given back.
    job: Option<Job>,
}

/// A job queued behind the pool's permits. Dropping it withdraws the job if it still waits.
#[derive(Debug)]
pub struct Waiting {
    pool: Arc<PermitPool>,
    ticket: u64,
}

/// What [`PermitPool::offer`] did with a job.
#[derive(Debug)]
pub enum Offer {
    /// The job has a permit: run it on a blocking thread with [`Permit::run`].
    Run(Permit),
    /// The job waits for a permit and will run on the thread of the job that passes it on.
    Queued(Waiting),
}

impl PermitPool {
    /// A pool of `total` permits, all free.
    pub const fn new(total: u32) -> Self {
        Self {
            total,
            queue: Mutex::new(Queue::new(total)),
        }
    }

    /// The permits of the pool.
    pub const fn total(&self) -> u32 {
        self.total
    }

    /// The permits that no job holds.
    pub fn available(&self) -> u32 {
        self.lock().free
    }

    /// Gives `job` a free permit, or else queues it behind the jobs that wait, first come, first served.
    pub fn offer(self: &Arc<Self>, job: Job) -> Offer {
        let offered = self.lock().offer(job);
        match offered {
            Ok(job) => Offer::Run(Permit {
                pool: Arc::clone(self),
                job: Some(job),
            }),
            Err(ticket) => Offer::Queued(Waiting {
                pool: Arc::clone(self),
                ticket,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue<Job>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for PermitPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let queue = self.lock();
        f.debug_struct("PermitPool")
            .field("total", &self.total)
            .field("free", &queue.free)
            .field("waiting", &queue.waiting.len())
            .finish()
    }
}

impl Permit {
    /// Runs the job, then the job that has waited longest whenever one waits, and gives the permit back when none
    /// does. Call it on a blocking thread. A job that panics loses its own work, but neither the permit nor the
    /// jobs behind it.
    pub fn run(mut self) {
        while let Some(job) = self.job.take() {
            let _panicked = std::panic::catch_unwind(AssertUnwindSafe(job));
            self.job = self.pool.lock().pass_on();
        }
    }
}

impl fmt::Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit")
            .field("has_job", &self.job.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for Permit {
    /// A permit dropped before its jobs ran, as when the runtime shuts down before the blocking thread starts, has
    /// no thread to run them on: its job and every job it passes to are dropped, and the permit is free again.
    fn drop(&mut self) {
        while let Some(job) = self.job.take() {
            drop(job);
            self.job = self.pool.lock().pass_on();
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        // The job, if it was still queued, is dropped inside the lock; dropping a job does not touch the pool.
        self.pool.lock().withdraw(self.ticket);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// The most jobs waiting in a proof's queue.
    const MOST: u64 = 3;

    /// A queue and the permits its running jobs hold.
    struct State {
        queue: Queue<u64>,
        total: u32,
        running: u32,
    }

    /// Any state with up to [`MOST`] waiting jobs. Each satisfies [`holds`], which a proof below checks, so the
    /// others may assume it. Job `t` has ticket `t`. The queue's operations compare tickets only for equality and
    /// mint new ones from `next`, so these states stand for every state of their size whose tickets are distinct
    /// and below `next`. With the new pool's state, the proofs below are an induction: the invariant holds after
    /// any sequence of operations whose queues stay within this size.
    fn any_state() -> State {
        let waiting: u64 = kani::any();
        kani::assume(waiting <= MOST);
        let mut queue = Queue::new(0);
        // Room for one more job than the state holds, so that no operation reallocates: the verifier then reasons
        // about a buffer of one fixed size.
        queue.waiting = VecDeque::with_capacity(MOST as usize + 1);
        for ticket in 0..waiting {
            queue.waiting.push_back((ticket, ticket));
        }
        queue.next = waiting;
        if waiting == 0 {
            queue.free = kani::any();
        }
        let running: u32 = kani::any();
        let total = queue.free.checked_add(running);
        kani::assume(total.is_some());
        let total = total.unwrap_or(0);
        State {
            queue,
            total,
            running,
        }
    }

    /// The pool's invariant: the free permits and the permits that running jobs hold add up to the total, so no
    /// permit is lost or made and the jobs that run at once never exceed the total; a free permit leaves no job
    /// waiting; tickets are distinct and below `next`; each waiting job is its own.
    fn holds(state: &State) -> bool {
        let q = &state.queue;
        // Tickets here stay below 64, so one bit per ticket shows whether they are distinct.
        let mut seen = 0u64;
        let mut tickets_ok = q.next < 64;
        for (ticket, job) in &q.waiting {
            let bit = 1u64 << (ticket % 64);
            tickets_ok &= *ticket < q.next && seen & bit == 0 && ticket == job;
            seen |= bit;
        }
        u64::from(q.free) + u64::from(state.running) == u64::from(state.total)
            && state.running <= state.total
            && (q.free == 0 || q.waiting.is_empty())
            && tickets_ok
    }

    /// The states the other proofs start from satisfy the invariant, so none of them starts from nothing.
    #[kani::proof]
    #[kani::unwind(5)]
    fn the_proofs_start_from_states_that_satisfy_the_invariant() {
        assert!(holds(&any_state()));
    }

    /// A new pool satisfies the invariant: every permit is free and no job waits.
    #[kani::proof]
    fn a_new_pool_satisfies_the_invariant() {
        let total: u32 = kani::any();
        let state = State {
            queue: Queue::new(total),
            total,
            running: 0,
        };
        assert!(holds(&state));
    }

    /// An offered job runs at once on a free permit, which means no job waits, so a newcomer never passes a
    /// waiting job; or else it waits last, with a fresh ticket. The invariant is kept.
    #[kani::proof]
    #[kani::unwind(5)]
    fn an_offered_job_runs_on_a_free_permit_or_waits_last() {
        let mut state = any_state();
        kani::assume(holds(&state));
        let free = state.queue.free;
        let waiting = state.queue.waiting.len();
        let next = state.queue.next;
        match state.queue.offer(next) {
            Ok(job) => {
                assert!(job == next && free > 0 && waiting == 0);
                state.running += 1;
            }
            Err(ticket) => {
                assert!(free == 0 && ticket == next && state.queue.next == next + 1);
                assert!(state.queue.waiting.len() == waiting + 1);
                assert!(state.queue.waiting.back() == Some(&(ticket, next)));
            }
        }
        assert!(holds(&state));
    }

    /// When a job ends, its permit passes to the job that has waited longest, which runs next and leaves the
    /// queue; with no job waiting the permit is free again. No permit is lost or given back twice. The invariant
    /// is kept.
    #[kani::proof]
    #[kani::unwind(5)]
    fn an_ending_job_passes_its_permit_to_the_job_that_waited_longest() {
        let mut state = any_state();
        kani::assume(holds(&state) && state.running > 0);
        let first = state.queue.waiting.front().map(|(_, job)| *job);
        let free = state.queue.free;
        let waiting = state.queue.waiting.len();
        let next = state.queue.pass_on();
        assert!(next == first);
        match next {
            Some(_) => {
                assert!(state.queue.free == free && state.queue.waiting.len() + 1 == waiting)
            }
            None => {
                assert!(state.queue.free == free + 1);
                state.running -= 1;
            }
        }
        assert!(holds(&state));
    }

    /// A withdrawn job leaves the queue if it waits, and nothing else changes: no permit moves and no other job
    /// leaves. The invariant is kept.
    #[kani::proof]
    #[kani::unwind(5)]
    fn a_withdrawn_job_leaves_the_queue_and_nothing_else_changes() {
        let mut state = any_state();
        kani::assume(holds(&state));
        let ticket: u64 = kani::any();
        kani::assume(ticket <= state.queue.next);
        let free = state.queue.free;
        let waiting = state.queue.waiting.len();
        let queued = ticket < state.queue.next;
        state.queue.withdraw(ticket);
        assert!(state.queue.free == free);
        assert!(state.queue.waiting.len() + usize::from(queued) == waiting);
        assert!(state.queue.waiting.iter().all(|(t, _)| *t != ticket));
        assert!(holds(&state));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use proptest::prelude::*;

    use super::*;

    /// A job that records its name in `log` when it runs.
    fn logs(log: &Arc<Mutex<Vec<u32>>>, name: u32) -> Job {
        let log = Arc::clone(log);
        Box::new(move || log.lock().unwrap().push(name))
    }

    fn run(offer: Offer) -> Option<Waiting> {
        match offer {
            Offer::Run(permit) => {
                permit.run();
                None
            }
            Offer::Queued(waiting) => Some(waiting),
        }
    }

    #[test]
    fn shares_cover_the_total_and_never_drop_to_zero() {
        assert_eq!(CpuShares::of(6).ingest_prepare, 6);
        assert_eq!(CpuShares::of(1).ingest_prepare, 1);
        assert_eq!(CpuShares::of(0).ingest_prepare, 1);
    }

    #[test]
    fn waiting_jobs_run_first_come_on_the_permit_passed_on() {
        let pool = Arc::new(PermitPool::new(1));
        assert_eq!(pool.total(), 1);
        let log = Arc::default();
        let Offer::Run(first) = pool.offer(logs(&log, 0)) else {
            panic!("a free permit")
        };
        assert_eq!(pool.available(), 0);
        let waiting: Vec<Waiting> = (1..5)
            .map(|name| match pool.offer(logs(&log, name)) {
                Offer::Queued(waiting) => waiting,
                Offer::Run(_) => panic!("no free permit"),
            })
            .collect();
        // Job 2 is withdrawn while it waits; the rest run in arrival order on the first job's thread.
        let mut waiting = waiting.into_iter();
        let kept: Vec<Waiting> = [waiting.next(), waiting.nth(1), waiting.next()]
            .into_iter()
            .flatten()
            .collect();
        first.run();
        assert_eq!(*log.lock().unwrap(), [0, 1, 3, 4]);
        assert_eq!(pool.available(), 1);
        // Dropping the handle of a job that ran changes nothing.
        drop(kept);
        assert_eq!(pool.available(), 1);
        assert!(run(pool.offer(logs(&log, 5))).is_none());
        assert_eq!(*log.lock().unwrap(), [0, 1, 3, 4, 5]);
    }

    #[test]
    fn a_job_that_panics_passes_its_permit_on() {
        let pool = Arc::new(PermitPool::new(1));
        let log = Arc::default();
        let Offer::Run(permit) = pool.offer(Box::new(|| panic!("a defect in a job"))) else {
            panic!("a free permit")
        };
        let waiting = pool.offer(logs(&log, 1));
        permit.run();
        assert_eq!(*log.lock().unwrap(), [1]);
        assert_eq!(pool.available(), 1);
        drop(waiting);
    }

    #[test]
    fn a_permit_dropped_before_it_runs_drops_its_jobs_and_is_free_again() {
        /// Counts the jobs dropped without running.
        struct Dropped(Arc<AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let pool = Arc::new(PermitPool::new(1));
        let dropped = Arc::new(AtomicUsize::new(0));
        let job = |dropped: &Arc<AtomicUsize>| -> Job {
            let guard = Dropped(Arc::clone(dropped));
            Box::new(move || {
                std::mem::forget(guard);
                panic!("must not run")
            })
        };
        let Offer::Run(permit) = pool.offer(job(&dropped)) else {
            panic!("a free permit")
        };
        let waiting = [pool.offer(job(&dropped)), pool.offer(job(&dropped))];
        drop(permit);
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        assert_eq!(pool.available(), 1);
        drop(waiting);
    }

    proptest! {
        /// Jobs offered in any order, some withdrawn while they wait, then the permits run one after another:
        /// every job that was not withdrawn runs exactly once, the waiting jobs run in the order they were
        /// offered, at most `total` jobs ever have a permit at once, and every permit is free at the end.
        #[test]
        fn every_job_kept_runs_once_in_offer_order(
            total in 1..4u32,
            withdrawn in proptest::collection::vec(any::<bool>(), 0..24),
        ) {
            let pool = Arc::new(PermitPool::new(total));
            let log = Arc::default();
            let mut permits = Vec::new();
            let mut kept = Vec::new();
            let mut queued = Vec::new();
            for (name, withdraw) in withdrawn.iter().enumerate() {
                let name = u32::try_from(name).unwrap();
                match pool.offer(logs(&log, name)) {
                    Offer::Run(permit) => permits.push((name, permit)),
                    Offer::Queued(waiting) if *withdraw => drop(waiting),
                    Offer::Queued(waiting) => {
                        queued.push(name);
                        kept.push(waiting);
                    }
                }
            }
            prop_assert!(permits.len() <= total as usize);
            prop_assert_eq!(pool.available() as usize, total as usize - permits.len());
            let ran_first: Vec<u32> = permits.iter().map(|(name, _)| *name).collect();
            for (_, permit) in permits {
                permit.run();
            }
            let log = log.lock().unwrap().clone();
            // The first permit runs its own job and then every waiting job; the others run only their own.
            let mut expected = ran_first.first().copied().into_iter().collect::<Vec<_>>();
            expected.extend(&queued);
            expected.extend(ran_first.iter().skip(1));
            prop_assert_eq!(log, expected);
            prop_assert_eq!(pool.available(), total);
            drop(kept);
        }
    }

    /// Eight threads offer 2,000 jobs each to a pool of three and run the permits they are given: every job runs,
    /// at most three run at once, and every permit is free at the end.
    #[test]
    fn threads_contending_never_run_more_jobs_at_once_than_the_total() {
        let pool = Arc::new(PermitPool::new(3));
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        // The handles of queued jobs, kept until every thread is done: a job queued last still runs, because a
        // job is queued only while every permit is held, and a holder passes its permit on before it returns.
        let handles = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..2_000 {
                        let (running, most, done) =
                            (Arc::clone(&running), Arc::clone(&most), Arc::clone(&done));
                        let job: Job = Box::new(move || {
                            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                            most.fetch_max(now, Ordering::SeqCst);
                            std::hint::spin_loop();
                            running.fetch_sub(1, Ordering::SeqCst);
                            done.fetch_add(1, Ordering::SeqCst);
                        });
                        if let Some(waiting) = run(pool.offer(job)) {
                            handles.lock().unwrap().push(waiting);
                        }
                    }
                });
            }
        });
        assert_eq!(done.load(Ordering::SeqCst), 16_000);
        assert!(most.load(Ordering::SeqCst) <= 3);
        assert_eq!(pool.available(), 3);
        drop(handles);
    }
}
