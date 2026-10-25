//! Background worker that turns the room actor's dirty notifications
//! (one `Uuid` per successful persist) into batched task-index
//! refreshes.
//!
//! Why a worker, not an inline call from the room:
//!   - The room actor lives in `knot-crdt` and must not depend on
//!     `AppState` / storage handles tied to HTTP. Decoupling via an
//!     mpsc keeps the layering clean.
//!   - A single user editing a doc emits many updates per second; we
//!     don't want to re-parse markdown on every keystroke. The worker
//!     accumulates dirty doc-ids into a HashSet and flushes on a
//!     periodic tick.
//!
//! With [`spawn_with_sweep`], the worker also refreshes the cached Markdown
//! an older renderer wrote, and the task rows of those pages (P17): a page
//! nobody edits after an upgrade would otherwise keep its old render in
//! zip exports, public shares and search, and its old task rows. Each tick
//! it takes a few stale pages, in id order, and refreshes them through the
//! same path as an edit, except that unchanged task rows are left alone.
//!
//! Failure handling: `refresh_markdown_and_index` failures are logged
//! and dropped. The next dirty notification for the same doc will
//! re-attempt; the task index is best-effort already (see
//! `routes::api::markdown`). A stale page whose refresh fails is tried
//! again on the sweep's next pass. That includes a refresh that wrote the
//! Markdown but not the task rows: it marks the cache row stale again.
//!
//! Each refresh runs in a task of its own, so a panic in it is logged and
//! ends that refresh only, not the worker. A render can also kill the whole
//! process (a stack overflow aborts it), and the sweep would meet such a
//! page again on every boot, on every replica. So the sweep records an
//! attempt before it renders a page (`doc_markdown_sweep_attempts`), clears
//! it when the render returns, and skips a page the current renderer
//! started twice without finishing (`SWEEP_ATTEMPTS`), until a render of
//! the page finishes, an edit's too (`put_rendered` clears the attempts).
//! The debug log names each page before its render.

use std::collections::HashSet;
use std::time::Duration;

use sqlx::Connection;
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

use crate::AppState;
use crate::routes::api::markdown::{
    RefreshError, refresh_markdown_and_index, refresh_markdown_quietly,
};

/// Flush dirty doc-ids every `FLUSH_INTERVAL`. Snappy enough that the
/// `/tasks` page feels live, infrequent enough that a heavy typist
/// pays at most one extract per cycle.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Stale pages the sweep refreshes per tick: at most five a second.
const SWEEP_BATCH: i64 = 10;

/// How long the sweep rests after a pass that found nothing left to
/// refresh, or when another replica is sweeping. Looking again catches
/// rows that old pods write during a rolling upgrade, and pages restored
/// from the archive, at the cost of one query.
const SWEEP_IDLE: Duration = Duration::from_secs(10 * 60);

/// Advisory-lock key that keeps the sweep to one replica at a time
/// ("knotswep" in ASCII).
pub const SWEEP_LOCK_KEY: i64 = 0x6b6e_6f74_7377_6570;

/// How many times the sweep starts rendering a page with the current
/// renderer without the render returning (it panicked, or the process died
/// during it) before it skips the page. One such render may be a deploy
/// stopping the process; two are taken to be the page's fault, until a
/// render of the page finishes after all.
const SWEEP_ATTEMPTS: i32 = 2;

/// Spawn the reindex worker. Returns immediately; the worker exits
/// when `rx` closes (i.e. the `Rooms` registry is dropped).
pub fn spawn(state: AppState, rx: mpsc::Receiver<Uuid>) {
    tokio::spawn(run(state, rx, None));
}

/// [`spawn`], plus the sweep that refreshes stale cached Markdown and the
/// task rows of those pages, starting at once.
pub fn spawn_with_sweep(state: AppState, rx: mpsc::Receiver<Uuid>) {
    tokio::spawn(run(state, rx, Some(Sweep::new())));
}

