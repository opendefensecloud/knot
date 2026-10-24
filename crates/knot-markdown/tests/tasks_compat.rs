//! The `/tasks` listing read from the page itself (spec A11, A12; planning
//! decision P1), checked against the toggles (`patch_task_checked`) and
//! against the legacy listing (the legacy export, then `extract_tasks`):
//! - listing index `k` and toggle index `k` are the same item, on every page;
//! - on a page where the legacy listing agrees with the room, every task is
//!   listed as legacy listed it, unless its item falls in a named
//!   `task_class`, or text that reads as Markdown syntax (the writer's
//!   escaping follow-up) makes the difference.

mod common;

use chrono::{DateTime, Utc};
use common::corpus::{self, Case, Shape};
use common::{N, Run, build, el, ela, hard_break, p, read, run, text};
use knot_crdt::tasks::{patch_task_checked, task_items};
use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::legacy::{self, compat};
use knot_markdown::tasks::{Task, extract_tasks_from_doc};
use uuid::Uuid;
use yrs::types::text::YChange;
use yrs::{Any, ReadTxn, Text, Transact, Transaction, Xml, XmlElementRef, XmlFragment, XmlOut};

/// Seeds of the random corpus, and documents per seed: the corpus of
/// `tests/compat.rs`.
const RANDOM_SEEDS: std::ops::RangeInclusive<u64> = 1..=50;
const RANDOM_DOCS_PER_SEED: usize = 200;

/// How many failures a test prints before the total.
const SHOWN_FAILURES: usize = 10;

const TIME_URL: &str = "knot://time/2026-06-04T17:00:00Z";

/// The generated and random documents, and the legacy import of every
/// fixture and probe.
fn corpus_docs() -> Vec<Case> {
    let mut cases = corpus::generated();
    for seed in RANDOM_SEEDS {
        cases.extend(corpus::random(seed, RANDOM_DOCS_PER_SEED));
    }
    for (name, md) in corpus::markdown_inputs() {
        let (doc, _) = legacy::parse(&md).expect("legacy parse");
        cases.push(Case {
            name: format!("legacy import of {name}"),
            doc,
        });
    }
    cases
}

fn assert_no_failures(what: &str, failures: &[String]) {
    if failures.is_empty() {
        return;
    }
    let shown: Vec<&str> = failures
        .iter()
        .take(SHOWN_FAILURES)
        .map(String::as_str)
        .collect();
    panic!(
        "{what}: {} case(s), first {}:\n\n{}",
        failures.len(),
        shown.len(),
        shown.join("\n\n")
    );
}

fn assert_at_least(what: &str, got: usize, min: usize) {
    assert!(
        got >= min,
        "only {got} {what} compared, expected at least {min}"
    );
}

/// A listed task, from the new or the legacy extractor.
#[derive(Clone, Debug, PartialEq)]
struct Listed {
    index: i32,
    text: String,
    assignee: Option<Uuid>,
    checked: bool,
    due: Option<DateTime<Utc>>,
}

impl From<Task> for Listed {
    fn from(t: Task) -> Self {
        Listed {
            index: t.item_index,
            text: t.text,
            assignee: t.assignee_user_id,
            checked: t.checked,
            due: t.due_at,
        }
    }
}

impl From<legacy::Task> for Listed {
    fn from(t: legacy::Task) -> Self {
        Listed {
            index: t.item_index,
            text: t.text,
            assignee: t.assignee_user_id,
            checked: t.checked,
            due: t.due_at,
        }
    }
}

/// The new listing of `doc`.
fn listing(doc: &DocHandle) -> Vec<Listed> {
    extract_tasks_from_doc(doc)
        .into_iter()
        .map(Listed::from)
        .collect()
}

/// The legacy listing of `doc`, when the legacy export of `doc` succeeds.
fn legacy_listing(doc: &DocHandle) -> Option<Vec<Listed>> {
    let md = legacy::serialise(&YrsEngine, doc).ok()?;
    Some(
        legacy::extract_tasks(&md)
            .into_iter()
            .map(Listed::from)
            .collect(),
    )
}

