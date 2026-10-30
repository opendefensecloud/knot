//! Integration: after an upgrade, the reindex worker's sweep re-renders every
//! cached Markdown row an older renderer wrote, and every live page that has
//! no row, and reindexes their tasks (P17). It never boots a room (P18), it
//! leaves alone what is already current (fresh cache rows, and task rows
//! whose values don't change), and only one replica sweeps at a time.
//!
//! A refresh that doesn't write a page's task rows leaves the page stale, so
//! the sweep refreshes it in full later.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_markdown::RENDERER_VERSION;
use knot_server::AppState;
use knot_server::routes::api::markdown::{export_markdown_only, refresh_markdown_and_index};
use knot_storage::{
    AssignedTask, DocTask, DocTaskInput, MarkdownCacheEntry, MarkdownCacheError,
    MarkdownCacheStore, PgMarkdownCache, PgSnapshotStore, PgTaskStore, PgUpdatesStore,
    SnapshotStore, TaskStore, TaskStoreError, UpdatesStore, WorkspaceRole,
};
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

/// How long a test waits for the sweep. Its first pass starts at once and
/// takes ten pages per two-second tick.
const DEADLINE: Duration = Duration::from_secs(15);

/// A timestamp no write of this test produces, to tell untouched rows apart.
const LONG_AGO: &str = "2001-01-01T00:00:00Z";

/// A page whose nested checklist the legacy writer flattened.
const NESTED_CHECKLIST: &str = "- [ ] a\n  - [x] b\n    - [ ] c\n";

/// The version a stale row carries.
const STALE: i32 = knot_storage::STALE_RENDERER_VERSION;

/// The server state and rooms of `tasks_live_reindex.rs`, with pages that are
/// only in storage: no room has loaded them.
struct Harness {
    db: knot_test_support::TestDb,
    state: AppState,
    rooms: Arc<Rooms>,
    /// What the rooms tell the reindex worker after an edit.
    dirty_tx: mpsc::Sender<Uuid>,
    dirty_rx: Option<mpsc::Receiver<Uuid>>,
    ws: Uuid,
    user: Uuid,
}

