//! Page history M1.2: how revs are handed out under concurrency, how a flush,
//! a snapshot write and legacy numbering meet a purge, and how an append that
//! is stuck or abandoned gives its connection and locks back. Against the dev
//! Postgres, a fresh database per test.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use knot_storage::{
    DocStore, NewUpdate, PgDocStore, PgSnapshotStore, PgUpdatesStore, SnapshotStamp, SnapshotStore,
    SnapshotStoreError, UpdateSource, UpdatesStore, UpdatesStoreError,
};
use knot_test_support::locks::wait_for_lock_waits;
use sqlx::PgPool;
use uuid::Uuid;

/// knot 0.9.9's `INSERT_UPDATES_SQL`, verbatim: 0.9.9 pods keep running it
/// during the rolling upgrade, and the trigger numbers its rows.
const OLD_INSERT_UPDATES_SQL: &str = "WITH ins AS (
             INSERT INTO doc_updates (doc_id, by_user_id, update_bytes)
             SELECT $1, u.by_user_id, u.update_bytes
             FROM UNNEST($2::uuid[], $3::bytea[])
                  WITH ORDINALITY AS u(by_user_id, update_bytes, ord)
             ORDER BY u.ord
             RETURNING seq, by_user_id
         ),
         contrib AS (
             INSERT INTO doc_contributors (doc_id, user_id)
             SELECT DISTINCT $1::uuid, ins.by_user_id
             FROM ins
             WHERE ins.by_user_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM doc_contributors c
                   WHERE c.doc_id = $1
                     AND c.user_id = ins.by_user_id
                     AND c.last_edited_at >= now() - interval '1 minute'
               )
             ORDER BY ins.by_user_id
             ON CONFLICT (doc_id, user_id) DO UPDATE
                 SET last_edited_at = now()
                 WHERE doc_contributors.last_edited_at < now() - interval '1 minute'
         )
         SELECT seq FROM ins ORDER BY seq";

/// A 0.9.10 snapshot write (Task 3's `migrations_apply.rs`).
const STAMPED_SNAPSHOT_INSERT_SQL: &str = "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, $2, $3, $4, $5, $6)";

/// Statements (0) and (1) of `append_in_tx` (plan, Global Constraints), so a
/// test can stop a flush between its steps.
const DOC_KEY_SHARE_SQL: &str = "SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE";
const HEADS_UPSERT_SQL: &str =
    "INSERT INTO doc_history_heads AS h (doc_id, head_rev, dirty, last_write_at)
     VALUES ($1, $2, true, clock_timestamp())
     ON CONFLICT (doc_id) DO UPDATE
     SET head_rev = h.head_rev + $2, dirty = true, last_write_at = clock_timestamp()
     RETURNING h.head_rev - $2 AS base, h.content_epoch";

/// The log columns migration: 0.9.9's schema plus `rev`, `source`, `meta`,
/// `epoch`, and no rev trigger yet.
const HISTORY_LOG_COLUMNS: i64 = 20261107120000;

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
}

fn store_sqlstate(e: &UpdatesStoreError) -> Option<String> {
    match e {
        UpdatesStoreError::Sqlx(e) => sqlstate(e),
        _ => None,
    }
}

async fn pool_on(url: &str, max: u32, application_name: Option<&str>) -> PgPool {
    let mut options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    if let Some(name) = application_name {
        options = options.application_name(name);
    }
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(max)
        .connect_with(options)
        .await
        .expect("pool")
}

struct World {
    url: String,
    pool: PgPool,
    ws: Uuid,
    users: [Uuid; 2],
}

async fn seed(pool: &PgPool) -> (Uuid, [Uuid; 2]) {
    let ws: Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
            .fetch_one(pool)
            .await
            .unwrap();
    let mut users = [Uuid::nil(); 2];
    for (i, user) in users.iter_mut().enumerate() {
        *user = sqlx::query_scalar(
            "INSERT INTO users (email, display_name) VALUES ($1, 'U') RETURNING id",
        )
        .bind(format!("u{i}@x.test"))
        .fetch_one(pool)
        .await
        .unwrap();
    }
    (ws, users)
}

