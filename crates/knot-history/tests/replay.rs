//! The replay engine (page history M1.3, Task 1): what each row changed,
//! what it parked, a replay that never panics on a gapped row, and the
//! isolated thread every replay runs on.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::underflow::skip_underflow_case;
use knot_history::{IsolateError, ReplayMode, Replayer, RowOutcome, run_isolated};
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Any, Doc, GetString, ID, IdSet, ReadTxn, StateVector, Subscription, Text, Transact,
    TransactionMut, Update, XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim,
    XmlTextRef,
};

#[path = "../../knot-crdt/tests/common/fixtures.rs"]
mod fixtures;

use fixtures::{Rng, para, parked_block_and_the_edit_it_fails_on};

/// The update of one transaction `f` runs on `doc` (`observe_update_v1`).
fn txn_update(doc: &Doc, f: impl FnOnce(&mut TransactionMut)) -> Vec<u8> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let sub = {
        let out = out.clone();
        doc.observe_update_v1(move |_, e| *out.lock().unwrap() = e.update.clone())
            .expect("observe_update_v1")
    };
    f(&mut doc.transact_mut());
    drop(sub);
    let update = std::mem::take(&mut *out.lock().unwrap());
    assert!(!update.is_empty(), "the transaction changed something");
    update
}

/// Every block `update` inserts, deleted ones included.
fn insertions_of(update: &[u8]) -> IdSet {
    Update::decode_v1(update)
        .expect("an update")
        .insertions(true)
}

/// The first id `update` inserts.
fn first_id_of(update: &[u8]) -> ID {
    let ins = insertions_of(update);
    let (client, ranges) = ins.iter().next().expect("an insertion");
    let first = ranges.iter().next().expect("a range");
    ID::new(*client, first.start)
}

/// The replay's pending structs and pending delete set, each encoded.
fn pending_bytes(r: &Replayer) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let txn = r.doc().transact();
    let store = txn.store();
    (
        store.pending_update().map(|p| p.update.encode_v1()),
        store.pending_ds().map(|ds| ds.encode_v1()),
    )
}

fn delete_set_of(r: &Replayer) -> Vec<u8> {
    r.doc().transact().snapshot().delete_set.encode_v1()
}

fn xml_string_of_doc(doc: &Doc) -> String {
    let frag = doc.get_or_insert_xml_fragment("default");
    frag.get_string(&doc.transact())
}

/// The `default` fragment as XML.
fn xml_string(r: &Replayer) -> String {
    xml_string_of_doc(r.doc())
}

fn xml_string_of_state(state: &[u8]) -> String {
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(state).expect("a state"))
        .expect("the state applies");
    xml_string_of_doc(&doc)
}

/// The text of the `i`th paragraph of `frag`.
fn text_of(frag: &XmlFragmentRef, txn: &impl ReadTxn, i: u32) -> XmlTextRef {
    match frag.get(txn, i) {
        Some(XmlOut::Element(p)) => match p.get(txn, 0) {
            Some(XmlOut::Text(t)) => t,
            other => panic!("paragraph {i} holds no text: {other:?}"),
        },
        other => panic!("no paragraph at {i}: {other:?}"),
    }
}

/// A doc of `client` that holds `state`.
fn doc_with(client: u64, state: &[u8]) -> Doc {
    let doc = Doc::with_client_id(client);
    doc.get_or_insert_xml_fragment("default");
    doc.transact_mut()
        .apply_update(Update::decode_v1(state).expect("a state"))
        .expect("the state applies");
    doc
}

fn applied(r: &mut Replayer, update: &[u8]) {
    let e = r.apply(update);
    assert_eq!(
        e.outcome,
        RowOutcome::Applied,
        "fixture: the update applies"
    );
}

