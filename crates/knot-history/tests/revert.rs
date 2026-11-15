//! Restore here (page history M2, Task 1): the revert to a version, by
//! yrs's undo, verified, with a whole-page fallback.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use common::rich;
use common::{History, Shape, has_gaps};
use knot_history::isolate::REPLAY_STACK;
use knot_history::pm_json;
use knot_history::revert::{
    Checkpoint, MAX_NESTING, PlanTrace, RevertError, RevertHooks, RevertKind, RevertOutcome,
    RevertTarget, copy_state, plan, plan_traced, plan_with, rendered, target, verify,
};
use knot_history::{ReplayMode, Replayer, RowOutcome};
use yrs::types::Attrs;
use yrs::types::ToJson;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Assoc, Doc, ID, IndexedSequence, Map, Out, ReadTxn, StickyIndex, Text, Transact,
    TransactionMut, Update, Xml, XmlElementPrelim, XmlElementRef, XmlFragment, XmlFragmentRef,
    XmlOut, XmlTextPrelim, XmlTextRef,
};

// ---------------------------------------------------------------------------
// Helpers

/// The KeepDeleted state of the rows of `h` up to `rev`: a version.
fn version(h: &History, rev: i64) -> Vec<u8> {
    let mut r = Replayer::new(ReplayMode::KeepDeleted);
    for row in h.rows().iter().filter(|r| r.rev.unwrap_or(0) <= rev) {
        r.apply(&row.update_bytes);
    }
    r.encode_state()
}

/// The KeepDeleted state of every row of `h`: the entry before a restore.
fn version_all(h: &History) -> Vec<u8> {
    version(h, i64::MAX)
}

fn last_rev(h: &History) -> i64 {
    h.rows().len() as i64
}

/// A GC replay of `state`.
fn gc(state: &[u8]) -> Replayer {
    Replayer::from_state(ReplayMode::Gc, state).expect("the state applies")
}

/// A GC replay of `state` that applied `updates` in order.
fn gc_after(state: &[u8], updates: &[&[u8]]) -> Replayer {
    let mut r = gc(state);
    for u in updates {
        assert_eq!(r.apply(u).outcome, RowOutcome::Applied);
    }
    r
}

fn json_of(doc: &Doc) -> Vec<u8> {
    rendered(doc).unwrap()
}

