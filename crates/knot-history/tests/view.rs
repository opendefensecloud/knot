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
    Db, Entry, build_clean, db, limits, page_with_rows, state_of, typed, visible, with_paragraph,
};
use common::metrics::CountingRecorder;
use common::{Client, History, author};
use knot_history::canonical::visible_content;
use knot_history::{
    BUILDER_VERSION, EntryKind, HistoryPermits, ReplayMode, Replayer, VIEW_PERMIT_WAIT, ViewError,
    ViewLimits, Views, history_permits, list_entries, merge_gap_at_end, pm_json,
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
            .restore_input(anyone(), doc, s.id, |b| Ok(b.len()))
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
        .restore_input(anyone(), doc, a.id, |bytes| {
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
            .restore_input(anyone(), other, s.id, |b| Ok(b.len()))
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
        .restore_input(anyone(), doc, s.id, |b| Ok(b.len()))
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
            .restore_input(anyone(), doc, s.id, |b| Ok(b.len()))
            .await,
        Err(ViewError::TooLarge)
    ));
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
            .restore_input(anyone(), doc, baseline.id, |b| Ok(b.len()))
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
                .restore_input(anyone(), doc, sealed.id, |b| Ok(b.len()))
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
    let views = views_with(&db.pool, &permits, |l| {
        l.hooks.hold_render = Duration::from_secs(1);
    });
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let dropped = tokio::time::timeout(
        Duration::from_millis(200),
        views.view(anyone(), doc, s.id, false),
    )
    .await;
    assert!(dropped.is_err(), "the request was dropped");
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
    let dropped = tokio::time::timeout(
        Duration::from_millis(100),
        views.view(anyone(), doc, s.id, true),
    )
    .await;
    assert!(dropped.is_err());
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
            .restore_input(anyone(), doc, s.id, |b| Ok(b.len()))
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
    let dropped =
        tokio::time::timeout(Duration::from_millis(100), views.view(a, doc, e2, false)).await;
    assert!(dropped.is_err());
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
        tokio::time::timeout(
            Duration::from_millis(100),
            views.view(anyone(), doc, s.id, false)
        ),
        views.view(anyone(), doc, s.id, false),
    );
    assert!(first.is_err(), "the first was dropped");
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
        small.restore_input(anyone(), doc, s.id, len).await,
        Err(ViewError::TooLarge)
    ));

    let views = views_with(&db.pool, &permits, |l| {
        l.permit_wait = Duration::from_millis(200);
        l.restore_timeout = Duration::from_millis(300);
        l.hooks.renders = Some(renders.clone());
    });
    let held = (permits.compute().await, permits.compute().await);
    assert!(matches!(
        views.restore_input(anyone(), doc, s.id, len).await,
        Err(ViewError::Busy)
    ));
    drop(held);
    match views
        .restore_input(anyone(), doc, s.id, |_| Err::<(), _>("x".to_string()))
        .await
    {
        Err(ViewError::Failed(m)) => assert_eq!(m, "x"),
        other => panic!("{:?}", other.err()),
    }
    assert!(matches!(
        views
            .restore_input(anyone(), doc, s.id, |_| -> Result<(), String> {
                panic!("x")
            })
            .await,
        Err(ViewError::Failed(_))
    ));
    assert!(matches!(
        views
            .restore_input(anyone(), doc, s.id, |_| {
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            })
            .await,
        Err(ViewError::TimedOut)
    ));
    assert_eq!(view_timeouts(&rec), 1);
    let before = count(&renders);
    assert!(matches!(
        views.restore_input(anyone(), doc, s.id, len).await,
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
