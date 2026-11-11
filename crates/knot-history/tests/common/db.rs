//! A database for the builder's tests: a workspace, the authors of
//! `common::author` as users, pages, rows, snapshots, and readers of what
//! the builder wrote. Assertions on `retry_after` use SQL predicates, never
//! a decoded timestamp (`'infinity'` panics chrono).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use knot_history::builder::{
    CLAIM_LIMIT, PASS_MAX_ROW_BYTES, PASS_MAX_ROWS, PASS_ROWS_BUDGET, PASS_WORDS_BUDGET,
    REPLAY_TIMEOUT, WORDS_TIMEOUT,
};
use knot_history::canonical::visible_content;
use knot_history::{
    BuildLimits, Builder, CHECKPOINT_FLOOR_BYTES, PassResult, ReplayMode, Replayer, TestHooks,
    history_permits,
};
use knot_storage::{
    DocStore, DocUpdate, NewUpdate, PgDocStore, PgSnapshotStore, PgUpdatesStore, UpdateSource,
    UpdatesStore,
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{
    Doc, ReadTxn, StateVector, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim,
};

use super::{Client, History, author};

/// The replay cap the tests build with unless they test it.
pub const CAP: i64 = 8 * 1024 * 1024;

pub struct Db {
    pub pool: PgPool,
    pub ws: Uuid,
    pub docs: PgDocStore,
    pub updates: Arc<PgUpdatesStore>,
    pub snapshots: PgSnapshotStore,
    pages: AtomicUsize,
}

/// A fresh, migrated database with the fixture's workspace and users.
pub async fn db() -> Db {
    let t = knot_test_support::fresh_db().await;
    Db::on(t.pool).await
}

/// The limits the tests build with: no pacing, no debounce, the
/// production budgets and an 8 MiB cap.
pub fn limits() -> BuildLimits {
    BuildLimits {
        max_rows: PASS_MAX_ROWS,
        max_row_bytes: PASS_MAX_ROW_BYTES,
        max_input_bytes: CAP,
        words_max_input_bytes: CAP,
        rows_budget: PASS_ROWS_BUDGET,
        words_budget: PASS_WORDS_BUDGET,
        lease: Duration::from_secs(5 * 60),
        replay_timeout: REPLAY_TIMEOUT,
        words_timeout: WORDS_TIMEOUT,
        checkpoint_floor: CHECKPOINT_FLOOR_BYTES,
        debounce: Duration::ZERO,
        cpu_share: 1.0,
        hooks: TestHooks::default(),
    }
}

/// `limits()` with `f` applied.
pub fn limits_with(f: impl FnOnce(&mut BuildLimits)) -> BuildLimits {
    let mut l = limits();
    f(&mut l);
    l
}

/// A doc state's blocks and delete set, without pending content (as
/// `History::room_snapshot` writes it).
pub fn own_state(doc: &Doc) -> Vec<u8> {
    let mut encoder = EncoderV1::new();
    doc.transact()
        .encode_state_as_update(&StateVector::default(), &mut encoder);
    encoder.to_vec()
}

/// The state after applying `updates` in order.
pub fn state_of<'a>(updates: impl IntoIterator<Item = &'a [u8]>) -> Vec<u8> {
    let doc = Doc::new();
    for u in updates {
        if let Ok(u) = Update::decode_v1(u) {
            let _ = doc.transact_mut().apply_update(u);
        }
    }
    own_state(&doc)
}

/// `state` plus a paragraph `text` that client `client` typed on its view
/// of it (content no row carries); the paragraph's update alone second.
pub fn with_paragraph(client: u64, state: &[u8], text: &str) -> (Vec<u8>, Vec<u8>) {
    let c = Client::new(client);
    c.doc
        .transact_mut()
        .apply_update(Update::decode_v1(state).expect("a state"))
        .expect("the state applies");
    let update = c.txn(|txn, frag| {
        let n = frag.len(txn);
        let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(text));
    });
    (own_state(&c.doc), update)
}

/// What a reader sees in `state`.
pub fn visible(state: &[u8]) -> Value {
    visible_content(
        Replayer::from_state(ReplayMode::Gc, state)
            .expect("a state")
            .doc(),
    )
}