/// The text of each top-level block of `doc`.
fn blocks(doc: &Doc) -> Vec<String> {
    let v = pm_json(doc);
    v["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            b["content"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(|t| t["text"].as_str())
                        .collect::<String>()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// A plan's verified revert, or a panic.
fn expect_revert(result: Result<RevertOutcome, RevertError>) -> (Vec<u8>, RevertKind, Vec<String>) {
    match result {
        Ok(RevertOutcome::Revert {
            update,
            kind,
            other_roots_changed,
        }) => (update, kind, other_roots_changed),
        other => panic!("expected a revert, got {other:?}"),
    }
}

/// The version P1 "alpha", P2 "beta" (rev 2), then A deletes "alpha",
/// types " gamma" after "beta" and adds P3 "delta".
fn alpha_beta() -> (History, Vec<u8>) {
    let mut h = History::new(1);
    h.type_para(0, "alpha", common::at(0));
    h.type_para(0, "beta", common::at(1));
    let v = version(&h, 2);
    let bytes = h.clients[0].txn(|txn, frag| {
        let t = text_at_frag(frag, txn, 0);
        t.remove_range(txn, 0, 5);
        let t = text_at_frag(frag, txn, 1);
        t.insert(txn, 4, " gamma");
    });
    h.edit_at(0, bytes, common::at(2));
    h.type_para(0, "delta", common::at(3));
    (h, v)
}

fn text_at_frag(frag: &XmlFragmentRef, txn: &impl ReadTxn, i: u32) -> XmlTextRef {
    match frag.get(txn, i) {
        Some(XmlOut::Element(p)) => match p.get(txn, 0) {
            Some(XmlOut::Text(t)) => t,
            other => panic!("block {i} holds no text: {other:?}"),
        },
        other => panic!("no block at {i}: {other:?}"),
    }
}

/// The clients an update's blocks name.
fn clients_of(update: &[u8]) -> BTreeSet<u64> {
    Update::decode_v1(update)
        .unwrap()
        .insertions(true)
        .client_ids()
        .map(|c| c.get())
        .collect()
}

/// Every client a state names: its state vector and its blocks.
fn named_clients(state: &[u8]) -> BTreeSet<u64> {
    let u = Update::decode_v1(state).unwrap();
    let mut out: BTreeSet<u64> = u.state_vector().iter().map(|(c, _)| c.get()).collect();
    out.extend(u.insertions(true).client_ids().map(|c| c.get()));
    out
}

/// One transaction `f` on `doc`, as an update.
fn txn_update(doc: &Doc, f: impl FnOnce(&mut TransactionMut)) -> Vec<u8> {
    let out = Arc::new(Mutex::new(Vec::new()));
    let sub = {
        let out = out.clone();
        doc.observe_update_v1(move |_, e| out.lock().unwrap().push(e.update.clone()))
            .unwrap()
    };
    f(&mut doc.transact_mut());
    drop(sub);
    let mut all = std::mem::take(&mut *out.lock().unwrap());
    assert_eq!(all.len(), 1, "one transaction, one update");
    all.pop().unwrap()
}

/// A paragraph `text` that a fresh client (id `client`) appends to the page
/// `state`: a concurrent edit.
fn concurrent_paragraph(state: &[u8], client: u64, text: &str) -> Vec<u8> {
    let doc = Doc::with_client_id(client);
    doc.transact_mut()
        .apply_update(Update::decode_v1(state).unwrap())
        .unwrap();
    let frag = doc.get_or_insert_xml_fragment("default");
    txn_update(&doc, |txn| {
        let n = frag.len(txn);
        let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(text));
    })
}

/// Two replays hold the same page: state vectors, delete sets and pm_json
/// equal, no pending content.
fn assert_converged(a: &Replayer, b: &Replayer, what: &str) {
    assert_eq!(a.state_vector(), b.state_vector(), "{what}: state vectors");
    assert_eq!(
        a.doc().transact().snapshot().delete_set,
        b.doc().transact().snapshot().delete_set,
        "{what}: delete sets"
    );
    assert_eq!(json_of(a.doc()), json_of(b.doc()), "{what}: pm_json");
    assert!(!a.has_pending() && !b.has_pending(), "{what}: pending");
}

/// The id of every character of every live text of `doc`'s page, with a
/// sticky index on it.
fn char_ids(doc: &Doc) -> HashMap<ID, StickyIndex> {
    let txn = doc.transact();
    let mut out = HashMap::new();
    let Some(frag) = txn.get_xml_fragment("default") else {
        return out;
    };
    for t in rich::texts(&txn, &frag) {
        for i in 0..t.len(&txn) {
            if let Some(s) = t.sticky_index(&txn, i, Assoc::After)
                && let Some(id) = s.id()
            {
                out.insert(*id, s);
            }
        }
    }
    out
}

/// Where `sticky` points in `doc` now, as the id of the character there.
fn resolve(doc: &Doc, sticky: &StickyIndex) -> Option<ID> {
    let txn = doc.transact();
    let offset = sticky.get_offset(&txn)?;
    let text = XmlTextRef::from(offset.branch);
    text.sticky_index(&txn, offset.index, Assoc::After)?
        .id()
        .copied()
}

/// For every character visible in both `version` (a state) and the live
/// page `live`, a sticky index taken on the live page resolves after
/// `update` to the same character. Returns how many were checked.
fn assert_anchors_survive(version: &[u8], live: &[u8], update: &[u8], what: &str) -> usize {
    let v = gc(version);
    let mut l = gc(live);
    let in_version = char_ids(v.doc());
    let in_live = char_ids(l.doc());
    let shared: Vec<(&ID, &StickyIndex)> = in_live
        .iter()
        .filter(|(id, _)| in_version.contains_key(id))
        .collect();
    assert_eq!(l.apply(update).outcome, RowOutcome::Applied);
    for (id, sticky) in &shared {
        assert_eq!(
            resolve(l.doc(), sticky).as_ref(),
            Some(*id),
            "{what}: the anchor on {id} moved"
        );
    }
    shared.len()
}

/// The element at `path` (child indexes from `default`).
fn el_at(txn: &impl ReadTxn, path: &[u32]) -> XmlElementRef {
    let frag = txn.get_xml_fragment("default").unwrap();
    let mut el = match frag.get(txn, path[0]) {
        Some(XmlOut::Element(e)) => e,
        other => panic!("no element at {path:?}: {other:?}"),
    };
    for &i in &path[1..] {
        el = match el.get(txn, i) {
            Some(XmlOut::Element(e)) => e,
            other => panic!("no element at {path:?}: {other:?}"),
        };
    }
    el
}

fn verified(live: &[u8], update: &[u8], t: &RevertTarget) {
    verify(live, update, &t.json).expect("the revert verifies");
}

// ---------------------------------------------------------------------------
// The target and the copy

#[test]
fn target_is_the_state_and_its_pm_json() {
    let (h, v) = alpha_beta();
    let t = target(&v).unwrap();
    assert_eq!(t.state, v);
    let doc = Replayer::from_state(ReplayMode::KeepDeleted, &v).unwrap();
    assert_eq!(t.json, serde_json::to_vec(&pm_json(doc.doc())).unwrap());
    assert_eq!(t.json, json_of(doc.doc()));
    assert!(!t.nests_too_deep);
    let live = target(&h.room_snapshot()).unwrap();
    assert_ne!(live.json, t.json);
    assert!(target(&[0xff, 1, 2]).is_err());
}

#[test]
fn copy_state_is_a_gc_state_of_the_same_page() {
    let mut h = History::new(1);
    let ledger = h.clients[0]
        .doc
        .get_or_insert_map("knot_applied_suggestions");
    let first = h.clients[0].txn(|txn, frag| {
        let p = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("zzgoneparagraphzz"));
        let p = frag.insert(txn, 1, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("keep qqgonewordqq words"));
        ledger.insert(txn, "k", "kkoldvaluekk");
    });
    h.edit_at(0, first, common::at(0));
    let second = h.clients[0].txn(|txn, frag| {
        frag.remove_range(txn, 0, 1);
        let t = text_at_frag(frag, txn, 0);
        t.remove_range(txn, 5, "qqgonewordqq ".len() as u32);
        ledger.insert(txn, "k", "new");
    });
    h.edit_at(0, second, common::at(1));
    let s = version_all(&h);
    let copy = copy_state(&s).unwrap();

    let kept = Replayer::from_state(ReplayMode::KeepDeleted, &s).unwrap();
    let copied = gc(&copy);
    assert_eq!(json_of(copied.doc()), json_of(kept.doc()));
    assert!(copy.len() < s.len(), "{} < {}", copy.len(), s.len());
    let contains =
        |hay: &[u8], needle: &str| hay.windows(needle.len()).any(|w| w == needle.as_bytes());
    for marker in ["zzgoneparagraphzz", "qqgonewordqq", "kkoldvaluekk"] {
        assert!(contains(&s, marker), "{marker} is in the version");
        assert!(!contains(&copy, marker), "{marker} is not in the copy");
    }
    assert_eq!(blocks(copied.doc()), vec!["keep words".to_string()]);
    let txn = copied.doc().transact();
    let map = txn.get_map("knot_applied_suggestions").unwrap();
    assert_eq!(map.get(&txn, "k"), Some(Out::Any(Any::from("new"))));
}

// ---------------------------------------------------------------------------
// The undo

#[test]
fn a_revert_to_the_current_state_is_unchanged() {
    let (h, _) = alpha_beta();
    let t = target(&version_all(&h)).unwrap();
    assert!(matches!(
        plan(&t, &h.room_snapshot(), RevertHooks::default()),
        Ok(RevertOutcome::Unchanged)
    ));
}

#[test]
fn a_revert_restores_deleted_text_and_removes_added_text() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let (r, kind, other) = expect_revert(plan(&t, &live, RevertHooks::default()));
    assert_eq!(kind, RevertKind::Undo);
    assert!(other.is_empty());
    verified(&live, &r, &t);
    let after = gc_after(&live, &[&r]);
    assert_eq!(blocks(after.doc()), vec!["alpha", "beta"]);
    assert_eq!(json_of(after.doc()), t.json);
}

