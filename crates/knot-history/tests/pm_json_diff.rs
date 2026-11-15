//! `pm_json_diff`, the "Show changes" render (page history M1.4, Task 2):
//! the page at an entry's end as ProseMirror JSON, the entry's own changes
//! as `historyInsert` / `historyDelete` marks and `historyChange` node
//! attributes, on the union-doc diff the stored word counts come from.
//!
//! It also holds the architecture's spike "encode_state_from_snapshot on a
//! mixed skip_gc doc" for its M1.4 use, `diff::lo_doc` (`D_lo` from `S_lo`);
//! M1.3's `tests/words.rs` covers the union doc.

mod common;

use std::collections::HashMap;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{History, at, author};
use knot_history::canonical::visible_content;
use knot_history::diff::{DiffError, EntryDiff, entry_diff, lo_doc, text_chunks};
use knot_history::pm_json::{
    HISTORY_CHANGE, HISTORY_DELETE, HISTORY_INSERT, pm_json, pm_json_diff,
};
use knot_history::words::{WordCounts, count_words};
use knot_history::{Progress, ReplayMode, Replayer, RowOutcome};
use serde_json::{Value, json};
use uuid::Uuid;
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Doc, GetString, IdSet, ReadTxn, Text, Transact, TransactionMut, Update, Xml,
    XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

const A: usize = 0;
const B: usize = 1;
const C: usize = 2;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A history replayed into a skip-GC doc, with each author's insert and
/// delete sets merged over all of their rows (one entry per author).
struct Replayed {
    r: Replayer,
    sets: HashMap<Option<Uuid>, (IdSet, IdSet)>,
}

/// Replays `h`'s rows with `ReplayMode::KeepDeleted`; with `applied`, every
/// row must apply.
fn replay_rows(h: &History, applied: bool) -> Replayed {
    let mut r = Replayer::new(ReplayMode::KeepDeleted);
    let mut sets: HashMap<Option<Uuid>, (IdSet, IdSet)> = HashMap::new();
    for row in h.rows() {
        let e = r.apply(&row.update_bytes);
        if applied {
            assert_eq!(e.outcome, RowOutcome::Applied, "fixture: rev {:?}", row.rev);
        }
        let (i, d) = sets.entry(row.by_user_id).or_default();
        i.merge_with(e.inserted);
        d.merge_with(e.deleted);
    }
    Replayed { r, sets }
}

fn replay(h: &History) -> Replayed {
    replay_rows(h, true)
}

impl Replayed {
    fn h(&self) -> &Doc {
        self.r.doc()
    }

    fn sets(&self, c: usize) -> (IdSet, IdSet) {
        self.sets.get(&Some(author(c))).cloned().unwrap_or_default()
    }

    fn diff(&self, c: usize) -> EntryDiff {
        let (i, d) = self.sets(c);
        entry_diff(self.h(), &i, &d).expect("the entry diff")
    }

    /// Author `c`'s Show changes render.
    fn render(&self, c: usize) -> Value {
        pm_json_diff(self.h(), &self.diff(c), &Progress::default()).expect("the render")
    }
}

/// `f`'s result, failing instead of hanging when it does not return within
/// a minute (a transaction held across `diff_text` would block for good).
fn returns_in_time<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(60))
        .expect("the render returns")
}

/// Every node of `v` depth-first, `v` included.
fn nodes(v: &Value) -> Vec<&Value> {
    let mut out = vec![v];
    if let Some(content) = v["content"].as_array() {
        for child in content {
            out.extend(nodes(child));
        }
    }
    out
}

fn text_nodes(v: &Value) -> Vec<&Value> {
    nodes(v)
        .into_iter()
        .filter(|n| n["type"] == "text")
        .collect()
}

fn mark_types(t: &Value) -> Vec<&str> {
    t["marks"]
        .as_array()
        .map(|marks| marks.iter().filter_map(|m| m["type"].as_str()).collect())
        .unwrap_or_default()
}

fn marked(t: &Value, mark: &str) -> bool {
    mark_types(t).contains(&mark)
}

/// The text of every text node under `v`.
fn plain_text(v: &Value) -> String {
    text_nodes(v)
        .iter()
        .filter_map(|t| t["text"].as_str())
        .collect()
}

fn text(s: &str) -> Value {
    json!({"type": "text", "text": s})
}

