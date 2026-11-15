//! The views service (page history M1.4, Task 5): loads, permits, renders,
//! caches and the metrics, against Postgres.
//!
//! Tests that assert a metric run on a current-thread runtime (the local
//! recorder is thread-local) and await every view on the test task, two at
//! once through `tokio::join!`; the renders still run on threads of their
//! own. Every call passes a fresh principal unless the test is about the
//! principal's render slot.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::db::{
    Db, Entry, build_clean, db, insertions, limits, page_with_rows, state_of, typed, visible,
    with_paragraph,
};
use common::metrics::CountingRecorder;
use common::{Client, History, author};
use knot_history::canonical::visible_content;
use knot_history::revert::{self, Checkpoint};
use knot_history::{
    BUILDER_VERSION, EntryKind, HistoryPermits, PlanError, REVERT_MIN_PLAN_TIME, ReplayMode,
    Replayer, Restorable, RevertHooks, RevertKind, RevertOutcome, RevertTarget, VIEW_PERMIT_WAIT,
    ViewError, ViewLimits, Views, history_permits, list_entries, merge_gap_at_end, pm_json,
    state_input,
};
use knot_storage::UpdateSource;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::time::Instant;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{Doc, Text, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fresh principal: no test waits on a render slot by accident.
fn anyone() -> Uuid {
    Uuid::new_v4()
}

/// `ViewLimits::default()` with `f` applied.
fn views_with(pool: &PgPool, permits: &HistoryPermits, f: impl FnOnce(&mut ViewLimits)) -> Views {
    let mut l = ViewLimits::default();
    f(&mut l);
    Views::new(pool.clone(), permits.clone(), l)
}

fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn count(c: &Arc<AtomicUsize>) -> usize {
    c.load(Ordering::SeqCst)
}

fn outcome(rec: &CountingRecorder, o: &str) -> u64 {
    rec.counter("knot_history_views_total", &[("outcome", o)])
}

/// Every `knot_history_views_total` outcome.
const OUTCOMES: [&str; 10] = [
    "cached",
    "rendered",
    "diff_failed",
    "no_changes",
    "not_found",
    "rebuilding",
    "too_large",
    "busy",
    "failed",
    "timeout",
];

fn views_counted(rec: &CountingRecorder) -> u64 {
    OUTCOMES.iter().map(|o| outcome(rec, o)).sum()
}

fn view_timeouts(rec: &CountingRecorder) -> u64 {
    rec.counter("knot_history_replay_timeouts_total", &[("phase", "view")])
}

fn stalled_gauge(rec: &CountingRecorder) -> Option<f64> {
    rec.gauge("knot_history_replay_threads_stalled")
}

fn view_seconds(rec: &CountingRecorder) -> u64 {
    rec.histogram("knot_history_view_seconds", &[])
}

/// The doc of a rendered view.
fn json_of(v: &knot_history::RenderedView) -> Value {
    serde_json::from_slice(&v.doc).expect("the view is JSON")
}

/// Every text node of a ProseMirror doc, with its marks' types.
fn texts(v: &Value) -> Vec<(String, Vec<String>)> {
    fn walk(v: &Value, out: &mut Vec<(String, Vec<String>)>) {
        if v["type"] == "text" {
            let marks = v["marks"]
                .as_array()
                .map(|m| {
                    m.iter()
                        .map(|m| m["type"].as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default();
            out.push((v["text"].as_str().unwrap_or_default().to_string(), marks));
        }
        if let Some(c) = v["content"].as_array() {
            for x in c {
                walk(x, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

/// The doc's text, its text nodes joined by `|`.
fn text_of(v: &Value) -> String {
    texts(v)
        .into_iter()
        .map(|(t, _)| t)
        .collect::<Vec<_>>()
        .join("|")
}

/// Whether some text node holding `s` carries mark `mark`.
fn marked(v: &Value, s: &str, mark: &str) -> bool {
    texts(v)
        .iter()
        .any(|(t, m)| t.contains(s) && m.iter().any(|x| x == mark))
}

/// Polls `f` every 10 ms on the test task until it holds, at most
/// `within`.
/// Polls `request` until its render thread has started (`renders` counts
/// past its value at the call), then drops it. A fixed timeout instead
/// drops the request before its render starts when the database load is
/// slow (CI), and a test that needs a render in flight then sees none.
/// Returns `Some` only if the request finished before its render started.
async fn drop_once_rendering<T>(
    request: impl std::future::Future<Output = T>,
    renders: &Arc<AtomicUsize>,
) -> Option<T> {
    let before = count(renders);
    tokio::select! {
        r = request => Some(r),
        _ = async {
            while count(renders) <= before {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        } => None,
    }
}

async fn wait_until(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Silences the panics the test hooks raise (each nextest test is a
/// process of its own).
fn quiet_hook_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if !msg.starts_with("test hook") && msg != "x" {
            default(info);
        }
    }));
}

fn session_of(entries: &[Entry], who: usize) -> Entry {
    entries
        .iter()
        .find(|e| e.kind == "session" && e.author == Some(author(who)))
        .unwrap_or_else(|| panic!("no session of {who}: {entries:#?}"))
        .clone()
}

fn of_kind<'a>(entries: &'a [Entry], kind: &str) -> Vec<&'a Entry> {
    entries.iter().filter(|e| e.kind == kind).collect()
}

/// A page where A typed `alpha` and `beta` just now (one open session),
/// built; returns it, A's session and the history to type on.
async fn open_session_page(db: &Db) -> (Uuid, Entry, History) {
    let doc = db.page().await;
    let mut h = History::new(1);
    for t in ["alpha", "beta"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    build_clean(&db.builder(limits()), doc).await;
    let s = session_of(&db.entries(doc).await, 0);
    assert!(!s.sealed);
    (doc, s, h)
}

/// A page whose A typed 2 h ago (a sealed, counted session) and B a minute
/// ago (an open session); returns the page and both sessions.
async fn sealed_and_open(db: &Db) -> (Uuid, Entry, Entry) {
    let (doc, _) = page_with_rows(db, &[(0, "alpha beta", 0), (1, "open one", 119)]).await;
    build_clean(&db.builder(limits()), doc).await;
    let entries = db.entries(doc).await;
    let (a, b) = (session_of(&entries, 0), session_of(&entries, 1));
    assert!(a.sealed && a.words_added.is_some(), "{a:?}");
    assert!(
        !b.sealed && b.words_added.is_none() && !b.words_pending,
        "{b:?}"
    );
    (doc, a, b)
}

/// A page where A, B and C each typed a paragraph just now (three open
/// sessions), built; returns the page and the three entry ids.
async fn three_sessions(db: &Db) -> (Uuid, [i64; 3]) {
    let doc = db.page().await;
    let mut h = History::new(1);
    for (who, t) in [(0, "a text"), (1, "b text"), (2, "c text")] {
        let u = typed(&mut h, who, t);
        db.append(doc, who, &u).await;
    }
    build_clean(&db.builder(limits()), doc).await;
    let entries = db.entries(doc).await;
    let ids = [0, 1, 2].map(|who| session_of(&entries, who).id);
    (doc, ids)
}

/// A legacy page (created a day before `history_since`) with two
/// baselines, "older" and "newer", and a legacy row on top of "older";
/// returns the page, the baseline ids and the newer state.
async fn page_with_baselines(db: &Db) -> (Uuid, [i64; 2], Vec<u8>) {
    let doc = db.page().await;
    db.exec(
        "UPDATE documents SET created_at = history_since - interval '1 day' WHERE id = $1",
        doc,
    )
    .await;
    let (older, _) = with_paragraph(40, &state_of([]), "older");
    let (newer, _) = with_paragraph(41, &older, "newer");
    let mut ids = [0; 2];
    for (i, (seq, state)) in [(1_i64, &older), (2, &newer)].into_iter().enumerate() {
        ids[i] = sqlx::query_scalar(
            "INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
             VALUES ($1, $2, $3, '\\x00', now() - make_interval(hours => 10 - $2::int))
             RETURNING id",
        )
        .bind(doc)
        .bind(seq)
        .bind(state)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    }
    let (_, legacy) = with_paragraph(42, &older, "legacy row");
    let since = db.history_since(doc).await;
    db.insert_row(doc, None, &legacy, None, since - chrono::Duration::hours(1))
        .await;
    build_clean(&db.builder(limits()), doc).await;
    (doc, ids, newer)
}

/// The page's genesis checkpoint: its state and pending revs.
async fn genesis_checkpoint(db: &Db, doc: Uuid) -> (Vec<u8>, Vec<i64>) {
    sqlx::query_as(
        "SELECT state_bytes, pending_revs FROM doc_history_checkpoints
          WHERE doc_id = $1 AND kind = 'genesis'",
    )
    .bind(doc)
    .fetch_one(&db.pool)
    .await
    .expect("the genesis checkpoint")
}

/// pm_json of a KeepDeleted replay of `state` with `rows` applied.
fn replayed(state: &[u8], rows: &[Vec<u8>]) -> Value {
    let mut r = Replayer::from_state(ReplayMode::KeepDeleted, state).expect("a state");
    for row in rows {
        r.apply(row);
    }
    pm_json(r.doc())
}

async fn set_entry(db: &Db, id: i64, set: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE doc_history_entries SET {set} WHERE id = $1"
    )))
    .bind(id)
    .execute(&db.pool)
    .await
    .unwrap_or_else(|e| panic!("{set}: {e}"));
}

/// What a word count the words phase attempted and gave up leaves.
const COUNT_LEFT_NULL: &str = "words_pending = false, words_added = NULL, words_removed = NULL";

// ---------------------------------------------------------------------------
// What a view shows
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_view_is_the_page_at_its_end() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    for (who, t) in [(1, "b early"), (0, "a text"), (1, "b late")] {
        let u = typed(&mut h, who, t);
        db.append(doc, who, &u).await;
    }
    build_clean(&db.builder(limits()), doc).await;
    let a = session_of(&db.entries(doc).await, 0);
    assert_eq!((a.start_rev, a.end_rev), (Some(2), Some(2)));
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let v = views
        .view(anyone(), doc, a.id, false)
        .await
        .expect("a view");
    assert!(!v.changes);
    assert_eq!((v.entry_id, v.kind), (a.id, EntryKind::Session));
    let text = text_of(&json_of(&v));
    assert!(
        text.contains("b early") && text.contains("a text"),
        "{text}"
    );
    assert!(!text.contains("b late"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn show_changes_highlights_only_the_entrys_author() {
    let db = db().await;
    let (doc, _) = page_with_rows(
        &db,
        &[
            (0, "a one", 0),
            (1, "b one", 10),
            (0, "a two", 15),
            (1, "b two", 20),
            (0, "a three", 30),
            (0, "a four", 40),
        ],
    )
    .await;
    build_clean(&db.builder(limits()), doc).await;
    let a = session_of(&db.entries(doc).await, 0);
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let v = views.view(anyone(), doc, a.id, true).await.expect("a view");
    assert!(v.changes);
    let doc_json = json_of(&v);
    for t in ["a one", "a two", "a three", "a four"] {
        assert!(marked(&doc_json, t, "historyInsert"), "{t}: {doc_json}");
    }
    for t in ["b one", "b two"] {
        assert!(text_of(&doc_json).contains(t));
        assert!(!marked(&doc_json, t, "historyInsert"), "{t}: {doc_json}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_entrys_view_includes_its_gap_and_the_entry_below_does_not() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    for (who, t) in [(0, "one"), (0, "two"), (1, "three")] {
        let u = typed(&mut h, who, t);
        db.append(doc, who, &u).await;
    }
    let (with_x, _) = with_paragraph(9, &db.state_at(doc, 2).await, "unlogged x");
    db.snapshot(doc, 2, &with_x).await;
    build_clean(&db.builder(limits()), doc).await;
    let entries = db.entries(doc).await;
    let gaps = of_kind(&entries, "gap");
    assert_eq!(gaps.len(), 1, "{entries:#?}");
    assert_eq!(gaps[0].end_rev, Some(2));
    let a = session_of(&entries, 0);
    assert_eq!(a.end_rev, Some(2));

    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let g = views.view(anyone(), doc, gaps[0].id, true).await.unwrap();
    assert_eq!(g.kind, EntryKind::Gap);
    assert!(g.changes);
    assert!(marked(&json_of(&g), "unlogged x", "historyInsert"));
    for changes in [false, true] {
        let s = views.view(anyone(), doc, a.id, changes).await.unwrap();
        let text = text_of(&json_of(&s));
        assert!(
            text.contains("two") && !text.contains("unlogged x"),
            "{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn baseline_and_rollout_entries_view_their_states_without_changes() {
    let db = db().await;
    let (doc, ids, newer) = page_with_baselines(&db).await;
    let entries = db.entries(doc).await;
    let baseline = entries
        .iter()
        .find(|e| e.baseline_id == Some(ids[1]))
        .expect("the newer baseline's entry");
    let rollout = entries
        .iter()
        .find(|e| e.kind == "baseline" && e.baseline_id.is_none())
        .expect("the rollout entry");
    let views = views_with(&db.pool, &history_permits(1), |_| {});

    let b = views.view(anyone(), doc, baseline.id, true).await.unwrap();
    assert_eq!(b.kind, EntryKind::Baseline);
    assert!(!b.changes);
    assert_eq!(json_of(&b), replayed(&newer, &[]));

    let (genesis, pending) = genesis_checkpoint(&db, doc).await;
    assert!(pending.is_empty());
    let r = views.view(anyone(), doc, rollout.id, true).await.unwrap();
    assert!(!r.changes);
    let r = json_of(&r);
    assert_eq!(r, replayed(&genesis, &[]));
    assert!(text_of(&r).contains("legacy row"), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_rollout_entry_reapplies_genesis_pending_rows() {
    let db = db().await;
    let doc = db.page().await;
    db.exec(
        "UPDATE documents SET created_at = history_since - interval '1 day' WHERE id = $1",
        doc,
    )
    .await;
    let since = db.history_since(doc).await;
    // A visible legacy paragraph, and a row of client 2 typing into a
    // paragraph of client 3 that no row carries: it parks for good.
    let shown = Client::new(4).txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("legacy text"));
    });
    let hidden = Client::new(3).txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("never delivered"));
    });
    let c2 = Client::new(2);
    c2.doc
        .transact_mut()
        .apply_update(Update::decode_v1(&hidden).unwrap())
        .unwrap();
    let parked = c2.txn(|txn, frag| {
        common::fixtures::text_at(frag, txn, 0)
            .expect("the paragraph's text")
            .insert(txn, 1, "parked");
    });
    db.insert_row(doc, None, &shown, None, since - chrono::Duration::hours(1))
        .await;
    let parked_rev = db
        .insert_row(
            doc,
            Some(author(1)),
            &parked,
            Some(UpdateSource::Edit),
            since - chrono::Duration::minutes(30),
        )
        .await;
    build_clean(&db.builder(limits()), doc).await;
    let (genesis, pending) = genesis_checkpoint(&db, doc).await;
    assert_eq!(pending, vec![parked_rev], "the row is pending at genesis");
    let rollout = db
        .entries(doc)
        .await
        .into_iter()
        .find(|e| e.kind == "baseline" && e.baseline_id.is_none())
        .expect("the rollout entry");

    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |_| {});
    let v = views.view(anyone(), doc, rollout.id, false).await.unwrap();
    let v = json_of(&v);
    assert_eq!(v, replayed(&genesis, &[parked]));
    assert!(text_of(&v).contains("legacy text"));
    assert!(!text_of(&v).contains("parked"), "{v}");

    // The size counts the pending rows: a cap of exactly n renders, n − 1
    // does not.
    let n: i64 = sqlx::query_scalar(
        "SELECT (octet_length(c.state_bytes)
                 + (SELECT sum(octet_length(u.update_bytes)) FROM doc_updates u
                     WHERE u.doc_id = c.doc_id AND u.rev = ANY(c.pending_revs)))::bigint
           FROM doc_history_checkpoints c WHERE c.doc_id = $1 AND c.kind = 'genesis'",
    )
    .bind(doc)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(n > genesis.len() as i64);
    let at_cap = |cap: i64| {
        views_with(&db.pool, &permits, move |l| {
            l.max_plain_input_bytes = cap;
            l.max_changes_input_bytes = cap;
        })
    };
    assert!(
        at_cap(n)
            .view(anyone(), doc, rollout.id, false)
            .await
            .is_ok()
    );
    assert!(matches!(
        at_cap(n - 1).view(anyone(), doc, rollout.id, false).await,
        Err(ViewError::TooLarge)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn skipped_rows_are_left_out_of_a_view() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    let mut revs = Vec::new();
    for (who, t) in [(0, "one"), (1, "two"), (2, "three")] {
        let u = typed(&mut h, who, t);
        revs.push(db.append(doc, who, &u).await);
    }
    build_clean(&db.builder(limits()), doc).await;
    let c = session_of(&db.entries(doc).await, 2);
    sqlx::query("UPDATE doc_history_state SET skip_revs = ARRAY[$2] WHERE doc_id = $1")
        .bind(doc)
        .bind(revs[1])
        .execute(&db.pool)
        .await
        .unwrap();
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let v = views.view(anyone(), doc, c.id, false).await.unwrap();
    let text = text_of(&json_of(&v));
    assert!(text.contains("one") && text.contains("three"), "{text}");
    assert!(!text.contains("two"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_sessions_view_follows_its_new_end() {
    let db = db().await;
    let (doc, s, mut h) = open_session_page(&db).await;
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let first = views.view(anyone(), doc, s.id, false).await.unwrap();
    assert!(!text_of(&json_of(&first)).contains("gamma"));

    let u = typed(&mut h, 0, "gamma");
    db.append(doc, 0, &u).await;
    build_clean(&db.builder(limits()), doc).await;
    let moved = session_of(&db.entries(doc).await, 0);
    assert_eq!(moved.id, s.id);
    assert!(moved.updated_at > s.updated_at);
    let next = views.view(anyone(), doc, s.id, false).await.unwrap();
    assert!(!Arc::ptr_eq(&first, &next), "rendered again");
    assert!(text_of(&json_of(&next)).contains("gamma"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_removed_by_a_rebuild_is_not_found() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    views.view(anyone(), doc, s.id, false).await.unwrap();
    db.exec(
        "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
        doc,
    )
    .await;
    build_clean(&db.builder(limits()), doc).await;
    let after = db.entries(doc).await;
    assert!(after.iter().all(|e| e.id != s.id), "{after:#?}");
    assert!(matches!(
        views.view(anyone(), doc, s.id, false).await,
        Err(ViewError::NotFound)
    ));
    assert!(matches!(
        views
            .restore_input(anyone(), doc, s.id, None, |b| Ok(b.len()))
            .await,
        Err(ViewError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_input_is_the_state_at_the_entrys_end() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    for (who, t) in [(1, "b early"), (0, "a text"), (1, "b late")] {
        let u = typed(&mut h, who, t);
        db.append(doc, who, &u).await;
    }
    build_clean(&db.builder(limits()), doc).await;
    let a = session_of(&db.entries(doc).await, 0);
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let view = views.view(anyone(), doc, a.id, false).await.unwrap();
    let r = views
        .restore_input(anyone(), doc, a.id, None, |bytes| {
            let d = Doc::new();
            d.transact_mut()
                .apply_update(Update::decode_v1(bytes).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            Ok((visible_content(&d), pm_json(&d)))
        })
        .await
        .expect("the restore input");
    let (content, pm) = r.value;
    assert_eq!(pm, json_of(&view), "the state the view shows");
    assert_eq!(content, visible(&db.state_at(doc, 2).await));
    assert_eq!(
        (r.kind, r.end_rev, r.ended_at),
        (EntryKind::Session, a.end_rev, a.ended_at)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_render_holds_no_connection() {
    let t = knot_test_support::fresh_db().await;
    let db = Db::on(t.pool.clone()).await;
    let (doc, s, _) = open_session_page(&db).await;
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&t.url)
        .await
        .expect("a one-connection pool");
    let permits = history_permits(1);
    let views = Arc::new(Views::new(one.clone(), permits.clone(), {
        let mut l = ViewLimits::default();
        l.hooks.hold_render = Duration::from_secs(1);
        l
    }));
    let render = {
        let views = views.clone();
        tokio::spawn(async move { views.view(anyone(), doc, s.id, false).await })
    };
    wait_until("the render", Duration::from_secs(5), || {
        permits.available_compute() == 0
    })
    .await;
    let started = Instant::now();
    let page = list_entries(&one, doc, None, 50)
        .await
        .expect("the list")
        .expect("a page");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(!page.entries.is_empty());
    assert!(!render.is_finished(), "the list ran during the render");
    render.await.unwrap().expect("the view");
}

#[test]
fn merge_gap_at_end_rule() {
    assert!(merge_gap_at_end(EntryKind::Gap, &json!({}), 5));
    assert!(merge_gap_at_end(
        EntryKind::Session,
        &json!({"gap_credit_rev": 5}),
        5
    ));
    assert!(!merge_gap_at_end(
        EntryKind::Session,
        &json!({"gap_credit_rev": 4}),
        5
    ));
    assert!(!merge_gap_at_end(EntryKind::Restore, &json!({}), 5));
}

// ---------------------------------------------------------------------------
// What is refused before any replay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_entry_of_another_page_is_not_found() {
    let db = db().await;
    let (_, s, _) = open_session_page(&db).await;
    let other = db.page().await;
    let renders = counter();
    let views = views_with(&db.pool, &history_permits(1), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    assert!(matches!(
        views.view(anyone(), other, s.id, false).await,
        Err(ViewError::NotFound)
    ));
    assert!(matches!(
        views
            .restore_input(anyone(), other, s.id, None, |b| Ok(b.len()))
            .await,
        Err(ViewError::NotFound)
    ));
    assert_eq!(count(&renders), 0);
    assert_eq!(outcome(&rec, "not_found"), 1);
    assert_eq!(outcome(&rec, "failed"), 0);
}

#[tokio::test]
async fn over_the_cap_is_too_large() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.max_plain_input_bytes = 64;
        l.max_changes_input_bytes = 64;
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    for changes in [true, false] {
        assert!(matches!(
            views.view(anyone(), doc, s.id, changes).await,
            Err(ViewError::TooLarge)
        ));
    }
    assert_eq!(count(&renders), 0);
    assert_eq!(outcome(&rec, "too_large"), 2);

    // The size check comes before the permit: no 503.
    let held = permits.compute().await;
    for changes in [true, false] {
        assert!(matches!(
            views.view(anyone(), doc, s.id, changes).await,
            Err(ViewError::TooLarge)
        ));
    }
    drop(held);
    assert_eq!(outcome(&rec, "too_large"), 4);
    assert_eq!(outcome(&rec, "busy"), 0);
}

#[tokio::test]
async fn plain_views_and_restores_use_the_plain_cap() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let (legacy, ids, _) = page_with_baselines(&db).await;
    let baseline = db
        .entries(legacy)
        .await
        .into_iter()
        .find(|e| e.baseline_id == Some(ids[1]))
        .unwrap();
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.max_changes_input_bytes = 64;
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);

    let lowered = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(!lowered.changes);
    assert_eq!(outcome(&rec, "no_changes"), 1);
    let plain = views.view(anyone(), doc, s.id, false).await.unwrap();
    assert_eq!(lowered.doc, plain.doc);
    assert_eq!(count(&renders), 1, "the plain key serves both");
    assert_eq!(outcome(&rec, "cached"), 1);
    assert_eq!(outcome(&rec, "rendered"), 0);
    let r = views
        .restore_input(anyone(), doc, s.id, None, |b| Ok(b.len()))
        .await
        .expect("the restore input");
    assert!(r.value > 0);

    // A baseline's changes=1 view is its plain key, and counts cached.
    views
        .view(anyone(), legacy, baseline.id, false)
        .await
        .unwrap();
    let after = count(&renders);
    let b = views
        .view(anyone(), legacy, baseline.id, true)
        .await
        .unwrap();
    assert!(!b.changes);
    assert_eq!(count(&renders), after);
    assert_eq!(outcome(&rec, "cached"), 2);
    assert_eq!(outcome(&rec, "no_changes"), 1);

    let small = views_with(&db.pool, &permits, |l| {
        l.max_plain_input_bytes = 64;
        l.max_changes_input_bytes = 64;
    });
    for changes in [true, false] {
        assert!(matches!(
            small.view(anyone(), doc, s.id, changes).await,
            Err(ViewError::TooLarge)
        ));
    }
    assert!(matches!(
        small
            .restore_input(anyone(), doc, s.id, None, |b| Ok(b.len()))
            .await,
        Err(ViewError::TooLarge)
    ));
}

/// A Show changes view sizes its input against the diff cap before it
/// waits for a permit, and the load sizes it again: an open session that
/// outgrows the diff cap meanwhile (the builder commits onto it) is
/// rendered plain, not diffed on input up to the plain cap.
#[tokio::test]
async fn a_session_that_outgrows_the_diff_cap_while_waiting_is_shown_plain() {
    let db = db().await;
    let (doc, s, mut h) = open_session_page(&db).await;
    let cap = window_bytes(&db, doc, &s).await;
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.max_changes_input_bytes = cap;
        l.permit_wait = Duration::from_secs(30);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let held = permits.compute().await;
    let (v, ()) = tokio::join!(views.view(anyone(), doc, s.id, true), async {
        wait_until("the view's permit wait", Duration::from_secs(10), || {
            permits.waiting() == 1
        })
        .await;
        let u = typed(&mut h, 0, "gamma");
        db.append(doc, 0, &u).await;
        build_clean(&db.builder(limits()), doc).await;
        let grown = session_of(&db.entries(doc).await, 0);
        assert_eq!(grown.id, s.id);
        assert!(window_bytes(&db, doc, &grown).await > cap, "{grown:?}");
        drop(held);
    });
    let v = v.unwrap();
    assert!(!v.changes, "rendered plain");
    assert!(text_of(&json_of(&v)).contains("gamma"), "the grown session");
    assert_eq!(outcome(&rec, "no_changes"), 1);
    assert_eq!(outcome(&rec, "rendered"), 0);
}

/// A Show changes view the cache holds, or one already found over the
/// diff cap, is served without sizing its input again.
#[tokio::test]
async fn a_repeated_show_changes_view_does_not_size_its_input_again() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let small = views_with(&db.pool, &history_permits(1), |l| {
        l.max_changes_input_bytes = 64;
    });
    let diffed = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(diffed.changes);
    let lowered = small.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(!lowered.changes);

    // Sizing the input now finds no checkpoint (not found); the entry,
    // and so the cache key, is unchanged.
    sqlx::query("DELETE FROM doc_history_checkpoints WHERE doc_id = $1")
        .bind(doc)
        .execute(&db.pool)
        .await
        .unwrap();
    let again = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(Arc::ptr_eq(&again, &diffed));
    let again = small.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(Arc::ptr_eq(&again, &lowered));
}

/// A restore of a version the reader saw end at another time is refused
/// before it waits for a permit or replays; the same check after the load
/// stays the authoritative one.
#[tokio::test]
async fn a_restore_of_a_moved_entry_is_refused_before_the_replay() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.renders = Some(renders.clone());
    });
    let len = |b: &[u8]| Ok(b.len());
    // With the only permit held, a refusal after the permit wait would be
    // Busy: Moved means it was refused before.
    let held = permits.compute().await;
    let seen = s.ended_at - chrono::Duration::seconds(1);
    assert!(matches!(
        views
            .restore_input(anyone(), doc, s.id, Some(seen), len)
            .await,
        Err(ViewError::Moved)
    ));
    assert_eq!(count(&renders), 0);
    drop(held);

    let r = views
        .restore_input(anyone(), doc, s.id, Some(s.ended_at), len)
        .await
        .expect("the version the reader saw");
    assert_eq!(r.ended_at, s.ended_at);
    assert_eq!(count(&renders), 1);
}

/// A restore that passes the early check and then waits for a permit
/// while the builder extends its session is refused on the entry as the
/// load read it, before the replay.
#[tokio::test]
async fn a_restore_of_an_entry_that_moves_while_it_waits_is_refused() {
    let db = db().await;
    let (doc, s, mut h) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.renders = Some(renders.clone());
        l.permit_wait = Duration::from_secs(30);
    });
    let len = |b: &[u8]| Ok(b.len());
    let held = permits.compute().await;
    let (r, ()) = tokio::join!(
        views.restore_input(anyone(), doc, s.id, Some(s.ended_at), len),
        async {
            wait_until("the restore's permit wait", Duration::from_secs(10), || {
                permits.waiting() == 1
            })
            .await;
            let u = typed(&mut h, 0, "gamma");
            db.append(doc, 0, &u).await;
            build_clean(&db.builder(limits()), doc).await;
            let moved = session_of(&db.entries(doc).await, 0);
            assert_eq!(moved.id, s.id);
            assert_ne!(moved.ended_at, s.ended_at);
            drop(held);
        }
    );
    assert!(
        matches!(r, Err(ViewError::Moved)),
        "{:?}",
        r.map(|r| r.ended_at)
    );
    assert_eq!(count(&renders), 0, "no replay");
}

/// A Show changes render that found its input over the diff cap only at
/// the load is filed as the plain view it is: while the cache key stands,
/// a repeat is lowered without sizing again and a plain request shares it.
#[tokio::test]
async fn a_version_that_outgrew_the_diff_cap_is_served_plain_from_then_on() {
    let db = db().await;
    let (doc, s, mut h) = open_session_page(&db).await;
    let cap = window_bytes(&db, doc, &s).await;
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.max_changes_input_bytes = cap;
        l.permit_wait = Duration::from_secs(30);
        l.hooks.renders = Some(renders.clone());
    });
    let updated_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM doc_history_entries WHERE id = $1")
            .bind(s.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let held = permits.compute().await;
    let (v, ()) = tokio::join!(views.view(anyone(), doc, s.id, true), async {
        wait_until("the view's permit wait", Duration::from_secs(10), || {
            permits.waiting() == 1
        })
        .await;
        let u = typed(&mut h, 0, "gamma");
        db.append(doc, 0, &u).await;
        build_clean(&db.builder(limits()), doc).await;
        // The key stands while the input grows, as when the builder drops
        // a checkpoint the window started at.
        sqlx::query("UPDATE doc_history_entries SET updated_at = $2 WHERE id = $1")
            .bind(s.id)
            .bind(updated_at)
            .execute(&db.pool)
            .await
            .unwrap();
        drop(held);
    });
    let v = v.unwrap();
    assert!(!v.changes);
    assert_eq!(outcome(&rec, "no_changes"), 1);

    let again = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(Arc::ptr_eq(&again, &v));
    assert_eq!(outcome(&rec, "no_changes"), 2, "lowered, not a cached diff");
    let plain = views.view(anyone(), doc, s.id, false).await.unwrap();
    assert!(Arc::ptr_eq(&plain, &v));
    assert_eq!(outcome(&rec, "cached"), 1);
    assert_eq!(count(&renders), 1, "one replay for both keys");
}

#[tokio::test]
async fn a_session_the_builder_could_not_count_is_shown_plain() {
    quiet_hook_panics();
    let db = db().await;
    let (doc, sealed, open) = sealed_and_open(&db).await;
    set_entry(&db, sealed.id, COUNT_LEFT_NULL).await;
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.panic_in_diff = true;
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let v = views.view(anyone(), doc, sealed.id, true).await.unwrap();
    assert!(!v.changes);
    assert_eq!(count(&renders), 1);
    assert_eq!(outcome(&rec, "no_changes"), 1);
    assert_eq!(outcome(&rec, "diff_failed"), 0, "the diff was never tried");

    // An open session has NULL counts too, and is highlighted.
    let plain = views_with(&db.pool, &permits, |_| {});
    let o = plain.view(anyone(), doc, open.id, true).await.unwrap();
    assert!(o.changes);
    assert!(marked(&json_of(&o), "open one", "historyInsert"));
}

#[tokio::test]
async fn a_session_being_counted_is_not_shown_plain() {
    let db = db().await;
    let (doc, sealed, _) = sealed_and_open(&db).await;
    set_entry(&db, sealed.id, COUNT_LEFT_NULL).await;
    db.exec(
        "UPDATE doc_history_state
            SET claimed_by = gen_random_uuid(), claimed_until = now() + interval '5 minutes'
          WHERE doc_id = $1",
        doc,
    )
    .await;
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let v = views.view(anyone(), doc, sealed.id, true).await.unwrap();
    assert!(v.changes, "a count in progress");
    assert_eq!(outcome(&rec, "rendered"), 1);
    assert_eq!(outcome(&rec, "no_changes"), 0);

    db.exec(
        "UPDATE doc_history_state SET claimed_until = now() - interval '1 second'
          WHERE doc_id = $1",
        doc,
    )
    .await;
    let v = views.view(anyone(), doc, sealed.id, true).await.unwrap();
    assert!(!v.changes, "the count failed");
    assert_eq!(outcome(&rec, "no_changes"), 1);
}

#[tokio::test]
async fn a_skipped_baseline_is_not_replayed() {
    let db = db().await;
    let (doc, ids, _) = page_with_baselines(&db).await;
    let entries = db.entries(doc).await;
    let baseline = entries
        .iter()
        .find(|e| e.baseline_id == Some(ids[1]))
        .unwrap();
    let rollout = entries
        .iter()
        .find(|e| e.kind == "baseline" && e.baseline_id.is_none())
        .unwrap();
    sqlx::query("UPDATE doc_history_state SET skip_baselines = ARRAY[$2] WHERE doc_id = $1")
        .bind(doc)
        .bind(ids[1])
        .execute(&db.pool)
        .await
        .unwrap();
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    assert!(matches!(
        views.view(anyone(), doc, baseline.id, false).await,
        Err(ViewError::Failed(_))
    ));
    assert!(matches!(
        views
            .restore_input(anyone(), doc, baseline.id, None, |b| Ok(b.len()))
            .await,
        Err(ViewError::Failed(_))
    ));
    assert_eq!(count(&renders), 0);
    assert_eq!(permits.available_compute(), 1);
    assert_eq!(outcome(&rec, "failed"), 1);

    assert!(views.view(anyone(), doc, rollout.id, false).await.is_ok());
}

#[tokio::test]
async fn a_page_an_older_builder_built_refuses_views_and_restores() {
    let db = db().await;
    let (doc, sealed, _) = sealed_and_open(&db).await;
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    views.view(anyone(), doc, sealed.id, false).await.unwrap();
    assert_eq!(count(&renders), 1);

    let refused = async |round: u64| {
        for changes in [false, true] {
            assert!(matches!(
                views.view(anyone(), doc, sealed.id, changes).await,
                Err(ViewError::Rebuilding)
            ));
        }
        assert!(matches!(
            views
                .restore_input(anyone(), doc, sealed.id, None, |b| Ok(b.len()))
                .await,
            Err(ViewError::Rebuilding)
        ));
        assert_eq!(count(&renders), 1, "the cached render is not served");
        assert_eq!(permits.available_compute(), 1);
        assert_eq!(outcome(&rec, "rebuilding"), 2 * round);
        assert_eq!(outcome(&rec, "cached"), 0);
    };
    sqlx::query("UPDATE doc_history_state SET builder_version = $2 WHERE doc_id = $1")
        .bind(doc)
        .bind(BUILDER_VERSION - 1)
        .execute(&db.pool)
        .await
        .unwrap();
    refused(1).await;
    db.exec(
        "UPDATE doc_history_state SET retry_after = 'infinity' WHERE doc_id = $1",
        doc,
    )
    .await;
    refused(2).await;

    sqlx::query(
        "UPDATE doc_history_state SET builder_version = $2, retry_after = NULL WHERE doc_id = $1",
    )
    .bind(doc)
    .bind(BUILDER_VERSION)
    .execute(&db.pool)
    .await
    .unwrap();
    views.view(anyone(), doc, sealed.id, false).await.unwrap();
    assert_eq!(count(&renders), 1);
    assert_eq!(outcome(&rec, "cached"), 1);
}

#[tokio::test]
async fn a_busy_permit_is_busy_after_the_wait() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.permit_wait = Duration::from_millis(200);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let _held = permits.compute().await;
    let started = Instant::now();
    assert!(matches!(
        views.view(anyone(), doc, s.id, false).await,
        Err(ViewError::Busy)
    ));
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(200) && took < Duration::from_secs(1),
        "{took:?}"
    );
    assert_eq!(outcome(&rec, "busy"), 1);
    assert_eq!(view_seconds(&rec), 0);
}

// ---------------------------------------------------------------------------
// Renders that fail, hang or are dropped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_diff_that_fails_falls_back_to_the_plain_view() {
    quiet_hook_panics();
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let views = views_with(&db.pool, &history_permits(1), |l| {
        l.hooks.panic_in_diff = true;
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let with = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(!with.changes);
    assert_eq!(outcome(&rec, "diff_failed"), 1);
    assert_eq!(outcome(&rec, "rendered"), 0);
    let without = views.view(anyone(), doc, s.id, false).await.unwrap();
    assert_eq!(with.doc, without.doc);
    assert_eq!(outcome(&rec, "rendered"), 1);
}

#[tokio::test]
async fn a_dropped_view_request_cancels_its_render() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let permits = history_permits(1);
    let renders = counter();
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(1);
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let dropped = drop_once_rendering(views.view(anyone(), doc, s.id, false), &renders).await;
    assert!(dropped.is_none(), "the request was dropped");
    assert_eq!(permits.stalled(), 1);
    assert_eq!(stalled_gauge(&rec), Some(1.0));
    assert_eq!(
        permits.available_compute(),
        0,
        "the thread keeps its permit"
    );

    wait_until("the thread's end", Duration::from_secs(2), || {
        permits.stalled() == 0
            && permits.available_compute() == 1
            && stalled_gauge(&rec) == Some(0.0)
    })
    .await;
    let started = Instant::now();
    let unheld = views_with(&db.pool, &permits, |_| {});
    unheld.view(anyone(), doc, s.id, false).await.unwrap();
    assert!(started.elapsed() < VIEW_PERMIT_WAIT);
    assert_eq!(view_timeouts(&rec), 0, "a dropped request is not a timeout");
}

#[tokio::test]
async fn a_dropped_render_that_outlives_the_timeout_is_not_replayed() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(2);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_in_diff = Duration::from_millis(1500);
        l.hooks.renders = Some(renders.clone());
        l.timeout = Duration::from_millis(300);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let dropped = drop_once_rendering(views.view(anyone(), doc, s.id, true), &renders).await;
    assert!(dropped.is_none());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(view_timeouts(&rec), 1, "the watcher recorded it");
    assert_eq!(permits.stalled(), 1);

    let started = Instant::now();
    let v = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!v.changes);
    assert_eq!(outcome(&rec, "no_changes"), 1);
    assert_eq!(
        count(&renders),
        2,
        "the dropped diff render and the plain one"
    );

    wait_until("the first thread's end", Duration::from_secs(3), || {
        permits.stalled() == 0 && stalled_gauge(&rec) == Some(0.0)
    })
    .await;
}

#[tokio::test]
async fn a_render_that_panics_fails_and_one_that_hangs_times_out_and_keeps_its_permit() {
    quiet_hook_panics();
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(1);
    let panicking = views_with(&db.pool, &permits, |l| {
        l.hooks.panic_render = true;
        l.hooks.renders = Some(renders.clone());
    });
    let hanging = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.timeout = Duration::from_millis(300);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    assert!(matches!(
        panicking.view(anyone(), doc, s.id, false).await,
        Err(ViewError::Failed(_))
    ));
    assert_eq!(outcome(&rec, "failed"), 1);

    assert!(matches!(
        hanging.view(anyone(), doc, s.id, false).await,
        Err(ViewError::TimedOut)
    ));
    assert_eq!(outcome(&rec, "timeout"), 1);
    assert_eq!(view_timeouts(&rec), 1);
    assert_eq!(stalled_gauge(&rec), Some(1.0));
    assert_eq!(permits.available_compute(), 0);
    assert_eq!(permits.stalled(), 1);
    assert_eq!(view_seconds(&rec), 0);
    wait_until("the thread's end", Duration::from_secs(3), || {
        permits.stalled() == 0 && permits.available_compute() == 1
    })
    .await;

    let before = count(&renders);
    for changes in [false, true] {
        assert!(matches!(
            hanging.view(anyone(), doc, s.id, changes).await,
            Err(ViewError::Failed(_))
        ));
    }
    assert!(matches!(
        hanging
            .restore_input(anyone(), doc, s.id, None, |b| Ok(b.len()))
            .await,
        Err(ViewError::Failed(_))
    ));
    assert_eq!(count(&renders), before);
    assert_eq!(permits.available_compute(), 1);
    assert_eq!(outcome(&rec, "failed"), 3);
    assert_eq!(view_seconds(&rec), 0);
}

/// Views on two workers whose renders hang 2 s and time out at 300 ms.
fn hanging_views(db: &Db, permits: &HistoryPermits, renders: &Arc<AtomicUsize>) -> Views {
    views_with(&db.pool, permits, |l| {
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.timeout = Duration::from_millis(300);
    })
}

#[tokio::test]
async fn a_plain_timeout_blocks_the_show_changes_view_too() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(2);
    let views = hanging_views(&db, &permits, &renders);
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    assert!(matches!(
        views.view(anyone(), doc, s.id, false).await,
        Err(ViewError::TimedOut)
    ));
    let before = count(&renders);
    assert!(matches!(
        views.view(anyone(), doc, s.id, true).await,
        Err(ViewError::Failed(_))
    ));
    assert_eq!(count(&renders), before);
    assert_eq!(permits.available_compute(), 1, "no second thread");
    assert_eq!(outcome(&rec, "failed"), 1);
    assert_eq!(outcome(&rec, "no_changes"), 0);
}

