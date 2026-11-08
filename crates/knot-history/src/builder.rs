//! The background builder: claims dirty pages with a short committed lease,
//! loads each page's inputs in a short read transaction, replays them on an
//! isolated thread (`isolate::run_isolated`, no transaction open), writes
//! checkpoints and entries under the lease, counts sealed sessions' words,
//! and releases the page (Global Constraints, "Builder pass").
//!
//! A failure is recorded on the page (`record_failure`), never returned:
//! the claim's committed crash marker backs the page off, and a page that
//! keeps failing, times out twice with nothing to blame, crashes the
//! process three times or exceeds the replay cap is given up. A panic or a
//! timeout is blamed on the input that was running when it happened
//! (`Progress`), never on an innocent row.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use knot_storage::{UpdatesStore, UpdatesStoreError};
use sqlx::PgPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::diff::DiffError;
use crate::genesis::{GenesisError, GenesisInput, build_genesis};
use crate::isolate::{IsolateError, run_isolated};
use crate::pass::{
    CHECKPOINT_FLOOR_BYTES, CheckpointKind, PassInput, PassOutput, PassStats, Progress,
    ReplayHooks, Stage, WindowError, WindowInput, replay_window, run_pass,
};
use crate::replay::{ReplayError, ReplayMode};
use crate::store::{
    self, Claim, FailureAction, GenesisLoaded, PassLimits, PassLoad, PassLoaded, PassWrite,
    Recorded, WordsLoaded, Write,
};
use crate::words::{WordCounts, entry_word_counts};

/// The builder's version: a page built by an older one is rebuilt, a page
/// built by a newer one is left alone.
pub const BUILDER_VERSION: i32 = 1;
/// How often the builder looks for dirty pages.
pub const TICK: Duration = Duration::from_secs(2);
/// Passes a loop keeps queued at once (they wait for a worker permit).
pub const CLAIM_LIMIT: i64 = 20;
/// How often the sweep runs (idle sessions, older builds, late snapshots,
/// the gauges).
pub const SWEEP: Duration = Duration::from_secs(60);
/// A page written less than this ago waits (Rulings, debounce) ...
pub const DEBOUNCE: Duration = Duration::from_secs(10);
/// ... unless it is this many revs behind.
pub const DEBOUNCE_MAX_LAG_REVS: i64 = 200;
/// How long a claim holds a page.
pub const LEASE: Duration = Duration::from_secs(5 * 60);
/// Failed passes after which a page is given up.
pub const MAX_FAILURES: i32 = 10;
/// Replay timeouts with nothing to blame after which a page is given up.
pub const MAX_TIMEOUTS: i32 = 2;
/// Passes that died with the process after which a page is given up.
pub const MAX_CRASHES: i32 = 3;
/// Rebuilds asked for by a rejected genesis or gap starting checkpoint
/// after which a page is given up. Only a give-up and a newer builder
/// version reset the count (not a written genesis or pass, nor the claim's
/// lift of the rebuild request), so a rejection that recurs after every
/// rebuild cannot loop forever.
pub const MAX_REBUILDS: i32 = 3;
/// New rows one pass replays at most.
pub const PASS_MAX_ROWS: i64 = 5_000;
/// New row bytes one pass loads at most.
pub const PASS_MAX_ROW_BYTES: i64 = 16 * 1024 * 1024;
/// Snapshots (and kept gaps) one pass probes at most.
pub const PASS_MAX_SNAPSHOTS: i64 = 8;
/// Compute time for new rows, counted from the first new row's apply.
pub const PASS_ROWS_BUDGET: Duration = Duration::from_secs(10);
/// Wall time of the words phase.
pub const PASS_WORDS_BUDGET: Duration = Duration::from_secs(10);
/// One pass or genesis compute.
pub const REPLAY_TIMEOUT: Duration = Duration::from_secs(LEASE.as_secs() - 30);
/// One word count.
pub const WORDS_TIMEOUT: Duration = Duration::from_secs(60);

/// What one `build_page` call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassResult {
    /// A pass wrote its rows (and counts) and released the page.
    Built { built_rev: i64, rows: u64 },
    /// Genesis was written.
    Genesis { genesis_rev: i64 },
    /// The claim's clean check found nothing to do; no lease taken.
    Clean,
    /// Another builder holds the page, or a claim statement hit its lock
    /// timeout.
    Busy,
    /// The page is gone.
    Gone,
    /// The page backs off (`retry_after` in the future) or a newer builder
    /// owns it.
    Deferred,
    /// The pass failed (recorded on the page) or the page was given up.
    Failed,
}

/// Test hooks; production code never sets them.
#[doc(hidden)]
#[derive(Default, Clone)]
pub struct TestHooks {
    /// Sleeps inside every pass or genesis compute, at `Stage::Idle` before
    /// the replay starts (only for `hold_compute_doc` when set).
    pub hold_compute: Duration,
    pub hold_compute_doc: Option<Uuid>,
    /// Sleeps inside every word-count compute.
    pub hold_words: Duration,
    /// Sleeps on the builder task before a release in its own transaction.
    pub hold_before_release: Duration,
    pub replay: ReplayHooks,
    /// The word count returns `DiffError::Encode`.
    pub fail_words: bool,
    /// Receives the byte size of every replay input `build_page` loaded.
    pub input_log: Option<Arc<Mutex<Vec<i64>>>>,
}