fn inserted(s: &str) -> Value {
    json!({"type": "text", "text": s, "marks": [{"type": HISTORY_INSERT}]})
}

fn deleted(s: &str) -> Value {
    json!({"type": "text", "text": s, "marks": [{"type": HISTORY_DELETE}]})
}

fn attrs(pairs: Vec<(&str, Any)>) -> Attrs {
    pairs.into_iter().map(|(k, v)| (Arc::from(k), v)).collect()
}

fn empty_map() -> Any {
    Any::Map(Arc::new(HashMap::new()))
}

/// The first text of the `i`th block of `frag`.
fn text_of(frag: &XmlFragmentRef, txn: &impl ReadTxn, i: u32) -> XmlTextRef {
    match frag.get(txn, i) {
        Some(XmlOut::Element(p)) => match p.get(txn, 0) {
            Some(XmlOut::Text(t)) => t,
            other => panic!("block {i} holds no text: {other:?}"),
        },
        other => panic!("no block at {i}: {other:?}"),
    }
}

/// Word counts as the render shows them: `split_whitespace` over each
/// maximal run of consecutive text nodes carrying the mark, inside one
/// parent's content, broken by any other node.
fn render_counts(doc: &Value) -> WordCounts {
    let mut counts = WordCounts {
        added: 0,
        removed: 0,
    };
    for node in nodes(doc) {
        let Some(content) = node["content"].as_array() else {
            continue;
        };
        for (mark, slot) in [
            (HISTORY_INSERT, &mut counts.added),
            (HISTORY_DELETE, &mut counts.removed),
        ] {
            let mut run = String::new();
            for child in content {
                if child["type"] == "text" && marked(child, mark) {
                    run.push_str(child["text"].as_str().expect("text"));
                } else {
                    *slot += run.split_whitespace().count() as i32;
                    run.clear();
                }
            }
            *slot += run.split_whitespace().count() as i32;
        }
    }
    counts
}

/// Whether some element (or the root) of `doc`'s `default` fragment holds
/// two `XmlText`s next to each other.
fn has_adjacent_texts(doc: &Doc) -> bool {
    fn walk<T: ReadTxn>(txn: &T, children: Vec<XmlOut>) -> bool {
        let adjacent = children
            .windows(2)
            .any(|w| matches!(w, [XmlOut::Text(_), XmlOut::Text(_)]));
        adjacent
            || children.into_iter().any(|c| match c {
                XmlOut::Element(e) => walk(txn, e.children(txn).collect()),
                _ => false,
            })
    }
    let txn = doc.transact();
    let Some(root) = txn.get_xml_fragment("default") else {
        return false;
    };
    walk(&txn, root.children(&txn).collect())
}

// ---------------------------------------------------------------------------
// Highlighting
// ---------------------------------------------------------------------------

#[test]
fn overlapping_sessions_highlight_only_the_authors_own_changes() {
    let rp = replay(&History::overlap_case());
    for (who, own) in [(A, "a "), (B, "b ")] {
        let doc = rp.render(who);
        let blocks = doc["content"].as_array().expect("content");
        assert_eq!(blocks.len(), 6, "{doc}");
        for p in blocks {
            let s = plain_text(p);
            let content = json!([if s.starts_with(own) {
                inserted(&s)
            } else {
                text(&s)
            }]);
            let expected = if s.starts_with(own) {
                json!({"type": "paragraph", "attrs": {HISTORY_CHANGE: "added"}, "content": content})
            } else {
                json!({"type": "paragraph", "content": content})
            };
            assert_eq!(p, &expected, "author {who}: {doc}");
        }
        let mine = text_nodes(&doc)
            .iter()
            .filter(|t| marked(t, HISTORY_INSERT))
            .count();
        assert_eq!(mine, if who == A { 4 } else { 2 }, "{doc}");
    }
}

#[test]
fn an_added_paragraph_is_marked_added() {
    let mut h = History::new(1);
    h.type_para(B, "base", at(0));
    h.pull(A);
    h.type_para(A, "new text", at(5));
    assert_eq!(
        replay(&h).render(A),
        json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [text("base")]},
            {"type": "paragraph", "attrs": {HISTORY_CHANGE: "added"}, "content": [inserted("new text")]},
        ]})
    );
}

