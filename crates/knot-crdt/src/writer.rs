//! Per-room writer task: appends what the room applied to the page's log,
//! in order, and tells the room where each row landed.
//!
//! Nothing the room applied is dropped. Jobs are buffered in arrival order
//! and flushed from the front in batches of at most `batch_cap` jobs
//! ([`BATCH_MAX`], halved after an error that may depend on the batch's size
//! and doubled back after each success), [`BATCH_INTERVAL`] after the oldest
//! arrived or as soon as a full batch waits. Batches are never reordered: a
//! server-side job's update can build on the WS jobs ahead of it. A failed
//! batch stays at the front and is retried, [`RETRY_MIN`] doubling to
//! [`RETRY_MAX`], with later jobs behind it. The writer keeps receiving jobs
//! while an attempt is in flight and while it waits to retry, so the room's
//! channel never fills behind a slow append.
//!
//! Each success sends one [`WriterEvent::Applied`] per job, in order, answers
//! the job's `persisted` reply, and publishes the highest rev on the bus
//! (best-effort: the room's catch-up tick covers a lost publish).
//!
//! The writer ends its room with a [`WriterEvent::Fatal`] in two ways:
//! - **Permanent**: the store refuses this page's writes for good (epoch
//!   changed, page gone, data it can never take). Every buffered job fails,
//!   nothing more is written, and the writer ends.
//! - **Evict**: the buffer grew past [`BUFFER_MAX_JOBS`] or
//!   [`BUFFER_MAX_BYTES`], or a server-side job (one with a `persisted`
//!   reply: a task toggle, import, accept, ...) missed its deadline
//!   ([`WriterLimits::server_job_deadline`], counted from the first attempt
//!   that failed after it arrived). The server-side job fails at once (its
//!   room is blocked awaiting the reply), the channel closes, and the jobs
//!   ahead of it are still written, with retries and no deadline, before the
//!   writer ends.
//!
//! The evicted room's clients reconnect (1012) and resend what they hold.
//! When the room ends on its own, the writer likewise flushes what it holds
//! before it ends. [`WriterDone`] resolves once the writer task has ended.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use uuid::Uuid;

use crate::bus::Bus;
use knot_storage::{NewUpdate, Persisted, UpdateSource, UpdatesStore};

/// The most jobs one append holds.
pub const BATCH_MAX: usize = 200;
/// How long the oldest buffered job waits for others to share its append.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(250);
/// The first wait before retrying a failed append; it doubles up to
/// [`RETRY_MAX`] and resets after a success.
pub const RETRY_MIN: Duration = Duration::from_millis(250);
pub const RETRY_MAX: Duration = Duration::from_secs(5);
/// More buffered jobs than this evicts the room.
pub const BUFFER_MAX_JOBS: usize = 20_000;
/// More buffered update bytes than this evicts the room.
pub const BUFFER_MAX_BYTES: usize = 32 * 1024 * 1024;
/// How long one bus publish may take before the writer moves on.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);

/// Input the writer receives from the room.
pub struct PersistJob {
    pub bytes: Vec<u8>,
    pub by_user_id: Option<Uuid>,
    pub source: UpdateSource,
    /// Ids, timestamps and page titles only; never page text or names.
    pub meta: Option<serde_json::Value>,
    /// Set for a server-side job, whose room awaits it before it replies to
    /// its caller (PATCH /tasks, an import, an accept): answered with the
    /// row's seq once the row is durably appended, or with `Err` when it is
    /// not persisted.
    pub persisted: Option<oneshot::Sender<Result<i64, String>>>,
}

/// Where one job's row landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub seq: i64,
    pub rev: i64,
}

/// What the writer tells its room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterEvent {
    /// A job's row was appended. Sent once per job, in job order.
    Applied(Applied),
    /// The room must evict itself (module doc). Sent at most once.
    Fatal(String),
}

/// How long the writer waits on a server-side job and on one append.
#[derive(Clone, Copy, Debug)]
pub struct WriterLimits {
    /// From the end of the first attempt that failed after a server-side job
    /// arrived, how long that job has to be persisted before the room is
    /// evicted. Checked only between attempts.
    pub server_job_deadline: Duration,
    /// How long one append may take before it is abandoned (transient).
    pub attempt_timeout: Duration,
}

impl Default for WriterLimits {
    fn default() -> Self {
        Self {
            server_job_deadline: Duration::from_secs(5),
            attempt_timeout: Duration::from_secs(10),
        }
    }
}

/// Resolves once a writer task has ended, also when it panicked.
#[derive(Clone)]
pub struct WriterDone(watch::Receiver<bool>);

impl WriterDone {
    pub fn is_done(&self) -> bool {
        *self.0.borrow() || self.0.has_changed().is_err()
    }

    pub async fn wait(&mut self) {
        // An error means the sender is gone: the task ended without saying
        // so (a panic).
        let _ = self.0.wait_for(|done| *done).await;
    }

    /// A `WriterDone` the test resolves by sending `true` or dropping the
    /// sender.
    #[cfg(test)]
    pub(crate) fn pair() -> (watch::Sender<bool>, WriterDone) {
        let (tx, rx) = watch::channel(false);
        (tx, WriterDone(rx))
    }
}

/// Spawns the writer of page `doc_id`, which appends at `content_epoch`.
pub fn spawn(
    doc_id: Uuid,
    content_epoch: i32,
    store: Arc<dyn UpdatesStore>,
    bus: Arc<dyn Bus>,
    rx: mpsc::Receiver<PersistJob>,
    events: mpsc::UnboundedSender<WriterEvent>,
    limits: WriterLimits,
) -> WriterDone {
    let (done_tx, done) = watch::channel(false);
    let writer = Writer {
        doc_id,
        content_epoch,
        store,
        bus,
        rx,
        events,
        limits,
        buf: VecDeque::new(),
        buf_bytes: 0,
        batch_cap: BATCH_MAX,
        backoff: RETRY_MIN,
        retry_at: None,
        earliest_deadline: None,
        draining: false,
        fatal_sent: false,
    };
    tokio::spawn(async move {
        writer.run().await;
        done_tx.send_replace(true);
    });
    WriterDone(done)
}

struct Buffered {
    job: PersistJob,
    arrived: Instant,
    /// A server-side job's deadline: `None` until an attempt fails after it
    /// arrived.
    deadline: Option<Instant>,
}