/// What the builder may load and spend per page.
#[derive(Clone)]
pub struct BuildLimits {
    pub max_rows: i64,
    pub max_row_bytes: i64,
    /// The cap on a build's replay input (`history_replay_max_bytes`).
    pub max_input_bytes: i64,
    /// The cap on a word count's replay input.
    pub words_max_input_bytes: i64,
    pub rows_budget: Duration,
    pub words_budget: Duration,
    pub lease: Duration,
    pub replay_timeout: Duration,
    pub words_timeout: Duration,
    pub checkpoint_floor: u64,
    pub debounce: Duration,
    /// The share of one core each worker may use on average.
    pub cpu_share: f64,
    #[doc(hidden)]
    pub hooks: TestHooks,
}

impl BuildLimits {
    pub fn from_config(cfg: &knot_config::Config) -> BuildLimits {
        let cap = i64::try_from(cfg.history_replay_max_bytes).unwrap_or(i64::MAX);
        BuildLimits {
            max_rows: PASS_MAX_ROWS,
            max_row_bytes: PASS_MAX_ROW_BYTES,
            max_input_bytes: cap,
            words_max_input_bytes: cap,
            rows_budget: PASS_ROWS_BUDGET,
            words_budget: PASS_WORDS_BUDGET,
            lease: LEASE,
            replay_timeout: REPLAY_TIMEOUT,
            words_timeout: WORDS_TIMEOUT,
            checkpoint_floor: CHECKPOINT_FLOOR_BYTES,
            debounce: DEBOUNCE,
            cpu_share: cfg.history_cpu_share,
            hooks: TestHooks::default(),
        }
    }
}

/// `min(30 s × 2^min(failures, 7), 1 h)`: how long a page backs off after
/// its `failures + 1`-th claim.
pub fn failure_backoff(failures: i32) -> chrono::Duration {
    let exp = failures.clamp(0, 7) as u32;
    chrono::Duration::seconds((30_i64 << exp).min(3600))
}

/// How long a worker sleeps after `compute` of replay work so it uses at
/// most `cpu_share` of a core on average: `compute × (1 / share − 1)`.
pub fn pacing_delay(compute: Duration, cpu_share: f64) -> Duration {
    if cpu_share.is_nan() || cpu_share <= 0.0 || cpu_share >= 1.0 {
        return Duration::ZERO;
    }
    compute.mul_f64(1.0 / cpu_share - 1.0)
}

/// The worker permits: `workers` replays at once per replica (at least
/// one). `main.rs` creates them once; M1.4's views share them.
pub fn history_permits(workers: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(workers.max(1)))
}

/// A worker permit while it rides a replay thread, with the thread's state
/// for the stalled-thread gauge: 0 running, 1 timed out, 2 ended.
struct ComputeGuard {
    _permit: OwnedSemaphorePermit,
    state: Arc<AtomicU8>,
    stalled: Arc<AtomicI64>,
}