/// How many task items the room counts in `doc`.
fn item_count(doc: &DocHandle) -> usize {
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    txn.get_xml_fragment("default")
        .map_or(0, |frag| task_items(&frag, &txn).len())
}

/// A copy of `doc` with task item `k` flipped by `patch_task_checked`, the
/// room's toggle. The copy goes through an update, because `yrs::Doc::clone`
/// shares its store.
fn flipped(doc: &DocHandle, k: usize) -> DocHandle {
    let engine = YrsEngine;
    let update = engine.encode_state_as_update(doc, None).expect("encode");
    let copy = engine.new_doc();
    engine.apply_update(&copy, &update).expect("apply");
    {
        let ydoc = copy.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        let checked = task_items(&frag, &txn)[k]
            .get_attribute(&txn, "checked")
            .is_some_and(|v| v.to_string(&txn) == "true");
        patch_task_checked(&frag, &mut txn, k as i32, !checked).expect("patch");
    }
    copy
}

/// `tasks` with task `k`'s `checked` flipped.
fn with_flipped(tasks: &[Listed], k: usize) -> Vec<Listed> {
    let mut want = tasks.to_vec();
    want[k].checked = !want[k].checked;
    want
}

/// The legacy listing of `doc` when it agrees with the room: it has as many
/// tasks as the room has task items, and flipping item `k` with the room's
/// toggle changes legacy task `k`'s `checked` and nothing else.
fn agreeing_legacy_listing(doc: &DocHandle) -> Option<Vec<Listed>> {
    let tasks = legacy_listing(doc)?;
    if tasks.len() != item_count(doc) {
        return None;
    }
    for k in 0..tasks.len() {
        if legacy_listing(&flipped(doc, k))? != with_flipped(&tasks, k) {
            return None;
        }
    }
    Some(tasks)
}

/// `(item_index, text)` of each task listed for `doc`.
fn texts(doc: &DocHandle) -> Vec<(i32, String)> {
    listing(doc)
        .into_iter()
        .map(|t| (t.index, t.text))
        .collect()
}

fn owned(pairs: &[(i32, &str)]) -> Vec<(i32, String)> {
    pairs.iter().map(|(i, s)| (*i, s.to_string())).collect()
}

fn task(shape: Shape, checked: bool, children: Vec<N>) -> N {
    ela(
        "list_item",
        vec![("checked", shape.boolean(checked))],
        children,
    )
}

fn bullets(items: Vec<N>) -> N {
    el("bullet_list", items)
}

#[test]
fn nested_checklist_lists_parent_and_child_separately() {
    for shape in Shape::ALL {
        let doc = build(&[bullets(vec![
            task(
                shape,
                false,
                vec![
                    p("parent"),
                    bullets(vec![task(shape, false, vec![p("child")])]),
                ],
            ),
            task(shape, false, vec![p("next")]),
        ])]);
        assert_eq!(
            texts(&doc),
            owned(&[(0, "parent"), (1, "child"), (2, "next")]),
            "{}",
            shape.name()
        );
    }
}

#[test]
fn table_cell_checklists_are_listed() {
    for shape in Shape::ALL {
        let cell = |kids| el("table_cell", kids);
        let doc = build(&[
            el(
                "table",
                vec![
                    el("table_row", vec![el("table_header", vec![p("h")])]),
                    el(
                        "table_row",
                        vec![cell(vec![bullets(vec![
                            task(shape, true, vec![p("in a cell")]),
                            task(shape, false, vec![p("also in it")]),
                        ])])],
                    ),
                ],
            ),
            bullets(vec![task(shape, false, vec![p("after")])]),
        ]);
        let got = listing(&doc);
        assert_eq!(
            got.iter()
                .map(|t| (t.index, t.text.as_str(), t.checked))
                .collect::<Vec<_>>(),
            vec![
                (0, "in a cell", true),
                (1, "also in it", false),
                (2, "after", false)
            ],
            "{}",
            shape.name()
        );
    }
}