/// The insertions of an update.
pub fn insertions(update: &[u8]) -> yrs::IdSet {
    Update::decode_v1(update)
        .expect("an update")
        .insertions(true)
}

/// A `doc_history_state` row, `retry_after` as SQL predicates.
#[derive(Debug, Clone)]
pub struct State {
    pub built_rev: i64,
    pub genesis_rev: Option<i64>,
    pub builder_version: i32,
    pub snapshot_checked_rev: i64,
    pub skip_revs: Vec<i64>,
    pub skip_baselines: Vec<i64>,
    pub skip_snapshots: Vec<i64>,
    pub rebuilds: i32,
    pub failures: i32,
    pub timeouts: i32,
    pub crashes: i32,
    pub last_error: Option<String>,
    pub claimed_by: Option<Uuid>,
    /// `claimed_until > now()`.
    pub leased: bool,
    /// `retry_after = 'infinity'`.
    pub given_up: bool,
    /// `retry_after > now()`.
    pub deferred: bool,
    /// `retry_after IS NULL`.
    pub retry_null: bool,
    pub parked: Value,
    pub updated_at: DateTime<Utc>,
}

/// A `doc_history_entries` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub kind: String,
    pub author: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub start_rev: Option<i64>,
    pub end_rev: Option<i64>,
    pub sealed: bool,
    pub baseline_id: Option<i64>,
    pub ins_ids: Option<Vec<u8>>,
    pub del_ids: Option<Vec<u8>>,
    pub words_added: Option<i32>,
    pub words_removed: Option<i32>,
    pub words_pending: bool,
    pub meta: Value,
    pub updated_at: DateTime<Utc>,
}

/// An entry's content, ids and `updated_at` aside: what two builds of the
/// same rows must agree on.
pub type EntryKey = (
    String,
    Option<Uuid>,
    Option<i64>,
    Option<i64>,
    DateTime<Utc>,
    DateTime<Utc>,
    bool,
    Vec<u8>,
    Vec<u8>,
    Option<i32>,
    Option<i32>,
    String,
);

impl Entry {
    pub fn key(&self) -> EntryKey {
        (
            self.kind.clone(),
            self.author,
            self.start_rev,
            self.end_rev,
            self.started_at,
            self.ended_at,
            self.sealed,
            self.ins_ids.clone().unwrap_or_default(),
            self.del_ids.clone().unwrap_or_default(),
            self.words_added,
            self.words_removed,
            self.meta.to_string(),
        )
    }

    pub fn inserted(&self) -> yrs::IdSet {
        use yrs::updates::decoder::Decode;
        match &self.ins_ids {
            Some(b) => yrs::IdSet::decode_v1(b).expect("ins_ids"),
            None => yrs::IdSet::new(),
        }
    }
}

/// The keys of `entries`, sorted.
pub fn keys(entries: &[Entry]) -> Vec<EntryKey> {
    let mut k: Vec<EntryKey> = entries.iter().map(Entry::key).collect();
    k.sort();
    k
}

/// A checkpoint row.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub rev: i64,
    pub kind: String,
    pub state_bytes: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

impl Db {
    /// Seeds the workspace and the users `author(0..3)` on a migrated
    /// database.
    pub async fn on(pool: PgPool) -> Db {
        let ws: Uuid = sqlx::query_scalar(
            "INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("workspace");
        for i in 0..3 {
            sqlx::query("INSERT INTO users (id, email, display_name) VALUES ($1, $2, $3)")
                .bind(author(i))
                .bind(format!("u{i}@x.test"))
                .bind(format!("User {i}"))
                .execute(&pool)
                .await
                .expect("user");
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
            )
            .bind(ws)
            .bind(author(i))
            .execute(&pool)
            .await
            .expect("member");
        }
        Db {
            docs: PgDocStore::new(pool.clone()),
            updates: Arc::new(PgUpdatesStore::new(pool.clone())),
            snapshots: PgSnapshotStore::new(pool.clone()),
            pool,
            ws,
            pages: AtomicUsize::new(0),
        }
    }

    /// A `Db` over a database already seeded (the users `author(0..3)`
    /// exist).
    pub async fn on_existing(pool: PgPool, ws: Uuid) -> Db {
        Db {
            docs: PgDocStore::new(pool.clone()),
            updates: Arc::new(PgUpdatesStore::new(pool.clone())),
            snapshots: PgSnapshotStore::new(pool.clone()),
            pool,
            ws,
            pages: AtomicUsize::new(0),
        }
    }

