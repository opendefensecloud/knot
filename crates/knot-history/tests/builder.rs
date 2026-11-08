//! The builder (page history M1.3, Task 7): claims, leases, genesis,
//! passes, word counts, failures and the candidate queries, against
//! Postgres.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use common::db::{
    Db, Entry, base_of, build_clean, build_until, db, filler, genesis, genesis_then_rows, holding,
    is_built, keys, limits, limits_with, oversized_snapshot_page, page_with_gaps, page_with_rows,
    state_of, typed, visible, with_paragraph,
};
use common::underflow::skip_underflow_case;
use common::{Client, History, author};
use knot_history::builder::{DEBOUNCE_MAX_LAG_REVS, MAX_FAILURES, MAX_REBUILDS};
use knot_history::{BUILDER_VERSION, PassResult, history_permits};
use knot_storage::{DocStore, GrantStore, PgGrantStore, UpdateSource};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{Text, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn minutes(n: i64) -> chrono::Duration {
    chrono::Duration::minutes(n)
}

fn sessions(entries: &[Entry]) -> Vec<&Entry> {
    entries.iter().filter(|e| e.kind == "session").collect()
}

fn of_kind<'a>(entries: &'a [Entry], kind: &str) -> Vec<&'a Entry> {
    entries.iter().filter(|e| e.kind == kind).collect()
}

/// The same rows on a twin page (same `history_since`, same times).
async fn twin_of(db: &Db, doc: Uuid) -> Uuid {
    let twin = db.page().await;
    db.backdate(twin, db.history_since(doc).await).await;
    sqlx::query(
        "INSERT INTO doc_updates (doc_id, by_user_id, update_bytes, source, meta, epoch, created_at)
         SELECT $2, by_user_id, update_bytes, source, meta, 0, created_at
           FROM doc_updates WHERE doc_id = $1 ORDER BY rev",
    )
    .bind(doc)
    .bind(twin)
    .execute(&db.pool)
    .await
    .expect("twin rows");
    twin
}