impl Buffered {
    fn is_server(&self) -> bool {
        self.job.persisted.is_some()
    }
}

struct Writer {
    doc_id: Uuid,
    content_epoch: i32,
    store: Arc<dyn UpdatesStore>,
    bus: Arc<dyn Bus>,
    rx: mpsc::Receiver<PersistJob>,
    events: mpsc::UnboundedSender<WriterEvent>,
    limits: WriterLimits,
    /// Jobs not yet appended, in arrival order.
    buf: VecDeque<Buffered>,
    buf_bytes: usize,
    batch_cap: usize,
    /// The wait before the next retry.
    backoff: Duration,
    /// When the next attempt may start after a failed one.
    retry_at: Option<Instant>,
    /// The earliest armed server-side deadline.
    earliest_deadline: Option<Instant>,
    /// `rx` is closed (the room ended, or an Evict): flush what is left with
    /// retries and no deadlines, then end.
    draining: bool,
    /// A Fatal went to the room (an Evict, or a Permanent).
    fatal_sent: bool,
}

impl Writer {
    async fn run(mut self) {
        loop {
            if !self.draining && self.earliest_deadline.is_some_and(|d| d <= Instant::now()) {
                let reason = format!(
                    "a server-side edit missed its {:?} deadline",
                    self.limits.server_job_deadline
                );
                self.evict(reason, 0);
                continue;
            }
            if self.buf.is_empty() {
                if self.draining {
                    return;
                }
                match self.rx.recv().await {
                    Some(job) => self.receive(job, 0),
                    None => self.draining = true,
                }
                continue;
            }
            let due = self.attempt_due();
            if due > Instant::now() {
                self.wait(due).await;
                continue;
            }
            if self.attempt().await.is_break() {
                return;
            }
        }
    }

    /// When the next attempt may start.
    fn attempt_due(&self) -> Instant {
        if let Some(at) = self.retry_at {
            return at;
        }
        if self.draining || self.buf.len() >= self.batch_cap {
            return Instant::now();
        }
        self.buf.front().expect("a buffered job").arrived + BATCH_INTERVAL
    }