#[test]
fn a_deleted_paragraph_comes_back_with_its_text() {
    let mut h = History::new(1);
    let first = h.clients[0].txn(|txn, frag| {
        let p = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
        let t = p.push_back(txn, XmlTextPrelim::new("alpha"));
        let bold: Attrs = [("bold".into(), Any::Bool(true))].into_iter().collect();
        t.format(txn, 1, 3, bold);
        let p = frag.insert(txn, 1, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("beta"));
    });
    h.edit_at(0, first, common::at(0));
    let v = version(&h, 1);
    h.pull(1);
    let second = h.clients[1].txn(|txn, frag| frag.remove_range(txn, 0, 1));
    h.edit_at(1, second, common::at(1));
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let (r, kind, _) = expect_revert(plan(&t, &live, RevertHooks::default()));
    assert_eq!(kind, RevertKind::Undo);
    verified(&live, &r, &t);
    let after = gc_after(&live, &[&r]);
    assert_eq!(blocks(after.doc()), vec!["alpha", "beta"]);
    let p = &pm_json(after.doc())["content"][0]["content"];
    assert_eq!(p[1]["text"], "lph");
    assert_eq!(p[1]["marks"][0]["type"], "bold");
}

/// A version with one attribute or mark (by A), and the same changed by B.
fn attribute_case(
    build: impl FnOnce(&mut TransactionMut, &XmlFragmentRef),
    change: impl FnOnce(&mut TransactionMut, &XmlFragmentRef),
) -> (Vec<u8>, Vec<u8>) {
    let mut h = History::new(1);
    let first = h.clients[0].txn(build);
    h.edit_at(0, first, common::at(0));
    let v = version(&h, 1);
    h.pull(1);
    let second = h.clients[1].txn(change);
    h.edit_at(1, second, common::at(1));
    (v, h.room_snapshot())
}

#[test]
fn attribute_reverts_redo() {
    let el = |frag: &XmlFragmentRef, txn: &TransactionMut, i: u32| match frag.get(txn, i) {
        Some(XmlOut::Element(e)) => e,
        other => panic!("{other:?}"),
    };
    let child = |e: &XmlElementRef, txn: &TransactionMut, i: u32| match e.get(txn, i) {
        Some(XmlOut::Element(e)) => e,
        other => panic!("{other:?}"),
    };
    let link = |href: &str| -> Attrs {
        let m: HashMap<String, Any> = [("href".to_string(), Any::from(href))].into();
        [("link".into(), Any::from(m))].into_iter().collect()
    };
    type Case = (
        &'static str,
        Box<dyn FnOnce(&mut TransactionMut, &XmlFragmentRef)>,
        Box<dyn FnOnce(&mut TransactionMut, &XmlFragmentRef)>,
    );
    let cases: Vec<Case> = vec![
        (
            "heading level 1 -> 2",
            Box::new(|txn, frag| {
                let h = frag.push_back(txn, XmlElementPrelim::empty("heading"));
                h.insert_attribute(txn, "level", Any::Number(1.0));
                h.push_back(txn, XmlTextPrelim::new("Title"));
            }),
            Box::new(move |txn, frag| {
                el(frag, txn, 0).insert_attribute(txn, "level", Any::Number(2.0));
            }),
        ),
        (
            "list_item checked false -> true",
            Box::new(|txn, frag| {
                let l = frag.push_back(txn, XmlElementPrelim::empty("bullet_list"));
                let li = l.push_back(txn, XmlElementPrelim::empty("list_item"));
                li.insert_attribute(txn, "checked", Any::Bool(false));
                let p = li.push_back(txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(txn, XmlTextPrelim::new("task"));
            }),
            Box::new(move |txn, frag| {
                let l = el(frag, txn, 0);
                child(&l, txn, 0).insert_attribute(txn, "checked", Any::Bool(true));
            }),
        ),
        (
            "table_cell colwidth [100] -> [200]",
            Box::new(|txn, frag| {
                let t = frag.push_back(txn, XmlElementPrelim::empty("table"));
                let r = t.push_back(txn, XmlElementPrelim::empty("table_row"));
                let c = r.push_back(txn, XmlElementPrelim::empty("table_cell"));
                c.insert_attribute(
                    txn,
                    "colwidth",
                    Any::Array(Arc::from(vec![Any::Number(100.0)])),
                );
                let p = c.push_back(txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(txn, XmlTextPrelim::new("cell"));
            }),
            Box::new(move |txn, frag| {
                let t = el(frag, txn, 0);
                let r = child(&t, txn, 0);
                child(&r, txn, 0).insert_attribute(
                    txn,
                    "colwidth",
                    Any::Array(Arc::from(vec![Any::Number(200.0)])),
                );
            }),
        ),
        (
            "link href changed",
            Box::new(move |txn, frag| {
                let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
                let t = p.push_back(txn, XmlTextPrelim::new("see the docs"));
                t.format(txn, 8, 4, link("https://a.example"));
            }),
            Box::new(move |txn, frag| {
                let t = text_at_frag(frag, txn, 0);
                t.format(txn, 8, 4, link("https://b.example"));
            }),
        ),
        (
            "bold added to a word",
            Box::new(|txn, frag| {
                let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(txn, XmlTextPrelim::new("one two three"));
            }),
            Box::new(|txn, frag| {
                let t = text_at_frag(frag, txn, 0);
                let bold: Attrs = [("bold".into(), Any::Bool(true))].into_iter().collect();
                t.format(txn, 4, 3, bold);
            }),
        ),
    ];
    let mut fallbacks = Vec::new();
    let total = cases.len();
    for (name, build, change) in cases {
        let (v, live) = attribute_case(build, change);
        let t = target(&v).unwrap();
        let (r, kind, _) = expect_revert(plan(&t, &live, RevertHooks::default()));
        verified(&live, &r, &t);
        println!("attribute case {name:?}: {kind:?}");
        if kind == RevertKind::Fallback {
            fallbacks.push(name);
        }
    }
    println!(
        "attribute cases that fell back: {} of {total}: {fallbacks:?}",
        fallbacks.len()
    );
}

#[test]
fn restored_items_use_one_fresh_client() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let (r, _, _) = expect_revert(plan(&t, &live, RevertHooks::default()));
    let clients = clients_of(&r);
    assert_eq!(clients.len(), 1, "{clients:?}");
    let c = clients.into_iter().next().unwrap();
    assert!(!named_clients(&v).contains(&c));
    assert!(!named_clients(&live).contains(&c));
}

/// The ledger and the map "other" of a doc, as JSON.
fn side_maps(doc: &Doc) -> (Any, Any) {
    let txn = doc.transact();
    let read = |name: &str| {
        txn.get_map(name)
            .map(|m| m.to_json(&txn))
            .unwrap_or(Any::Null)
    };
    (read("knot_applied_suggestions"), read("other"))
}

#[test]
fn the_suggestion_ledger_is_not_reverted() {
    for (hooks, expected) in [
        (RevertHooks::default(), RevertKind::Undo),
        (
            RevertHooks {
                skip_undo: true,
                ..Default::default()
            },
            RevertKind::Fallback,
        ),
    ] {
        let mut h = History::new(1);
        h.type_para(0, "alpha", common::at(0));
        let v = version(&h, 1);
        let ledger = h.clients[0]
            .doc
            .get_or_insert_map("knot_applied_suggestions");
        let other = h.clients[0].doc.get_or_insert_map("other");
        let maps = h.clients[0].txn(|txn, _| {
            ledger.insert(txn, "s1", "applied");
            other.insert(txn, "o", "kept");
        });
        h.edit_at(0, maps.clone(), common::at(1));
        let text = h.clients[0].txn(|txn, frag| {
            let t = text_at_frag(frag, txn, 0);
            t.insert(txn, 5, " more");
        });
        h.edit_at(0, text, common::at(2));
        let live = h.room_snapshot();
        let t = target(&v).unwrap();
        let (r, kind, other_roots) = expect_revert(plan(&t, &live, hooks));
        assert_eq!(kind, expected);
        assert_eq!(other_roots, vec!["other".to_string()]);
        verified(&live, &r, &t);
        let before = gc(&live);
        let after = gc_after(&live, &[&r]);
        assert_eq!(side_maps(after.doc()), side_maps(before.doc()));
        assert_eq!(
            side_maps(after.doc()),
            (
                Any::from(HashMap::from([("s1".to_string(), Any::from("applied"))])),
                Any::from(HashMap::from([("o".to_string(), Any::from("kept"))]))
            )
        );
        let map_items = Update::decode_v1(&maps).unwrap().insertions(true);
        let ds = Update::decode_v1(&r).unwrap().delete_set().clone();
        assert!(
            ds.intersect(&map_items).is_empty(),
            "the revert deletes no map entry"
        );
    }
}

/// `gen(seed, 40)` for 50 seeds, without gaps, a version at half the
/// rows: every verified undo keeps the anchors on shared characters.
fn anchors_over(name: &str, gen_fn: fn(u64, usize) -> History) {
    let (mut checked, mut undo, mut fallback, mut unchanged, mut gapped) = (0, 0, 0, 0, 0);
    for seed in 1..=50 {
        let h = gen_fn(seed, 40);
        if h.rows().is_empty() {
            continue;
        }
        if has_gaps(h.room()) {
            gapped += 1;
            continue;
        }
        let live = h.room_snapshot();
        let v = version(&h, (last_rev(&h) / 2).max(1));
        let t = target(&v).unwrap();
        match plan(&t, &live, RevertHooks::default()) {
            Ok(RevertOutcome::Unchanged) => unchanged += 1,
            Ok(RevertOutcome::Revert {
                kind: RevertKind::Fallback,
                ..
            }) => fallback += 1,
            Ok(RevertOutcome::Revert {
                update,
                kind: RevertKind::Undo,
                ..
            }) => {
                undo += 1;
                checked += assert_anchors_survive(&v, &live, &update, &format!("{name} {seed}"));
            }
            Err(e) => panic!("{name} seed {seed}: {e}"),
        }
    }
    println!(
        "anchors {name}: {undo} undo cases ({checked} anchors), {fallback} fallbacks skipped, \
         {unchanged} unchanged, {gapped} gap histories skipped"
    );
    assert!(undo > 0);
}

#[test]
fn anchors_on_untouched_text_survive_a_revert() {
    anchors_over("generate", History::generate);
    anchors_over("generate_rich", History::generate_rich);
}

// ---------------------------------------------------------------------------
// The fallback

#[test]
fn a_skipped_undo_falls_back_to_a_whole_page_replace() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let hooks = RevertHooks {
        skip_undo: true,
        ..Default::default()
    };
    let (r, kind, _) = expect_revert(plan(&t, &live, hooks));
    assert_eq!(kind, RevertKind::Fallback);
    verified(&live, &r, &t);
    assert_eq!(blocks(gc_after(&live, &[&r]).doc()), vec!["alpha", "beta"]);
}