// ---------------------------------------------------------------------------
// Genesis, sessions, word counts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_new_page_gets_genesis_then_sessions() {
    let db = db().await;
    let doc = db.page().await;
    let b = db.builder(limits());
    let mut h = History::new(1);
    for t in ["one", "two", "three"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert_eq!(
        b.build_page(doc).await,
        PassResult::Built {
            built_rev: 3,
            rows: 3
        }
    );
    let entries = db.entries(doc).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    let e = &entries[0];
    assert_eq!(
        (e.kind.as_str(), e.author, e.start_rev, e.end_rev, e.sealed),
        ("session", Some(author(0)), Some(1), Some(3), false)
    );
    assert!(!db.head(doc).await.1, "the page is clean");
    let s = db.state(doc).await;
    assert_eq!((s.failures, s.retry_null, s.claimed_by), (0, true, None));

    let u = typed(&mut h, 0, "four");
    db.append(doc, 0, &u).await;
    assert_eq!(
        b.build_page(doc).await,
        PassResult::Built {
            built_rev: 4,
            rows: 1
        }
    );
    let after = db.entries(doc).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, e.id, "the same session continues");
    assert_eq!(after[0].end_rev, Some(4));
    assert!(after[0].updated_at > e.updated_at);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_seal_and_get_word_counts() {
    let db = db().await;
    // history_since 2 h ago; A at +89 min, +90 min, then +119 min (now − 1).
    let (doc, _) = page_with_rows(
        &db,
        &[(0, "alpha beta", 89), (0, "gamma", 90), (0, "delta", 119)],
    )
    .await;
    let b = db.builder(limits());
    build_clean(&b, doc).await;
    let entries = db.entries(doc).await;
    let s = sessions(&entries);
    assert_eq!(s.len(), 2, "{entries:?}");
    assert!(s[0].sealed);
    assert_eq!((s[0].words_added, s[0].words_removed), (Some(3), Some(0)));
    assert!(!s[0].words_pending);
    assert!(!s[1].sealed);
    assert_eq!((s[1].words_added, s[1].words_removed), (None, None));
    assert!(!db.head(doc).await.1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_existing_page_gets_baseline_entries_and_a_genesis_with_its_late_seq_row() {
    use knot_test_support::migrate::{bare_db, migrate_all, migrate_up_to};
    let (_, pool) = bare_db(8).await;
    migrate_up_to(&pool, 20261102120000).await;
    let ws: Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    for i in 0..3 {
        sqlx::query("INSERT INTO users (id, email, display_name) VALUES ($1, $2, $3)")
            .bind(author(i))
            .bind(format!("u{i}@x.test"))
            .bind(format!("User {i}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by, created_at)
         VALUES ($1, 'D', 'm', $2, now() - interval '1 day') RETURNING id",
    )
    .bind(ws)
    .bind(author(0))
    .fetch_one(&pool)
    .await
    .unwrap();
    // Legacy rows by three clients: a (seq 7), b (seq 8), c (seq 9); the
    // room's snapshot at seq 10 holds a and c, not b (b's save committed
    // after the snapshot read the log).
    let para = |client: u64, text: &str| {
        let c = Client::new(client);
        c.txn(|txn, frag| {
            let p = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
            p.push_back(txn, XmlTextPrelim::new(text));
        })
    };
    let (a, bb, c) = (
        para(1, "legacy a"),
        para(2, "legacy b"),
        para(3, "legacy c"),
    );
    for (seq, bytes) in [(7_i64, &a), (8, &bb), (9, &c)] {
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, seq, update_bytes, created_at)
             VALUES ($1, $2, $3, now() - interval '1 hour')",
        )
        .bind(doc)
        .bind(seq)
        .bind(bytes)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("SELECT setval(pg_get_serial_sequence('doc_updates', 'seq'), 100)")
        .execute(&pool)
        .await
        .unwrap();
    let baseline = state_of([a.as_slice(), c.as_slice()]);
    sqlx::query(
        "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
         VALUES ($1, 10, $2, '\\x00', now() - interval '30 minutes')",
    )
    .bind(doc)
    .bind(&baseline)
    .execute(&pool)
    .await
    .unwrap();
    migrate_all(&pool).await;
    let db = Db::on_existing(pool.clone(), ws).await;

    // A types after history_since.
    let a_client = Client::new(10);
    let room = state_of([a.as_slice(), bb.as_slice(), c.as_slice()]);
    a_client
        .doc
        .transact_mut()
        .apply_update(yrs::Update::decode_v1(&room).unwrap())
        .unwrap();
    let mut a_rows = Vec::new();
    for text in ["new one", "new two"] {
        let u = a_client.txn(|txn, frag| {
            let n = frag.len(txn);
            let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
            p.push_back(txn, XmlTextPrelim::new(text));
        });
        a_rows.push(u.clone());
        db.append(doc, 0, &u).await;
    }
    let head = db.head(doc).await.0;
    assert_eq!(head, 5);
    // A stamped snapshot above G holding the room's state, b included.
    let all: Vec<&[u8]> = [a.as_slice(), bb.as_slice(), c.as_slice()]
        .into_iter()
        .chain(a_rows.iter().map(Vec::as_slice))
        .collect();
    db.snapshot(doc, head, &state_of(all)).await;

    let b = db.builder(limits());
    assert_eq!(b.build_page(doc).await, genesis(3));
    build_clean(&b, doc).await;
    let entries = db.entries(doc).await;
    let since = db.history_since(doc).await;
    let baselines: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM doc_history_baselines WHERE doc_id = $1")
            .bind(doc)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(baselines.len(), 1);
    assert_eq!(entries.len(), 3, "{entries:#?}");
    let listed = of_kind(&entries, "baseline");
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .any(|e| e.baseline_id == Some(baselines[0]) && e.author.is_none())
    );
    let rollout = listed
        .iter()
        .find(|e| e.baseline_id.is_none())
        .expect("the rollout entry");
    assert_eq!(rollout.meta, serde_json::json!({"rollout": true}));
    assert_eq!(rollout.end_rev, Some(3));
    assert_eq!(rollout.ended_at, since);
    let s = sessions(&entries);
    assert_eq!(s.len(), 1);
    assert_eq!(
        (s[0].author, s[0].start_rev, s[0].end_rev),
        (Some(author(0)), Some(4), Some(5))
    );
    assert!(
        of_kind(&entries, "gap").is_empty(),
        "the snapshot above G holds no gap"
    );
    assert_eq!(db.state(doc).await.snapshot_checked_rev, 5, "it was probed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stamped_snapshot_with_unlogged_content_becomes_a_gap_entry() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    for t in ["one", "two", "three"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    let (with_x, x) = with_paragraph(9, &db.state_at(doc, 2).await, "unlogged x");
    db.snapshot(doc, 2, &with_x).await;
    let b = db.builder(limits());
    build_clean(&b, doc).await;
    let entries = db.entries(doc).await;
    let gaps = of_kind(&entries, "gap");
    assert_eq!(gaps.len(), 1, "{entries:#?}");
    assert_eq!((gaps[0].start_rev, gaps[0].end_rev), (Some(2), Some(2)));
    assert!(gaps[0].author.is_none() && gaps[0].sealed);
    assert_eq!(gaps[0].inserted(), common::db::insertions(&x));
    let cps = db.checkpoints(doc).await;
    assert!(cps.iter().any(|c| c.rev == 2 && c.kind == "gap"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_snapshot_is_gap_checked_by_the_sweep() {
    let db = db().await;
    let b = db.builder(limits());
    for via_tick in [false, true] {
        let doc = db.page().await;
        let mut h = History::new(1);
        for t in ["one", "two", "three"] {
            let u = typed(&mut h, 0, t);
            db.append(doc, 0, &u).await;
        }
        build_clean(&b, doc).await;
        assert!(!db.head(doc).await.1);
        let (with_x, _) = with_paragraph(9, &db.state_at(doc, 3).await, "late x");
        db.snapshot(doc, 3, &with_x).await;
        if via_tick {
            let u = typed(&mut h, 0, "four");
            db.append(doc, 0, &u).await;
            assert!(b.tick().await >= 1);
        } else {
            assert!(b.sweep().await >= 1);
        }
        let entries = db.entries(doc).await;
        let gaps = of_kind(&entries, "gap");
        assert_eq!(gaps.len(), 1, "via tick {via_tick}: {entries:#?}");
        assert_eq!(gaps[0].end_rev, Some(3));
    }
}

fn holds(state: &[u8], text: &str) -> bool {
    visible(state).to_string().contains(text)
}

async fn truncate_checkpoint(db: &Db, doc: Uuid, rev: i64) {
    sqlx::query(
        "UPDATE doc_history_checkpoints
            SET state_bytes = substring(state_bytes from 1 for octet_length(state_bytes) / 2)
          WHERE doc_id = $1 AND rev = $2",
    )
    .bind(doc)
    .bind(rev)
    .execute(&db.pool)
    .await
    .expect("truncate");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_keeps_gap_content_whose_snapshot_is_gone() {
    let db = db().await;
    let doc = db.page().await;
    let extras = page_with_gaps(&db, doc, &[(2, "gap x")], false).await;
    let before = db.entries(doc).await;
    let gap = of_kind(&before, "gap");
    assert_eq!(gap.len(), 1);
    let found_at = gap[0].started_at;
    db.exec("DELETE FROM doc_snapshots WHERE doc_id = $1", doc)
        .await;
    db.exec(
        "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
        doc,
    )
    .await;
    let mut h = History::new(2);
    let u = typed(&mut h, 1, "five");
    db.append(doc, 1, &u).await;
    let b = db.builder(limits());
    let results = build_until(&b, doc, is_built).await;
    assert_eq!(results[0], genesis(0), "{results:?}");
    let entries = db.entries(doc).await;
    let gaps = of_kind(&entries, "gap");
    assert_eq!(gaps.len(), 1, "{entries:#?}");
    assert_eq!(gaps[0].end_rev, Some(2));
    assert_eq!(gaps[0].started_at, found_at, "the time it was first found");
    assert_eq!(gaps[0].inserted(), common::db::insertions(&extras[0]));
    let cps = db.checkpoints(doc).await;
    assert!(cps.iter().any(|c| c.rev == 2 && c.kind == "gap"));
    for c in cps.iter().filter(|c| c.rev >= 2) {
        assert!(
            holds(&c.state_bytes, "gap x"),
            "checkpoint {} lacks X",
            c.rev
        );
    }
    // A's (and B's) sessions are those of a twin that never had X.
    let twin = twin_of(&db, doc).await;
    build_clean(&b, twin).await;
    assert_eq!(
        keys(&db.entries(twin).await),
        keys(
            &entries
                .iter()
                .filter(|e| e.kind != "gap")
                .cloned()
                .collect::<Vec<_>>()
        )
    );

    // A second gap checkpoint survives a rebuild that drops the first.
    let doc = db.page().await;
    let extras = page_with_gaps(&db, doc, &[(2, "gap x"), (3, "gap y")], true).await;
    db.exec(
        "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
        doc,
    )
    .await;
    let two = db.builder(limits_with(|l| l.max_rows = 2));
    assert_eq!(two.build_page(doc).await, genesis(0));
    assert!(is_built(&two.build_page(doc).await));
    assert_eq!(db.state(doc).await.built_rev, 2);
    truncate_checkpoint(&db, doc, 2).await;
    let u = typed(&mut h, 1, "six");
    db.append(doc, 1, &u).await;
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!(
        s.builder_version, 0,
        "a rejected gap start rebuilds the page"
    );
    let cps = db.checkpoints(doc).await;
    assert!(!cps.iter().any(|c| c.rev == 2), "{cps:?}");
    assert!(cps.iter().any(|c| c.rev == 3 && c.kind == "gap"));
    db.retry_now(doc).await;
    build_until(&b, doc, is_built).await;
    let entries = db.entries(doc).await;
    let gaps = of_kind(&entries, "gap");
    assert_eq!(gaps.len(), 1, "{entries:#?}");
    assert_eq!(gaps[0].end_rev, Some(3));
    // The kept gap at 3 is the replay's state after that merge, X
    // included: X's entry at 2 is gone, its content is reported at 3.
    let at3 = gaps[0].inserted();
    for x in &extras {
        assert!(common::db::insertions(x).diff(&at3).is_empty());
    }
    let last = db.checkpoints(doc).await.pop().unwrap();
    assert!(holds(&last.state_bytes, "gap y"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_that_cannot_merge_a_kept_gap_drops_only_that_gap() {
    let db = db().await;
    let doc = db.page().await;
    page_with_gaps(&db, doc, &[(2, "gap x"), (3, "gap y")], true).await;
    db.exec(
        "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
        doc,
    )
    .await;
    let hooked = db.builder(limits_with(|l| {
        l.hooks.replay.corrupt_kept_gap_at = Some(2);
    }));
    assert_eq!(hooked.build_page(doc).await, genesis(0));
    assert_eq!(hooked.build_page(doc).await, PassResult::Failed);
    let cps = db.checkpoints(doc).await;
    assert!(
        !cps.iter().any(|c| c.rev == 2 && c.kind == "gap"),
        "{cps:?}"
    );
    assert!(cps.iter().any(|c| c.rev == 3 && c.kind == "gap"));
    db.retry_now(doc).await;
    let b = db.builder(limits());
    build_until(&b, doc, is_built).await;
    let entries = db.entries(doc).await;
    let gaps = of_kind(&entries, "gap");
    assert_eq!(gaps.len(), 1, "{entries:#?}");
    assert_eq!(gaps[0].end_rev, Some(3));
}

/// Client 9 creates paragraph P (empty text, never logged); B types `text`
/// into it on a doc that has P: B's update parks wherever P is missing.
/// Returns (P's update, B's update).
fn text_in_unlogged_paragraph(text: &str) -> (Vec<u8>, Vec<u8>) {
    let p = Client::new(9);
    let p_update = p.txn(|txn, frag| {
        let para = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
        para.push_back(txn, XmlTextPrelim::new(""));
    });
    let b = Client::new(12);
    b.doc
        .transact_mut()
        .apply_update(yrs::Update::decode_v1(&p_update).unwrap())
        .unwrap();
    let b_update = b.txn(|txn, frag| {
        let para = frag
            .get(txn, 0)
            .and_then(|n| n.into_xml_element())
            .expect("P");
        let t = para
            .get(txn, 0)
            .and_then(|n| n.into_xml_text())
            .expect("P's text");
        t.insert(txn, 0, text);
    });
    (p_update, b_update)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_gap_credit_is_counted_in_its_session() {
    let db = db().await;
    let b = db.builder(limits());
    for own_row in [false, true] {
        let doc = db.page().await;
        let since = Utc::now() - chrono::Duration::hours(2);
        db.backdate(doc, since).await;
        let mut h = History::new(1);
        let x = typed(&mut h, 1, "x");
        db.insert_row(doc, Some(author(1)), &x, Some(UpdateSource::Edit), since)
            .await;
        let (p, drained) = text_in_unlogged_paragraph("drained words here");
        db.insert_row(
            doc,
            Some(author(1)),
            &drained,
            Some(UpdateSource::Edit),
            since + minutes(1),
        )
        .await;
        let a = typed(&mut h, 0, "a now");
        db.append(doc, 0, &a).await;
        let mut rows = vec![x.clone(), drained.clone(), a.clone()];
        if own_row {
            let own = typed(&mut h, 1, "own");
            db.append(doc, 1, &own).await;
            rows.push(own);
        }
        let head = rows.len() as i64;
        build_clean(&b, doc).await;
        let first = sessions(&db.entries(doc).await)
            .into_iter()
            .find(|e| e.author == Some(author(1)) && e.start_rev == Some(1))
            .cloned()
            .expect("B's session over rev 1");
        assert!(first.sealed, "sealed by the idle rule");
        // The room's snapshot holds P and B's text.
        let mut all: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
        all.push(&p);
        db.snapshot(doc, head, &state_of(all)).await;
        build_clean(&b, doc).await;
        let credited = sessions(&db.entries(doc).await)
            .into_iter()
            .find(|e| e.author == Some(author(1)) && e.end_rev == Some(head))
            .cloned()
            .expect("B's session credited at the head");
        assert_eq!(
            credited.meta.get("gap_credit_rev").and_then(|v| v.as_i64()),
            Some(head),
            "{credited:?}"
        );
        if !own_row {
            assert_eq!(credited.start_rev, Some(head));
        }
        sqlx::query(
            "UPDATE doc_history_entries SET ended_at = ended_at - interval '22 minutes'
              WHERE id = $1",
        )
        .bind(credited.id)
        .execute(&db.pool)
        .await
        .unwrap();
        build_clean(&b, doc).await;
        let counted = db
            .entries(doc)
            .await
            .into_iter()
            .find(|e| e.id == credited.id)
            .unwrap();
        assert!(counted.sealed);
        assert_eq!(
            counted.words_added,
            Some(if own_row { 4 } else { 3 }),
            "{counted:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn late_snapshots_count_toward_the_replay_cap() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    let big = typed(&mut h, 0, &filler(40 * 1024, "shared"));
    db.append(doc, 0, &big).await;
    for t in ["r one", "r two", "r three"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    let normal = db.builder(limits());
    build_clean(&normal, doc).await;
    let revs = [2_i64, 3, 4];
    for (i, &r) in revs.iter().enumerate() {
        let (s, _) = with_paragraph(
            30 + i as u64,
            &db.state_at(doc, r).await,
            &format!("extra {i}"),
        );
        db.snapshot(doc, r, &s).await;
    }
    for &r in &revs {
        let base = base_of(&db, doc).await;
        let mut sizes: Vec<i64> = sqlx::query_scalar(
            "SELECT octet_length(n.state_bytes)::bigint FROM doc_snapshots n
               JOIN doc_history_state s USING (doc_id)
              WHERE n.doc_id = $1 AND n.snapshot_rev > s.snapshot_checked_rev",
        )
        .bind(doc)
        .fetch_all(&db.pool)
        .await
        .unwrap();
        sizes.sort_unstable();
        let largest = *sizes.last().unwrap();
        if sizes.len() >= 2 {
            assert!(sizes[0] + sizes[1] > largest + 512, "{sizes:?}");
        }
        let capped = db.builder(limits_with(|l| l.max_input_bytes = base + largest + 512));
        assert!(is_built(&capped.build_page(doc).await));
        let s = db.state(doc).await;
        assert_eq!(s.snapshot_checked_rev, r, "one snapshot per pass");
        assert!(!s.given_up);
    }
    let entries = db.entries(doc).await;
    let gaps = of_kind(&entries, "gap");
    assert_eq!(gaps.len(), 3, "{entries:#?}");
    assert!(gaps.iter().all(|g| g.end_rev == Some(4)));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_snapshot_is_left_unchecked_not_given_up() {
    let db = db().await;
    let (doc, cap) = oversized_snapshot_page(&db).await;
    let b = db.builder(limits_with(|l| l.max_input_bytes = cap));
    assert!(is_built(&b.build_page(doc).await));
    let s = db.state(doc).await;
    assert!(s.retry_null && !s.given_up);
    assert!(s.snapshot_checked_rev >= 1);
    assert!(of_kind(&db.entries(doc).await, "gap").is_empty());
}

// ---------------------------------------------------------------------------
// Passes, concurrency, leases
// ---------------------------------------------------------------------------

/// `n` generated rows (seed `seed`), with their own times and authors.
fn generated_rows(seed: u64, n: usize) -> Vec<knot_storage::DocUpdate> {
    let mut h = History::new(seed);
    while h.rows().len() < n {
        h.step();
    }
    h.rows()[..n].to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn small_passes_build_what_one_pass_builds() {
    let db = db().await;
    let rows = generated_rows(5, 40);
    let since = rows[0].created_at - chrono::Duration::hours(1);
    let (small_doc, big_doc) = (db.page().await, db.page().await);
    for doc in [small_doc, big_doc] {
        db.backdate(doc, since).await;
        db.insert_rows(doc, &rows).await;
    }
    let small = db.builder(limits_with(|l| {
        l.max_rows = 3;
        l.checkpoint_floor = 256;
    }));
    let big = db.builder(limits());
    let passes = build_clean(&small, small_doc).await;
    assert!(passes.len() > 10, "{passes:?}");
    build_clean(&big, big_doc).await;
    let (a, b) = (db.entries(small_doc).await, db.entries(big_doc).await);
    assert!(
        sessions(&a).iter().any(|e| e.words_added.is_some()),
        "word counts compared too"
    );
    assert_eq!(keys(&a), keys(&b));
    let periodic = db
        .checkpoints(small_doc)
        .await
        .iter()
        .filter(|c| c.kind == "periodic")
        .count();
    assert!(periodic > 0, "the small floor places checkpoints");
}

#[tokio::test(flavor = "multi_thread")]
async fn appends_racing_passes_settle_clean() {
    let db = Arc::new(db().await);
    let doc = db.page().await;
    let deadlocks = db.deadlocks().await;
    let floor = || limits_with(|l| l.checkpoint_floor = 256);
    let (b1, b2) = (db.builder(floor()), db.builder(floor()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let running =
        |stop: &Arc<std::sync::atomic::AtomicBool>| !stop.load(std::sync::atomic::Ordering::SeqCst);

    let typed_rows: Vec<Vec<u8>> = {
        let mut h = History::new(3);
        (0..1000)
            .map(|i| typed(&mut h, i % 2, &format!("row {i} lorem ipsum dolor")))
            .collect()
    };
    let appender = {
        let (db, stop) = (db.clone(), stop.clone());
        tokio::spawn(async move {
            let mut slowest = Duration::ZERO;
            let mut i = 0;
            while running(&stop) && i < typed_rows.len() {
                let u = &typed_rows[i];
                let t = Instant::now();
                db.append(doc, i % 2, u).await;
                slowest = slowest.max(t.elapsed());
                i += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            slowest
        })
    };
    let snapshotter = {
        let (db, stop) = (db.clone(), stop.clone());
        tokio::spawn(async move {
            while running(&stop) {
                let head: Option<i64> =
                    sqlx::query_scalar("SELECT max(rev) FROM doc_updates WHERE doc_id = $1")
                        .bind(doc)
                        .fetch_one(&db.pool)
                        .await
                        .unwrap();
                if let Some(head) = head {
                    let state = db.state_at(doc, head).await;
                    db.snapshot(doc, head, &state).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    let builders: Vec<_> = [b1.clone(), b2.clone()]
        .into_iter()
        .map(|b| {
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut results = Vec::new();
                while running(&stop) {
                    results.push(b.build_page(doc).await);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                results
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(3)).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let slowest = appender.await.unwrap();
    snapshotter.await.unwrap();
    for b in builders {
        for r in b.await.unwrap() {
            assert_ne!(r, PassResult::Failed);
        }
    }
    db.expire_lease(doc).await;
    build_clean(&b1, doc).await;
    let (head_rev, dirty) = db.head(doc).await;
    assert!(!dirty);
    assert_eq!(db.state(doc).await.built_rev, head_rev);
    let twin = twin_of(&db, doc).await;
    build_clean(&db.builder(limits()), twin).await;
    assert_eq!(keys(&db.entries(doc).await), keys(&db.entries(twin).await));
    assert!(
        slowest < knot_storage::APPEND_STATEMENT_TIMEOUT,
        "{slowest:?}"
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(db.deadlocks().await, deadlocks);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_builders_never_build_one_page_at_once() {
    let db = db().await;
    let deadlocks = db.deadlocks().await;
    let (doc, _) =
        page_with_rows(&db, &[(0, "a", 0), (1, "b", 1), (0, "c", 40), (2, "d", 41)]).await;
    let (b1, b2) = (db.builder(limits()), db.builder(limits()));
    for _ in 0..20 {
        let (r1, r2) = tokio::join!(b1.build_page(doc), b2.build_page(doc));
        for r in [r1, r2] {
            assert!(
                matches!(
                    r,
                    PassResult::Built { .. }
                        | PassResult::Genesis { .. }
                        | PassResult::Clean
                        | PassResult::Busy
                        | PassResult::Deferred
                ),
                "{r:?}"
            );
        }
    }
    db.expire_lease(doc).await;
    build_clean(&b1, doc).await;
    let twin = twin_of(&db, doc).await;
    build_clean(&b1, twin).await;
    assert_eq!(keys(&db.entries(doc).await), keys(&db.entries(twin).await));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(db.deadlocks().await, deadlocks);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clean_page_is_not_rebuilt_by_a_late_claimant() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    for t in ["one", "two"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    let (a, b) = (db.builder(limits()), db.builder(limits()));
    assert_eq!(a.build_page(doc).await, genesis(0));
    assert!(is_built(&a.build_page(doc).await));
    let cps: Vec<_> = db
        .checkpoints(doc)
        .await
        .into_iter()
        .map(|c| (c.rev, c.created_at))
        .collect();
    let before = db.state(doc).await;
    assert_eq!(b.build_page(doc).await, PassResult::Clean);
    let after = db.state(doc).await;
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!((after.claimed_by, after.failures), (None, 0));
    let cps_after: Vec<_> = db
        .checkpoints(doc)
        .await
        .into_iter()
        .map(|c| (c.rev, c.created_at))
        .collect();
    assert_eq!(cps_after, cps);
    // A stamped snapshot above snapshot_checked_rev is work.
    let (s, _) = with_paragraph(9, &db.state_at(doc, 2).await, "late");
    db.snapshot(doc, 2, &s).await;
    assert!(is_built(&b.build_page(doc).await));
    assert_eq!(b.build_page(doc).await, PassResult::Clean);
    // So is a session idle for 22 minutes.
    db.exec(
        "UPDATE doc_history_entries SET ended_at = ended_at - interval '22 minutes'
          WHERE doc_id = $1 AND NOT sealed",
        doc,
    )
    .await;
    assert!(is_built(&b.build_page(doc).await));
    assert!(db.entries(doc).await.iter().all(|e| e.sealed));
}

async fn wait_for(what: &str, mut f: impl AsyncFnMut() -> bool) {
    let t = Instant::now();
    while !f().await {
        assert!(t.elapsed() < Duration::from_secs(10), "waited for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn claimed_by(db: &Db, doc: Uuid, id: Uuid) {
    wait_for("the claim", async || {
        sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT claimed_by FROM doc_history_state WHERE doc_id = $1",
        )
        .bind(doc)
        .fetch_optional(&db.pool)
        .await
        .unwrap()
        .flatten()
            == Some(id)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_build_racing_another_builder_is_busy_not_failed() {
    let db = db().await;
    let doc = db.page().await;
    let a = db.builder(holding(Duration::from_secs(6)));
    let b = db.builder(limits());
    let first = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let t = Instant::now();
    let r = b.build_page(doc).await;
    assert!(t.elapsed() < Duration::from_millis(1500));
    assert!(
        matches!(r, PassResult::Busy | PassResult::Deferred),
        "{r:?}"
    );
    assert_eq!(first.await.unwrap(), genesis(0));
    let s = db.state(doc).await;
    assert_eq!((s.failures, s.last_error), (0, None));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pass_that_lost_its_lease_writes_nothing() {
    let db = Arc::new(db().await);
    // The row write.
    let doc = db.page().await;
    let mut h = History::new(1);
    for t in ["one", "two"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    let b = db.builder(limits());
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert!(is_built(&b.build_page(doc).await));
    for t in ["three", "four"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    let a = db.builder(holding(Duration::from_secs(3)));
    let pass = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    claimed_by(&db, doc, a.id()).await;
    db.expire_lease(doc).await;
    assert!(is_built(&b.build_page(doc).await));
    let entries = db.entries(doc).await;
    let cps: Vec<_> = db
        .checkpoints(doc)
        .await
        .into_iter()
        .map(|c| (c.rev, c.kind))
        .collect();
    let s = db.state(doc).await;
    assert_eq!(pass.await.unwrap(), PassResult::Failed);
    assert_eq!(db.entries(doc).await, entries);
    let cps_after: Vec<_> = db
        .checkpoints(doc)
        .await
        .into_iter()
        .map(|c| (c.rev, c.kind))
        .collect();
    assert_eq!(cps_after, cps);
    let after = db.state(doc).await;
    assert_eq!((after.built_rev, &after.parked), (s.built_rev, &s.parked));
    assert_eq!(
        (after.last_error, after.failures, after.claimed_by),
        (None, 0, None),
        "what B's release left"
    );

    // The word count's write.
    let (doc, _) = page_with_rows(&db, &[(0, "alpha beta", 0), (0, "gamma", 30)]).await;
    let a = db.builder(limits_with(|l| l.hooks.hold_words = Duration::from_secs(3)));
    assert_eq!(a.build_page(doc).await, genesis(0));
    let pass = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    // A took a count: one sealed session is no longer pending and has none.
    wait_for("the count's take", async || {
        db.entries(doc)
            .await
            .iter()
            .any(|e| e.sealed && !e.words_pending && e.words_added.is_none())
    })
    .await;
    let taken = db
        .entries(doc)
        .await
        .into_iter()
        .find(|e| e.sealed && !e.words_pending && e.words_added.is_none())
        .unwrap();
    db.expire_lease(doc).await;
    assert!(is_built(&b.build_page(doc).await));
    let s = db.state(doc).await;
    assert_eq!(pass.await.unwrap(), PassResult::Failed);
    let after = db
        .entries(doc)
        .await
        .into_iter()
        .find(|e| e.id == taken.id)
        .unwrap();
    assert_eq!(
        (after.words_added, after.words_removed, after.words_pending),
        (None, None, false)
    );
    let s2 = db.state(doc).await;
    assert_eq!(
        (s2.updated_at, s2.claimed_by, s2.failures, s2.last_error),
        (s.updated_at, s.claimed_by, s.failures, s.last_error)
    );

    // The release in its own transaction.
    let (doc, _) = page_with_rows(&db, &[(0, "alpha beta", 0), (0, "gamma", 30)]).await;
    let a = db.builder(limits_with(|l| {
        l.words_max_input_bytes = 1;
        l.hooks.hold_before_release = Duration::from_secs(3);
    }));
    assert_eq!(a.build_page(doc).await, genesis(0));
    let pass = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    wait_for("the skipped counts", async || {
        let e = db.entries(doc).await;
        e.iter().any(|e| e.sealed) && e.iter().all(|e| !e.words_pending)
    })
    .await;
    let dirty = db.head(doc).await.1;
    db.expire_lease(doc).await;
    let b5 = db.builder(holding(Duration::from_secs(5)));
    let second = {
        let b5 = b5.clone();
        tokio::spawn(async move { b5.build_page(doc).await })
    };
    claimed_by(&db, doc, b5.id()).await;
    assert_eq!(pass.await.unwrap(), PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!(s.claimed_by, Some(b5.id()));
    assert!(
        s.leased && s.deferred,
        "B's claim wrote its lease and marker"
    );
    assert_eq!(s.failures, 2, "A's claim, then B's");
    assert_eq!(db.head(doc).await.1, dirty, "A did not release");
    assert!(is_built(&second.await.unwrap()));
}

/// A pass whose own lease expired (nobody claimed the page since) records
/// a failure when its write finds the lease gone, so the claim's crash
/// marker does not make the next claim count a crash.
#[tokio::test(flavor = "multi_thread")]
async fn a_pass_that_outlived_its_lease_is_not_counted_as_a_crash() {
    let db = db().await;
    let (doc, _) = page_with_rows(&db, &[(0, "alpha beta", 0), (0, "gamma", 30)]).await;
    let a = db.builder(limits_with(|l| l.hooks.hold_words = Duration::from_secs(2)));
    assert_eq!(a.build_page(doc).await, genesis(0));
    let pass = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    wait_for("the count's take", async || {
        db.entries(doc)
            .await
            .iter()
            .any(|e| e.sealed && !e.words_pending && e.words_added.is_none())
    })
    .await;
    db.expire_lease(doc).await;
    assert_eq!(pass.await.unwrap(), PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!(s.claimed_by, None, "{s:?}");
    assert!(
        s.last_error
            .as_deref()
            .is_some_and(|e| e.starts_with("lease lost")),
        "{s:?}"
    );
    db.retry_now(doc).await;
    let b = db.builder(holding(Duration::from_secs(2)));
    let next = {
        let b = b.clone();
        tokio::spawn(async move { b.build_page(doc).await })
    };
    claimed_by(&db, doc, b.id()).await;
    assert_eq!(db.state(doc).await.crashes, 0, "no crash counted");
    next.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_pass_holds_no_lock_on_the_page() {
    let db = db().await;
    let parent = db.page().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(doc, 0, &u).await;
    db.docs
        .set_restricted(db.ws, doc, author(0), true)
        .await
        .expect("restrict");
    let grants = PgGrantStore::new(db.pool.clone());
    let b = db.builder(holding(Duration::from_secs(3)));
    let pass = {
        let b = b.clone();
        tokio::spawn(async move { b.build_page(doc).await })
    };
    claimed_by(&db, doc, b.id()).await;
    let quick = Duration::from_millis(500);
    tokio::time::timeout(
        quick,
        db.docs.move_to(db.ws, doc, author(0), Some(parent), "m"),
    )
    .await
    .expect("move_to waited")
    .expect("move_to");
    tokio::time::timeout(quick, db.docs.archive(db.ws, doc, author(0)))
        .await
        .expect("archive waited")
        .expect("archive");
    tokio::time::timeout(quick, db.docs.restore(db.ws, doc, author(0)))
        .await
        .expect("restore waited")
        .expect("restore");
    let _ = tokio::time::timeout(
        quick,
        grants.delete(db.ws, doc, &format!("user:{}", author(0)), author(0)),
    )
    .await
    .expect("the grant removal waited");
    assert_eq!(pass.await.unwrap(), genesis(0));

    // A purge during the compute: the pass finds the page gone.
    let u = typed(&mut h, 0, "two");
    db.append(doc, 0, &u).await;
    let pass = {
        let b = b.clone();
        tokio::spawn(async move { b.build_page(doc).await })
    };
    claimed_by(&db, doc, b.id()).await;
    db.docs
        .archive(db.ws, doc, author(0))
        .await
        .expect("archive");
    tokio::time::timeout(quick, async {
        db.docs.begin_purge(db.ws, doc).await.expect("begin_purge");
        db.docs
            .purge(db.ws, doc, Some(author(0)))
            .await
            .expect("purge")
    })
    .await
    .expect("the purge waited");
    assert_eq!(pass.await.unwrap(), PassResult::Gone);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_purge_racing_builder_passes_never_deadlocks() {
    let db = Arc::new(db().await);
    let deadlocks = db.deadlocks().await;
    let mut pages = Vec::new();
    let mut h = History::new(1);
    for i in 0..30 {
        let doc = db.page().await;
        for j in 0..3 {
            let u = typed(&mut h, (i + j) % 3, &format!("page {i} row {j}"));
            db.append(doc, (i + j) % 3, &u).await;
        }
        pages.push(doc);
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let loops: Vec<_> = (0..2)
        .map(|_| {
            let (b, pages, stop) = (db.builder(limits()), pages.clone(), stop.clone());
            tokio::spawn(async move {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    for &doc in &pages {
                        b.build_page(doc).await;
                    }
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    for &doc in &pages {
        db.docs
            .archive(db.ws, doc, author(0))
            .await
            .expect("archive");
        db.docs.begin_purge(db.ws, doc).await.expect("begin_purge");
        db.docs
            .purge(db.ws, doc, Some(author(0)))
            .await
            .expect("purge");
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for l in loops {
        l.await.unwrap();
    }
    for table in [
        "doc_history_state",
        "doc_history_checkpoints",
        "doc_history_entries",
    ] {
        let left: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE doc_id = ANY($1)"
        )))
        .bind(&pages)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(left, 0, "{table}");
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(db.deadlocks().await, deadlocks);
}

// ---------------------------------------------------------------------------
// Crashes, timeouts, pacing, leases
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_pass_that_dies_mid_compute_defers_the_page() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(doc, 0, &u).await;
    let a = db.builder_with(holding(Duration::from_secs(5)), history_permits(2));
    let b = db.builder(limits());
    let dying = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    dying.abort();
    let _ = dying.await;
    assert_eq!(b.build_page(doc).await, PassResult::Deferred);
    let s = db.state(doc).await;
    assert_eq!((s.failures, s.claimed_by), (1, Some(a.id())));
    assert!(s.deferred);
    assert_eq!(s.last_error.as_deref(), Some("pass interrupted"));
    db.exec(
        "UPDATE doc_history_state
            SET retry_after = now() - interval '1 s', claimed_until = now() - interval '1 s'
          WHERE doc_id = $1",
        doc,
    )
    .await;
    // B's claim counts A's crash; its release clears it.
    let held = {
        let pool = db.pool.clone();
        tokio::spawn(async move {
            let mut seen = 0;
            for _ in 0..200 {
                let c: Option<i32> = sqlx::query_scalar(
                    "SELECT crashes FROM doc_history_state WHERE doc_id = $1 AND claimed_by IS NOT NULL",
                )
                .bind(doc)
                .fetch_optional(&pool)
                .await
                .unwrap();
                if let Some(c) = c {
                    seen = seen.max(c);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            seen
        })
    };
    let slow_b = db.builder(holding(Duration::from_millis(500)));
    assert_eq!(slow_b.build_page(doc).await, genesis(0));
    assert_eq!(held.await.unwrap(), 1, "the claim counted A's crash");
    let s = db.state(doc).await;
    assert_eq!((s.failures, s.crashes, s.claimed_by), (0, 0, None));
    let u = typed(&mut h, 0, "two");
    db.append(doc, 0, &u).await;
    assert!(
        is_built(&a.build_page(doc).await),
        "the aborted future's in-flight guard was dropped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_replay_times_out_and_backs_off_the_page() {
    let db = db().await;
    let (p, q) = (db.page().await, db.page().await);
    let b = db.builder_with(
        limits_with(|l| {
            l.replay_timeout = Duration::from_secs(1);
            l.hooks.hold_compute = Duration::from_secs(4);
            l.hooks.hold_compute_doc = Some(p);
        }),
        history_permits(1),
    );
    let t0 = Instant::now();
    assert_eq!(b.build_page(p).await, PassResult::Failed);
    assert!(t0.elapsed() < Duration::from_secs(2));
    let s = db.state(p).await;
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .contains("replay timed out"),
        "{s:?}"
    );
    assert!(s.deferred && !s.given_up);
    assert_eq!((s.claimed_by, s.timeouts), (None, 1));
    assert!(s.skip_revs.is_empty());
    assert_eq!(b.build_page(q).await, genesis(0));
    assert!(
        t0.elapsed() >= Duration::from_millis(3500),
        "the permit rode the held thread"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_row_held_just_past_the_timeout_never_skips_the_next_row() {
    let db = db().await;
    let normal = db.builder(limits());
    for short in [false, true] {
        let doc = db.page().await;
        assert_eq!(normal.build_page(doc).await, genesis(0));
        let mut h = History::new(1);
        for t in ["one", "two", "three"] {
            let u = typed(&mut h, 0, t);
            db.append(doc, 0, &u).await;
        }
        let b = db.builder(limits_with(|l| {
            l.replay_timeout = Duration::from_secs(1);
            if short {
                l.hooks.hold_compute = Duration::from_millis(800);
                l.hooks.replay.sleep_at_rev = Some((2, Duration::from_millis(400)));
            } else {
                l.hooks.replay.sleep_at_rev = Some((2, Duration::from_millis(1300)));
            }
        }));
        assert_eq!(b.build_page(doc).await, PassResult::Failed);
        let s = db.state(doc).await;
        if short {
            assert!(
                s.skip_revs.is_empty(),
                "Row(2) held < half the timeout: {s:?}"
            );
            assert!(s.deferred);
        } else {
            assert_eq!(s.skip_revs, vec![2], "the frozen stage, never row 3");
            tokio::time::sleep(Duration::from_secs(1)).await;
            let s = db.state(doc).await;
            assert_eq!((s.skip_revs, s.built_rev), (vec![2], 0));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replay_that_hangs_while_encoding_a_checkpoint_is_given_up() {
    let db = db().await;
    let doc = db.page().await;
    let b = db.builder(limits_with(|l| {
        l.replay_timeout = Duration::from_millis(500);
        l.hooks.replay.sleep_at_emit = Some(Duration::from_millis(1500));
    }));
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert!(s.given_up);
    assert!(
        s.last_error
            .as_deref()
            .unwrap()
            .contains("checkpoint encode"),
        "{s:?}"
    );
    assert!(s.skip_revs.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hang_with_nothing_to_blame_is_given_up_at_its_second_timeout() {
    let db = db().await;
    let (p, q) = (db.page().await, db.page().await);
    let hang = || {
        limits_with(|l| {
            l.replay_timeout = Duration::from_millis(500);
            l.hooks.hold_compute = Duration::from_secs(3);
            l.hooks.hold_compute_doc = Some(p);
        })
    };
    let a = db.builder_with(hang(), history_permits(1));
    let b = db.builder_with(hang(), history_permits(1));
    let t0 = Instant::now();
    assert_eq!(a.build_page(p).await, PassResult::Failed);
    let s = db.state(p).await;
    assert_eq!(s.timeouts, 1);
    assert!(!s.given_up && s.deferred);
    db.exec(
        "UPDATE doc_history_state SET retry_after = now() - interval '1 s' WHERE doc_id = $1",
        p,
    )
    .await;
    assert_eq!(b.build_page(p).await, PassResult::Failed);
    let s = db.state(p).await;
    assert!(s.given_up);
    assert_eq!(s.timeouts, 2);
    assert!(
        s.last_error.as_deref().unwrap().contains("timed out"),
        "{s:?}"
    );
    assert_eq!(
        db.builder(limits()).build_page(p).await,
        PassResult::Deferred
    );
    tokio::time::sleep(
        Duration::from_secs(3).saturating_sub(t0.elapsed()) + Duration::from_millis(200),
    )
    .await;
    assert_eq!(
        a.build_page(q).await,
        genesis(0),
        "A's replica is not stuck"
    );
    // A release resets the count.
    let r = db.page().await;
    let normal = db.builder(limits());
    assert_eq!(normal.build_page(r).await, genesis(0));
    db.exec(
        "UPDATE doc_history_state SET timeouts = 1 WHERE doc_id = $1",
        r,
    )
    .await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(r, 0, &u).await;
    assert!(is_built(&normal.build_page(r).await));
    assert_eq!(db.state(r).await.timeouts, 0);
}

/// A page created a day before `history_since` with two baselines (older
/// "older", newer "older" + "newer") and a legacy row typing "legacy row";
/// returns the page, the baselines' ids (older first) and states, and the
/// legacy row.
async fn page_with_baselines(db: &Db, newest: Option<&[u8]>) -> (Uuid, [i64; 2], Vec<u8>, Vec<u8>) {
    let doc = db.page().await;
    db.exec(
        "UPDATE documents SET created_at = history_since - interval '1 day' WHERE id = $1",
        doc,
    )
    .await;
    let (older, _) = with_paragraph(40, &state_of([]), "older");
    let (newer, _) = with_paragraph(41, &older, "newer");
    let newest = newest.map_or(newer, <[u8]>::to_vec);
    let mut ids = [0; 2];
    for (i, (seq, state)) in [(1_i64, &older), (2, &newest)].into_iter().enumerate() {
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
    (doc, ids, older, legacy)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_baseline_whose_merge_hangs_is_skipped() {
    let db = db().await;
    let (doc, ids, older, legacy) = page_with_baselines(&db, None).await;
    let b = db.builder_with(
        limits_with(|l| {
            l.replay_timeout = Duration::from_millis(500);
            l.hooks.replay.sleep_at_baseline = Some((ids[1], Duration::from_secs(2)));
        }),
        history_permits(2),
    );
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!(s.skip_baselines, vec![ids[1]]);
    assert!(s.skip_revs.is_empty() && !s.given_up);
    db.exec(
        "UPDATE doc_history_state SET retry_after = now() - interval '1 s' WHERE doc_id = $1",
        doc,
    )
    .await;
    assert_eq!(b.build_page(doc).await, genesis(1));
    let cp = db.checkpoints(doc).await.remove(0);
    assert_eq!(cp.kind, "genesis");
    assert_eq!(
        visible(&cp.state_bytes),
        visible(&state_of([older.as_slice(), legacy.as_slice()]))
    );
    let entries = db.entries(doc).await;
    let mut listed: Vec<i64> = entries.iter().filter_map(|e| e.baseline_id).collect();
    listed.sort_unstable();
    assert_eq!(listed, ids.to_vec());
    let rollout: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == "baseline" && e.baseline_id.is_none())
        .collect();
    assert_eq!(
        rollout.len(),
        1,
        "genesis shows more than the older baseline"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_that_crashes_the_process_three_times_is_given_up() {
    let db = db().await;
    let doc = db.page().await;
    for i in 0..3 {
        let x = db.builder_with(holding(Duration::from_secs(5)), history_permits(1));
        let dying = tokio::spawn(async move { x.build_page(doc).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        dying.abort();
        let _ = dying.await;
        let s = db.state(doc).await;
        assert_eq!(s.last_error.as_deref(), Some("pass interrupted"));
        assert_eq!(
            s.crashes,
            i,
            "claim {} counted the crashes before it",
            i + 1
        );
        db.exec(
            "UPDATE doc_history_state SET claimed_until = now() - interval '1 s', retry_after = NULL
              WHERE doc_id = $1",
            doc,
        )
        .await;
    }
    assert_eq!(
        db.builder(limits()).build_page(doc).await,
        PassResult::Failed
    );
    let s = db.state(doc).await;
    assert!(s.given_up);
    assert!(s.last_error.as_deref().unwrap().contains("crash"), "{s:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_graceful_shutdown_is_not_counted_as_a_crash() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(doc, 0, &u).await;
    let a = db.builder(holding(Duration::from_secs(5)));
    let token = tokio_util::sync::CancellationToken::new();
    let handle = a.clone().spawn(token.clone());
    claimed_by(&db, doc, a.id()).await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("the builder stops")
        .unwrap();
    let s = db.state(doc).await;
    assert_eq!(
        (s.claimed_by, s.last_error.as_deref(), s.failures),
        (None, Some("shutdown"), 0)
    );
    assert!(s.retry_null);
    let b = db.builder(limits());
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert_eq!(db.state(doc).await.crashes, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_is_paced_by_the_cpu_share() {
    let db = db().await;
    let (p, q, r) = (db.page().await, db.page().await, db.page().await);
    let permits = history_permits(1);
    let paced = db.builder_with(
        limits_with(|l| {
            l.cpu_share = 0.5;
            l.hooks.hold_compute = Duration::from_secs(1);
        }),
        permits.clone(),
    );
    let other = db.builder_with(limits(), permits);
    let t0 = Instant::now();
    let first = {
        let paced = paced.clone();
        tokio::spawn(async move { paced.build_page(p).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(other.build_page(q).await, genesis(0));
    assert!(
        t0.elapsed() >= Duration::from_secs(2),
        "the pacing held the permit"
    );
    assert_eq!(first.await.unwrap(), genesis(0));
    let unpaced = db.builder(holding(Duration::from_secs(1)));
    let t1 = Instant::now();
    assert_eq!(unpaced.build_page(r).await, genesis(0));
    assert!(t1.elapsed() < Duration::from_millis(1800));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_built_at_an_older_version_with_maxed_failures_is_rebuilt_not_given_up() {
    let db = db().await;
    let b = db.builder(limits());
    for failures in [MAX_FAILURES, MAX_FAILURES - 1] {
        let (doc, _) = page_with_rows(&db, &[(0, "one", 0), (1, "two", 1)]).await;
        assert_eq!(b.build_page(doc).await, genesis(0));
        assert!(is_built(&b.build_page(doc).await));
        sqlx::query(
            "UPDATE doc_history_state
                SET builder_version = 0, failures = $2, retry_after = now() - interval '1 s'
              WHERE doc_id = $1",
        )
        .bind(doc)
        .bind(failures)
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(b.build_page(doc).await, genesis(0), "failures {failures}");
        assert!(is_built(&b.build_page(doc).await));
        let s = db.state(doc).await;
        assert!(s.retry_null);
        assert_eq!((s.failures, s.builder_version), (0, BUILDER_VERSION));
        let twin = twin_of(&db, doc).await;
        build_clean(&b, twin).await;
        assert_eq!(keys(&db.entries(doc).await), keys(&db.entries(twin).await));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_build_that_outlasts_one_lease_renews_it() {
    let db = Arc::new(db().await);
    let slow = |words: bool| {
        limits_with(|l| {
            l.lease = Duration::from_secs(3);
            l.replay_timeout = Duration::from_secs(2);
            l.words_timeout = Duration::from_secs(2);
            l.hooks.hold_compute = Duration::from_millis(1600);
            if words {
                l.hooks.hold_words = Duration::from_millis(1600);
            }
        })
    };
    // (a) two genesis computes: the newest baseline does not apply.
    let (doc, _, _, _) = page_with_baselines(&db, Some(b"\xff\xff\xff")).await;
    let a = db.builder(slow(false));
    let run = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    tokio::time::sleep(Duration::from_millis(2500)).await;
    db.retry_now(doc).await;
    assert_eq!(db.builder(limits()).build_page(doc).await, PassResult::Busy);
    assert_eq!(run.await.unwrap(), genesis(1));
    assert_eq!(db.state(doc).await.failures, 0);

    // (b) a pass and its forced count.
    let (doc, _) = page_with_rows(&db, &[(0, "alpha beta", 0), (0, "gamma", 30)]).await;
    assert_eq!(db.builder(limits()).build_page(doc).await, genesis(0));
    let a = db.builder(slow(true));
    let run = {
        let a = a.clone();
        tokio::spawn(async move { a.build_page(doc).await })
    };
    tokio::time::sleep(Duration::from_millis(2500)).await;
    db.retry_now(doc).await;
    assert_eq!(db.builder(limits()).build_page(doc).await, PassResult::Busy);
    assert!(is_built(&run.await.unwrap()));
    let counted = db.entries(doc).await;
    assert!(
        counted.iter().any(|e| e.sealed && e.words_added.is_some()),
        "{counted:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_row_after_small_rows_waits_for_its_own_pass() {
    const C: i64 = 64 * 1024;
    let db = db().await;
    let doc = db.page().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let b = db.builder(limits_with(|l| {
        l.max_input_bytes = C;
        l.checkpoint_floor = 256;
        l.hooks.input_log = Some(log.clone());
    }));
    let mut h = History::new(1);
    let first = typed(&mut h, 0, &filler(C as usize * 3 / 10, "first"));
    db.append(doc, 0, &first).await;
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert!(is_built(&b.build_page(doc).await));
    for i in 0..3 {
        let u = typed(&mut h, 0, &filler(C as usize / 10, &format!("small{i}")));
        db.append(doc, 0, &u).await;
    }
    let large = typed(&mut h, 0, &filler(C as usize / 2, "large"));
    db.append(doc, 0, &large).await;
    assert_eq!(
        b.build_page(doc).await,
        PassResult::Built {
            built_rev: 4,
            rows: 3
        },
        "the large row would cross the cap"
    );
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert!(s.given_up);
    assert!(s.last_error.as_deref().unwrap().contains("too large"));
    let log = log.lock().unwrap();
    assert!(log.iter().all(|&n| n <= C), "{log:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_snapshot_ahead_is_not_loaded() {
    const C: i64 = 64 * 1024;
    let db = db().await;
    let doc = db.page().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let b = db.builder(limits_with(|l| {
        l.max_input_bytes = C;
        l.hooks.input_log = Some(log.clone());
    }));
    let mut h = History::new(1);
    let first = typed(&mut h, 0, &filler(C as usize / 2, "base"));
    db.append(doc, 0, &first).await;
    build_clean(&b, doc).await;
    let base = base_of(&db, doc).await;
    for i in 0..10 {
        let u = typed(&mut h, 0, &filler(C as usize * 3 / 100, &format!("r{i}")));
        db.append(doc, 0, &u).await;
    }
    let (ahead, _) = with_paragraph(9, &state_of([]), &filler(C as usize * 4 / 10, "ahead"));
    db.snapshot(doc, 11, &ahead).await;
    log.lock().unwrap().clear();
    let r = b.build_page(doc).await;
    let PassResult::Built { built_rev, .. } = r else {
        panic!("{r:?}");
    };
    assert!(built_rev <= 10, "rows through r + 9 at most: {built_rev}");
    {
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert!(
            log[0] < base + ahead.len() as i64,
            "the snapshot was not loaded: {log:?}"
        );
    }
    build_clean(&b, doc).await;
    let s = db.state(doc).await;
    assert!(!s.given_up && s.snapshot_checked_rev >= 11);
    let log = log.lock().unwrap();
    assert!(log.iter().all(|&n| n <= C), "{log:?}");
}

// ---------------------------------------------------------------------------
// Give-ups and skips
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_page_given_up_at_genesis_still_lists_its_baselines() {
    let db = db().await;
    let doc = db.page().await;
    let mut created = Vec::new();
    for seq in [1_i64, 2] {
        let (state, _) = with_paragraph(40 + seq as u64, &state_of([]), &format!("b{seq}"));
        let row: (i64, chrono::DateTime<Utc>) = sqlx::query_as(
            "INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
             VALUES ($1, $2, $3, '\\x00', now() - make_interval(days => $2::int)) RETURNING id, created_at",
        )
        .bind(doc)
        .bind(seq)
        .bind(&state)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        created.push(row);
    }
    let tiny = db.builder(limits_with(|l| l.max_input_bytes = 1));
    assert_eq!(tiny.build_page(doc).await, PassResult::Failed);
    let check = async || {
        let entries = db.entries(doc).await;
        assert_eq!(entries.len(), 2, "{entries:#?}");
        for (id, at) in &created {
            let e = entries
                .iter()
                .find(|e| e.baseline_id == Some(*id))
                .expect("listed");
            assert_eq!(
                (e.kind.as_str(), e.author, e.ended_at),
                ("baseline", None, *at)
            );
        }
    };
    check().await;
    assert!(db.state(doc).await.given_up);
    db.exec(
        "UPDATE doc_history_state SET failures = 0, timeouts = 0, crashes = 0, retry_after = NULL
          WHERE doc_id = $1",
        doc,
    )
    .await;
    assert_eq!(db.builder(limits()).build_page(doc).await, genesis(0));
    check().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_that_keeps_failing_is_given_up() {
    let db = db().await;
    let b = db.builder(limits());
    let (doc, mut h) = page_with_rows(&db, &[(0, "one", 0)]).await;
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert!(is_built(&b.build_page(doc).await));
    let u = typed(&mut h, 0, "two");
    db.append(doc, 0, &u).await;
    db.exec(
        &format!("UPDATE doc_history_state SET failures = {MAX_FAILURES} WHERE doc_id = $1"),
        doc,
    )
    .await;
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert!(s.given_up);
    assert_eq!((s.builder_version, s.claimed_by), (BUILDER_VERSION, None));
    assert!(
        s.last_error.as_deref().unwrap().contains("failed passes"),
        "{s:?}"
    );
    assert_eq!(b.build_page(doc).await, PassResult::Deferred);
    assert!(!b.dirty_candidates().await.contains(&doc));
    db.exec(
        "UPDATE doc_history_state SET failures = 0, timeouts = 0, crashes = 0, retry_after = NULL
          WHERE doc_id = $1",
        doc,
    )
    .await;
    assert!(is_built(&b.build_page(doc).await));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_given_up_page_is_rebuilt_after_a_version_bump() {
    let db = db().await;
    let b = db.builder(limits());
    let (doc, _) = page_with_rows(&db, &[(0, "one", 0), (1, "two", 1)]).await;
    build_clean(&b, doc).await;
    let give_up_old = async || {
        db.exec(
            &format!("UPDATE doc_history_state SET failures = {MAX_FAILURES} WHERE doc_id = $1"),
            doc,
        )
        .await;
        db.exec(
            "UPDATE doc_history_heads SET dirty = true WHERE doc_id = $1",
            doc,
        )
        .await;
        assert_eq!(b.build_page(doc).await, PassResult::Failed);
        assert!(db.state(doc).await.given_up);
        db.exec(
            "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
            doc,
        )
        .await;
    };
    give_up_old().await;
    assert!(b.sweep_candidates().await.contains(&doc));
    assert_eq!(
        b.build_page(doc).await,
        genesis(0),
        "the claim lifted the give-up"
    );
    assert!(is_built(&b.build_page(doc).await));
    let s = db.state(doc).await;
    assert!(s.retry_null);
    assert_eq!(s.failures, 0);
    let twin = twin_of(&db, doc).await;
    build_clean(&b, twin).await;
    assert_eq!(keys(&db.entries(doc).await), keys(&db.entries(twin).await));

    // A failure right after the lift counts from 1.
    give_up_old().await;
    let hung = db.builder(limits_with(|l| {
        l.replay_timeout = Duration::from_millis(200);
        l.hooks.hold_compute = Duration::from_secs(1);
    }));
    assert_eq!(hung.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!(s.failures, 1);
    assert!(!s.given_up && s.deferred);
    db.exec(
        "UPDATE doc_history_state SET retry_after = now() - interval '1 s' WHERE doc_id = $1",
        doc,
    )
    .await;
    assert_eq!(
        b.build_page(doc).await,
        genesis(0),
        "retried, not given up again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_over_the_input_cap_is_given_up() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(doc, 0, &u).await;
    let b = db.builder(limits_with(|l| l.max_input_bytes = 1));
    assert_eq!(
        b.build_page(doc).await,
        genesis(0),
        "an empty genesis reads 0 bytes"
    );
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert!(s.given_up);
    assert!(s.last_error.as_deref().unwrap().contains("too large"));
    assert_eq!(s.builder_version, BUILDER_VERSION);
    let cps = db.checkpoints(doc).await;
    assert_eq!(cps.len(), 1);
    assert_eq!(cps[0].kind, "genesis");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_row_that_always_panics_is_skipped() {
    let db = db().await;
    let doc = db.page().await;
    let rows = genesis_then_rows(&db, doc).await;
    let b = db.builder(limits_with(|l| l.hooks.replay.panic_at_rev = Some(2)));
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!((s.skip_revs, s.failures), (vec![2], 1));
    db.retry_now(doc).await;
    assert_eq!(
        b.build_page(doc).await,
        PassResult::Built {
            built_rev: 3,
            rows: 2
        }
    );
    let row2 = common::db::insertions(&rows[1]);
    for e in db.entries(doc).await {
        assert!(e.inserted().intersect(&row2).is_empty(), "{e:?}");
    }
}

/// Builds A's rows ("line i ...", about 120 bytes each) with a 256-byte
/// checkpoint floor until the latest checkpoint is a periodic one below
/// `built_rev`, with at least `min` periodic ones; returns the latest.
async fn periodic_below_built(db: &Db, doc: Uuid, h: &mut History, min: usize) -> i64 {
    let b = db.builder(limits_with(|l| l.checkpoint_floor = 256));
    for i in 0..60 {
        let u = typed(
            h,
            0,
            &format!("line {i} of a: {}", "lorem ipsum ".repeat(8)),
        );
        db.append(doc, 0, &u).await;
        build_clean(&b, doc).await;
        let built = db.state(doc).await.built_rev;
        let latest = db
            .checkpoints(doc)
            .await
            .into_iter()
            .rfind(|c| c.rev <= built)
            .unwrap();
        let periodic = db
            .checkpoints(doc)
            .await
            .iter()
            .filter(|c| c.kind == "periodic")
            .count();
        if i >= 7 && latest.kind == "periodic" && latest.rev < built && periodic >= min {
            return latest.rev;
        }
    }
    panic!("no periodic checkpoint below built_rev");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prefix_row_that_panics_is_never_skipped() {
    let db = db().await;
    for corrupt in [false, true] {
        let doc = db.page().await;
        let mut h = History::new(1);
        let c = periodic_below_built(&db, doc, &mut h, 1).await;
        let entries = db.entries(doc).await;
        let u = typed(&mut h, 0, "the next row");
        let head = db.append(doc, 0, &u).await;
        let hooked = db.builder(limits_with(|l| {
            l.checkpoint_floor = 256;
            if corrupt {
                l.hooks.replay.corrupt_at_rev = Some(c + 1);
            } else {
                l.hooks.replay.panic_at_rev = Some(c + 1);
            }
        }));
        assert_eq!(hooked.build_page(doc).await, PassResult::Failed);
        let s = db.state(doc).await;
        assert!(s.skip_revs.is_empty(), "a prefix row is never skipped");
        assert!(
            !db.checkpoints(doc).await.iter().any(|cp| cp.rev == c),
            "the periodic checkpoint at {c} is dropped"
        );
        assert_eq!(db.entries(doc).await, entries);
        db.retry_now(doc).await;
        let b = db.builder(limits());
        assert!(matches!(
            b.build_page(doc).await,
            PassResult::Built { built_rev, .. } if built_rev == head
        ));
        let twin = twin_of(&db, doc).await;
        build_clean(&b, twin).await;
        assert_eq!(keys(&db.entries(doc).await), keys(&db.entries(twin).await));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_row_that_always_panics_in_genesis_is_skipped() {
    let db = db().await;
    let doc = db.page().await;
    let since = db.history_since(doc).await;
    let mut h = History::new(1);
    let mut rows = Vec::new();
    for t in ["l1", "l2", "l3"] {
        let u = typed(&mut h, 0, t);
        db.insert_row(
            doc,
            Some(author(0)),
            &u,
            None,
            since - chrono::Duration::hours(1),
        )
        .await;
        rows.push(u);
    }
    let b = db.builder(limits_with(|l| l.hooks.replay.panic_at_rev = Some(2)));
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!((s.skip_revs, s.genesis_rev), (vec![2], None));
    assert!(db.checkpoints(doc).await.is_empty());
    db.retry_now(doc).await;
    assert_eq!(b.build_page(doc).await, genesis(3));
    let cp = db.checkpoints(doc).await.remove(0);
    assert_eq!(
        visible(&cp.state_bytes),
        visible(&state_of([rows[0].as_slice(), rows[2].as_slice()]))
    );
    let u = typed(&mut h, 0, "after");
    db.append(doc, 0, &u).await;
    assert!(is_built(&b.build_page(doc).await));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_whose_gap_merge_corrupts_is_skipped_not_a_row() {
    let db = db().await;
    for panic_probe in [false, true] {
        let doc = db.page().await;
        let rows = genesis_then_rows(&db, doc).await;
        let (s, _) = with_paragraph(9, &db.state_at(doc, 2).await, "extra");
        db.snapshot(doc, 2, &s).await;
        let b = db.builder(limits_with(|l| {
            if panic_probe {
                l.hooks.replay.panic_probe_at = Some(2);
            } else {
                l.hooks.replay.corrupt_merge_at = Some(2);
            }
        }));
        assert_eq!(b.build_page(doc).await, PassResult::Failed);
        let s = db.state(doc).await;
        assert!(s.skip_revs.is_empty());
        assert_eq!((s.skip_snapshots, s.snapshot_checked_rev), (vec![2], 0));
        db.retry_now(doc).await;
        assert!(matches!(
            b.build_page(doc).await,
            PassResult::Built { built_rev: 3, .. }
        ));
        let entries = db.entries(doc).await;
        assert!(of_kind(&entries, "gap").is_empty());
        let mut all = yrs::IdSet::new();
        for r in &rows {
            all.merge_with(common::db::insertions(r));
        }
        let s = sessions(&entries);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].inserted(), all, "every row's content is A's");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_whose_probe_panics_is_skipped() {
    let db = db().await;
    let doc = db.page().await;
    genesis_then_rows(&db, doc).await;
    let (s, _) = with_paragraph(9, &db.state_at(doc, 2).await, "extra");
    db.snapshot(doc, 2, &s).await;
    let b = db.builder(limits_with(|l| l.hooks.replay.panic_probe_at = Some(2)));
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert!(s.skip_revs.is_empty());
    assert_eq!((s.skip_snapshots, s.snapshot_checked_rev), (vec![2], 0));
    db.retry_now(doc).await;
    assert!(matches!(
        b.build_page(doc).await,
        PassResult::Built { built_rev: 3, .. }
    ));
}

/// A probe that fails at a later snapshot leaves only that snapshot out:
/// the earlier snapshots the same pass had gap-checked are probed again by
/// the next pass, so content only they held still enters history.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_probe_skips_only_its_own_snapshot_not_earlier_ones() {
    let db = db().await;
    for panic_probe in [false, true] {
        let doc = db.page().await;
        assert_eq!(db.builder(limits()).build_page(doc).await, genesis(0));
        let mut h = History::new(1);
        for t in ["one", "two", "three", "four"] {
            let u = typed(&mut h, 0, t);
            db.append(doc, 0, &u).await;
        }
        let (s2, x) = with_paragraph(9, &db.state_at(doc, 2).await, "unlogged x");
        db.snapshot(doc, 2, &s2).await;
        let (s4, _) = with_paragraph(8, &db.state_at(doc, 4).await, "unlogged y");
        db.snapshot(doc, 4, &s4).await;
        let b = db.builder(limits_with(|l| {
            if panic_probe {
                l.hooks.replay.panic_probe_at = Some(4);
            } else {
                l.hooks.replay.corrupt_merge_at = Some(4);
            }
        }));
        assert_eq!(b.build_page(doc).await, PassResult::Failed);
        let s = db.state(doc).await;
        assert_eq!(
            (s.skip_snapshots.clone(), s.snapshot_checked_rev),
            (vec![4], 0),
            "only the snapshot at 4 is left out: {s:?}"
        );
        db.retry_now(doc).await;
        build_clean(&b, doc).await;
        let entries = db.entries(doc).await;
        let gaps = of_kind(&entries, "gap");
        assert_eq!(gaps.len(), 1, "panic {panic_probe}: {entries:#?}");
        assert_eq!(gaps[0].end_rev, Some(2));
        assert_eq!(gaps[0].inserted(), common::db::insertions(&x));
        let cps = db.checkpoints(doc).await;
        assert!(cps.iter().any(|c| c.rev == 2 && c.kind == "gap"));
        assert!(
            !cps.iter().any(|c| c.rev == 4 && c.kind == "gap"),
            "the failed snapshot stays out"
        );
        assert!(
            !b.sweep_candidates().await.contains(&doc),
            "a skipped snapshot is not a late one"
        );
    }
}

/// A genesis checkpoint that every pass rejects, again after each rebuild,
/// ends in a give-up after `MAX_REBUILDS` rebuilds: the claim's lift of the
/// rebuild request clears the failure counts, and each written genesis
/// releases them, so only the rebuild count can end the loop.
#[tokio::test(flavor = "multi_thread")]
async fn a_genesis_rejected_after_every_rebuild_gives_the_page_up() {
    let db = db().await;
    let b = db.builder(limits());
    let doc = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(doc, 0, &u).await;
    for cycle in 1..=MAX_REBUILDS {
        assert_eq!(b.build_page(doc).await, genesis(0), "cycle {cycle}");
        db.exec(
            "UPDATE doc_history_checkpoints SET state_bytes = '\\xffffff'
              WHERE doc_id = $1 AND kind = 'genesis'",
            doc,
        )
        .await;
        assert_eq!(b.build_page(doc).await, PassResult::Failed);
        let s = db.state(doc).await;
        assert_eq!((s.builder_version, s.rebuilds), (0, cycle), "{s:?}");
        assert!(!s.given_up);
        db.retry_now(doc).await;
    }
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert!(s.given_up, "{s:?}");
    assert_eq!((s.builder_version, s.rebuilds), (BUILDER_VERSION, 0));
    assert!(
        s.last_error.as_deref().unwrap_or("").contains("rebuilds"),
        "{s:?}"
    );
    assert_eq!(b.build_page(doc).await, PassResult::Deferred);
    assert!(!b.sweep_candidates().await.contains(&doc));

    // A newer builder lifts it with a fresh count.
    db.exec(
        "UPDATE doc_history_state SET builder_version = 0 WHERE doc_id = $1",
        doc,
    )
    .await;
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert!(is_built(&b.build_page(doc).await));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_checkpoint_is_dropped_and_rebuilt_past() {
    let db = db().await;
    let b = db.builder(limits());
    // A periodic checkpoint.
    let doc = db.page().await;
    let mut h = History::new(1);
    let c = periodic_below_built(&db, doc, &mut h, 1).await;
    truncate_checkpoint(&db, doc, c).await;
    let u = typed(&mut h, 0, "next");
    db.append(doc, 0, &u).await;
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    assert!(!db.checkpoints(doc).await.iter().any(|cp| cp.rev == c));
    assert!(db.state(doc).await.skip_revs.is_empty());
    db.retry_now(doc).await;
    assert!(is_built(&b.build_page(doc).await));
    let twin = twin_of(&db, doc).await;
    build_clean(&b, twin).await;
    assert_eq!(keys(&db.entries(doc).await), keys(&db.entries(twin).await));

    // A gap checkpoint: the page is rebuilt past it.
    let doc = db.page().await;
    let mut h = History::new(2);
    for t in ["one", "two", "three"] {
        let u = typed(&mut h, 0, t);
        db.append(doc, 0, &u).await;
    }
    let (s, x) = with_paragraph(9, &db.state_at(doc, 2).await, "gap para");
    db.snapshot(doc, 2, &s).await;
    build_clean(&b, doc).await;
    let u = typed(&mut h, 0, "four");
    db.append(doc, 0, &u).await;
    build_clean(&b, doc).await;
    truncate_checkpoint(&db, doc, 2).await;
    let u = typed(&mut h, 0, "five");
    db.append(doc, 0, &u).await;
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    assert_eq!(db.state(doc).await.builder_version, 0);
    assert!(!db.checkpoints(doc).await.iter().any(|cp| cp.rev == 2));
    db.retry_now(doc).await;
    build_until(&b, doc, is_built).await;
    let entries = db.entries(doc).await;
    let gaps = of_kind(&entries, "gap");
    assert_eq!(gaps.len(), 1, "{entries:#?}");
    assert_eq!(gaps[0].inserted(), common::db::insertions(&x));
    let twin = twin_of(&db, doc).await;
    build_clean(&b, twin).await;
    let non_gap: Vec<Entry> = entries.into_iter().filter(|e| e.kind != "gap").collect();
    assert_eq!(keys(&non_gap), keys(&db.entries(twin).await));

    // Choosing the checkpoint: two periodic ones below built_rev.
    let doc = db.page().await;
    let mut h = History::new(3);
    let c2 = periodic_below_built(&db, doc, &mut h, 2).await;
    let periodic: Vec<i64> = db
        .checkpoints(doc)
        .await
        .into_iter()
        .filter(|cp| cp.kind == "periodic" && cp.rev < c2)
        .map(|cp| cp.rev)
        .collect();
    let c1 = *periodic.last().expect("a second periodic checkpoint");
    truncate_checkpoint(&db, doc, c1).await;
    let u = typed(&mut h, 0, "next");
    db.append(doc, 0, &u).await;
    assert!(is_built(&b.build_page(doc).await), "the pass starts at c2");
    truncate_checkpoint(&db, doc, c2).await;
    let built = db_built(&db, doc).await;
    let latest = db
        .checkpoints(doc)
        .await
        .into_iter()
        .rfind(|cp| cp.rev <= built)
        .unwrap();
    assert_eq!(latest.rev, c2, "no newer checkpoint than c2");
    let u = typed(&mut h, 0, "next again");
    db.append(doc, 0, &u).await;
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let cps: Vec<i64> = db.checkpoints(doc).await.iter().map(|cp| cp.rev).collect();
    assert!(!cps.contains(&c2) && cps.contains(&c1), "{cps:?}");
    assert!(db.state(doc).await.skip_revs.is_empty());
}

async fn db_built(db: &Db, doc: Uuid) -> i64 {
    db.state(doc).await.built_rev
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rev_gap_fails_the_page_with_backoff() {
    let db = db().await;
    let doc = db.page().await;
    genesis_then_rows(&db, doc).await;
    db.rewrite("DELETE FROM doc_updates WHERE doc_id = $1 AND rev = 2", doc)
        .await;
    let b = db.builder(limits());
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!((s.built_rev, s.failures, s.claimed_by), (1, 1, None));
    assert_eq!(s.last_error.as_deref(), Some("rev gap after 1"));
    assert!(s.deferred);
    assert!(!b.dirty_candidates().await.contains(&doc));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_legacy_count_mismatch_fails_the_page_with_backoff() {
    use knot_test_support::migrate::{bare_db, migrate_all, migrate_up_to};
    let (_, pool) = bare_db(8).await;
    migrate_up_to(&pool, 20261107120000).await;
    let ws: Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'A') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO doc_updates (doc_id, update_bytes)
         SELECT $1, int4send(g) FROM generate_series(1, 3) g",
    )
    .bind(doc)
    .execute(&pool)
    .await
    .unwrap();
    migrate_all(&pool).await;
    let db = Db::on_existing(pool, ws).await;
    db.rewrite(
        "UPDATE doc_updates SET rev = 100 + seq WHERE doc_id = $1",
        doc,
    )
    .await;
    let b = db.builder(limits());
    assert_eq!(b.build_page(doc).await, PassResult::Failed);
    let s = db.state(doc).await;
    assert_eq!(s.failures, 1);
    assert!(s.deferred);
    assert!(s.last_error.as_deref().unwrap().contains("legacy"), "{s:?}");
    assert!(!b.dirty_candidates().await.contains(&doc));
}

// ---------------------------------------------------------------------------
// The dirty flag and word counts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_dirty_flag_stays_while_rows_or_word_counts_remain() {
    let db = db().await;
    let doc = db.page().await;
    let mut h = History::new(1);
    for i in 0..5 {
        let u = typed(&mut h, 0, &format!("row {i}"));
        db.append(doc, 0, &u).await;
    }
    let b = db.builder(limits_with(|l| l.max_rows = 2));
    assert_eq!(b.build_page(doc).await, genesis(0));
    for (built, dirty) in [(2, true), (4, true), (5, false)] {
        assert!(matches!(
            b.build_page(doc).await,
            PassResult::Built { built_rev, .. } if built_rev == built
        ));
        assert_eq!(db.head(doc).await.1, dirty, "after {built}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn word_counts_wait_for_the_budget() {
    let db = db().await;
    // A and B at +0, A and B at +30: the first pass seals two sessions.
    let (doc, _) = page_with_rows(
        &db,
        &[
            (0, "a one", 0),
            (1, "b one", 0),
            (0, "a two", 30),
            (1, "b two", 30),
        ],
    )
    .await;
    let b = db.builder(limits_with(|l| l.words_budget = Duration::ZERO));
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert!(is_built(&b.build_page(doc).await));
    let counted = |entries: &[Entry]| {
        entries
            .iter()
            .filter(|e| e.sealed && e.kind == "session" && e.words_added.is_some())
            .count()
    };
    let entries = db.entries(doc).await;
    let sealed: Vec<_> = entries.iter().filter(|e| e.sealed).collect();
    assert_eq!(sealed.len(), 4, "the idle rule seals the second ones too");
    assert_eq!(counted(&entries), 1, "the pass reached the head: one count");
    let oldest = sealed.iter().min_by_key(|e| (e.end_rev, e.id)).unwrap();
    assert!(oldest.words_added.is_some());
    assert!(db.head(doc).await.1, "counts remain");
    for n in 2..=4 {
        assert!(is_built(&b.build_page(doc).await));
        assert_eq!(counted(&db.entries(doc).await), n);
    }
    assert!(!db.head(doc).await.1);

    // Not at the head: no count.
    let (doc, _) = page_with_rows(
        &db,
        &[(0, "a one", 0), (0, "a two", 30), (0, "a three", 31)],
    )
    .await;
    let b = db.builder(limits_with(|l| {
        l.words_budget = Duration::ZERO;
        l.max_rows = 1;
    }));
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert!(is_built(&b.build_page(doc).await));
    assert!(is_built(&b.build_page(doc).await));
    let first = db
        .entries(doc)
        .await
        .into_iter()
        .find(|e| e.sealed)
        .expect("sealed by row 2");
    assert!(first.words_pending && first.words_added.is_none());

    // A rows budget of zero: one row per pass, and the last one counts.
    let (doc, _) =
        page_with_rows(&db, &[(0, "a one", 0), (0, "a two", 1), (0, "a three", 2)]).await;
    let b = db.builder(limits_with(|l| l.rows_budget = Duration::ZERO));
    assert_eq!(b.build_page(doc).await, genesis(0));
    for rev in 1..=3 {
        assert_eq!(
            b.build_page(doc).await,
            PassResult::Built {
                built_rev: rev,
                rows: 1
            }
        );
    }
    let e = db.entries(doc).await;
    assert_eq!(e.len(), 1);
    assert!(e[0].sealed && e[0].words_added == Some(6), "{e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_spanning_a_union_gap_gets_its_word_count() {
    let db = db().await;
    let doc = db.page().await;
    let since = Utc::now() - chrono::Duration::hours(2);
    db.backdate(doc, since).await;
    let (doc_state, update, room_state) = skip_underflow_case();
    db.insert_row(
        doc,
        Some(author(0)),
        &doc_state,
        Some(UpdateSource::Edit),
        since,
    )
    .await;
    db.insert_row(
        doc,
        Some(author(0)),
        &update,
        Some(UpdateSource::Edit),
        since + minutes(1),
    )
    .await;
    db.snapshot(doc, 2, &room_state).await;
    let (_, typed_words) = with_paragraph(5, &room_state, "four new words here");
    db.insert_row(
        doc,
        Some(author(0)),
        &typed_words,
        Some(UpdateSource::Edit),
        since + minutes(2),
    )
    .await;
    let mut h = History::new(9);
    let later = typed(&mut h, 0, "later");
    db.insert_row(
        doc,
        Some(author(0)),
        &later,
        Some(UpdateSource::Edit),
        since + minutes(30),
    )
    .await;
    let b = db.builder(limits());
    build_clean(&b, doc).await;
    assert!(
        db.state(doc).await.skip_revs.contains(&2),
        "row 2 was refused"
    );
    let entries = db.entries(doc).await;
    let first = sessions(&entries)
        .into_iter()
        .find(|e| e.start_rev == Some(1))
        .cloned()
        .expect("A's first session");
    assert_eq!(first.end_rev, Some(3), "{entries:#?}");
    assert!(first.words_added.is_some_and(|n| n >= 4), "{first:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_word_count_over_the_cap_is_skipped() {
    let db = db().await;
    let (doc, _) = page_with_rows(&db, &[(0, "alpha beta", 0), (0, "gamma", 30)]).await;
    let b = db.builder(limits_with(|l| l.words_max_input_bytes = 1));
    build_clean(&b, doc).await;
    let entries = db.entries(doc).await;
    assert!(entries.iter().any(|e| e.sealed));
    for e in entries.iter().filter(|e| e.sealed) {
        assert_eq!(
            (e.words_added, e.words_removed, e.words_pending),
            (None, None, false)
        );
    }
    assert!(!db.head(doc).await.1);
    assert_eq!(db.state(doc).await.built_rev, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_word_count_that_fails_is_skipped_not_retried() {
    let db = db().await;
    let (doc, _) = page_with_rows(&db, &[(0, "alpha beta", 0), (0, "gamma", 30)]).await;
    let b = db.builder(limits_with(|l| l.hooks.fail_words = true));
    assert_eq!(b.build_page(doc).await, genesis(0));
    assert!(is_built(&b.build_page(doc).await));
    build_clean(&b, doc).await;
    let entries = db.entries(doc).await;
    for e in entries.iter().filter(|e| e.sealed) {
        assert_eq!(
            (e.words_added, e.words_removed, e.words_pending),
            (None, None, false)
        );
    }
    let s = db.state(doc).await;
    assert_eq!((s.failures, s.built_rev), (0, 2));
    assert!(!db.head(doc).await.1);
}

// ---------------------------------------------------------------------------
// Versions, the sweep and the candidates
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_builder_version_is_left_alone_and_an_older_one_rebuilt() {
    let db = db().await;
    let b = db.builder(limits());
    let doc = db.page().await;
    let mut h = History::new(1);
    let one = typed(&mut h, 0, "one");
    db.append(doc, 0, &one).await;
    db.append(doc, 0, &one).await; // A retried append: no effect.
    let three = typed(&mut h, 0, "three");
    db.append(doc, 0, &three).await;
    let (s, _) = with_paragraph(9, &db.state_at(doc, 3).await, "gap para");
    db.snapshot(doc, 3, &s).await;
    build_clean(&b, doc).await;
    db.exec(
        "INSERT INTO doc_history_entries (doc_id, kind, started_at, ended_at, start_rev, end_rev, sealed)
         VALUES ($1, 'erase', now(), now(), 1, 1, true)",
        doc,
    )
    .await;
    let before = db.entries(doc).await;
    assert_eq!(of_kind(&before, "gap").len(), 1);

    db.exec(
        "UPDATE doc_history_state SET builder_version = 2 WHERE doc_id = $1",
        doc,
    )
    .await;
    db.exec(
        "UPDATE doc_history_heads SET dirty = true WHERE doc_id = $1",
        doc,
    )
    .await;
    assert_eq!(b.build_page(doc).await, PassResult::Deferred);
    assert_eq!(db.entries(doc).await, before);

    db.exec(
        "UPDATE doc_history_state SET builder_version = 0, skip_revs = '{2}' WHERE doc_id = $1",
        doc,
    )
    .await;
    let slow = db.builder(holding(Duration::from_secs(2)));
    let rebuild = {
        let slow = slow.clone();
        tokio::spawn(async move { slow.build_page(doc).await })
    };
    wait_for("the reset", async || {
        db.state(doc).await.genesis_rev.is_none()
    })
    .await;
    let s = db.state(doc).await;
    assert!(db.head(doc).await.1);
    assert_eq!(s.skip_revs, vec![2]);
    let kinds: Vec<String> = db
        .checkpoints(doc)
        .await
        .into_iter()
        .map(|c| c.kind)
        .collect();
    assert_eq!(
        kinds,
        vec!["gap".to_string()],
        "only the gap checkpoint stays"
    );
    let kept: Vec<String> = db.entries(doc).await.into_iter().map(|e| e.kind).collect();
    assert_eq!(kept, vec!["erase".to_string()]);
    assert_eq!(rebuild.await.unwrap(), genesis(0));
    build_clean(&b, doc).await;
    let after = db.entries(doc).await;
    assert_eq!(keys(&after), keys(&before), "rebuilt equal, the gap once");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_seals_idle_sessions_on_clean_pages() {
    let db = db().await;
    let b = db.builder(limits());
    let mut docs = Vec::new();
    for _ in 0..3 {
        let doc = db.page().await;
        let mut h = History::new(1);
        let u = typed(&mut h, 0, "one");
        db.append(doc, 0, &u).await;
        build_clean(&b, doc).await;
        db.exec(
            "UPDATE doc_history_entries SET ended_at = ended_at - interval '22 minutes'
              WHERE doc_id = $1",
            doc,
        )
        .await;
        docs.push(doc);
    }
    let (idle, backing_off, newer) = (docs[0], docs[1], docs[2]);
    db.exec(
        "UPDATE doc_history_state SET retry_after = now() + interval '1 hour' WHERE doc_id = $1",
        backing_off,
    )
    .await;
    db.exec(
        "UPDATE doc_history_state SET builder_version = 2 WHERE doc_id = $1",
        newer,
    )
    .await;
    let candidates = b.sweep_candidates().await;
    assert!(candidates.contains(&idle));
    assert!(!candidates.contains(&backing_off) && !candidates.contains(&newer));
    assert!(b.sweep().await >= 1);
    assert!(db.entries(idle).await.iter().all(|e| e.sealed));
}

#[tokio::test(flavor = "multi_thread")]
async fn ticks_visit_seeded_pages_oldest_first() {
    let db = db().await;
    let b = db.builder(limits());
    let mut docs = Vec::new();
    for hours in [3, 1, 2] {
        let doc = db.page().await;
        let mut h = History::new(1);
        let u = typed(&mut h, 0, "one");
        db.append(doc, 0, &u).await;
        sqlx::query(
            "UPDATE doc_history_heads SET last_write_at = now() - make_interval(hours => $2)
              WHERE doc_id = $1",
        )
        .bind(doc)
        .bind(hours)
        .execute(&db.pool)
        .await
        .unwrap();
        docs.push(doc);
    }
    let backing_off = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(backing_off, 0, &u).await;
    assert_eq!(b.build_page(backing_off).await, genesis(0));
    db.exec(
        "UPDATE doc_history_state SET retry_after = now() + interval '1 hour' WHERE doc_id = $1",
        backing_off,
    )
    .await;
    assert_eq!(b.dirty_candidates().await, vec![docs[0], docs[2], docs[1]]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_edited_seconds_ago_waits_unless_far_behind() {
    let db = db().await;
    let b = db.builder(limits_with(|l| l.debounce = Duration::from_secs(10)));
    let doc = db.page().await;
    let mut h = History::new(1);
    let u = typed(&mut h, 0, "one");
    db.append(doc, 0, &u).await;
    assert!(!b.dirty_candidates().await.contains(&doc));
    db.exec(
        "UPDATE doc_history_heads SET last_write_at = now() - interval '11 seconds' WHERE doc_id = $1",
        doc,
    )
    .await;
    assert!(b.dirty_candidates().await.contains(&doc));
    let behind = db.page().await;
    db.append(behind, 0, &u).await;
    sqlx::query(
        "UPDATE doc_history_heads SET head_rev = $2, last_write_at = now() WHERE doc_id = $1",
    )
    .bind(behind)
    .bind(DEBOUNCE_MAX_LAG_REVS)
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(b.dirty_candidates().await.contains(&behind));
    // The sweep is not debounced: a late snapshot of a page written now.
    let late = db.page().await;
    db.append(late, 0, &u).await;
    build_clean(&db.builder(limits()), late).await;
    let (s, _) = with_paragraph(9, &db.state_at(late, 1).await, "late");
    db.snapshot(late, 1, &s).await;
    let v = typed(&mut h, 0, "two");
    db.append(late, 0, &v).await;
    assert!(b.sweep_candidates().await.contains(&late));
}