#[test]
fn a_removed_paragraph_is_marked_removed_and_its_text_struck() {
    let mut h = History::new(1);
    h.type_para(B, "keep", at(0));
    h.type_para(B, "old para", at(1));
    h.pull(A);
    let bytes = h.clients[A].txn(|txn, frag| frag.remove_range(txn, 1, 1));
    h.edit_at(A, bytes, at(5));
    let rp = replay(&h);
    assert!(
        !plain_text(&pm_json(rp.h())).contains("old para"),
        "precondition: the paragraph is gone at the entry's end"
    );
    assert_eq!(
        rp.render(A),
        json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [text("keep")]},
            {"type": "paragraph", "attrs": {HISTORY_CHANGE: "removed"}, "content": [deleted("old para")]},
        ]})
    );
}

#[test]
fn a_removed_heading_keeps_its_attributes() {
    let doc = returns_in_time(|| {
        let mut h = History::new(1);
        let bytes = h.clients[B].txn(|txn, frag| {
            let heading = frag.push_back(txn, XmlElementPrelim::empty("heading"));
            heading.insert_attribute(txn, "level", Any::Number(2.0));
            heading.push_back(txn, XmlTextPrelim::new("Title"));
            let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(txn, XmlTextPrelim::new("body"));
        });
        h.edit_at(B, bytes, at(0));
        h.pull(A);
        let bytes = h.clients[A].txn(|txn, frag| frag.remove_range(txn, 0, 1));
        h.edit_at(A, bytes, at(5));
        replay(&h).render(A)
    });
    assert_eq!(
        doc,
        json!({"type": "doc", "content": [
            {"type": "heading", "attrs": {"level": 2, HISTORY_CHANGE: "removed"}, "content": [deleted("Title")]},
            {"type": "paragraph", "content": [text("body")]},
        ]})
    );
}

#[test]
fn changes_inside_code_are_marked() {
    let doc = replay(&History::code_case()).render(A);
    assert_eq!(
        doc,
        json!({"type": "doc", "content": [
            {"type": "code_block", "attrs": {"language": "rust"}, "content": [
                text("let x = 1;\n"),
                inserted("let y = 2;"),
            ]},
            {"type": "paragraph", "content": [
                text("see "),
                {"type": "text", "text": "y", "marks": [
                    {"type": "code", "attrs": {}},
                    {"type": HISTORY_INSERT},
                ]},
            ]},
        ]})
    );
}

/// C writes a task list item (`checked` false) holding "task"; A and B
/// pull it.
fn task_history() -> History {
    let mut h = History::new(1);
    let bytes = h.clients[C].txn(|txn, frag| {
        let list = frag.push_back(txn, XmlElementPrelim::empty("bullet_list"));
        let item = list.push_back(txn, XmlElementPrelim::empty("list_item"));
        item.insert_attribute(txn, "checked", Any::Bool(false));
        let p = item.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("task"));
    });
    h.edit_at(C, bytes, at(0));
    h.pull(A);
    h.pull(B);
    h
}

/// Client `c` sets the task's `checked`.
fn check_task(h: &mut History, c: usize, checked: bool, minutes: i64) {
    let bytes = h.clients[c].txn(|txn, frag| {
        let Some(XmlOut::Element(list)) = frag.get(txn, 0) else {
            panic!("the list")
        };
        let Some(XmlOut::Element(item)) = list.get(txn, 0) else {
            panic!("the item")
        };
        item.insert_attribute(txn, "checked", Any::Bool(checked));
    });
    h.edit_at(c, bytes, at(minutes));
}

fn list_item(doc: &Value) -> &Value {
    nodes(doc)
        .into_iter()
        .find(|n| n["type"] == "list_item")
        .expect("a list item")
}

#[test]
fn a_checked_task_is_marked_changed() {
    let mut h = task_history();
    check_task(&mut h, A, true, 5);
    let doc = replay(&h).render(A);
    assert_eq!(
        list_item(&doc)["attrs"],
        json!({"checked": true, HISTORY_CHANGE: "changed"}),
        "{doc}"
    );
    let others: Vec<&Value> = nodes(&doc)
        .into_iter()
        .filter(|n| n["type"] != "list_item" && !n["attrs"][HISTORY_CHANGE].is_null())
        .collect();
    assert!(others.is_empty(), "only the item changed: {doc}");
    assert!(
        text_nodes(&doc).iter().all(|t| t["marks"].is_null()),
        "{doc}"
    );
}

