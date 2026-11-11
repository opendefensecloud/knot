//! The builder's metrics (page history M1.3, Task 7). Current-thread tests:
//! the local recorder is thread-local, and on a current-thread runtime the
//! builder's tasks run on the test's thread (replay threads record
//! nothing).

mod common;

use std::time::Duration;

use common::db::{
    build_clean, db, genesis, genesis_then_rows, holding, is_built, limits, limits_with,
    oversized_snapshot_page, page_with_gaps, page_with_rows, state_of, typed, with_paragraph,
};
use common::metrics::CountingRecorder;
use common::{History, author};
use knot_history::builder::{MAX_CRASHES, MAX_FAILURES};
use knot_history::{PassResult, history_permits};
use knot_storage::UpdateSource;

fn given_up(rec: &CountingRecorder, reason: &str) -> u64 {
    rec.counter("knot_history_pages_given_up_total", &[("reason", reason)])
}

fn unchecked(rec: &CountingRecorder, reason: &str) -> u64 {
    rec.counter(
        "knot_history_snapshots_unchecked_total",
        &[("reason", reason)],
    )
}

#[tokio::test]
async fn a_pass_records_its_metrics() {
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let db = db().await;
    let b = db.builder(limits());

    // P: three good rows, an undecodable one, one that parks for good, and
    // a row 30 minutes later that seals A's first session.
    let p = db.page().await;
    let since = chrono::Utc::now() - chrono::Duration::hours(2);
    db.backdate(p, since).await;
    let mut h = History::new(1);
    let mut good = Vec::new();
    for t in ["one", "two", "three"] {
        let u = typed(&mut h, 0, t);
        db.insert_row(p, Some(author(0)), &u, Some(UpdateSource::Edit), since)
            .await;
        good.push(u);
    }
    db.insert_row(
        p,
        Some(author(0)),
        b"\x00not an update",
        Some(UpdateSource::Edit),
        since,
    )
    .await;
    let (_, parks) = {
        let unlogged = common::Client::new(9);
        let para = unlogged.txn(|txn, frag| {
            use yrs::XmlFragment;
            let p = frag.insert(txn, 0, yrs::XmlElementPrelim::empty("paragraph"));
            p.push_back(txn, yrs::XmlTextPrelim::new("p"));
        });
        with_paragraph(12, &state_of([para.as_slice()]), "never arrives")
    };
    db.insert_row(p, Some(author(1)), &parks, Some(UpdateSource::Edit), since)
        .await;
    let late = typed(&mut h, 0, "later");
    db.insert_row(
        p,
        Some(author(0)),
        &late,
        Some(UpdateSource::Edit),
        since + chrono::Duration::minutes(30),
    )
    .await;
    good.push(late);

    assert_eq!(b.build_page(p).await, genesis(0));
    assert!(rec.histogram("knot_history_replay_seconds", &[("phase", "genesis")]) >= 1);
    assert_eq!(b.sweep().await, 0, "P has no work the sweep finds");
    assert_eq!(rec.gauge("knot_history_builder_lag_revs"), Some(6.0));
    let (with_extra, _) = with_paragraph(9, &state_of(good.iter().map(Vec::as_slice)), "extra");
    db.snapshot(p, 6, &with_extra).await;
    assert!(is_built(&b.build_page(p).await));
    assert_eq!(rec.counter("knot_history_rows_built_total", &[]), 6);
    assert_eq!(
        rec.counter(
            "knot_history_rows_unapplied_total",
            &[("reason", "undecodable")]
        ),
        1
    );
    assert_eq!(rec.counter("knot_history_rows_parked_total", &[]), 1);
    assert_eq!(rec.counter("knot_history_gap_total", &[]), 1);
    assert!(rec.histogram("knot_history_replay_seconds", &[("phase", "build")]) >= 1);
    assert!(rec.histogram("knot_history_replay_seconds", &[("phase", "words")]) >= 1);
    b.sweep().await;
    assert_eq!(rec.gauge("knot_history_builder_lag_revs"), Some(0.0));

    // Q: a count over the cap.
    let (q, _) = page_with_rows(&db, &[(0, "alpha", 89), (0, "beta", 119)]).await;
    let small = db.builder(limits_with(|l| l.words_max_input_bytes = 1));
    build_clean(&small, q).await;
    assert_eq!(
        rec.counter(
            "knot_history_words_skipped_total",
            &[("reason", "too_large")]
        ),
        1
    );

    // R: a rev gap, then a give-up.
    let r = db.page().await;
    genesis_then_rows(&db, r).await;
    db.rewrite("DELETE FROM doc_updates WHERE doc_id = $1 AND rev = 2", r)
        .await;
    assert_eq!(b.build_page(r).await, PassResult::Failed);
    assert_eq!(rec.counter("knot_history_build_failures_total", &[]), 1);
    db.exec(
        &format!(
            "UPDATE doc_history_state SET failures = {MAX_FAILURES}, retry_after = NULL
              WHERE doc_id = $1"
        ),
        r,
    )
    .await;
    assert_eq!(b.build_page(r).await, PassResult::Failed);
    assert_eq!(given_up(&rec, "failures"), 1);
    assert_eq!(rec.counter("knot_history_build_failures_total", &[]), 1);
    assert_eq!(b.build_page(r).await, PassResult::Deferred);
    assert_eq!(given_up(&rec, "failures"), 1);
    b.sweep().await;
    assert_eq!(rec.gauge("knot_history_given_up_pages"), Some(1.0));
    assert_eq!(
        rec.gauge("knot_history_builder_lag_revs"),
        Some(0.0),
        "a given-up page is not in the lag"
    );
}