async fn world() -> World {
    let db = knot_test_support::fresh_db().await;
    let (ws, users) = seed(&db.pool).await;
    World {
        url: db.url,
        pool: db.pool,
        ws,
        users,
    }
}

/// A top-level page with a key of its own.
async fn page(pool: &PgPool, ws: Uuid, user: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', gen_random_uuid()::text, $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn edit(by: Option<Uuid>) -> NewUpdate {
    NewUpdate {
        by_user_id: by,
        bytes: vec![1],
        source: UpdateSource::Edit,
        meta: None,
    }
}

async fn revs_by_rev(pool: &PgPool, doc: Uuid) -> Vec<i64> {
    sqlx::query_scalar("SELECT rev FROM doc_updates WHERE doc_id = $1 ORDER BY rev")
        .bind(doc)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn row_count(pool: &PgPool, doc: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE doc_id = $1")
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn heads_exists(pool: &PgPool, doc: Uuid) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM doc_history_heads WHERE doc_id = $1)")
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A seeded xorshift, so a failing run can be replayed.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

/// What one writer task of the concurrency test saw.
struct WriterReport {
    path: &'static str,
    latencies: Vec<Duration>,
    errors: Vec<String>,
    rows: i64,
}

impl WriterReport {
    fn print(&mut self) {
        self.latencies.sort();
        eprintln!(
            "{}: {} flushes, {} rows, p50 {:?}, p99 {:?}",
            self.path,
            self.latencies.len(),
            self.rows,
            percentile(&self.latencies, 0.5),
            percentile(&self.latencies, 0.99),
        );
    }
}

/// Spike "rev allocation under concurrency": two 0.9.10 replicas and a 0.9.9
/// pod flush the same page at once. Every flush gets its revs, the log is
/// gapless at every moment a reader looks, and nothing deadlocks.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn concurrent_appends_get_gapless_commit_ordered_revs() {
    let w = world().await;
    let d = page(&w.pool, w.ws, w.users[0]).await;
    let users = w.users;

    let replica = move |pool: PgPool, path: &'static str, seed: u64| {
        tokio::spawn(async move {
            let store = PgUpdatesStore::new(pool);
            let mut rng = XorShift(seed);
            let mut report = WriterReport {
                path,
                latencies: Vec::new(),
                errors: Vec::new(),
                rows: 0,
            };
            for i in 0..300 {
                let n = 1 + (rng.next() % 5) as usize;
                let rows: Vec<NewUpdate> = (0..n).map(|_| edit(Some(users[i % 2]))).collect();
                let started = Instant::now();
                match store.append(d, 0, &rows).await {
                    Ok(persisted) => {
                        report.rows += n as i64;
                        let first = persisted[0].rev;
                        let revs: Vec<i64> = persisted.iter().map(|p| p.rev).collect();
                        if revs != (first..first + n as i64).collect::<Vec<_>>() {
                            report.errors.push(format!("not consecutive: {revs:?}"));
                        }
                    }
                    Err(e) => report
                        .errors
                        .push(format!("{e} ({:?})", store_sqlstate(&e))),
                }
                report.latencies.push(started.elapsed());
            }
            report
        })
    };
    let a = replica(pool_on(&w.url, 4, None).await, "replica A", 0x9E37_79B9);
    let b = replica(pool_on(&w.url, 4, None).await, "replica B", 0x85EB_CA6B);
    let c = {
        let pool = pool_on(&w.url, 4, None).await;
        tokio::spawn(async move {
            let mut rng = XorShift(0xC2B2_AE35);
            let mut report = WriterReport {
                path: "0.9.9 pod",
                latencies: Vec::new(),
                errors: Vec::new(),
                rows: 0,
            };
            for i in 0..300 {
                let n = 1 + (rng.next() % 3) as usize;
                let started = Instant::now();
                let saved: Result<Vec<i64>, sqlx::Error> =
                    sqlx::query_scalar(OLD_INSERT_UPDATES_SQL)
                        .bind(d)
                        .bind(vec![Some(users[i % 2]); n])
                        .bind(vec![vec![1u8]; n])
                        .fetch_all(&pool)
                        .await;
                match saved {
                    Ok(_) => report.rows += n as i64,
                    Err(e) => report.errors.push(format!("{e} ({:?})", sqlstate(&e))),
                }
                report.latencies.push(started.elapsed());
            }
            report
        })
    };
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let pool = pool_on(&w.url, 2, None).await;
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut polls = 0u64;
            let mut bad = Vec::new();
            while !stop.load(SeqCst) {
                let revs = revs_by_rev(&pool, d).await;
                if revs != (1..=revs.len() as i64).collect::<Vec<_>>() {
                    bad.push(format!("a reader saw a gap: {revs:?}"));
                }
                polls += 1;
            }
            (polls, bad)
        })
    };

    let mut reports = vec![a.await.unwrap(), b.await.unwrap(), c.await.unwrap()];
    stop.store(true, SeqCst);
    let (polls, bad) = reader.await.unwrap();
    for report in &mut reports {
        report.print();
        assert!(
            report.errors.is_empty(),
            "{}: {:?}",
            report.path,
            report.errors
        );
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert!(polls > 0);
    eprintln!("reader: {polls} polls, each gapless");

    let n: i64 = reports.iter().map(|r| r.rows).sum();
    assert_eq!(row_count(&w.pool, d).await, n);
    assert_eq!(revs_by_rev(&w.pool, d).await, (1..=n).collect::<Vec<_>>());
    let head_rev: i64 =
        sqlx::query_scalar("SELECT head_rev FROM doc_history_heads WHERE doc_id = $1")
            .bind(d)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!(head_rev, n);
    let created: Vec<DateTime<Utc>> =
        sqlx::query_scalar("SELECT created_at FROM doc_updates WHERE doc_id = $1 ORDER BY rev")
            .bind(d)
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert!(
        created.windows(2).all(|p| p[0] <= p[1]),
        "created_at is non-decreasing in rev order"
    );
}