#[test]
fn another_authors_attribute_change_is_not_marked() {
    let mut h = task_history();
    h.type_para(A, "mine", at(5));
    check_task(&mut h, B, true, 6);
    h.type_para(A, "more", at(7));
    let rp = replay(&h);
    let doc = rp.render(A);
    assert_eq!(list_item(&doc)["attrs"], json!({"checked": true}), "{doc}");
    // Not vacuous: B's own render marks it.
    let theirs = rp.render(B);
    assert_eq!(
        list_item(&theirs)["attrs"],
        json!({"checked": true, HISTORY_CHANGE: "changed"}),
        "{theirs}"
    );
}

/// Attributes of an element alive at the entry's end are read from the
/// replay, not from the union doc, where an attribute the entry removed
/// still shows (its item is one the entry deleted).
#[test]
fn an_attribute_the_entry_removed_is_absent_and_marked_changed() {
    let mut h = task_history();
    let bytes = h.clients[A].txn(|txn, frag| {
        let Some(XmlOut::Element(list)) = frag.get(txn, 0) else {
            panic!("the list")
        };
        let Some(XmlOut::Element(item)) = list.get(txn, 0) else {
            panic!("the item")
        };
        item.remove_attribute(txn, &"checked");
    });
    h.edit_at(A, bytes, at(5));
    let doc = replay(&h).render(A);
    assert_eq!(
        list_item(&doc)["attrs"],
        json!({HISTORY_CHANGE: "changed"}),
        "{doc}"
    );
}

#[test]
fn formatting_renders_as_at_the_entrys_end() {
    let mut h = History::new(1);
    h.type_para(B, "make this bold", at(0));
    h.pull(A);
    let bytes = h.clients[A].txn(|txn, frag| {
        let t = text_of(frag, txn, 0);
        assert_eq!(t.get_string(txn), "make this bold");
        t.format(txn, 5, 4, attrs(vec![("bold", Any::Bool(true))]));
    });
    h.edit_at(A, bytes, at(5));
    assert_eq!(
        replay(&h).render(A),
        json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [
                text("make "),
                {"type": "text", "text": "this", "marks": [{"type": "bold", "attrs": true}]},
                text(" bold"),
            ]},
        ]})
    );
}

// ---------------------------------------------------------------------------
// pm_json's rules hold for the render
// ---------------------------------------------------------------------------

#[test]
fn an_entry_that_changed_nothing_renders_like_pm_json() {
    let rp = replay(&History::overlap_case());
    let h = rp.h();
    let d = entry_diff(h, &IdSet::new(), &IdSet::new()).expect("the diff");
    assert_eq!(
        pm_json_diff(h, &d, &Progress::default()).expect("the render"),
        pm_json(h)
    );
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    update: String,
}

