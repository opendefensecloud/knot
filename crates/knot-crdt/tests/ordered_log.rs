//! The ordered log against Postgres (page history M1.2): a room boots by
//! rev, persists and fans out the effective update, catches up by rev, and
//! writes a snapshot only when it holds nothing beyond the page's
//! pre-upgrade base and the log up to the snapshot's rev. Failed flushes
//! (before the write, or after a COMMIT whose acknowledgement was lost) lose
//! nothing, and pages last saved before the upgrade boot complete.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/interleave.rs"]
mod interleave;
#[path = "common/log_fixtures.rs"]
mod log_fixtures;

use fixtures::{Rng, cross_client_parked, overlapping_skips, para};
use interleave::held;
use knot_crdt::nesting::MAX_NESTING;
use knot_crdt::pending::{self, holds_any};
use knot_crdt::{
    Boot, BootMode, Bus, DocHandle, Engine, Event, MemBus, Room, RoomHandle, SnapshotPolicy,
    YrsEngine,
};
use knot_storage::{
    DocUpdate, HistoryHead, NewUpdate, Persisted, PgSnapshotStore, PgUpdatesStore, RowsAfter,
    SnapshotStamp, SnapshotStore, UpdateSource, UpdatesStore, UpdatesStoreError,
};
use log_fixtures::{
    Client, assert_same_doc, difference, doc_of, pending, replay, row_count, rows, text,
};
use sqlx::PgPool;
use tokio::sync::oneshot;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, ReadTxn, StateVector, Text, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim,
};

/// The migration that adds the log columns: a database migrated up to it
/// holds rows and snapshots as 0.9.9 wrote them (no rev, no heads row).
const HISTORY_LOG_COLUMNS: i64 = 20261107120000;

/// The schema of 0.9.13, the release before `pending_revs`.
const V0_9_13_SCHEMA: i64 = 20261113120000;

/// A snapshot write of every release from 0.9.10 to 0.9.13, stamped with
/// the rev it holds the log up to and the content epoch it was taken at. It
/// names no `pending_revs` (`knot-storage/tests/migrations_apply.rs`).
const STAMPED_SNAPSHOT_INSERT_SQL: &str = "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, $2, $3, $4, $5, $6)";

/// The shape of the stamped snapshot upsert of every release from 0.9.10 to
/// 0.9.13 (`PgSnapshotStore::insert_stamped`), which names no `pending_revs`.
const STAMPED_SNAPSHOT_UPSERT_SQL: &str = "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (doc_id, snapshot_seq) DO UPDATE
             SET state_bytes = EXCLUDED.state_bytes,
                 state_vector = EXCLUDED.state_vector,
                 created_at = now(),
                 snapshot_rev = EXCLUDED.snapshot_rev,
                 epoch = EXCLUDED.epoch";

/// 0.9.10's to 0.9.13's snapshot GC (`gc(doc, 5, 30)`), which keeps no
/// trusted row (`knot-storage/tests/snapshots.rs`).
const V0_9_13_GC_SQL: &str = "WITH recent AS (
                 SELECT snapshot_seq FROM doc_snapshots
                 WHERE doc_id = $1 ORDER BY snapshot_seq DESC LIMIT $2
             ),
             per_day AS (
                 SELECT DISTINCT ON (date_trunc('day', created_at)) snapshot_seq
                 FROM doc_snapshots
                 WHERE doc_id = $1 AND created_at >= now() - ($3 || ' days')::interval
                 ORDER BY date_trunc('day', created_at), created_at DESC
             )
             DELETE FROM doc_snapshots
             WHERE doc_id = $1
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM recent)
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM per_day)";

// ---------------------------------------------------------------------------
// The database
// ---------------------------------------------------------------------------

/// The embedded migrations up to and including `last`.
fn migrator_up_to(last: i64) -> sqlx::migrate::Migrator {
    let full = sqlx::migrate!("../../migrations");
    assert!(
        full.migrations.iter().any(|m| m.version == last),
        "migration {last} not embedded (cargo clean -p knot-crdt?)"
    );
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version <= last)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
}

/// A page in a workspace of its own.
async fn page(pool: &PgPool) -> Uuid {
    let ws: Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
            .fetch_one(pool)
            .await
            .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'A') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A fully migrated database with one page.
async fn new_page() -> (PgPool, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let doc = page(&pool).await;
    (pool, doc)
}

/// A database migrated up to the log columns (as 0.9.9 left it), with one
/// page; the test writes its legacy rows, then calls [`finish_migrations`].
async fn legacy_page() -> (PgPool, Uuid) {
    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap();
    migrator_up_to(HISTORY_LOG_COLUMNS)
        .run(&pool)
        .await
        .unwrap();
    let doc = page(&pool).await;
    (pool, doc)
}

/// A database migrated up to 0.9.13's schema, with one page; the test
/// writes what 0.9.13 pods wrote, then calls [`finish_migrations`].
async fn v0_9_13_page() -> (PgPool, Uuid) {
    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap();
    migrator_up_to(V0_9_13_SCHEMA).run(&pool).await.unwrap();
    let doc = page(&pool).await;
    (pool, doc)
}

async fn finish_migrations(pool: &PgPool) {
    sqlx::migrate!("../../migrations").run(pool).await.unwrap();
}

/// A row as a 0.9.9 pod wrote it (no rev).
async fn legacy_row(pool: &PgPool, doc: Uuid, bytes: &[u8]) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO doc_updates (doc_id, update_bytes) VALUES ($1, $2) RETURNING seq",
    )
    .bind(doc)
    .bind(bytes)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A snapshot as a 0.9.9 room wrote it (no rev, no epoch).