#[test]
fn ordered_list_checkboxes_are_listed() {
    for shape in Shape::ALL {
        let doc = build(&[ela(
            "ordered_list",
            vec![("start", shape.int(1))],
            vec![
                task(shape, true, vec![p("first")]),
                task(shape, false, vec![p("second")]),
            ],
        )]);
        let got = listing(&doc);
        assert_eq!(
            got.iter()
                .map(|t| (t.index, t.text.as_str(), t.checked))
                .collect::<Vec<_>>(),
            vec![(0, "first", true), (1, "second", false)],
            "{}",
            shape.name()
        );
    }
}

#[test]
fn plain_bullet_starting_with_brackets_is_not_a_task() {
    for shape in Shape::ALL {
        let doc = build(&[bullets(vec![
            el("list_item", vec![p("[ ] not a task")]),
            task(shape, false, vec![p("a task")]),
        ])]);
        assert_eq!(texts(&doc), owned(&[(0, "a task")]), "{}", shape.name());
    }
}

#[test]
fn due_date_in_second_paragraph_is_kept() {
    for shape in Shape::ALL {
        let doc = build(&[bullets(vec![task(
            shape,
            false,
            vec![
                p("do X"),
                el(
                    "paragraph",
                    vec![N::Text(vec![
                        run("by ", Vec::new()),
                        run("Jun 4", vec![("link", shape.link(TIME_URL, None))]),
                    ])],
                ),
            ],
        )])]);
        let legacy_due = legacy_listing(&doc).expect("legacy export")[0].due;
        assert_eq!(
            legacy_due.map(|d| d.to_rfc3339()).as_deref(),
            Some("2026-06-04T17:00:00+00:00"),
            "{}",
            shape.name()
        );
        assert_eq!(listing(&doc)[0].due, legacy_due, "{}", shape.name());
    }
}

#[test]
fn empty_task_item_is_listed_with_empty_text() {
    for shape in Shape::ALL {
        let doc = build(&[bullets(vec![
            task(shape, false, Vec::new()),
            task(shape, false, vec![el("paragraph", Vec::new())]),
            task(shape, false, vec![p("x")]),
        ])]);
        assert_eq!(
            texts(&doc),
            owned(&[(0, ""), (1, ""), (2, "x")]),
            "{}",
            shape.name()
        );
    }
}

/// Spec "Tasks": the required lockstep property, over the whole corpus.
#[test]
fn listing_and_toggles_hit_the_same_item() {
    let mut toggles = 0;
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let before = listing(&case.doc);
        let count = item_count(&case.doc);
        if before.len() != count {
            failures.push(format!(
                "{}: {} task(s) listed, the room counts {count}\n  listed: {before:?}",
                case.name,
                before.len()
            ));
            continue;
        }
        for k in 0..count {
            toggles += 1;
            let after = listing(&flipped(&case.doc, k));
            let want = with_flipped(&before, k);
            if after != want {
                failures.push(format!(
                    "{}: toggle {k}\n  want: {want:?}\n  got:  {after:?}",
                    case.name
                ));
                break;
            }
        }
    }
    assert_no_failures("a toggle doesn't flip exactly the listed task", &failures);
    assert_at_least("toggles", toggles, MIN_TOGGLES);
}

/// How the new listing lists a task of a page where the legacy listing
/// agrees with the room.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Verdict {
    /// As legacy listed it.
    Same,
    /// Its item falls in this `task_class`, and the listing differs from
    /// legacy's no further than the class allows.
    Exempt(&'static str),
    /// Otherwise.
    Differs,
}