#[test]
fn apply_reports_what_the_row_changed() {
    let doc = Doc::with_client_id(1);
    let frag = doc.get_or_insert_xml_fragment("default");
    let u1 = txn_update(&doc, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("hello"));
    });
    let u2 = txn_update(&doc, |txn| text_of(&frag, txn, 0).remove_range(txn, 0, 1));

    let mut r = Replayer::new(ReplayMode::Gc);
    let e1 = r.apply(&u1);
    assert_eq!(e1.outcome, RowOutcome::Applied);
    assert!(!e1.inserted.is_empty() && e1.deleted.is_empty());
    assert_eq!(e1.inserted, insertions_of(&u1));
    let e2 = r.apply(&u2);
    assert_eq!(e2.outcome, RowOutcome::Applied);
    assert!(e2.inserted.is_empty() && !e2.deleted.is_empty());
    assert!(r.apply(&u1).is_empty(), "a resend integrates nothing");
    assert_eq!(xml_string(&r), "<paragraph>ello</paragraph>");
}

/// A base paragraph from client 9, and client 1 typing into it: "ab" (u_a),
/// then "c" right after "b" (u_b), whose left origin is u_a's item. yrs
/// 0.27.3 integrates a block whose own origins it holds past a gap without
/// parking, so u_b must depend on u_a to park.
fn base_and_two_dependent_edits() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let base = para(9, "x");
    let doc = doc_with(1, &base);
    let frag = doc.get_or_insert_xml_fragment("default");
    let u_a = txn_update(&doc, |txn| text_of(&frag, txn, 0).insert(txn, 1, "ab"));
    let u_b = txn_update(&doc, |txn| text_of(&frag, txn, 0).insert(txn, 3, "c"));
    (base, u_a, u_b)
}

#[test]
fn parked_content_is_reported_then_credited_on_drain() {
    let (base, u_a, u_b) = base_and_two_dependent_edits();
    let mut r = Replayer::new(ReplayMode::Gc);
    applied(&mut r, &base);
    let parked = r.apply(&u_b);
    assert_eq!(parked.outcome, RowOutcome::Applied);
    assert!(parked.is_empty() && !parked.parked_inserts.is_empty());
    assert_eq!(parked.parked_inserts, insertions_of(&u_b));
    assert!(parked.parked_deletes.is_empty());
    assert!(r.has_pending());
    let drained = r.apply(&u_a);
    assert!(
        drained.inserted.contains(&first_id_of(&u_b)),
        "u_b's blocks integrate in u_a's transaction"
    );
    assert!(drained.parked_inserts.is_empty());
    assert!(!r.has_pending());
    assert_eq!(xml_string(&r), "<paragraph>xabc</paragraph>");
}

/// Client 1 makes a paragraph "abcdef" (U1), then deletes "bcd" (U2): U2
/// alone carries only a delete set over items a doc without U1 lacks (M1.2
/// Task 1's delete-before-insert step).
fn insert_then_delete() -> (Vec<u8>, Vec<u8>) {
    let doc = Doc::with_client_id(1);
    let frag = doc.get_or_insert_xml_fragment("default");
    let u1 = txn_update(&doc, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("abcdef"));
    });
    let u2 = txn_update(&doc, |txn| text_of(&frag, txn, 0).remove_range(txn, 1, 3));
    (u1, u2)
}

#[test]
fn a_delete_of_unknown_items_parks_deletes_only() {
    let (_u1, u2) = insert_then_delete();
    let mut r = Replayer::new(ReplayMode::Gc);
    let e = r.apply(&u2);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert!(e.is_empty());
    assert!(e.parked_inserts.is_empty() && !e.parked_deletes.is_empty());
    assert!(r.has_pending());
}

#[test]
fn checkpoint_round_trip_keeps_pending_content() {
    let (base, _u_a, u_b) = base_and_two_dependent_edits();
    let (_u1, u2) = insert_then_delete();
    let mut r = Replayer::new(ReplayMode::Gc);
    applied(&mut r, &base);
    applied(&mut r, &u_b);
    applied(&mut r, &u2);
    let (structs, deletes) = pending_bytes(&r);
    assert!(
        structs.is_some() && deletes.is_some(),
        "fixture: r holds pending structs and a pending delete set"
    );

    let again = Replayer::from_state(ReplayMode::Gc, &r.encode_state()).unwrap();
    assert_eq!(pending_bytes(&again), pending_bytes(&r));
    assert_eq!(again.state_vector(), r.state_vector());
    assert!(again.has_pending());
}

