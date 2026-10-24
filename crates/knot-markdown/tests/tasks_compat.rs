//! The `/tasks` listing read from the page itself (spec A11, A12; planning
//! decision P1), checked against the toggles (`patch_task_checked`) and
//! against the legacy listing (the legacy export, then `extract_tasks`):
//! - listing index `k` and toggle index `k` are the same item, on every page;
//! - on a page where the legacy listing agrees with the room, every task is
//!   listed as legacy listed it, unless its item falls in a named
//!   `task_class`, and a classed task is listed otherwise only where legacy
//!   damaged it;
//! - an item with text of its own is never listed without text: no line of
//!   it is read as a nested list item.

mod common;

use chrono::{DateTime, Utc};
use common::corpus::{self, Case, Shape};
use common::{N, build, el, ela, hard_break, p, run, text};
use knot_crdt::tasks::{patch_task_checked, task_items};
use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::legacy::{self, compat};
use knot_markdown::tasks::{Task, extract_tasks_from_doc};
use uuid::Uuid;
use yrs::types::text::YChange;
use yrs::{
    Any, Out, ReadTxn, Text, Transact, Transaction, Xml, XmlElementRef, XmlFragment, XmlOut,
};

/// Seeds of the random corpus, and documents per seed: the corpus of
/// `tests/compat.rs`.
const RANDOM_SEEDS: std::ops::RangeInclusive<u64> = 1..=50;
const RANDOM_DOCS_PER_SEED: usize = 200;

/// How many failures a test prints before the total.
const SHOWN_FAILURES: usize = 10;

const TIME_URL: &str = "knot://time/2026-06-04T17:00:00Z";
const USER_URL: &str = "knot://user/44444444-5555-6666-7777-888888888888";

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

/// A paragraph of `lines`, with a hard break (Shift+Enter) between each two.
fn lines(lines: &[&str]) -> N {
    let mut inlines = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            inlines.push(hard_break());
        }
        inlines.push(text(line));
    }
    el("paragraph", inlines)
}

/// A task whose text has a line that reads as block syntax, a list marker
/// above all, is listed with that text as written, as legacy listed it. On
/// this branch before the fix, a line `1. slides` or `- milk` was read as a
/// nested list item, which took the task's place: the task was listed
/// without text, assignee or due date (controller ruling).
#[test]
fn text_that_reads_as_block_syntax_is_listed_as_written() {
    let board = || {
        ela(
            "excalidraw_board",
            vec![
                ("board_id", "11111111-2222-3333-4444-555555555555".into()),
                ("label", "Diagram".into()),
            ],
            Vec::new(),
        )
    };
    let cases: Vec<(&str, Vec<N>, &str)> = vec![
        (
            "ordered list marker after a line break",
            vec![lines(&["Prepare:", "1. slides"])],
            "Prepare: 1. slides",
        ),
        (
            "bullet list marker after a line break",
            vec![lines(&["Buy:", "- milk"])],
            "Buy: - milk",
        ),
        ("plus marker", vec![lines(&["a", "+ plus"])], "a + plus"),
        ("star marker", vec![lines(&["a", "* star"])], "a * star"),
        (
            "ordered marker with a parenthesis",
            vec![lines(&["a", "2) two"])],
            "a 2) two",
        ),
        ("marker alone", vec![lines(&["a", "-"])], "a -"),
        (
            "star in a second paragraph",
            vec![p("a"), p("* star")],
            "a * star",
        ),
        (
            "ordered marker in a second paragraph",
            vec![p("a"), p("1. slides")],
            "a 1. slides",
        ),
        ("heading", vec![lines(&["a", "# title"])], "a # title"),
        ("quote", vec![lines(&["a", "> quote"])], "a > quote"),
        // Markdown reads the run of backticks as a piece of text of its own.
        ("code fence", vec![lines(&["a", "```js"])], "a ``` js"),
        ("rule", vec![lines(&["a", "---"])], "a ---"),
        ("setext underline", vec![lines(&["a", "==="])], "a ==="),
        (
            "indented line",
            vec![lines(&["a", "    indented"])],
            "a indented",
        ),
        (
            "heading after a board",
            vec![p("a"), board(), p("# hash")],
            "a Diagram # hash",
        ),
    ];
    let mut failures = Vec::new();
    for shape in Shape::ALL {
        for (what, children, want) in &cases {
            let doc = build(&[bullets(vec![task(shape, false, children.clone())])]);
            let got = listing(&doc);
            let texts: Vec<&str> = got.iter().map(|t| t.text.as_str()).collect();
            if texts != [*want] {
                failures.push(format!("{what} ({}): listed {texts:?}", shape.name()));
            }
            let legacy = legacy_listing(&doc);
            if legacy.as_ref() != Some(&got) {
                failures.push(format!(
                    "{what} ({}): legacy listed {legacy:?}, now {got:?}",
                    shape.name()
                ));
            }
        }
    }
    assert_no_failures("a task isn't listed with its text as written", &failures);
}