/// The legacy and the new listing of `doc`, with a verdict per task, when
/// the legacy listing agrees with the room.
fn verdicts(doc: &DocHandle) -> Option<Vec<(Listed, Listed, Verdict)>> {
    let old = agreeing_legacy_listing(doc)?;
    let new = listing(doc);
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    let items = txn
        .get_xml_fragment("default")
        .map(|frag| task_items(&frag, &txn))
        .unwrap_or_default();
    assert_eq!(new.len(), items.len(), "the lockstep property");
    Some(
        old.into_iter()
            .zip(new)
            .zip(&items)
            .map(|((old, new), item)| {
                let verdict = match compat::task_class(&txn, item) {
                    None if old == new => Verdict::Same,
                    Some(class) if within_class(&txn, item, &old, &new) => Verdict::Exempt(class),
                    _ => Verdict::Differs,
                };
                (old, new, verdict)
            })
            .collect(),
    )
}

/// Whether `new` differs from `old`, both a listing of the classed task
/// item `item`, no further than P1 allows: in its text, and in its assignee
/// and due date where legacy's came from content that isn't the item's own
/// (it holds no such link), or where legacy found none (it read the link as
/// text or code).
fn within_class(txn: &Transaction, item: &XmlElementRef, old: &Listed, new: &Listed) -> bool {
    let own = own_links(txn, item);
    let own_due = |due: &DateTime<Utc>| {
        own.iter().any(|href| {
            href.strip_prefix(TIME_PREFIX)
                .and_then(|ts| DateTime::parse_from_rfc3339(ts.trim_end_matches('/')).ok())
                .is_some_and(|ts| ts == *due)
        })
    };
    old.index == new.index
        && old.checked == new.checked
        && (old.assignee == new.assignee
            || !old
                .assignee
                .is_some_and(|user| own.contains(&format!("{USER_PREFIX}{user}"))))
        && (old.due == new.due || !old.due.as_ref().is_some_and(own_due))
}

const USER_PREFIX: &str = "knot://user/";
const TIME_PREFIX: &str = "knot://time/";

/// The link targets in the item's own content: its blocks other than those
/// that are, or hold, a list item (P1).
fn own_links(txn: &Transaction, item: &XmlElementRef) -> Vec<String> {
    fn holds_list_item(txn: &Transaction, node: &XmlOut) -> bool {
        match node {
            XmlOut::Element(el) => {
                el.tag().as_ref() == "list_item"
                    || (0..el.len(txn))
                        .filter_map(|i| el.get(txn, i))
                        .any(|child| holds_list_item(txn, &child))
            }
            XmlOut::Fragment(f) => (0..f.len(txn))
                .filter_map(|i| f.get(txn, i))
                .any(|child| holds_list_item(txn, &child)),
            XmlOut::Text(_) => false,
        }
    }
    fn links(txn: &Transaction, node: &XmlOut, out: &mut Vec<String>) {
        match node {
            XmlOut::Element(el) => {
                for child in (0..el.len(txn)).filter_map(|i| el.get(txn, i)) {
                    links(txn, &child, out);
                }
            }
            XmlOut::Text(t) => {
                for chunk in t.diff(txn, YChange::identity) {
                    if let Some(Any::Map(link)) =
                        chunk.attributes.as_ref().and_then(|a| a.get("link"))
                        && let Some(Any::String(href)) = link.get("href")
                    {
                        out.push(href.to_string());
                    }
                }
            }
            XmlOut::Fragment(_) => {}
        }
    }
    let mut out = Vec::new();
    for child in (0..item.len(txn)).filter_map(|i| item.get(txn, i)) {
        if !holds_list_item(txn, &child) {
            links(txn, &child, &mut out);
        }
    }
    out
}

/// `blocks` with every ASCII punctuation character, space and tab of their
/// text made an `x`, as `markdown_escaping` in `tests/compat.rs` repairs a
/// document: text that reads as Markdown syntax is the writer's escaping
/// follow-up.
fn without_markdown_syntax(blocks: &[N]) -> Vec<N> {
    blocks
        .iter()
        .map(|node| match node {
            N::El(tag, attrs, children) => {
                N::El(tag, attrs.clone(), without_markdown_syntax(children))
            }
            N::Text(runs) => N::Text(
                runs.iter()
                    .map(|r| Run {
                        text: r
                            .text
                            .chars()
                            .map(|c| {
                                if c.is_ascii_punctuation() || c == ' ' || c == '\t' {
                                    'x'
                                } else {
                                    c
                                }
                            })
                            .collect(),
                        marks: r.marks.clone(),
                    })
                    .collect(),
            ),
        })
        .collect()
}