#[test]
fn the_fallback_keeps_typed_attributes() {
    let link = |href: &str| -> Attrs {
        let m: HashMap<String, Any> = [("href".to_string(), Any::from(href))].into();
        [("link".into(), Any::from(m))].into_iter().collect()
    };
    let mut h = History::new(1);
    let first = h.clients[0].txn(|txn, frag| {
        let hd = frag.push_back(txn, XmlElementPrelim::empty("heading"));
        hd.insert_attribute(txn, "level", Any::Number(1.0));
        hd.push_back(txn, XmlTextPrelim::new("Title"));
        let bl = frag.push_back(txn, XmlElementPrelim::empty("bullet_list"));
        let li = bl.push_back(txn, XmlElementPrelim::empty("list_item"));
        li.insert_attribute(txn, "checked", Any::Bool(false));
        let p = li.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("task"));
        let ol = frag.push_back(txn, XmlElementPrelim::empty("ordered_list"));
        ol.insert_attribute(txn, "start", Any::Number(3.0));
        let li = ol.push_back(txn, XmlElementPrelim::empty("list_item"));
        let p = li.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("third"));
        let table = frag.push_back(txn, XmlElementPrelim::empty("table"));
        let row = table.push_back(txn, XmlElementPrelim::empty("table_row"));
        let cell = row.push_back(txn, XmlElementPrelim::empty("table_cell"));
        cell.insert_attribute(txn, "colspan", Any::Number(2.0));
        cell.insert_attribute(
            txn,
            "colwidth",
            Any::Array(Arc::from(vec![Any::Number(100.0)])),
        );
        let p = cell.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("cell"));
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        let t = p.push_back(txn, XmlTextPrelim::new("see the docs"));
        t.format(txn, 8, 4, link("https://a.example"));
        let img = frag.push_back(txn, XmlElementPrelim::empty("image"));
        img.insert_attribute(txn, "src", Any::from("x.png"));
    });
    h.edit_at(0, first, common::at(0));
    let v = version(&h, 1);
    h.pull(1);
    let second = h.clients[1].txn(|txn, frag| {
        let get = |txn: &TransactionMut, path: &[u32]| el_at(txn, path);
        get(txn, &[0]).insert_attribute(txn, "level", Any::Number(2.0));
        get(txn, &[1, 0]).insert_attribute(txn, "checked", Any::Bool(true));
        get(txn, &[2]).insert_attribute(txn, "start", Any::Number(1.0));
        get(txn, &[3, 0, 0]).insert_attribute(
            txn,
            "colwidth",
            Any::Array(Arc::from(vec![Any::Number(200.0)])),
        );
        let t = text_at_frag(frag, txn, 4);
        t.format(txn, 8, 4, link("https://b.example"));
        frag.remove_range(txn, 5, 1);
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("added"));
    });
    h.edit_at(1, second, common::at(1));
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let hooks = RevertHooks {
        skip_undo: true,
        ..Default::default()
    };
    let (r, kind, _) = expect_revert(plan(&t, &live, hooks));
    assert_eq!(kind, RevertKind::Fallback);
    verified(&live, &r, &t);
    let after = gc_after(&live, &[&r]);
    let txn = after.doc().transact();
    let attr = |path: &[u32], key: &str| el_at(&txn, path).get_attribute(&txn, key);
    assert_eq!(attr(&[0], "level"), Some(Out::Any(Any::Number(1.0))));
    assert_eq!(attr(&[1, 0], "checked"), Some(Out::Any(Any::Bool(false))));
    assert_eq!(attr(&[2], "start"), Some(Out::Any(Any::Number(3.0))));
    assert_eq!(
        attr(&[3, 0, 0], "colspan"),
        Some(Out::Any(Any::Number(2.0)))
    );
    assert_eq!(
        attr(&[3, 0, 0], "colwidth"),
        Some(Out::Any(Any::Array(Arc::from(vec![Any::Number(100.0)]))))
    );
    assert_eq!(attr(&[5], "src"), Some(Out::Any(Any::from("x.png"))));
}