#[test]
fn a_checkpoint_with_pending_nested_content_resumes_like_one_that_never_round_tripped() {
    // Client 1: paragraph P holding a text "ab" (u1); then "X" at the end of
    // P's text and a nested element N after the text, inside P, holding an
    // empty text (u2).
    let c1 = Doc::with_client_id(1);
    let frag = c1.get_or_insert_xml_fragment("default");
    let u1 = txn_update(&c1, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("ab"));
    });
    let u2 = txn_update(&c1, |txn| {
        text_of(&frag, txn, 0).insert(txn, 2, "X");
        let Some(XmlOut::Element(p)) = frag.get(txn, 0) else {
            panic!("P")
        };
        let n = p.push_back(txn, XmlElementPrelim::empty("nested"));
        n.push_back(txn, XmlTextPrelim::new(""));
    });
    // Client 2 holds both and types "c" into P's text right after "b" (u3:
    // its right origin is u2's "X"), and "zz" into N's empty text (u4: no
    // origins, its parent N's text named by id).
    let c2 = doc_with(2, &u1);
    c2.transact_mut()
        .apply_update(Update::decode_v1(&u2).unwrap())
        .unwrap();
    let frag2 = c2.get_or_insert_xml_fragment("default");
    let u3 = txn_update(&c2, |txn| text_of(&frag2, txn, 0).insert(txn, 2, "c"));
    let u4 = txn_update(&c2, |txn| {
        let Some(XmlOut::Element(p)) = frag2.get(txn, 0) else {
            panic!("P")
        };
        let Some(XmlOut::Element(n)) = p.get(txn, 1) else {
            panic!("N")
        };
        let Some(XmlOut::Text(t)) = n.get(txn, 0) else {
            panic!("N's text")
        };
        t.insert(txn, 0, "zz");
    });

    let mut r = Replayer::new(ReplayMode::Gc);
    applied(&mut r, &u1);
    let e3 = r.apply(&u3);
    let e4 = r.apply(&u4);
    assert!(
        !e3.parked_inserts.is_empty() && !e4.parked_inserts.is_empty(),
        "fixture: u3 and u4 park while u2 is withheld"
    );
    let mut again = Replayer::from_state(ReplayMode::Gc, &r.encode_state()).unwrap();
    assert!(again.has_pending());

    let drained = r.apply(&u2);
    let drained_again = again.apply(&u2);
    assert_eq!(drained.outcome, RowOutcome::Applied);
    assert_eq!(drained_again.outcome, RowOutcome::Applied);
    assert_eq!(drained_again.inserted, drained.inserted);
    assert_eq!(again.state_vector(), r.state_vector());
    assert_eq!(delete_set_of(&again), delete_set_of(&r));
    assert_eq!(xml_string(&again), xml_string(&r));
    assert_eq!(
        xml_string(&r),
        xml_string_of_doc(&c2),
        "the replay holds what client 2 holds"
    );
    assert!(!again.has_pending() && !r.has_pending());
}

/// Client 1's three transactions on root-level paragraphs: t1 "one" at 0,
/// t2 "two" after it, t3 "three" at index 0, which depends on t1 only. The
/// client's doc holds all three.
fn three_paragraphs() -> (Doc, Vec<u8>, Vec<u8>, Vec<u8>, StateVector) {
    let doc = Doc::with_client_id(1);
    let frag = doc.get_or_insert_xml_fragment("default");
    let new_para = |txn: &mut TransactionMut, at: u32, text: &str| {
        let p = frag.insert(txn, at, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(text));
    };
    let t1 = txn_update(&doc, |txn| new_para(txn, 0, "one"));
    let after_t1 = doc.transact().state_vector();
    let t2 = txn_update(&doc, |txn| new_para(txn, 1, "two"));
    let t3 = txn_update(&doc, |txn| new_para(txn, 0, "three"));
    (doc, t1, t2, t3, after_t1)
}

/// A replay that holds t1 and t3: t3 integrated past a gap (client 1's
/// state-vector clock stops at t2), nothing pending.
fn gapped_replay(t1: &[u8], t3: &[u8]) -> Replayer {
    let mut r = Replayer::new(ReplayMode::Gc);
    applied(&mut r, t1);
    let e = r.apply(t3);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert_eq!(e.inserted, insertions_of(t3), "fixture: t3 integrates");
    assert!(!r.has_pending(), "fixture: past the gap, nothing parked");
    r
}