/// The append takes the page row before the heads row, so a purge and a
/// flush wait for each other instead of deadlocking, in either order.
#[tokio::test(flavor = "multi_thread")]
async fn purge_racing_an_append_never_deadlocks() {
    let w = world().await;
    let docs = PgDocStore::new(w.pool.clone());
    let store = PgUpdatesStore::new(w.pool.clone());
    let actor = w.users[0];

    // The flush first, stopped after its heads upsert.
    let d = page(&w.pool, w.ws, actor).await;
    docs.archive(w.ws, d, actor).await.unwrap();
    let mut flush = w.pool.begin().await.unwrap();
    sqlx::query(DOC_KEY_SHARE_SQL)
        .bind(d)
        .fetch_one(&mut *flush)
        .await
        .unwrap();
    let (base, epoch): (i64, i32) = sqlx::query_as(HEADS_UPSERT_SQL)
        .bind(d)
        .bind(1i64)
        .fetch_one(&mut *flush)
        .await
        .unwrap();
    let purge = tokio::spawn({
        let docs = PgDocStore::new(w.pool.clone());
        let ws = w.ws;
        async move { tokio::time::timeout(Duration::from_secs(5), docs.purge(ws, d, None)).await }
    });
    wait_for_lock_waits(&w.pool, 1).await;
    tokio::time::timeout(
        Duration::from_secs(5),
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, rev, epoch, source)
             VALUES ($1, '\\x01', $2, $3, 'edit')",
        )
        .bind(d)
        .bind(base + 1)
        .bind(epoch)
        .execute(&mut *flush),
    )
    .await
    .expect("the flush's insert does not wait on the purge")
    .expect("the flush's insert");
    flush.commit().await.unwrap();
    let purged = purge
        .await
        .unwrap()
        .expect("purge within 5 s")
        .expect("the purge waited, then ran");
    assert_eq!(purged, vec![d]);
    assert!(!heads_exists(&w.pool, d).await, "the heads row cascades");
    assert_eq!(row_count(&w.pool, d).await, 0);

    // The purge first: the append waits on the page row, then finds it gone.
    let d = page(&w.pool, w.ws, actor).await;
    store.append(d, 0, &[edit(Some(actor))]).await.unwrap();
    let mut purge = w.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(d)
        .execute(&mut *purge)
        .await
        .unwrap();
    let append = tokio::spawn({
        let store = store.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(5), store.append(d, 0, &[edit(None)])).await
        }
    });
    wait_for_lock_waits(&w.pool, 1).await;
    purge.commit().await.unwrap();
    let e = append
        .await
        .unwrap()
        .expect("append within 5 s")
        .unwrap_err();
    assert!(matches!(e, UpdatesStoreError::DocGone), "{e:?}");
    assert!(e.is_permanent());
}