/// Runs `f` with the panic hook silenced.
fn quietly<T>(f: impl FnOnce() -> T) -> T {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let out = f();
    std::panic::set_hook(hook);
    out
}

#[test]
fn an_undo_that_panics_falls_back() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let hooks = RevertHooks {
        panic_in_undo: true,
        ..Default::default()
    };
    let (result, trace) = quietly(|| plan_traced(&t, &live, hooks));
    let (r, kind, _) = expect_revert(result);
    assert_eq!(kind, RevertKind::Fallback);
    assert!(trace.undo_panicked);
    assert_eq!(trace.undo_captures, None);
    verified(&live, &r, &t);
    let (r, kind, _) = expect_revert(plan(&t, &live, RevertHooks::default()));
    assert_eq!(kind, RevertKind::Undo);
    verified(&live, &r, &t);
}

#[test]
fn an_undo_that_does_not_verify_falls_back() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let hooks = RevertHooks {
        corrupt_undo: true,
        ..Default::default()
    };
    let (result, trace) = plan_traced(&t, &live, hooks);
    let (r, kind, _) = expect_revert(result);
    assert_eq!(kind, RevertKind::Fallback);
    verified(&live, &r, &t);
    assert_eq!(trace.undo_captures, Some(1));
    assert!(!trace.undo_panicked);
    assert!(trace.undo_unverified.is_some(), "{trace:?}");
}

#[test]
fn an_undo_that_captured_nothing_falls_back() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let hooks = RevertHooks {
        empty_undo: true,
        ..Default::default()
    };
    let (result, trace) = plan_traced(&t, &live, hooks);
    let (r, kind, _) = expect_revert(result);
    assert_eq!(kind, RevertKind::Fallback);
    assert_eq!(trace.undo_captures, Some(0));
    verified(&live, &r, &t);
}

// ---------------------------------------------------------------------------
// Undoing a restore