#[tokio::test]
async fn a_render_that_hangs_before_the_diff_blocks_both_variants() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(2);
    let views = hanging_views(&db, &permits, &renders);
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    assert!(matches!(
        views.view(anyone(), doc, s.id, true).await,
        Err(ViewError::TimedOut)
    ));
    assert_eq!(view_timeouts(&rec), 1);
    let before = count(&renders);
    for changes in [true, false] {
        assert!(matches!(
            views.view(anyone(), doc, s.id, changes).await,
            Err(ViewError::Failed(_))
        ));
    }
    assert_eq!(count(&renders), before);
    assert_eq!(permits.available_compute(), 1);
    assert_eq!(outcome(&rec, "no_changes"), 0);
}

#[tokio::test]
async fn a_diff_that_hangs_is_not_tried_again() {
    let db = db().await;
    let (doc, s, mut h) = open_session_page(&db).await;
    let permits = history_permits(2);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_in_diff = Duration::from_secs(2);
        l.timeout = Duration::from_millis(300);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    assert!(matches!(
        views.view(anyone(), doc, s.id, true).await,
        Err(ViewError::TimedOut)
    ));
    assert_eq!(view_timeouts(&rec), 1);

    let started = Instant::now();
    let v = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!v.changes, "the plain render on the second permit");
    assert_eq!(outcome(&rec, "no_changes"), 1);
    assert_eq!(view_timeouts(&rec), 1);

    wait_until("the diff thread's end", Duration::from_secs(3), || {
        permits.available_compute() == 2
    })
    .await;
    let u = typed(&mut h, 0, "gamma");
    db.append(doc, 0, &u).await;
    build_clean(&db.builder(limits()), doc).await;
    assert!(session_of(&db.entries(doc).await, 0).updated_at > s.updated_at);
    assert!(matches!(
        views.view(anyone(), doc, s.id, true).await,
        Err(ViewError::TimedOut)
    ));
    assert_eq!(
        view_timeouts(&rec),
        2,
        "the extended session tried the diff"
    );
}