/// A snapshot write takes the page row first too (in `doc_snapshots_epoch`).
#[tokio::test(flavor = "multi_thread")]
async fn purge_racing_a_snapshot_write_never_deadlocks() {
    let w = world().await;
    let docs = PgDocStore::new(w.pool.clone());
    let snapshots = PgSnapshotStore::new(w.pool.clone());
    let actor = w.users[0];

    // The snapshot first: one autocommit statement in production, held open
    // here by hand.
    let d = page(&w.pool, w.ws, actor).await;
    docs.archive(w.ws, d, actor).await.unwrap();
    let mut write = w.pool.begin().await.unwrap();
    sqlx::query(STAMPED_SNAPSHOT_INSERT_SQL)
        .bind(d)
        .bind(1i64)
        .bind(b"state".as_slice())
        .bind(b"sv".as_slice())
        .bind(0i64)
        .bind(0i32)
        .execute(&mut *write)
        .await
        .unwrap();
    let purge = tokio::spawn({
        let docs = PgDocStore::new(w.pool.clone());
        let ws = w.ws;
        async move { tokio::time::timeout(Duration::from_secs(5), docs.purge(ws, d, None)).await }
    });
    wait_for_lock_waits(&w.pool, 1).await;
    write.commit().await.unwrap();
    let purged = purge
        .await
        .unwrap()
        .expect("purge within 5 s")
        .expect("the purge waited, then ran");
    assert_eq!(purged, vec![d]);

    // The purge first: the snapshot write waits, then finds the page gone.
    let d = page(&w.pool, w.ws, actor).await;
    let mut purge = w.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(d)
        .execute(&mut *purge)
        .await
        .unwrap();
    let write = tokio::spawn({
        let snapshots = snapshots.clone();
        async move {
            let stamp = SnapshotStamp {
                snapshot_seq: 1,
                snapshot_rev: Some(0),
                epoch: Some(0),
            };
            tokio::time::timeout(
                Duration::from_secs(5),
                snapshots.insert_stamped(d, stamp, b"state", b"sv"),
            )
            .await
        }
    });
    wait_for_lock_waits(&w.pool, 1).await;
    purge.commit().await.unwrap();
    let e = write
        .await
        .unwrap()
        .expect("snapshot write within 5 s")
        .unwrap_err();
    assert!(matches!(e, SnapshotStoreError::DocGone), "{e:?}");
}

