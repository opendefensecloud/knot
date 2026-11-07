//! The ordered log against Postgres (page history M1.2): a room boots by
//! rev, persists and fans out the effective update, catches up by rev, and
//! writes a snapshot only when it holds nothing beyond the page's
//! pre-upgrade base and the log up to the snapshot's rev. Failed flushes
//! (before the write, or after a COMMIT whose acknowledgement was lost) lose
//! nothing, and pages last saved before the upgrade boot complete.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use knot_crdt::nesting::MAX_NESTING;
use knot_crdt::{
    BootMode, Bus, ConnHandle, DocHandle, Engine, Event, InMsg, MemBus, Room, RoomHandle,
    SnapshotPolicy, YrsEngine,
};
use knot_storage::{
    DocUpdate, HistoryHead, NewUpdate, Persisted, PgSnapshotStore, PgUpdatesStore, RowsAfter,
    UpdatesStore, UpdatesStoreError,
};
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, ReadTxn, StateVector, Subscription, Text, Transact, Update, XmlElementPrelim,
    XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
};

/// The migration that adds the log columns: a database migrated up to it
/// holds rows and snapshots as 0.9.9 wrote them (no rev, no heads row).
const HISTORY_LOG_COLUMNS: i64 = 20261107120000;

// ---------------------------------------------------------------------------
// Updates and docs
// ---------------------------------------------------------------------------

/// A seeded xorshift64: the histories are reproducible from the seed alone.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }

    fn word(&mut self) -> String {
        let len = 1 + self.below(6);
        (0..len)
            .map(|_| char::from(b'a' + self.below(26) as u8))
            .collect()
    }
}

/// Whether two updates carry the same insertions and deletions: a room
/// persists and fans out what an apply changed, which yrs re-encodes. As
/// `knot_crdt::room`'s test helper.
fn same_update(a: &[u8], b: &[u8]) -> bool {
    let (a, b) = (
        Update::decode_v1(a).expect("a decodes"),
        Update::decode_v1(b).expect("b decodes"),
    );
    a.insertions(true) == b.insertions(true) && a.delete_set() == b.delete_set()
}

/// The update inside a sync update frame.
fn unwrap_frame(frame: &[u8]) -> Vec<u8> {
    assert_eq!(frame[..2], [0, 2], "not a sync update frame");
    let (mut len, mut shift, mut at) = (0usize, 0, 2);
    loop {
        let b = frame[at];
        at += 1;
        len |= ((b & 0x7f) as usize) << shift;
        if b < 0x80 {
            break;
        }
        shift += 7;
    }
    assert_eq!(frame.len() - at, len, "frame length");
    frame[at..].to_vec()
}

/// A paragraph holding `text`, from a fresh doc of `client`.
fn para(client: u64, text: &str) -> Vec<u8> {
    let doc = Doc::with_client_id(client);
    let frag = doc.get_or_insert_xml_fragment("default");
    {
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(text));
    }
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

fn doc_of(updates: &[&[u8]]) -> DocHandle {
    let doc = YrsEngine.new_doc();
    for u in updates {
        YrsEngine.apply_update(&doc, u).expect("the update applies");
    }
    doc
}

fn text(d: &DocHandle) -> String {
    let frag = d.inner().get_or_insert_xml_fragment("default");
    frag.get_string(&d.inner().transact())
}

/// The pending structs and pending delete set, each encoded.
fn pending(d: &DocHandle) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let txn = d.inner().transact();
    let store = txn.store();
    (
        store.pending_update().map(|p| p.update.encode_v1()),
        store.pending_ds().map(|ds| ds.encode_v1()),
    )
}

/// Where two docs differ (state vector, delete set, text, pending content),
/// if they do.
fn difference(a: &DocHandle, b: &DocHandle) -> Option<String> {
    let state = |d: &DocHandle| {
        let txn = d.inner().transact();
        (txn.state_vector(), txn.snapshot().delete_set.encode_v1())
    };
    let ((sv_a, ds_a), (sv_b, ds_b)) = (state(a), state(b));
    if sv_a != sv_b {
        return Some(format!("state vectors differ: {sv_a:?} / {sv_b:?}"));
    }
    if ds_a != ds_b {
        return Some("delete sets differ".into());
    }
    let (text_a, text_b) = (text(a), text(b));
    if text_a != text_b {
        return Some(format!("content differs:\n{text_a}\n{text_b}"));
    }
    let ((pu_a, pd_a), (pu_b, pd_b)) = (pending(a), pending(b));
    let structs_agree = match (&pu_a, &pu_b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_update(a, b),
        _ => false,
    };
    if !structs_agree {
        return Some(format!(
            "pending structs differ: {:?} / {:?}",
            pu_a.is_some(),
            pu_b.is_some()
        ));
    }
    if pd_a != pd_b {
        return Some("pending deletes differ".into());
    }
    None
}

#[track_caller]
fn assert_same_doc(a: &DocHandle, b: &DocHandle, what: &str) {
    if let Some(how) = difference(a, b) {
        panic!("{what}: {how}");
    }
}

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