async fn legacy_snapshot(pool: &PgPool, doc: Uuid, seq: i64, state: &DocHandle) {
    let txn = state.inner().transact();
    sqlx::query(
        "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(doc)
    .bind(seq)
    .bind(txn.encode_state_as_update_v1(&StateVector::default()))
    .bind(txn.state_vector().encode_v1())
    .execute(pool)
    .await
    .unwrap();
}

/// The page's stamped snapshots: rev, epoch and state, by rev.
async fn stamped_snapshots(pool: &PgPool, doc: Uuid) -> Vec<(i64, Option<i32>, Vec<u8>)> {
    sqlx::query_as(
        "SELECT snapshot_rev, epoch, state_bytes FROM doc_snapshots
         WHERE doc_id = $1 AND snapshot_rev IS NOT NULL ORDER BY snapshot_rev",
    )
    .bind(doc)
    .fetch_all(pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// The room and its clients
// ---------------------------------------------------------------------------

/// Snapshots only when the room ends.
const LAZY: SnapshotPolicy = SnapshotPolicy {
    every_n: 1000,
    idle: Duration::from_secs(3600),
};

const EVERY_ROW: SnapshotPolicy = SnapshotPolicy {
    every_n: 1,
    idle: Duration::from_secs(3600),
};

async fn open_room(
    pool: &PgPool,
    doc: Uuid,
    updates: Arc<dyn UpdatesStore>,
    policy: SnapshotPolicy,
) -> RoomHandle {
    let bus = Arc::new(MemBus::new());
    let sub = bus.subscribe(doc).await.unwrap();
    Room::spawn(
        doc,
        Arc::new(YrsEngine),
        bus,
        sub,
        updates,
        Arc::new(PgSnapshotStore::new(pool.clone())),
        policy,
        None,
    )
    .await
    .expect("the room boots")
}

async fn shut_down(h: &RoomHandle) {
    let _ = h.tx.send(Event::Shutdown).await;
    tokio::time::timeout(Duration::from_secs(30), h.tx.closed())
        .await
        .expect("the room did not end");
    let mut done = h.writer_done.clone();
    tokio::time::timeout(Duration::from_secs(60), done.wait())
        .await
        .expect("the writer did not drain");
}

/// The room's doc, copied, and its seq.
async fn room_doc(h: &RoomHandle) -> (DocHandle, i64) {
    let (tx, rx) = oneshot::channel();
    h.tx.send(Event::ExportState(tx)).await.unwrap();
    let (bytes, seq) = rx.await.unwrap().unwrap();
    (doc_of(&[&bytes]), seq)
}

/// Waits until the log replayed equals the room's doc and the log has
/// stopped growing for a second; returns the room's doc.
async fn settle(pool: &PgPool, doc: Uuid, h: &RoomHandle) -> DocHandle {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let mut stable_since: Option<(i64, tokio::time::Instant)> = None;
    loop {
        let count = row_count(pool, doc).await;
        let (room, _) = room_doc(h).await;
        let same = difference(&room, &replay(pool, doc, i64::MAX).await).is_none();
        match stable_since {
            Some((n, since)) if n == count && same => {
                if since.elapsed() >= Duration::from_secs(1) {
                    return room;
                }
            }
            _ => stable_since = Some((count, tokio::time::Instant::now())),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the log never caught up with the room ({count} rows)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits until the page holds `n` rows.
async fn until_rows(pool: &PgPool, doc: Uuid, n: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while row_count(pool, doc).await < n {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the page never held {n} rows"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// A store whose appends fail
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The first K appends fail before touching the database.
    FailBefore,
    /// The first K appends commit, then fail (a lost COMMIT
    /// acknowledgement), so the writer writes their rows again.
    LostAck,
}

struct Flaky {
    inner: PgUpdatesStore,
    mode: Mode,
    left: AtomicUsize,
    /// Whether the latest append returned an error.
    failing: AtomicBool,
}

impl Flaky {
    fn new(pool: &PgPool, mode: Mode, k: usize) -> Arc<Self> {
        Arc::new(Flaky {
            inner: PgUpdatesStore::new(pool.clone()),
            mode,
            left: AtomicUsize::new(k),
            failing: AtomicBool::new(false),
        })
    }

    /// Every failure is used up and an append has succeeded since the last
    /// one, so the writer is out of its retry backoff.
    fn recovered(&self) -> bool {
        self.left.load(Ordering::SeqCst) == 0 && !self.failing.load(Ordering::SeqCst)
    }

    /// Waits until [`Flaky::recovered`]. A lost acknowledgement leaves its
    /// rows committed, so the log can equal the room's doc (and look
    /// settled) while the writer still waits to retry them.
    async fn until_recovered(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !self.recovered() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the writer never got past its failed appends"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn take_failure(&self) -> bool {
        self.left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
    }

    async fn append_or_fail(
        &self,
        doc_id: Uuid,
        epoch: i32,
        rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError> {
        match self.mode {
            Mode::FailBefore => {
                if self.take_failure() {
                    return Err(UpdatesStoreError::Sqlx(sqlx::Error::PoolTimedOut));
                }
                self.inner.append(doc_id, epoch, rows).await
            }
            Mode::LostAck => {
                let persisted = self.inner.append(doc_id, epoch, rows).await?;
                if self.take_failure() {
                    return Err(UpdatesStoreError::Sqlx(sqlx::Error::Io(
                        std::io::Error::new(std::io::ErrorKind::ConnectionReset, "lost ack"),
                    )));
                }
                Ok(persisted)
            }
        }
    }
}

#[async_trait::async_trait]
impl UpdatesStore for Flaky {
    async fn append(
        &self,
        doc_id: Uuid,
        epoch: i32,
        rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError> {
        let result = self.append_or_fail(doc_id, epoch, rows).await;
        self.failing.store(result.is_err(), Ordering::SeqCst);
        result
    }
    async fn rows_after(
        &self,
        doc_id: Uuid,
        from: RowsAfter,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        self.inner.rows_after(doc_id, from).await
    }
    async fn head(&self, doc_id: Uuid) -> Result<HistoryHead, UpdatesStoreError> {
        self.inner.head(doc_id).await
    }
    async fn ensure_legacy_numbered(&self, doc_id: Uuid) -> Result<u64, UpdatesStoreError> {
        self.inner.ensure_legacy_numbered(doc_id).await
    }
    async fn since(&self, doc_id: Uuid, after: i64) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        self.inner.since(doc_id, after).await
    }
    async fn max_seq(&self, doc_id: Uuid) -> Result<i64, UpdatesStoreError> {
        self.inner.max_seq(doc_id).await
    }
}

async fn assert_gapless(pool: &PgPool, doc: Uuid) {
    let revs: Vec<i64> =
        sqlx::query_scalar("SELECT rev FROM doc_updates WHERE doc_id = $1 ORDER BY rev")
            .bind(doc)
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(revs, (1..=revs.len() as i64).collect::<Vec<_>>());
}

// ---------------------------------------------------------------------------
// Failed flushes
// ---------------------------------------------------------------------------

/// Flushes that fail before writing, or after a COMMIT whose acknowledgement
/// was lost, are retried in order: the log replayed equals the room, revs
/// stay gapless, and a row written twice integrates nothing the second time.
#[tokio::test(flavor = "multi_thread")]
async fn flush_failures_lose_nothing() {
    for mode in [Mode::FailBefore, Mode::LostAck] {
        let (pool, doc) = new_page().await;
        let h = open_room(&pool, doc, Flaky::new(&pool, mode, 6), LAZY).await;
        let mut a = Client::join(&h, 1).await;
        let mut b = Client::join(&h, 2).await;
        let mut rng = Rng::new(mode as u64 + 1);
        for _ in 0..40 {
            let c = if rng.chance(50) { &mut a } else { &mut b };
            c.sync();
            let update = c.edit(&mut rng);
            c.send(&h, update).await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        let room = settle(&pool, doc, &h).await;
        assert_same_doc(&room, &replay(&pool, doc, i64::MAX).await, "{mode:?}");
        assert_gapless(&pool, doc).await;
        assert!(!h.shutdown.is_cancelled(), "{mode:?}: the room was evicted");

        let log = rows(&pool, doc).await;
        let mut seen: Vec<&[u8]> = Vec::new();
        let mut duplicates = 0;
        for (i, row) in log.iter().enumerate() {
            if seen.contains(&row.update_bytes.as_slice()) {
                duplicates += 1;
                let before: Vec<&[u8]> =
                    log[..i].iter().map(|r| r.update_bytes.as_slice()).collect();
                let replayed = doc_of(&before);
                let m = YrsEngine
                    .apply_update_measuring(&replayed, &row.update_bytes, MAX_NESTING)
                    .unwrap();
                assert!(
                    !m.changed,
                    "rev {} written twice integrates something",
                    row.rev.unwrap()
                );
            }
            seen.push(&row.update_bytes);
        }
        match mode {
            Mode::FailBefore => assert_eq!(duplicates, 0),
            Mode::LostAck => assert!(duplicates > 0, "no row was written twice"),
        }
        shut_down(&h).await;
    }
}

/// Every stamped snapshot equals the log replayed to its rev, around failed
/// flushes, held-back content and a reconnect resend, and none holds
/// pending content; a reboot equals the whole log replayed.
#[tokio::test(flavor = "multi_thread")]
async fn every_snapshot_equals_the_log_replayed_to_its_rev() {
    for mode in [Mode::FailBefore, Mode::LostAck] {
        let (pool, doc) = new_page().await;
        let flaky = Flaky::new(&pool, mode, 4);
        let policy = SnapshotPolicy {
            every_n: 5,
            idle: Duration::from_secs(3600),
        };
        let h = open_room(&pool, doc, flaky.clone(), policy).await;
        let mut a = Client::join(&h, 1).await;
        let mut b = Client::join(&h, 2).await;
        let mut rng = Rng::new(mode as u64 + 10);
        let mut edits = 0;
        let edit = async |rng: &mut Rng, a: &mut Client, b: &mut Client| {
            let c = if rng.chance(50) { a } else { b };
            c.sync();
            let update = c.edit(rng);
            c.send(&h, update).await;
        };

        // Failed flushes first. The writer must be past them before the
        // held-back update below: otherwise its retry takes the first
        // spaced edits into its batch, and no snapshot falls between them.
        while edits < 20 {
            edit(&mut rng, &mut a, &mut b).await;
            edits += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        flaky.until_recovered().await;
        settle(&pool, doc, &h).await;
        assert_eq!(flaky.left.load(Ordering::SeqCst), 0, "{mode:?}");

        // A held-back update: u2 arrives before u1, which it builds on, and
        // stays pending (persisted raw) while the room writes snapshots.
        let (u1, u2) = {
            let held = Doc::with_client_id(99);
            let frag = held.get_or_insert_xml_fragment("default");
            let own = Arc::new(Mutex::new(Vec::new()));
            let _sub = {
                let own = own.clone();
                held.observe_update_v1(move |_, e| own.lock().unwrap().push(e.update.clone()))
                    .unwrap()
            };
            let text = {
                let mut txn = held.transact_mut();
                let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(&mut txn, XmlTextPrelim::new("held"))
            };
            text.insert(&mut held.transact_mut(), 0, "back ");
            let own = own.lock().unwrap().clone();
            (own[0].clone(), own[1].clone())
        };
        a.send(&h, u2).await;
        // One row per flush, so the room writes a snapshot after its 5th.
        for _ in 0..7 {
            edit(&mut rng, &mut a, &mut b).await;
            edits += 1;
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        a.send(&h, u1).await;
        edits += 2;

        // A reconnect: two edits made offline, then the whole state resent.
        a.sync();
        let _ = a.edit(&mut rng);
        let _ = a.edit(&mut rng);
        let resend = a
            .doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        a.send(&h, resend).await;
        edits += 1;

        while edits < 60 {
            edit(&mut rng, &mut a, &mut b).await;
            edits += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        settle(&pool, doc, &h).await;

        let snapshots = stamped_snapshots(&pool, doc).await;
        // One before the held-back update and at least one after it
        // drained: until the room records the rows that park its pending
        // content, it writes none while u2 waits.
        assert!(
            snapshots.len() >= 2,
            "{mode:?}: {} snapshots",
            snapshots.len()
        );
        let mut with_pending = 0;
        for (rev, _, state) in &snapshots {
            let snapshot = doc_of(&[state]);
            if pending(&snapshot).0.is_some() {
                with_pending += 1;
            }
            assert_same_doc(
                &snapshot,
                &replay(&pool, doc, *rev).await,
                &format!("{mode:?}: snapshot at rev {rev}"),
            );
        }
        // Until the room records the rows that park its pending content,
        // it writes no snapshot while content is pending.
        assert_eq!(
            with_pending, 0,
            "{mode:?}: a snapshot was written while content was pending"
        );
        assert!(!h.shutdown.is_cancelled(), "{mode:?}: the room was evicted");

        shut_down(&h).await;
        let h = open_room(
            &pool,
            doc,
            Arc::new(PgUpdatesStore::new(pool.clone())),
            LAZY,
        )
        .await;
        let (rebooted, _) = room_doc(&h).await;
        assert_same_doc(
            &rebooted,
            &replay(&pool, doc, i64::MAX).await,
            &format!("{mode:?}: the rebooted room"),
        );
        assert_eq!(pending(&rebooted), (None, None), "{mode:?}");
        shut_down(&h).await;
    }
}

// ---------------------------------------------------------------------------
// Epochs
// ---------------------------------------------------------------------------

/// A page whose content epoch is past 0 (after an erase): the room boots
/// with it, appends at it and stamps its snapshots with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_nonzero_content_epoch_reaches_rows_and_snapshots() {
    let (pool, doc) = new_page().await;
    sqlx::query("INSERT INTO doc_history_heads (doc_id, content_epoch) VALUES ($1, 2)")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap();
    let updates = Arc::new(PgUpdatesStore::new(pool.clone()));
    let booted = knot_crdt::boot(
        &YrsEngine,
        updates.as_ref(),
        &PgSnapshotStore::new(pool.clone()),
        doc,
        BootMode::Room,
    )
    .await
    .unwrap();
    assert_eq!(booted.content_epoch, 2);

    let h = open_room(&pool, doc, updates, EVERY_ROW).await;
    let c = Client::join(&h, 1).await;
    for i in 0..3 {
        c.send(&h, para(10 + i, "edit")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    until_rows(&pool, doc, 3).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while stamped_snapshots(&pool, doc).await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no snapshot");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let epochs: Vec<Option<i32>> =
        sqlx::query_scalar("SELECT epoch FROM doc_updates WHERE doc_id = $1")
            .bind(doc)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(epochs, vec![Some(2); 3]);
    for (_, epoch, _) in stamped_snapshots(&pool, doc).await {
        assert_eq!(epoch, Some(2));
    }
    assert!(!h.shutdown.is_cancelled(), "the room was evicted");
    shut_down(&h).await;
}

/// An epoch bump (an erase or restore elsewhere) evicts the room on its
/// next flush, which writes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn epoch_bump_evicts_the_room_on_its_next_flush() {
    let (pool, doc) = new_page().await;
    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        LAZY,
    )
    .await;
    let c = Client::join(&h, 1).await;
    c.send(&h, para(10, "first")).await;
    until_rows(&pool, doc, 1).await;

    let bumped = sqlx::query("UPDATE doc_history_heads SET content_epoch = 1 WHERE doc_id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(bumped, 1);
    c.send(&h, para(11, "second")).await;
    tokio::time::timeout(Duration::from_secs(30), h.shutdown.cancelled())
        .await
        .expect("the room was not evicted");
    let mut done = h.writer_done.clone();
    tokio::time::timeout(Duration::from_secs(30), done.wait())
        .await
        .expect("the writer did not end");
    assert_eq!(
        row_count(&pool, doc).await,
        1,
        "a row was written past the bump"
    );
}

// ---------------------------------------------------------------------------
// Pages last saved before the upgrade
// ---------------------------------------------------------------------------

/// Five legacy rows and a legacy snapshot at the third: the room opens with
/// all five, numbers them 1..=5 in seq order once, and continues at 6.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_page_boots_complete_and_continues_numbering() {
    let (pool, doc) = legacy_page().await;
    let updates: Vec<Vec<u8>> = (0..5).map(|i| para(20 + i, &format!("p{i}"))).collect();
    let mut seqs = Vec::new();
    for u in &updates {
        seqs.push(legacy_row(&pool, doc, u).await);
    }
    let first_three: Vec<&[u8]> = updates[..3].iter().map(Vec::as_slice).collect();
    legacy_snapshot(&pool, doc, seqs[2], &doc_of(&first_three)).await;
    finish_migrations(&pool).await;

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        EVERY_ROW,
    )
    .await;
    let all: Vec<&[u8]> = updates.iter().map(Vec::as_slice).collect();
    assert_eq!(text(&room_doc(&h).await.0), text(&doc_of(&all)));
    let numbered: Vec<(i64, Option<i64>)> =
        sqlx::query_as("SELECT seq, rev FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
            .bind(doc)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        numbered,
        seqs.iter()
            .zip(1..)
            .map(|(&s, r)| (s, Some(r)))
            .collect::<Vec<_>>()
    );

    let c = Client::join(&h, 1).await;
    c.send(&h, para(30, "after")).await;
    until_rows(&pool, doc, 6).await;
    let newest: Option<i64> = sqlx::query_scalar(
        "SELECT rev FROM doc_updates WHERE doc_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(newest, Some(6));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !stamped_snapshots(&pool, doc)
        .await
        .iter()
        .any(|(rev, _, _)| *rev == 6)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no snapshot at rev 6"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    shut_down(&h).await;
}

/// A 0.9.x room skipped a peer row whose seq was below one it had already
/// applied, and snapshotted without it: the room boots with that row, and
/// its first stamped snapshot equals the log replayed to its rev.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_snapshot_missing_a_lower_seq_row_boots_with_it() {
    let (pool, doc) = legacy_page().await;
    let (a, b, c) = (para(21, "alpha"), para(22, "bravo"), para(23, "charlie"));
    legacy_row(&pool, doc, &a).await;
    legacy_row(&pool, doc, &b).await;
    let seq_c = legacy_row(&pool, doc, &c).await;
    legacy_snapshot(&pool, doc, seq_c, &doc_of(&[&a, &c])).await;
    finish_migrations(&pool).await;

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        EVERY_ROW,
    )
    .await;
    let page = text(&room_doc(&h).await.0);
    for word in ["alpha", "bravo", "charlie"] {
        assert!(page.contains(word), "{word} is missing: {page}");
    }
    let client = Client::join(&h, 1).await;
    client.send(&h, para(30, "delta")).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let (rev, state) = loop {
        if let Some((rev, _, state)) = stamped_snapshots(&pool, doc).await.into_iter().next() {
            break (rev, state);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no stamped snapshot"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(rev, 4);
    assert_same_doc(
        &doc_of(&[&state]),
        &replay(&pool, doc, rev).await,
        "the first stamped snapshot",
    );
    shut_down(&h).await;
}

/// A read-only load (Markdown refresh, export-check) of a legacy page reads
/// every row and writes nothing: the rows stay unnumbered and untouched.
#[tokio::test(flavor = "multi_thread")]
async fn read_only_boot_never_numbers_legacy_rows() {
    let (pool, doc) = legacy_page().await;
    let updates: Vec<Vec<u8>> = (0..5).map(|i| para(20 + i, &format!("p{i}"))).collect();
    let mut seqs = Vec::new();
    for u in &updates {
        seqs.push(legacy_row(&pool, doc, u).await);
    }
    let first_three: Vec<&[u8]> = updates[..3].iter().map(Vec::as_slice).collect();
    legacy_snapshot(&pool, doc, seqs[2], &doc_of(&first_three)).await;
    finish_migrations(&pool).await;

    let versions = async || -> Vec<(i64, String)> {
        sqlx::query_as("SELECT seq, xmin::text FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
            .bind(doc)
            .fetch_all(&pool)
            .await
            .unwrap()
    };
    let before = versions().await;
    let booted = knot_crdt::boot(
        &YrsEngine,
        &PgUpdatesStore::new(pool.clone()),
        &PgSnapshotStore::new(pool.clone()),
        doc,
        BootMode::ReadOnly,
    )
    .await
    .unwrap();
    let all: Vec<&[u8]> = updates.iter().map(Vec::as_slice).collect();
    assert_eq!(text(&booted.doc), text(&doc_of(&all)));
    assert_eq!(booted.last_seq, seqs[4]);
    let unnumbered: i64 = sqlx::query_scalar(
        "SELECT count(*) FILTER (WHERE rev IS NULL) FROM doc_updates WHERE doc_id = $1",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unnumbered, 5);
    assert_eq!(versions().await, before, "a row was written");
}

/// A legacy snapshot holding content no row carries (the pre-M1.1 GC pruned
/// its rows): that content is the base of every stamped snapshot, which
/// equals the legacy snapshot united with the log replayed to its rev.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_snapshot_content_without_rows_is_the_base_of_stamped_snapshots() {
    let (pool, doc) = legacy_page().await;
    let p0 = para(20, "pruned");
    let p1 = para(21, "survived");
    // The seq the snapshot was taken at, its rows since pruned.
    let snapshot_seq: i64 =
        sqlx::query_scalar("SELECT nextval(pg_get_serial_sequence('doc_updates', 'seq'))")
            .fetch_one(&pool)
            .await
            .unwrap();
    let legacy = doc_of(&[&p0]);
    legacy_snapshot(&pool, doc, snapshot_seq, &legacy).await;
    let seq_p1 = legacy_row(&pool, doc, &p1).await;
    assert!(seq_p1 > snapshot_seq);
    finish_migrations(&pool).await;
    let legacy_rows: i64 =
        sqlx::query_scalar("SELECT legacy_rows FROM doc_history_heads WHERE doc_id = $1")
            .bind(doc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy_rows, 1);

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        EVERY_ROW,
    )
    .await;
    let c = Client::join(&h, 1).await;
    c.send(&h, para(30, "typed")).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let (rev, state) = loop {
        if let Some((rev, _, state)) = stamped_snapshots(&pool, doc).await.into_iter().next() {
            break (rev, state);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no stamped snapshot"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let snapshot = doc_of(&[&state]);
    let page = text(&snapshot);
    for word in ["pruned", "survived", "typed"] {
        assert!(page.contains(word), "{word} is missing: {page}");
    }
    let replayed = replay(&pool, doc, rev).await;
    assert!(!text(&replayed).contains("pruned"));
    let base_and_log = doc_of(&[&legacy
        .inner()
        .transact()
        .encode_state_as_update_v1(&StateVector::default())]);
    for row in rows(&pool, doc).await {
        if row.rev.unwrap() <= rev {
            YrsEngine
                .apply_update(&base_and_log, &row.update_bytes)
                .unwrap();
        }
    }
    assert_same_doc(&snapshot, &base_and_log, "legacy snapshot ∪ replay(R)");
    shut_down(&h).await;
}

// ---------------------------------------------------------------------------
// Reconnects
// ---------------------------------------------------------------------------

/// A reconnecting client resends its whole state (its answer to the
/// server's SyncStep1) with one new paragraph: only the paragraph is
/// persisted, not the content and delete set the room already holds, and
/// peers get exactly the persisted bytes.
#[tokio::test(flavor = "multi_thread")]
async fn reconnect_resend_persists_only_new_content() {
    let (pool, doc) = new_page().await;
    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        LAZY,
    )
    .await;
    let a = Client::join(&h, 1).await;
    let mut b = Client::join(&h, 2).await;

    // A types a paragraph and deletes part of it.
    let text_ref = {
        let mut txn = a.doc.transact_mut();
        let p = a
            .frag
            .push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("firstx"))
    };
    text_ref.remove_range(&mut a.doc.transact_mut(), 5, 1);
    let first = a
        .doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    a.take();
    a.send(&h, first).await;
    until_rows(&pool, doc, 1).await;

    // Offline: one new paragraph; then the reconnect resend.
    {
        let mut txn = a.doc.transact_mut();
        let p = a
            .frag
            .push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("second"));
    }
    let new_part = a.take().pop().unwrap();
    let resend = a
        .doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    a.send(&h, resend.clone()).await;
    until_rows(&pool, doc, 2).await;

    let row = rows(&pool, doc).await.pop().unwrap();
    let persisted = Update::decode_v1(&row.update_bytes).unwrap();
    let new_part = Update::decode_v1(&new_part).unwrap();
    assert_eq!(
        persisted.insertions(true),
        new_part.insertions(true),
        "the resend persisted more than the new paragraph"
    );
    assert!(
        persisted.delete_set().is_empty(),
        "the resend persisted the delete set again"
    );
    assert!(row.update_bytes.len() < resend.len());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < 2 {
        got.extend(b.sync());
        assert!(
            tokio::time::Instant::now() < deadline,
            "B got {} frames",
            got.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(got.len(), 2);
    assert_eq!(got[1], row.update_bytes, "B got other bytes than the row");
    assert_same_doc(
        &room_doc(&h).await.0,
        &replay(&pool, doc, i64::MAX).await,
        "the log replayed",
    );
    shut_down(&h).await;
}

// ---------------------------------------------------------------------------
// Pending content across reboots and the rolling upgrade
// ---------------------------------------------------------------------------

/// Appends `rows` as edits at epoch 0, one flush each; returns where each
/// landed.
async fn append_rows(pool: &PgPool, doc: Uuid, rows: &[&[u8]]) -> Vec<Persisted> {
    let store = PgUpdatesStore::new(pool.clone());
    let mut out = Vec::new();
    for bytes in rows {
        let row = NewUpdate {
            by_user_id: None,
            bytes: bytes.to_vec(),
            source: UpdateSource::Edit,
            meta: None,
        };
        out.extend(store.append(doc, 0, &[row]).await.unwrap());
    }
    out
}

/// A stamped snapshot without `pending_revs`, as 0.9.13 pods wrote it, at
/// the seq of the row at its rev, through `sql` (the insert or the upsert).
async fn older_pods_snapshot(
    pool: &PgPool,
    doc: Uuid,
    sql: &'static str,
    at: Persisted,
    state: &[u8],
) {
    let sv = doc_of(&[state])
        .inner()
        .transact()
        .state_vector()
        .encode_v1();
    sqlx::query(sql)
        .bind(doc)
        .bind(at.seq)
        .bind(state)
        .bind(sv)
        .bind(at.rev)
        .bind(0i32)
        .execute(pool)
        .await
        .unwrap();
}

/// yrs's own full state of a doc that applied `updates`: what 0.9.13 rooms
/// wrote, shifted when their pending update holds overlapping Skips.
fn shifted_state(updates: &[&[u8]]) -> Vec<u8> {
    doc_of(updates)
        .inner()
        .transact()
        .encode_state_as_update_v1(&StateVector::default())
}

fn pending_free(d: &DocHandle) -> Vec<u8> {
    pending::encode_without_pending(&d.inner().transact(), &StateVector::default())
}

/// A room-mode boot of the page, which writes nothing: the pending state a
/// room starts with.
async fn direct_boot(pool: &PgPool, doc: Uuid) -> Boot {
    knot_crdt::boot(
        &YrsEngine,
        &PgUpdatesStore::new(pool.clone()),
        &PgSnapshotStore::new(pool.clone()),
        doc,
        BootMode::Room,
    )
    .await
    .unwrap()
}

fn insertions_of(updates: &[&[u8]]) -> yrs::IdSet {
    updates.iter().fold(yrs::IdSet::new(), |acc, u| {
        acc.merge(&Update::decode_v1(u).unwrap().insertions(true))
    })
}

fn within(a: &yrs::IdSet, b: &yrs::IdSet) -> bool {
    !holds_any(&a.diff(b))
}

/// A doc that applied `updates` through the room's measured apply.
fn live_of(updates: &[&[u8]]) -> DocHandle {
    let live = YrsEngine.new_doc();
    for u in updates {
        YrsEngine
            .apply_update_measuring(&live, u, MAX_NESTING)
            .unwrap();
    }
    live
}

/// An edit by client 3 inside "base" (`overlapping_skips().t0`'s text).
fn edit_inside_base(t0: &[u8]) -> Vec<u8> {
    let c = Doc::with_client_id(3);
    c.transact_mut()
        .apply_update(Update::decode_v1(t0).unwrap())
        .unwrap();
    let frag = c.get_or_insert_xml_fragment("default");
    let sv = c.transact().state_vector();
    {
        let mut txn = c.transact_mut();
        let text = fixtures::text_at(&frag, &txn, 0).unwrap();
        text.insert(&mut txn, 2, "EDIT");
    }
    c.transact().encode_diff_v1(&sv)
}

/// Waits until the room's text holds `word`.
async fn until_text(h: &RoomHandle, word: &str) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let page = text(&room_doc(h).await.0);
        if page.contains(word) {
            return page;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the room never showed {word:?}: {page}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Review Focus 4: a 0.9.13 room wrote a snapshot whose pending update held
/// overlapping Skips; boot drops its phantoms (rule 3, `legacy`) and the
/// content drains when its dependency arrives.
#[tokio::test(flavor = "multi_thread")]
async fn an_older_pods_snapshot_with_shifted_pending_content_boots_without_phantoms() {
    let o = overlapping_skips();
    let (pool, doc) = v0_9_13_page().await;
    let rows: [&[u8]; 3] = [&o.t0, &o.t2, &o.t3];
    let at = append_rows(&pool, doc, &rows).await;
    older_pods_snapshot(
        &pool,
        doc,
        STAMPED_SNAPSHOT_INSERT_SQL,
        at[2],
        &shifted_state(&rows),
    )
    .await;
    finish_migrations(&pool).await;

    let boot = direct_boot(&pool, doc).await;
    assert_eq!(boot.fallback.as_ref().map(|f| f.reason), Some("legacy"));
    assert!(
        within(&held(boot.doc.inner()), &insertions_of(&rows)),
        "a phantom"
    );

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        LAZY,
    )
    .await;
    let c = Client::join(&h, 7).await;
    c.send(&h, o.t1.clone()).await;
    c.send(&h, o.t2b.clone()).await;
    until_rows(&pool, doc, 5).await;
    shut_down(&h).await;
    let all: [&[u8]; 5] = [&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3];
    let reboot = direct_boot(&pool, doc).await;
    assert!(
        pending::claims(&reboot.doc).is_none(),
        "the content drained"
    );
    assert_eq!(text(&reboot.doc), text(&doc_of(&all)));
}

/// Review Focus 2: a 0.9.13 room open across the migration on a page whose
/// pending update holds overlapping Skips: its pre-migration snapshot (the
/// trusted one) and its flagged one both hold shifted bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_0_9_13_room_open_across_the_migration_on_a_shifted_page_boots_from_the_prefix_and_drains()
 {
    let o = overlapping_skips();
    let (pool, doc) = v0_9_13_page().await;
    let first: [&[u8]; 3] = [&o.t0, &o.t2, &o.t3];
    let at = append_rows(&pool, doc, &first).await;
    older_pods_snapshot(
        &pool,
        doc,
        STAMPED_SNAPSHOT_INSERT_SQL,
        at[2],
        &shifted_state(&first),
    )
    .await;
    finish_migrations(&pool).await;
    let edit = edit_inside_base(&o.t0);
    let four: [&[u8]; 4] = [&o.t0, &o.t2, &o.t3, &edit];
    let at4 = append_rows(&pool, doc, &[&edit]).await;
    older_pods_snapshot(
        &pool,
        doc,
        STAMPED_SNAPSHOT_UPSERT_SQL,
        at4[0],
        &shifted_state(&four),
    )
    .await;

    let boot = direct_boot(&pool, doc).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log), ("rollout", true));
    assert!(
        within(&held(boot.doc.inner()), &insertions_of(&four)),
        "a phantom"
    );
    assert!(boot.carry.is_empty());
    assert_eq!(pending::claims(&boot.doc), pending::claims(&live_of(&four)));

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        LAZY,
    )
    .await;
    let c = Client::join(&h, 7).await;
    c.send(&h, o.t1.clone()).await;
    c.send(&h, o.t2b.clone()).await;
    until_rows(&pool, doc, 6).await;
    shut_down(&h).await;
    let all: [&[u8]; 6] = [&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3, &edit];
    let reboot = direct_boot(&pool, doc).await;
    assert!(
        pending::claims(&reboot.doc).is_none(),
        "the content drained"
    );
    assert_eq!(text(&reboot.doc), text(&doc_of(&all)));
}

/// Review Focus 2: a 0.9.13 room booted from a fixed snapshot (it ignores
/// `pending_revs`), so its own snapshot lacks the parked content; boot
/// takes the last trusted snapshot and re-parks it.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_an_older_pod_wrote_after_the_migration_boots_from_the_last_trusted_one() {
    let (u1, u2) = cross_client_parked();
    let edit = para(3, "own paragraph");
    let (pool, doc) = new_page().await;
    let at = append_rows(&pool, doc, &[&u2, &edit]).await;
    let after_row_1 = doc_of(&[&u2]);
    let written = PgSnapshotStore::new(pool.clone())
        .insert_stamped(
            doc,
            SnapshotStamp {
                snapshot_seq: at[0].seq,
                snapshot_rev: Some(at[0].rev),
                epoch: Some(0),
            },
            &pending_free(&after_row_1),
            &after_row_1.inner().transact().state_vector().encode_v1(),
            Some(&[at[0].rev]),
        )
        .await
        .unwrap();
    assert!(written);
    older_pods_snapshot(&pool, doc, STAMPED_SNAPSHOT_UPSERT_SQL, at[1], &edit).await;

    let latest = PgSnapshotStore::new(pool.clone())
        .latest(doc)
        .await
        .unwrap()
        .unwrap();
    assert!(latest.older_writer);
    let boot = direct_boot(&pool, doc).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log), ("rollout", false));
    let claims = pending::claims(&boot.doc).expect("u2 pending");
    assert!(within(&insertions_of(&[&u2]), &claims.structs));

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        LAZY,
    )
    .await;
    let c = Client::join(&h, 7).await;
    c.send(&h, u1).await;
    until_text(&h, "back held").await;
    shut_down(&h).await;
}

/// Review Focus 4: a pre-0.9.9 page whose parking row the old GC deleted;
/// its legacy snapshot holds the only copy of the parked content, and boot
/// carries it (Ruling 19).
#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_page_whose_parking_row_was_collected_keeps_its_pending_content() {
    let (_, u2) = cross_client_parked();
    let (pool, doc) = legacy_page().await;
    let snapshot_seq: i64 =
        sqlx::query_scalar("SELECT nextval(pg_get_serial_sequence('doc_updates', 'seq'))")
            .fetch_one(&pool)
            .await
            .unwrap();
    let parked = doc_of(&[&u2]);
    let bytes = parked
        .inner()
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    assert!(pending::parks_when_applied(&bytes) && !pending::may_be_shifted(&bytes));
    legacy_snapshot(&pool, doc, snapshot_seq, &parked).await;
    finish_migrations(&pool).await;

    let boot = direct_boot(&pool, doc).await;
    assert_eq!(boot.fallback, None);
    let claims = pending::claims(&boot.doc).expect("u2 pending");
    assert!(within(&insertions_of(&[&u2]), &claims.structs));
    assert!(!boot.carry.is_empty());
}

/// Ruling 6: 0.9.13's GC can delete the trusted snapshot during a long
/// rollout; boot then replays the whole log over the flagged row.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_whose_trusted_snapshot_an_older_gc_deleted_boots_from_the_whole_log() {
    let (u1, u2) = cross_client_parked();
    let (pool, doc) = new_page().await;
    let edits: Vec<Vec<u8>> = (0..6).map(|i| para(30 + i, &format!("edit {i}"))).collect();
    let mut rows: Vec<&[u8]> = vec![&u2];
    rows.extend(edits.iter().map(Vec::as_slice));
    let at = append_rows(&pool, doc, &rows).await;
    let snapshots = PgSnapshotStore::new(pool.clone());
    let after_row_1 = doc_of(&[&u2]);
    snapshots
        .insert_stamped(
            doc,
            SnapshotStamp {
                snapshot_seq: at[0].seq,
                snapshot_rev: Some(at[0].rev),
                epoch: Some(0),
            },
            &pending_free(&after_row_1),
            &after_row_1.inner().transact().state_vector().encode_v1(),
            Some(&[at[0].rev]),
        )
        .await
        .unwrap();
    for i in 1..rows.len() {
        let state = pending_free(&doc_of(&rows[1..=i]));
        older_pods_snapshot(&pool, doc, STAMPED_SNAPSHOT_UPSERT_SQL, at[i], &state).await;
    }
    snapshots.gc(doc, 5, 30).await.unwrap();
    let boot = direct_boot(&pool, doc).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log), ("rollout", false));

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        LAZY,
    )
    .await;
    shut_down(&h).await;
    let trusted = snapshots.latest_trusted(doc).await.unwrap().unwrap();
    assert_eq!(
        trusted.snapshot_rev,
        Some(at[0].rev),
        "no newer trusted row"
    );

    sqlx::query(V0_9_13_GC_SQL)
        .bind(doc)
        .bind(5i64)
        .bind(30i32)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(snapshots.latest_trusted(doc).await.unwrap(), None);
    let boot = direct_boot(&pool, doc).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log), ("rollout", true));
    let claims = pending::claims(&boot.doc).expect("u2 pending");
    assert!(within(&insertions_of(&[&u2]), &claims.structs));

    let h = open_room(
        &pool,
        doc,
        Arc::new(PgUpdatesStore::new(pool.clone())),
        LAZY,
    )
    .await;
    let c = Client::join(&h, 7).await;
    c.send(&h, u1).await;
    until_text(&h, "back held").await;
    shut_down(&h).await;
}
