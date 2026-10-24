//! Integration: after an upgrade, the reindex worker's sweep re-renders every
//! cached Markdown row an older renderer wrote, and every live page that has
//! no row, and reindexes their tasks (P17). It never boots a room (P18), it
//! leaves alone what is already current (fresh cache rows, and task rows
//! whose values don't change), and only one replica sweeps at a time.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::AppState;
use knot_storage::{
    DocTaskInput, PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole,
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

/// The server state and rooms of `tasks_live_reindex.rs`, with pages that are
/// only in storage: no room has loaded them.
struct Harness {
    db: knot_test_support::TestDb,
    state: AppState,
    rooms: Arc<Rooms>,
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
            .with_dirty_tx(dirty_tx),
        );
        state.bus = Some(bus);
        state.rooms_v2 = Some(rooms.clone());
        Harness {
            db,
            state,
            rooms,
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
        let (_, update) = knot_markdown::from_markdown::parse(md).unwrap();
        self.store_update(doc, update).await;
        doc
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
        sqlx::query(
            "INSERT INTO doc_markdown_cache
                 (doc_id, rendered_at_seq, markdown_text, renderer_version, updated_at)
             VALUES ($1, $2, $3, $4, $5::timestamptz)",
        )
        .bind(doc)
        .bind(self.latest_seq(doc).await)
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
        self.state
            .tasks
            .as_ref()
            .unwrap()
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

    fn start_sweep(&mut self) {
        let rx = self.dirty_rx.take().expect("sweep already started");
        knot_server::reindex::spawn_with_sweep(self.state.clone(), rx);
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
        self.task_rows(doc)
            .await
            .into_iter()
            .map(|t| (t.item_index, t.text, t.checked))
            .collect()
    }

    async fn task_rows(&self, doc: Uuid) -> Vec<knot_storage::DocTask> {
        self.state
            .tasks
            .as_ref()
            .unwrap()
            .list_for_doc(doc)
            .await
            .unwrap()
    }

    /// Waits until every page in `docs` has a current cache row.
    async fn wait_until_rendered(&self, docs: &[Uuid]) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let mut done = true;
            for doc in docs {
                done &= matches!(self.cache_row(*doc).await, Some((2, _, _)));
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

fn long_ago() -> DateTime<Utc> {
    LONG_AGO.parse().unwrap()
}

fn owned(tasks: &[(i32, &str, bool)]) -> Vec<(i32, String, bool)> {
    tasks
        .iter()
        .map(|(i, text, checked)| (*i, (*text).to_owned(), *checked))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_rerenders_stale_rows_and_reindexes_tasks() {
    let mut h = Harness::new().await;

    // What an earlier release cached and indexed for a nested checklist.
    let broken = h.page("a", NESTED_CHECKLIST).await;
    let content = h.content(broken).await;
    let legacy_md = knot_markdown::legacy::to_markdown::serialise(&YrsEngine, &content).unwrap();
    let healed_md = knot_markdown::serialise(&YrsEngine, &content).unwrap();
    assert_eq!(healed_md, NESTED_CHECKLIST);
    assert_ne!(legacy_md, healed_md, "the legacy render is not broken");
    let legacy_tasks: Vec<(i32, String, bool)> =
        knot_markdown::legacy::tasks::extract_tasks(&legacy_md)
            .into_iter()
            .map(|t| (t.item_index, t.text, t.checked))
            .collect();
    let healed_tasks = owned(&[(0, "a", false), (1, "b", true), (2, "c", false)]);
    assert_ne!(legacy_tasks, healed_tasks, "the legacy task rows are right");
    h.seed_cache(broken, 1, &legacy_md).await;
    let seeded: Vec<(i32, &str, bool)> = legacy_tasks
        .iter()
        .map(|(i, text, checked)| (*i, text.as_str(), *checked))
        .collect();
    h.seed_tasks(broken, &seeded).await;

    // A stale page a room has loaded: the sweep reads that room's state.
    let open = h.page("b", "- [ ] open\n").await;
    h.seed_cache(open, 1, "stale\n").await;
    h.rooms.acquire(open).await.unwrap();

    // A row the current renderer wrote.
    let fresh = h.page("c", "fresh\n").await;
    h.seed_cache(fresh, 2, "fresh\n").await;

    h.start_sweep();
    h.wait_until_rendered(&[broken, open]).await;

    let (_, md, _) = h.cache_row(broken).await.unwrap();
    assert_eq!(md, healed_md);
    assert_eq!(h.tasks(broken).await, healed_tasks);
    let (_, md, _) = h.cache_row(open).await.unwrap();
    assert_eq!(md, "- [ ] open\n");
    assert_eq!(h.tasks(open).await, owned(&[(0, "open", false)]));
    assert_eq!(
        h.cache_row(fresh).await,
        Some((2, "fresh\n".to_owned(), long_ago()))
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
        assert_eq!(h.tasks(*doc).await, owned(&[(0, "ok", false)]));
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
    assert_eq!(h.tasks(doc).await, owned(&[(0, "done", true)]));
    assert!(h.rooms.get_active(doc).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_keeps_healthy_task_timestamps() {
    let mut h = Harness::new().await;
    let md = "- [ ] one\n- [x] two\n";
    let doc = h.page("a", md).await;
    h.seed_cache(doc, 1, md).await;
    let tasks = [(0, "one", false), (1, "two", true)];
    h.seed_tasks(doc, &tasks).await;
    let before = h.task_rows(doc).await;

    h.start_sweep();
    h.wait_until_rendered(&[doc]).await;

    let after = h.task_rows(doc).await;
    assert_eq!(h.tasks(doc).await, owned(&tasks));
    let stamps = |rows: &[knot_storage::DocTask]| -> Vec<_> {
        rows.iter()
            .map(|t| (t.created_at, t.updated_at, t.completed_at))
            .collect()
    };
    assert_eq!(stamps(&after), stamps(&before));
    assert!(after.iter().all(|t| t.updated_at == long_ago()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_waits_while_another_replica_sweeps() {
    let mut h = Harness::new().await;
    let doc = h.page("a", "- [ ] one\n").await;
    h.seed_cache(doc, 1, "- [ ] one\n").await;
    let mut other = h.pool().acquire().await.unwrap().detach();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(knot_server::reindex::SWEEP_LOCK_KEY)
        .execute(&mut other)
        .await
        .unwrap();

    h.start_sweep();
    // Two ticks.
    tokio::time::sleep(Duration::from_millis(4500)).await;

    assert_eq!(h.cache_row(doc).await.unwrap().0, 1);
    assert!(h.tasks(doc).await.is_empty());
}