#[test]
fn a_revert_of_a_revert_restores_the_state_before_it() {
    for first in [
        RevertHooks::default(),
        RevertHooks {
            skip_undo: true,
            ..Default::default()
        },
    ] {
        let (h, v) = alpha_beta();
        let l = h.room_snapshot();
        let t1 = target(&v).unwrap();
        let (r1, kind1, _) = expect_revert(plan(&t1, &l, first));
        verified(&l, &r1, &t1);
        let l1 = gc_after(&l, &[&r1]).encode_state();
        let t2 = target(&version_all(&h)).unwrap();
        let (r2, kind2, _) = expect_revert(plan(&t2, &l1, RevertHooks::default()));
        println!("revert of a {kind1:?} revert: {kind2:?}");
        verified(&l1, &r2, &t2);
        let after = gc_after(&l1, &[&r2]);
        assert_eq!(json_of(after.doc()), json_of(gc(&l).doc()));
        assert_eq!(blocks(after.doc()), vec!["", "beta gamma", "delta"]);
        assert_ne!(clients_of(&r1), clients_of(&r2));
        if kind1 == RevertKind::Undo && kind2 == RevertKind::Undo {
            // "beta" was in L and in L1, and neither restore touched it.
            let beta: Vec<(ID, StickyIndex)> = {
                let d = gc(&l1);
                let txn = d.doc().transact();
                let frag = txn.get_xml_fragment("default").unwrap();
                let t = text_at_frag(&frag, &txn, 1);
                (0..4)
                    .map(|i| {
                        let s = t.sticky_index(&txn, i, Assoc::After).unwrap();
                        (*s.id().unwrap(), s)
                    })
                    .collect()
            };
            for (id, s) in &beta {
                assert_eq!(resolve(after.doc(), s), Some(*id));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cancellation, nesting, errors

#[test]
fn a_cancelled_plan_stops_at_its_next_checkpoint() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let run = |hooks: RevertHooks, stop_from: Option<Checkpoint>| {
        let seen = Mutex::new(Vec::new());
        let stopped = Mutex::new(false);
        let result = plan_with(&t, &live, hooks, &|c| {
            seen.lock().unwrap().push(c);
            let mut s = stopped.lock().unwrap();
            if stop_from == Some(c) || stop_from == Some(Checkpoint::Undo) {
                *s = true;
            }
            !*s
        });
        (result, seen.into_inner().unwrap())
    };
    let (result, seen) = run(RevertHooks::default(), Some(Checkpoint::Undo));
    assert!(matches!(result, Err(RevertError::Cancelled)), "{result:?}");
    assert_eq!(seen, vec![Checkpoint::Undo]);

    let corrupt = RevertHooks {
        corrupt_undo: true,
        ..Default::default()
    };
    let (result, seen) = run(corrupt, Some(Checkpoint::Fallback));
    assert!(matches!(result, Err(RevertError::Cancelled)), "{result:?}");
    assert_eq!(
        seen,
        vec![
            Checkpoint::Undo,
            Checkpoint::UndoVerify,
            Checkpoint::Fallback
        ]
    );

    let (result, seen) = run(RevertHooks::default(), None);
    let (r, kind, _) = expect_revert(result);
    assert_eq!(kind, RevertKind::Undo);
    verified(&live, &r, &t);
    assert_eq!(seen, vec![Checkpoint::Undo, Checkpoint::UndoVerify]);
}

/// A page with P "x" and, when `depth > 0`, a chain of `depth` nested
/// blockquotes under `default` (the innermost holding a paragraph).
fn chain_page(h: &mut History, depth: usize) {
    let bytes = h.clients[0].txn(|txn, frag| {
        let n = frag.len(txn);
        let mut el = frag.insert(txn, n, XmlElementPrelim::empty("blockquote"));
        for _ in 1..depth {
            el = el.push_back(txn, XmlElementPrelim::empty("blockquote"));
        }
        let p = el.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("deep"));
    });
    h.edit_at(0, bytes, common::at(1));
}

fn remove_chain(h: &mut History) {
    let bytes = h.clients[0].txn(|txn, frag| frag.remove_range(txn, 1, 1));
    h.edit_at(0, bytes, common::at(2));
}

fn on_replay_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(REPLAY_STACK)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_page_nested_past_the_limit_is_not_reverted() {
    on_replay_stack(|| {
        let deep_err = |result: Result<RevertOutcome, RevertError>| match result {
            Err(RevertError::Unverified(why)) => {
                assert!(why.contains("nested past the limit"), "{why}")
            }
            other => panic!("expected the nesting refusal, got {other:?}"),
        };
        let skip = RevertHooks {
            skip_undo: true,
            ..Default::default()
        };

        // A deep version, a live page that removed it.
        let mut h = History::new(1);
        h.type_para(0, "x", common::at(0));
        chain_page(&mut h, MAX_NESTING + 2);
        let v = version(&h, 2);
        remove_chain(&mut h);
        let live = h.room_snapshot();
        let t = target(&v).unwrap();
        assert!(t.nests_too_deep);
        deep_err(plan(&t, &live, RevertHooks::default()));
        deep_err(plan(&t, &live, skip));

        // A shallow version, a deep live page.
        let mut h = History::new(1);
        h.type_para(0, "x", common::at(0));
        let v = version(&h, 1);
        chain_page(&mut h, MAX_NESTING + 2);
        let t = target(&v).unwrap();
        assert!(!t.nests_too_deep);
        deep_err(plan(&t, &h.room_snapshot(), RevertHooks::default()));
        deep_err(plan(&t, &h.room_snapshot(), skip));

        // Exactly at the limit: the paragraph sits at MAX_NESTING.
        let mut h = History::new(1);
        h.type_para(0, "x", common::at(0));
        chain_page(&mut h, MAX_NESTING - 2);
        let v = version(&h, 2);
        remove_chain(&mut h);
        let live = h.room_snapshot();
        let t = target(&v).unwrap();
        assert!(!t.nests_too_deep);
        let (r, kind, _) = expect_revert(plan(&t, &live, RevertHooks::default()));
        println!("a page at the nesting limit: {kind:?}");
        verified(&live, &r, &t);
    });
}

#[test]
fn nothing_verified_is_an_error() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    for hooks in [
        RevertHooks {
            skip_undo: true,
            fail_fallback: true,
            ..Default::default()
        },
        RevertHooks {
            corrupt_undo: true,
            fail_fallback: true,
            ..Default::default()
        },
    ] {
        let result = plan(&t, &live, hooks);
        assert!(
            matches!(result, Err(RevertError::Unverified(_))),
            "{hooks:?}: {result:?}"
        );
    }
}

#[test]
fn other_roots_are_reported_not_reverted() {
    for (hooks, expected) in [
        (RevertHooks::default(), RevertKind::Undo),
        (
            RevertHooks {
                skip_undo: true,
                ..Default::default()
            },
            RevertKind::Fallback,
        ),
    ] {
        // A key of "other" set after the version.
        let mut h = History::new(1);
        h.type_para(0, "alpha", common::at(0));
        let v = version(&h, 1);
        let other = h.clients[0].doc.get_or_insert_map("other");
        let set = h.clients[0].txn(|txn, frag| {
            other.insert(txn, "o", "kept");
            text_at_frag(frag, txn, 0).insert(txn, 5, " more");
        });
        h.edit_at(0, set.clone(), common::at(1));
        let live = h.room_snapshot();
        let t = target(&v).unwrap();
        let (r, kind, roots) = expect_revert(plan(&t, &live, hooks));
        assert_eq!(kind, expected);
        assert_eq!(roots, vec!["other".to_string()]);
        verified(&live, &r, &t);
        let after = gc_after(&live, &[&r]);
        assert_eq!(side_maps(after.doc()).1, side_maps(gc(&live).doc()).1);
        let ds = Update::decode_v1(&r).unwrap().delete_set().clone();
        let set_items = Update::decode_v1(&set).unwrap().insertions(true);
        // The " more" text is the revert's to delete; the map entry is not.
        let map_entry = {
            let txn = after.doc().transact();
            let m = txn.get_map("other").unwrap();
            assert_eq!(m.get(&txn, "o"), Some(Out::Any(Any::from("kept"))));
            m.len(&txn)
        };
        assert_eq!(map_entry, 1);
        assert!(!ds.is_empty() && !set_items.is_empty());

        // The ledger alone changed (and the text): nothing to report.
        let mut h = History::new(1);
        h.type_para(0, "alpha", common::at(0));
        let v = version(&h, 1);
        let ledger = h.clients[0]
            .doc
            .get_or_insert_map("knot_applied_suggestions");
        let set = h.clients[0].txn(|txn, frag| {
            ledger.insert(txn, "s", "applied");
            text_at_frag(frag, txn, 0).insert(txn, 5, " more");
        });
        h.edit_at(0, set, common::at(1));
        let live = h.room_snapshot();
        let t = target(&v).unwrap();
        let (r, kind, roots) = expect_revert(plan(&t, &live, hooks));
        assert_eq!(kind, expected);
        assert!(roots.is_empty(), "{roots:?}");
        verified(&live, &r, &t);
    }
}

#[test]
fn an_edit_inside_a_removed_paragraph_goes_with_it_on_both_peers() {
    for hooks in [
        RevertHooks::default(),
        RevertHooks {
            skip_undo: true,
            ..Default::default()
        },
    ] {
        let mut h = History::new(1);
        h.type_para(0, "alpha", common::at(0));
        h.type_para(0, "beta", common::at(1));
        let v = version(&h, 2);
        h.type_para(0, "delta", common::at(2));
        h.pull(1);
        let typed = h.clients[1].txn(|txn, frag| {
            text_at_frag(frag, txn, 2).insert(txn, 5, " typed");
        });
        let live = h.room_snapshot();
        let t = target(&v).unwrap();
        let (r, kind, _) = expect_revert(plan(&t, &live, hooks));
        println!("an edit inside a removed paragraph: {kind:?}");
        verified(&live, &r, &t);
        let a = gc_after(&live, &[&r, &typed]);
        let b = gc_after(&live, &[&typed, &r]);
        assert_converged(&a, &b, "an edit inside a removed paragraph");
        for peer in [&a, &b] {
            assert_eq!(blocks(peer.doc()), vec!["alpha", "beta"]);
            let json = String::from_utf8(json_of(peer.doc())).unwrap();
            assert!(!json.contains("typed") && !json.contains("delta"));
        }
    }
}

#[test]
fn a_live_state_that_does_not_decode_is_an_error() {
    let (_, v) = alpha_beta();
    let t = target(&v).unwrap();
    assert!(matches!(
        plan(&t, &[0xff, 1], RevertHooks::default()),
        Err(RevertError::Live(_))
    ));
}

#[test]
fn verify_refuses_an_update_that_leaves_pending_or_differs() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();

    // A client the live page never saw types twice; its second update
    // alone names an origin the live page lacks, so it parks.
    let z = Doc::with_client_id(77);
    let frag = z.get_or_insert_xml_fragment("default");
    txn_update(&z, |txn| {
        let p = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("first"));
    });
    let parks = txn_update(&z, |txn| {
        text_at_frag(&frag, txn, 0).insert(txn, 5, " second")
    });
    assert!(verify(&live, &parks, &t.json).is_err());
    assert!(verify(&live, Update::EMPTY_V1, &t.json).is_err());
    let (r, _, _) = expect_revert(plan(&t, &live, RevertHooks::default()));
    assert!(verify(&live, &r, &t.json).is_ok());
}