/// The page's rows, in rev order.
async fn rows(pool: &PgPool, doc: Uuid) -> Vec<DocUpdate> {
    PgUpdatesStore::new(pool.clone())
        .rows_after(doc, RowsAfter::Rev(0))
        .await
        .unwrap()
}

/// The page's log replayed into a fresh doc up to and including `upto`.
async fn replay(pool: &PgPool, doc: Uuid, upto: i64) -> DocHandle {
    let rows = rows(pool, doc).await;
    let upto: Vec<&[u8]> = rows
        .iter()
        .filter(|r| r.rev.unwrap() <= upto)
        .map(|r| r.update_bytes.as_slice())
        .collect();
    doc_of(&upto)
}

async fn row_count(pool: &PgPool, doc: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE doc_id = $1")
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
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

/// An editor: a yrs doc joined to the room through a connection of its own.
struct Client {
    doc: Doc,
    frag: XmlFragmentRef,
    conn: Uuid,
    rx: mpsc::Receiver<Vec<u8>>,
    /// The update of each local transaction since the last `take`.
    own: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Off while the client applies what it receives.
    recording: Arc<AtomicBool>,
    _sub: Subscription,
}

impl Client {
    async fn join(h: &RoomHandle, client: u64) -> Self {
        let conn = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(8192);
        let (reply, joined) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id: conn,
            handle: ConnHandle::new(tx),
            reply,
        })
        .await
        .unwrap();
        let state = joined.await.unwrap().unwrap();
        let doc = Doc::with_client_id(client);
        let frag = doc.get_or_insert_xml_fragment("default");
        let own = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::new(AtomicBool::new(true));
        let sub = {
            let (own, recording) = (own.clone(), recording.clone());
            doc.observe_update_v1(move |_, e| {
                if recording.load(Ordering::SeqCst) {
                    own.lock().unwrap().push(e.update.clone());
                }
            })
            .unwrap()
        };
        let client = Client {
            doc,
            frag,
            conn,
            rx,
            own,
            recording,
            _sub: sub,
        };
        client.apply(&state.update);
        client
    }

    fn apply(&self, update: &[u8]) {
        self.recording.store(false, Ordering::SeqCst);
        let _ = self
            .doc
            .transact_mut()
            .apply_update(Update::decode_v1(update).unwrap());
        self.recording.store(true, Ordering::SeqCst);
    }

    /// Applies every frame received so far.
    fn sync(&mut self) -> Vec<Vec<u8>> {
        let mut got = Vec::new();
        while let Ok(frame) = self.rx.try_recv() {
            let update = unwrap_frame(&frame);
            self.apply(&update);
            got.push(update);
        }
        got
    }

    fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.own.lock().unwrap())
    }

    fn text_of(&self, txn: &impl ReadTxn, i: u32) -> Option<XmlTextRef> {
        match self.frag.get(txn, i)? {
            XmlOut::Element(p) => match p.get(txn, 0)? {
                XmlOut::Text(t) => Some(t),
                _ => None,
            },
            _ => None,
        }
    }

    /// One random transaction: a new paragraph, a word typed into one, or
    /// a few characters deleted. Returns its update.
    fn edit(&mut self, rng: &mut Rng) -> Vec<u8> {
        {
            let mut txn = self.doc.transact_mut();
            let n = self.frag.len(&txn);
            let pick = if n > 0 { rng.below(3) } else { 2 };
            let target = if n > 0 {
                self.text_of(&txn, rng.below(n as usize) as u32)
            } else {
                None
            };
            match (pick, target) {
                (0, Some(t)) => {
                    let at = rng.below(t.len(&txn) as usize + 1) as u32;
                    t.insert(&mut txn, at, &rng.word());
                }
                (1, Some(t)) if t.len(&txn) > 2 => {
                    let at = rng.below(t.len(&txn) as usize - 2) as u32;
                    t.remove_range(&mut txn, at, 1 + rng.below(2) as u32);
                }
                _ => {
                    let at = rng.below(n as usize + 1) as u32;
                    let p = self
                        .frag
                        .insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
                    p.push_back(&mut txn, XmlTextPrelim::new(rng.word()));
                }
            }
        }
        let mut own = self.take();
        assert_eq!(own.len(), 1, "one transaction, one update");
        own.pop().unwrap()
    }

    async fn send(&self, h: &RoomHandle, bytes: Vec<u8>) {
        h.tx.send(Event::Inbound(InMsg {
            from: self.conn,
            by_user: None,
            bytes,
        }))
        .await
        .unwrap();
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
}

impl Flaky {
    fn new(pool: &PgPool, mode: Mode, k: usize) -> Arc<Self> {
        Arc::new(Flaky {
            inner: PgUpdatesStore::new(pool.clone()),
            mode,
            left: AtomicUsize::new(k),
        })
    }

    fn take_failure(&self) -> bool {
        self.left
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
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

/// Every stamped snapshot equals the log replayed to its rev, also while
/// the room holds pending content, around failed flushes and a reconnect
/// resend; a reboot equals the whole log replayed.
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

        // Failed flushes first.
        while edits < 20 {
            edit(&mut rng, &mut a, &mut b).await;
            edits += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
        assert!(
            snapshots.len() >= 3,
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
        assert!(
            with_pending > 0,
            "{mode:?}: no snapshot was written while content was pending"
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