impl Drop for ComputeGuard {
    fn drop(&mut self) {
        if self.state.swap(2, Ordering::SeqCst) == 1 {
            self.stalled.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// A page's membership in this replica's in-flight set; leaving removes it,
/// also when the `build_page` future is dropped.
struct InFlight {
    set: Arc<Mutex<HashSet<Uuid>>>,
    id: Uuid,
}

impl InFlight {
    fn enter(set: &Arc<Mutex<HashSet<Uuid>>>, id: Uuid) -> Option<InFlight> {
        let inserted = set
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id);
        inserted.then(|| InFlight {
            set: set.clone(),
            id,
        })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
    }
}

/// The page history builder of one replica.
pub struct Builder {
    id: Uuid,
    pool: PgPool,
    updates: Arc<dyn UpdatesStore>,
    limits: BuildLimits,
    permits: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashSet<Uuid>>>,
    stalled: Arc<AtomicI64>,
}

impl Builder {
    /// A builder with a random lease owner id.
    ///
    /// # Panics
    ///
    /// When `replay_timeout` or `words_timeout` is not below `lease`: the
    /// lease must cover every compute.
    pub fn new(
        pool: PgPool,
        updates: Arc<dyn UpdatesStore>,
        limits: BuildLimits,
        permits: Arc<Semaphore>,
    ) -> Builder {
        assert!(
            limits.replay_timeout < limits.lease && limits.words_timeout < limits.lease,
            "the replay and word-count timeouts must be below the lease"
        );
        Builder {
            id: Uuid::new_v4(),
            pool,
            updates,
            limits,
            permits,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            stalled: Arc::new(AtomicI64::new(0)),
        }
    }

    /// The lease owner id this builder writes into `claimed_by`.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// One pass over `doc_id` (module docs). Cancellation-safe: dropping
    /// the future leaves only the committed claim, which the lease and the
    /// backoff cover.
    pub async fn build_page(&self, doc_id: Uuid) -> PassResult {
        let Some(_in_flight) = InFlight::enter(&self.in_flight, doc_id) else {
            return PassResult::Busy;
        };
        let Ok(permit) = self.permits.clone().acquire_owned().await else {
            return PassResult::Busy;
        };
        let mut run = Run {
            b: self,
            doc_id,
            guard: Some(ComputeGuard {
                _permit: permit,
                state: Arc::new(AtomicU8::new(0)),
                stalled: self.stalled.clone(),
            }),
            compute: Duration::ZERO,
            count_failure: true,
        };
        let result = run.go().await;
        if result == PassResult::Failed && run.count_failure {
            metrics::counter!("knot_history_build_failures_total").increment(1);
        }
        self.set_stalled_gauge();
        // Pacing (Rulings (CPU)): the permit is held through the sleep.
        if let Some(guard) = run.guard.take() {
            let delay = pacing_delay(run.compute, self.limits.cpu_share);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            drop(guard);
        }
        result
    }

    /// One round of dirty pages, each built by its own task; returns how
    /// many were `Built` or `Genesis`.
    pub async fn tick(self: &Arc<Self>) -> usize {
        let exclude = self.in_flight_ids();
        let ids = match store::dirty_candidates(
            &self.pool,
            &exclude,
            CLAIM_LIMIT,
            self.limits.debounce,
            DEBOUNCE_MAX_LAG_REVS,
        )
        .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "history builder: dirty candidates failed");
                Vec::new()
            }
        };
        self.build_all(ids).await
    }

    /// One sweep round (idle sessions, older builds, late snapshots), then
    /// the lag and given-up gauges.
    pub async fn sweep(self: &Arc<Self>) -> usize {
        let exclude = self.in_flight_ids();
        let ids = match store::sweep_candidates(&self.pool, &exclude, CLAIM_LIMIT).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(error = %e, "history builder: sweep candidates failed");
                Vec::new()
            }
        };
        let built = self.build_all(ids).await;
        self.sample_gauges().await;
        built
    }

    #[doc(hidden)]
    pub async fn dirty_candidates(&self) -> Vec<Uuid> {
        store::dirty_candidates(
            &self.pool,
            &[],
            CLAIM_LIMIT,
            self.limits.debounce,
            DEBOUNCE_MAX_LAG_REVS,
        )
        .await
        .unwrap_or_else(|e| panic!("dirty candidates: {e}"))
    }

    #[doc(hidden)]
    pub async fn sweep_candidates(&self) -> Vec<Uuid> {
        store::sweep_candidates(&self.pool, &[], CLAIM_LIMIT)
            .await
            .unwrap_or_else(|e| panic!("sweep candidates: {e}"))
    }

    /// The builder's background task: a dirty loop every `TICK` and a sweep
    /// loop every `SWEEP`, each refilling its own set of queued passes up to
    /// `CLAIM_LIMIT` (Rulings). On `shutdown` both stop claiming, abort
    /// their passes and release the pages that were in flight
    /// (`release_on_shutdown`).
    pub fn spawn(self: Arc<Self>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (dirty, swept) = tokio::join!(
                self.clone().run_loop(false, shutdown.clone()),
                self.clone().run_loop(true, shutdown.clone()),
            );
            let mut ids = dirty;
            ids.extend(swept);
            ids.sort_unstable();
            ids.dedup();
            self.release_on_shutdown(&ids).await;
        })
    }

    /// Releases the leases this builder holds on `ids` after a graceful
    /// shutdown: `last_error = 'shutdown'`, the claim's failure taken back.
    #[doc(hidden)]
    pub async fn release_on_shutdown(&self, ids: &[Uuid]) {
        match store::release_on_shutdown(&self.pool, self.id, ids).await {
            Ok(n) => tracing::info!(released = n, "history builder: leases released on shutdown"),
            Err(e) => tracing::warn!(error = %e, "history builder: release on shutdown failed"),
        }
    }

    fn in_flight_ids(&self) -> Vec<Uuid> {
        self.in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect()
    }

    async fn build_all(self: &Arc<Self>, ids: Vec<Uuid>) -> usize {
        let mut set = JoinSet::new();
        for id in ids {
            let b = self.clone();
            set.spawn(async move { b.build_page(id).await });
        }
        let mut built = 0;
        while let Some(r) = set.join_next().await {
            if matches!(r, Ok(PassResult::Built { .. } | PassResult::Genesis { .. })) {
                built += 1;
            }
        }
        built
    }

    async fn sample_gauges(&self) {
        match store::max_lag(&self.pool).await {
            Ok(lag) => metrics::gauge!("knot_history_builder_lag_revs").set(lag as f64),
            Err(e) => tracing::warn!(error = %e, "history builder: lag gauge failed"),
        }
        match store::given_up_count(&self.pool).await {
            Ok(n) => metrics::gauge!("knot_history_given_up_pages").set(n as f64),
            Err(e) => tracing::warn!(error = %e, "history builder: given-up gauge failed"),
        }
        self.set_stalled_gauge();
    }

    fn set_stalled_gauge(&self) {
        metrics::gauge!("knot_history_replay_threads_stalled")
            .set(self.stalled.load(Ordering::SeqCst) as f64);
    }

    /// One refilling loop; returns the pages in flight when it stopped.
    async fn run_loop(self: Arc<Self>, sweep: bool, shutdown: CancellationToken) -> Vec<Uuid> {
        let mut set: JoinSet<PassResult> = JoinSet::new();
        let mut queued: HashMap<tokio::task::Id, Uuid> = HashMap::new();
        let mut every = tokio::time::interval(if sweep { SWEEP } else { TICK });
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = every.tick() => {}
            }
            while let Some(done) = set.try_join_next_with_id() {
                let id = match done {
                    Ok((id, _)) => id,
                    Err(e) => e.id(),
                };
                queued.remove(&id);
            }
            let free = CLAIM_LIMIT - set.len() as i64;
            if free > 0 {
                let mut exclude: Vec<Uuid> = queued.values().copied().collect();
                exclude.extend(self.in_flight_ids());
                let ids = if sweep {
                    store::sweep_candidates(&self.pool, &exclude, free).await
                } else {
                    store::dirty_candidates(
                        &self.pool,
                        &exclude,
                        free,
                        self.limits.debounce,
                        DEBOUNCE_MAX_LAG_REVS,
                    )
                    .await
                };
                match ids {
                    Ok(ids) => {
                        for doc_id in ids.into_iter().take(free as usize) {
                            let b = self.clone();
                            let handle = set.spawn(async move { b.build_page(doc_id).await });
                            queued.insert(handle.id(), doc_id);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, sweep, "history builder: candidates failed")
                    }
                }
            }
            if sweep {
                self.sample_gauges().await;
            }
        }
        let in_flight: Vec<Uuid> = queued.values().copied().collect();
        set.abort_all();
        while set.join_next().await.is_some() {}
        in_flight
    }
}