/// Two replicas booting the same legacy page at once number it once, and a
/// purge racing them waits instead of deadlocking.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_legacy_numbering_never_deadlocks() {
    const ROWS: i32 = 500;
    let url = knot_test_support::fresh_db_url().await;
    let pool = pool_on(&url, 8, None).await;
    let full = sqlx::migrate!("../../migrations");
    assert!(
        full.migrations
            .iter()
            .any(|m| m.version == HISTORY_LOG_COLUMNS)
    );
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version <= HISTORY_LOG_COLUMNS)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
    .run(&pool)
    .await
    .unwrap();
    let (ws, users) = seed(&pool).await;
    let mut pages = Vec::new();
    for _ in 0..15 {
        let d = page(&pool, ws, users[0]).await;
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes)
             SELECT $1, int4send(g) FROM generate_series(1, $2) g",
        )
        .bind(d)
        .bind(ROWS)
        .execute(&pool)
        .await
        .unwrap();
        pages.push(d);
    }
    full.run(&pool).await.unwrap();

    let a = PgUpdatesStore::new(pool_on(&url, 2, None).await);
    let b = PgUpdatesStore::new(pool_on(&url, 2, None).await);
    let number = |store: &PgUpdatesStore, barrier: &Arc<tokio::sync::Barrier>, d: Uuid| {
        let store = store.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            tokio::time::timeout(Duration::from_secs(10), store.ensure_legacy_numbered(d))
                .await
                .expect("numbering within 10 s")
        })
    };

    for &d in &pages[..10] {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let (x, y) = (number(&a, &barrier, d), number(&b, &barrier, d));
        let (x, y) = (x.await.unwrap(), y.await.unwrap());
        let (x, y) = (
            x.unwrap_or_else(|e| panic!("{e:?}")),
            y.unwrap_or_else(|e| panic!("{e:?}")),
        );
        assert_eq!(x + y, ROWS as u64, "one numbered everything: {x} + {y}");
        let revs: Vec<Option<i64>> =
            sqlx::query_scalar("SELECT rev FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
                .bind(d)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(revs, (1..=ROWS as i64).map(Some).collect::<Vec<_>>());
    }

    let docs = PgDocStore::new(pool_on(&url, 2, None).await);
    for &d in &pages[10..] {
        docs.archive(ws, d, users[0]).await.unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let (x, y) = (number(&a, &barrier, d), number(&b, &barrier, d));
        let purge = tokio::spawn({
            let docs = docs.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                tokio::time::timeout(Duration::from_secs(10), docs.purge(ws, d, None))
                    .await
                    .expect("purge within 10 s")
            }
        });
        for numbered in [x.await.unwrap(), y.await.unwrap()] {
            match numbered {
                Ok(_) | Err(UpdatesStoreError::DocGone) => {}
                Err(e) => panic!("numbering racing a purge: {e:?}"),
            }
        }
        let purged = purge.await.unwrap().expect("the purge succeeds");
        assert_eq!(purged, vec![d]);
    }
}

/// Samples, every 100 ms until stopped, the most connections `pool` had in
/// use and the most backends named `app` waiting on a lock at once.
struct Sampler {
    stop: Arc<AtomicBool>,
    max_in_use: Arc<AtomicU64>,
    max_waiting: Arc<AtomicU64>,
    handle: tokio::task::JoinHandle<()>,
}