#[tokio::test]
async fn a_timed_out_version_stays_blocked_while_its_thread_runs() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(2);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.timeout = Duration::from_millis(300);
        l.cache_ttl = Duration::from_millis(200);
    });
    let t0 = Instant::now();
    assert!(matches!(
        views.view(anyone(), doc, s.id, false).await,
        Err(ViewError::TimedOut)
    ));
    tokio::time::sleep_until(t0 + Duration::from_millis(700)).await;
    let before = count(&renders);
    assert!(matches!(
        views.view(anyone(), doc, s.id, false).await,
        Err(ViewError::Failed(_))
    ));
    assert_eq!(count(&renders), before);
    assert_eq!(permits.available_compute(), 1);

    wait_until("the thread's end", Duration::from_secs(3), || {
        permits.stalled() == 0
    })
    .await;
    assert!(
        matches!(
            views.view(anyone(), doc, s.id, false).await,
            Err(ViewError::Failed(_))
        ),
        "recorded afresh at the thread's end"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(matches!(
        views.view(anyone(), doc, s.id, false).await,
        Err(ViewError::TimedOut)
    ));
    assert_eq!(count(&renders), before + 1);
}

// ---------------------------------------------------------------------------
// Render slots and the cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_principal_renders_one_version_at_a_time() {
    let db = db().await;
    let (doc, [e1, e2, e3]) = three_sessions(&db).await;
    let (a, b) = (anyone(), anyone());
    let renders = counter();
    let permits = history_permits(2);
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);

    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(1);
        l.hooks.renders = Some(renders.clone());
        l.permit_wait = Duration::from_millis(200);
    });
    let (r1, r2, r3) = tokio::join!(
        views.view(a, doc, e1, false),
        views.view(a, doc, e2, false),
        views.view(b, doc, e3, false),
    );
    assert!(r3.is_ok(), "{:?}", r3.err());
    let busy = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(ViewError::Busy)))
        .count();
    let ok = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    assert_eq!((ok, busy), (1, 1), "{:?} {:?}", r1.err(), r2.err());
    assert_eq!(count(&renders), 2, "it waited on A's slot, not a permit");
    assert_eq!(outcome(&rec, "busy"), 1);

    // The slot is held by the render's thread, not by the request.
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(1);
        l.hooks.renders = Some(renders.clone());
        l.permit_wait = Duration::from_secs(2);
    });
    let dropped = drop_once_rendering(views.view(a, doc, e2, false), &renders).await;
    assert!(dropped.is_none());
    let at_drop = Instant::now();
    let held = permits.available_compute();
    assert_eq!(held, 1, "the dropped render runs on");
    views
        .view(a, doc, e3, true)
        .await
        .expect("after the thread");
    // It waited for the dropped render's thread (>= 0.8 s from the drop),
    // then rendered its own 1 s; without the wait, only the second.
    assert!(
        at_drop.elapsed() >= Duration::from_millis(1700),
        "{:?}",
        at_drop.elapsed()
    );

    // A render that times out frees its slot at the deadline.
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.timeout = Duration::from_millis(300);
    });
    assert!(matches!(
        views.view(a, doc, e1, true).await,
        Err(ViewError::TimedOut)
    ));
    let before = count(&renders);
    let started = Instant::now();
    assert!(matches!(
        views.view(a, doc, e2, true).await,
        Err(ViewError::TimedOut)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(count(&renders), before + 1);
}

#[tokio::test]
async fn a_caller_never_waits_on_another_principals_slot() {
    let db = db().await;
    let (doc, [e1, e2, e3]) = three_sessions(&db).await;
    let (a, b) = (anyone(), anyone());
    let renders = counter();
    let permits = history_permits(2);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(1);
        l.hooks.renders = Some(renders.clone());
        l.permit_wait = Duration::from_millis(200);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let (r1, (a2, b2)) = tokio::join!(views.view(a, doc, e1, false), async {
        wait_until("A's e1 render", Duration::from_secs(5), || {
            permits.available_compute() == 1
        })
        .await;
        tokio::join!(views.view(a, doc, e2, false), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            views.view(b, doc, e2, false).await
        })
    });
    assert!(r1.is_ok(), "{:?}", r1.err());
    assert!(matches!(a2, Err(ViewError::Busy)), "{:?}", a2.err());
    assert!(b2.is_ok(), "{:?}", b2.err());
    assert_eq!(count(&renders), 2);
    assert_eq!(outcome(&rec, "busy"), 1);
    assert_eq!(outcome(&rec, "rendered"), 2);

    // A hit takes no slot.
    let (r3, took) = tokio::join!(views.view(a, doc, e3, false), async {
        wait_until("A's e3 render", Duration::from_secs(5), || {
            permits.available_compute() == 1
        })
        .await;
        let t = Instant::now();
        views.view(a, doc, e2, false).await.expect("the cached e2");
        t.elapsed()
    });
    assert!(r3.is_ok());
    assert!(took < Duration::from_millis(100), "{took:?}");
    assert_eq!(outcome(&rec, "cached"), 1);
}