/// Spec "Compatibility guarantees", Tasks: where the legacy listing agrees
/// with the room, texts, indexes and states are as before (P1), unless the
/// item falls in a named `task_class`. A task listed otherwise must be
/// listed as legacy listed it once text that reads as Markdown syntax is
/// taken out of the page (`without_markdown_syntax`): the writer doesn't
/// escape text, a follow-up.
#[test]
fn listing_matches_legacy_where_legacy_agrees_with_the_room() {
    let (mut same, mut classed) = (0, 0);
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let Some(tasks) = verdicts(&case.doc) else {
            continue;
        };
        let mut repaired = None;
        for (k, (old, new, verdict)) in tasks.iter().enumerate() {
            match verdict {
                Verdict::Same => same += 1,
                Verdict::Exempt(_) => classed += 1,
                Verdict::Differs => {
                    let repaired = repaired.get_or_insert_with(|| {
                        verdicts(&build(&without_markdown_syntax(&read(&case.doc))))
                    });
                    match repaired.as_ref().and_then(|v| v.get(k)) {
                        Some((_, _, Verdict::Same | Verdict::Exempt(_))) => {}
                        _ => failures.push(format!(
                            "{}: task {k}\n  legacy: {old:?}\n  new:    {new:?}",
                            case.name
                        )),
                    }
                }
            }
        }
    }
    assert_no_failures(
        "a task is listed differently from legacy where legacy agrees with the room",
        &failures,
    );
    assert_at_least("tasks listed as before", same, MIN_SAME_TASKS);
    assert_at_least("classed tasks", classed, MIN_CLASSED_TASKS);
}

/// Lower bounds on what the corpus tests compare, so a broken corpus can't
/// make them pass by comparing nothing. Far below today's counts: 21,452
/// toggles, and on the pages where legacy agrees with the room 1,826 tasks
/// listed as before, 2,443 classed ones (task_with_nested_list 1,116,
/// task_block_syntax_as_text 1,074, task_lines_read_as_code 196,
/// block_read_into_task 57; 2,352 of them listed differently) and 9 that
/// differ only through text that reads as Markdown syntax.
const MIN_TOGGLES: usize = 10_000;
const MIN_SAME_TASKS: usize = 1_000;
const MIN_CLASSED_TASKS: usize = 1_000;