#[test]
fn a_resend_over_blocks_held_past_a_gap_is_trimmed_and_applied() {
    let (doc, t1, t2, t3, after_t1) = three_paragraphs();
    let mut r = gapped_replay(&t1, &t3);
    let resend = doc.transact().encode_state_as_update_v1(&after_t1);
    assert_eq!(
        insertions_of(&resend),
        insertions_of(&t2).merge(&insertions_of(&t3)),
        "fixture: the resend carries t2 and t3"
    );
    let e = r.apply(&resend);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert_eq!(e.inserted, insertions_of(&t2));
    assert_eq!(xml_string(&r), xml_string_of_doc(&doc));
    assert!(!r.has_pending());
}

/// A checked apply (the replay holds t3 past client 1's gap, and the
/// update has client 1's blocks) that drains parked content the update does
/// not itself carry is a legitimate apply, not `Corrupt`: content parked
/// before the apply may become held by it.
#[test]
fn a_checked_apply_that_drains_parked_content_is_applied() {
    let (_doc, t1, t2, t3, _) = three_paragraphs();
    // Client 3 holds t1 and adds paragraph "q" (e3); client 2 holds t1 and
    // e3 and types "W" into q (e2), which depends on e3.
    let c3 = doc_with(3, &t1);
    let frag3 = c3.get_or_insert_xml_fragment("default");
    let e3 = txn_update(&c3, |txn| {
        let p = frag3.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("q"));
    });
    let c2 = doc_with(2, &yrs::merge_updates_v1([&t1, &e3]).expect("merge"));
    let frag2 = c2.get_or_insert_xml_fragment("default");
    let e2 = txn_update(&c2, |txn| text_of(&frag2, txn, 1).insert(txn, 1, "W"));

    let mut r = gapped_replay(&t1, &t3);
    let parked = r.apply(&e2);
    assert_eq!(parked.outcome, RowOutcome::Applied);
    assert_eq!(
        parked.parked_inserts,
        insertions_of(&e2),
        "fixture: e2 parks on client 3"
    );
    let row = yrs::merge_updates_v1([&t2, &e3]).expect("merge t2 and e3");
    let e = r.apply(&row);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert!(
        e.inserted.contains(&first_id_of(&e2)),
        "e2's parked block drains in this transaction"
    );
    assert!(!r.has_pending());

    // A plain yrs doc applies the same rows to the same content.
    let plain = Doc::new();
    for u in [&t1, &t3, &e2, &row] {
        plain
            .transact_mut()
            .apply_update(Update::decode_v1(u).expect("an update"))
            .expect("applies");
    }
    assert_eq!(xml_string(&r), xml_string_of_doc(&plain));
    assert_eq!(
        xml_string(&r),
        "<paragraph>three</paragraph><paragraph>one</paragraph>\
         <paragraph>two</paragraph><paragraph>qW</paragraph>"
    );
}

#[test]
fn a_full_state_probe_over_a_gapped_replay_reports_the_missing_range() {
    let (doc, t1, t2, t3, _) = three_paragraphs();
    let r = gapped_replay(&t1, &t3);
    let full = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let e = r.probe(&full);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert_eq!(e.inserted, insertions_of(&t2));
}

#[test]
fn the_skip_underflow_is_refused_not_applied() {
    let (doc_state, update, _room_state) = skip_underflow_case();
    let mut r = Replayer::from_state(ReplayMode::Gc, &doc_state).unwrap();
    let before = r.encode_state();
    let e = r.apply(&update);
    assert_eq!(e.outcome, RowOutcome::Refused);
    assert!(e.is_empty());
    assert_eq!(r.encode_state(), before, "the doc is unchanged");
    assert_eq!(r.probe(&update).outcome, RowOutcome::Refused);
    assert_eq!(RowOutcome::Refused.metric_reason(), Some("skip_underflow"));
}