impl Harness {
    async fn new() -> Self {
        let db = knot_test_support::fresh_db().await;
        let pool = db.pool.clone();
        let mut state = AppState::with_pool(pool.clone());
        let ws = state
            .workspaces
            .as_ref()
            .unwrap()
            .create("default", "WS")
            .await
            .unwrap();
        let user = state
            .users
            .as_ref()
            .unwrap()
            .create_local("alice@example.com", "Alice", "$h$")
            .await
            .unwrap();
        state
            .workspaces
            .as_ref()
            .unwrap()
            .add_member(ws.id, user.id, WorkspaceRole::Owner)
            .await
            .unwrap();

        let bus: Arc<dyn knot_crdt::Bus> =
            Arc::new(knot_crdt::PgBus::connect(&db.url).await.unwrap());
        let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
        let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
        let policy = SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(3600),
        };
        let (dirty_tx, dirty_rx) = mpsc::channel::<Uuid>(64);
        let rooms = Arc::new(
            Rooms::new(
                Arc::new(YrsEngine),
                bus.clone(),
                updates,
                snaps,
                policy,
                Duration::from_secs(3600),
            )
            .with_dirty_tx(dirty_tx.clone()),
        );
        state.bus = Some(bus);
        state.rooms_v2 = Some(rooms.clone());
        Harness {
            db,
            state,
            rooms,
            dirty_tx,
            dirty_rx: Some(dirty_rx),
            ws: ws.id,
            user: user.id,
        }
    }

    fn pool(&self) -> &PgPool {
        &self.db.pool
    }

    /// A top-level page, keyed `sort_key`, whose stored content is `md`.
    async fn page(&self, sort_key: &str, md: &str) -> Uuid {
        let doc = self.empty_page(sort_key).await;
        self.fill(doc, md).await;
        doc
    }

    /// Stores `md` as the content of an empty page.
    async fn fill(&self, doc: Uuid, md: &str) {
        let (_, update) = knot_markdown::from_markdown::parse(md).unwrap();
        self.store_update(doc, update).await;
    }

    async fn empty_page(&self, sort_key: &str) -> Uuid {
        self.state
            .docs
            .as_ref()
            .unwrap()
            .create(self.ws, None, "P", sort_key, self.user)
            .await
            .unwrap()
            .id
    }

    async fn store_update(&self, doc: Uuid, update: Vec<u8>) -> i64 {
        PgUpdatesStore::new(self.pool().clone())
            .insert_batch(doc, &[(Some(self.user), update)])
            .await
            .unwrap()[0]
    }

    /// The page's content, read the way a room loads it.
    async fn content(&self, doc: Uuid) -> knot_crdt::DocHandle {
        knot_server::export_check::hydrate(self.pool(), doc)
            .await
            .unwrap()
    }

    async fn latest_seq(&self, doc: Uuid) -> i64 {
        PgUpdatesStore::new(self.pool().clone())
            .max_seq(doc)
            .await
            .unwrap()
    }

    /// A cache row as an earlier release wrote it, rendered at the page's
    /// latest seq, last written `LONG_AGO`.
    async fn seed_cache(&self, doc: Uuid, version: i32, md: &str) {
        self.seed_cache_at(doc, self.latest_seq(doc).await, version, md)
            .await;
    }

    /// [`Self::seed_cache`], rendered at `seq`.
    async fn seed_cache_at(&self, doc: Uuid, seq: i64, version: i32, md: &str) {
        sqlx::query(
            "INSERT INTO doc_markdown_cache
                 (doc_id, rendered_at_seq, markdown_text, renderer_version, updated_at)
             VALUES ($1, $2, $3, $4, $5::timestamptz)",
        )
        .bind(doc)
        .bind(seq)
        .bind(md)
        .bind(version)
        .bind(LONG_AGO)
        .execute(self.pool())
        .await
        .unwrap();
    }

    /// The page's task rows as the edit path writes them, last written
    /// `LONG_AGO`.
    async fn seed_tasks(&self, doc: Uuid, tasks: &[(i32, &str, bool)]) {
        let inputs: Vec<DocTaskInput> = tasks
            .iter()
            .map(|(item_index, text, checked)| DocTaskInput {
                item_index: *item_index,
                text: (*text).to_owned(),
                checked: *checked,
                ..DocTaskInput::default()
            })
            .collect();
        PgTaskStore::new(self.pool().clone())
            .upsert_for_doc(self.ws, doc, &inputs)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE doc_tasks SET updated_at = $2::timestamptz, created_at = $2::timestamptz
             WHERE doc_id = $1",
        )
        .bind(doc)
        .bind(LONG_AGO)
        .execute(self.pool())
        .await
        .unwrap();
    }

    /// The cache row and task rows an earlier release wrote for `doc`, a
    /// page holding [`NESTED_CHECKLIST`]: its legacy render as a stale row,
    /// and the task rows read from that render. Returns those task rows,
    /// after checking that both differ from what the current renderer
    /// writes.
    async fn seed_legacy_checklist(&self, doc: Uuid) -> Vec<(i32, String, bool)> {
        let content = self.content(doc).await;
        let legacy_md =
            knot_markdown::legacy::to_markdown::serialise(&YrsEngine, &content).unwrap();
        let healed_md = knot_markdown::serialise(&YrsEngine, &content).unwrap();
        assert_eq!(healed_md, NESTED_CHECKLIST);
        assert_ne!(legacy_md, healed_md, "the legacy render is not broken");
        let legacy_tasks: Vec<(i32, String, bool)> =
            knot_markdown::legacy::tasks::extract_tasks(&legacy_md)
                .into_iter()
                .map(|t| (t.item_index, t.text, t.checked))
                .collect();
        assert_ne!(
            legacy_tasks,
            owned(HEALED_CHECKLIST),
            "the legacy task rows are right"
        );
        self.seed_cache(doc, STALE, &legacy_md).await;
        let seeded: Vec<(i32, &str, bool)> = legacy_tasks
            .iter()
            .map(|(i, text, checked)| (*i, text.as_str(), *checked))
            .collect();
        self.seed_tasks(doc, &seeded).await;
        legacy_tasks
    }

    /// Replaces the task store with one whose writes can be made to fail.
    /// Call before [`Self::start_sweep`], which copies the state.
    fn flaky_tasks(&mut self) -> Arc<FlakyTasks> {
        let tasks = Arc::new(FlakyTasks {
            inner: PgTaskStore::new(self.pool().clone()),
            to_fail: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        });
        self.state.tasks = Some(tasks.clone());
        tasks
    }

    /// Replaces the markdown cache with one whose write of `doc`'s render
    /// panics. Call before [`Self::start_sweep`], which copies the state.
    fn panicky_cache(&mut self, doc: Uuid) -> Arc<PanickyCache> {
        let cache = Arc::new(PanickyCache {
            inner: PgMarkdownCache::new(self.pool().clone()),
            panics_on: doc,
            panicked: AtomicUsize::new(0),
        });
        self.state.markdown_cache = Some(cache.clone());
        cache
    }

    fn start_sweep(&mut self) {
        let rx = self.dirty_rx.take().expect("sweep already started");
        knot_server::reindex::spawn_with_sweep(self.state.clone(), rx);
    }

    /// The reindex worker without the sweep: it refreshes edited pages only.
    fn start_worker(&mut self) {
        let rx = self.dirty_rx.take().expect("worker already started");
        knot_server::reindex::spawn(self.state.clone(), rx);
    }

    /// An unfinished render of `doc` by the sweep, as an earlier process
    /// left it: `attempts` renders by renderer `version` that never
    /// returned, because they panicked or the process died during them.
    async fn seed_attempts(&self, doc: Uuid, version: i32, attempts: i32) {
        sqlx::query(
            "INSERT INTO doc_markdown_sweep_attempts (doc_id, renderer_version, attempts)
             VALUES ($1, $2, $3)",
        )
        .bind(doc)
        .bind(version)
        .bind(attempts)
        .execute(self.pool())
        .await
        .unwrap();
    }

    /// The sweep's unfinished renders of `doc`: renderer version and count.
    async fn attempts(&self, doc: Uuid) -> Option<(i32, i32)> {
        sqlx::query_as(
            "SELECT renderer_version, attempts FROM doc_markdown_sweep_attempts
             WHERE doc_id = $1",
        )
        .bind(doc)
        .fetch_optional(self.pool())
        .await
        .unwrap()
    }

    /// Waits until the sweep's unfinished renders of `doc` are `expected`.
    async fn wait_until_attempts(&self, doc: Uuid, expected: Option<(i32, i32)>) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let got = self.attempts(doc).await;
            if got == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the sweep's attempts at {doc} are {got:?}, not {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The page's cache row: renderer version, Markdown and last write.
    async fn cache_row(&self, doc: Uuid) -> Option<(i32, String, DateTime<Utc>)> {
        sqlx::query_as(
            "SELECT renderer_version, markdown_text, updated_at
             FROM doc_markdown_cache WHERE doc_id = $1",
        )
        .bind(doc)
        .fetch_optional(self.pool())
        .await
        .unwrap()
    }

    /// The page's task rows, as `(item_index, text, checked)`.
    async fn tasks(&self, doc: Uuid) -> Vec<(i32, String, bool)> {
        tuples(&self.task_rows(doc).await)
    }

    async fn task_rows(&self, doc: Uuid) -> Vec<DocTask> {
        PgTaskStore::new(self.pool().clone())
            .list_for_doc(doc)
            .await
            .unwrap()
    }

    /// Waits until the page's task rows are `expected`.
    async fn wait_until_tasks(&self, doc: Uuid, expected: &[(i32, &str, bool)]) {
        let expected = owned(expected);
        self.wait_until_task_rows(doc, |rows| tuples(rows) == expected)
            .await;
    }

    /// Waits until the page's task rows are `done`, and returns them. A
    /// refresh writes them after the cache row, in a transaction of their
    /// own, so a current cache row doesn't mean they are written yet.
    async fn wait_until_task_rows(
        &self,
        doc: Uuid,
        done: impl Fn(&[DocTask]) -> bool,
    ) -> Vec<DocTask> {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let rows = self.task_rows(doc).await;
            if done(&rows) {
                return rows;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the task rows of {doc} did not change as expected in time: {:?}",
                tuples(&rows)
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits until every page in `docs` has a current cache row.
    async fn wait_until_rendered(&self, docs: &[Uuid]) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let mut done = true;
            for doc in docs {
                done &= matches!(
                    self.cache_row(*doc).await,
                    Some((version, _, _)) if version == RENDERER_VERSION
                );
            }
            if done {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the sweep did not re-render {docs:?} in time"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// The task store, except that its next writes fail when told to, as when
/// the database drops out between a page's cache write and its task write.
struct FlakyTasks {
    inner: PgTaskStore,
    to_fail: AtomicUsize,
    failed: AtomicUsize,
}

impl FlakyTasks {
    /// Fail the next `n` writes.
    fn fail_next(&self, n: usize) {
        self.to_fail.store(n, Ordering::SeqCst);
    }

    /// The writes that failed so far.
    fn failed(&self) -> usize {
        self.failed.load(Ordering::SeqCst)
    }

    fn write(&self) -> Result<(), TaskStoreError> {
        if self
            .to_fail
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            self.failed.fetch_add(1, Ordering::SeqCst);
            return Err(TaskStoreError::Sqlx(sqlx::Error::PoolTimedOut));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl TaskStore for FlakyTasks {
    async fn upsert_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<(), TaskStoreError> {
        self.write()?;
        self.inner.upsert_for_doc(workspace_id, doc_id, items).await
    }

    async fn sync_for_doc_quiet(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<(), TaskStoreError> {
        self.write()?;
        self.inner
            .sync_for_doc_quiet(workspace_id, doc_id, items)
            .await
    }

    async fn list_for_assignee(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        include_completed: bool,
        hidden: &[Uuid],
    ) -> Result<Vec<AssignedTask>, TaskStoreError> {
        self.inner
            .list_for_assignee(workspace_id, user_id, include_completed, hidden)
            .await
    }

    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<DocTask>, TaskStoreError> {
        self.inner.list_for_doc(doc_id).await
    }
}

/// The markdown cache, except that writing one page's render panics, as a bug
/// in a writer would.
struct PanickyCache {
    inner: PgMarkdownCache,
    panics_on: Uuid,
    panicked: AtomicUsize,
}

impl PanickyCache {
    /// How many writes panicked so far.
    fn panicked(&self) -> usize {
        self.panicked.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl MarkdownCacheStore for PanickyCache {
    async fn get_if_fresh(
        &self,
        doc_id: Uuid,
        current_seq: i64,
    ) -> Result<Option<MarkdownCacheEntry>, MarkdownCacheError> {
        self.inner.get_if_fresh(doc_id, current_seq).await
    }

    async fn get(&self, doc_id: Uuid) -> Result<Option<MarkdownCacheEntry>, MarkdownCacheError> {
        self.inner.get(doc_id).await
    }

    async fn put(
        &self,
        doc_id: Uuid,
        rendered_at_seq: i64,
        markdown: &str,
    ) -> Result<(), MarkdownCacheError> {
        self.inner.put(doc_id, rendered_at_seq, markdown).await
    }

    async fn put_rendered(
        &self,
        doc_id: Uuid,
        rendered_at_seq: i64,
        markdown: &str,
        renderer_version: i32,
    ) -> Result<bool, MarkdownCacheError> {
        if doc_id == self.panics_on {
            self.panicked.fetch_add(1, Ordering::SeqCst);
            panic!("a render of {doc_id} panicked");
        }
        self.inner
            .put_rendered(doc_id, rendered_at_seq, markdown, renderer_version)
            .await
    }

    async fn stale_ids(
        &self,
        current: i32,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<Uuid>, MarkdownCacheError> {
        self.inner.stale_ids(current, after, limit).await
    }

    async fn mark_stale(&self, doc_id: Uuid) -> Result<(), MarkdownCacheError> {
        self.inner.mark_stale(doc_id).await
    }

    async fn invalidate(&self, doc_id: Uuid) -> Result<(), MarkdownCacheError> {
        self.inner.invalidate(doc_id).await
    }

    async fn start_sweep_attempt(
        &self,
        doc_id: Uuid,
        renderer_version: i32,
        max_attempts: i32,
    ) -> Result<bool, MarkdownCacheError> {
        self.inner
            .start_sweep_attempt(doc_id, renderer_version, max_attempts)
            .await
    }

    async fn finish_sweep_attempt(&self, doc_id: Uuid) -> Result<(), MarkdownCacheError> {
        self.inner.finish_sweep_attempt(doc_id).await
    }
}

fn long_ago() -> DateTime<Utc> {
    LONG_AGO.parse().unwrap()
}

fn owned(tasks: &[(i32, &str, bool)]) -> Vec<(i32, String, bool)> {
    tasks
        .iter()
        .map(|(i, text, checked)| (*i, (*text).to_owned(), *checked))
        .collect()
}

/// Task rows as `(item_index, text, checked)`.
fn tuples(rows: &[DocTask]) -> Vec<(i32, String, bool)> {
    rows.iter()
        .map(|t| (t.item_index, t.text.clone(), t.checked))
        .collect()
}

/// The task rows of [`NESTED_CHECKLIST`].
const HEALED_CHECKLIST: &[(i32, &str, bool)] = &[(0, "a", false), (1, "b", true), (2, "c", false)];

#[tokio::test(flavor = "multi_thread")]
async fn sweep_rerenders_stale_rows_and_reindexes_tasks() {
    let mut h = Harness::new().await;

    // What an earlier release cached and indexed for a nested checklist.
    let broken = h.page("a", NESTED_CHECKLIST).await;
    h.seed_legacy_checklist(broken).await;

    // A stale page a room has loaded is refreshed too, and its room stays.
    let open = h.page("b", "- [ ] open\n").await;
    h.seed_cache(open, STALE, "stale\n").await;
    h.rooms.acquire(open).await.unwrap();

    // A row the current renderer wrote.
    let fresh = h.page("c", "fresh\n").await;
    h.seed_cache(fresh, RENDERER_VERSION, "fresh\n").await;

    h.start_sweep();
    h.wait_until_rendered(&[broken, open]).await;

    let (_, md, _) = h.cache_row(broken).await.unwrap();
    assert_eq!(md, NESTED_CHECKLIST);
    h.wait_until_tasks(broken, HEALED_CHECKLIST).await;
    let (_, md, _) = h.cache_row(open).await.unwrap();
    assert_eq!(md, "- [ ] open\n");
    h.wait_until_tasks(open, &[(0, "open", false)]).await;
    assert_eq!(
        h.cache_row(fresh).await,
        Some((RENDERER_VERSION, "fresh\n".to_owned(), long_ago()))
    );

    // The sweep loaded no page into a room; the one already open stays so.
    assert!(h.rooms.get_active(broken).is_none());
    assert!(h.rooms.get_active(fresh).is_none());
    assert!(h.rooms.get_active(open).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_skips_past_a_failing_doc() {
    let mut h = Harness::new().await;
    // More pages that fail than one tick takes, all ahead of the good ones
    // in the sweep's order.
    let mut pages = Vec::new();
    for i in 0..12 {
        pages.push(h.empty_page(&format!("k{i:02}")).await);
    }
    pages.sort();
    let (failing, good) = pages.split_at(10);
    for doc in failing {
        // Bytes no update decodes from, so loading the page fails.
        h.store_update(*doc, vec![0xff; 4]).await;
    }
    let (_, update) = knot_markdown::from_markdown::parse("- [ ] ok\n").unwrap();
    for doc in good {
        h.store_update(*doc, update.clone()).await;
    }

    h.start_sweep();
    h.wait_until_rendered(good).await;

    for doc in good {
        assert_eq!(h.cache_row(*doc).await.unwrap().1, "- [ ] ok\n");
        h.wait_until_tasks(*doc, &[(0, "ok", false)]).await;
    }
    for doc in failing {
        assert!(h.cache_row(*doc).await.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_renders_a_page_without_cache_row() {
    let mut h = Harness::new().await;
    let doc = h.page("a", "# Plan\n\n- [x] done\n").await;

    h.start_sweep();
    h.wait_until_rendered(&[doc]).await;

    assert_eq!(h.cache_row(doc).await.unwrap().1, "# Plan\n\n- [x] done\n");
    h.wait_until_tasks(doc, &[(0, "done", true)]).await;
    assert!(h.rooms.get_active(doc).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_keeps_healthy_task_timestamps() {
    let mut h = Harness::new().await;
    // The healthy page, and after it in the sweep's order, so in the same
    // tick, a page whose task rows the sweep writes: once they are there,
    // the sweep is done with the healthy page.
    let mut pages = [h.empty_page("a").await, h.empty_page("b").await];
    pages.sort();
    let [doc, marker] = pages;
    let md = "- [ ] one\n- [x] two\n";
    h.fill(doc, md).await;
    h.fill(marker, "- [ ] marker\n").await;
    h.seed_cache(doc, STALE, md).await;
    let tasks = [(0, "one", false), (1, "two", true)];
    h.seed_tasks(doc, &tasks).await;
    let before = h.task_rows(doc).await;

    h.start_sweep();
    h.wait_until_rendered(&[doc, marker]).await;
    h.wait_until_tasks(marker, &[(0, "marker", false)]).await;

    let after = h.task_rows(doc).await;
    assert_eq!(h.tasks(doc).await, owned(&tasks));
    let stamps = |rows: &[DocTask]| -> Vec<_> {
        rows.iter()
            .map(|t| (t.created_at, t.updated_at, t.completed_at))
            .collect()
    };
    assert_eq!(stamps(&after), stamps(&before));
    assert!(after.iter().all(|t| t.updated_at == long_ago()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_rewrites_only_the_task_rows_that_change() {
    let mut h = Harness::new().await;
    let md = "- [ ] one\n- [x] two\n- [ ] three\n";
    let doc = h.page("a", md).await;
    h.seed_cache(doc, STALE, md).await;
    // Row 0 is right, row 1 has an older render's text, row 2 is missing and
    // row 5 is left over.
    h.seed_tasks(
        doc,
        &[(0, "one", false), (1, "old two", true), (5, "gone", false)],
    )
    .await;
    let before = h.task_rows(doc).await;

    h.start_sweep();
    h.wait_until_rendered(&[doc]).await;

    // One transaction writes the changes, so once they show, all of them do.
    h.wait_until_tasks(
        doc,
        &[(0, "one", false), (1, "two", true), (2, "three", false)],
    )
    .await;
    let after = h.task_rows(doc).await;
    let row = |rows: &[DocTask], i: i32| rows.iter().find(|t| t.item_index == i).unwrap().clone();
    let stamps = |t: DocTask| (t.created_at, t.updated_at, t.completed_at);
    // The row that stays the same is left as it was.
    assert_eq!(stamps(row(&after, 0)), stamps(row(&before, 0)));
    // The row that changes is updated now, and keeps when it was made and
    // ticked.
    let (was, is) = (row(&before, 1), row(&after, 1));
    assert!(is.updated_at > long_ago());
    assert_eq!(
        (is.created_at, is.completed_at),
        (was.created_at, was.completed_at)
    );
    // The missing row is written now.
    assert!(row(&after, 2).created_at > long_ago());
}

#[tokio::test(flavor = "multi_thread")]
async fn page_both_stale_and_edited_takes_the_edit_path() {
    let mut h = Harness::new().await;
    let md = "- [ ] one\n";
    let mut pages = Vec::new();
    for i in 0..11 {
        pages.push(h.page(&format!("k{i:02}"), md).await);
    }
    pages.sort();
    // The first tick takes the ten lowest; the next one takes `edited`.
    let (first, rest) = pages.split_at(10);
    let edited = rest[0];
    h.seed_cache(edited, STALE, md).await;
    h.seed_tasks(edited, &[(0, "one", false)]).await;

    h.start_sweep();
    h.wait_until_rendered(first).await;
    // An edit marks the page dirty before the tick that sweeps it.
    h.dirty_tx.send(edited).await.unwrap();
    h.wait_until_rendered(&[edited]).await;

    // The edit's reindex rewrites the row, which the sweep's would leave as
    // it was.
    let rows = h
        .wait_until_task_rows(edited, |rows| {
            !rows.is_empty() && rows.iter().all(|t| t.updated_at > long_ago())
        })
        .await;
    assert_eq!(tuples(&rows), owned(&[(0, "one", false)]));
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_older_than_the_cached_render_leaves_the_tasks() {
    let h = Harness::new().await;
    let doc = h.page("a", "- [ ] one\n").await;
    // A render of a later state than the one this refresh reads, written
    // by another replica, say.
    let later = h.latest_seq(doc).await + 5;
    h.seed_cache_at(doc, later, RENDERER_VERSION, "- [x] one\n")
        .await;
    h.seed_tasks(doc, &[(0, "one", true)]).await;

    let md = refresh_markdown_and_index(&h.state, doc).await.unwrap();

    assert_eq!(md, "- [ ] one\n");
    assert_eq!(
        h.cache_row(doc).await,
        Some((RENDERER_VERSION, "- [x] one\n".to_owned(), long_ago()))
    );
    assert_eq!(h.tasks(doc).await, owned(&[(0, "one", true)]));
    let rows = h.task_rows(doc).await;
    assert!(rows.iter().all(|t| t.updated_at == long_ago()), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_heals_a_template_read_before_its_turn() {
    let mut h = Harness::new().await;
    let template = h.page("a", NESTED_CHECKLIST).await;
    h.state
        .docs
        .as_ref()
        .unwrap()
        .set_template(h.ws, template, h.user, true)
        .await
        .unwrap();
    let legacy_tasks = h.seed_legacy_checklist(template).await;

    // Making a page from the template reads it without touching its task
    // rows, so the template stays stale.
    let md = export_markdown_only(&h.state, template).await.unwrap();
    assert_eq!(md, NESTED_CHECKLIST);
    assert_eq!(h.tasks(template).await, legacy_tasks);
    let (version, cached, _) = h.cache_row(template).await.unwrap();
    assert_eq!((version, cached.as_str()), (STALE, NESTED_CHECKLIST));

    h.start_sweep();
    h.wait_until_rendered(&[template]).await;

    h.wait_until_tasks(template, HEALED_CHECKLIST).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_leaves_a_page_stale_when_its_task_write_fails() {
    let mut h = Harness::new().await;
    let tasks = h.flaky_tasks();
    tasks.fail_next(1);
    let doc = h.page("a", "- [ ] one\n").await;

    h.start_sweep();
    // The refresh writes the render, then fails to write the task rows.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while !(tasks.failed() == 1
        && matches!(h.cache_row(doc).await, Some((version, _, _)) if version == STALE))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page is not stale again: {:?}, {} failed writes",
            h.cache_row(doc).await,
            tasks.failed()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The render stays, and the next pass's query finds the page again.
    assert_eq!(h.cache_row(doc).await.unwrap().1, "- [ ] one\n");
    assert!(h.tasks(doc).await.is_empty());
    let stale = h
        .state
        .markdown_cache
        .as_ref()
        .unwrap()
        .stale_ids(RENDERER_VERSION, None, 10)
        .await
        .unwrap();
    assert_eq!(stale, [doc]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_retries_a_page_whose_edit_reindex_failed() {
    let mut h = Harness::new().await;
    let tasks = h.flaky_tasks();
    let doc = h.page("a", "- [ ] one\n").await;

    // An edit's refresh writes the render, then fails to write the tasks.
    tasks.fail_next(1);
    let md = refresh_markdown_and_index(&h.state, doc).await.unwrap();
    assert_eq!(md, "- [ ] one\n");
    assert_eq!(tasks.failed(), 1);
    assert!(h.tasks(doc).await.is_empty());
    assert_eq!(h.cache_row(doc).await.unwrap().0, STALE);

    h.start_sweep();
    h.wait_until_rendered(&[doc]).await;

    h.wait_until_tasks(doc, &[(0, "one", false)]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_waits_while_another_replica_sweeps() {
    let mut h = Harness::new().await;
    let doc = h.page("a", "- [ ] one\n").await;
    h.seed_cache(doc, STALE, "- [ ] one\n").await;
    let mut other = h.pool().acquire().await.unwrap().detach();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(knot_server::reindex::SWEEP_LOCK_KEY)
        .execute(&mut other)
        .await
        .unwrap();

    h.start_sweep();
    // Two ticks.
    tokio::time::sleep(Duration::from_millis(4500)).await;

    assert_eq!(h.cache_row(doc).await.unwrap().0, STALE);
    assert!(h.tasks(doc).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_ends_its_pass_when_its_lock_connection_drops() {
    let mut h = Harness::new().await;
    // Three ticks' worth of pages.
    let mut pages = Vec::new();
    for i in 0..30 {
        pages.push(h.empty_page(&format!("k{i:02}")).await);
    }
    pages.sort();

    h.start_sweep();
    h.wait_until_rendered(&pages[..10]).await;
    // The database ends the session that holds the lock, as a restart or a
    // failover does.
    let ended: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(l.pid)
         FROM pg_locks l
         JOIN pg_database db ON db.oid = l.database
         WHERE db.datname = current_database()
           AND l.locktype = 'advisory'
           AND l.objsubid = 1
           AND (l.classid::bigint << 32 | l.objid::bigint) = $1",
    )
    .bind(knot_server::reindex::SWEEP_LOCK_KEY)
    .fetch_all(h.pool())
    .await
    .unwrap();
    assert_eq!(ended, [true]);

    // Three ticks on, the pass has stopped short of the last pages: it
    // ended once its lock was gone.
    tokio::time::sleep(Duration::from_millis(6500)).await;
    for doc in &pages[20..] {
        assert!(h.cache_row(*doc).await.is_none(), "{doc} was refreshed");
    }
}

/// A page nested far deeper than the editor makes one (an import of `>`
/// repeated 100,000 times) is rendered degraded, and the sweep goes on to the
/// next page. A writer that recursed once per quote overflowed the worker's
/// stack, which aborts the process, and the sweep met the page again on every
/// boot (final review md-1).
#[tokio::test(flavor = "multi_thread")]
async fn sweep_renders_a_deeply_nested_page_degraded_and_moves_on() {
    let mut h = Harness::new().await;
    let mut pages = [h.empty_page("a").await, h.empty_page("b").await];
    pages.sort();
    let [deep, after] = pages;
    h.fill(deep, &format!("{} x\n", ">".repeat(100_000))).await;
    h.fill(after, "- [ ] after\n").await;

    h.start_sweep();
    h.wait_until_rendered(&[deep, after]).await;

    let degraded = format!("{}x\n", "> ".repeat(knot_markdown::to_markdown::MAX_DEPTH));
    assert_eq!(h.cache_row(deep).await.unwrap().1, degraded);
    assert_eq!(h.cache_row(after).await.unwrap().1, "- [ ] after\n");
    h.wait_until_tasks(after, &[(0, "after", false)]).await;
}

/// A page whose render killed the process (a stack overflow aborts it) is
/// skipped after a restart, so that it can't kill every replica in turn, on
/// every boot. The sweep records an attempt before it renders a page, and
/// clears it when the render returns; it skips a page the current renderer
/// started twice without finishing. A page started once (a deploy may have
/// stopped the process) is tried again, and so is one only an older renderer
/// never finished.
#[tokio::test(flavor = "multi_thread")]
async fn sweep_skips_a_page_whose_render_never_finished_twice() {
    let mut h = Harness::new().await;
    let mut pages = Vec::new();
    for i in 0..4 {
        pages.push(h.page(&format!("k{i}"), "- [ ] one\n").await);
    }
    // In the sweep's order, so that its turn at the page it skips comes
    // before the renders the test waits for.
    pages.sort();
    let (killer, once, older, fresh) = (pages[0], pages[1], pages[2], pages[3]);
    h.seed_attempts(killer, RENDERER_VERSION, 2).await;
    h.seed_attempts(once, RENDERER_VERSION, 1).await;
    h.seed_attempts(older, STALE, 2).await;

    h.start_sweep();
    h.wait_until_rendered(&[once, older, fresh]).await;
    for doc in [once, older, fresh] {
        h.wait_until_attempts(doc, None).await;
        h.wait_until_tasks(doc, &[(0, "one", false)]).await;
    }

    assert!(h.cache_row(killer).await.is_none());
    assert!(h.tasks(killer).await.is_empty());
    assert_eq!(h.attempts(killer).await, Some((RENDERER_VERSION, 2)));
}

/// A page the sweep gave up on (two of its renders were cut short, by
/// deploys say) is swept again once a render of it finishes, an edit's too,
/// and it goes stale again (F1 review).
#[tokio::test(flavor = "multi_thread")]
async fn sweep_takes_a_page_again_once_a_render_of_it_finishes() {
    let mut h = Harness::new().await;
    let doc = h.page("a", "- [ ] one\n").await;
    h.seed_attempts(doc, RENDERER_VERSION, 2).await;

    refresh_markdown_and_index(&h.state, doc).await.unwrap();
    assert_eq!(h.attempts(doc).await, None, "the edit's render finished");

    // Stale again, as after a failed task write or an old pod's render.
    h.state
        .markdown_cache
        .as_ref()
        .unwrap()
        .mark_stale(doc)
        .await
        .unwrap();
    h.start_sweep();
    h.wait_until_rendered(&[doc]).await;
    h.wait_until_attempts(doc, None).await;
}

/// A render that panics ends neither the worker nor the sweep's pass: the
/// sweep renders the pages after it. The page stays stale, and its attempt
/// stays recorded, so the sweep tries it once more on its next pass and
/// then skips it.
#[tokio::test(flavor = "multi_thread")]
async fn sweep_moves_on_past_a_render_that_panics() {
    let mut h = Harness::new().await;
    let mut pages = [h.empty_page("a").await, h.empty_page("b").await];
    pages.sort();
    let [panics, after] = pages;
    h.fill(panics, "- [ ] boom\n").await;
    h.fill(after, "- [ ] after\n").await;
    let cache = h.panicky_cache(panics);

    h.start_sweep();
    h.wait_until_rendered(&[after]).await;
    h.wait_until_tasks(after, &[(0, "after", false)]).await;

    assert_eq!(cache.panicked(), 1);
    assert!(h.cache_row(panics).await.is_none());
    h.wait_until_attempts(panics, Some((RENDERER_VERSION, 1)))
        .await;
}

/// An edit whose refresh panics doesn't stop the reindex worker: it goes on
/// refreshing the pages edited after it.
#[tokio::test(flavor = "multi_thread")]
async fn worker_goes_on_after_a_refresh_panics() {
    let mut h = Harness::new().await;
    let panics = h.page("a", "- [ ] boom\n").await;
    let edited = h.page("b", "- [ ] edited\n").await;
    let cache = h.panicky_cache(panics);

    h.start_worker();
    h.dirty_tx.send(panics).await.unwrap();
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while cache.panicked() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "no refresh ran");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    h.dirty_tx
        .send(edited)
        .await
        .expect("the reindex worker stopped");
    h.wait_until_rendered(&[edited]).await;
    h.wait_until_tasks(edited, &[(0, "edited", false)]).await;
}