/// `@Alice Prepare:`, Shift+Enter, `- milk by <date>`: the task keeps its
/// assignee and its due date, as legacy listed them.
#[test]
fn assignee_and_due_date_survive_a_list_marker_line() {
    for shape in Shape::ALL {
        let doc = build(&[bullets(vec![task(
            shape,
            false,
            vec![el(
                "paragraph",
                vec![
                    N::Text(vec![
                        run("@Alice", vec![("link", shape.link(USER_URL, None))]),
                        run(" Prepare:", Vec::new()),
                    ]),
                    hard_break(),
                    N::Text(vec![
                        run("- milk by ", Vec::new()),
                        run("Jun 4", vec![("link", shape.link(TIME_URL, None))]),
                    ]),
                ],
            )],
        )])]);
        let got = listing(&doc);
        assert_eq!(got.len(), 1, "{}", shape.name());
        assert_eq!(got[0].text, "Prepare: - milk by", "{}", shape.name());
        assert_eq!(
            got[0].assignee.map(|user| format!("knot://user/{user}")),
            Some(USER_URL.to_string()),
            "{}",
            shape.name()
        );
        assert_eq!(
            got[0].due.map(|due| due.to_rfc3339()).as_deref(),
            Some("2026-06-04T17:00:00+00:00"),
            "{}",
            shape.name()
        );
        assert_eq!(legacy_listing(&doc), Some(got), "{}", shape.name());
    }
}

/// Where a paragraph starts below a heading, rule or blockquote, starts a
/// blockquote, or follows a blank line, a first line that reads as block
/// syntax is escaped instead. The task keeps its text, as written but for
/// `1. `: Markdown reads the escaped `1\. ` as two pieces of text, which
/// `extract_tasks` joins with a space. Legacy wrote such items as text of
/// their first paragraph (`task_block_syntax_as_text`,
/// `task_lines_read_as_code`). A blank line right below a checkbox without
/// text would end the item, so none goes there, and code is fenced so that
/// none of its lines ends it.
#[test]
fn block_syntax_starting_a_paragraph_below_another_block_keeps_the_task() {
    for shape in Shape::ALL {
        let heading = || ela("heading", vec![("level", shape.int(2))], vec![text("h")]);
        let quote = |kids| el("blockquote", kids);
        let empty = || el("paragraph", Vec::new());
        let cases: Vec<(&str, Vec<N>, &str)> = vec![
            (
                "line break in a heading",
                vec![
                    p("a"),
                    ela(
                        "heading",
                        vec![("level", shape.int(2))],
                        vec![text("h"), hard_break(), text("- milk")],
                    ),
                ],
                "a h - milk",
            ),
            (
                "HTML below a blank line",
                vec![el(
                    "paragraph",
                    vec![
                        hard_break(),
                        hard_break(),
                        text("<u>"),
                        hard_break(),
                        text("x"),
                    ],
                )],
                "<u> x",
            ),
            // `extract_tasks` lists brackets as pieces of text of their own,
            // as it always did: `a [ x ] : y` within a paragraph.
            (
                "link reference definition below a heading",
                vec![p("a"), heading(), p("[x]: y")],
                "a h [x ] : y",
            ),
            (
                "two empty paragraphs first",
                vec![empty(), empty(), p("1. x")],
                "1 . x",
            ),
            (
                "code holding a fence",
                vec![el("code_block", vec![text("```")])],
                "```",
            ),
            (
                "bullet marker below a heading",
                vec![p("a"), heading(), p("- milk")],
                "a h - milk",
            ),
            (
                "ordered marker below a heading",
                vec![p("a"), heading(), p("1. slides")],
                "a h 1 . slides",
            ),
            (
                "ordered marker below a first heading",
                vec![heading(), p("2) two")],
                "h 2 ) two",
            ),
            (
                "quote below a rule",
                vec![p("a"), el("horizontal_rule", Vec::new()), p("> q")],
                "a > q",
            ),
            (
                "bullet marker starting a quote",
                vec![p("a"), quote(vec![p("- milk")])],
                "a - milk",
            ),
            (
                "bullet marker after a line break in a quote",
                vec![p("a"), quote(vec![lines(&["q", "- milk"])])],
                "a q - milk",
            ),
            (
                "ordered marker in a second paragraph of a quote",
                vec![p("a"), quote(vec![p("q"), p("1. slides")])],
                "a q 1. slides",
            ),
        ];
        for (what, children, want) in cases {
            let doc = build(&[bullets(vec![task(shape, false, children)])]);
            assert_eq!(
                texts(&doc),
                owned(&[(0, want)]),
                "{what} ({})",
                shape.name()
            );
        }
    }
}