/// How a compute ended.
enum Computed<T> {
    Done(T),
    Panicked(String),
    TimedOut,
    /// The permit went with an earlier timed-out thread.
    NoPermit,
}

/// What a failed pass records.
struct Failure {
    message: String,
    action: FailureAction,
    timed_out: bool,
    /// `snapshots_unchecked_total`'s reason when the action leaves a
    /// snapshot unchecked or drops a kept gap.
    unchecked: Option<&'static str>,
}

impl Failure {
    fn plain(message: impl Into<String>) -> Failure {
        Failure {
            message: message.into(),
            action: FailureAction::None,
            timed_out: false,
            unchecked: None,
        }
    }
}

/// Where a failure points (genesis or a pass).
#[derive(Clone, Copy)]
enum Phase {
    /// Genesis from this baseline (if any).
    Genesis { baseline: Option<i64> },
    /// A pass from this starting checkpoint.
    Pass {
        cp_rev: i64,
        cp_kind: CheckpointKind,
    },
}

/// One `build_page` call.
struct Run<'a> {
    b: &'a Builder,
    doc_id: Uuid,
    /// The worker permit, while it is ours.
    guard: Option<ComputeGuard>,
    /// The wall time of the computes whose guard came back.
    compute: Duration,
    /// Whether a `Failed` counts in `knot_history_build_failures_total`.
    count_failure: bool,
}