#[test]
fn union_takes_a_snapshot_whose_probe_is_refused() {
    let (doc_state, update, room_state) = skip_underflow_case();
    let mut r = Replayer::from_state(ReplayMode::Gc, &doc_state).unwrap();
    assert_eq!(
        r.probe(&room_state).outcome,
        RowOutcome::Refused,
        "the refused shape again"
    );
    let e = r.union(&room_state);
    assert_eq!(e.outcome, RowOutcome::Applied);
    // What the refused row brings that the doc lacks (the row also re-sends
    // the block the doc holds past its gap, which is what refuses it) is in
    // the union's effect. IdSet has no subset_of in yrs 0.27.3.
    let ins = insertions_of(&update).diff(&insertions_of(&doc_state));
    assert!(!ins.is_empty(), "fixture: the row brings new content");
    assert_eq!(e.inserted.intersect(&ins), ins);
    assert!(!e.deleted.is_empty(), "the room deleted paragraph B");
    assert_eq!(xml_string(&r), xml_string_of_state(&room_state));
    assert!(!r.has_pending());

    let mut r2 = Replayer::from_state(ReplayMode::KeepDeleted, &doc_state).unwrap();
    // merge_state is Refused, so it takes the union.
    let e2 = r2.merge_gap_state(&room_state);
    assert_eq!(e2.outcome, RowOutcome::Applied);
    assert_eq!(xml_string(&r2), xml_string_of_state(&room_state));
}

#[test]
fn union_adds_what_the_state_holds_and_the_replay_lacks() {
    let base = para(1, "base");
    let source = doc_with(2, &base);
    let frag = source.get_or_insert_xml_fragment("default");
    let extra_paragraph_update = txn_update(&source, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("extra"));
    });
    let state_with_extra_paragraph = source
        .transact()
        .encode_state_as_update_v1(&StateVector::default());

    let mut r = Replayer::new(ReplayMode::Gc);
    applied(&mut r, &base);
    let e = r.union(&state_with_extra_paragraph);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert_eq!(e.inserted, insertions_of(&extra_paragraph_update));
    assert!(e.deleted.is_empty());
    assert_eq!(xml_string(&r), xml_string_of_doc(&source));
}

#[test]
fn undecodable_and_rejected_rows_are_reported() {
    let mut r = Replayer::new(ReplayMode::Gc);
    let e = r.apply(&[0xff, 0xff, 0xff]);
    assert!(matches!(e.outcome, RowOutcome::Undecodable(_)));
    assert!(e.is_empty());
    assert_eq!(e.outcome.metric_reason(), Some("undecodable"));
    // A row yrs rejects part-way.
    let (crafted, edit, _) = parked_block_and_the_edit_it_fails_on();
    let parked = r.apply(&crafted); // parks its string
    assert!(
        !parked.parked_inserts.is_empty(),
        "fixture: the string parks"
    );
    let e = r.apply(&edit); // integrates the edit, then the parked string fails
    assert!(matches!(e.outcome, RowOutcome::ApplyError(_)));
    assert!(
        !e.inserted.is_empty(),
        "what integrated before the error is reported"
    );
    assert_eq!(e.outcome.metric_reason(), Some("apply_error"));
    assert_eq!(RowOutcome::Applied.metric_reason(), None);
    assert_eq!(RowOutcome::Corrupt.metric_reason(), None);
}

#[test]
fn probe_leaves_the_replay_untouched() {
    let base = para(1, "base");
    let source = doc_with(2, &base);
    let frag = source.get_or_insert_xml_fragment("default");
    txn_update(&source, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("extra"));
    });
    let state_with_extra_paragraph = source
        .transact()
        .encode_state_as_update_v1(&StateVector::default());

    let mut r = Replayer::new(ReplayMode::Gc);
    applied(&mut r, &base);
    let before = r.encode_state();
    let e = r.probe(&state_with_extra_paragraph);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert!(!e.inserted.is_empty());
    assert_eq!(r.encode_state(), before);
}

#[tokio::test]
async fn run_isolated_turns_a_panic_into_an_error_and_returns_the_guard() {
    let (r, g) = run_isolated((), Duration::from_secs(5), || 7).await;
    assert_eq!((r.unwrap(), g), (7, Some(())));
    let (r, g) = run_isolated((), Duration::from_secs(5), || -> u8 { panic!("boom") }).await;
    assert!(matches!(&r, Err(IsolateError::Panicked(m)) if m.contains("boom")));
    assert!(g.is_some());
}