#[test]
fn an_unchanged_diff_render_is_pm_json_on_every_fixture_case() {
    let mut cases = 0;
    let mut failures = Vec::new();
    for file in ["pm_json_golden.json", "pm_json_corpus.json"] {
        let path = format!("{FIXTURES}/{file}");
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let file_cases: Vec<Case> =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"));
        for case in file_cases {
            cases += 1;
            let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<bool, String> {
                let update = STANDARD.decode(&case.update).map_err(|e| e.to_string())?;
                let r = Replayer::from_state(ReplayMode::KeepDeleted, &update)
                    .map_err(|e| format!("replay: {e:?}"))?;
                let h = r.doc();
                let d = entry_diff(h, &IdSet::new(), &IdSet::new())
                    .map_err(|e| format!("diff: {e}"))?;
                let render = pm_json_diff(h, &d, &Progress::default())
                    .map_err(|e| format!("render: {e}"))?;
                Ok(render == pm_json(h))
            }));
            match outcome {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => failures.push(format!("{file}: {}: differs", case.name)),
                Ok(Err(e)) => failures.push(format!("{file}: {}: {e}", case.name)),
                Err(_) => failures.push(format!("{file}: {}: panicked", case.name)),
            }
        }
    }
    assert!(cases > 1000, "precondition: both fixtures read ({cases})");
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn an_added_paragraph_with_unknown_content_keeps_pm_jsons_rules() {
    let mut h = History::new(1);
    h.type_para(B, "base", at(0));
    h.pull(A);
    let bytes = h.clients[A].txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        let first = p.push_back(txn, XmlTextPrelim::new(""));
        first.insert_with_attributes(txn, 0, "new ", attrs(vec![("bold--AbCd1234", empty_map())]));
        p.push_back(txn, XmlElementPrelim::empty("mention_chip"));
        let last = p.push_back(txn, XmlTextPrelim::new(""));
        let comment = Any::Map(Arc::new(HashMap::from([(
            "id".to_string(),
            Any::String("c1".into()),
        )])));
        last.insert_with_attributes(
            txn,
            0,
            "x",
            attrs(vec![("comment", comment), ("italic", Any::Bool(true))]),
        );
        let callout = frag.push_back(txn, XmlElementPrelim::empty("callout"));
        let inner = callout.push_back(txn, XmlElementPrelim::empty("paragraph"));
        inner.push_back(txn, XmlTextPrelim::new("in"));
        let heading = frag.push_back(txn, XmlElementPrelim::empty("heading"));
        heading.insert_attribute(txn, "level", Any::Number(2.0));
        heading.push_back(txn, XmlTextPrelim::new("Head"));
    });
    h.edit_at(A, bytes, at(5));
    let doc = replay(&h).render(A);
    assert_eq!(
        doc,
        json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [text("base")]},
            {"type": "paragraph", "attrs": {HISTORY_CHANGE: "added"}, "content": [
                {"type": "text", "text": "new ", "marks": [
                    {"type": "bold", "attrs": {}},
                    {"type": HISTORY_INSERT},
                ]},
                inserted("Unsupported content (mention_chip)"),
                {"type": "text", "text": "x", "marks": [
                    {"type": HISTORY_INSERT},
                    {"type": "italic", "attrs": true},
                ]},
            ]},
            {"type": "paragraph", "attrs": {HISTORY_CHANGE: "added"}, "content": [
                text("Unsupported content (callout)"),
            ]},
            {"type": "heading", "attrs": {"level": 2, HISTORY_CHANGE: "added"}, "content": [
                inserted("Head"),
            ]},
        ]})
    );
    for t in text_nodes(&doc) {
        for m in mark_types(t) {
            assert!(!m.contains("--") && m != "comment", "{doc}");
        }
    }
    assert!(doc["content"][3]["attrs"]["level"].is_i64(), "{doc}");
}

// ---------------------------------------------------------------------------
// Word counts and the render
// ---------------------------------------------------------------------------

/// A panic of `f`, without the default hook's report.
fn quietly<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = catch_unwind(AssertUnwindSafe(f));
    std::panic::set_hook(hook);
    outcome
}

#[test]
fn words_match_the_highlighted_runs() {
    let mut compared = 0;
    for seed in 1..=100 {
        let h = History::generate(seed, 60);
        let rp = replay_rows(&h, false);
        for c in [A, B, C] {
            let d = rp.diff(c);
            assert!(
                !has_adjacent_texts(&d.union),
                "seed {seed}: the generator wrote adjacent texts; this test holds for the editor shape only"
            );
            let render = quietly(|| pm_json_diff(rp.h(), &d, &Progress::default()));
            let Ok(stored) = quietly(|| count_words(&text_chunks(&d))) else {
                // M1.3's diff cannot split a union doc by the delete set of
                // a replay that holds blocks past a clock gap (yrs 0.27.3's
                // `find_index` indexes past the list in
                // `split_by_snapshot`); the render fails with it, and the
                // view then shows the plain version.
                assert!(
                    common::has_gaps(rp.h()),
                    "seed {seed}, author {c}: the diff panicked on a replay without gaps"
                );
                assert!(
                    render.is_err(),
                    "seed {seed}, author {c}: the render outlived a diff that panics"
                );
                continue;
            };
            let doc = render
                .unwrap_or_else(|_| panic!("seed {seed}, author {c}: the render panicked"))
                .unwrap_or_else(|e| panic!("seed {seed}, author {c}: {e}"));
            assert_eq!(
                render_counts(&doc),
                stored,
                "seed {seed}, author {c}: {doc}"
            );
            compared += 1;
        }
    }
    assert!(
        compared >= 200,
        "precondition: most cases compared ({compared})"
    );
}