impl Run<'_> {
    fn limits(&self) -> &BuildLimits {
        &self.b.limits
    }

    fn hooks(&self) -> &TestHooks {
        &self.b.limits.hooks
    }

    /// `hold_compute` for this page.
    fn hold(&self, d: Duration) -> Duration {
        match self.hooks().hold_compute_doc {
            Some(doc) if doc != self.doc_id => Duration::ZERO,
            _ => d,
        }
    }

    fn log_input(&self, bytes: i64) {
        if let Some(log) = &self.hooks().input_log {
            log.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(bytes);
        }
    }

    async fn go(&mut self) -> PassResult {
        let (pool, doc_id, me) = (&self.b.pool, self.doc_id, self.b.id);
        let claimed = match store::claim(pool, doc_id, me, self.limits().lease).await {
            Ok(Claim::Gone) => return PassResult::Gone,
            Ok(Claim::Busy) => return PassResult::Busy,
            Ok(Claim::Deferred) => return PassResult::Deferred,
            Ok(Claim::Clean) => return PassResult::Clean,
            Ok(Claim::GaveUp { reason, message }) => {
                self.gave_up(reason, &message);
                return PassResult::Failed;
            }
            Ok(Claim::Claimed(c)) => c,
            Err(e) if e.is_lock_timeout() => return PassResult::Busy,
            Err(e) => {
                tracing::warn!(%doc_id, error = %e, "history build failed: the claim");
                return PassResult::Failed;
            }
        };

        match self.b.updates.ensure_legacy_numbered(doc_id).await {
            Ok(_) => {}
            Err(UpdatesStoreError::DocGone) => return PassResult::Gone,
            Err(e) => {
                return self
                    .fail(Failure::plain(format!("legacy numbering: {e}")))
                    .await;
            }
        }

        let mut genesis = claimed.genesis_rev.is_none();
        if claimed.builder_version < BUILDER_VERSION && claimed.genesis_rev.is_some() {
            match store::reset_for_rebuild(pool, doc_id, me).await {
                Ok(Write::Done) => {
                    tracing::info!(%doc_id, "history page rebuilt for a newer builder");
                    genesis = true;
                }
                Ok(w) => return self.lost(w, "the rebuild"),
                Err(e) => return self.fail(Failure::plain(format!("rebuild: {e}"))).await,
            }
        }
        if genesis {
            self.genesis().await
        } else {
            self.pass().await
        }
    }

    /// A write that found the page gone or the lease not ours.
    fn lost(&self, w: Write, what: &str) -> PassResult {
        match w {
            Write::Gone => PassResult::Gone,
            _ => {
                tracing::warn!(doc_id = %self.doc_id, "history build failed: lease lost ({what})");
                PassResult::Failed
            }
        }
    }

    fn gave_up(&mut self, reason: &'static str, message: &str) {
        self.count_failure = false;
        tracing::error!(doc_id = %self.doc_id, reason, error = message, "history page given up");
        metrics::counter!("knot_history_pages_given_up_total", "reason" => reason).increment(1);
    }

    /// `give_up`'s after-claim form → `Failed`.
    async fn give_up(&mut self, reason: &'static str, message: String) -> PassResult {
        match store::give_up(&self.b.pool, self.doc_id, self.b.id, &message).await {
            Ok(true) => self.gave_up(reason, &message),
            Ok(false) => {
                tracing::warn!(doc_id = %self.doc_id, "history page not given up: lease lost");
            }
            Err(e) => {
                tracing::warn!(doc_id = %self.doc_id, error = %e, "history give-up failed");
            }
        }
        PassResult::Failed
    }

    /// Records a failed pass → `Failed`.
    async fn fail(&mut self, f: Failure) -> PassResult {
        let doc_id = self.doc_id;
        match store::record_failure(
            &self.b.pool,
            doc_id,
            self.b.id,
            &f.message,
            f.action,
            f.timed_out,
        )
        .await
        {
            Ok(Recorded::GaveUp) => {
                self.gave_up("timeout", &f.message);
                return PassResult::Failed;
            }
            Ok(Recorded::Done) => {
                if let Some(reason) = f.unchecked {
                    metrics::counter!("knot_history_snapshots_unchecked_total", "reason" => reason)
                        .increment(1);
                }
                if let FailureAction::DropKeptGap(rev)
                | FailureAction::Rebuild {
                    drop_gap: Some(rev),
                } = f.action
                {
                    tracing::error!(%doc_id, rev, "history gap checkpoint dropped: its content leaves history");
                }
            }
            Ok(Recorded::NoMatch) => {}
            Err(e) => {
                tracing::warn!(%doc_id, error = %e, "history failure not recorded");
            }
        }
        tracing::warn!(%doc_id, error = %f.message, "history build failed");
        PassResult::Failed
    }

    /// Renews the lease before a compute; `Err` ends the pass.
    async fn renew(&mut self) -> Result<(), PassResult> {
        match store::renew_lease(&self.b.pool, self.doc_id, self.b.id, self.limits().lease).await {
            Ok(Write::Done) => Ok(()),
            Ok(w) => Err(self.lost(w, "renewal")),
            Err(e) => Err(self
                .fail(Failure::plain(format!("lease renewal: {e}")))
                .await),
        }
    }

    /// Runs `f` on a replay thread with the worker permit as its guard.
    async fn compute<T: Send + 'static>(
        &mut self,
        timeout: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Computed<T> {
        let Some(guard) = self.guard.take() else {
            return Computed::NoPermit;
        };
        let state = guard.state.clone();
        let started = Instant::now();
        let (result, back) = run_isolated(guard, timeout, f).await;
        if let Some(guard) = back {
            self.compute += started.elapsed();
            self.guard = Some(guard);
        }
        match result {
            Ok(v) => Computed::Done(v),
            Err(IsolateError::Panicked(m)) => Computed::Panicked(m),
            Err(IsolateError::TimedOut) => {
                if state
                    .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    self.b.stalled.fetch_add(1, Ordering::SeqCst);
                }
                Computed::TimedOut
            }
        }
    }

    /// The failure of a panic or a timeout at `stage` (Task 7 step 7).
    fn blame(&self, phase: Phase, stage: Stage, why: Blame) -> Failure {
        let what = match why {
            Blame::Panic => "panicked",
            Blame::Timeout => "timed out",
        };
        let unchecked = match why {
            Blame::Panic => "panic",
            Blame::Timeout => "timeout",
        };
        let timed_out = why == Blame::Timeout;
        let f = |message: String, action| Failure {
            message,
            action,
            timed_out,
            unchecked: None,
        };
        match (stage, phase) {
            (Stage::Row(rev), _) => f(format!("row {rev} {what}"), FailureAction::SkipRev(rev)),
            (Stage::Probe(rev), _) => Failure {
                unchecked: Some(unchecked),
                ..f(
                    format!("the probe of the snapshot at rev {rev} {what}"),
                    FailureAction::SkipSnapshot(rev),
                )
            },
            (Stage::KeptGap(rev), _) => Failure {
                unchecked: Some("kept_gap"),
                ..f(
                    format!("the merge of the kept gap at rev {rev} {what}"),
                    FailureAction::DropKeptGap(rev),
                )
            },
            (Stage::Checkpoint, Phase::Genesis { baseline: Some(id) }) => f(
                format!("the merge of baseline {id} {what}"),
                FailureAction::SkipBaseline(id),
            ),
            (Stage::Checkpoint | Stage::Prefix(_), Phase::Pass { cp_rev, cp_kind }) => {
                let at = match stage {
                    Stage::Prefix(rev) => format!("prefix row {rev}"),
                    _ => format!("the checkpoint at rev {cp_rev}"),
                };
                f(format!("{at} {what}"), checkpoint_action(cp_rev, cp_kind))
            }
            (Stage::Emit(rev), _) => f(
                format!("the checkpoint encode at rev {rev} {what}"),
                FailureAction::None,
            ),
            _ => f(format!("the replay {what} at idle"), FailureAction::None),
        }
    }

    /// A compute that panicked or timed out.
    async fn compute_failed(
        &mut self,
        phase: Phase,
        progress: &Progress,
        outcome: Interrupted,
        metric_phase: &'static str,
    ) -> PassResult {
        match outcome {
            Interrupted::Panicked(m) => {
                let mut f = self.blame(phase, progress.get(), Blame::Panic);
                f.message = format!("{}: {m}", f.message);
                self.fail(f).await
            }
            Interrupted::TimedOut => {
                let (stage, held) = progress.cancel();
                metrics::counter!("knot_history_replay_timeouts_total", "phase" => metric_phase)
                    .increment(1);
                tracing::error!(doc_id = %self.doc_id, ?stage, "history replay timed out");
                let mut f = self.blame(phase, stage, Blame::Timeout);
                f.message = format!("replay timed out: {}", f.message);
                if let Stage::Emit(_) = stage
                    && held >= self.limits().replay_timeout / 2
                {
                    // Nothing to blame and each attempt strands a worker.
                    return self.give_up("timeout", f.message).await;
                }
                if held < self.limits().replay_timeout / 2 {
                    f.action = FailureAction::None;
                    f.unchecked = None;
                }
                self.fail(f).await
            }
        }
    }

    async fn genesis(&mut self) -> PassResult {
        let mut rejected: Vec<i64> = Vec::new();
        loop {
            let loaded = match store::load_genesis_inputs(
                &self.b.pool,
                self.doc_id,
                self.limits().max_input_bytes,
                &rejected,
            )
            .await
            {
                Ok(GenesisLoaded::Load(l)) => l,
                Ok(GenesisLoaded::Gone) => return PassResult::Gone,
                Ok(GenesisLoaded::TooLarge(bytes)) => {
                    let message = format!(
                        "too large: genesis reads {bytes} bytes, over the cap of {}",
                        self.limits().max_input_bytes
                    );
                    return self.give_up("too_large", message).await;
                }
                Err(e) => {
                    return self
                        .fail(Failure::plain(format!("genesis read: {e}")))
                        .await;
                }
            };
            self.log_input(loaded.bytes);
            if let Err(r) = self.renew().await {
                return r;
            }
            let progress = Progress::default();
            let input = GenesisInput {
                page_created_at: loaded.page_created_at,
                history_since: loaded.history_since,
                genesis_rev: loaded.genesis_rev,
                baselines: loaded.baselines,
                rows: loaded.rows,
                skip_revs: loaded.skip_revs,
                progress: progress.clone(),
                hooks: self.hooks().replay.clone(),
            };
            let hold = self.hold(self.hooks().hold_compute);
            let started = Instant::now();
            let computed = self
                .compute(self.limits().replay_timeout, move || {
                    if !hold.is_zero() {
                        std::thread::sleep(hold);
                    }
                    build_genesis(input)
                })
                .await;
            metrics::histogram!("knot_history_replay_seconds", "phase" => "genesis")
                .record(started.elapsed().as_secs_f64());
            let phase = Phase::Genesis {
                baseline: loaded.start,
            };
            let out = match computed {
                Computed::Done(Ok(out)) => out,
                Computed::Done(Err(GenesisError::BaselineRejected { id })) => {
                    tracing::warn!(doc_id = %self.doc_id, baseline = id, "history baseline does not apply");
                    rejected.push(id);
                    continue;
                }
                Computed::Done(Err(GenesisError::Corrupt { rev })) => {
                    return self
                        .fail(Failure {
                            action: FailureAction::SkipRev(rev),
                            ..Failure::plain(format!("row {rev} corrupted the genesis replay"))
                        })
                        .await;
                }
                Computed::Done(Err(e)) => return self.fail(Failure::plain(e.to_string())).await,
                Computed::Panicked(m) => {
                    return self
                        .compute_failed(phase, &progress, Interrupted::Panicked(m), "genesis")
                        .await;
                }
                Computed::TimedOut => {
                    return self
                        .compute_failed(phase, &progress, Interrupted::TimedOut, "genesis")
                        .await;
                }
                Computed::NoPermit => return PassResult::Failed,
            };
            return match store::write_genesis(&self.b.pool, self.doc_id, self.b.id, &out).await {
                Ok(Write::Done) => {
                    record_stats(&out.stats);
                    PassResult::Genesis {
                        genesis_rev: out.checkpoint.rev,
                    }
                }
                Ok(w) => match self.lost(w, "the genesis write") {
                    PassResult::Gone => PassResult::Gone,
                    _ => self.fail(Failure::plain("lease lost")).await,
                },
                Err(e) => {
                    self.fail(Failure::plain(format!("genesis write: {e}")))
                        .await
                }
            };
        }
    }

    async fn pass(&mut self) -> PassResult {
        let limits = PassLimits {
            cap: self.limits().max_input_bytes,
            max_rows: self.limits().max_rows,
            max_row_bytes: self.limits().max_row_bytes,
            max_snapshots: PASS_MAX_SNAPSHOTS,
        };
        let load = match store::load_pass_inputs(&self.b.pool, self.doc_id, &limits).await {
            Ok(PassLoaded::Load(l)) => l,
            Ok(PassLoaded::TooLarge(message)) => return self.give_up("too_large", message).await,
            Ok(PassLoaded::NoCheckpoint) => {
                return self
                    .fail(Failure {
                        action: FailureAction::Rebuild { drop_gap: None },
                        ..Failure::plain("no checkpoint at or below built_rev")
                    })
                    .await;
            }
            Err(e) => return self.fail(Failure::plain(format!("pass read: {e}"))).await,
        };
        let load = *load;
        self.log_input(load.bytes);
        if let Err(r) = self.renew().await {
            return r;
        }
        let PassLoad {
            built_rev,
            checkpoint_rev,
            checkpoint_kind,
            checkpoint_state,
            reapply,
            replay,
            genesis_rev,
            rows,
            rows_exhausted,
            open,
            claims,
            kept_gaps,
            snapshots,
            snapshot_checked_rev,
            skip_revs,
            now,
            bytes: _,
        } = load;
        // When each gap checkpoint's snapshot was written: probes run at
        // their own rev, late ones at `built_rev`.
        let mut gap_times: Vec<(i64, chrono::DateTime<chrono::Utc>)> = Vec::new();
        for s in snapshots.iter().filter(|s| s.state_bytes.is_some()) {
            let at = s.rev.max(built_rev);
            match gap_times.iter_mut().find(|(r, _)| *r == at) {
                Some((_, t)) => *t = (*t).max(s.created_at),
                None => gap_times.push((at, s.created_at)),
            }
        }
        let progress = Progress::default();
        let input = PassInput {
            checkpoint_rev,
            checkpoint_state,
            reapply,
            replay,
            genesis_rev,
            built_rev,
            rows,
            rows_exhausted,
            open,
            claims,
            kept_gaps,
            snapshots,
            snapshot_checked_rev,
            skip_revs,
            progress: progress.clone(),
            now,
            rows_budget: Some(self.limits().rows_budget),
            checkpoint_floor: self.limits().checkpoint_floor,
            hooks: self.hooks().replay.clone(),
        };
        let hold = self.hold(self.hooks().hold_compute);
        let started = Instant::now();
        let computed = self
            .compute(self.limits().replay_timeout, move || {
                if !hold.is_zero() {
                    std::thread::sleep(hold);
                }
                run_pass(input)
            })
            .await;
        metrics::histogram!("knot_history_replay_seconds", "phase" => "build")
            .record(started.elapsed().as_secs_f64());
        let phase = Phase::Pass {
            cp_rev: checkpoint_rev,
            cp_kind: checkpoint_kind,
        };
        let out = match computed {
            Computed::Done(Ok(out)) => out,
            Computed::Done(Err(e)) => {
                let f = replay_failure(e, checkpoint_rev, checkpoint_kind);
                return self.fail(f).await;
            }
            Computed::Panicked(m) => {
                return self
                    .compute_failed(phase, &progress, Interrupted::Panicked(m), "build")
                    .await;
            }
            Computed::TimedOut => {
                return self
                    .compute_failed(phase, &progress, Interrupted::TimedOut, "build")
                    .await;
            }
            Computed::NoPermit => return PassResult::Failed,
        };
        self.write_pass(built_rev, rows_exhausted, out, &gap_times)
            .await
    }

    async fn write_pass(
        &mut self,
        loaded_built_rev: i64,
        rows_exhausted: bool,
        out: PassOutput,
        gap_times: &[(i64, chrono::DateTime<chrono::Utc>)],
    ) -> PassResult {
        let written = store::write_pass(
            &self.b.pool,
            self.doc_id,
            self.b.id,
            loaded_built_rev,
            &out,
            gap_times,
        )
        .await;
        let built = PassResult::Built {
            built_rev: out.built_rev,
            rows: out.stats.rows,
        };
        match written {
            Ok(PassWrite::Released) => {
                record_stats(&out.stats);
                built
            }
            Ok(PassWrite::RevGap) => {
                record_stats(&out.stats);
                let message = format!("rev gap after {}", out.built_rev);
                tracing::error!(doc_id = %self.doc_id, built_rev = out.built_rev, "history rev gap");
                self.fail(Failure::plain(message)).await
            }
            Ok(PassWrite::WordsPending) => {
                record_stats(&out.stats);
                // A pass that reached the head counts at least one
                // (Rulings).
                let forced = rows_exhausted && !out.stats.stopped_early;
                if let Err(r) = self.words(forced).await {
                    return r;
                }
                self.release(built).await
            }
            Ok(PassWrite::LeaseLost) => self.fail(Failure::plain("lease lost")).await,
            Ok(PassWrite::Gone) => PassResult::Gone,
            Err(e) => self.fail(Failure::plain(format!("pass write: {e}"))).await,
        }
    }

    /// The release in its own transaction (step 10).
    async fn release(&mut self, built: PassResult) -> PassResult {
        let hold = self.hooks().hold_before_release;
        if !hold.is_zero() {
            tokio::time::sleep(hold).await;
        }
        match store::release(&self.b.pool, self.doc_id, self.b.id).await {
            Ok(Write::Done) => built,
            Ok(Write::Gone) => PassResult::Gone,
            Ok(Write::LeaseLost) => {
                // The lease is someone else's: nothing to record.
                self.count_failure = false;
                tracing::warn!(doc_id = %self.doc_id, "history release failed: lease lost");
                PassResult::Failed
            }
            Err(e) => self.fail(Failure::plain(format!("release: {e}"))).await,
        }
    }

    /// The words phase (step 9): sealed sessions' counts, oldest first,
    /// while the words budget lasts, and at least one when `forced`.
    async fn words(&mut self, forced: bool) -> Result<(), PassResult> {
        let (pool, doc_id, me) = (self.b.pool.clone(), self.doc_id, self.b.id);
        let started = Instant::now();
        let mut attempted = 0;
        loop {
            if started.elapsed() >= self.limits().words_budget && !(forced && attempted == 0) {
                return Ok(());
            }
            let entry = match store::next_words_entry(&pool, doc_id).await {
                Ok(Some(e)) => e,
                Ok(None) => return Ok(()),
                Err(e) => return Err(self.fail(Failure::plain(format!("words read: {e}"))).await),
            };
            match store::take_words_entry(&pool, doc_id, me, entry.id).await {
                Ok(Write::Done) => {}
                Ok(w) => return Err(self.lost(w, "the word count")),
                Err(e) => return Err(self.fail(Failure::plain(format!("words: {e}"))).await),
            }
            attempted += 1;
            let load = match store::load_words_inputs(
                &pool,
                doc_id,
                entry.start_rev,
                entry.end_rev,
                self.limits().words_max_input_bytes,
            )
            .await
            {
                Ok(WordsLoaded::Load(l)) => l,
                Ok(WordsLoaded::TooLarge) => {
                    words_skipped("too_large");
                    continue;
                }
                Err(e) => return Err(self.fail(Failure::plain(format!("words read: {e}"))).await),
            };
            self.log_input(load.bytes);
            self.renew().await?;
            let progress = Progress::default();
            let input = WindowInput {
                mode: ReplayMode::KeepDeleted,
                checkpoint_state: load.checkpoint_state,
                reapply: load.reapply,
                rows: load.rows,
                gap_states: load.gap_states,
                skip_revs: load.skip_revs,
                end_rev: entry.end_rev,
                merge_gap_at_end: entry.gap_credit_rev == Some(entry.end_rev),
                progress: progress.clone(),
                hooks: ReplayHooks::default(),
            };
            let hold = self.hold(self.hooks().hold_words);
            let fail_words = self.hooks().fail_words;
            let (inserted, deleted) = (entry.inserted, entry.deleted);
            let t = Instant::now();
            let computed = self
                .compute(self.limits().words_timeout, move || {
                    if !hold.is_zero() {
                        std::thread::sleep(hold);
                    }
                    let replayer = replay_window(input)?;
                    if fail_words {
                        return Ok(Err(DiffError::Encode("test hook".into())));
                    }
                    Ok::<_, WindowError>(entry_word_counts(replayer.doc(), &inserted, &deleted))
                })
                .await;
            metrics::histogram!("knot_history_replay_seconds", "phase" => "words")
                .record(t.elapsed().as_secs_f64());
            let counts: WordCounts = match computed {
                Computed::Done(Ok(Ok(c))) => c,
                Computed::Done(Ok(Err(_))) => {
                    words_skipped("diff_error");
                    continue;
                }
                Computed::Done(Err(WindowError::Corrupt { .. } | WindowError::State(_))) => {
                    words_skipped("corrupt");
                    continue;
                }
                Computed::Done(Err(WindowError::GapState { .. })) => {
                    words_skipped("gap_state");
                    continue;
                }
                Computed::Done(Err(WindowError::Cancelled)) | Computed::NoPermit => {
                    return Ok(());
                }
                Computed::Panicked(m) => {
                    tracing::warn!(%doc_id, entry = entry.id, error = m, "history word count panicked");
                    words_skipped("panic");
                    continue;
                }
                Computed::TimedOut => {
                    progress.cancel();
                    metrics::counter!("knot_history_replay_timeouts_total", "phase" => "words")
                        .increment(1);
                    tracing::error!(%doc_id, entry = entry.id, "history replay timed out");
                    words_skipped("timeout");
                    // The permit stayed with the thread.
                    return Ok(());
                }
            };
            match store::write_words(&pool, doc_id, me, entry.id, counts).await {
                Ok(Write::Done) => {}
                Ok(w) => return Err(self.lost(w, "the word count write")),
                Err(e) => return Err(self.fail(Failure::plain(format!("words write: {e}"))).await),
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Blame {
    Panic,
    Timeout,
}

/// A compute that did not come back with a result.
enum Interrupted {
    Panicked(String),
    TimedOut,
}

/// What a rejected starting checkpoint costs (Rulings): a periodic one is
/// dropped, a genesis or gap one rebuilt past (a gap one dropped alone).
fn checkpoint_action(cp_rev: i64, cp_kind: CheckpointKind) -> FailureAction {
    match cp_kind {
        CheckpointKind::Periodic => FailureAction::DropCheckpoint(cp_rev),
        CheckpointKind::Genesis => FailureAction::Rebuild { drop_gap: None },
        CheckpointKind::Gap => FailureAction::Rebuild {
            drop_gap: Some(cp_rev),
        },
    }
}

/// The failure of a pass that returned a `ReplayError`.
fn replay_failure(e: ReplayError, cp_rev: i64, cp_kind: CheckpointKind) -> Failure {
    let message = e.to_string();
    let (action, unchecked) = match e {
        ReplayError::Corrupt { rev } => (FailureAction::SkipRev(rev), None),
        ReplayError::CorruptSnapshot { snapshot_rev } => {
            (FailureAction::SkipSnapshot(snapshot_rev), Some("corrupt"))
        }
        ReplayError::KeptGap { rev } => (FailureAction::DropKeptGap(rev), Some("kept_gap")),
        ReplayError::State(_) | ReplayError::Prefix { .. } => {
            (checkpoint_action(cp_rev, cp_kind), None)
        }
        ReplayError::Cancelled => (FailureAction::None, None),
    };
    Failure {
        message,
        action,
        timed_out: false,
        unchecked,
    }
}

/// The metrics of a written pass or genesis.
fn record_stats(stats: &PassStats) {
    metrics::counter!("knot_history_rows_built_total").increment(stats.rows);
    for (reason, n) in &stats.unapplied {
        metrics::counter!("knot_history_rows_unapplied_total", "reason" => *reason).increment(*n);
    }
    metrics::counter!("knot_history_rows_parked_total").increment(stats.parked);
    metrics::counter!("knot_history_gap_total").increment(stats.gaps);
    for (reason, n) in &stats.snapshots_unchecked {
        metrics::counter!("knot_history_snapshots_unchecked_total", "reason" => *reason)
            .increment(*n);
    }
}

fn words_skipped(reason: &'static str) {
    metrics::counter!("knot_history_words_skipped_total", "reason" => reason).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_delay_scales_with_the_share() {
        let s = Duration::from_secs(1);
        assert_eq!(pacing_delay(s, 0.5), Duration::from_secs(1));
        assert_eq!(pacing_delay(s, 0.25), Duration::from_secs(3));
        assert_eq!(pacing_delay(s, 1.0), Duration::ZERO);
        assert_eq!(pacing_delay(Duration::ZERO, 0.5), Duration::ZERO);
    }

    #[test]
    fn failure_backoff_is_capped() {
        assert_eq!(failure_backoff(0), chrono::Duration::seconds(30));
        assert_eq!(failure_backoff(1), chrono::Duration::seconds(60));
        assert_eq!(failure_backoff(7), chrono::Duration::hours(1));
        assert_eq!(failure_backoff(100), chrono::Duration::hours(1));
        assert_eq!(failure_backoff(i32::MAX), chrono::Duration::hours(1));
    }
}