/// Sets its flag when dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_timed_out_replay_keeps_its_guard_until_the_thread_ends() {
    let flag = Arc::new(AtomicBool::new(false));
    let guard = DropFlag(flag.clone());
    let (r, g) = run_isolated(guard, Duration::from_millis(100), || {
        std::thread::sleep(Duration::from_millis(600))
    })
    .await;
    assert!(matches!(r, Err(IsolateError::TimedOut)) && g.is_none());
    assert!(
        !flag.load(Ordering::SeqCst),
        "the thread still holds the guard"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(flag.load(Ordering::SeqCst));
}

// The skip underflow finder: M1.2's property generator, ported.

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn full_state(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

/// One editor of M1.2's property (`knot-crdt/tests/effective_update.rs`,
/// `struct Client`): a yrs doc, its own undelivered transactions and the
/// updates it holds back.
struct Client {
    doc: Doc,
    frag: XmlFragmentRef,
    undelivered: Arc<Mutex<Vec<Vec<u8>>>>,
    recording: Arc<AtomicBool>,
    held: Vec<Vec<u8>>,
    _sub: Subscription,
}

impl Client {
    fn new(id: u64) -> Self {
        let doc = Doc::with_client_id(id);
        let frag = doc.get_or_insert_xml_fragment("default");
        let undelivered = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::new(AtomicBool::new(true));
        let sub = {
            let undelivered = undelivered.clone();
            let recording = recording.clone();
            doc.observe_update_v1(move |_, e| {
                if recording.load(Ordering::SeqCst) {
                    undelivered.lock().unwrap().push(e.update.clone());
                }
            })
            .expect("observe_update_v1")
        };
        Client {
            doc,
            frag,
            undelivered,
            recording,
            held: Vec::new(),
            _sub: sub,
        }
    }

    fn take_undelivered(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.undelivered.lock().unwrap())
    }

    fn text_of(&self, txn: &impl ReadTxn, i: u32) -> Option<XmlTextRef> {
        let Some(XmlOut::Element(p)) = self.frag.get(txn, i) else {
            return None;
        };
        match p.get(txn, 0) {
            Some(XmlOut::Text(t)) => Some(t),
            _ => None,
        }
    }

    fn edit(&self, rng: &mut Rng) {
        let mut txn = self.doc.transact_mut();
        let paragraphs = self.frag.len(&txn);
        if paragraphs == 0 || rng.chance(25) {
            let at = rng.below(paragraphs as usize + 1) as u32;
            let p = self
                .frag
                .insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(rng.word()));
            return;
        }
        let i = rng.below(paragraphs as usize) as u32;
        let Some(text) = self.text_of(&txn, i) else {
            self.frag.remove_range(&mut txn, i, 1);
            return;
        };
        let len = text.len(&txn);
        match rng.below(10) {
            0..=3 => {
                let at = rng.below(len as usize + 1) as u32;
                text.insert(&mut txn, at, &rng.word());
            }
            4..=6 if len > 0 => {
                let at = rng.below(len as usize) as u32;
                let n = 1 + rng.below((len - at) as usize) as u32;
                text.remove_range(&mut txn, at, n);
            }
            7..=8 if len > 0 => {
                let at = rng.below(len as usize) as u32;
                let n = 1 + rng.below((len - at) as usize) as u32;
                let mark = ["bold", "italic", "code"][rng.below(3)];
                let value = if rng.chance(70) {
                    Any::Bool(true)
                } else {
                    Any::Null
                };
                let attrs: Attrs = [(mark.into(), value)].into_iter().collect();
                text.format(&mut txn, at, n, attrs);
            }
            _ => self.frag.remove_range(&mut txn, i, 1),
        }
    }
}

/// A panic found by [`find_the_skip_underflow`]: the doc's full state
/// before the apply, the update, and (filled by [`run_seed`]) the state of a
/// doc that holds both.
struct Found {
    /// Which doc panicked: `room`, `log` or `client`.
    doc: &'static str,
    doc_state: Vec<u8>,
    update: Vec<u8>,
    room_state: Vec<u8>,
}