    /// A new top-level page by A.
    pub async fn page(&self) -> Uuid {
        let n = self.pages.fetch_add(1, Ordering::SeqCst);
        self.docs
            .create(
                self.ws,
                None,
                &format!("P{n}"),
                &format!("k{n:06}"),
                author(0),
            )
            .await
            .expect("page")
            .id
    }

    /// A builder with its own `workers` permits.
    pub fn builder(&self, limits: BuildLimits) -> Arc<Builder> {
        self.builder_with(limits, history_permits(4))
    }

    pub fn builder_with(
        &self,
        limits: BuildLimits,
        permits: Arc<tokio::sync::Semaphore>,
    ) -> Arc<Builder> {
        Arc::new(Builder::new(
            self.pool.clone(),
            self.updates.clone(),
            limits,
            permits,
        ))
    }

    /// Appends an `edit` row by author `who` now; returns its rev.
    pub async fn append(&self, doc: Uuid, who: usize, bytes: &[u8]) -> i64 {
        self.append_as(doc, Some(author(who)), bytes, UpdateSource::Edit)
            .await
    }

    pub async fn append_as(
        &self,
        doc: Uuid,
        who: Option<Uuid>,
        bytes: &[u8],
        source: UpdateSource,
    ) -> i64 {
        self.updates
            .append(
                doc,
                0,
                &[NewUpdate {
                    by_user_id: who,
                    bytes: bytes.to_vec(),
                    source,
                    meta: None,
                }],
            )
            .await
            .expect("append")[0]
            .rev
    }