async fn run(state: AppState, mut rx: mpsc::Receiver<Uuid>, mut sweep: Option<Sweep>) {
    let mut pending: HashSet<Uuid> = HashSet::new();
    let mut tick = tokio::time::interval(FLUSH_INTERVAL);
    // `Burst` skip mode: if we miss a tick (heavy load), don't
    // try to catch up by firing back-to-back — just resume on the
    // next regular boundary.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(doc_id) => { pending.insert(doc_id); }
                None => break,
            },
            _ = tick.tick() => {
                let swept = match sweep.as_mut() {
                    Some(sweep) => sweep.next(&state).await,
                    None => Vec::new(),
                };
                if pending.is_empty() && swept.is_empty() {
                    continue;
                }
                let to_flush = std::mem::take(&mut pending);
                for doc_id in &to_flush {
                    if let Some(Err(e)) = refresh_caught(&state, *doc_id, Refresh::Edit).await {
                        tracing::warn!(error=?e, %doc_id, "reindex worker: refresh failed");
                    }
                }
                // A page that was also edited has just been refreshed.
                for doc_id in swept.into_iter().filter(|id| !to_flush.contains(id)) {
                    sweep_page(&state, doc_id).await;
                }
            }
        }
    }
    tracing::info!("reindex worker stopped");
}

/// Which refresh a page gets.
#[derive(Clone, Copy)]
enum Refresh {
    /// An edited page's: [`refresh_markdown_and_index`].
    Edit,
    /// A stale page's: [`refresh_markdown_quietly`].
    Sweep,
}

/// `refresh` of `doc_id`, run in a task of its own, so that a panic in it
/// ends that task, not the worker: `None` when it panicked, or the runtime
/// cancelled it as it shut down, which this logs.
async fn refresh_caught(
    state: &AppState,
    doc_id: Uuid,
    refresh: Refresh,
) -> Option<Result<String, RefreshError>> {
    let state = state.clone();
    let task = tokio::spawn(async move {
        match refresh {
            Refresh::Edit => refresh_markdown_and_index(&state, doc_id).await,
            Refresh::Sweep => refresh_markdown_quietly(&state, doc_id).await,
        }
    });
    match task.await {
        Ok(refreshed) => Some(refreshed),
        Err(e) if e.is_panic() => {
            tracing::error!(error = %e, %doc_id, "reindex worker: refresh panicked");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, %doc_id, "reindex worker: refresh cancelled");
            None
        }
    }
}

/// The sweep's refresh of `doc_id`, a stale page, between a recorded
/// attempt and its end ([`SWEEP_ATTEMPTS`]). A page whose renders never
/// finished is skipped, and so is one whose attempt can't be recorded:
/// unrecorded, it couldn't be skipped after a crash. The sweep's next pass
/// finds it again.
async fn sweep_page(state: &AppState, doc_id: Uuid) {
    let Some(cache) = state.markdown_cache.as_ref() else {
        return;
    };
    match cache
        .start_sweep_attempt(doc_id, knot_markdown::RENDERER_VERSION, SWEEP_ATTEMPTS)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                %doc_id,
                attempts = SWEEP_ATTEMPTS,
                "reindex sweep: skipped a page whose render never finished; an edit refreshes it"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, %doc_id, "reindex sweep: recording the attempt failed");
            return;
        }
    }
    tracing::debug!(%doc_id, "reindex sweep: rendering");
    match refresh_caught(state, doc_id, Refresh::Sweep).await {
        // The attempt stays recorded: the page is tried once more, then
        // skipped.
        None => return,
        Some(Ok(_)) => {}
        Some(Err(e)) => tracing::warn!(error=?e, %doc_id, "reindex sweep: refresh failed"),
    }
    if let Err(e) = cache.finish_sweep_attempt(doc_id).await {
        tracing::warn!(error = %e, %doc_id, "reindex sweep: ending the attempt failed");
    }
}

/// Where the sweep is: between passes, or in one, holding the sweep lock.
struct Sweep {
    /// A connection taken out of the pool that holds [`SWEEP_LOCK_KEY`]
    /// for the whole pass, so the refreshes of the pages it queues run
    /// under the lock too. The lock belongs to the session: it goes when
    /// the connection closes, also when this process dies mid-pass.
    ///
    /// The database can end the session too (a restart, a failover), so
    /// each tick checks the connection and ends the pass when it is gone,
    /// or doesn't answer within [`LOCK_CHECK_TIMEOUT`] (5 s). Until that
    /// check, another replica may start a pass alongside this one. That
    /// does no harm: a refresh writes the same result however often it
    /// runs, and never replaces a newer render (the seq guard).
    ///
    /// The check runs in the worker's only loop, before the refresh of
    /// edited pages, so a connection the network dropped silently delays
    /// those by 5 s at most.
    lock: Option<sqlx::PgConnection>,
    /// The last page this pass queued: the next query starts after it, so
    /// pages whose refresh fails can't hold the pass up.
    cursor: Option<Uuid>,
    /// When the next pass may start.
    next_pass: Instant,
}