#[test]
fn two_peers_converge_after_a_revert() {
    let (h, v) = alpha_beta();
    let live = h.room_snapshot();
    let t = target(&v).unwrap();
    let (r, _, _) = expect_revert(plan(&t, &live, RevertHooks::default()));
    let e = concurrent_paragraph(&live, 4242, "concurrent");
    let a = gc_after(&live, &[&r, &e]);
    let b = gc_after(&live, &[&e, &r]);
    assert_converged(&a, &b, "two peers");
    for peer in [&a, &b] {
        assert!(blocks(peer.doc()).contains(&"concurrent".to_string()));
    }
}

// ---------------------------------------------------------------------------
// The properties

/// Counts by outcome and cause, printed per generator.
#[derive(Default)]
struct Tally {
    undo: usize,
    fallback: usize,
    unchanged: usize,
    unverified: usize,
    causes: BTreeMap<String, usize>,
}

impl Tally {
    fn count(&mut self, result: &Result<RevertOutcome, RevertError>, trace: &PlanTrace) {
        match result {
            Ok(RevertOutcome::Unchanged) => self.unchanged += 1,
            Ok(RevertOutcome::Revert { kind, .. }) => match kind {
                RevertKind::Undo => self.undo += 1,
                RevertKind::Fallback => {
                    self.fallback += 1;
                    *self.causes.entry(cause(trace)).or_default() += 1;
                }
            },
            Err(_) => {
                self.unverified += 1;
                let why = format!(
                    "unverified: {} / fallback: {}",
                    cause(trace),
                    trace.fallback_unverified.as_deref().unwrap_or("-")
                );
                *self.causes.entry(why).or_default() += 1;
            }
        }
    }

    fn undo_share(&self) -> f64 {
        let reverts = self.undo + self.fallback + self.unverified;
        if reverts == 0 {
            1.0
        } else {
            self.undo as f64 / reverts as f64
        }
    }

    fn print(&self, what: &str) {
        println!(
            "{what}: undo {} / fallback {} / unchanged {} / unverified {} (undo share {:.1} %)",
            self.undo,
            self.fallback,
            self.unchanged,
            self.unverified,
            100.0 * self.undo_share()
        );
        for (cause, n) in &self.causes {
            println!("    {n} × {cause}");
        }
    }
}

/// Why the undo gave no verified update.
fn cause(trace: &PlanTrace) -> String {
    if trace.undo_panicked {
        return "undo panicked".to_string();
    }
    match (trace.undo_captures, &trace.undo_unverified) {
        (_, Some(why)) if why.starts_with("the live state does not apply") => why.clone(),
        (Some(0), _) => "undo captured nothing".to_string(),
        (Some(n), _) if n > 1 => "undo captured several".to_string(),
        (_, Some(why)) => format!("undo unverified: {why}"),
        (None, None) => "undo skipped".to_string(),
        _ => "undo unverified".to_string(),
    }
}