#[test]
fn adjacent_added_texts_count_per_text() {
    let mut h = History::new(1);
    let bytes = h.clients[A].txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("ab"));
        p.push_back(txn, XmlTextPrelim::new("cd"));
    });
    h.edit_at(A, bytes, at(0));
    let rp = replay(&h);
    let d = rp.diff(A);
    assert_eq!(
        count_words(&text_chunks(&d)),
        WordCounts {
            added: 2,
            removed: 0
        }
    );
    let doc = pm_json_diff(rp.h(), &d, &Progress::default()).expect("the render");
    assert_eq!(
        doc["content"][0]["content"],
        json!([inserted("ab"), inserted("cd")]),
        "{doc}"
    );
}

#[test]
fn elements_inserted_and_deleted_or_deleted_by_others_are_not_rendered() {
    let mut h = History::new(1);
    h.type_para(C, "theirs", at(0));
    h.pull(A);
    h.pull(B);
    h.type_para(A, "tmp", at(5));
    let bytes = h.clients[A].txn(|txn, frag| {
        assert_eq!(text_of(frag, txn, 1).get_string(txn), "tmp");
        frag.remove_range(txn, 1, 1);
    });
    h.edit_at(A, bytes, at(6));
    let bytes = h.clients[B].txn(|txn, frag| {
        assert_eq!(text_of(frag, txn, 0).get_string(txn), "theirs");
        frag.remove_range(txn, 0, 1);
    });
    h.edit_at(B, bytes, at(7));
    h.type_para(A, "mine", at(8));
    let rp = replay(&h);

    assert_eq!(
        rp.render(A),
        json!({"type": "doc", "content": [
            {"type": "paragraph", "attrs": {HISTORY_CHANGE: "added"}, "content": [inserted("mine")]},
        ]})
    );
    assert_eq!(
        rp.render(B),
        json!({"type": "doc", "content": [
            {"type": "paragraph", "attrs": {HISTORY_CHANGE: "removed"}, "content": [deleted("theirs")]},
            {"type": "paragraph", "content": [text("mine")]},
        ]})
    );
}

#[test]
fn a_client_with_clock_one_or_two_renders() {
    // B typed one character (clock 1): yrs 0.27.3's `find_index` divides
    // by zero there unless the union doc pads B.
    let mut h = History::new(1);
    h.type_para(A, "hello world", at(0));
    h.pull(B);
    let bytes = h.clients[B].txn(|txn, frag| text_of(frag, txn, 0).insert(txn, 5, "!"));
    h.edit_at(B, bytes, at(5));
    let rp = replay(&h);
    let doc = catch_unwind(AssertUnwindSafe(|| rp.render(B))).expect("no panic");
    assert_eq!(
        doc,
        json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [text("hello"), inserted("!"), text(" world")]},
        ]})
    );

    // B only made an empty paragraph (clock 2): it indexes past the list
    // unless padded; A's words still render, and the pad never shows.
    let mut h = History::new(1);
    h.type_para(A, "hello world", at(0));
    h.pull(B);
    let bytes = h.clients[B].txn(|txn, frag| {
        let n = frag.len(txn);
        let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(""));
    });
    h.edit_at(B, bytes, at(5));
    let rp = replay(&h);
    let doc = catch_unwind(AssertUnwindSafe(|| rp.render(A))).expect("no panic");
    assert_eq!(
        doc,
        json!({"type": "doc", "content": [
            {"type": "paragraph", "attrs": {HISTORY_CHANGE: "added"}, "content": [inserted("hello world")]},
            {"type": "paragraph", "content": []},
        ]})
    );
}

#[test]
fn a_cancelled_diff_stops() {
    let rp = replay(&History::overlap_case());
    let d = rp.diff(A);
    let progress = Progress::default();
    progress.cancel();
    assert!(matches!(
        pm_json_diff(rp.h(), &d, &progress),
        Err(DiffError::Cancelled)
    ));
}

// ---------------------------------------------------------------------------
// The spike: `D_lo` on a skip-GC doc seeded from a GC'd checkpoint
// ---------------------------------------------------------------------------

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

fn push_paragraph(frag: &XmlFragmentRef, txn: &mut TransactionMut, text: &str) {
    let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
    p.push_back(txn, XmlTextPrelim::new(text));
}