/// Applies `update` to `doc` inside `catch_unwind`; `Err` on a panic.
fn apply_caught(doc: &Doc, update: &[u8]) -> Result<(), ()> {
    catch_unwind(AssertUnwindSafe(|| {
        if let Ok(u) = Update::decode_v1(update) {
            let _ = doc.transact_mut().apply_update(u);
        }
    }))
    .map_err(|_| ())
}

/// A doc that holds everything the clients made: their full states merged
/// into a fresh doc. Each client holds its own clock from 0, so the merge
/// has no gap and its blocks are squashed as the clients' docs hold them.
fn everything(clients: &[Client]) -> Vec<u8> {
    let states: Vec<Vec<u8>> = clients.iter().map(|c| full_state(&c.doc)).collect();
    let merged = yrs::merge_updates_v1(&states).expect("merge the clients' states");
    let d = Doc::new();
    d.transact_mut()
        .apply_update(Update::decode_v1(&merged).expect("decode the merge"))
        .expect("apply the merge");
    full_state(&d)
}

/// What the room persisted for an apply, as M1.2's measured apply decides
/// (`knot-crdt/src/engine.rs`, `apply_update_measuring`): nothing when the
/// apply changed nothing or yrs rejected it; the raw bytes when pending
/// content changed or `encode_update_v1` does not hold exactly the
/// transaction's insertions; else `encode_update_v1`.
fn effective(txn: &mut TransactionMut, raw: &[u8]) -> Vec<u8> {
    let pending = |t: &TransactionMut| {
        let s = t.store();
        (
            s.pending_update().map(|p| p.update.encode_v1()),
            s.pending_ds().map(|d| d.encode_v1()),
        )
    };
    let before = pending(txn);
    let Ok(u) = Update::decode_v1(raw) else {
        return Vec::new();
    };
    if txn.apply_update(u).is_err() {
        return Vec::new();
    }
    let pending_changed = pending(txn) != before;
    let changed = !txn.insert_set().is_empty() || !txn.delete_set().is_empty() || pending_changed;
    txn.commit();
    if !changed {
        return Vec::new();
    }
    if pending_changed {
        return raw.to_vec();
    }
    let e = txn.encode_update_v1();
    if txn.insert_set().is_empty() {
        return e;
    }
    match Update::decode_v1(&e) {
        Ok(w) if &w.insertions(true) == txn.insert_set() => e,
        _ => raw.to_vec(),
    }
}

/// M1.2's server, with a doc that replays the log: `room` applies every
/// delivery, `log` what the room persisted.
struct Server {
    room: Doc,
    log: Doc,
}

impl Server {
    /// Delivers `bytes`; on a panic in either doc, what it panicked on.
    fn deliver(&self, bytes: &[u8]) -> Option<Found> {
        let room_before = full_state(&self.room);
        let mut persist = Vec::new();
        let caught = catch_unwind(AssertUnwindSafe(|| {
            let mut txn = self.room.transact_mut();
            persist = effective(&mut txn, bytes);
        }));
        if caught.is_err() {
            return Some(Found {
                doc: "room",
                room_state: Vec::new(),
                doc_state: room_before,
                update: bytes.to_vec(),
            });
        }
        if persist.is_empty() {
            return None;
        }
        let log_before = full_state(&self.log);
        if apply_caught(&self.log, &persist).is_err() {
            return Some(Found {
                doc: "log",
                doc_state: log_before,
                update: persist,
                room_state: full_state(&self.room),
            });
        }
        None
    }
}

fn pull(c: &Client, server: &Server) -> Option<Found> {
    let sv = c.doc.transact().state_vector();
    let diff = server.room.transact().encode_state_as_update_v1(&sv);
    let before = full_state(&c.doc);
    c.recording.store(false, Ordering::SeqCst);
    let r = apply_caught(&c.doc, &diff);
    c.recording.store(true, Ordering::SeqCst);
    r.err().map(|()| Found {
        doc: "client",
        room_state: Vec::new(),
        doc_state: before,
        update: diff,
    })
}