fn property(name: &str, gen_fn: fn(u64, usize) -> History, per_shape: bool) {
    let mut gap_free = Tally::default();
    let mut gapped = Tally::default();
    let mut gap_fallback = Tally::default();
    let mut second_after_undo = Tally::default();
    let mut second_after_fallback = Tally::default();
    // Shape → (cases, undo cases).
    let mut shapes: BTreeMap<Shape, (usize, usize)> = BTreeMap::new();
    let mut failures = Vec::new();

    for seed in 1..=200u64 {
        let h = gen_fn(seed, 60);
        let n = last_rev(&h);
        if n == 0 {
            continue;
        }
        let live = h.room_snapshot();
        let gaps = has_gaps(h.room());
        let before_restore = target(&version_all(&h)).unwrap();
        let revs: BTreeSet<i64> = [n / 4, n / 2, 3 * n / 4]
            .into_iter()
            .map(|r| r.max(1))
            .collect();
        for rev in revs {
            let what = format!("{name} seed {seed} rev {rev}/{n}");
            let t = target(&version(&h, rev)).unwrap();
            let (result, trace) = plan_traced(&t, &live, RevertHooks::default());
            if let Ok(RevertOutcome::Revert { update, .. }) = &result {
                if let Err(e) = verify(&live, update, &t.json) {
                    failures.push(format!("{what}: a returned revert does not verify: {e}"));
                }
                // A second peer that typed a paragraph meanwhile converges.
                let e = concurrent_paragraph(&live, 0x7fff_0000 + seed, "meanwhile");
                let a = gc_after(&live, &[update, &e]);
                let b = gc_after(&live, &[&e, update]);
                assert_converged(&a, &b, &what);
            }
            if gaps {
                gapped.count(&result, &trace);
                if let Err(e) = &result
                    && !matches!(e, RevertError::Unverified(_))
                {
                    failures.push(format!("{what}: {e}"));
                }
                // The whole-page replace restores every gap case.
                let skip = RevertHooks {
                    skip_undo: true,
                    ..Default::default()
                };
                let (fb, fb_trace) = plan_traced(&t, &live, skip);
                gap_fallback.count(&fb, &fb_trace);
                match &fb {
                    Ok(RevertOutcome::Revert {
                        update,
                        kind: RevertKind::Fallback,
                        ..
                    }) => {
                        if let Err(e) = verify(&live, update, &t.json) {
                            failures.push(format!("{what}: the gap fallback does not verify: {e}"));
                        }
                    }
                    Ok(RevertOutcome::Unchanged) => {}
                    other => failures.push(format!("{what}: the gap fallback: {other:?}")),
                }
                continue;
            }

            gap_free.count(&result, &trace);
            if trace.undo_captures.is_some_and(|c| c > 1) {
                failures.push(format!(
                    "{what}: the undo captured {:?}",
                    trace.undo_captures
                ));
            }
            if per_shape && !matches!(result, Ok(RevertOutcome::Unchanged)) {
                let present: BTreeSet<Shape> = h
                    .shapes()
                    .iter()
                    .filter(|(r, _)| *r > rev)
                    .map(|(_, s)| *s)
                    .collect();
                let undo = matches!(
                    result,
                    Ok(RevertOutcome::Revert {
                        kind: RevertKind::Undo,
                        ..
                    })
                );
                for s in present {
                    let e = shapes.entry(s).or_default();
                    e.0 += 1;
                    e.1 += usize::from(undo);
                }
            }
            let first = match result {
                Ok(RevertOutcome::Revert { update, .. }) => update,
                Ok(RevertOutcome::Unchanged) => continue,
                Err(e) => {
                    failures.push(format!("{what}: {e} ({trace:?})"));
                    continue;
                }
            };

            // Undoing the restore: from the page after it back to the entry
            // before it, after an undo restore and after a fallback restore.
            let skip = RevertHooks {
                skip_undo: true,
                ..Default::default()
            };
            let fallback_first = match plan(&t, &live, skip) {
                Ok(RevertOutcome::Revert { update, .. }) => update,
                other => {
                    failures.push(format!("{what}: the fallback restore: {other:?}"));
                    continue;
                }
            };
            for (first, tally) in [
                (&first, &mut second_after_undo),
                (&fallback_first, &mut second_after_fallback),
            ] {
                let l1 = gc_after(&live, &[first]).encode_state();
                let (second, trace) = plan_traced(&before_restore, &l1, RevertHooks::default());
                tally.count(&second, &trace);
                gap_free.count(&second, &trace);
                if per_shape {
                    let e = shapes.entry(Shape::Revert).or_default();
                    e.0 += 1;
                    e.1 += usize::from(matches!(
                        second,
                        Ok(RevertOutcome::Revert {
                            kind: RevertKind::Undo,
                            ..
                        })
                    ));
                }
                match &second {
                    Ok(RevertOutcome::Revert { update, .. }) => {
                        if let Err(e) = verify(&l1, update, &before_restore.json) {
                            failures.push(format!("{what}: the revert of a revert: {e}"));
                        }
                    }
                    other => failures.push(format!(
                        "{what}: the revert of a revert: {other:?} ({trace:?})"
                    )),
                }
            }
        }
    }

    gap_free.print(&format!("{name}, gap-free (second plans included)"));
    second_after_undo.print(&format!("{name}, revert of a revert, undo first"));
    second_after_fallback.print(&format!("{name}, revert of a revert, fallback first"));
    gapped.print(&format!("{name}, with gaps"));
    gap_fallback.print(&format!("{name}, with gaps, fallback only"));
    if per_shape {
        for (shape, (cases, undo)) in &shapes {
            println!(
                "    {name} {shape:?}: undo {undo} of {cases} ({:.1} %)",
                100.0 * *undo as f64 / (*cases).max(1) as f64
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        gap_free.undo_share() >= 0.9,
        "{name}: the gap-free undo share is {:.1} %",
        100.0 * gap_free.undo_share()
    );
}

#[test]
fn a_revert_restores_the_target_across_random_histories() {
    property("generate", History::generate, false);
}

#[test]
fn a_revert_restores_structured_targets_across_random_histories() {
    property("generate_rich", History::generate_rich, true);
}

/// A bold mark set on "XX" in "TXXtlkaale" by A, and "XX" deleted by B:
/// the mark's two formatting items stay, around nothing, and yrs's diff
/// splits the text there.
#[test]
fn the_comparison_joins_text_as_the_editor_does() {
    let mut h = History::new(1);
    let first = h.clients[0].txn(|txn, frag| {
        let p = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
        let t = p.push_back(txn, XmlTextPrelim::new("TXXtlkaale"));
        let bold: Attrs = [("bold".into(), Any::Bool(true))].into_iter().collect();
        t.format(txn, 1, 2, bold);
    });
    h.edit_at(0, first, common::at(0));
    h.pull(1);
    let second = h.clients[1].txn(|txn, frag| text_at_frag(frag, txn, 0).remove_range(txn, 1, 2));
    h.edit_at(1, second, common::at(1));
    let v = version_all(&h);
    h.type_para(1, "added", common::at(2));
    let live = h.room_snapshot();

    let version_doc = Replayer::from_state(ReplayMode::KeepDeleted, &v).unwrap();
    let raw = &pm_json(version_doc.doc())["content"][0]["content"];
    assert_eq!(
        raw.as_array().unwrap().len(),
        2,
        "the diff splits the text: {raw}"
    );
    let t = target(&v).unwrap();
    assert_eq!(
        t.json,
        serde_json::to_vec(&serde_json::json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "Ttlkaale"}]}
        ]}))
        .unwrap()
    );
    for hooks in [
        RevertHooks::default(),
        RevertHooks {
            skip_undo: true,
            ..Default::default()
        },
    ] {
        let (r, kind, _) = expect_revert(plan(&t, &live, hooks));
        println!("a version whose text the diff splits: {kind:?}");
        verified(&live, &r, &t);
    }
}