#[tokio::test]
async fn the_cache_serves_a_repeat() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let views = views_with(&db.pool, &history_permits(1), |l| {
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let first = views.view(anyone(), doc, s.id, true).await.unwrap();
    let second = views.view(anyone(), doc, s.id, true).await.unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(count(&renders), 1);
    assert_eq!(outcome(&rec, "rendered"), 1);
    assert_eq!(outcome(&rec, "cached"), 1);
    assert_eq!(view_seconds(&rec), 1);
}

#[tokio::test]
async fn concurrent_misses_render_once() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let views = views_with(&db.pool, &history_permits(2), |l| {
        l.hooks.hold_render = Duration::from_millis(300);
        l.hooks.renders = Some(renders.clone());
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let (x, y) = tokio::join!(
        views.view(anyone(), doc, s.id, false),
        views.view(anyone(), doc, s.id, false),
    );
    assert!(Arc::ptr_eq(&x.unwrap(), &y.unwrap()));
    assert_eq!(count(&renders), 1);
    assert_eq!(outcome(&rec, "rendered"), 1);
    assert_eq!(outcome(&rec, "cached"), 1);
    assert_eq!(view_seconds(&rec), 1);
}

#[tokio::test]
async fn joined_callers_count_one_timeout() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(2);
    let views = hanging_views(&db, &permits, &renders);
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let (x, y) = tokio::join!(
        views.view(anyone(), doc, s.id, false),
        views.view(anyone(), doc, s.id, false),
    );
    assert!(matches!(x, Err(ViewError::TimedOut)));
    assert!(matches!(y, Err(ViewError::TimedOut)));
    assert_eq!(count(&renders), 1);
    assert_eq!(outcome(&rec, "timeout"), 2);
    assert_eq!(view_timeouts(&rec), 1);
    assert_eq!(permits.stalled(), 1);
}

#[tokio::test]
async fn a_joined_caller_renders_after_the_first_is_dropped() {
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(1);
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_millis(300);
        l.hooks.renders = Some(renders.clone());
        l.permit_wait = Duration::from_secs(2);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let (first, second) = tokio::join!(
        drop_once_rendering(views.view(anyone(), doc, s.id, false), &renders),
        views.view(anyone(), doc, s.id, false),
    );
    assert!(first.is_none(), "the first was dropped");
    assert!(second.is_ok(), "{:?}", second.err());
    assert_eq!(count(&renders), 2);
    assert_eq!(outcome(&rec, "rendered"), 1);
    assert_eq!(views_counted(&rec), 1, "the dropped call records nothing");
    wait_until("both threads' end", Duration::from_secs(3), || {
        permits.stalled() == 0 && stalled_gauge(&rec) == Some(0.0)
    })
    .await;
}

#[tokio::test]
async fn restore_input_is_bounded_like_a_plain_view() {
    quiet_hook_panics();
    let db = db().await;
    let (doc, s, _) = open_session_page(&db).await;
    let renders = counter();
    let permits = history_permits(2);
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let len = |b: &[u8]| Ok(b.len());

    let small = views_with(&db.pool, &permits, |l| l.max_plain_input_bytes = 64);
    assert!(matches!(
        small.restore_input(anyone(), doc, s.id, None, len).await,
        Err(ViewError::TooLarge)
    ));

    let views = views_with(&db.pool, &permits, |l| {
        l.permit_wait = Duration::from_millis(200);
        l.restore_timeout = Duration::from_millis(300);
        l.hooks.renders = Some(renders.clone());
    });
    let held = (permits.compute().await, permits.compute().await);
    assert!(matches!(
        views.restore_input(anyone(), doc, s.id, None, len).await,
        Err(ViewError::Busy)
    ));
    drop(held);
    match views
        .restore_input(anyone(), doc, s.id, None, |_| Err::<(), _>("x".to_string()))
        .await
    {
        Err(ViewError::Failed(m)) => assert_eq!(m, "x"),
        other => panic!("{:?}", other.err()),
    }
    assert!(matches!(
        views
            .restore_input(anyone(), doc, s.id, None, |_| -> Result<(), String> {
                panic!("x")
            })
            .await,
        Err(ViewError::Failed(_))
    ));
    assert!(matches!(
        views
            .restore_input(anyone(), doc, s.id, None, |_| {
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            })
            .await,
        Err(ViewError::TimedOut)
    ));
    assert_eq!(view_timeouts(&rec), 1);
    let before = count(&renders);
    assert!(matches!(
        views.restore_input(anyone(), doc, s.id, None, len).await,
        Err(ViewError::Failed(_))
    ));
    assert_eq!(count(&renders), before, "no replay");
    assert_eq!(views_counted(&rec), 0, "restores are not views");

    // The restore key does not block views.
    views.view(anyone(), doc, s.id, false).await.unwrap();
    assert_eq!(count(&renders), before + 1);
    assert_eq!(outcome(&rec, "rendered"), 1);
    assert_eq!(views_counted(&rec), 1);
}

// ---------------------------------------------------------------------------
// Baselines that may be shifted, and pages lifted from version 1 (Task 8)
// ---------------------------------------------------------------------------

/// yrs's own full state of a doc that applied `updates` in order: what
/// every room and every baseline held before the knot-crdt fix, shifted
/// when the doc's pending update holds overlapping Skips.
fn yrs_full_state(updates: &[&[u8]]) -> Vec<u8> {
    use yrs::ReadTxn;
    let doc = Doc::new();
    for u in updates {
        doc.transact_mut()
            .apply_update(Update::decode_v1(u).unwrap())
            .unwrap();
    }
    doc.transact()
        .encode_state_as_update_v1(&yrs::StateVector::default())
}

/// Whether every id of `a` is in `b`.
fn within(a: &yrs::IdSet, b: &yrs::IdSet) -> bool {
    !a.diff(b).iter().any(|(_, r)| !r.is_empty())
}

fn insertions_of_all(updates: &[&[u8]]) -> yrs::IdSet {
    updates
        .iter()
        .fold(yrs::IdSet::new(), |acc, u| acc.merge(&insertions(u)))
}

/// pm_json of a fresh doc that applied `state`.
fn applied(state: &[u8]) -> Value {
    let d = Doc::new();
    d.transact_mut()
        .apply_update(Update::decode_v1(state).expect("the state decodes"))
        .expect("the state applies");
    pm_json(&d)
}

/// A legacy page (created a day before `history_since`) whose baseline is
/// `baseline` and whose legacy rows are `rows`, an hour before
/// `history_since`, a minute apart.
async fn legacy_page_with_baseline(db: &Db, baseline: &[u8], rows: &[&[u8]]) -> Uuid {
    let doc = db.page().await;
    db.exec(
        "UPDATE documents SET created_at = history_since - interval '1 day' WHERE id = $1",
        doc,
    )
    .await;
    let since = db.history_since(doc).await;
    sqlx::query(
        "INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
         VALUES ($1, 3, $2, '\\x00', $3)",
    )
    .bind(doc)
    .bind(baseline)
    .bind(since - chrono::Duration::hours(2))
    .execute(&db.pool)
    .await
    .unwrap();
    for (i, u) in rows.iter().enumerate() {
        db.insert_row(
            doc,
            None,
            u,
            None,
            since - chrono::Duration::minutes(60 - i as i64),
        )
        .await;
    }
    doc
}

/// [`legacy_page_with_baseline`] as the knot-crdt fix's builder tests
/// build it: the baseline is yrs's shifted full state after the fixture's
/// `t0`, `t2` and `t3`, which are its legacy rows; returns the page, the
/// baseline and the fixture.
async fn page_with_a_shifted_baseline(
    db: &Db,
) -> (Uuid, Vec<u8>, common::fixtures::OverlappingSkips) {
    let o = common::fixtures::overlapping_skips();
    let parked: [&[u8]; 3] = [&o.t0, &o.t2, &o.t3];
    let baseline = yrs_full_state(&parked);
    let doc = legacy_page_with_baseline(db, &baseline, &parked).await;
    (doc, baseline, o)
}

/// A transaction of client 3 on a doc holding `before`: `text` typed at
/// `index` of paragraph 0. Unlike the builder test's helper, each edit
/// continues client 3's clock (pass its earlier edits in `before`), so
/// the second one is a real row, not a repeat of the first one's ids.
fn edit_by_client_3(before: &[&[u8]], index: u32, text: &str) -> Vec<u8> {
    let c = Doc::with_client_id(3);
    for u in before {
        c.transact_mut()
            .apply_update(Update::decode_v1(u).unwrap())
            .unwrap();
    }
    let frag = c.get_or_insert_xml_fragment("default");
    let mut txn = c.transact_mut();
    common::fixtures::text_at(&frag, &txn, 0)
        .expect("paragraph 0")
        .insert(&mut txn, index, text);
    txn.encode_update_v1()
}

#[test]
fn state_input_cuts_a_shifted_state_to_its_prefix() {
    use knot_history::replay::{may_be_shifted, trusted_prefix};
    let o = common::fixtures::overlapping_skips();
    let shifted = yrs_full_state(&[&o.t0, &o.t2, &o.t3]);
    assert!(
        may_be_shifted(&shifted),
        "fixture: the state may be shifted"
    );
    assert_eq!(
        state_input(&shifted).unwrap(),
        trusted_prefix(&shifted).unwrap()
    );

    let whole = yrs_full_state(&[&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3]);
    assert!(!may_be_shifted(&whole));
    assert_eq!(state_input(&whole).unwrap(), whole);
    let (typed, _) = with_paragraph(5, &state_of([]), "plain");
    assert_eq!(state_input(&typed).unwrap(), typed);
}

/// The plan's baseline (the fix's overlapping-Skips state) renders and
/// restores the same from its raw bytes as from its prefix: a fresh doc
/// parks every block past the gap, and the restore's encoding leaves
/// pending content out. So the test also takes the fix's shifted state
/// that parks nothing, whose raw bytes show the phantoms as text.
#[tokio::test(flavor = "multi_thread")]
async fn a_baseline_that_may_be_shifted_views_and_restores_from_its_prefix() {
    use knot_history::replay::{may_be_shifted, trusted_prefix};
    let db = db().await;
    let (overlapping, overlapping_baseline, _) = page_with_a_shifted_baseline(&db).await;
    let parks_nothing = common::fixtures::shifted_state_that_parks_nothing();
    let row = common::fixtures::para(9, "legacy row");
    let phantoms = legacy_page_with_baseline(&db, &parks_nothing, &[&row]).await;
    assert_ne!(
        replayed(&parks_nothing, &[]),
        replayed(&trusted_prefix(&parks_nothing).unwrap(), &[]),
        "fixture: the raw bytes show what the prefix leaves out"
    );
    let views = views_with(&db.pool, &history_permits(1), |_| {});

    for (doc, baseline) in [
        (overlapping, overlapping_baseline),
        (phantoms, parks_nothing),
    ] {
        assert!(
            may_be_shifted(&baseline),
            "fixture: the baseline may be shifted"
        );
        let prefix = trusted_prefix(&baseline).expect("the prefix");
        build_clean(&db.builder(limits()), doc).await;
        let entry = db
            .entries(doc)
            .await
            .into_iter()
            .find(|e| e.kind == "baseline" && e.baseline_id.is_some())
            .expect("the baseline's entry");

        let v = views.view(anyone(), doc, entry.id, false).await.unwrap();
        assert_eq!(json_of(&v), replayed(&prefix, &[]));
        let r = views
            .restore_input(anyone(), doc, entry.id, None, |bytes| Ok(bytes.to_vec()))
            .await
            .expect("the restore input");
        let held = insertions(&r.value);
        assert!(
            within(&held, &insertions(&prefix)),
            "the restore names {:?}",
            held.diff(&insertions(&prefix))
        );
    }
}

/// A page lifted from builder version 1: the fix's shifted baseline with
/// its legacy rows `t0`, `t2`, `t3`, two rows of client 3 (rows 4 and 5)
/// and a fixed room's snapshot at rev 5 holding G, built, then given the
/// gap checkpoint 0.9.14 wrote at rev 4 and marked as built by version 1.
struct LiftedPage {
    doc: Uuid,
    builder: Arc<knot_history::Builder>,
    /// The page's five rows.
    rows: Vec<Vec<u8>>,
    /// The gap content only the room's snapshot holds.
    g: Vec<u8>,
    baseline: Vec<u8>,
}

impl LiftedPage {
    /// Every id a restore of the page may name: the rows', the baseline's
    /// trusted prefix's and G's.
    fn allowed(&self) -> yrs::IdSet {
        let rows: Vec<&[u8]> = self.rows.iter().map(Vec::as_slice).collect();
        insertions_of_all(&rows)
            .merge(&insertions(
                &knot_history::replay::trusted_prefix(&self.baseline).expect("the prefix"),
            ))
            .merge(&insertions(&self.g))
    }

    /// The room's state at rev 5: the rows plus G.
    fn room(&self) -> Vec<u8> {
        state_of(
            self.rows
                .iter()
                .map(Vec::as_slice)
                .chain([self.g.as_slice()]),
        )
    }
}

async fn page_lifted_from_version_1(db: &Db) -> LiftedPage {
    let b = db.builder(limits());
    let (doc, baseline, o) = page_with_a_shifted_baseline(db).await;
    let e4 = edit_by_client_3(&[&o.t0], 2, "four");
    let e5 = edit_by_client_3(&[&o.t0, &e4], 1, "five");
    assert_eq!(db.append(doc, 2, &e4).await, 4);
    assert_eq!(db.append(doc, 2, &e5).await, 5);
    let g = common::fixtures::para(9, "gap text");
    let rows: [&[u8]; 5] = [&o.t0, &o.t2, &o.t3, &e4, &e5];
    // A fixed room's snapshot at rev 5: the rows plus G.
    let room: Vec<&[u8]> = rows.iter().copied().chain([g.as_slice()]).collect();
    db.snapshot(doc, 5, &state_of(room)).await;
    build_clean(&b, doc).await;

    // What 0.9.14 wrote at rev 4: the baseline's raw bytes, rows 1-4 and G.
    let v1_gap = {
        let mut r = Replayer::new(ReplayMode::Gc);
        r.merge_state(&baseline);
        for u in &rows[..4] {
            r.apply(u);
        }
        r.merge_state(&g);
        r.encode_state()
    };
    let sv = Replayer::from_state(ReplayMode::Gc, &v1_gap)
        .expect("the state applies")
        .state_vector();
    sqlx::query(
        "INSERT INTO doc_history_checkpoints
             (doc_id, rev, kind, state_bytes, state_vector, pending_revs, created_at)
         VALUES ($1, 4, 'gap', $2, $3, '{}', now() - interval '1 day')
         ON CONFLICT (doc_id, rev) DO UPDATE
            SET kind = 'gap', state_bytes = EXCLUDED.state_bytes,
                state_vector = EXCLUDED.state_vector, pending_revs = '{}',
                created_at = EXCLUDED.created_at",
    )
    .bind(doc)
    .bind(&v1_gap)
    .bind(sv)
    .execute(&db.pool)
    .await
    .unwrap();
    db.exec(
        "UPDATE doc_history_state SET builder_version = 1, rebuilds = 2 WHERE doc_id = $1",
        doc,
    )
    .await;
    LiftedPage {
        doc,
        builder: b,
        rows: rows.iter().map(|r| r.to_vec()).collect(),
        g,
        baseline,
    }
}

/// The rebuilt lifted page's entries, after asserting the rebuild made the
/// session over rows 4-5 and the gap entry at rev 5.
async fn rebuilt(db: &Db, p: &LiftedPage) -> Vec<Entry> {
    build_clean(&p.builder, p.doc).await;
    assert_eq!(db.state(p.doc).await.builder_version, BUILDER_VERSION);
    let entries = db.entries(p.doc).await;
    assert!(
        entries.iter().any(|e| e.kind == "session"
            && e.start_rev.is_some_and(|s| s <= 4)
            && e.end_rev.is_some_and(|e| e >= 5)),
        "no session covers rows 4-5: {entries:#?}"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.kind == "gap" && e.end_rev == Some(5)),
        "no gap entry at rev 5: {entries:#?}"
    );
    entries
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_lifted_from_version_1_views_no_phantom() {
    let db = db().await;
    let p = page_lifted_from_version_1(&db).await;
    let doc = p.doc;

    // Version 1's entries: refused until the rebuild.
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let before = db.entries(doc).await;
    assert!(!before.is_empty());
    for e in &before {
        assert!(
            matches!(
                views.view(anyone(), doc, e.id, false).await,
                Err(ViewError::Rebuilding)
            ),
            "{e:?}"
        );
        assert!(
            matches!(
                views
                    .restore_input(anyone(), doc, e.id, None, |b| Ok(b.len()))
                    .await,
                Err(ViewError::Rebuilding)
            ),
            "{e:?}"
        );
    }

    let entries = rebuilt(&db, &p).await;
    let allowed = p.allowed();
    for e in &entries {
        let r = views
            .restore_input(anyone(), doc, e.id, None, |bytes| Ok(bytes.to_vec()))
            .await
            .unwrap_or_else(|err| panic!("{e:?}: {err:?}"));
        let held = insertions(&r.value);
        assert!(
            within(&held, &allowed),
            "the {} entry {:?}..{:?} restores {:?}",
            e.kind,
            e.start_rev,
            e.end_rev,
            held.diff(&allowed)
        );
        let v = views.view(anyone(), doc, e.id, false).await.unwrap();
        assert_eq!(json_of(&v), applied(&r.value), "{e:?}");
    }
}

/// The replay input of a session's window, as the views sum it: the
/// latest checkpoint at or before `start_rev − 1`, the gap checkpoints
/// after it and the rows (and its pending rows) through `end_rev`.
async fn window_bytes(db: &Db, doc: Uuid, e: &Entry) -> i64 {
    sqlx::query_scalar(
        "WITH cp AS (
             SELECT rev, octet_length(state_bytes)::bigint AS size, pending_revs
               FROM doc_history_checkpoints WHERE doc_id = $1 AND rev <= $2 - 1
              ORDER BY rev DESC LIMIT 1)
         SELECT (cp.size
                 + coalesce((SELECT sum(octet_length(c.state_bytes)) FROM doc_history_checkpoints c
                              WHERE c.doc_id = $1 AND c.kind = 'gap'
                                AND c.rev > cp.rev AND c.rev <= $3), 0)
                 + coalesce((SELECT sum(octet_length(u.update_bytes)) FROM doc_updates u
                              WHERE u.doc_id = $1
                                AND ((u.rev > cp.rev AND u.rev <= $3) OR u.rev = ANY(cp.pending_revs))),
                            0))::bigint
           FROM cp",
    )
    .bind(doc)
    .bind(e.start_rev.expect("a window"))
    .bind(e.end_rev.expect("a window"))
    .fetch_one(&db.pool)
    .await
    .expect("the window's size")
}