/// A checkpoint as a `ReplayMode::Gc` replay encodes it (the one
/// `tests/words.rs` starts from): P1 "alpha beta" and P2 "old words here",
/// after a word typed into P1 and deleted again (an `ItemContent::Deleted`
/// item) and a paragraph added and removed again (GC blocks).
fn gc_checkpoint() -> Vec<u8> {
    let seed = Doc::with_client_id(1);
    let frag = seed.get_or_insert_xml_fragment("default");
    let mut gc = Replayer::new(ReplayMode::Gc);
    let rows = [
        txn_update(&seed, |txn| {
            push_paragraph(&frag, txn, "alpha beta");
            push_paragraph(&frag, txn, "old words here");
        }),
        txn_update(&seed, |txn| text_of(&frag, txn, 0).insert(txn, 10, " junk")),
        txn_update(&seed, |txn| text_of(&frag, txn, 0).remove_range(txn, 10, 5)),
        txn_update(&seed, |txn| push_paragraph(&frag, txn, "gone soon")),
        txn_update(&seed, |txn| frag.remove_range(txn, 2, 1)),
    ];
    for row in &rows {
        assert_eq!(gc.apply(row).outcome, RowOutcome::Applied, "fixture");
    }
    let state = gc.encode_state();
    let update = Update::decode_v1(&state).expect("a state");
    assert!(
        !update
            .insertions(true)
            .diff(&update.insertions(false))
            .is_empty(),
        "precondition: the checkpoint holds GC blocks"
    );
    state
}

#[test]
fn lo_doc_builds_on_a_doc_seeded_from_a_gc_checkpoint() {
    let checkpoint = gc_checkpoint();
    let mut h = Replayer::from_state(ReplayMode::KeepDeleted, &checkpoint).expect("H");
    let mut b_only = Replayer::from_state(ReplayMode::KeepDeleted, &checkpoint).expect("B only");
    let docs: Vec<Doc> = [10, 20]
        .into_iter()
        .map(|id| {
            let doc = Doc::with_client_id(id);
            doc.get_or_insert_xml_fragment("default");
            doc.transact_mut()
                .apply_update(Update::decode_v1(&checkpoint).expect("a state"))
                .expect("the checkpoint applies");
            doc
        })
        .collect();
    let (mut ins_a, mut del_a) = (IdSet::new(), IdSet::new());
    type Edit = Box<dyn FnOnce(&XmlFragmentRef, &mut TransactionMut)>;
    let edits: Vec<(usize, Edit)> = vec![
        (
            A,
            Box::new(|f, txn| {
                let t = text_of(f, txn, 0);
                let len = t.len(txn);
                t.insert(txn, len, " gamma");
            }),
        ),
        (
            B,
            Box::new(|f, txn| {
                let t = text_of(f, txn, 1);
                let len = t.len(txn);
                t.insert(txn, len, " delta");
            }),
        ),
        (
            A,
            Box::new(|f, txn| {
                let t = text_of(f, txn, 0);
                assert_eq!(&t.get_string(txn)[6..10], "beta");
                t.remove_range(txn, 6, 4);
            }),
        ),
        (
            B,
            Box::new(|f, txn| {
                let t = text_of(f, txn, 1);
                assert_eq!(&t.get_string(txn)[..4], "old ");
                t.remove_range(txn, 0, 4);
            }),
        ),
        (A, Box::new(|f, txn| push_paragraph(f, txn, "a new"))),
    ];
    for (who, edit) in edits {
        let doc = &docs[who];
        let frag = doc.get_or_insert_xml_fragment("default");
        let row = txn_update(doc, |txn| edit(&frag, txn));
        docs[1 - who]
            .transact_mut()
            .apply_update(Update::decode_v1(&row).expect("a row"))
            .expect("the row applies");
        let e = h.apply(&row);
        assert_eq!(e.outcome, RowOutcome::Applied, "fixture: the row applies");
        if who == A {
            ins_a.merge_with(e.inserted);
            del_a.merge_with(e.deleted);
        } else {
            assert_eq!(b_only.apply(&row).outcome, RowOutcome::Applied, "B alone");
        }
    }

    let d = entry_diff(h.doc(), &ins_a, &del_a).expect("the union doc");
    let lo = lo_doc(h.doc(), &d).expect("encode_state_from_snapshot(S_lo) on a mixed doc");
    assert!(lo.skip_gc(), "D_lo keeps deleted content");
    assert_ne!(
        visible_content(&lo),
        visible_content(h.doc()),
        "precondition: A changed the page"
    );
    assert_eq!(visible_content(&lo), visible_content(b_only.doc()));
}