/// An item that starts with a heading, code block or blockquote, as only
/// the old importer stored them, is listed with its text: its checkbox goes
/// on a line of its own (A7), which Markdown still reads as the item's
/// checkbox.
#[test]
fn heading_first_and_code_first_items_are_listed_with_their_text() {
    for shape in Shape::ALL {
        let doc = build(&[bullets(vec![
            task(
                shape,
                true,
                vec![ela(
                    "heading",
                    vec![("level", shape.int(2))],
                    vec![text("h")],
                )],
            ),
            task(shape, false, vec![el("code_block", vec![text("x")])]),
            task(shape, false, vec![el("blockquote", vec![p("q")])]),
        ])]);
        let got = listing(&doc);
        assert_eq!(
            got.iter()
                .map(|t| (t.index, t.text.as_str(), t.checked))
                .collect::<Vec<_>>(),
            vec![(0, "h", true), (1, "x", false), (2, "q", false)],
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
/// item `item`, no further than the classes allow: in its text, and in its
/// assignee and due date where legacy's came from content that isn't the
/// item's own (it holds no such link), or where legacy found none (it read
/// the link as text or code) and the new one is a link of the item's own.
fn within_class(txn: &Transaction, item: &XmlElementRef, old: &Listed, new: &Listed) -> bool {
    let own = own_links(txn, item);
    let own_user = |user: &Uuid| own.contains(&format!("{USER_PREFIX}{user}"));
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
            || (!old.assignee.as_ref().is_some_and(own_user)
                && new.assignee.as_ref().is_none_or(own_user)))
        && (old.due == new.due
            || (!old.due.as_ref().is_some_and(own_due) && new.due.as_ref().is_none_or(own_due)))
}

const USER_PREFIX: &str = "knot://user/";
const TIME_PREFIX: &str = "knot://time/";

/// The text runs of the item's own content, its blocks other than those
/// that are, or hold, a list item (P1), each with its link target, if any.
fn own_runs(txn: &Transaction, item: &XmlElementRef) -> Vec<(String, Option<String>)> {
    fn runs(txn: &Transaction, node: &XmlOut, out: &mut Vec<(String, Option<String>)>) {
        match node {
            XmlOut::Element(el) => {
                for child in (0..el.len(txn)).filter_map(|i| el.get(txn, i)) {
                    runs(txn, &child, out);
                }
            }
            XmlOut::Text(t) => {
                for chunk in t.diff(txn, YChange::identity) {
                    let Out::Any(Any::String(text)) = chunk.insert else {
                        continue;
                    };
                    let href = match chunk.attributes.as_ref().and_then(|a| a.get("link")) {
                        Some(Any::Map(link)) => match link.get("href") {
                            Some(Any::String(href)) => Some(href.to_string()),
                            _ => None,
                        },
                        _ => None,
                    };
                    out.push((text.to_string(), href));
                }
            }
            XmlOut::Fragment(_) => {}
        }
    }
    let mut out = Vec::new();
    for child in (0..item.len(txn)).filter_map(|i| item.get(txn, i)) {
        if !compat::holds_list_item(txn, &child) {
            runs(txn, &child, &mut out);
        }
    }
    out
}

/// The link targets in the item's own content.
fn own_links(txn: &Transaction, item: &XmlElementRef) -> Vec<String> {
    own_runs(txn, item)
        .into_iter()
        .filter_map(|(_, href)| href)
        .collect()
}

/// The text of the item's own content outside mention and date chips, whose
/// text `/tasks` may leave out, and without HTML tags (`<u>`): the writer
/// doesn't escape text (a follow-up), so Markdown reads them as HTML, which
/// `/tasks` leaves out, as legacy did.
fn own_text(txn: &Transaction, item: &XmlElementRef) -> String {
    let chip = |href: &str| href.starts_with(USER_PREFIX) || href.starts_with(TIME_PREFIX);
    let text: String = own_runs(txn, item)
        .into_iter()
        .filter(|(_, href)| !href.as_deref().is_some_and(chip))
        .map(|(text, _)| text)
        .collect();
    without_html_tags(&text)
}

/// `text` without its HTML tags: `<`, an optional `/`, a letter, and
/// everything up to the next `>`.
fn without_html_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tag = &rest[start + 1..];
        let name = tag.strip_prefix('/').unwrap_or(tag);
        match tag.find('>') {
            Some(end) if name.starts_with(|c: char| c.is_ascii_alphabetic()) => {
                rest = &tag[end + 1..];
            }
            _ => {
                out.push('<');
                rest = tag;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Spec "Compatibility guarantees", Tasks: where the legacy listing agrees
/// with the room, texts, indexes and states are as before (P1), unless the
/// item falls in a named `task_class`.
#[test]
fn listing_matches_legacy_where_legacy_agrees_with_the_room() {
    let (mut same, mut classed) = (0, 0);
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let Some(tasks) = verdicts(&case.doc) else {
            continue;
        };
        for (k, (old, new, verdict)) in tasks.iter().enumerate() {
            match verdict {
                Verdict::Same => same += 1,
                Verdict::Exempt(_) => classed += 1,
                Verdict::Differs => failures.push(format!(
                    "{}: task {k}\n  legacy: {old:?}\n  new:    {new:?}",
                    case.name
                )),
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

/// The task items of `doc`, each as `compat::normalised_node` writes it.
fn normalised_items(doc: &DocHandle) -> Vec<String> {
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    txn.get_xml_fragment("default")
        .map(|frag| {
            task_items(&frag, &txn)
                .into_iter()
                .map(|item| compat::normalised_node(&txn, &XmlOut::Element(item)))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether legacy damaged task item `k` of `doc`: the legacy export of the
/// page reads back under legacy with item `k` changed, or with another
/// number of task items. The page's legacy round trip is then lossy, and
/// the loss is the item's own.
fn legacy_damaged_item(doc: &DocHandle, k: usize) -> bool {
    let Ok(md) = legacy::serialise(&YrsEngine, doc) else {
        return true;
    };
    let Ok((back, _)) = legacy::parse(&md) else {
        return true;
    };
    let (before, after) = (normalised_items(doc), normalised_items(&back));
    before.len() != after.len() || before[k] != after[k]
}

/// The task classes exempt only listings that were wrong (controller
/// ruling), as `export_classes_name_only_damaged_documents` in
/// `tests/compat.rs` checks for the export classes: a classed task listed
/// otherwise than legacy listed it is one legacy damaged, so its page's
/// legacy round trip is lossy, at that item. A classed task legacy writes
/// back as it is, on a page legacy round-trips losslessly above all, is
/// listed as legacy listed it.
#[test]
fn classed_tasks_are_listed_otherwise_only_where_legacy_damaged_them() {
    let mut docs = corpus_docs();
    docs.extend(
        task_class_cases()
            .into_iter()
            .map(|(what, blocks, _)| Case {
                name: format!("case: {what}"),
                doc: build(&blocks),
            }),
    );
    let mut otherwise = 0;
    let mut failures = Vec::new();
    for case in docs {
        let Some(tasks) = verdicts(&case.doc) else {
            continue;
        };
        let differ: Vec<_> = tasks
            .iter()
            .enumerate()
            .filter(|(_, (old, new, verdict))| matches!(verdict, Verdict::Exempt(_)) && old != new)
            .collect();
        if differ.is_empty() {
            continue;
        }
        otherwise += differ.len();
        for (k, (old, new, verdict)) in differ {
            if legacy_damaged_item(&case.doc, k) {
                continue;
            }
            failures.push(format!(
                "{} ({verdict:?}): task {k}\n  legacy: {old:?}\n  new:    {new:?}",
                case.name
            ));
        }
    }
    assert_no_failures(
        "a classed task legacy writes back as it is is listed otherwise",
        &failures,
    );
    assert_at_least(
        "classed tasks listed otherwise",
        otherwise,
        MIN_CLASSED_TASKS_LISTED_OTHERWISE,
    );
}

/// An item with text of its own, outside mention and date chips, is never
/// listed without text (controller ruling): no line of that text is read as
/// a nested list item, which would take the item's place.
#[test]
fn task_with_text_of_its_own_is_listed_with_text() {
    let mut compared = 0;
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let listed = listing(&case.doc);
        let ydoc = case.doc.inner();
        let txn = ydoc.transact();
        let Some(frag) = txn.get_xml_fragment("default") else {
            continue;
        };
        for (k, item) in task_items(&frag, &txn).iter().enumerate() {
            let own = own_text(&txn, item);
            if own.trim().is_empty() {
                continue;
            }
            compared += 1;
            if listed[k].text.is_empty() {
                failures.push(format!(
                    "{}: task {k} has text {own:?}, listed {:?}",
                    case.name, listed[k]
                ));
            }
        }
    }
    assert_no_failures(
        "a task with text of its own is listed without text",
        &failures,
    );
    assert_at_least(
        "tasks with text of their own",
        compared,
        MIN_TASKS_WITH_TEXT,
    );
}

/// Lower bounds on what the corpus tests compare, so a broken corpus can't
/// make them pass by comparing nothing. Far below today's counts:
/// - 21,452 toggles;
/// - on the pages where legacy agrees with the room, 1,834 tasks listed as
///   before and 2,444 classed ones: task_with_nested_list 1,116,
///   task_block_syntax_as_text 1,073, task_lines_read_as_code 197 and
///   block_read_into_task 57;
/// - 2,385 classed tasks listed otherwise than legacy listed them, 10 of
///   them from `task_class_cases`, every one an item legacy damaged;
/// - 19,666 tasks with text of their own.
const MIN_TOGGLES: usize = 10_000;
const MIN_SAME_TASKS: usize = 1_000;
const MIN_CLASSED_TASKS: usize = 1_000;
const MIN_CLASSED_TASKS_LISTED_OTHERWISE: usize = 1_000;
const MIN_TASKS_WITH_TEXT: usize = 10_000;

/// Pages, each with the `task_class` of its first task item: the shapes each
/// class names, and their neighbours that it doesn't.
fn task_class_cases() -> Vec<(&'static str, Vec<N>, Option<&'static str>)> {
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
    vec![
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
    ]
}

/// The `task_class` of the first task item of each page, against the shapes
/// each class names and their neighbours that it doesn't.
#[test]
fn task_classes_name_their_items() {
    let mut failures = Vec::new();
    for (name, blocks, want) in task_class_cases() {
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