/// A baseline's state is cut to its trusted prefix before it is replayed,
/// which costs more than a plain replay (`SHIFTED_MEMORY_FACTOR`), so a
/// baseline gets a lower cap than the plain one; a window does not.
#[tokio::test(flavor = "multi_thread")]
async fn a_baseline_over_the_shifted_cap_is_too_large() {
    use knot_history::baseline_max_input_bytes;
    let db = db().await;
    let (doc, ids, _) = page_with_baselines(&db).await;
    let baseline = db
        .entries(doc)
        .await
        .into_iter()
        .find(|e| e.baseline_id == Some(ids[1]))
        .expect("the newer baseline's entry");
    let b: i64 = sqlx::query_scalar(
        "SELECT octet_length(state_bytes)::bigint FROM doc_history_baselines WHERE id = $1",
    )
    .bind(ids[1])
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let (other, s, _) = open_session_page(&db).await;
    let w = window_bytes(&db, other, &s).await;
    let permits = history_permits(1);
    let len = |b: &[u8]| Ok(b.len());

    // At a plain cap equal to its own size: between the two caps.
    assert!(baseline_max_input_bytes(b) < b);
    let at = views_with(&db.pool, &permits, |l| l.max_plain_input_bytes = b);
    assert!(matches!(
        at.view(anyone(), doc, baseline.id, false).await,
        Err(ViewError::TooLarge)
    ));
    assert!(matches!(
        at.restore_input(anyone(), doc, baseline.id, None, len)
            .await,
        Err(ViewError::TooLarge)
    ));
    // A window of the same size, relative to its cap, renders.
    assert!(baseline_max_input_bytes(w) < w);
    let at = views_with(&db.pool, &permits, |l| l.max_plain_input_bytes = w);
    at.view(anyone(), other, s.id, false)
        .await
        .expect("the window renders");
    at.restore_input(anyone(), other, s.id, None, len)
        .await
        .expect("the window restores");
    // The baseline, once the plain cap leaves room for the shifted one.
    let room = (b * 44 + 39) / 40;
    assert!(baseline_max_input_bytes(room) >= b);
    let above = views_with(&db.pool, &permits, |l| l.max_plain_input_bytes = room);
    above
        .view(anyone(), doc, baseline.id, false)
        .await
        .expect("the baseline renders");
    above
        .restore_input(anyone(), doc, baseline.id, None, len)
        .await
        .expect("the baseline restores");
}

