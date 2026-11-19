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

    /// Waiter `ticket` gives up: it leaves the queue, or gives back the permit it was handed, which goes to the
    /// next waiter (returned for waking) or to the free permits.
    fn leave(&mut self, ticket: u64) -> Option<W> {
        if self.take_handed(ticket) {
            return self.give_back();
        }
        if let Some(at) = self.waiting.iter().position(|(t, _)| *t == ticket) {
            self.waiting.remove(at);
        }
        None
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

    /// A task of the proof, which the queue knows by its index.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Task {
        Idle,
        Waiting(u64),
        Holding,
    }

    const TASKS: usize = 3;

    /// Three tasks share a pool of up to two permits. At each step one of them, in any order, starts an
    /// acquisition (a free permit, or the end of the queue), polls its waiting acquisition again, gives up waiting,
    /// or gives its permit back. After every step:
    ///
    /// - no permit is lost or double-released: the free permits, the permits held and the permits handed to a
    ///   waiter add up to the total, so the permits held never exceed it;
    /// - a free permit never leaves a waiter waiting;
    /// - a permit given back, by its holder or by a waiter that was handed it and gave up, goes to the first
    ///   waiter, and only that waiter is woken; with nobody waiting it is free again;
    /// - a waiting task's poll resolves exactly when it was handed a permit.
    #[kani::proof]
    #[kani::unwind(5)]
    fn permits_are_conserved_and_a_release_wakes_the_first_waiter() {
        let total: u32 = kani::any();
        kani::assume(total <= 2);
        let mut queue: Queue<usize> = Queue::new(total);
        let mut tasks = [Task::Idle; TASKS];
        for _ in 0..4 {
            let task: usize = kani::any();
            kani::assume(task < TASKS);
            let first = queue.waiting.front().map(|(_, waiter)| *waiter);
            let free = queue.free;
            match tasks[task] {
                Task::Idle => {
                    tasks[task] = if queue.try_take() {
                        assert!(free > 0);
                        Task::Holding
                    } else {
                        assert!(free == 0);
                        Task::Waiting(queue.join(task))
                    };
                }
                Task::Holding => {
                    tasks[task] = Task::Idle;
                    assert!(queue.give_back() == first);
                    assert!(queue.free == free + u32::from(first.is_none()));
                }
                Task::Waiting(ticket) => {
                    let handed = queue.handed.contains(&ticket);
                    if kani::any() {
                        assert!(queue.take_handed(ticket) == handed);
                        if handed {
                            tasks[task] = Task::Holding;
                        }
                    } else {
                        tasks[task] = Task::Idle;
                        let woken = queue.leave(ticket);
                        if handed {
                            assert!(woken == first);
                            assert!(queue.free == free + u32::from(first.is_none()));
                        } else {
                            assert!(woken.is_none() && queue.free == free);
                        }
                    }
                }
            }

            let held = tasks.iter().filter(|t| **t == Task::Holding).count() as u32;
            let waiting = tasks.iter().filter(|t| matches!(t, Task::Waiting(_))).count();
            assert!(queue.free <= total && held <= total);
            assert!(queue.free + held + queue.handed.len() as u32 == total);
            assert!(queue.free == 0 || queue.waiting.is_empty());
            assert!(queue.waiting.len() + queue.handed.len() == waiting);
        }
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
