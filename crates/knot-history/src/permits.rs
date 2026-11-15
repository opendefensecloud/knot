//! The process's replay permits (M1.4, Rulings): one `HistoryPermits` per
//! process, shared by the builder and the history views.
//!
//! It holds the one *compute* semaphore: every replay thread, the builder's
//! and a view's, holds one of its permits until the thread ends, also after
//! a timeout, so `workers` bounds live replay threads and their memory. The
//! second semaphore, the *builder slots*, gates no replay: a builder task
//! holds one for its whole `build_page` call (claim, loads, computes,
//! writes and the pacing sleep), so at most `workers` builder tasks wait
//! for or hold a compute permit, and a view queued for one (FIFO) waits for
//! at most the computes running or already queued when it arrived.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// A replay thread's state: running.
const RUNNING: u8 = 0;
/// Its caller gave up waiting; the thread runs on with the permit.
const TIMED_OUT: u8 = 1;
/// The thread ended and the permit went back.
const ENDED: u8 = 2;

/// The compute permits and builder slots of one process.
#[derive(Clone)]
pub struct HistoryPermits {
    compute: Arc<Semaphore>,
    builder: Arc<Semaphore>,
    /// Replay threads whose caller timed out while they still run.
    stalled: Arc<AtomicI64>,
    /// Callers inside `compute()` or `compute_within()` not granted yet.
    waiting: Arc<AtomicUsize>,
}

/// `workers` compute permits and builder slots, at least one of each (with
/// `history.workers: 0` the builder is off, and views still get one).
pub fn history_permits(workers: usize) -> HistoryPermits {
    let n = workers.max(1);
    HistoryPermits {
        compute: Arc::new(Semaphore::new(n)),
        builder: Arc::new(Semaphore::new(n)),
        stalled: Arc::new(AtomicI64::new(0)),
        waiting: Arc::new(AtomicUsize::new(0)),
    }
}

/// Counts one caller in `waiting` while it is alive: dropping the acquire
/// future (a timeout, a deadline, a dropped request) leaves the count.
struct Waiting(Arc<AtomicUsize>);

impl Waiting {
    fn enter(count: &Arc<AtomicUsize>) -> Waiting {
        count.fetch_add(1, Ordering::SeqCst);
        Waiting(count.clone())
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl HistoryPermits {
    /// A compute permit, in FIFO order with every other caller.
    pub async fn compute(&self) -> ComputeGuard {
        let _waiting = Waiting::enter(&self.waiting);
        let permit = self
            .compute
            .clone()
            .acquire_owned()
            .await
            .expect("the compute semaphore is never closed");
        ComputeGuard {
            permit: Some(permit),
            state: Arc::new(AtomicU8::new(RUNNING)),
            stalled: self.stalled.clone(),
            ended: Arc::new(Notify::new()),
        }
    }

    /// A compute permit, or `None` after `wait` (the caller leaves the
    /// queue).
    pub async fn compute_within(&self, wait: Duration) -> Option<ComputeGuard> {
        tokio::time::timeout(wait, self.compute()).await.ok()
    }

    /// A builder slot, held by one `build_page` call through its pacing.
    pub(crate) async fn builder_slot(&self) -> OwnedSemaphorePermit {
        self.builder
            .clone()
            .acquire_owned()
            .await
            .expect("the builder semaphore is never closed")
    }

    /// Compute permits nobody holds.
    pub fn available_compute(&self) -> usize {
        self.compute.available_permits()
    }

    /// Replay threads that timed out and still run (the
    /// `knot_history_replay_threads_stalled` gauge).
    pub fn stalled(&self) -> i64 {
        self.stalled.load(Ordering::SeqCst)
    }

    /// Callers waiting for a compute permit (tests order waiters by it).
    #[doc(hidden)]
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }
}

/// A compute permit while it rides a replay thread, with the thread's
/// state for the stalled gauge. Dropping it (when the thread ends) returns
/// the permit and ends every [`StallWatch`] of it.
pub struct ComputeGuard {
    /// `Some` until `drop`, which returns it before it ends the watches,
    /// so whoever wakes on [`StallWatch::ended`] sees the permit back.
    permit: Option<OwnedSemaphorePermit>,
    state: Arc<AtomicU8>,
    stalled: Arc<AtomicI64>,
    ended: Arc<Notify>,
}

impl ComputeGuard {
    /// A handle on this guard's thread state that outlives the guard.
    pub fn stall_watch(&self) -> StallWatch {
        StallWatch {
            state: self.state.clone(),
            stalled: self.stalled.clone(),
            ended: self.ended.clone(),
        }
    }
}

impl Drop for ComputeGuard {
    fn drop(&mut self) {
        // Fields drop after this body: return the permit first.
        drop(self.permit.take());
        if self.state.swap(ENDED, Ordering::SeqCst) == TIMED_OUT {
            self.stalled.fetch_sub(1, Ordering::SeqCst);
        }
        // No runtime needed: the replay thread drops the guard.
        self.ended.notify_waiters();
    }
}

/// Watches the thread a [`ComputeGuard`] rides on.
#[derive(Clone)]
pub struct StallWatch {
    state: Arc<AtomicU8>,
    stalled: Arc<AtomicI64>,
    ended: Arc<Notify>,
}

impl StallWatch {
    /// The caller stopped waiting while the thread runs: counted as
    /// stalled until the guard drops. Only the first call counts, and none
    /// after the guard dropped.
    pub fn timed_out(&self) {
        if self
            .state
            .compare_exchange(RUNNING, TIMED_OUT, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.stalled.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Whether the guard (and so its thread) is still alive.
    pub fn running(&self) -> bool {
        self.state.load(Ordering::SeqCst) != ENDED
    }

    /// Resolves once the guard dropped (at once when it already has).
    pub fn ended(&self) -> impl Future<Output = ()> + Send + 'static {
        let state = self.state.clone();
        let ended = self.ended.clone();
        async move {
            let notified = ended.notified();
            tokio::pin!(notified);
            // Registered before the state is read, so a drop in between is
            // not missed (`notify_waiters` wakes only registered waiters).
            notified.as_mut().enable();
            if state.load(Ordering::SeqCst) == ENDED {
                return;
            }
            notified.await;
        }
    }
}