    /// Inserts a row with its own `created_at` (raw, without a rev: the
    /// trigger numbers it); returns its rev.
    pub async fn insert_row(
        &self,
        doc: Uuid,
        who: Option<Uuid>,
        bytes: &[u8],
        source: Option<UpdateSource>,
        created_at: DateTime<Utc>,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO doc_updates (doc_id, by_user_id, update_bytes, source, meta, epoch, created_at)
             VALUES ($1, $2, $3, $4, NULL, 0, $5) RETURNING rev",
        )
        .bind(doc)
        .bind(who)
        .bind(bytes)
        .bind(source.map(|s| s.as_str()))
        .bind(created_at)
        .fetch_one(&self.pool)
        .await
        .expect("insert row")
    }

    /// Inserts generated rows with their own `created_at`, `source` and
    /// `meta`; returns their revs.
    pub async fn insert_rows(&self, doc: Uuid, rows: &[DocUpdate]) -> Vec<i64> {
        let mut revs = Vec::new();
        for r in rows {
            let rev: i64 = sqlx::query_scalar(
                "INSERT INTO doc_updates
                     (doc_id, by_user_id, update_bytes, source, meta, epoch, created_at)
                 VALUES ($1, $2, $3, $4, $5, 0, $6) RETURNING rev",
            )
            .bind(doc)
            .bind(r.by_user_id)
            .bind(&r.update_bytes)
            .bind(r.source.map(|s| s.as_str()))
            .bind(&r.meta)
            .bind(r.created_at)
            .fetch_one(&self.pool)
            .await
            .expect("insert row");
            revs.push(rev);
        }
        revs
    }

    /// Moves the page's `history_since` (and `created_at`) to `since`.
    pub async fn backdate(&self, doc: Uuid, since: DateTime<Utc>) {
        sqlx::query("UPDATE documents SET history_since = $2, created_at = $2 WHERE id = $1")
            .bind(doc)
            .bind(since)
            .execute(&self.pool)
            .await
            .expect("backdate");
    }

    pub async fn history_since(&self, doc: Uuid) -> DateTime<Utc> {
        sqlx::query_scalar("SELECT history_since FROM documents WHERE id = $1")
            .bind(doc)
            .fetch_one(&self.pool)
            .await
            .expect("history_since")
    }

    /// A stamped room snapshot at `rev` (written now).
    pub async fn snapshot(&self, doc: Uuid, rev: i64, state: &[u8]) {
        sqlx::query(
            "INSERT INTO doc_snapshots
                 (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, (SELECT coalesce(max(snapshot_seq), 0) + 1000000
                            FROM doc_snapshots WHERE doc_id = $1),
                     $2, '\\x00', $3, 0)",
        )
        .bind(doc)
        .bind(state)
        .bind(rev)
        .execute(&self.pool)
        .await
        .expect("snapshot");
    }

    /// The bytes of every row of the page, in rev order.
    pub async fn row_bytes(&self, doc: Uuid) -> Vec<Vec<u8>> {
        sqlx::query_scalar("SELECT update_bytes FROM doc_updates WHERE doc_id = $1 ORDER BY rev")
            .bind(doc)
            .fetch_all(&self.pool)
            .await
            .expect("rows")
    }

    /// The room's state through `rev`.
    pub async fn state_at(&self, doc: Uuid, rev: i64) -> Vec<u8> {
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT update_bytes FROM doc_updates WHERE doc_id = $1 AND rev <= $2 ORDER BY rev",
        )
        .bind(doc)
        .bind(rev)
        .fetch_all(&self.pool)
        .await
        .expect("rows");
        state_of(rows.iter().map(Vec::as_slice))
    }

    /// Runs `sql` with `$1` bound to the page.
    pub async fn exec(&self, sql: &str, doc: Uuid) {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(doc)
            .execute(&self.pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// A `doc_updates` rewrite (`$1` the page), under the history rewrite
    /// flag.
    pub async fn rewrite(&self, sql: &str, doc: Uuid) {
        let mut tx = self.pool.begin().await.expect("begin");
        sqlx::query("SET LOCAL knot.history_rewrite = 'on'")
            .execute(&mut *tx)
            .await
            .expect("flag");
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(doc)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        tx.commit().await.expect("commit");
    }

    /// `SELECT <expr> FROM doc_history_state WHERE doc_id = $1` as a bool.
    pub async fn state_is(&self, doc: Uuid, expr: &str) -> bool {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(({expr}), false) FROM doc_history_state WHERE doc_id = $1"
        )))
        .bind(doc)
        .fetch_one(&self.pool)
        .await
        .unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    pub async fn state(&self, doc: Uuid) -> State {
        use sqlx::Row;
        let r = sqlx::query(
            "SELECT built_rev, genesis_rev, builder_version, snapshot_checked_rev, skip_revs,
                    skip_baselines, failures, timeouts, crashes, last_error, claimed_by,
                    coalesce(claimed_until > now(), false),
                    coalesce(retry_after = 'infinity', false),
                    coalesce(retry_after > now(), false),
                    retry_after IS NULL, parked, updated_at, skip_snapshots, rebuilds
               FROM doc_history_state WHERE doc_id = $1",
        )
        .bind(doc)
        .fetch_one(&self.pool)
        .await
        .expect("state");
        State {
            built_rev: r.get(0),
            genesis_rev: r.get(1),
            builder_version: r.get(2),
            snapshot_checked_rev: r.get(3),
            skip_revs: r.get(4),
            skip_baselines: r.get(5),
            failures: r.get(6),
            timeouts: r.get(7),
            crashes: r.get(8),
            last_error: r.get(9),
            claimed_by: r.get(10),
            leased: r.get(11),
            given_up: r.get(12),
            deferred: r.get(13),
            retry_null: r.get(14),
            parked: r.get(15),
            updated_at: r.get(16),
            skip_snapshots: r.get(17),
            rebuilds: r.get(18),
        }
    }

    pub async fn entries(&self, doc: Uuid) -> Vec<Entry> {
        type Row = (
            i64,
            String,
            Option<Uuid>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<i64>,
            Option<i64>,
            bool,
            Option<i64>,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<i32>,
            Option<i32>,
            bool,
            Value,
            DateTime<Utc>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, kind, author_id, started_at, ended_at, start_rev, end_rev, sealed,
                    baseline_id, ins_ids, del_ids, words_added, words_removed, words_pending,
                    meta, updated_at
               FROM doc_history_entries WHERE doc_id = $1 ORDER BY id",
        )
        .bind(doc)
        .fetch_all(&self.pool)
        .await
        .expect("entries");
        rows.into_iter()
            .map(|r| Entry {
                id: r.0,
                kind: r.1,
                author: r.2,
                started_at: r.3,
                ended_at: r.4,
                start_rev: r.5,
                end_rev: r.6,
                sealed: r.7,
                baseline_id: r.8,
                ins_ids: r.9,
                del_ids: r.10,
                words_added: r.11,
                words_removed: r.12,
                words_pending: r.13,
                meta: r.14,
                updated_at: r.15,
            })
            .collect()
    }

    pub async fn checkpoints(&self, doc: Uuid) -> Vec<Checkpoint> {
        let rows: Vec<(i64, String, Vec<u8>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT rev, kind, state_bytes, created_at FROM doc_history_checkpoints
              WHERE doc_id = $1 ORDER BY rev",
        )
        .bind(doc)
        .fetch_all(&self.pool)
        .await
        .expect("checkpoints");
        rows.into_iter()
            .map(|(rev, kind, state_bytes, created_at)| Checkpoint {
                rev,
                kind,
                state_bytes,
                created_at,
            })
            .collect()
    }

    /// `(head_rev, dirty)` of the page's heads row.
    pub async fn head(&self, doc: Uuid) -> (i64, bool) {
        sqlx::query_as("SELECT head_rev, dirty FROM doc_history_heads WHERE doc_id = $1")
            .bind(doc)
            .fetch_one(&self.pool)
            .await
            .expect("heads")
    }

    /// Deadlocks Postgres has counted in this database.
    pub async fn deadlocks(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()",
        )
        .fetch_one(&self.pool)
        .await
        .expect("deadlocks")
    }

    /// Clears the page's backoff so the next claim proceeds.
    pub async fn retry_now(&self, doc: Uuid) {
        self.exec(
            "UPDATE doc_history_state SET retry_after = NULL WHERE doc_id = $1",
            doc,
        )
        .await;
    }

    /// Ends whatever lease the page has and its backoff.
    pub async fn expire_lease(&self, doc: Uuid) {
        self.exec(
            "UPDATE doc_history_state
                SET claimed_until = now() - interval '1 s', retry_after = NULL
              WHERE doc_id = $1",
            doc,
        )
        .await;
    }
}