impl Sampler {
    fn start(pool: PgPool, observer: PgPool, app: String) -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let max_in_use = Arc::new(AtomicU64::new(0));
        let max_waiting = Arc::new(AtomicU64::new(0));
        let handle = tokio::spawn({
            let (stop, max_in_use, max_waiting) =
                (stop.clone(), max_in_use.clone(), max_waiting.clone());
            async move {
                while !stop.load(SeqCst) {
                    let in_use = pool.size() as u64 - pool.num_idle() as u64;
                    max_in_use.fetch_max(in_use, SeqCst);
                    max_waiting.fetch_max(lock_waiters(&observer, &app).await as u64, SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        });
        Sampler {
            stop,
            max_in_use,
            max_waiting,
            handle,
        }
    }

    async fn finish(self) -> (u64, u64) {
        self.stop.store(true, SeqCst);
        self.handle.await.unwrap();
        (self.max_in_use.load(SeqCst), self.max_waiting.load(SeqCst))
    }
}

/// Backends named `app` waiting on a lock.
async fn lock_waiters(observer: &PgPool, app: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
          WHERE application_name = $1 AND wait_event_type = 'Lock'",
    )
    .bind(app)
    .fetch_one(observer)
    .await
    .unwrap()
}

/// A page whose heads row exists (`head_rev` 0) and is held `FOR UPDATE` by
/// the returned transaction, and a store on a pool of its own, named so
/// `pg_stat_activity` can tell its backends apart.
async fn blocked_page(
    w: &World,
    statement_timeout: Duration,
) -> (
    Uuid,
    sqlx::Transaction<'static, sqlx::Postgres>,
    PgPool,
    PgUpdatesStore,
    String,
) {
    let d = page(&w.pool, w.ws, w.users[0]).await;
    sqlx::query("INSERT INTO doc_history_heads (doc_id) VALUES ($1)")
        .bind(d)
        .execute(&w.pool)
        .await
        .unwrap();
    let app = format!("knot-rev-{}", Uuid::new_v4().simple());
    let pool = pool_on(&w.url, 4, Some(&app)).await;
    let store = PgUpdatesStore::new(pool.clone()).with_statement_timeout(statement_timeout);
    let mut holder = w.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM doc_history_heads WHERE doc_id = $1 FOR UPDATE")
        .bind(d)
        .execute(&mut *holder)
        .await
        .unwrap();
    (d, holder, pool, store, app)
}

/// Review round 3: an append stuck behind a lock ends inside Postgres
/// (`lock_timeout`/`statement_timeout`), with a transient error, before the
/// writer's own timeout; retries never pile up connections behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_append_gives_up_inside_postgres() {
    let w = world().await;
    let (d, holder, pool, store, app) = blocked_page(&w, Duration::from_secs(1)).await;
    let sampler = Sampler::start(pool.clone(), w.pool.clone(), app);

    let started = Instant::now();
    let mut attempts = 0;
    while started.elapsed() < Duration::from_secs(10) {
        let attempt = Instant::now();
        let e = tokio::time::timeout(Duration::from_secs(10), store.append(d, 0, &[edit(None)]))
            .await
            .expect("Postgres ends the attempt, not the caller's timeout")
            .unwrap_err();
        let took = attempt.elapsed();
        assert!(took < Duration::from_millis(2000), "attempt took {took:?}");
        let code = store_sqlstate(&e);
        assert!(
            matches!(code.as_deref(), Some("55P03") | Some("57014")),
            "{e} ({code:?})"
        );
        assert!(!e.is_permanent(), "{e}");
        attempts += 1;
        // The writer's shortest backoff.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    holder.rollback().await.unwrap();
    let persisted = store.append(d, 0, &[edit(None)]).await.unwrap();
    let (max_in_use, max_waiting) = sampler.finish().await;

    assert!(attempts >= 5, "{attempts} attempts");
    assert!(max_in_use <= 1, "{max_in_use} connections in use at once");
    assert!(max_waiting <= 1, "{max_waiting} backends waiting at once");
    assert_eq!(persisted.iter().map(|p| p.rev).collect::<Vec<_>>(), [1]);
    assert_eq!(row_count(&w.pool, d).await, 1, "no failed attempt wrote");
    assert_eq!(revs_by_rev(&w.pool, d).await, [1]);
}

/// Review round 3: an attempt the caller abandons (the writer's
/// `attempt_timeout`) is cancelled in Postgres by the drop guard, so it gives
/// back its connection and its locks long before `statement_timeout`.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_append_is_cancelled_in_postgres() {
    let w = world().await;
    let (d, holder, pool, store, app) = blocked_page(&w, Duration::from_secs(30)).await;
    let sampler = Sampler::start(pool.clone(), w.pool.clone(), app.clone());

