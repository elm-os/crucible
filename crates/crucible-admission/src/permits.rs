//! The CPU budget for blocking work: one configured total of permits, a share of it per stage, and a first-come
//! pool of permits per share (Kani obligations in this module).
//!
//! A blocking job takes a permit before it is spawned and gives it back when it ends, so the jobs of a stage that
//! run at once never exceed its share, and a job that finds no permit waits its turn on an async task instead of
//! holding a thread.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};

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

/// A first-come pool of permits. [`PermitPool::acquire`] waits for one; waiters are served in the order they
/// first asked. A permit that is given back goes straight to the waiter at the head of the queue, which is
/// woken, so a permit never sits free while someone waits.
#[derive(Debug)]
pub struct PermitPool {
    total: u32,
    queue: Mutex<Queue<Waker>>,
}

/// The pool's accounting, the kernel the proofs check. `W` wakes a waiter: a [`Waker`] in the pool, a plain value
/// in the proofs.
#[derive(Debug)]
struct Queue<W> {
    /// Permits that nobody holds and nobody was handed.
    free: u32,
    /// The waiters in arrival order: each one's ticket and what wakes it.
    waiting: VecDeque<(u64, W)>,
    /// The tickets of waiters that were handed a permit and have not taken it yet.
    handed: Vec<u64>,
    /// The next waiter's ticket.
    next: u64,
}

impl<W> Queue<W> {
    const fn new(total: u32) -> Self {
        Self {
            free: total,
            waiting: VecDeque::new(),
            handed: Vec::new(),
            next: 0,
        }
    }

    /// Takes a free permit. A free permit means that nobody waits: a permit given back goes to the first waiter.
    fn try_take(&mut self) -> bool {
        if self.free == 0 {
            return false;
        }
        self.free -= 1;
        true
    }

    /// Joins the end of the queue and returns the waiter's ticket.
    fn join(&mut self, wake: W) -> u64 {
        let ticket = self.next;
        self.next = ticket.wrapping_add(1);
        self.waiting.push_back((ticket, wake));
        ticket
    }

    /// Gives a permit back: to the waiter at the head of the queue, whose `W` is returned for waking once the lock
    /// is released, or else to the free permits.
    fn give_back(&mut self) -> Option<W> {
        match self.waiting.pop_front() {
            Some((ticket, wake)) => {
                self.handed.push(ticket);
                Some(wake)
            }
            None => {
                self.free += 1;
                None
            }
        }
    }

    /// Takes the permit handed to `ticket`, if there is one.
    fn take_handed(&mut self, ticket: u64) -> bool {
        match self.handed.iter().position(|t| *t == ticket) {
            Some(at) => {
                self.handed.swap_remove(at);
                true
            }
            None => false,
        }
    }

    /// Waiter `ticket` gives up: it leaves the queue, or takes the permit it was handed and gives it back, so that
    /// it goes to the next waiter (returned for waking) or to the free permits.
    fn leave(&mut self, ticket: u64) -> Option<W> {
        if self.take_handed(ticket) {
            return self.give_back();
        }
        self.dequeue(ticket);
        None
    }

    /// Takes waiter `ticket` out of the queue, if it is there.
    fn dequeue(&mut self, ticket: u64) {
        self.waiting.retain(|(t, _)| *t != ticket);
    }

    /// What wakes waiter `ticket` while it is in the queue.
    fn wake_of(&mut self, ticket: u64) -> Option<&mut W> {
        self.waiting
            .iter_mut()
            .find_map(|(t, wake)| (*t == ticket).then_some(wake))
    }
}

/// A permit of a [`PermitPool`]. Dropping it gives it back.
#[derive(Debug)]
pub struct Permit {
    pool: Arc<PermitPool>,
}

/// The future of [`PermitPool::acquire`]. Dropping it before it resolves leaves the queue, and gives back a
/// permit it was handed but had not taken.
#[derive(Debug)]
pub struct Acquire {
    pool: Arc<PermitPool>,
    /// The ticket while the future waits in the queue.
    ticket: Option<u64>,
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

    /// The permits that nobody holds or waits to take.
    pub fn available(&self) -> u32 {
        self.lock().free
    }

    /// A permit, if one is free.
    pub fn try_acquire(self: &Arc<Self>) -> Option<Permit> {
        self.lock().try_take().then(|| Permit {
            pool: Arc::clone(self),
        })
    }