/// Calls `build_page` until `done` holds for its result (at most 50 times);
/// returns every result.
pub async fn build_until(
    b: &Builder,
    doc: Uuid,
    done: impl Fn(&PassResult) -> bool,
) -> Vec<PassResult> {
    let mut results = Vec::new();
    for _ in 0..50 {
        let r = b.build_page(doc).await;
        let stop = done(&r);
        results.push(r);
        if stop {
            return results;
        }
    }
    panic!("build_page never got there: {results:?}");
}

/// Builds until `Clean`.
pub async fn build_clean(b: &Builder, doc: Uuid) -> Vec<PassResult> {
    build_until(b, doc, |r| *r == PassResult::Clean).await
}

pub fn is_built(r: &PassResult) -> bool {
    matches!(r, PassResult::Built { .. })
}

// ---------------------------------------------------------------------------
// Pages the builder tests share
// ---------------------------------------------------------------------------

/// Author `who` types a paragraph `text`; returns the row's bytes.
pub fn typed(h: &mut History, who: usize, text: &str) -> Vec<u8> {
    let now = h.now();
    let rev = h.type_para(who, text, now);
    h.rows()[rev as usize - 1].update_bytes.clone()
}

pub fn genesis(rev: i64) -> PassResult {
    PassResult::Genesis { genesis_rev: rev }
}

/// A page whose `history_since` is 2 h ago, with rows by `who` typing
/// `texts` at `history_since + minutes`.
pub async fn page_with_rows(db: &Db, rows: &[(usize, &str, i64)]) -> (Uuid, History) {
    let doc = db.page().await;
    let since = Utc::now() - chrono::Duration::hours(2);
    db.backdate(doc, since).await;
    let mut h = History::new(1);
    for &(who, text, m) in rows {
        let bytes = typed(&mut h, who, text);
        db.insert_row(
            doc,
            Some(author(who)),
            &bytes,
            Some(UpdateSource::Edit),
            since + chrono::Duration::minutes(m),
        )
        .await;
    }
    (doc, h)
}