/// M1.2's `deliveries` without its `has_gaps` guard: a reconnect resend
/// also goes to a server that holds blocks past a gap.
fn deliveries(c: &mut Client, server: &Server, rng: &mut Rng) -> Vec<Vec<u8>> {
    let pick = rng.below(100);
    if pick < 20 && !c.held.is_empty() {
        let i = rng.below(c.held.len());
        return vec![c.held.remove(i)];
    }
    if pick < 45 {
        c.take_undelivered();
        let sv = server.room.transact().state_vector();
        return vec![c.doc.transact().encode_state_as_update_v1(&sv)];
    }
    let mut own = c.take_undelivered();
    if own.is_empty() {
        return Vec::new();
    }
    if own.len() > 1 && rng.chance(20) {
        let i = rng.below(own.len());
        c.held.push(own.remove(i));
    }
    if rng.chance(50) {
        return own;
    }
    let merged = yrs::merge_updates_v1(&own).expect("merge own updates");
    if rng.chance(10) {
        c.held.push(merged);
        return Vec::new();
    }
    vec![merged]
}

/// One seed of M1.2's property, with both `has_gaps` guards removed; the
/// first panic, with the step it happened at.
fn run_seed(seed: u64) -> Option<(usize, Found)> {
    let mut clients: Vec<Client> = (1..=3).map(Client::new).collect();
    let mut found = run_steps(seed, &mut clients)?;
    found.1.room_state = everything(&clients);
    Some(found)
}

fn run_steps(seed: u64, clients: &mut [Client]) -> Option<(usize, Found)> {
    const STEPS: usize = 60;
    let mut rng = Rng::new(seed);
    let server = Server {
        room: Doc::new(),
        log: Doc::new(),
    };
    for step in 0..STEPS {
        let editor = rng.below(clients.len());
        if rng.chance(40)
            && let Some(f) = pull(&clients[editor], &server)
        {
            return Some((step, f));
        }
        for _ in 0..1 + rng.below(3) {
            clients[editor].edit(&mut rng);
        }
        let c = rng.below(clients.len());
        for bytes in deliveries(&mut clients[c], &server, &mut rng) {
            if let Some(f) = server.deliver(&bytes) {
                return Some((step, f));
            }
        }
    }
    for c in clients.iter_mut() {
        let own = c.take_undelivered();
        if !own.is_empty() {
            c.held
                .push(yrs::merge_updates_v1(&own).expect("merge own updates"));
        }
        while !c.held.is_empty() {
            let i = rng.below(c.held.len());
            let bytes = c.held.remove(i);
            if let Some(f) = server.deliver(&bytes) {
                return Some((STEPS, f));
            }
        }
    }
    for c in clients.iter_mut() {
        c.take_undelivered();
        let sv = server.room.transact().state_vector();
        let resend = c.doc.transact().encode_state_as_update_v1(&sv);
        if let Some(f) = server.deliver(&resend) {
            return Some((STEPS + 1, f));
        }
    }
    None
}

/// Finds yrs 0.27.3's skip underflow (`block_store.rs:318`) with M1.2's
/// property generator: prints the first panic's seed, step, doc state,
/// update and the state of a doc that holds both, as hex. Run it on a
/// debug build:
/// `cargo nextest run -p knot-history --test replay --run-ignored only find_the_skip_underflow --no-capture`.
#[test]
#[ignore]
fn find_the_skip_underflow() {
    for seed in 1..=2000 {
        if let Some((step, f)) = run_seed(seed) {
            if f.doc == "client" {
                println!("seed {seed}, step {step}: a client's pull panicked");
                continue;
            }
            println!("seed {seed}, step {step}, in the {} doc", f.doc);
            println!("doc_state  {}", hex(&f.doc_state));
            println!("update     {}", hex(&f.update));
            println!("room_state {}", hex(&f.room_state));
            let r = Replayer::from_state(ReplayMode::Gc, &f.doc_state).expect("the doc state");
            println!(
                "the replay's probe of the update: {:?}",
                r.probe(&f.update).outcome
            );
            println!("of the room state: {:?}", r.probe(&f.room_state).outcome);
            return;
        }
    }
    panic!("no panic in 2000 seeds");
}