    /// Waits for a permit. Waiters are served first come, first served.
    pub fn acquire(self: &Arc<Self>) -> Acquire {
        Acquire {
            pool: Arc::clone(self),
            ticket: None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue<Waker>> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Wakes the waiter a permit went to, once the pool's lock is released.
fn wake(woken: Option<Waker>) {
    if let Some(waker) = woken {
        waker.wake();
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let woken = self.pool.lock().give_back();
        wake(woken);
    }
}

impl Future for Acquire {
    type Output = Permit;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Permit> {
        let this = self.get_mut();
        let mut queue = this.pool.lock();
        match this.ticket {
            None if queue.try_take() => {}
            None => {
                this.ticket = Some(queue.join(cx.waker().clone()));
                return Poll::Pending;
            }
            Some(ticket) if queue.take_handed(ticket) => this.ticket = None,
            Some(ticket) => {
                if let Some(waker) = queue.wake_of(ticket) {
                    waker.clone_from(cx.waker());
                }
                return Poll::Pending;
            }
        }
        drop(queue);
        Poll::Ready(Permit {
            pool: Arc::clone(&this.pool),
        })
    }
}

impl Drop for Acquire {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket {
            let woken = self.pool.lock().leave(ticket);
            wake(woken);
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// The most waiters, and the most permits handed to waiters, in a proof's queue.
    const MOST: u64 = 3;

    /// A queue and the permits its holders hold.
    struct State {
        queue: Queue<u64>,
        total: u32,
        held: u32,
    }

    /// Any state with up to [`MOST`] permits handed and [`MOST`] waiters. Each satisfies [`holds`], which a proof
    /// below checks, so the others may assume it. Waiter `t` has ticket `t` and is woken by the value `t`. The
    /// queue's operations compare tickets only for equality and mint new ones from `next`, so these states stand
    /// for every state of their size whose tickets are distinct and below `next`. With the new pool's state, the
    /// proofs below are an induction: the invariant holds after any sequence of operations whose queues stay within
    /// these sizes.
    fn any_state() -> State {
        let handed: u64 = kani::any();
        let waiting: u64 = kani::any();
        kani::assume(handed <= MOST && waiting <= MOST);
        let mut queue = Queue::new(0);
        // Room for one more waiter than the state holds, so that no operation reallocates: the verifier then
        // reasons about buffers of one fixed size.
        queue.handed = Vec::with_capacity(MOST as usize + 1);
        queue.waiting = VecDeque::with_capacity(MOST as usize + 1);
        for ticket in 0..handed {
            queue.handed.push(ticket);
        }
        for ticket in handed..handed + waiting {
            queue.waiting.push_back((ticket, ticket));
        }
        queue.next = handed + waiting;
        if waiting == 0 {
            queue.free = kani::any();
        }
        let held: u32 = kani::any();
        let total = queue
            .free
            .checked_add(held)
            .and_then(|n| n.checked_add(handed as u32));
        kani::assume(total.is_some());
        let total = total.unwrap_or(0);
        State { queue, total, held }
    }

    /// The pool's invariant: the free permits, the permits held and the permits handed to a waiter add up to the
    /// total, so no permit is lost or made and the permits held never exceed the total; a free permit leaves
    /// nobody waiting; tickets are distinct and below `next`; each waiter keeps its own waker.
    fn holds(state: &State) -> bool {
        let q = &state.queue;
        let accounted = u64::from(q.free) + u64::from(state.held) + q.handed.len() as u64;
        // Tickets here stay below 64, so one bit per ticket shows whether they are distinct.
        let mut seen = 0u64;
        let mut tickets_ok = q.next < 64;
        let mut note = |ticket: u64| {
            let bit = 1u64 << (ticket % 64);
            tickets_ok &= ticket < q.next && seen & bit == 0;
            seen |= bit;
        };
        for ticket in &q.handed {
            note(*ticket);
        }
        let mut wakers_ok = true;
        for (ticket, wake) in &q.waiting {
            note(*ticket);
            wakers_ok &= ticket == wake;
        }
        accounted == u64::from(state.total)
            && state.held <= state.total
            && (q.free == 0 || q.waiting.is_empty())
            && tickets_ok
            && wakers_ok
    }

    /// The states the other proofs start from satisfy the invariant, so none of them starts from nothing.
    #[kani::proof]
    #[kani::unwind(5)]
    fn the_proofs_start_from_states_that_satisfy_the_invariant() {
        assert!(holds(&any_state()));
    }

    /// A new pool satisfies the invariant: every permit is free and nobody waits.
    #[kani::proof]
    fn a_new_pool_satisfies_the_invariant() {
        let total: u32 = kani::any();
        let state = State {
            queue: Queue::new(total),
            total,
            held: 0,
        };
        assert!(holds(&state));
    }

    /// An acquisition's first poll takes a free permit, which means nobody waits, so a newcomer never passes a
    /// waiter; or else it joins the end of the queue with a fresh ticket. The invariant is kept.
    #[kani::proof]
    #[kani::unwind(5)]
    fn an_acquisition_takes_a_free_permit_or_joins_the_end() {
        let mut state = any_state();
        kani::assume(holds(&state));
        let free = state.queue.free;
        let waiting = state.queue.waiting.len();
        if state.queue.try_take() {
            assert!(free > 0 && waiting == 0);
            state.held += 1;
        } else {
            assert!(free == 0);
            let next = state.queue.next;
            let ticket = state.queue.join(next);
            assert!(ticket == next && state.queue.next == next + 1);
            assert!(state.queue.waiting.len() == waiting + 1);
            assert!(state.queue.waiting.back() == Some(&(ticket, ticket)));
        }
        assert!(holds(&state));
    }

    /// A permit given back goes to the first waiter, which alone is woken, leaves the queue and takes the permit
    /// at its next poll; with nobody waiting the permit is free again. The invariant is kept at each step.
    #[kani::proof]
    #[kani::unwind(5)]
    fn a_permit_given_back_wakes_the_first_waiter() {
        let mut state = any_state();
        kani::assume(holds(&state));
        kani::assume(state.held > 0);
        state.held -= 1;
        let first = state.queue.waiting.front().map(|(ticket, _)| *ticket);
        let free = state.queue.free;
        let woken = state.queue.give_back();
        assert!(woken == first);
        assert!(holds(&state));
        match first {
            Some(ticket) => {
                assert!(state.queue.free == free);
                assert!(state.queue.wake_of(ticket).is_none());
                assert!(state.queue.take_handed(ticket));
                state.held += 1;
            }
            None => assert!(state.queue.free == free + 1),
        }
        assert!(holds(&state));
    }

    /// A waiting acquisition's poll takes a permit exactly when one was handed to it; until then it stays in the
    /// queue, and the waker its poll finds is its own. The invariant is kept.
    #[kani::proof]
    #[kani::unwind(5)]
    fn a_waiter_takes_a_permit_only_once_handed_one() {
        let mut state = any_state();
        kani::assume(holds(&state));
        let ticket: u64 = kani::any();
        kani::assume(ticket < state.queue.next);
        let handed = state.queue.handed.contains(&ticket);
        if state.queue.take_handed(ticket) {
            assert!(handed);
            state.held += 1;
        } else {
            // It is still queued, under its own waker, which a later poll may replace.
            assert!(!handed && state.queue.wake_of(ticket).map(|wake| *wake) == Some(ticket));
        }
        assert!(holds(&state));
    }

    /// A waiter that gives up while queued leaves the queue, and nothing else changes: no permit moves and nobody
    /// is woken. The invariant is kept. A waiter that gives up after it was handed a permit takes the permit and
    /// gives it back ([`Queue::leave`]), two steps that the proofs above cover.
    #[kani::proof]
    #[kani::unwind(5)]
    fn a_queued_waiter_that_gives_up_leaves_the_queue() {
        let mut state = any_state();
        kani::assume(holds(&state));
        // `any_state` queues the tickets from the number handed up to `next`.
        let ticket: u64 = kani::any();
        kani::assume(state.queue.handed.len() as u64 <= ticket && ticket < state.queue.next);
        let free = state.queue.free;
        let handed = state.queue.handed.len();
        let waiting = state.queue.waiting.len();
        state.queue.dequeue(ticket);
        assert!(state.queue.free == free && state.queue.handed.len() == handed);
        assert!(state.queue.waiting.len() + 1 == waiting);
        assert!(state.queue.wake_of(ticket).is_none());
        assert!(holds(&state));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::task::Wake;

    use proptest::prelude::*;

    use super::*;

    /// Counts the wakes of one waiter.
    #[derive(Default)]
    struct Wakes(AtomicU32);

    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Wakes {
        fn count(&self) -> u32 {
            self.0.load(Ordering::Relaxed)
        }
    }

    fn poll(acquire: &mut Acquire, wakes: &Arc<Wakes>) -> Option<Permit> {
        let waker = Waker::from(Arc::clone(wakes));
        match Pin::new(acquire).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(permit) => Some(permit),
            Poll::Pending => None,
        }
    }

    #[test]
    fn shares_cover_the_total_and_never_drop_to_zero() {
        assert_eq!(CpuShares::of(6).ingest_prepare, 6);
        assert_eq!(CpuShares::of(1).ingest_prepare, 1);
        assert_eq!(CpuShares::of(0).ingest_prepare, 1);
    }

    #[test]
    fn waiters_are_served_first_come_and_each_release_wakes_one() {
        let pool = Arc::new(PermitPool::new(1));
        assert_eq!(pool.total(), 1);
        let first = pool.try_acquire().unwrap();
        assert!(pool.try_acquire().is_none());
        let wakes: Vec<Arc<Wakes>> = (0..3).map(|_| Arc::default()).collect();
        let mut waiting: Vec<Acquire> = (0..3).map(|_| pool.acquire()).collect();
        for (acquire, wakes) in waiting.iter_mut().zip(&wakes) {
            assert!(poll(acquire, wakes).is_none());
        }
        // A newcomer cannot take the permit that is handed on.
        let mut late = pool.acquire();
        let late_wakes = Arc::default();
        drop(first);
        assert_eq!(
            wakes.iter().map(|w| w.count()).collect::<Vec<_>>(),
            [1, 0, 0]
        );
        assert!(pool.try_acquire().is_none());
        assert!(poll(&mut late, &late_wakes).is_none());
        assert!(poll(&mut waiting[2], &wakes[2]).is_none());
        let second = poll(&mut waiting[0], &wakes[0]).unwrap();
        drop(second);
        assert_eq!(wakes[1].count(), 1);
        let third = poll(&mut waiting[1], &wakes[1]).unwrap();
        drop(third);
        let fourth = poll(&mut waiting[2], &wakes[2]).unwrap();
        drop(fourth);
        assert_eq!(late_wakes.count(), 1);
        drop(poll(&mut late, &late_wakes).unwrap());
        assert_eq!(pool.available(), 1);
    }

    #[test]
    fn a_dropped_waiter_leaves_the_queue_and_passes_on_a_handed_permit() {
        let pool = Arc::new(PermitPool::new(1));
        let held = pool.try_acquire().unwrap();
        let wakes: Vec<Arc<Wakes>> = (0..3).map(|_| Arc::default()).collect();
        let mut waiting: Vec<Option<Acquire>> = (0..3).map(|_| Some(pool.acquire())).collect();
        for (acquire, wakes) in waiting.iter_mut().zip(&wakes) {
            assert!(poll(acquire.as_mut().unwrap(), wakes).is_none());
        }
        // The second waiter gives up while queued: it is skipped.
        waiting[1] = None;
        drop(held);
        assert_eq!(wakes[0].count(), 1);
        // The first gives up after it was handed the permit: the permit goes to the third.
        waiting[0] = None;
        assert_eq!(wakes[1].count(), 0);
        assert_eq!(wakes[2].count(), 1);
        let last = poll(waiting[2].as_mut().unwrap(), &wakes[2]).unwrap();
        assert_eq!(pool.available(), 0);
        drop(last);
        assert_eq!(pool.available(), 1);
        // Dropping a future that took its permit, or never waited, changes nothing.
        drop(waiting);
        let mut quick = pool.acquire();
        let permit = poll(&mut quick, &Arc::default()).unwrap();
        drop(quick);
        assert_eq!(pool.available(), 0);
        drop(permit);
        assert_eq!(pool.available(), 1);
    }

    #[derive(Debug, Clone)]
    enum Op {
        /// A task starts an acquisition, polls its waiting one, or gives its permit back.
        Step(usize),
        /// A task drops its waiting acquisition.
        Cancel(usize),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => (0..5usize).prop_map(Op::Step),
            1 => (0..5usize).prop_map(Op::Cancel),
        ]
    }

    /// The model's release: the permit goes to the longest waiting task, or else it is free.
    fn give_back(free: &mut u32, queue: &mut VecDeque<usize>, handed: &mut BTreeSet<usize>) {
        match queue.pop_front() {
            Some(next) => {
                handed.insert(next);
            }
            None => *free += 1,
        }
    }

    proptest! {
        /// Under random interleavings of five tasks, the pool matches a model of a first-come queue: a task gets a
        /// permit at once only when one is free, a release hands the permit to the longest waiting task and wakes
        /// it, a cancelled waiter is skipped or passes its permit on, and no permit is lost or made.
        #[test]
        fn the_pool_matches_a_first_come_model(
            total in 0..4u32,
            ops in proptest::collection::vec(op(), 0..80),
        ) {
            let pool = Arc::new(PermitPool::new(total));
            let wakes: Vec<Arc<Wakes>> = (0..5).map(|_| Arc::default()).collect();
            let mut waiting: Vec<Option<Acquire>> = (0..5).map(|_| None).collect();
            let mut holding: Vec<Option<Permit>> = (0..5).map(|_| None).collect();
            // The model: free permits, waiting tasks in order, and the tasks handed a permit.
            let mut free = total;
            let mut queue: VecDeque<usize> = VecDeque::new();
            let mut handed: BTreeSet<usize> = BTreeSet::new();
            for op in ops {
                let before: Vec<u32> = wakes.iter().map(|w| w.count()).collect();
                let mut woken = None;
                match op {
                    Op::Step(task) => {
                        if let Some(permit) = holding[task].take() {
                            woken = queue.front().copied();
                            drop(permit);
                            give_back(&mut free, &mut queue, &mut handed);
                        } else if let Some(mut acquire) = waiting[task].take() {
                            let expected = handed.remove(&task);
                            match poll(&mut acquire, &wakes[task]) {
                                Some(permit) => {
                                    prop_assert!(expected);
                                    holding[task] = Some(permit);
                                }
                                None => {
                                    prop_assert!(!expected);
                                    waiting[task] = Some(acquire);
                                }
                            }
                        } else {
                            let mut acquire = pool.acquire();
                            match poll(&mut acquire, &wakes[task]) {
                                Some(permit) => {
                                    prop_assert!(free > 0);
                                    free -= 1;
                                    holding[task] = Some(permit);
                                }
                                None => {
                                    prop_assert_eq!(free, 0);
                                    queue.push_back(task);
                                    waiting[task] = Some(acquire);
                                }
                            }
                        }
                    }
                    Op::Cancel(task) => {
                        if waiting[task].take().is_some() {
                            if handed.remove(&task) {
                                woken = queue.front().copied();
                                give_back(&mut free, &mut queue, &mut handed);
                            } else {
                                queue.retain(|t| *t != task);
                            }
                        }
                    }
                }
                for (task, wake) in wakes.iter().enumerate() {
                    prop_assert_eq!(wake.count() - before[task], u32::from(woken == Some(task)));
                }
                prop_assert_eq!(pool.available(), free);
                let held = holding.iter().flatten().count() as u32;
                prop_assert_eq!(free + held + handed.len() as u32, total);
            }
        }
    }

    /// Blocks the thread until `fut` resolves, parking between polls.
    fn block_on<F: Future + Unpin>(mut fut: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(value) = Pin::new(&mut fut).poll(&mut cx) {
                return value;
            }
            std::thread::park();
        }
    }

    /// Eight threads take and give back permits of a pool of three, 2,000 times each: no wake is lost (every
    /// thread finishes), at most three hold a permit at once, and every permit is free at the end.
    #[test]
    fn threads_contending_never_hold_more_than_the_total() {
        let pool = Arc::new(PermitPool::new(3));
        let holders = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..2_000 {
                        let permit = block_on(pool.acquire());
                        let now = holders.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        std::hint::spin_loop();
                        holders.fetch_sub(1, Ordering::SeqCst);
                        drop(permit);
                    }
                });
            }
        });
        assert!(most.load(Ordering::SeqCst) <= 3);
        assert_eq!(pool.available(), 3);
    }
}