/// A page whose rows 1..4 A typed now, with gap checkpoints from snapshots
/// at `revs` (each the room's state there plus every extra paragraph so
/// far, one more per snapshot), built clean; the snapshots are deleted
/// (thinned) afterwards when `thin`. Returns the paragraphs' updates.
pub async fn page_with_gaps(db: &Db, doc: Uuid, revs: &[(i64, &str)], thin: bool) -> Vec<Vec<u8>> {
    let mut h = History::new(1);
    for t in ["one", "two", "three", "four"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    let mut extras: Vec<Vec<u8>> = Vec::new();
    for (i, &(rev, text)) in revs.iter().enumerate() {
        let room = db.state_at(doc, rev).await;
        let mut state =
            state_of(std::iter::once(room.as_slice()).chain(extras.iter().map(Vec::as_slice)));
        let (s, x) = with_paragraph(20 + i as u64, &state, text);
        state = s;
        extras.push(x);
        db.snapshot(doc, rev, &state).await;
    }
    let b = db.builder(limits());
    build_clean(&b, doc).await;
    if thin {
        db.exec("DELETE FROM doc_snapshots WHERE doc_id = $1", doc)
            .await;
    }
    extras
}

/// `octet_length` of the latest checkpoint at or below `built_rev`, its
/// reapply rows and the replay prefix: a pass's `base`.
pub async fn base_of(db: &Db, doc: Uuid) -> i64 {
    sqlx::query_scalar(
        "WITH s AS (SELECT built_rev FROM doc_history_state WHERE doc_id = $1),
              cp AS (SELECT rev, octet_length(state_bytes) AS len, pending_revs
                       FROM doc_history_checkpoints
                      WHERE doc_id = $1 AND rev <= (SELECT built_rev FROM s)
                      ORDER BY rev DESC LIMIT 1)
         SELECT ((SELECT len FROM cp)
                 + coalesce((SELECT sum(octet_length(update_bytes)) FROM doc_updates
                              WHERE doc_id = $1
                                AND ((rev > (SELECT rev FROM cp) AND rev <= (SELECT built_rev FROM s))
                                     OR rev = ANY(coalesce((SELECT pending_revs FROM cp), ARRAY[]::bigint[])))), 0))::bigint",
    )
    .bind(doc)
    .fetch_one(&db.pool)
    .await
    .expect("base")
}

/// A paragraph of about `bytes` bytes.
pub fn filler(bytes: usize, seed: &str) -> String {
    let mut s = String::with_capacity(bytes);
    while s.len() < bytes {
        s.push_str(seed);
        s.push(' ');
    }
    s
}

/// A page built to its head (rev 1, about 40 KiB), a snapshot at the head
/// about as large, and one more small row; returns the page and a cap that
/// fits the base and the row but not the base and the snapshot.
pub async fn oversized_snapshot_page(db: &Db) -> (Uuid, i64) {
    let doc = db.page().await;
    let mut h = History::new(1);
    let big = typed(&mut h, 0, &filler(40 * 1024, "state"));
    db.append(doc, 0, &big).await;
    build_clean(&db.builder(limits()), doc).await;
    let (s, _) = with_paragraph(9, &db.state_at(doc, 1).await, "more");
    db.snapshot(doc, 1, &s).await;
    let small = typed(&mut h, 0, "small");
    db.append(doc, 0, &small).await;
    let base = base_of(db, doc).await;
    let cap = base * 10 / 6;
    assert!(base + s.len() as i64 > cap && base + small.len() as i64 <= cap);
    (doc, cap)
}

/// A page past genesis with rows 1..3 by A ("r1", "r2", "r3"); returns the
/// rows.
pub async fn genesis_then_rows(db: &Db, doc: Uuid) -> Vec<Vec<u8>> {
    assert_eq!(db.builder(limits()).build_page(doc).await, genesis(0));
    let mut h = History::new(1);
    let mut rows = Vec::new();
    for t in ["r1", "r2", "r3"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
        rows.push(u);
    }
    rows
}

/// `hold_compute` for every page.
pub fn holding(d: Duration) -> knot_history::BuildLimits {
    limits_with(|l| l.hooks.hold_compute = d)
}

/// `CLAIM_LIMIT`, re-exported for the candidate tests.
pub const LIMIT: i64 = CLAIM_LIMIT;
