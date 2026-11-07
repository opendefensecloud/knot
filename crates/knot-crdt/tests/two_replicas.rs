//! Two 0.9.10 replicas and a 0.9.9 pod writing one page at once, as during
//! the rolling upgrade to 0.9.10 (page history M1.2, Review Focus 2): the
//! 0.9.9 pod's rows get revs from the numbering trigger and interleave
//! gaplessly with the replicas' rows, each replica's room picks up every row
//! by rev, and the 0.9.9 pod's snapshot upserts never overwrite a stamped
//! snapshot.

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/log_fixtures.rs"]
mod log_fixtures;

use fixtures::{Rng, random_edit};
use knot_crdt::{Bus, DocHandle, Event, MemBus, RoomHandle, Rooms, SnapshotPolicy, YrsEngine};
use knot_storage::{
    DocSnapshot, DocUpdate, HistoryHead, NewUpdate, Persisted, PgSnapshotStore, PgUpdatesStore,
    RowsAfter, SnapshotMeta, SnapshotStamp, SnapshotStore, SnapshotStoreError, UpdatesStore,
    UpdatesStoreError,
};
use knot_test_support::old_sql::{OLD_INSERT_UPDATES_SQL, OLD_SNAPSHOT_UPSERT_SQL};
use log_fixtures::{Client, assert_same_doc, doc_of, replay, row_count};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::oneshot;
use uuid::Uuid;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, StateVector, Transact, XmlFragmentRef};

/// The rooms snapshot every 7 rows.
const POLICY: SnapshotPolicy = SnapshotPolicy {
    every_n: 7,
    idle: Duration::from_secs(3600),
};

/// Edits typed by the three editors, together.
const EDITS: usize = 200;

/// Saves of the 0.9.9 pod (1-3 rows each); it snapshots on every 5th.
const OLD_SAVES: usize = 60;

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

/// A replica's (or the 0.9.9 pod's) own pool on the test database.
async fn pool_of(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(8)
        .connect(url)
        .await
        .unwrap()
}

/// `n` users, and a page in a workspace of its own created by the first.
async fn page_and_users(pool: &PgPool, n: usize) -> (Uuid, Vec<Uuid>) {
    let ws: Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
            .fetch_one(pool)
            .await
            .unwrap();
    let mut users = Vec::new();
    for i in 0..n {
        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email, display_name) VALUES ($1, $2) RETURNING id",
        )
        .bind(format!("u{i}@x.test"))
        .bind(format!("U{i}"))
        .fetch_one(pool)
        .await
        .unwrap();
        users.push(user);
    }
    let doc = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(users[0])
    .fetch_one(pool)
    .await
    .unwrap();
    (doc, users)
}

/// Waits until the page's row count has not changed for a second.
async fn quiesce(pool: &PgPool, doc: Uuid) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut stable_since = (row_count(pool, doc).await, tokio::time::Instant::now());
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let count = row_count(pool, doc).await;
        if count != stable_since.0 {
            stable_since = (count, tokio::time::Instant::now());
        } else if stable_since.1.elapsed() >= Duration::from_secs(1) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page's rows never stopped growing ({count} rows)"
        );
    }
}

// ---------------------------------------------------------------------------
// A store that keeps every error
// ---------------------------------------------------------------------------

/// A replica's stores (`PgUpdatesStore` and `PgSnapshotStore`), which keep
/// every error they return: the writer retries a failed append and the
/// room only logs a failed snapshot write, so the rows alone cannot show
/// that a write deadlocked and was retried.
struct Recording {
    updates: PgUpdatesStore,
    snapshots: PgSnapshotStore,
    errors: Mutex<Vec<String>>,
}

impl Recording {
    fn new(pool: &PgPool) -> Arc<Self> {
        Arc::new(Recording {
            updates: PgUpdatesStore::new(pool.clone()),
            snapshots: PgSnapshotStore::new(pool.clone()),
            errors: Mutex::new(Vec::new()),
        })
    }