    for i in 0..8 {
        let rows = [edit(None)];
        // Boxed, so `drop(attempt)` below drops the future itself.
        let mut attempt = Box::pin(store.append(d, 0, &rows));
        // Wait until the attempt waits on the heads row, then abandon it.
        let waiting_since = Instant::now();
        loop {
            tokio::select! {
                r = &mut attempt => panic!("attempt {i} ended: {r:?}"),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if lock_waiters(&w.pool, &app).await == 1 {
                break;
            }
            assert!(
                waiting_since.elapsed() < Duration::from_secs(5),
                "attempt {i} never waited on the heads row"
            );
        }
        tokio::select! {
            r = &mut attempt => panic!("attempt {i} ended: {r:?}"),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        drop(attempt);

        let abandoned = Instant::now();
        while lock_waiters(&w.pool, &app).await > 0 {
            assert!(
                abandoned.elapsed() < Duration::from_secs(3),
                "attempt {i}'s backend still waits on the heads row"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    holder.rollback().await.unwrap();
    store.append(d, 0, &[edit(None)]).await.unwrap();
    let (max_in_use, _) = sampler.finish().await;

    assert!(max_in_use <= 2, "{max_in_use} connections in use at once");
    assert_eq!(
        row_count(&w.pool, d).await,
        1,
        "no abandoned attempt committed"
    );
    assert_eq!(revs_by_rev(&w.pool, d).await, [1]);
}

/// `(pg_backend_pid(), now())` inside a transaction on `pool`'s only
/// connection, then `pg_sleep(2)` in it; sends the pair out before sleeping.
fn sleep_in_a_transaction(
    pool: PgPool,
    info: tokio::sync::oneshot::Sender<(i32, DateTime<Utc>)>,
) -> tokio::task::JoinHandle<Result<(), sqlx::Error>> {
    tokio::spawn(async move {
        let mut tx = pool.begin().await?;
        let started: (i32, DateTime<Utc>) = sqlx::query_as("SELECT pg_backend_pid(), now()")
            .fetch_one(&mut *tx)
            .await?;
        info.send(started).unwrap();
        sqlx::query("SELECT pg_sleep(2)").execute(&mut *tx).await?;
        tx.commit().await
    })
}

async fn wait_until_sleeping(observer: &PgPool, pid: i32) {
    for _ in 0..250 {
        let sleeping: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                             WHERE pid = $1 AND wait_event = 'PgSleep')",
        )
        .bind(pid)
        .fetch_one(observer)
        .await
        .unwrap();
        if sleeping {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("backend {pid} never slept");
}

/// Review round 5: the guard's cancel matches the abandoned transaction's
/// start, so it never cancels later work on the same pooled connection.
#[tokio::test(flavor = "multi_thread")]
async fn the_drop_guard_cancel_never_hits_a_reused_connection() {
    let w = world().await;
    let p1 = pool_on(&w.url, 1, None).await;
    let p2 = w.pool.clone();

    let mut tx = p1.begin().await.unwrap();
    let (pid, t_old): (i32, DateTime<Utc>) = sqlx::query_as("SELECT pg_backend_pid(), now()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Later work on the same connection is left alone.
    let (info, started) = tokio::sync::oneshot::channel();
    let sleeper = sleep_in_a_transaction(p1.clone(), info);
    let (pid2, t_new) = started.await.unwrap();
    assert_eq!(pid2, pid, "the pool reused its one connection");
    assert_ne!(t_new, t_old);
    wait_until_sleeping(&p2, pid).await;
    let cancelled: Option<bool> = sqlx::query_scalar(knot_storage::testing::CANCEL_ABANDONED_SQL)
        .bind(pid)
        .bind(t_old)
        .fetch_optional(&p2)
        .await
        .unwrap();
    assert!(!cancelled.unwrap_or(false), "{cancelled:?}");
    sleeper
        .await
        .unwrap()
        .expect("the reused connection's transaction was not cancelled");

    // Positive control: bound to the running transaction's start, it cancels.
    let (info, started) = tokio::sync::oneshot::channel();
    let sleeper = sleep_in_a_transaction(p1.clone(), info);
    let (pid3, t_now) = started.await.unwrap();
    assert_eq!(pid3, pid);
    wait_until_sleeping(&p2, pid).await;
    let cancelled: Option<bool> = sqlx::query_scalar(knot_storage::testing::CANCEL_ABANDONED_SQL)
        .bind(pid3)
        .bind(t_now)
        .fetch_optional(&p2)
        .await
        .unwrap();
    assert_eq!(cancelled, Some(true));
    let e = sleeper.await.unwrap().unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("57014"), "{e}");
}