/// The `task_class` of the first task item of each page, against the shapes
/// each class names and their neighbours that it doesn't.
#[test]
fn task_classes_name_their_items() {
    let shape = Shape::Import;
    let t = |kids| task(shape, false, kids);
    let quote = |kids| el("blockquote", kids);
    let heading = || ela("heading", vec![("level", shape.int(2))], vec![text("h")]);
    let code = || el("code_block", vec![text("x")]);
    let empty = || el("paragraph", Vec::new());
    let image = || {
        ela(
            "image",
            vec![("src", "https://example.com/a.png".into())],
            Vec::new(),
        )
    };
    let cell = |kids| el("table", vec![el("table_row", vec![el("table_cell", kids)])]);
    let cases: Vec<(&str, Vec<N>, Option<&str>)> = vec![
        ("one paragraph", vec![bullets(vec![t(vec![p("a")])])], None),
        (
            "nested list",
            vec![bullets(vec![t(vec![
                p("a"),
                bullets(vec![el("list_item", vec![p("b")])]),
            ])])],
            Some("task_with_nested_list"),
        ),
        (
            "quote holding a checklist",
            vec![bullets(vec![t(vec![
                p("a"),
                quote(vec![bullets(vec![t(vec![p("b")])])]),
            ])])],
            Some("task_with_nested_list"),
        ),
        (
            "table holding a list",
            vec![bullets(vec![t(vec![
                p("a"),
                cell(vec![bullets(vec![el("list_item", vec![p("b")])])]),
            ])])],
            Some("task_with_nested_list"),
        ),
        (
            "heading after the paragraph",
            vec![bullets(vec![t(vec![p("a"), heading()])])],
            Some("task_block_syntax_as_text"),
        ),
        (
            "heading first",
            vec![bullets(vec![t(vec![heading()])])],
            Some("task_block_syntax_as_text"),
        ),
        (
            "code after the paragraph",
            vec![bullets(vec![t(vec![p("a"), code()])])],
            Some("task_block_syntax_as_text"),
        ),
        (
            "quote after the paragraph",
            vec![bullets(vec![t(vec![p("a"), quote(vec![p("q")])])])],
            Some("task_block_syntax_as_text"),
        ),
        (
            "two paragraphs",
            vec![bullets(vec![t(vec![p("a"), p("b")])])],
            None,
        ),
        (
            "image first",
            vec![bullets(vec![t(vec![image(), p("b")])])],
            None,
        ),
        (
            "empty first paragraph",
            vec![bullets(vec![t(vec![empty(), p("b")])])],
            Some("task_lines_read_as_code"),
        ),
        (
            "leading line break",
            vec![bullets(vec![t(vec![el(
                "paragraph",
                vec![hard_break(), text("b")],
            )])])],
            Some("task_lines_read_as_code"),
        ),
        (
            "empty paragraph between two",
            vec![bullets(vec![t(vec![p("a"), empty(), p("b")])])],
            Some("task_lines_read_as_code"),
        ),
        (
            "empty paragraph last",
            vec![bullets(vec![t(vec![p("a"), empty()])])],
            None,
        ),
        (
            "empty paragraph alone",
            vec![bullets(vec![t(vec![empty()])])],
            None,
        ),
        (
            "line break between text",
            vec![bullets(vec![t(vec![el(
                "paragraph",
                vec![text("a"), hard_break(), text("b")],
            )])])],
            None,
        ),
        (
            "paragraph after the list in a quote",
            vec![quote(vec![bullets(vec![t(vec![p("t")])]), p("after")])],
            Some("block_read_into_task"),
        ),
        (
            "paragraph after the list in an item",
            vec![bullets(vec![el(
                "list_item",
                vec![p("a"), bullets(vec![t(vec![p("t")])]), p("after")],
            )])],
            Some("block_read_into_task"),
        ),
        (
            "heading after the list in a quote",
            vec![quote(vec![bullets(vec![t(vec![p("t")])]), heading()])],
            None,
        ),
        (
            "paragraph after a list at the top level",
            vec![bullets(vec![t(vec![p("t")])]), p("after")],
            None,
        ),
        (
            "not the last item",
            vec![quote(vec![
                bullets(vec![t(vec![p("t")]), t(vec![p("u")])]),
                p("after"),
            ])],
            None,
        ),
        (
            "ordered list",
            vec![ela(
                "ordered_list",
                vec![("start", shape.int(1))],
                vec![t(vec![p("a"), heading()])],
            )],
            None,
        ),
        (
            "table cell",
            vec![cell(vec![bullets(vec![t(vec![p("a"), heading()])])])],
            None,
        ),
    ];
    let mut failures = Vec::new();
    for (name, blocks, want) in cases {
        let doc = build(&blocks);
        let ydoc = doc.inner();
        let txn = ydoc.transact();
        let frag = txn.get_xml_fragment("default").expect("content");
        let items = task_items(&frag, &txn);
        let got = compat::task_class(&txn, &items[0]);
        if got != want {
            failures.push(format!("{name}: want {want:?}, got {got:?}"));
        }
    }
    assert_no_failures("a task item is named by the wrong class", &failures);
}