impl Sweep {
    fn new() -> Self {
        Self {
            lock: None,
            cursor: None,
            next_pass: Instant::now(),
        }
    }

    /// The stale pages to refresh on this tick, starting a pass when one is
    /// due and this replica gets the lock, and ending it when the lock's
    /// connection is gone or doesn't answer.
    async fn next(&mut self, state: &AppState) -> Vec<Uuid> {
        let (Some(cache), Some(pool)) = (state.markdown_cache.as_ref(), state.pool.as_ref()) else {
            return Vec::new();
        };
        if let Some(conn) = self.lock.as_mut() {
            if !lock_answers(conn.ping(), LOCK_CHECK_TIMEOUT).await {
                // Dropped, not closed: closing a connection that didn't
                // answer could block as well, and a ping cut short leaves it
                // in the middle of an exchange anyway.
                self.lock = None;
                self.rest().await;
                return Vec::new();
            }
        } else {
            if Instant::now() < self.next_pass {
                return Vec::new();
            }
            match try_lock(pool).await {
                Ok(Some(conn)) => self.lock = Some(conn),
                Ok(None) => {
                    tracing::debug!("reindex sweep: another replica holds the lock");
                    self.rest().await;
                    return Vec::new();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "reindex sweep: lock failed");
                    self.rest().await;
                    return Vec::new();
                }
            }
        }
        match cache
            .stale_ids(knot_markdown::RENDERER_VERSION, self.cursor, SWEEP_BATCH)
            .await
        {
            Ok(ids) if ids.is_empty() => {
                self.rest().await;
                Vec::new()
            }
            Ok(ids) => {
                self.cursor = ids.last().copied();
                ids
            }
            Err(e) => {
                tracing::warn!(error = %e, "reindex sweep: stale query failed");
                self.rest().await;
                Vec::new()
            }
        }
    }

    /// End the pass, if one is running, and wait [`SWEEP_IDLE`] before the
    /// next.
    async fn rest(&mut self) {
        if let Some(conn) = self.lock.take() {
            // Ending the session releases the lock. A failed close ends it
            // too: the connection is dropped.
            let _ = conn.close().await;
        }
        self.cursor = None;
        self.next_pass = Instant::now() + SWEEP_IDLE;
    }
}

/// How long the sweep waits for its lock connection to answer the check
/// each tick. Without a limit, a half-open TCP connection would keep the
/// check waiting until the kernel gives up on it, about 15 minutes, and hold
/// up the `/tasks` refresh of every edit meanwhile.
const LOCK_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the lock connection answers `ping` within `limit`. When it
/// doesn't, the pass ends, and this logs why.
async fn lock_answers(
    ping: impl Future<Output = Result<(), sqlx::Error>>,
    limit: Duration,
) -> bool {
    match tokio::time::timeout(limit, ping).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reindex sweep: lock connection lost, pass ended");
            false
        }
        Err(_) => {
            tracing::warn!(
                ?limit,
                "reindex sweep: lock connection didn't answer, pass ended"
            );
            false
        }
    }
}

/// [`SWEEP_LOCK_KEY`] on a connection of its own, or `None` when another
/// replica holds it. The lock is session-level, so the connection is taken
/// out of the pool: back in the pool it would keep the lock after the pass
/// and stop every later one, on every replica. As in the archive purge job.
async fn try_lock(pool: &knot_storage::Pool) -> Result<Option<sqlx::PgConnection>, sqlx::Error> {
    let mut conn = pool.acquire().await?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SWEEP_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if locked {
        return Ok(Some(conn));
    }
    // A failed close still ends the session: the connection is dropped.
    let _ = conn.close().await;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};
    use std::time::Duration;

    use super::lock_answers;

    /// A lock connection that never answers its check, such as a half-open
    /// TCP connection, ends the pass once the limit is up.
    #[tokio::test]
    async fn a_lock_connection_that_does_not_answer_ends_the_pass() {
        let check = lock_answers(pending(), Duration::from_millis(10));
        let answered = tokio::time::timeout(Duration::from_secs(5), check)
            .await
            .expect("the check waited on for a ping that never answers");
        assert!(!answered);
    }

    #[tokio::test]
    async fn a_lock_connection_that_answers_keeps_the_pass() {
        assert!(lock_answers(ready(Ok(())), Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn a_lock_connection_that_fails_its_check_ends_the_pass() {
        let lost = ready(Err(sqlx::Error::PoolClosed));
        assert!(!lock_answers(lost, Duration::from_millis(10)).await);
    }
}