#[tokio::test]
async fn an_oversized_page_counts_as_too_large() {
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(doc, 0, &u).await;
    let b = db.builder(limits_with(|l| l.max_input_bytes = 1));
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    assert_eq!(given_up(&rec, "too_large"), 1);
    assert_eq!(rec.counter("knot_history_build_failures_total", &[]), 0);
}

#[tokio::test]
async fn snapshot_failures_and_timeouts_are_counted() {
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let db = db().await;

    // A gap merge that corrupts the replay.
    let doc = db.page().await;
    genesis_then_rows(&db, doc).await;
    let (s, _) = with_paragraph(9, &db.state_at(doc, 2).await, "extra");
    db.snapshot(doc, 2, &s).await;
    let corrupt = db.builder(limits_with(|l| l.hooks.replay.corrupt_merge_at = Some(2)));
    assert_eq!(corrupt.build_page(doc).await, PassResult::Failed);
    assert_eq!(unchecked(&rec, "corrupt"), 1);

    // A snapshot too large to probe.
    let (doc, cap) = oversized_snapshot_page(&db).await;
    let capped = db.builder(limits_with(|l| l.max_input_bytes = cap));
    assert!(is_built(&capped.build_page(doc).await));
    assert_eq!(unchecked(&rec, "too_large"), 1);

    // A pass that times out; its thread counts as stalled until it ends.
    let doc = db.page().await;
    genesis_then_rows(&db, doc).await;
    let slow = db.builder(limits_with(|l| {
        l.replay_timeout = Duration::from_millis(200);
        l.hooks.hold_compute = Duration::from_secs(1);
    }));
    assert_eq!(slow.build_page(doc).await, PassResult::Failed);
    assert_eq!(
        rec.counter("knot_history_replay_timeouts_total", &[("phase", "build")]),
        1
    );
    assert_eq!(rec.gauge("knot_history_replay_threads_stalled"), Some(1.0));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    slow.sweep().await;
    assert_eq!(rec.gauge("knot_history_replay_threads_stalled"), Some(0.0));

    // A word count that times out is skipped; the pass is built.
    let failures = rec.counter("knot_history_build_failures_total", &[]);
    let (doc, _) = page_with_rows(&db, &[(0, "alpha beta", 89), (0, "gamma", 119)]).await;
    assert_eq!(db.builder(limits()).build_page(doc).await, genesis(0));
    let words = db.builder(limits_with(|l| {
        l.words_timeout = Duration::from_millis(200);
        l.hooks.hold_words = Duration::from_secs(1);
    }));
    assert!(is_built(&words.build_page(doc).await));
    assert_eq!(
        rec.counter("knot_history_replay_timeouts_total", &[("phase", "words")]),
        1
    );
    assert_eq!(
        rec.counter("knot_history_words_skipped_total", &[("reason", "timeout")]),
        1
    );
    assert_eq!(
        rec.counter("knot_history_build_failures_total", &[]),
        failures
    );

    // A checkpoint encode that hangs gives the page up at once ...
    let doc = db.page().await;
    let emit = db.builder(limits_with(|l| {
        l.replay_timeout = Duration::from_millis(500);
        l.hooks.replay.sleep_at_emit = Some(Duration::from_millis(1500));
    }));
    assert_eq!(emit.build_page(doc).await, PassResult::Failed);
    assert_eq!(given_up(&rec, "timeout"), 1);
    // ... and a hang with nothing to blame at its second timeout.
    let doc = db.page().await;
    let hang = || {
        limits_with(|l| {
            l.replay_timeout = Duration::from_millis(300);
            l.hooks.hold_compute = Duration::from_secs(1);
        })
    };
    let (x, y) = (
        db.builder_with(hang(), history_permits(1)),
        db.builder_with(hang(), history_permits(1)),
    );
    assert_eq!(x.build_page(doc).await, PassResult::Failed);
    db.retry_now(doc).await;
    assert_eq!(y.build_page(doc).await, PassResult::Failed);
    assert_eq!(given_up(&rec, "timeout"), 2);

    // A kept gap that does not merge.
    let doc = db.page().await;
    page_with_gaps(&db, doc, &[(2, "gap x"), (3, "gap y")], true).await;
    db.exec(
        "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
        doc,
    )
    .await;
    let kept = db.builder(limits_with(|l| {
        l.hooks.replay.corrupt_kept_gap_at = Some(2)
    }));
    assert_eq!(kept.build_page(doc).await, genesis(0));
    assert_eq!(kept.build_page(doc).await, PassResult::Failed);
    assert_eq!(unchecked(&rec, "kept_gap"), 1);

    // A page given up for crashes.
    let doc = db.page().await;
    sqlx::query(
        "INSERT INTO doc_history_state (doc_id, crashes, last_error) VALUES ($1, $2, 'pass interrupted')",
    )
    .bind(doc)
    .bind(MAX_CRASHES - 1)
    .execute(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        db.builder(holding(Duration::ZERO)).build_page(doc).await,
        PassResult::Failed
    );
    assert_eq!(given_up(&rec, "crash"), 1);
}