    /// Waits until `due`, a job arrives, or the earliest server-side
    /// deadline passes (which the loop then acts on).
    async fn wait(&mut self, due: Instant) {
        let deadline = self.earliest_deadline.filter(|_| !self.draining);
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline.unwrap_or(due)), if deadline.is_some() => {}
            job = self.rx.recv(), if !self.draining => match job {
                Some(job) => self.receive(job, 0),
                None => self.draining = true,
            },
            _ = tokio::time::sleep_until(due) => {}
        }
    }

    /// One append of the front of the buffer. Breaks when the writer must
    /// end (a permanent error).
    async fn attempt(&mut self) -> ControlFlow<()> {
        let held = self.buf.len().min(self.batch_cap);
        let rows: Vec<NewUpdate> = self
            .buf
            .iter()
            .take(held)
            .map(|b| NewUpdate {
                by_user_id: b.job.by_user_id,
                bytes: b.job.bytes.clone(),
                source: b.job.source,
                meta: b.job.meta.clone(),
            })
            .collect();
        let store = self.store.clone();
        let started = Instant::now();
        let append = tokio::time::timeout(
            self.limits.attempt_timeout,
            store.append(self.doc_id, self.content_epoch, &rows),
        );
        tokio::pin!(append);
        // Only the attempt and new jobs: a deadline that passes meanwhile is
        // acted on once the attempt ends.
        let outcome = loop {
            tokio::select! {
                biased;
                outcome = &mut append => break outcome,
                job = self.rx.recv(), if !self.draining => match job {
                    Some(job) => self.receive(job, held),
                    None => self.draining = true,
                },
            }
        };
        metrics::histogram!("knot_writer_flush_seconds").record(started.elapsed().as_secs_f64());
        match outcome {
            Ok(Ok(persisted)) if persisted.len() == held => {
                self.persisted(persisted).await;
                ControlFlow::Continue(())
            }
            Ok(Ok(persisted)) => {
                self.fail_permanently(format!(
                    "the store returned {} rows for an append of {held}",
                    persisted.len()
                ));
                ControlFlow::Break(())
            }
            Ok(Err(e)) if e.is_permanent() => {
                self.fail_permanently(format!("permanent store error: {e}"));
                ControlFlow::Break(())
            }
            Ok(Err(e)) => {
                self.retry(held, &e.to_string(), e.shrinks_batch());
                ControlFlow::Continue(())
            }
            Err(_) => {
                let error = format!("no answer within {:?}", self.limits.attempt_timeout);
                self.retry(held, &error, false);
                ControlFlow::Continue(())
            }
        }
    }

    /// Buffers a job that arrived while `held` jobs at the front are in
    /// flight; past the buffer's limits that evicts the room.
    fn receive(&mut self, job: PersistJob, held: usize) {
        self.buf_bytes += job.bytes.len();
        self.buf.push_back(Buffered {
            job,
            arrived: Instant::now(),
            deadline: None,
        });
        if self.buf.len() > BUFFER_MAX_JOBS || self.buf_bytes > BUFFER_MAX_BYTES {
            let reason = format!(
                "the write buffer is over its limit ({} jobs, {} bytes; at most {BUFFER_MAX_JOBS} jobs, {BUFFER_MAX_BYTES} bytes)",
                self.buf.len(),
                self.buf_bytes
            );
            self.evict(reason, held);
        }
    }

    /// The front `persisted.len()` jobs are appended.
    async fn persisted(&mut self, persisted: Vec<Persisted>) {
        let mut highest_rev = 0;
        for p in persisted {
            let b = self.buf.pop_front().expect("an appended job");
            self.buf_bytes -= b.job.bytes.len();
            let _ = self.events.send(WriterEvent::Applied(Applied {
                seq: p.seq,
                rev: p.rev,
            }));
            if let Some(reply) = b.job.persisted {
                let _ = reply.send(Ok(p.seq));
            }
            highest_rev = highest_rev.max(p.rev);
        }
        self.batch_cap = (self.batch_cap * 2).min(BATCH_MAX);
        self.backoff = RETRY_MIN;
        self.retry_at = None;
        if self.earliest_deadline.is_some() {
            self.earliest_deadline = self.buf.iter().filter_map(|b| b.deadline).min();
        }
        match tokio::time::timeout(PUBLISH_TIMEOUT, self.bus.publish(self.doc_id, highest_rev))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(doc_id = %self.doc_id, error = ?e, "bus publish failed; relying on catch-up tick");
            }
            Err(_) => {
                tracing::debug!(doc_id = %self.doc_id, "bus publish timed out; relying on catch-up tick");
            }
        }
    }

    /// The attempt holding the front `held` jobs failed transiently: keep
    /// them, arm the deadlines, and retry after the backoff.
    fn retry(&mut self, held: usize, error: &str, shrink: bool) {
        metrics::counter!("knot_writer_flush_retries_total").increment(1);
        if shrink {
            self.batch_cap = (self.batch_cap / 2).max(1);
        }
        tracing::warn!(
            doc_id = %self.doc_id,
            rows = held,
            buffered = self.buf.len(),
            retry_in = ?self.backoff,
            error,
            "writer flush failed; retrying"
        );
        let now = Instant::now();
        self.retry_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(RETRY_MAX);
        if self.fatal_sent {
            // After an Evict a server-side job held by the failed attempt is
            // not retried.
            self.fail_server_jobs(0, "the room was evicted");
        } else if !self.draining {
            let deadline = now + self.limits.server_job_deadline;
            let mut armed = false;
            for b in self.buf.iter_mut() {
                if b.is_server() && b.deadline.is_none() {
                    b.deadline = Some(deadline);
                    armed = true;
                }
            }
            if armed {
                self.earliest_deadline =
                    Some(self.earliest_deadline.map_or(deadline, |d| d.min(deadline)));
            }
        }
    }

    /// Evict: tell the room, fail every server-side job the in-flight
    /// attempt (the front `held` jobs) does not hold, close the channel and
    /// take what is queued in it. What is left is written with retries and
    /// no deadline.
    fn evict(&mut self, reason: String, held: usize) {
        tracing::warn!(
            doc_id = %self.doc_id,
            buffered = self.buf.len(),
            "{reason}; evicting the room and writing the edits it holds"
        );
        let _ = self.events.send(WriterEvent::Fatal(reason.clone()));
        self.fatal_sent = true;
        self.draining = true;
        self.earliest_deadline = None;
        self.fail_server_jobs(held, &reason);
        self.rx.close();
        let (mut dropped, mut behind) = (0usize, 0usize);
        while let Ok(job) = self.rx.try_recv() {
            if let Some(reply) = job.persisted {
                let _ = reply.send(Err(format!("not persisted: {reason}")));
                dropped += 1;
            } else {
                if dropped > 0 {
                    behind += 1;
                }
                self.buf_bytes += job.bytes.len();
                self.buf.push_back(Buffered {
                    job,
                    arrived: Instant::now(),
                    deadline: None,
                });
            }
        }
        self.check_nothing_behind(dropped, behind);
    }

    /// Fails and removes every buffered server-side job past the front
    /// `held` ones.
    fn fail_server_jobs(&mut self, held: usize, why: &str) {
        let Some(first) = self
            .buf
            .iter()
            .skip(held)
            .position(Buffered::is_server)
            .map(|i| i + held)
        else {
            return;
        };
        let behind = self
            .buf
            .iter()
            .skip(first + 1)
            .filter(|b| !b.is_server())
            .count();
        let (mut index, mut dropped, mut bytes) = (0usize, 0usize, 0usize);
        self.buf.retain_mut(|b| {
            let keep = index < held || !b.is_server();
            index += 1;
            if !keep {
                if let Some(reply) = b.job.persisted.take() {
                    let _ = reply.send(Err(format!("not persisted: {why}")));
                }
                bytes += b.job.bytes.len();
                dropped += 1;
            }
            keep
        });
        self.buf_bytes -= bytes;
        self.check_nothing_behind(dropped, behind);
    }

    /// A room awaits a server-side job's reply before it sends another job,
    /// so a failed server-side job is always the last: removing it reorders
    /// nothing.
    fn check_nothing_behind(&self, dropped: usize, behind: usize) {
        debug_assert!(
            behind == 0,
            "{behind} jobs were behind {dropped} failed server-side jobs"
        );
        if behind > 0 {
            tracing::error!(
                doc_id = %self.doc_id,
                dropped,
                behind,
                "dropped server-side jobs that had jobs behind them"
            );
        }
    }

    /// Permanent: fail every buffered and queued job, and end.
    fn fail_permanently(&mut self, reason: String) {
        tracing::error!(
            doc_id = %self.doc_id,
            buffered = self.buf.len(),
            "{reason}; failing every buffered edit"
        );
        if !self.fatal_sent {
            let _ = self.events.send(WriterEvent::Fatal(reason.clone()));
            self.fatal_sent = true;
        }
        self.rx.close();
        let mut jobs: Vec<PersistJob> = self.buf.drain(..).map(|b| b.job).collect();
        while let Ok(job) = self.rx.try_recv() {
            jobs.push(job);
        }
        self.buf_bytes = 0;
        for job in jobs {
            if let Some(reply) = job.persisted {
                let _ = reply.send(Err(format!("not persisted: {reason}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{BusError, Subscription};
    use crate::test_metrics::CountingRecorder;
    use knot_storage::{
        DocUpdate, HistoryHead, NewUpdate, Persisted, RowsAfter, UpdateSource, UpdatesStoreError,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use tokio::sync::{Notify, oneshot, watch};
    use tokio::time::Instant;

    /// What one append call of [`ScriptedStore`] does.
    enum Step {
        Ok,
        Fail(Fail),
        /// Never returns.
        Hang,
        /// Waits this long (virtual time), then does the rest.
        After(Duration, Box<Step>),
        /// Waits until the test notifies, then does the rest.
        Wait(Arc<Notify>, Box<Step>),
    }

    #[derive(Clone, Copy)]
    enum Fail {
        /// Transient, and does not shrink the batch.
        Pool,
        /// 57014: transient, halves the batch.
        Timeout,
        /// Permanent: the page's epoch moved.
        Epoch,
        /// 22P02: permanent, deterministic.
        Data,
    }

    impl Fail {
        fn error(self) -> UpdatesStoreError {
            use knot_storage::testing::db_error;
            match self {
                Fail::Pool => UpdatesStoreError::Sqlx(sqlx::Error::PoolTimedOut),
                Fail::Timeout => UpdatesStoreError::Sqlx(db_error("57014")),
                Fail::Epoch => UpdatesStoreError::EpochChanged { room: 0, stored: 1 },
                Fail::Data => UpdatesStoreError::Sqlx(db_error("22P02")),
            }
        }
    }

    /// One append call: which jobs it held, when it started and ended, and
    /// whether it succeeded (`None` while in flight or when abandoned).
    #[derive(Clone, Debug)]
    struct Call {
        ids: Vec<u64>,
        started: Instant,
        ended: Option<Instant>,
        ok: Option<bool>,
    }

    type Script = Box<dyn Fn(usize, &[NewUpdate]) -> Step + Send + Sync>;

    /// An updates store whose `append` does what its script says for each
    /// call (by call number and rows), and records every call.
    struct ScriptedStore {
        script: Script,
        calls: Mutex<Vec<Call>>,
        /// Job ids of every row appended, in commit order.
        appended: Mutex<Vec<u64>>,
        next: AtomicI64,
        /// (calls started, calls finished or abandoned).
        progress: watch::Sender<(usize, usize)>,
    }

    impl ScriptedStore {
        fn new(script: impl Fn(usize, &[NewUpdate]) -> Step + Send + Sync + 'static) -> Self {
            Self {
                script: Box::new(script),
                calls: Mutex::new(Vec::new()),
                appended: Mutex::new(Vec::new()),
                next: AtomicI64::new(0),
                progress: watch::channel((0, 0)).0,
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn appended(&self) -> Vec<u64> {
            self.appended.lock().unwrap().clone()
        }

        /// The row counts of the calls that succeeded.
        fn successful_sizes(&self) -> Vec<usize> {
            self.calls()
                .iter()
                .filter(|c| c.ok == Some(true))
                .map(|c| c.ids.len())
                .collect()
        }

        /// Waits until `(started, finished)` satisfies `f`.
        async fn until(&self, f: impl FnMut(&(usize, usize)) -> bool) {
            let mut rx = self.progress.subscribe();
            within(rx.wait_for(f)).await.unwrap();
        }

        async fn run(step: Step) -> Result<(), UpdatesStoreError> {
            let mut step = step;
            loop {
                match step {
                    Step::Ok => return Ok(()),
                    Step::Fail(f) => return Err(f.error()),
                    Step::Hang => std::future::pending::<()>().await,
                    Step::After(d, then) => {
                        tokio::time::sleep(d).await;
                        step = *then;
                    }
                    Step::Wait(n, then) => {
                        n.notified().await;
                        step = *then;
                    }
                }
            }
        }
    }

    /// Counts a call as finished when it returns or is dropped.
    struct Finished<'a>(&'a watch::Sender<(usize, usize)>);
    impl Drop for Finished<'_> {
        fn drop(&mut self) {
            self.0.send_modify(|p| p.1 += 1);
        }
    }

    #[async_trait::async_trait]
    impl UpdatesStore for ScriptedStore {
        async fn append(
            &self,
            _: Uuid,
            _: i32,
            rows: &[NewUpdate],
        ) -> Result<Vec<Persisted>, UpdatesStoreError> {
            let ids: Vec<u64> = rows.iter().map(|r| id_of(&r.bytes)).collect();
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(Call {
                    ids: ids.clone(),
                    started: Instant::now(),
                    ended: None,
                    ok: None,
                });
                calls.len() - 1
            };
            self.progress.send_modify(|p| p.0 += 1);
            let _finished = Finished(&self.progress);
            let result = Self::run((self.script)(n, rows)).await;
            {
                let mut calls = self.calls.lock().unwrap();
                calls[n].ended = Some(Instant::now());
                calls[n].ok = Some(result.is_ok());
            }
            result?;
            self.appended.lock().unwrap().extend(ids);
            Ok(rows
                .iter()
                .map(|_| {
                    let seq = self.next.fetch_add(1, Ordering::SeqCst) + 1;
                    Persisted { seq, rev: seq }
                })
                .collect())
        }
        async fn rows_after(
            &self,
            _: Uuid,
            _: RowsAfter,
        ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
            Ok(vec![])
        }
        async fn head(&self, _: Uuid) -> Result<HistoryHead, UpdatesStoreError> {
            Ok(HistoryHead::default())
        }
        async fn ensure_legacy_numbered(&self, _: Uuid) -> Result<u64, UpdatesStoreError> {
            Ok(0)
        }
        async fn since(&self, _: Uuid, _: i64) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
            Ok(vec![])
        }
        async fn max_seq(&self, _: Uuid) -> Result<i64, UpdatesStoreError> {
            Ok(0)
        }
    }

    /// A bus that records every publish, and can hang in it.
    #[derive(Default)]
    struct RecordingBus {
        published: Mutex<Vec<i64>>,
        hang: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Bus for RecordingBus {
        async fn publish(&self, _: Uuid, rev: i64) -> Result<(), BusError> {
            self.published.lock().unwrap().push(rev);
            if self.hang.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn publish_presence(&self, _: Uuid, _: Vec<u8>) -> Result<(), BusError> {
            Ok(())
        }
        async fn subscribe(&self, _: Uuid) -> Result<Subscription, BusError> {
            Err(BusError::Io("not supported".into()))
        }
        async fn unsubscribe(&self, _: Uuid) -> Result<(), BusError> {
            Ok(())
        }
    }

    /// Awaits `f` for at most ten minutes of virtual time, so a writer that
    /// retries forever fails its test instead of hanging it.
    async fn within<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(600), f)
            .await
            .expect("still waiting after 600 s of virtual time")
    }

    /// A server-side job's answer.
    async fn answer(reply: Reply) -> Result<i64, String> {
        within(reply).await.expect("the writer dropped the reply")
    }

    /// A job's bytes: its id, padded to `len` bytes.
    fn bytes(id: u64, len: usize) -> Vec<u8> {
        let mut b = id.to_le_bytes().to_vec();
        b.resize(len.max(8), 0);
        b
    }

    fn id_of(bytes: &[u8]) -> u64 {
        u64::from_le_bytes(bytes[..8].try_into().unwrap())
    }

    /// A WS job (no reply).
    fn ws(id: u64) -> PersistJob {
        ws_sized(id, 8)
    }

    fn ws_sized(id: u64, len: usize) -> PersistJob {
        PersistJob {
            bytes: bytes(id, len),
            by_user_id: None,
            source: UpdateSource::Edit,
            meta: None,
            persisted: None,
        }
    }

    type Reply = oneshot::Receiver<Result<i64, String>>;

    /// A server-side job (one with a reply).
    fn server(id: u64) -> (PersistJob, Reply) {
        let (tx, rx) = oneshot::channel();
        let job = PersistJob {
            bytes: bytes(id, 8),
            by_user_id: None,
            source: UpdateSource::Task,
            meta: None,
            persisted: Some(tx),
        };
        (job, rx)
    }

    struct Harness {
        tx: mpsc::Sender<PersistJob>,
        events: mpsc::UnboundedReceiver<WriterEvent>,
        done: WriterDone,
        store: Arc<ScriptedStore>,
        bus: Arc<RecordingBus>,
    }

    impl Harness {
        fn start(store: ScriptedStore, capacity: usize, limits: WriterLimits) -> Self {
            let store = Arc::new(store);
            let bus = Arc::new(RecordingBus::default());
            let (tx, rx) = mpsc::channel(capacity);
            let (events_tx, events) = mpsc::unbounded_channel();
            let done = spawn(
                Uuid::new_v4(),
                0,
                store.clone(),
                bus.clone(),
                rx,
                events_tx,
                limits,
            );
            Self {
                tx,
                events,
                done,
                store,
                bus,
            }
        }

        async fn send(&self, job: PersistJob) {
            within(self.tx.send(job))
                .await
                .expect("the writer closed its channel");
        }

        /// Waits until the writer has taken every queued job off the channel.
        async fn until_received(&self) {
            while self.tx.capacity() < self.tx.max_capacity() {
                tokio::task::yield_now().await;
            }
        }

        /// The events so far: Applied (seq, rev) pairs and Fatal reasons.
        fn take_events(&mut self) -> (Vec<(i64, i64)>, Vec<String>) {
            let (mut applied, mut fatal) = (Vec::new(), Vec::new());
            while let Ok(ev) = self.events.try_recv() {
                match ev {
                    WriterEvent::Applied(Applied { seq, rev }) => applied.push((seq, rev)),
                    WriterEvent::Fatal(reason) => fatal.push(reason),
                }
            }
            (applied, fatal)
        }

        /// The next Fatal, skipping Applied events.
        async fn next_fatal(&mut self) -> String {
            loop {
                match within(self.events.recv())
                    .await
                    .expect("events closed before a Fatal")
                {
                    WriterEvent::Fatal(reason) => return reason,
                    WriterEvent::Applied(_) => {}
                }
            }
        }

        /// Waits for the next `n` Applied events, failing on a Fatal.
        async fn applied(&mut self, n: usize) -> Vec<(i64, i64)> {
            let mut applied = Vec::new();
            while applied.len() < n {
                match within(self.events.recv()).await.expect("events closed") {
                    WriterEvent::Applied(Applied { seq, rev }) => applied.push((seq, rev)),
                    WriterEvent::Fatal(reason) => panic!("unexpected Fatal: {reason}"),
                }
            }
            applied
        }

        /// Waits (virtual time) for the writer task to end.
        async fn ended(&mut self) {
            within(self.done.wait()).await;
        }
    }

    fn short_limits() -> WriterLimits {
        WriterLimits {
            server_job_deadline: Duration::from_secs(1),
            attempt_timeout: Duration::from_secs(10),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_batch_is_retried_in_order_with_later_jobs_behind_it() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let store = ScriptedStore::new(|n, _| {
            if n < 2 {
                Step::Fail(Fail::Pool)
            } else {
                Step::Ok
            }
        });
        let mut h = Harness::start(store, 16, WriterLimits::default());
        let mut replies = Vec::new();
        for id in 1..=3 {
            let (job, reply) = server(id);
            h.send(job).await;
            replies.push(reply);
        }
        tokio::time::sleep(BATCH_INTERVAL).await;
        for id in 4..=5 {
            let (job, reply) = server(id);
            h.send(job).await;
            replies.push(reply);
        }
        let mut seqs = Vec::new();
        for reply in replies {
            seqs.push(answer(reply).await.expect("persisted"));
        }
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        assert_eq!(h.store.appended(), vec![1, 2, 3, 4, 5]);
        let calls = h.store.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[2].ids, vec![1, 2, 3, 4, 5]);
        assert_eq!(
            calls[1].started - calls[0].started,
            Duration::from_millis(250)
        );
        assert_eq!(
            calls[2].started - calls[1].started,
            Duration::from_millis(500)
        );
        let (applied, fatal) = h.take_events();
        assert_eq!(applied, (1..=5).map(|r| (r, r)).collect::<Vec<_>>());
        assert!(fatal.is_empty(), "{fatal:?}");
        assert_eq!(rec.counter("knot_writer_flush_retries_total", &[]), 2);
        assert_eq!(rec.histogram_count("knot_writer_flush_seconds"), 3);
        assert_eq!(*h.bus.published.lock().unwrap(), vec![5]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_permanent_error_is_fatal_and_fails_every_buffered_job() {
        let store = ScriptedStore::new(|_, _| Step::Fail(Fail::Epoch));
        let mut h = Harness::start(store, 16, WriterLimits::default());
        h.send(ws(1)).await;
        let (job, reply) = server(2);
        h.send(job).await;
        assert!(answer(reply).await.is_err());
        h.ended().await;
        let (applied, fatal) = h.take_events();
        assert!(applied.is_empty());
        assert_eq!(fatal.len(), 1, "{fatal:?}");
        assert_eq!(h.store.calls().len(), 1);
        assert!(h.tx.send(ws(3)).await.is_err(), "the channel is still open");
    }

    #[tokio::test(start_paused = true)]
    async fn too_many_buffered_jobs_evicts_and_still_writes_them() {
        let up = Arc::new(AtomicBool::new(false));
        let flag = up.clone();
        let store = ScriptedStore::new(move |_, _| {
            if flag.load(Ordering::SeqCst) {
                Step::Ok
            } else {
                Step::Fail(Fail::Pool)
            }
        });
        let mut h = Harness::start(store, 64, WriterLimits::default());
        let last = BUFFER_MAX_JOBS as u64 + 1;
        for id in 1..=last {
            h.send(ws(id)).await;
        }
        let reason = h.next_fatal().await;
        assert!(reason.contains("buffer"), "{reason}");
        assert!(
            h.tx.send(ws(last + 1)).await.is_err(),
            "the channel is still open"
        );
        up.store(true, Ordering::SeqCst);
        h.ended().await;
        assert_eq!(h.store.appended(), (1..=last).collect::<Vec<_>>());
        assert!(h.store.calls().iter().all(|c| c.ids.len() <= BATCH_MAX));
        let (_, fatal) = h.take_events();
        assert!(fatal.is_empty(), "a second Fatal: {fatal:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn too_many_buffered_bytes_evicts() {
        let store = ScriptedStore::new(|_, _| Step::Fail(Fail::Pool));
        let mut h = Harness::start(store, 64, WriterLimits::default());
        const MIB: usize = 1024 * 1024;
        for id in 1..=32 {
            h.send(ws_sized(id, MIB)).await;
        }
        h.until_received().await;
        h.store.until(|&(s, f)| s >= 1 && s == f).await;
        let (_, fatal) = h.take_events();
        assert!(fatal.is_empty(), "32 MiB is within the limit: {fatal:?}");
        h.send(ws_sized(33, MIB)).await;
        let reason = h.next_fatal().await;
        assert!(reason.contains("buffer"), "{reason}");
    }

    /// Buffers `BUFFER_MAX_JOBS` WS jobs (the limit, not past it) on a store
    /// that fails until `up` is set, and waits until an attempt has failed
    /// and none is in flight.
    async fn full_buffer(capacity: usize) -> (Harness, Arc<AtomicBool>) {
        let up = Arc::new(AtomicBool::new(false));
        let flag = up.clone();
        let store = ScriptedStore::new(move |_, _| {
            if flag.load(Ordering::SeqCst) {
                Step::Ok
            } else {
                Step::Fail(Fail::Pool)
            }
        });
        let mut h = Harness::start(store, capacity, WriterLimits::default());
        for id in 1..=BUFFER_MAX_JOBS as u64 {
            h.send(ws(id)).await;
        }
        h.until_received().await;
        h.store.until(|&(s, f)| s >= 1 && s == f).await;
        let (_, fatal) = h.take_events();
        assert!(
            fatal.is_empty(),
            "a full buffer is within the limit: {fatal:?}"
        );
        (h, up)
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_job_that_overflows_the_buffer_is_answered_at_once() {
        let (mut h, up) = full_buffer(64).await;
        let server_id = BUFFER_MAX_JOBS as u64 + 1;
        let (job, reply) = server(server_id);
        h.send(job).await;
        let answer = tokio::time::timeout(Duration::from_millis(10), reply)
            .await
            .expect("the overflowing server job was not answered at once")
            .unwrap();
        assert!(answer.is_err(), "{answer:?}");
        let reason = h.next_fatal().await;
        assert!(reason.contains("buffer"), "{reason}");
        assert!(h.tx.is_closed());
        up.store(true, Ordering::SeqCst);
        h.ended().await;
        assert_eq!(
            h.store.appended(),
            (1..=BUFFER_MAX_JOBS as u64).collect::<Vec<_>>()
        );
        let (_, fatal) = h.take_events();
        assert!(fatal.is_empty(), "a second Fatal: {fatal:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_job_queued_behind_the_overflowing_job_is_answered_at_once() {
        let (mut h, up) = full_buffer(4).await;
        let last_ws = BUFFER_MAX_JOBS as u64 + 1;
        let (job, reply) = server(last_ws + 1);
        // Both queued before the writer receives either: the room sent its
        // WS job and then handled a server event.
        h.tx.try_send(ws(last_ws)).unwrap();
        h.tx.try_send(job).unwrap();
        let answer = tokio::time::timeout(Duration::from_millis(10), reply)
            .await
            .expect("the queued server job was not answered at once")
            .unwrap();
        assert!(answer.is_err(), "{answer:?}");
        let reason = h.next_fatal().await;
        assert!(reason.contains("buffer"), "{reason}");
        up.store(true, Ordering::SeqCst);
        h.ended().await;
        assert_eq!(h.store.appended(), (1..=last_ws).collect::<Vec<_>>());
        let (_, fatal) = h.take_events();
        assert!(fatal.is_empty(), "a second Fatal: {fatal:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_batch_that_always_times_out_is_split_until_it_fits() {
        let store = ScriptedStore::new(|n, rows| {
            if n == 0 {
                Step::Fail(Fail::Pool)
            } else if rows.len() > 30 {
                Step::Fail(Fail::Timeout)
            } else {
                Step::Ok
            }
        });
        let mut h = Harness::start(store, 256, WriterLimits::default());
        for id in 1..=200 {
            h.send(ws(id)).await;
        }
        assert_eq!(
            h.applied(200).await,
            (1..=200).map(|r| (r, r)).collect::<Vec<_>>()
        );
        let attempts: Vec<(usize, bool)> = h
            .store
            .calls()
            .iter()
            .map(|c| (c.ids.len(), c.ok.unwrap()))
            .collect();
        assert_eq!(
            attempts[0],
            (200, false),
            "the first, PoolTimedOut, attempt"
        );
        assert_eq!(
            attempts[1..8],
            [
                (200, false),
                (100, false),
                (50, false),
                (25, true),
                (50, false),
                (25, true),
                (50, false)
            ]
        );
        // From then on: 50 rows fail, 25 succeed, until the rest fits.
        let (mut cap, mut left, mut expected) = (BATCH_MAX, 200usize, vec![(200, false)]);
        while left > 0 {
            let size = left.min(cap);
            if size > 30 {
                expected.push((size, false));
                cap /= 2;
            } else {
                expected.push((size, true));
                left -= size;
                cap = (cap * 2).min(BATCH_MAX);
            }
        }
        assert_eq!(attempts, expected);
        assert_eq!(h.store.appended(), (1..=200).collect::<Vec<_>>());
        let (_, fatal) = h.take_events();
        assert!(fatal.is_empty(), "{fatal:?}");
        assert!(!h.done.is_done());
        let Harness { tx, mut done, .. } = h;
        drop(tx);
        tokio::time::timeout(Duration::from_secs(1), done.wait())
            .await
            .expect("the writer did not end once its channel closed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_deterministic_error_is_permanent() {
        let store = ScriptedStore::new(|_, _| Step::Fail(Fail::Data));
        let mut h = Harness::start(store, 16, WriterLimits::default());
        h.send(ws(1)).await;
        let (job, reply) = server(2);
        h.send(job).await;
        assert!(answer(reply).await.is_err());
        h.ended().await;
        let (_, fatal) = h.take_events();
        assert_eq!(fatal.len(), 1, "{fatal:?}");
        assert_eq!(
            h.store.calls().len(),
            1,
            "a deterministic error was retried"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_job_held_by_the_attempt_that_fails_past_its_deadline_is_not_retried() {
        let release = Arc::new(Notify::new());
        let gate = release.clone();
        let store = ScriptedStore::new(move |n, _| match n {
            0 => Step::Fail(Fail::Pool),
            1 => Step::Wait(gate.clone(), Box::new(Step::Fail(Fail::Pool))),
            _ => Step::Ok,
        });
        let mut h = Harness::start(store, 16, short_limits());
        h.send(ws(1)).await;
        let (job, reply) = server(2);
        h.send(job).await;
        h.store.until(|&(s, _)| s >= 2).await;
        assert_eq!(h.store.calls()[1].ids, vec![1, 2]);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let (_, fatal) = h.take_events();
        assert!(
            fatal.is_empty(),
            "an in-flight attempt was cut short: {fatal:?}"
        );
        release.notify_one();
        let reason = h.next_fatal().await;
        assert!(reason.contains("deadline"), "{reason}");
        assert!(answer(reply).await.is_err());
        h.ended().await;
        let calls = h.store.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[2].ids, vec![1]);
        assert_eq!(h.store.appended(), vec![1]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_backlog_flushes_in_batches_of_at_most_batch_max() {
        let store = ScriptedStore::new(|n, _| {
            if n == 0 {
                Step::Fail(Fail::Pool)
            } else {
                Step::Ok
            }
        });
        let h = Harness::start(store, 1024, WriterLimits::default());
        h.send(ws(1)).await;
        h.store.until(|&(_, f)| f >= 1).await;
        for id in 2..=450 {
            h.send(ws(id)).await;
        }
        let Harness {
            tx,
            mut done,
            store,
            ..
        } = h;
        drop(tx);
        tokio::time::timeout(Duration::from_secs(60), done.wait())
            .await
            .expect("the writer never ended");
        assert_eq!(store.successful_sizes(), vec![200, 200, 50]);
        assert_eq!(store.appended(), (1..=450).collect::<Vec<_>>());
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_channel_still_flushes_the_buffer() {
        let store = ScriptedStore::new(|n, _| {
            if n < 2 {
                Step::Fail(Fail::Pool)
            } else {
                Step::Ok
            }
        });
        let h = Harness::start(store, 16, WriterLimits::default());
        for id in 1..=3 {
            h.send(ws(id)).await;
        }
        let Harness {
            tx,
            mut done,
            store,
            ..
        } = h;
        drop(tx);
        assert!(!done.is_done());
        tokio::time::timeout(Duration::from_secs(60), done.wait())
            .await
            .expect("the writer never ended");
        assert!(done.is_done());
        assert_eq!(store.appended(), vec![1, 2, 3]);
        assert_eq!(store.calls().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_job_past_its_deadline_evicts_and_the_ws_jobs_ahead_are_still_written() {
        let up = Arc::new(AtomicBool::new(false));
        let flag = up.clone();
        let store = ScriptedStore::new(move |_, _| {
            if flag.load(Ordering::SeqCst) {
                Step::Ok
            } else {
                Step::Fail(Fail::Pool)
            }
        });
        let limits = WriterLimits::default();
        let mut h = Harness::start(store, 16, limits);
        h.send(ws(1)).await;
        h.send(ws(2)).await;
        let (job, reply) = server(3);
        h.send(job).await;
        h.store.until(|&(_, f)| f >= 1).await;
        tokio::time::sleep(limits.server_job_deadline).await;
        let reason = tokio::time::timeout(Duration::from_millis(10), h.next_fatal())
            .await
            .expect("no Fatal at the deadline");
        assert!(reason.contains("deadline"), "{reason}");
        assert!(answer(reply).await.is_err());
        assert!(h.tx.is_closed());
        let evicted_after = h.store.calls().len();
        up.store(true, Ordering::SeqCst);
        h.ended().await;
        let calls = h.store.calls();
        assert!(calls[evicted_after..].iter().all(|c| !c.ids.contains(&3)));
        assert_eq!(h.store.successful_sizes(), vec![2]);
        assert_eq!(h.store.appended(), vec![1, 2]);
        let (applied, fatal) = h.take_events();
        assert_eq!(applied, vec![(1, 1), (2, 2)]);
        assert!(fatal.is_empty(), "a second Fatal: {fatal:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_successful_attempt_never_fails_a_server_job() {
        let store =
            ScriptedStore::new(|_, _| Step::After(Duration::from_secs(6), Box::new(Step::Ok)));
        let mut h = Harness::start(store, 16, WriterLimits::default());
        h.send(ws(1)).await;
        h.store.until(|&(s, _)| s >= 1).await;
        let (job, reply) = server(2);
        h.send(job).await;
        tokio::time::sleep(Duration::from_secs(15)).await;
        let (_, fatal) = h.take_events();
        assert!(fatal.is_empty(), "{fatal:?}");
        assert_eq!(answer(reply).await, Ok(2));
        assert_eq!(h.store.appended(), vec![1, 2]);
        let ids: Vec<Vec<u64>> = h.store.calls().into_iter().map(|c| c.ids).collect();
        assert_eq!(ids, vec![vec![1], vec![2]]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_job_behind_a_succeeding_backlog_waits_without_a_deadline() {
        let store =
            ScriptedStore::new(|_, _| Step::After(Duration::from_secs(3), Box::new(Step::Ok)));
        let mut h = Harness::start(store, 1024, WriterLimits::default());
        for id in 1..=450 {
            h.send(ws(id)).await;
        }
        let (job, reply) = server(451);
        h.send(job).await;
        assert_eq!(answer(reply).await, Ok(451));
        assert_eq!(h.store.successful_sizes(), vec![200, 200, 51]);
        assert_eq!(h.store.calls()[2].ids.last(), Some(&451));
        let (_, fatal) = h.take_events();
        assert!(fatal.is_empty(), "{fatal:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_attempt_without_the_server_job_still_arms_its_deadline() {
        let attempt = Duration::from_millis(100);
        let store =
            ScriptedStore::new(move |_, _| Step::After(attempt, Box::new(Step::Fail(Fail::Pool))));
        let limits = WriterLimits::default();
        let mut h = Harness::start(store, 1024, limits);
        for id in 1..=450 {
            h.send(ws(id)).await;
        }
        // Arrives during the first attempt, which holds jobs 1..=200.
        h.store.until(|&(s, _)| s >= 1).await;
        let (job, reply) = server(451);
        h.send(job).await;
        let reason = h.next_fatal().await;
        let at = Instant::now();
        assert!(reason.contains("deadline"), "{reason}");
        let first = h.store.calls()[0].clone();
        assert_eq!(first.ids.len(), 200);
        assert_eq!(at - first.ended.unwrap(), limits.server_job_deadline);
        assert!(
            h.store.calls().iter().all(|c| !c.ids.contains(&451)),
            "an attempt held the server job"
        );
        assert!(answer(reply).await.is_err());
    }

    /// A deadline armed by a failed attempt still holds after a later
    /// attempt succeeds without the server-side job.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_armed_before_a_partial_success_still_holds() {
        let store = ScriptedStore::new(|n, _| match n {
            0 => Step::After(Duration::from_millis(100), Box::new(Step::Fail(Fail::Pool))),
            1 => Step::Ok,
            _ => Step::Fail(Fail::Pool),
        });
        let limits = WriterLimits::default();
        let mut h = Harness::start(store, 1024, limits);
        for id in 1..=450 {
            h.send(ws(id)).await;
        }
        h.store.until(|&(s, _)| s >= 1).await;
        let (job, reply) = server(451);
        h.send(job).await;
        let reason = h.next_fatal().await;
        let at = Instant::now();
        assert!(reason.contains("deadline"), "{reason}");
        let calls = h.store.calls();
        assert_eq!(calls[1].ok, Some(true), "{calls:?}");
        assert!(!calls[1].ids.contains(&451));
        assert_eq!(at - calls[0].ended.unwrap(), limits.server_job_deadline);
        assert!(answer(reply).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn ws_jobs_alone_never_hit_a_deadline() {
        let outage_ends = Instant::now() + Duration::from_secs(60);
        let store = ScriptedStore::new(move |_, _| {
            if Instant::now() < outage_ends {
                Step::Fail(Fail::Pool)
            } else {
                Step::Ok
            }
        });
        let h = Harness::start(store, 16, short_limits());
        for id in 1..=10 {
            h.send(ws(id)).await;
        }
        let Harness {
            tx,
            mut done,
            store,
            mut events,
            ..
        } = h;
        drop(tx);
        tokio::time::timeout(Duration::from_secs(120), done.wait())
            .await
            .expect("the writer never ended");
        assert_eq!(store.appended(), (1..=10).collect::<Vec<_>>());
        while let Ok(ev) = events.try_recv() {
            assert!(
                !matches!(ev, WriterEvent::Fatal(_)),
                "WS jobs alone got a Fatal"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_attempt_is_abandoned_and_retried() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let store = ScriptedStore::new(|n, _| if n == 0 { Step::Hang } else { Step::Ok });
        let limits = WriterLimits::default();
        let mut h = Harness::start(store, 16, limits);
        for id in 1..=3 {
            h.send(ws(id)).await;
        }
        h.store.until(|&(_, f)| f >= 2).await;
        let calls = h.store.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].ok, None, "the hanging attempt returned");
        assert!(calls[1].started - calls[0].started >= limits.attempt_timeout);
        assert_eq!(rec.counter("knot_writer_flush_retries_total", &[]), 1);
        assert_eq!(h.store.appended(), vec![1, 2, 3]);
        let (applied, fatal) = h.take_events();
        assert_eq!(applied, vec![(1, 1), (2, 2), (3, 3)]);
        assert!(fatal.is_empty(), "{fatal:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_publish_does_not_stop_the_writer() {
        let store = ScriptedStore::new(|_, _| Step::Ok);
        let mut h = Harness::start(store, 2, WriterLimits::default());
        h.bus.hang.store(true, Ordering::SeqCst);
        h.send(ws(1)).await;
        h.store.until(|&(_, f)| f >= 1).await;
        tokio::time::sleep(PUBLISH_TIMEOUT).await;
        for id in 2..=10 {
            let t = Instant::now();
            h.send(ws(id)).await;
            assert!(
                t.elapsed() <= PUBLISH_TIMEOUT + BATCH_INTERVAL,
                "a send blocked for {:?}",
                t.elapsed()
            );
        }
        h.applied(10).await;
        assert_eq!(h.store.appended(), (1..=10).collect::<Vec<_>>());
        assert!(h.bus.published.lock().unwrap().len() >= 2);
    }

    #[tokio::test(start_paused = true)]
    async fn jobs_are_received_while_an_attempt_is_in_flight() {
        let release = Arc::new(Notify::new());
        let gate = release.clone();
        let store = ScriptedStore::new(move |n, _| {
            if n == 0 {
                Step::Wait(gate.clone(), Box::new(Step::Ok))
            } else {
                Step::Ok
            }
        });
        let h = Harness::start(store, 2, WriterLimits::default());
        h.send(ws(1)).await;
        tokio::time::sleep(BATCH_INTERVAL).await;
        h.store.until(|&(s, _)| s >= 1).await;
        for id in 2..=10 {
            tokio::time::timeout(Duration::from_secs(1), h.send(ws(id)))
                .await
                .expect("a send blocked behind the in-flight attempt");
        }
        release.notify_one();
        h.store.until(|&(_, f)| f >= 2).await;
        let ids: Vec<Vec<u64>> = h.store.calls().into_iter().map(|c| c.ids).collect();
        assert_eq!(ids, vec![vec![1], (2..=10).collect::<Vec<_>>()]);
    }

    #[tokio::test(start_paused = true)]
    async fn writer_done_resolves_when_its_sender_is_dropped() {
        let (tx, mut done) = WriterDone::pair();
        assert!(!done.is_done());
        let waiter = tokio::spawn({
            let mut done = done.clone();
            async move { done.wait().await }
        });
        drop(tx);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("wait() did not return once the sender was dropped")
            .unwrap();
        assert!(done.is_done());
        done.wait().await;

        let (tx, done) = WriterDone::pair();
        tx.send(true).unwrap();
        assert!(done.is_done());
    }
}