    /// Keeps `r`'s error, with its SQLSTATE, and returns `r`.
    fn keep<T, E: std::fmt::Display>(
        &self,
        call: &str,
        r: Result<T, E>,
        sqlx_error: impl Fn(&E) -> Option<&sqlx::Error>,
    ) -> Result<T, E> {
        if let Err(e) = &r {
            let code = sqlx_error(e)
                .and_then(|e| e.as_database_error())
                .and_then(|d| d.code())
                .map_or_else(|| "-".to_string(), |c| c.into_owned());
            self.errors
                .lock()
                .unwrap()
                .push(format!("{call}: SQLSTATE {code}: {e}"));
        }
        r
    }

    fn keep_u<T>(
        &self,
        call: &str,
        r: Result<T, UpdatesStoreError>,
    ) -> Result<T, UpdatesStoreError> {
        self.keep(call, r, |e| match e {
            UpdatesStoreError::Sqlx(e) => Some(e),
            _ => None,
        })
    }

    fn keep_s<T>(
        &self,
        call: &str,
        r: Result<T, SnapshotStoreError>,
    ) -> Result<T, SnapshotStoreError> {
        self.keep(call, r, |e| match e {
            SnapshotStoreError::Sqlx(e) => Some(e),
            _ => None,
        })
    }

    fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl UpdatesStore for Recording {
    async fn append(
        &self,
        doc_id: Uuid,
        epoch: i32,
        rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError> {
        self.keep_u("append", self.updates.append(doc_id, epoch, rows).await)
    }
    async fn rows_after(
        &self,
        doc_id: Uuid,
        from: RowsAfter,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        self.keep_u("rows_after", self.updates.rows_after(doc_id, from).await)
    }
    async fn head(&self, doc_id: Uuid) -> Result<HistoryHead, UpdatesStoreError> {
        self.keep_u("head", self.updates.head(doc_id).await)
    }
    async fn ensure_legacy_numbered(&self, doc_id: Uuid) -> Result<u64, UpdatesStoreError> {
        let numbered = self.updates.ensure_legacy_numbered(doc_id).await;
        self.keep_u("ensure_legacy_numbered", numbered)
    }
    async fn since(&self, doc_id: Uuid, after: i64) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        self.keep_u("since", self.updates.since(doc_id, after).await)
    }
    async fn max_seq(&self, doc_id: Uuid) -> Result<i64, UpdatesStoreError> {
        self.keep_u("max_seq", self.updates.max_seq(doc_id).await)
    }
}

#[async_trait::async_trait]
impl SnapshotStore for Recording {
    async fn insert_stamped(
        &self,
        doc_id: Uuid,
        stamp: SnapshotStamp,
        state_bytes: &[u8],
        state_vector: &[u8],
    ) -> Result<(), SnapshotStoreError> {
        let written = self
            .snapshots
            .insert_stamped(doc_id, stamp, state_bytes, state_vector)
            .await;
        self.keep_s("insert_stamped", written)
    }
    async fn latest(&self, doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        self.keep_s("latest", self.snapshots.latest(doc_id).await)
    }
    async fn gc(
        &self,
        doc_id: Uuid,
        keep_recent: i64,
        retain_days: i32,
    ) -> Result<u64, SnapshotStoreError> {
        let gc = self.snapshots.gc(doc_id, keep_recent, retain_days).await;
        self.keep_s("gc", gc)
    }
    async fn list(
        &self,
        doc_id: Uuid,
        limit: i64,
    ) -> Result<Vec<SnapshotMeta>, SnapshotStoreError> {
        self.keep_s("list", self.snapshots.list(doc_id, limit).await)
    }
    async fn by_seq(
        &self,
        doc_id: Uuid,
        snapshot_seq: i64,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        self.keep_s("by_seq", self.snapshots.by_seq(doc_id, snapshot_seq).await)
    }
}

// ---------------------------------------------------------------------------
// Replicas and editors
// ---------------------------------------------------------------------------

/// A 0.9.10 replica: its own pool and its own rooms, on the shared bus.
fn replica(pool: &PgPool, bus: &Arc<MemBus>) -> (Rooms, Arc<Recording>) {
    let stores = Recording::new(pool);
    let rooms = Rooms::new(
        Arc::new(YrsEngine),
        bus.clone(),
        stores.clone(),
        stores.clone(),
        POLICY,
        Duration::from_secs(300),
    );
    (rooms, stores)
}

/// The room's doc, copied.
async fn room_doc(h: &RoomHandle) -> DocHandle {
    let (tx, rx) = oneshot::channel();
    h.tx.send(Event::ExportState(tx)).await.unwrap();
    let (bytes, _seq) = rx.await.unwrap().unwrap();
    doc_of(&[&bytes])
}

/// Makes the room catch up on every row committed before this call, then
/// returns its doc. `SuggestionApplied` runs the rev catch-up before it
/// answers and answers only when the room caught up (it drops the reply at
/// a rev gap or a failed read), so its answer means the room holds the log
/// up to its read. A bus publish would not do: the room's `biased` select
/// serves its event channel before the bus, and `MemBus` drops a subscriber
/// whose channel was once full, so after a burst of edits only the room's
/// 5 s tick would catch up. A dropped reply is retried for up to 10 s.
async fn caught_up_doc(h: &RoomHandle, what: &str) -> DocHandle {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (reply, answer) = oneshot::channel();
        h.tx.send(Event::SuggestionApplied {
            suggestion_id: Uuid::new_v4(),
            apply_token: Uuid::new_v4(),
            reply,
        })
        .await
        .unwrap();
        match tokio::time::timeout(Duration::from_secs(10), answer).await {
            Ok(Ok(applied)) => {
                assert!(!applied, "{what}: an unknown suggestion reads as applied");
                return room_doc(h).await;
            }
            Ok(Err(_)) => {}
            Err(_) => panic!("{what}: the applied check hung"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: the room never caught up (a rev gap?)"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

impl Client {
    /// Types `edits` random transactions into `h`, catching up on what the
    /// room fans out before each one.
    async fn type_into(&mut self, h: &RoomHandle, edits: usize, seed: u64) {
        let mut rng = Rng::new(seed);
        for _ in 0..edits {
            self.sync();
            let update = self.edit(&mut rng);
            self.send(h, update).await;
            // Bursts with pauses, so the rooms find moments with nothing in
            // flight and write snapshots while the others keep saving.
            let pause = if rng.below(12) == 0 {
                300 + rng.below(400)
            } else {
                rng.below(15)
            };
            tokio::time::sleep(Duration::from_millis(pause as u64)).await;
        }
    }
}

/// A 0.9.9 pod saving edits to the page with its own SQL, as its rooms do,
/// from a yrs doc of its own.
struct OldPod {
    pool: PgPool,
    doc: Uuid,
    mine: Doc,
    frag: XmlFragmentRef,
    rng: Rng,
    /// Every error its statements returned.
    errors: Vec<String>,
}

impl OldPod {
    fn new(pool: PgPool, doc: Uuid) -> Self {
        let mine = Doc::with_client_id(9);
        let frag = mine.get_or_insert_xml_fragment("default");
        OldPod {
            pool,
            doc,
            mine,
            frag,
            rng: Rng::new(9),
            errors: Vec::new(),
        }
    }

    /// `OLD_SAVES` saves of 1-3 rows each, each published on the bus with
    /// its last seq, and on every 5th save a snapshot ([`OldPod::snapshot`])
    /// a moment later, as a 0.9.9 room writes one after its own counter
    /// fires.
    async fn run(mut self, bus: Arc<MemBus>, users: Vec<Uuid>) -> Self {
        for save in 1..=OLD_SAVES {
            let n = 1 + self.rng.below(3);
            let mut by_user: Vec<Option<Uuid>> = Vec::new();
            let mut bytes: Vec<Vec<u8>> = Vec::new();
            for _ in 0..n {
                let before = self.mine.transact().state_vector();
                random_edit(&self.mine, &self.frag, &mut self.rng);
                bytes.push(self.mine.transact().encode_state_as_update_v1(&before));
                by_user.push(Some(users[self.rng.below(users.len())]));
            }
            let seqs: Result<Vec<i64>, sqlx::Error> = sqlx::query_scalar(OLD_INSERT_UPDATES_SQL)
                .bind(self.doc)
                .bind(by_user)
                .bind(bytes)
                .fetch_all(&self.pool)
                .await;
            match seqs {
                Ok(seqs) => {
                    let _ = bus.publish(self.doc, *seqs.last().unwrap()).await;
                }
                Err(e) => self.errors.push(format!("save {save}: {e}")),
            }
            if save % 5 == 0 {
                let pause = self.rng.below(400) as u64;
                tokio::time::sleep(Duration::from_millis(pause)).await;
                if let Err(e) = self.snapshot().await {
                    self.errors.push(format!("snapshot after save {save}: {e}"));
                }
            }
            let pause = 5 + self.rng.below(20) as u64;
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
        self
    }

    /// A 0.9.9 room's snapshot: the state of this pod's doc, which holds
    /// only what this pod saved (never the page's replay), upserted at the
    /// page's highest seq, so it lands on the seqs of stamped snapshots.
    /// Returns how many rows it changed: 0 when a stamped snapshot held
    /// that seq (`doc_snapshots_epoch` skips the update).
    async fn snapshot(&self) -> Result<u64, sqlx::Error> {
        let (state, vector) = {
            let txn = self.mine.transact();
            (
                txn.encode_state_as_update_v1(&StateVector::default()),
                txn.state_vector().encode_v1(),
            )
        };
        let seq: i64 = sqlx::query_scalar("SELECT max(seq) FROM doc_updates WHERE doc_id = $1")
            .bind(self.doc)
            .fetch_one(&self.pool)
            .await?;
        let done = sqlx::query(OLD_SNAPSHOT_UPSERT_SQL)
            .bind(self.doc)
            .bind(seq)
            .bind(state)
            .bind(vector)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// Every stamped snapshot of the page equals the log replayed to its rev.
async fn assert_stamped_snapshots_replay(pool: &PgPool, doc: Uuid) {
    let stamped: Vec<(i64, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT snapshot_rev, snapshot_seq, state_bytes FROM doc_snapshots
          WHERE doc_id = $1 AND snapshot_rev IS NOT NULL ORDER BY snapshot_rev",
    )
    .bind(doc)
    .fetch_all(pool)
    .await
    .unwrap();
    for (rev, seq, state) in &stamped {
        assert_same_doc(
            &doc_of(&[state]),
            &replay(pool, doc, *rev).await,
            &format!("the stamped snapshot at rev {rev} (seq {seq})"),
        );
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Two 0.9.10 replicas (two editors on A, one on B) and a 0.9.9 pod save
/// one page at once. Every row gets a rev, gapless and in commit order; no
/// write fails (no deadlock between the old and new save paths); both
/// rooms, once caught up, hold exactly the log replayed; every stamped
/// snapshot is the log replayed to its rev, whatever the 0.9.9 pod upserted
/// at its seq; and a third replica booting the page afterwards gets the
/// whole log.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn old_and_new_replicas_share_one_gapless_log() {
    let db = knot_test_support::fresh_db().await;
    let (doc, users) = page_and_users(&db.pool, 4).await;
    let bus = Arc::new(MemBus::new());
    let pool_a = pool_of(&db.url).await;
    let pool_b = pool_of(&db.url).await;
    let (rooms_a, store_a) = replica(&pool_a, &bus);
    let (rooms_b, store_b) = replica(&pool_b, &bus);
    let a = rooms_a
        .acquire(doc)
        .await
        .expect("replica A opens the page");
    let b = rooms_b
        .acquire(doc)
        .await
        .expect("replica B opens the page");

    let mut a1 = Client::join_as(&a, 1, Some(users[0])).await;
    let mut a2 = Client::join_as(&a, 2, Some(users[1])).await;
    let mut b1 = Client::join_as(&b, 3, Some(users[2])).await;

    // Who types each of the edits, from one seed.
    let mut shares = [0usize; 3];
    let mut rng = Rng::new(1);
    for _ in 0..EDITS {
        shares[rng.below(3)] += 1;
    }
    let old = OldPod::new(pool_of(&db.url).await, doc);
    let old = tokio::spawn(old.run(bus.clone(), users));
    tokio::join!(
        a1.type_into(&a, shares[0], 11),
        a2.type_into(&a, shares[1], 12),
        b1.type_into(&b, shares[2], 13),
    );
    let old = old.await.expect("the 0.9.9 pod's task");

    quiesce(&db.pool, doc).await;
    let count = row_count(&db.pool, doc).await;
    let revs: Vec<Option<i64>> =
        sqlx::query_scalar("SELECT rev FROM doc_updates WHERE doc_id = $1 ORDER BY rev")
            .bind(doc)
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        revs,
        (1..=count).map(Some).collect::<Vec<_>>(),
        "revs are not exactly 1..={count}"
    );

    for (what, errors) in [
        ("replica A", store_a.errors()),
        ("replica B", store_b.errors()),
        ("the 0.9.9 pod", old.errors.clone()),
    ] {
        let deadlocks: Vec<&String> = errors.iter().filter(|e| e.contains("40P01")).collect();
        assert!(deadlocks.is_empty(), "{what} deadlocked: {deadlocks:#?}");
        assert!(errors.is_empty(), "{what} saw errors: {errors:#?}");
    }

    let all = replay(&db.pool, doc, i64::MAX).await;
    assert_same_doc(&caught_up_doc(&a, "replica A").await, &all, "replica A");
    assert_same_doc(&caught_up_doc(&b, "replica B").await, &all, "replica B");
    // How many stamped snapshots the rooms wrote while everyone typed
    // depends on timing (a room snapshots only when nothing is in flight),
    // so this checks whichever exist; the step below writes one for certain.
    assert_stamped_snapshots_replay(&db.pool, doc).await;

    // Whether the 0.9.9 pod's upserts above met a stamped snapshot depends
    // on timing, so meet one for certain: one more edit on A, whose room
    // snapshots it as it ends (the page's newest row, so its highest seq),
    // then the 0.9.9 pod's upsert at that seq.
    a1.sync();
    let update = a1.edit(&mut Rng::new(14));
    a1.send(&a, update).await;
    quiesce(&db.pool, doc).await;
    a.tx.send(Event::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), a.tx.closed())
        .await
        .expect("room A did not end");
    let at_newest: Option<i64> = sqlx::query_scalar(
        "SELECT s.snapshot_rev FROM doc_snapshots s
          WHERE s.doc_id = $1
            AND s.snapshot_seq = (SELECT max(seq) FROM doc_updates WHERE doc_id = $1)",
    )
    .bind(doc)
    .fetch_optional(&db.pool)
    .await
    .unwrap()
    .flatten();
    assert_eq!(
        at_newest,
        Some(count + 1),
        "room A's last snapshot is stamped at the newest row"
    );
    assert_eq!(
        old.snapshot().await.unwrap(),
        0,
        "the 0.9.9 upsert changed the stamped snapshot"
    );
    assert_stamped_snapshots_replay(&db.pool, doc).await;

    let all = replay(&db.pool, doc, i64::MAX).await;
    assert_same_doc(
        &caught_up_doc(&b, "replica B").await,
        &all,
        "replica B, again",
    );
    let pool_c = pool_of(&db.url).await;
    let (rooms_c, store_c) = replica(&pool_c, &bus);
    let c = rooms_c
        .acquire(doc)
        .await
        .expect("replica C opens the page");
    assert_same_doc(&room_doc(&c).await, &all, "replica C's boot");

    for (what, errors) in [
        ("replica A", store_a.errors()),
        ("replica B", store_b.errors()),
        ("replica C", store_c.errors()),
    ] {
        assert!(errors.is_empty(), "{what} saw errors: {errors:#?}");
    }
    for rooms in [&rooms_a, &rooms_b, &rooms_c] {
        assert_eq!(rooms.drain_all(Duration::from_secs(30)).await, 0);
    }
}