// ---------------------------------------------------------------------------
// Restore here: the target and the plan (M2 Task 4)
// ---------------------------------------------------------------------------

/// A restore's deadline as the route sets it.
fn in_27s() -> Instant {
    Instant::now() + Duration::from_secs(27)
}

/// The checkpoints a plan asks at, as `ViewHooks::revert_checkpoints`
/// records them.
type Checkpoints = Arc<std::sync::Mutex<Vec<Checkpoint>>>;

fn checkpoints() -> Checkpoints {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

fn seen(c: &Checkpoints) -> Vec<Checkpoint> {
    c.lock().unwrap().clone()
}

/// How often a plan reached the undo's verification.
fn undo_verifies(c: &Checkpoints) -> usize {
    seen(c)
        .into_iter()
        .filter(|c| *c == Checkpoint::UndoVerify)
        .count()
}

/// A page where A typed "one" and later B "two" (two sessions), built;
/// returns the page, A's and B's sessions and the live state (its rows).
async fn one_then_two(db: &Db) -> (Uuid, Entry, Entry, Vec<u8>) {
    let doc = db.page().await;
    let mut h = History::new(1);
    for (who, t) in [(0, "one"), (1, "two")] {
        let u = typed(&mut h, who, t);
        db.append(doc, who, &u).await;
    }
    build_clean(&db.builder(limits()), doc).await;
    let entries = db.entries(doc).await;
    let rows = db.row_bytes(doc).await;
    (
        doc,
        session_of(&entries, 0),
        session_of(&entries, 1),
        state_of(rows.iter().map(Vec::as_slice)),
    )
}

/// Restore here's target of entry `e`.
async fn target_of(views: &Views, doc: Uuid, e: &Entry) -> Restorable<RevertTarget> {
    views
        .restore_target(anyone(), doc, e.id, None, in_27s())
        .await
        .unwrap_or_else(|err| panic!("the target of {e:?}: {err:?}"))
}

/// pm_json of a GC replay of `live` with `update` applied, if any.
fn page_after(live: &[u8], update: Option<&[u8]>) -> Value {
    let mut r = Replayer::from_state(ReplayMode::Gc, live).expect("the live state");
    if let Some(u) = update {
        r.apply(u);
    }
    pm_json(r.doc())
}

fn took_between(started: Instant, low: Duration, high: Duration) {
    let took = started.elapsed();
    assert!(took >= low && took < high, "took {took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_revert_reverts_to_the_entrys_version() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let t = target_of(&views, doc, &a).await;
    assert_eq!(t.content_epoch, 0, "the page's epoch");
    let target = Arc::new(t.value);
    match views
        .plan_revert(anyone(), &t.ticket, target.clone(), live.clone(), in_27s())
        .await
    {
        Ok(RevertOutcome::Revert {
            update,
            kind: RevertKind::Undo,
            ..
        }) => {
            revert::verify(&live, &update, &target.json).expect("the revert verifies");
            assert_eq!(text_of(&page_after(&live, Some(&update))), "one");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn plan_revert_over_the_revert_cap_is_too_large() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let permits = history_permits(1);
    let probe = views_with(&db.pool, &permits, |_| {});
    let n = i64::try_from(target_of(&probe, doc, &a).await.value.state.len()).unwrap();
    let both = n + i64::try_from(live.len()).unwrap();
    let renders = counter();

    // Between the target alone and the target with the live page.
    let views = views_with(&db.pool, &permits, |l| {
        l.max_revert_input_bytes = both - 1;
        l.hooks.renders = Some(renders.clone());
    });
    assert!(n < both, "the target alone fits");
    let t = target_of(&views, doc, &a).await;
    assert_eq!(count(&renders), 1);
    let r = views
        .plan_revert(
            anyone(),
            &t.ticket,
            Arc::new(t.value),
            live.clone(),
            in_27s(),
        )
        .await;
    assert!(matches!(r, Err(PlanError::OverCap)), "{r:?}");
    assert_eq!(count(&renders), 1, "the target's render only");
    assert!(matches!(
        views
            .restore_target(anyone(), doc, a.id, None, in_27s())
            .await,
        Err(PlanError::OverCap)
    ));
    assert_eq!(count(&renders), 1, "refused before its replay");
    views
        .restore_input(anyone(), doc, a.id, None, revert::copy_state)
        .await
        .expect("only restores here are refused");
    assert_eq!(count(&renders), 2);

    // Below the target alone: refused after the target's replay.
    let alone = views_with(&db.pool, &permits, |l| {
        l.max_revert_input_bytes = n - 1;
        l.hooks.renders = Some(renders.clone());
    });
    for replays in [1, 0] {
        let before = count(&renders);
        assert!(matches!(
            alone
                .restore_target(anyone(), doc, a.id, None, in_27s())
                .await,
            Err(PlanError::OverCap)
        ));
        assert_eq!(count(&renders), before + replays);
    }
}

#[tokio::test]
async fn plan_revert_waits_no_longer_than_its_deadline() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let permits = history_permits(1);
    let renders = counter();
    let views = views_with(&db.pool, &permits, |l| {
        l.min_plan_time = Duration::from_millis(50);
        l.permit_wait = Duration::from_secs(5);
        l.hooks.renders = Some(renders.clone());
    });
    let t = target_of(&views, doc, &a).await;
    let before = count(&renders);
    let held = permits.compute().await;
    let started = Instant::now();
    let r = views
        .plan_revert(
            anyone(),
            &t.ticket,
            Arc::new(t.value),
            live,
            started + Duration::from_millis(400),
        )
        .await;
    assert!(matches!(r, Err(PlanError::View(ViewError::Busy))), "{r:?}");
    took_between(started, Duration::from_millis(350), Duration::from_secs(1));
    assert_eq!(count(&renders), before);
    drop(held);
}

#[tokio::test]
async fn a_plan_that_times_out_is_too_slow_and_blocks_only_restores_here() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let permits = history_permits(2);
    let renders = counter();
    let marks = checkpoints();
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.hooks.revert_checkpoints = Some(marks.clone());
        l.revert_timeout = Duration::from_millis(300);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let t = views
        .restore_input(anyone(), doc, a.id, None, revert::target)
        .await
        .expect("the target");
    let target = Arc::new(t.value);
    let r = views
        .plan_revert(anyone(), &t.ticket, target.clone(), live.clone(), in_27s())
        .await;
    assert!(matches!(r, Err(PlanError::TooSlow)), "{r:?}");
    assert_eq!(view_timeouts(&rec), 1);

    let before = count(&renders);
    let r = views
        .plan_revert(anyone(), &t.ticket, target, live, in_27s())
        .await;
    assert!(matches!(r, Err(PlanError::TooSlow)), "{r:?}");
    assert!(matches!(
        views
            .restore_target(anyone(), doc, a.id, None, in_27s())
            .await,
        Err(PlanError::TooSlow)
    ));
    assert_eq!(count(&renders), before, "refused before any replay");

    views
        .restore_input(anyone(), doc, a.id, None, revert::target)
        .await
        .expect("restore_input does not check the plan's record");
    views
        .restore_input(anyone(), doc, a.id, None, revert::copy_state)
        .await
        .expect("restore as new page still works");
    views
        .view(anyone(), doc, a.id, false)
        .await
        .expect("the view still works");
    wait_until("the plan thread's end", Duration::from_secs(3), || {
        stalled_gauge(&rec) == Some(0.0)
    })
    .await;
    assert_eq!(
        seen(&marks),
        [Checkpoint::Undo],
        "cancelled: it stopped at its first checkpoint"
    );
    assert_eq!(view_timeouts(&rec), 1);
}

#[tokio::test]
async fn a_recorded_revert_refuses_the_target_before_its_replay() {
    let db = db().await;
    let (doc, a, b, live) = one_then_two(&db).await;
    let permits = history_permits(1);
    let renders = counter();
    let views = views_with(&db.pool, &permits, |l| {
        l.permit_wait = Duration::from_millis(300);
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.revert_timeout = Duration::from_millis(300);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let t = target_of(&views, doc, &a).await;
    let r = views
        .plan_revert(anyone(), &t.ticket, Arc::new(t.value), live, in_27s())
        .await;
    assert!(matches!(r, Err(PlanError::TooSlow)), "{r:?}");
    wait_until("the plan thread's end", Duration::from_secs(3), || {
        permits.stalled() == 0 && stalled_gauge(&rec) == Some(0.0)
    })
    .await;

    let held = permits.compute().await;
    let before = count(&renders);
    let started = Instant::now();
    assert!(matches!(
        views
            .restore_target(anyone(), doc, a.id, None, in_27s())
            .await,
        Err(PlanError::TooSlow)
    ));
    took_between(started, Duration::ZERO, Duration::from_millis(100));
    assert_eq!(count(&renders), before);

    let started = Instant::now();
    let r = views
        .restore_target(anyone(), doc, b.id, None, in_27s())
        .await;
    assert!(
        matches!(r, Err(PlanError::View(ViewError::Busy))),
        "{:?}",
        r.err()
    );
    took_between(started, Duration::from_millis(250), Duration::from_secs(1));
    assert_eq!(count(&renders), before);
    drop(held);
}

#[tokio::test]
async fn a_plan_the_deadline_cuts_short_is_busy_and_records_nothing() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let permits = history_permits(2);
    let renders = counter();
    let marks = checkpoints();
    let views = views_with(&db.pool, &permits, |l| {
        l.min_plan_time = Duration::from_millis(50);
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.hooks.revert_checkpoints = Some(marks.clone());
        l.revert_timeout = Duration::from_secs(10);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let t = views
        .restore_input(anyone(), doc, a.id, None, revert::target)
        .await
        .expect("the target");
    let target = Arc::new(t.value);
    let plan = |deadline: Instant| {
        views.plan_revert(anyone(), &t.ticket, target.clone(), live.clone(), deadline)
    };

    let started = Instant::now();
    let r = plan(started + Duration::from_millis(500)).await;
    assert!(matches!(r, Err(PlanError::View(ViewError::Busy))), "{r:?}");
    took_between(started, Duration::from_millis(400), Duration::from_secs(1));
    assert_eq!(view_timeouts(&rec), 0, "not recorded, not counted");

    // Dropped.
    let dropped = tokio::time::timeout(
        Duration::from_millis(300),
        plan(Instant::now() + Duration::from_millis(400)),
    )
    .await;
    assert!(dropped.is_err(), "the request was dropped");
    wait_until("both plan threads ended", Duration::from_secs(10), || {
        permits.stalled() == 0 && stalled_gauge(&rec) == Some(0.0)
    })
    .await;
    assert_eq!(count(&renders), 3, "the target and two plans");
    assert_eq!(view_timeouts(&rec), 0, "each ended within its full timeout");
    assert_eq!(undo_verifies(&marks), 2, "neither was cancelled");

    match plan(in_27s()).await {
        Ok(RevertOutcome::Revert { .. }) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(undo_verifies(&marks), 3);
    views
        .restore_input(anyone(), doc, a.id, None, revert::copy_state)
        .await
        .expect("nothing recorded");
}

#[tokio::test]
async fn a_cut_short_plan_that_keeps_running_is_recorded() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let permits = history_permits(1);
    let renders = counter();
    let marks = checkpoints();
    let views = Arc::new(views_with(&db.pool, &permits, |l| {
        l.min_plan_time = Duration::from_millis(50);
        l.hooks.hold_render = Duration::from_secs(2);
        l.hooks.renders = Some(renders.clone());
        l.hooks.revert_checkpoints = Some(marks.clone());
        l.hooks.revert.corrupt_undo = true;
        l.revert_timeout = Duration::from_secs(1);
    }));
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let t = views
        .restore_input(anyone(), doc, a.id, None, revert::target)
        .await
        .expect("the target");
    let (ticket, target) = (t.ticket, Arc::new(t.value));

    let started = Instant::now();
    let r = views
        .plan_revert(
            anyone(),
            &ticket,
            target.clone(),
            live.clone(),
            started + Duration::from_millis(300),
        )
        .await;
    assert!(matches!(r, Err(PlanError::View(ViewError::Busy))), "{r:?}");
    took_between(started, Duration::from_millis(250), Duration::from_secs(1));
    assert_eq!(view_timeouts(&rec), 0, "cut short, not timed out");

    // It waits for the only compute permit, which the first plan's thread
    // holds until it ends (about 2 s after it started).
    let second = tokio::spawn({
        let (views, ticket, target, live) =
            (views.clone(), ticket.clone(), target.clone(), live.clone());
        async move {
            views
                .plan_revert(anyone(), &ticket, target, live, in_27s())
                .await
        }
    });
    wait_until("the watcher's record", Duration::from_millis(1800), || {
        view_timeouts(&rec) == 1
    })
    .await;
    assert!(!second.is_finished(), "the second plan still waits");
    let r = second.await.unwrap();
    assert!(matches!(r, Err(PlanError::TooSlow)), "{r:?}");
    assert_eq!(count(&renders), 2, "never a second plan thread");

    let started = Instant::now();
    let r = views
        .plan_revert(anyone(), &ticket, target, live, in_27s())
        .await;
    assert!(matches!(r, Err(PlanError::TooSlow)), "{r:?}");
    took_between(started, Duration::ZERO, Duration::from_millis(100));
    assert_eq!(count(&renders), 2);

    wait_until("the plan thread's end", Duration::from_secs(3), || {
        permits.stalled() == 0 && stalled_gauge(&rec) == Some(0.0)
    })
    .await;
    views
        .restore_input(anyone(), doc, a.id, None, revert::copy_state)
        .await
        .expect("only restores here are refused");
    assert_eq!(
        seen(&marks),
        [Checkpoint::Undo],
        "cancelled at its full timeout: no undo, no fallback"
    );
    assert_eq!(view_timeouts(&rec), 1);
}

#[tokio::test]
async fn restore_waits_are_bounded_by_the_restores_deadline() {
    let db = db().await;
    let (doc, a, _, _) = one_then_two(&db).await;
    let permits = history_permits(1);
    let renders = counter();
    let views = views_with(&db.pool, &permits, |l| {
        l.permit_wait = Duration::from_secs(5);
        l.hooks.renders = Some(renders.clone());
    });
    let held = permits.compute().await;
    let started = Instant::now();
    let r = views
        .restore_target(
            anyone(),
            doc,
            a.id,
            None,
            started + Duration::from_millis(400),
        )
        .await;
    assert!(
        matches!(r, Err(PlanError::View(ViewError::Busy))),
        "{:?}",
        r.err()
    );
    took_between(started, Duration::from_millis(350), Duration::from_secs(1));
    let started = Instant::now();
    let r = views
        .restore_input_by(
            anyone(),
            doc,
            a.id,
            None,
            started + Duration::from_millis(400),
            revert::copy_state,
        )
        .await;
    assert!(matches!(r, Err(ViewError::Busy)), "{:?}", r.err());
    took_between(started, Duration::from_millis(350), Duration::from_secs(1));
    assert_eq!(count(&renders), 0);
    drop(held);
    target_of(&views, doc, &a).await;
    assert_eq!(count(&renders), 1);
}

#[tokio::test]
async fn a_plan_nothing_verifies_is_unverified() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let views = views_with(&db.pool, &history_permits(1), |l| {
        l.hooks.revert = RevertHooks {
            skip_undo: true,
            fail_fallback: true,
            ..RevertHooks::default()
        };
    });
    let t = target_of(&views, doc, &a).await;
    let r = views
        .plan_revert(anyone(), &t.ticket, Arc::new(t.value), live, in_27s())
        .await;
    assert!(matches!(r, Err(PlanError::Unverified(_))), "{r:?}");
}

#[tokio::test]
async fn a_restore_permit_is_one_per_principal() {
    let db = db().await;
    let (doc, a, _, _) = one_then_two(&db).await;
    let renders = counter();
    let views = views_with(&db.pool, &history_permits(1), |l| {
        l.permit_wait = Duration::from_millis(300);
        l.hooks.renders = Some(renders.clone());
    });
    let (pa, pb) = (Uuid::new_v4(), Uuid::new_v4());
    let first = views
        .restore_permit(pa, in_27s())
        .await
        .expect("A's permit");

    let started = Instant::now();
    let r = views.restore_permit(pa, in_27s()).await;
    assert!(matches!(r, Err(ViewError::Busy)), "{r:?}");
    took_between(started, Duration::from_millis(250), Duration::from_secs(1));
    let started = Instant::now();
    let r = views
        .restore_permit(pa, started + Duration::from_millis(100))
        .await;
    assert!(matches!(r, Err(ViewError::Busy)), "{r:?}");
    took_between(
        started,
        Duration::from_millis(90),
        Duration::from_millis(250),
    );

    let started = Instant::now();
    let other = views
        .restore_permit(pb, in_27s())
        .await
        .expect("B's permit");
    took_between(started, Duration::ZERO, Duration::from_millis(100));
    drop(other);

    let started = Instant::now();
    drop(first);
    let again = views.restore_permit(pa, in_27s()).await.expect("A's again");
    took_between(started, Duration::ZERO, Duration::from_millis(100));
    assert_eq!(count(&renders), 0, "it renders nothing");

    views
        .view(pa, doc, a.id, false)
        .await
        .expect("a view does not take the restore permit");
    assert_eq!(count(&renders), 1);
    drop(again);
}

#[tokio::test]
async fn a_dropped_plan_is_cancelled_at_once() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let permits = history_permits(1);
    let renders = counter();
    let marks = checkpoints();
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(1);
        l.hooks.renders = Some(renders.clone());
        l.hooks.revert_checkpoints = Some(marks.clone());
        l.hooks.revert.corrupt_undo = true;
        l.revert_timeout = Duration::from_secs(10);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let t = target_of(&views, doc, &a).await;
    let dropped = tokio::time::timeout(
        Duration::from_millis(300),
        views.plan_revert(anyone(), &t.ticket, Arc::new(t.value), live, in_27s()),
    )
    .await;
    assert!(dropped.is_err(), "the request was dropped");
    wait_until("the plan thread's end", Duration::from_secs(3), || {
        permits.stalled() == 0 && stalled_gauge(&rec) == Some(0.0)
    })
    .await;
    assert_eq!(seen(&marks), [Checkpoint::Undo], "cancelled at once");
    assert_eq!(view_timeouts(&rec), 0);
    assert_eq!(count(&renders), 2, "the target and the plan");
}

#[tokio::test]
async fn a_plan_with_too_little_time_left_starts_no_thread() {
    let db = db().await;
    let (doc, a, _, live) = one_then_two(&db).await;
    let permits = history_permits(1);
    let renders = counter();
    let views = views_with(&db.pool, &permits, |l| {
        assert_eq!(l.min_plan_time, REVERT_MIN_PLAN_TIME);
        l.hooks.renders = Some(renders.clone());
    });
    let t = target_of(&views, doc, &a).await;
    let target = Arc::new(t.value);
    let before = count(&renders);

    let started = Instant::now();
    let r = views
        .plan_revert(
            anyone(),
            &t.ticket,
            target.clone(),
            live.clone(),
            started + Duration::from_millis(10),
        )
        .await;
    assert!(matches!(r, Err(PlanError::View(ViewError::Busy))), "{r:?}");
    took_between(started, Duration::ZERO, Duration::from_millis(100));
    assert_eq!(count(&renders), before);
    let granted = tokio::time::timeout(Duration::from_millis(100), permits.compute())
        .await
        .expect("no permit taken");
    drop(granted);

    // The permit comes after 500 ms, with less than the floor left then.
    let held = permits.compute().await;
    let started = Instant::now();
    let (r, ()) = tokio::join!(
        views.plan_revert(
            anyone(),
            &t.ticket,
            target,
            live,
            started + REVERT_MIN_PLAN_TIME + Duration::from_millis(300),
        ),
        async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(held);
        }
    );
    assert!(matches!(r, Err(PlanError::View(ViewError::Busy))), "{r:?}");
    took_between(
        started,
        Duration::from_millis(450),
        Duration::from_millis(1500),
    );
    assert_eq!(count(&renders), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_lifted_from_version_1_restores_no_phantom() {
    let db = db().await;
    let p = page_lifted_from_version_1(&db).await;
    let doc = p.doc;
    let entries = rebuilt(&db, &p).await;
    let allowed = p.allowed();
    let live = p.room();
    let views = views_with(&db.pool, &history_permits(1), |_| {});
    let mut kinds = Vec::new();
    for e in &entries {
        let target = views
            .restore_input(anyone(), doc, e.id, None, revert::target)
            .await
            .unwrap_or_else(|err| panic!("{e:?}: {err:?}"));
        let copy = views
            .restore_input(anyone(), doc, e.id, None, revert::copy_state)
            .await
            .unwrap_or_else(|err| panic!("{e:?}: {err:?}"));
        for (what, state) in [("target", &target.value.state), ("copy", &copy.value)] {
            let held = insertions(state);
            assert!(
                within(&held, &allowed),
                "the {what} of the {} entry {:?}..{:?} names {:?}",
                e.kind,
                e.start_rev,
                e.end_rev,
                held.diff(&allowed)
            );
        }

        let v = json_of(&views.view(anyone(), doc, e.id, false).await.unwrap());
        let t = views
            .restore_target(anyone(), doc, e.id, None, in_27s())
            .await
            .unwrap_or_else(|err| panic!("{e:?}: {err:?}"));
        let json: Value = serde_json::from_slice(&t.value.json).expect("the target's JSON");
        assert_eq!(json, v, "{e:?}");
        let target = Arc::new(t.value);
        let kind = match views
            .plan_revert(anyone(), &t.ticket, target.clone(), live.clone(), in_27s())
            .await
        {
            Ok(RevertOutcome::Unchanged) => {
                assert_eq!(page_after(&live, None), v, "{e:?}");
                "unchanged"
            }
            Ok(RevertOutcome::Revert { update, kind, .. }) => {
                revert::verify(&live, &update, &target.json)
                    .unwrap_or_else(|why| panic!("{e:?}: {why}"));
                assert_eq!(page_after(&live, Some(&update)), v, "{e:?}");
                kind.outcome()
            }
            // Task 1 recorded no gap exceptions (every gap case's
            // whole-page replace verified), so none is allowed here.
            Err(err) => panic!("{e:?}: {err:?}"),
        };
        kinds.push((e.kind.clone(), e.start_rev, e.end_rev, kind));
    }
    println!("plans per entry (kind, start, end, outcome): {kinds:?}");
}
