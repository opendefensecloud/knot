//! The rules by which the new Markdown code is judged against the frozen
//! legacy copy (spec "Compatibility guarantees").
//!
//! This module sorts documents and Markdown inputs into the named bug
//! classes, where the new code may export, import or list a task otherwise
//! than legacy did. It also decides whether a document is healthy, checks a
//! document against the schema, and writes a canonical dump of a document
//! for comparing imports. And it holds the task differential: the rule by
//! which a page's new `/tasks` listing is judged against its legacy one.
//!
//! Two harnesses use it. The differential harness runs it over a generated
//! corpus (`tests/compat.rs` and `tests/tasks_compat.rs` in this crate).
//! The real-data check runs it over every page of a database
//! (`knot-server/tests/markdown_compat.rs`).

use std::fmt::Write as _;
use std::ops::Range;

use chrono::{DateTime, Utc};
use knot_crdt::tasks::{patch_task_checked, task_items};
use knot_crdt::{DocHandle, Engine, YrsEngine};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use uuid::Uuid;
use yrs::types::Attrs;
use yrs::types::text::YChange;
use yrs::{
    Any, GetString, Out, ReadTxn, Text, Transact, Transaction, Xml, XmlElementRef, XmlFragment,
    XmlOut,
};

use super::{extract_tasks, parse, serialise};

/// A named bug class: its name, and whether a document or input is in it.
type Class<T> = (&'static str, fn(&T) -> bool);

/// Named export bug classes. A healthy document in one of them may export
/// differently from the legacy serializer (serializer rule 1); every other
/// healthy document must export byte for byte as before. Each entry is a bug
/// from the spec, and the first entry that matches names the class.
///
/// A table cell is joined onto one line, where nothing reads back as block
/// structure, so cells keep the legacy layout: only `ordered_checkbox` and
/// `damaged_shape` look inside them.
const EXPORT_CLASSES: &[Class<DocHandle>] = &[
    ("damaged_shape", damaged_shape),
    ("nested_tasklist", nested_tasklist),
    ("multi_block_item", multi_block_item),
    ("quote_block_sequence", quote_block_sequence),
    ("ordered_checkbox", ordered_checkbox),
    ("rule_in_container", rule_under_line_in_container),
];

/// Named import bug classes. A healthy Markdown input in one of them may
/// import differently from the legacy importer (importer rule).
const IMPORT_CLASSES: &[Class<str>] = &[
    (
        "item_first_child_not_paragraph",
        item_first_child_not_paragraph,
    ),
    ("rule_in_container", rule_in_container),
    ("html_between_item_lines", html_between_item_lines),
    (
        "image_after_block_in_tight_item",
        image_after_block_in_tight_item,
    ),
    // Legacy dropped the line breaks between the pieces of a code block's text.
    ("code_block_line_breaks", code_block_line_breaks),
    // Legacy stored a blockquote without blocks, which the schema forbids.
    ("empty_blockquote", empty_blockquote),
    // Legacy gave a paragraph a table cell's image in place of its own.
    ("cell_image_candidate_leak", cell_image_candidate_leak),
    // Legacy dropped every image and board in a table cell.
    ("image_in_cell", image_in_cell),
];

/// The export bug class `doc` falls in, if any.
pub fn export_class(doc: &DocHandle) -> Option<&'static str> {
    EXPORT_CLASSES
        .iter()
        .find(|(_, is_in)| is_in(doc))
        .map(|(name, _)| *name)
}

/// The import bug class `md` falls in, if any.
pub fn import_class(md: &str) -> Option<&'static str> {
    IMPORT_CLASSES
        .iter()
        .find(|(_, is_in)| is_in(md))
        .map(|(name, _)| *name)
}

/// Markdown with text the writer can't write back yet, a follow-up: no
/// round trip of its import can be stable. They import as they always did,
/// so they aren't import classes; only the stability property leaves them
/// out.
const STABILITY_EXEMPT: &[Class<str>] = &[
    ("markdown_escaping", markdown_escaping),
    ("leading_hard_break", leading_hard_break),
];

/// Why the import of `md` can't survive an export and a re-import yet, if
/// it can't.
pub fn stability_exempt(md: &str) -> Option<&'static str> {
    STABILITY_EXEMPT
        .iter()
        .find(|(_, is_in)| is_in(md))
        .map(|(name, _)| *name)
}

/// A named task bug class: its name, and whether a task item, read in a
/// transaction, is in it.
type TaskClass = (&'static str, fn(&Transaction, &XmlElementRef) -> bool);

/// Named task bug classes (planning decision P1, and A4, A5 and A6 by
/// controller ruling). On a page where the legacy listing agrees with the
/// room, a task item in one of them may be listed with other text than
/// legacy listed it. Its assignee and due date may differ only where
/// legacy's came from content that isn't the item's own, or where legacy
/// found none and the new one is a link in the item's own content. Each
/// entry is a bug from the spec in how legacy wrote the item, and the first
/// entry that matches names the class. A classed task is listed otherwise
/// only where legacy damaged it: the legacy export of its page reads back
/// under legacy with the item changed (`tests/tasks_compat.rs` checks it).
const TASK_CLASSES: &[TaskClass] = &[
    ("task_with_nested_list", task_with_nested_list),
    ("task_block_syntax_as_text", task_block_syntax_as_text),
    ("task_lines_read_as_code", task_lines_read_as_code),
    ("block_read_into_task", block_read_into_task),
];

/// The task bug class `item`, a task item of a page read in `txn`, falls in,
/// if any.
pub fn task_class(txn: &Transaction, item: &XmlElementRef) -> Option<&'static str> {
    TASK_CLASSES
        .iter()
        .find(|(_, is_in)| is_in(txn, item))
        .map(|(name, _)| *name)
}

/// A task item with a block that is, or holds, a list item: a nested list,
/// or a table or quote holding one. The listing leaves such blocks out of
/// the item's text (P1): their items are tasks of their own, or plain
/// bullets. Legacy wrote them as more lines of the item's first paragraph
/// (A4), so it took their text, and their first mention or date when the
/// item had none of its own, as the item's.
fn task_with_nested_list(txn: &Transaction, item: &XmlElementRef) -> bool {
    children(txn, item)
        .iter()
        .any(|child| holds_list_item(txn, child))
}

/// A task item of a bullet list, outside table cells, with a block that
/// legacy wrote as text of the checkbox's paragraph (A4): a first block
/// other than a paragraph, attachment, image or board, written right after
/// the checkbox, or a later heading, blockquote, code block, rule or table,
/// indented as more lines of that paragraph. Their Markdown syntax (`## `,
/// `> `, a code fence, `---`, table pipes) was read as text.
fn task_block_syntax_as_text(txn: &Transaction, item: &XmlElementRef) -> bool {
    let blocks = children(txn, item);
    let Some(XmlOut::Element(first)) = blocks.first() else {
        return false;
    };
    in_bullet_list_outside_cells(item)
        && (!matches!(
            first.tag().as_ref(),
            "paragraph" | "attachment" | "image" | "excalidraw_board"
        ) || blocks[1..].iter().any(|block| {
            matches!(block, XmlOut::Element(el) if matches!(
                el.tag().as_ref(),
                "heading" | "blockquote" | "code_block" | "horizontal_rule" | "table"
            ))
        }))
}

/// A task item of a bullet list, outside table cells, that legacy wrote with
/// a blank line before another of its lines (A4): an empty paragraph, or a
/// paragraph line without text, the checkbox's line included. Legacy
/// indented the lines below it by the width of `- [ ] `, which makes them an
/// indented code block: raw Markdown, its links not read as links.
fn task_lines_read_as_code(txn: &Transaction, item: &XmlElementRef) -> bool {
    let mut blank = Vec::new();
    for block in children(txn, item) {
        match &block {
            XmlOut::Element(el) if el.tag().as_ref() == "paragraph" => {
                blank.extend(blank_lines(txn, el));
            }
            _ => blank.push(false),
        }
    }
    in_bullet_list_outside_cells(item)
        && blank
            .split_last()
            .is_some_and(|(_, before_last)| before_last.contains(&true))
}

/// For each line `paragraph` is written on, whether it has no text.
fn blank_lines<T: ReadTxn>(txn: &T, paragraph: &XmlElementRef) -> Vec<bool> {
    let mut lines = vec![true];
    for inline in children(txn, paragraph) {
        match inline {
            XmlOut::Element(e) if e.tag().as_ref() == "hard_break" => lines.push(true),
            XmlOut::Text(t) => {
                for (i, line) in t.get_string(txn).split('\n').enumerate() {
                    if i > 0 {
                        lines.push(true);
                    }
                    if !line.trim().is_empty() {
                        *lines.last_mut().expect("a line") = false;
                    }
                }
            }
            XmlOut::Element(_) | XmlOut::Fragment(_) => {
                *lines.last_mut().expect("a line") = false;
            }
        }
    }
    lines
}

/// Whether `item` sits in a bullet list outside table cells, where legacy
/// wrote its checkbox and kept the item's lines apart from the cell's.
fn in_bullet_list_outside_cells(item: &XmlElementRef) -> bool {
    let Some(XmlOut::Element(list)) = item.parent() else {
        return false;
    };
    let mut node = list.clone();
    while let Some(XmlOut::Element(parent)) = node.parent() {
        if matches!(parent.tag().as_ref(), "table_cell" | "table_header") {
            return false;
        }
        node = parent;
    }
    list.tag().as_ref() == "bullet_list"
}

/// A task item that ends the list it sits in, at any depth, where the block
/// legacy wrote on the line right below that list, in a list item or
/// blockquote outside table cells, reads as part of the item's last
/// paragraph (A5, A6, as `multi_block_item` and `quote_block_sequence`
/// name the pages): a lazy continuation line, or table rows. Legacy listed
/// that block's text as the item's.
fn block_read_into_task(txn: &Transaction, item: &XmlElementRef) -> bool {
    if !in_bullet_list_outside_cells(item) {
        return false;
    }
    let mut node = item.clone();
    while let Some(XmlOut::Element(parent)) = node.parent() {
        let siblings = children(txn, &parent);
        let Some(pos) = siblings
            .iter()
            .position(|sibling| matches!(sibling, XmlOut::Element(el) if *el == node))
        else {
            return false;
        };
        if let Some(next) = siblings.get(pos + 1) {
            let in_quote = parent.tag().as_ref() == "blockquote";
            return matches!(parent.tag().as_ref(), "list_item" | "blockquote")
                && match (written(txn, &siblings[pos]), written(txn, next)) {
                    (Some(prev), Some(next)) => {
                        next != Written::Rule && read_into(prev, next, in_quote)
                    }
                    _ => false,
                };
        }
        node = parent;
    }
    false
}

/// Whether `node` is, or holds, a list item: a block of a task item that
/// isn't the item's own content (P1).
pub fn holds_list_item<T: ReadTxn>(txn: &T, node: &XmlOut) -> bool {
    match node {
        XmlOut::Element(el) => {
            el.tag().as_ref() == "list_item"
                || children(txn, el)
                    .iter()
                    .any(|child| holds_list_item(txn, child))
        }
        XmlOut::Fragment(f) => (0..f.len(txn))
            .filter_map(|i| f.get(txn, i))
            .any(|child| holds_list_item(txn, &child)),
        XmlOut::Text(_) => false,
    }
}

// The task differential (spec "Compatibility guarantees", Tasks): the new
// listing, the Y-walk, against the legacy one, the legacy export read by
// legacy `extract_tasks`. `tests/tasks_compat.rs` proves it over the corpus
// and the real-data check applies it to every page of a database, so both
// judge a page by this one rule.

/// The href prefix of a mention.
pub const USER_PREFIX: &str = "knot://user/";
/// The href prefix of a date.
pub const TIME_PREFIX: &str = crate::TIME_URL_PREFIX;

/// A listed task, from the new or the legacy extractor.
#[derive(Clone, Debug, PartialEq)]
pub struct Listed {
    pub index: i32,
    pub text: String,
    pub assignee: Option<Uuid>,
    pub checked: bool,
    pub due: Option<DateTime<Utc>>,
}

impl From<crate::tasks::Task> for Listed {
    fn from(t: crate::tasks::Task) -> Self {
        Listed {
            index: t.item_index,
            text: t.text,
            assignee: t.assignee_user_id,
            checked: t.checked,
            due: t.due_at,
        }
    }
}

impl From<super::Task> for Listed {
    fn from(t: super::Task) -> Self {
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
pub fn listing(doc: &DocHandle) -> Vec<Listed> {
    crate::tasks::extract_tasks_from_doc(doc)
        .into_iter()
        .map(Listed::from)
        .collect()
}

/// The legacy listing of `doc`, when the legacy export of `doc` succeeds:
/// what the refresh listed before this release.
pub fn legacy_listing(doc: &DocHandle) -> Option<Vec<Listed>> {
    let md = serialise(&YrsEngine, doc).ok()?;
    Some(extract_tasks(&md).into_iter().map(Listed::from).collect())
}

/// How many task items the room counts in `doc`.
pub fn item_count(doc: &DocHandle) -> usize {
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    txn.get_xml_fragment("default")
        .map_or(0, |frag| task_items(&frag, &txn).len())
}

/// A copy of `doc` with task item `k` flipped by `patch_task_checked`, the
/// room's toggle. The copy goes through an update, because `yrs::Doc::clone`
/// shares its store.
///
/// # Panics
///
/// When `doc` has no task item `k`.
pub fn flipped(doc: &DocHandle, k: usize) -> DocHandle {
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
pub fn with_flipped(tasks: &[Listed], k: usize) -> Vec<Listed> {
    let mut want = tasks.to_vec();
    want[k].checked = !want[k].checked;
    want
}

/// Whether `legacy`, the legacy listing of `doc`, agrees with the room: it
/// has as many tasks as the room has task items, and flipping item `k` with
/// the room's toggle changes legacy task `k`'s `checked` and nothing else.
pub fn agrees_with_room(doc: &DocHandle, legacy: &[Listed]) -> bool {
    legacy.len() == item_count(doc)
        && (0..legacy.len())
            .all(|k| legacy_listing(&flipped(doc, k)) == Some(with_flipped(legacy, k)))
}

/// The legacy listing of `doc` when it agrees with the room.
pub fn agreeing_legacy_listing(doc: &DocHandle) -> Option<Vec<Listed>> {
    legacy_listing(doc).filter(|tasks| agrees_with_room(doc, tasks))
}

/// How the new listing lists a task of a page where the legacy listing
/// agrees with the room.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TaskVerdict {
    /// As legacy listed it.
    Same,
    /// Its item falls in this `task_class`, and the listing differs from
    /// legacy's no further than the class allows.
    Exempt(&'static str),
    /// Otherwise.
    Differs,
}

/// The legacy and the new listing of each task of `doc`, with its verdict.
/// `legacy` is the legacy listing of `doc`, and agrees with the room
/// ([`agrees_with_room`]).
///
/// # Panics
///
/// When the new listing breaks the lockstep property: it lists another
/// number of tasks than the room has task items.
pub fn task_verdicts(doc: &DocHandle, legacy: Vec<Listed>) -> Vec<(Listed, Listed, TaskVerdict)> {
    let new = listing(doc);
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    let items = txn
        .get_xml_fragment("default")
        .map(|frag| task_items(&frag, &txn))
        .unwrap_or_default();
    assert_eq!(new.len(), items.len(), "the lockstep property");
    legacy
        .into_iter()
        .zip(new)
        .zip(&items)
        .map(|((old, new), item)| {
            let verdict = match task_class(&txn, item) {
                None if old == new => TaskVerdict::Same,
                Some(class) if within_class(&txn, item, &old, &new) => TaskVerdict::Exempt(class),
                _ => TaskVerdict::Differs,
            };
            (old, new, verdict)
        })
        .collect()
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

/// The text runs of the item's own content, its blocks other than those
/// that are, or hold, a list item (P1), each with its link target, if any.
pub fn own_runs(txn: &Transaction, item: &XmlElementRef) -> Vec<(String, Option<String>)> {
    fn runs(txn: &Transaction, node: &XmlOut, out: &mut Vec<(String, Option<String>)>) {
        match node {
            XmlOut::Element(el) => {
                for child in children(txn, el) {
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
    for child in children(txn, item) {
        if !holds_list_item(txn, &child) {
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

/// The pulldown-cmark events of `md` with their source ranges, read with
/// the importers' options.
fn events(md: &str) -> impl Iterator<Item = (Event<'_>, Range<usize>)> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);
    Parser::new_ext(md, opts).into_offset_iter()
}

/// Text from a backslash escape (`\*`) or a character reference (`&amp;`)
/// outside code. The writer doesn't escape text, which is a non-goal and a
/// follow-up issue, so such text can come back as Markdown syntax. An
/// escaped `|` in a table cell is not in the class: the writer escapes it.
fn markdown_escaping(md: &str) -> bool {
    let mut in_code_block = false;
    let mut in_cell = false;
    events(md).any(|(event, range)| {
        match event {
            Event::Start(Tag::CodeBlock(_)) => in_code_block = true,
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Start(Tag::TableCell) => in_cell = true,
            Event::End(TagEnd::TableCell) => in_cell = false,
            Event::Text(text) if !in_code_block => {
                let reference = md[range.clone()] != *text;
                let escape = md[..range.start].ends_with('\\')
                    && text.starts_with(|c: char| c.is_ascii_punctuation())
                    && !(in_cell && text.starts_with('|'));
                return reference || escape;
            }
            _ => {}
        }
        false
    })
}

/// A hard break before any other inline content of its paragraph, heading
/// or tight list item (`- \` then `b`). The writer's `"  \n"` at the start
/// of a line is not a hard break: the same follow-up as `markdown_escaping`.
/// Neither image alt text nor inline HTML counts as content: the importer
/// drops an image that shares its paragraph, and ignores inline HTML.
fn leading_hard_break(md: &str) -> bool {
    let mut at_start = false;
    let mut image_depth = 0usize;
    events(md).any(|(event, _)| {
        match event {
            Event::Start(Tag::Image { .. }) => image_depth += 1,
            Event::End(TagEnd::Image) => image_depth -= 1,
            _ if image_depth > 0 => {}
            Event::HardBreak => return at_start,
            Event::Text(_) | Event::Code(_) | Event::SoftBreak => at_start = false,
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. })
            | Event::End(
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link,
            )
            | Event::InlineHtml(_)
            | Event::TaskListMarker(_) => {}
            // A block starts or ends: the next inline content starts a
            // paragraph.
            _ => at_start = true,
        }
        false
    })
}

/// A list item the legacy importer stored without its leading paragraph:
/// an empty item (`-`), one starting with another block (`- # h`), or one
/// holding only an image or board, which the legacy importer dropped. The
/// new importer puts an empty paragraph first (A7).
fn item_first_child_not_paragraph(md: &str) -> bool {
    fn has_bad_item<T: ReadTxn>(txn: &T, node: &XmlOut) -> bool {
        let XmlOut::Element(el) = node else {
            return false;
        };
        let children: Vec<XmlOut> = (0..el.len(txn)).filter_map(|i| el.get(txn, i)).collect();
        (el.tag().as_ref() == "list_item"
            && children.first().and_then(tag_of).as_deref() != Some("paragraph"))
            || children.iter().any(|child| has_bad_item(txn, child))
    }
    let Ok((doc, _)) = parse(md) else {
        return false;
    };
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    txn.get_xml_fragment("default").is_some_and(|frag| {
        (0..frag.len(&txn))
            .filter_map(|i| frag.get(&txn, i))
            .any(|child| has_bad_item(&txn, &child))
    })
}

/// A rule inside a list item or blockquote, which the legacy importer moved
/// to the top level, after the enclosing container (A9).
fn rule_in_container(md: &str) -> bool {
    let mut depth = 0usize;
    events(md).any(|(event, _)| {
        match event {
            Event::Start(Tag::Item | Tag::BlockQuote(_)) => depth += 1,
            Event::End(TagEnd::Item | TagEnd::BlockQuote(_)) => depth -= 1,
            Event::Rule => return depth > 0,
            _ => {}
        }
        false
    })
}

/// Inline content, an HTML block, then more inline content, all directly in
/// one tight list item (`- a`, `  <!-- c -->`, `  b`). The legacy importer
/// appended the later line to the first paragraph, giving `"ab"` (A3); the
/// new importer keeps two paragraphs.
fn html_between_item_lines(md: &str) -> bool {
    /// What was last seen directly in an open list item.
    #[derive(Clone, Copy, PartialEq)]
    enum Seen {
        Nothing,
        Inline,
        HtmlAfterInline,
        Block,
    }
    // One entry per open block, innermost last: `Some` for a list item.
    let mut open: Vec<Option<Seen>> = Vec::new();
    events(md).any(|(event, _)| {
        let inline = matches!(
            event,
            Event::Text(_)
                | Event::Code(_)
                | Event::InlineHtml(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::Start(
                    Tag::Emphasis
                        | Tag::Strong
                        | Tag::Strikethrough
                        | Tag::Link { .. }
                        | Tag::Image { .. }
                )
        );
        let item = open.last_mut().and_then(Option::as_mut);
        match event {
            _ if inline => {
                if let Some(seen) = item {
                    if *seen == Seen::HtmlAfterInline {
                        return true;
                    }
                    *seen = Seen::Inline;
                }
            }
            Event::Start(Tag::Item) => open.push(Some(Seen::Nothing)),
            Event::Start(tag) => {
                if let Some(seen) = item {
                    let html_after_inline = matches!(tag, Tag::HtmlBlock)
                        && matches!(*seen, Seen::Inline | Seen::HtmlAfterInline);
                    *seen = if html_after_inline {
                        Seen::HtmlAfterInline
                    } else {
                        Seen::Block
                    };
                }
                open.push(None);
            }
            Event::End(
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image,
            ) => {}
            Event::End(_) => {
                open.pop();
            }
            Event::Rule => {
                if let Some(seen) = item {
                    *seen = Seen::Block;
                }
            }
            _ => {}
        }
        false
    })
}

/// A lone image or board as a run of its own directly in a tight list item,
/// after another block of the item (`- a`, `  # h`, `  ![x](y.png)`): the
/// run has exactly one image and no text, code or line break outside its alt
/// text. The legacy importer dropped it, because promotion needs a paragraph
/// on the stack (A2); the new importer keeps it as an `image` or
/// `excalidraw_board` block. A run that also has text imported the same way
/// before (the image is dropped from a paragraph with text), and so does one
/// with no image or several (it yields nothing).
fn image_after_block_in_tight_item(md: &str) -> bool {
    /// An open list item: whether one of its blocks was seen, and the run
    /// being read directly in it.
    #[derive(Default)]
    struct Item {
        after_block: bool,
        images: usize,
        image_depth: usize,
        content: bool,
    }
    impl Item {
        /// Ends the run directly in this item: whether it was a lone image
        /// after a block.
        fn end_run(&mut self) -> bool {
            let lone_image = self.after_block && self.images == 1 && !self.content;
            *self = Self {
                after_block: true,
                ..Self::default()
            };
            lone_image
        }
    }
    // One entry per open block, innermost last: `Some` for a list item.
    let mut open: Vec<Option<Item>> = Vec::new();
    events(md).any(|(event, _)| {
        let item = open.last_mut().and_then(Option::as_mut);
        match event {
            Event::Start(Tag::Image { .. }) => {
                if let Some(item) = item {
                    item.images += 1;
                    item.image_depth += 1;
                }
            }
            Event::End(TagEnd::Image) => {
                if let Some(item) = item {
                    item.image_depth = item.image_depth.saturating_sub(1);
                }
            }
            // The importer writes alt text nowhere, but code and line breaks
            // even inside an image.
            Event::Text(_) => {
                if let Some(item) = item
                    && item.image_depth == 0
                {
                    item.content = true;
                }
            }
            Event::Code(_) | Event::SoftBreak | Event::HardBreak => {
                if let Some(item) = item {
                    item.content = true;
                }
            }
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. })
            | Event::End(
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link,
            )
            | Event::InlineHtml(_)
            | Event::TaskListMarker(_) => {}
            // A block of the item starts, or a leaf block sits in it: the
            // run before it ends.
            Event::Start(tag) => {
                if let Some(item) = item
                    && item.end_run()
                {
                    return true;
                }
                open.push(matches!(tag, Tag::Item).then(Item::default));
            }
            Event::Rule | Event::Html(_) => {
                if let Some(item) = item
                    && item.end_run()
                {
                    return true;
                }
            }
            // A block ends; at the end of an item, so does its last run.
            Event::End(_) => {
                if let Some(Some(mut item)) = open.pop()
                    && item.end_run()
                {
                    return true;
                }
            }
            _ => {}
        }
        false
    })
}

/// A code block whose text pulldown-cmark emits in pieces, one of them
/// before the last ending in a line break: it emits the text line by line in
/// a list item or blockquote, and in an indented block. The legacy importer
/// stripped a line break from the end of every piece, not only from the end
/// of the text (the one before the closing fence), so `x\n\ny` read back as
/// `xy`; the new importer keeps them.
fn code_block_line_breaks(md: &str) -> bool {
    let mut pieces: Option<Vec<String>> = None;
    events(md).any(|(event, _)| {
        match event {
            Event::Start(Tag::CodeBlock(_)) => pieces = Some(Vec::new()),
            Event::Text(text) => {
                if let Some(pieces) = pieces.as_mut() {
                    pieces.push(text.to_string());
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                let pieces = pieces.take().unwrap_or_default();
                let legacy: String = pieces
                    .iter()
                    .map(|piece| piece.strip_suffix('\n').unwrap_or(piece))
                    .collect();
                let text = pieces.concat();
                return legacy != text.strip_suffix('\n').unwrap_or(&text);
            }
            _ => {}
        }
        false
    })
}

/// A blockquote the legacy importer stored without blocks, which the schema
/// (`blockquote: block+`) forbids: a lone `>`, or a quote of only an HTML
/// block, which the importer drops. The new importer gives it an empty
/// paragraph, which is written as `>` again.
fn empty_blockquote(md: &str) -> bool {
    parse(md).is_ok_and(|(doc, _)| {
        any_element(&doc, &|txn, el, _| {
            el.tag().as_ref() == "blockquote" && el.len(txn) == 0
        })
    })
}

/// A paragraph of one image or board and nothing else, the first paragraph
/// to end after a table cell whose line starts with an image or board. The
/// legacy importer kept that image of the cell as the candidate for
/// promotion to a block, and a cell ends no paragraph, so the candidate was
/// left for the next paragraph to end: a lone image or board there was
/// replaced by the cell's, unless the cell left only an image and the
/// paragraph's own was a board. The new importer makes each image of a cell
/// a block of the cell, and leaves nothing behind.
fn cell_image_candidate_leak(md: &str) -> bool {
    /// The paragraph being read: its images, whether the last of them is a
    /// board, and whether it has text, code or a line break outside them.
    #[derive(Default)]
    struct Para {
        images: usize,
        board: bool,
        content: bool,
    }
    // The candidates cells left: an image, a board.
    let (mut left_image, mut left_board) = (false, false);
    // In a table cell: whether text or code is on its line yet.
    let mut cell_written: Option<bool> = None;
    let mut para: Option<Para> = None;
    let mut image_depth = 0usize;
    events(md).any(|(event, _)| {
        match event {
            Event::Start(Tag::TableCell) => cell_written = Some(false),
            Event::End(TagEnd::TableCell) => cell_written = None,
            Event::Start(Tag::Paragraph) => para = Some(Para::default()),
            Event::Start(Tag::Image { dest_url, .. }) => {
                image_depth += 1;
                let board = is_board_sentinel(&dest_url);
                if cell_written == Some(false) {
                    left_board |= board;
                    left_image |= !board;
                } else if let Some(para) = para.as_mut() {
                    para.images += 1;
                    para.board = board;
                }
            }
            Event::End(TagEnd::Image) => image_depth = image_depth.saturating_sub(1),
            Event::Text(_) if image_depth > 0 => {}
            Event::Text(_) | Event::Code(_) | Event::SoftBreak | Event::HardBreak => {
                if let Some(written) = cell_written.as_mut() {
                    *written = true;
                }
                if let Some(para) = para.as_mut() {
                    para.content = true;
                }
            }
            Event::End(TagEnd::Paragraph) => {
                let para = para.take().unwrap_or_default();
                let lone = para.images == 1 && !para.content;
                let leak = lone && (left_board || (left_image && !para.board));
                (left_image, left_board) = (false, false);
                return leak;
            }
            _ => {}
        }
        false
    })
}

/// An image or board in a table cell. A GFM cell is one line, onto which the
/// serializer joins the cell's blocks with a space; the legacy importer
/// dropped every image there, alone or next to text. The new importer splits
/// the line back into blocks at each image.
fn image_in_cell(md: &str) -> bool {
    let mut in_cell = false;
    events(md).any(|(event, _)| {
        match event {
            Event::Start(Tag::TableCell) => in_cell = true,
            Event::End(TagEnd::TableCell) => in_cell = false,
            Event::Start(Tag::Image { .. }) => return in_cell,
            _ => {}
        }
        false
    })
}

/// Whether `url` is a board sentinel, `knot://board/<uuid>.svg`.
fn is_board_sentinel(url: &str) -> bool {
    url.strip_prefix(super::BOARD_URL_PREFIX)
        .and_then(|rest| rest.strip_suffix(super::BOARD_URL_SUFFIX))
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}

/// The elements of `doc`, with whether each sits in a table cell, for
/// `is_in` to test until one matches.
fn any_element(
    doc: &DocHandle,
    is_in: &dyn Fn(&Transaction, &XmlElementRef, bool) -> bool,
) -> bool {
    fn walk(
        txn: &Transaction,
        node: &XmlOut,
        in_cell: bool,
        is_in: &dyn Fn(&Transaction, &XmlElementRef, bool) -> bool,
    ) -> bool {
        let XmlOut::Element(el) = node else {
            return false;
        };
        let in_cell = in_cell || matches!(el.tag().as_ref(), "table_cell" | "table_header");
        is_in(txn, el, in_cell)
            || children(txn, el)
                .iter()
                .any(|child| walk(txn, child, in_cell, is_in))
    }
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    let Some(frag) = txn.get_xml_fragment("default") else {
        return false;
    };
    (0..frag.len(&txn))
        .filter_map(|i| frag.get(&txn, i))
        .any(|child| walk(&txn, &child, false, is_in))
}

fn children<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> Vec<XmlOut> {
    (0..el.len(txn)).filter_map(|i| el.get(txn, i)).collect()
}

/// Text or a hard break directly in a block container (the top level, a list
/// item, a blockquote, a table cell): the shape the old importer stored for a
/// line break or soft break in a tight list item (A1), which restore,
/// from-template and zip import then created again (A10). Legacy can't
/// export such a page at all; the new export glues the split line back
/// together (P3).
fn damaged_shape(doc: &DocHandle) -> bool {
    fn stray(children: &[XmlOut]) -> bool {
        children.iter().any(|child| match child {
            XmlOut::Text(_) => true,
            XmlOut::Element(el) => el.tag().as_ref() == "hard_break",
            XmlOut::Fragment(_) => false,
        })
    }
    let top_level = {
        let ydoc = doc.inner();
        let txn = ydoc.transact();
        txn.get_xml_fragment("default").is_some_and(|frag| {
            let children: Vec<XmlOut> = (0..frag.len(&txn))
                .filter_map(|i| frag.get(&txn, i))
                .collect();
            stray(&children)
        })
    };
    top_level
        || any_element(doc, &|txn, el, _| {
            matches!(
                el.tag().as_ref(),
                "list_item" | "blockquote" | "table_cell" | "table_header"
            ) && stray(&children(txn, el))
        })
}

/// A task item of a bullet list, outside table cells, that is more than one
/// line of text: a nested checklist above all, a second block, a line
/// break, a first block other than a paragraph or attachment, or no block
/// at all. Legacy indented the lines after the checkbox's by the width of
/// `- [ ] ` instead of the marker's, and wrote any first block right after
/// the checkbox, so Markdown read them as more text of the checkbox's line
/// (A4); an item without blocks it left out.
fn nested_tasklist(doc: &DocHandle) -> bool {
    any_element(doc, &|txn, el, in_cell| {
        !in_cell
            && el.tag().as_ref() == "bullet_list"
            && children(txn, el).iter().any(|item| {
                let XmlOut::Element(item) = item else {
                    return false;
                };
                item.get_attribute(txn, "checked").is_some()
                    && match children(txn, item).as_slice() {
                        [XmlOut::Element(only)] => match only.tag().as_ref() {
                            "paragraph" => has_line_break(txn, only),
                            "attachment" => false,
                            _ => true,
                        },
                        _ => true,
                    }
            })
    })
}

/// Whether `paragraph` is written on more than one line.
fn has_line_break<T: ReadTxn>(txn: &T, paragraph: &XmlElementRef) -> bool {
    children(txn, paragraph).iter().any(|inline| match inline {
        XmlOut::Element(e) => e.tag().as_ref() == "hard_break",
        XmlOut::Text(t) => t.get_string(txn).contains('\n'),
        XmlOut::Fragment(_) => false,
    })
}

/// An item of an ordered list with a checkbox (a `checked` attribute).
/// Legacy wrote no checkbox there, so the page lost it on export, while
/// `/tasks` toggles still counted the item (A8).
fn ordered_checkbox(doc: &DocHandle) -> bool {
    any_element(doc, &|txn, el, _| {
        el.tag().as_ref() == "ordered_list"
            && children(txn, el).iter().any(
                |item| matches!(item, XmlOut::Element(item) if item.get_attribute(txn, "checked").is_some()),
            )
    })
}

/// A list item, outside table cells, with a block that legacy wrote on the
/// line right below the block before it, where Markdown reads it as part of
/// that block (A5): a second paragraph runs on into the first, an image or
/// board joins the paragraph above and is dropped, a list that can't
/// interrupt a paragraph (`3.`, an empty first item) turns into its text or
/// a heading, and a paragraph, image or table continues a nested table, or a
/// nested list or quote whose last line is a paragraph's or a table row (not
/// one ending in a heading or code block). A rule under a paragraph is
/// `rule_in_container`.
fn multi_block_item(doc: &DocHandle) -> bool {
    any_element(doc, &|txn, el, in_cell| {
        !in_cell && el.tag().as_ref() == "list_item" && runs_on(txn, el, false)
    })
}

/// A blockquote, outside table cells, with a block that legacy wrote on the
/// line right below the block before it, where Markdown reads it as part of
/// that block (A6): a paragraph, image or table after a table or after a
/// list or quote whose last line is a paragraph's or a table row, a `3.`
/// list after a paragraph, a quote after a quote, a paragraph after an
/// image. A paragraph followed by a paragraph, image, board or
/// attachment is not in it: the importer reads every line of a quote's
/// paragraph as a paragraph of its own, so those round-trip as they are
/// (P2). A rule under a paragraph is `rule_in_container`.
fn quote_block_sequence(doc: &DocHandle) -> bool {
    any_element(doc, &|txn, el, in_cell| {
        !in_cell && el.tag().as_ref() == "blockquote" && runs_on(txn, el, true)
    })
}

/// A rule right below a paragraph, image, board or attachment in a list item
/// or blockquote, outside table cells. Legacy wrote it on the next line,
/// where `---` under a line makes that line a level-2 heading (A9).
fn rule_under_line_in_container(doc: &DocHandle) -> bool {
    any_element(doc, &|txn, el, in_cell| {
        !in_cell
            && matches!(el.tag().as_ref(), "list_item" | "blockquote")
            && children(txn, el).windows(2).any(|pair| {
                matches!(written(txn, &pair[0]), Some(Written::Line { .. }))
                    && written(txn, &pair[1]) == Some(Written::Rule)
            })
    })
}

/// Whether a block of `container` (a list item, or a blockquote when
/// `in_quote`) other than a rule reads as part of the block before it when
/// written on the next line.
fn runs_on<T: ReadTxn>(txn: &T, container: &XmlElementRef, in_quote: bool) -> bool {
    children(txn, container).windows(2).any(|pair| {
        match (written(txn, &pair[0]), written(txn, &pair[1])) {
            (Some(prev), Some(next)) => next != Written::Rule && read_into(prev, next, in_quote),
            _ => false,
        }
    })
}

/// A block as Markdown writes it, for what the line right below it can do
/// to it, and what its own first line does right below another block.
#[derive(Clone, Copy, PartialEq)]
enum Written {
    /// A heading or code block. It ends on its own last line, and its first
    /// line ends a paragraph above it.
    Closed,
    /// A rule. It ends on its own line, but makes a paragraph line right
    /// above it a heading.
    Rule,
    /// A paragraph without content, written as a blank line.
    Blank,
    /// A paragraph line: a paragraph, an attachment (a link) or, when
    /// `image`, an image or board, which the importer keeps only when it is
    /// alone in its paragraph.
    Line {
        image: bool,
    },
    /// A list. Its first line `interrupts` a paragraph above it when its
    /// first item has content and, if ordered, is numbered 1. It
    /// `ends_open` when its last line is a line of a paragraph or a table
    /// row, which the line right below it goes on.
    List {
        interrupts: bool,
        ends_open: bool,
    },
    /// A blockquote, which `ends_open` as a list does.
    Quote {
        ends_open: bool,
    },
    Table,
}

fn written<T: ReadTxn>(txn: &T, block: &XmlOut) -> Option<Written> {
    let XmlOut::Element(el) = block else {
        return None;
    };
    Some(match el.tag().as_ref() {
        "heading" | "code_block" => Written::Closed,
        "horizontal_rule" => Written::Rule,
        "paragraph" if !has_content(txn, el) => Written::Blank,
        "paragraph" | "attachment" => Written::Line { image: false },
        "image" | "excalidraw_board" => Written::Line { image: true },
        "bullet_list" | "ordered_list" => Written::List {
            interrupts: list_interrupts(txn, el),
            ends_open: ends_open(txn, block),
        },
        "blockquote" => Written::Quote {
            ends_open: ends_open(txn, block),
        },
        "table" => Written::Table,
        _ => return None,
    })
}

/// Whether `paragraph` holds text or a line break.
fn has_content<T: ReadTxn>(txn: &T, paragraph: &XmlElementRef) -> bool {
    children(txn, paragraph).iter().any(|inline| match inline {
        XmlOut::Text(t) => !t.get_string(txn).is_empty(),
        XmlOut::Element(_) | XmlOut::Fragment(_) => true,
    })
}

/// Whether `item` is written on a line at all: one without blocks or
/// checkbox is written as nothing.
fn is_written<T: ReadTxn>(txn: &T, item: &XmlElementRef) -> bool {
    item.len(txn) > 0 || item.get_attribute(txn, "checked").is_some()
}

/// Whether the first line of `list` can interrupt a paragraph above it. That
/// line is its first item's that is written at all ([`is_written`]), and
/// it can when it has content after the marker, a checkbox or a first block
/// other than an empty paragraph, and, in an ordered list, is numbered 1
/// (the list numbered from `start`, 1 when it is missing or not a number).
fn list_interrupts<T: ReadTxn>(txn: &T, list: &XmlElementRef) -> bool {
    let items: Vec<XmlElementRef> = children(txn, list)
        .into_iter()
        .filter_map(|item| match item {
            XmlOut::Element(item) => Some(item),
            _ => None,
        })
        .collect();
    let Some((index, item)) = items
        .iter()
        .enumerate()
        .find(|(_, item)| is_written(txn, item))
    else {
        return false;
    };
    let has_content = item.get_attribute(txn, "checked").is_some()
        || match item.get(txn, 0) {
            Some(XmlOut::Element(first)) if first.tag().as_ref() == "paragraph" => {
                has_content(txn, &first)
            }
            Some(_) => true,
            None => false,
        };
    let start = list
        .get_attribute(txn, "start")
        .and_then(|start| start.to_string(txn).parse::<u64>().ok())
        .unwrap_or(1);
    has_content
        && (list.tag().as_ref() == "bullet_list" || start.checked_add(index as u64) == Some(1))
}

/// Whether the last line `block` is written on, in a list item or
/// blockquote, is a line of a paragraph (an attachment, image or board is
/// written as one) or a table row, which the line right below it goes on as
/// a lazy continuation line or another row. A heading, code block or rule
/// ends on its own line, and so do a marker without content (`- `,
/// `- [x] `) and a quote's blank line (`>`), the last line of a quote that
/// ends in an empty paragraph. A list's last line is its last written item's
/// ([`is_written`]), and an item's that of its last block other than an
/// empty paragraph: the blank lines at the end of a list are left out.
fn ends_open<T: ReadTxn>(txn: &T, block: &XmlOut) -> bool {
    let XmlOut::Element(el) = block else {
        return false;
    };
    match el.tag().as_ref() {
        "paragraph" => has_content(txn, el),
        "attachment" | "image" | "excalidraw_board" | "table" => true,
        "bullet_list" | "ordered_list" => children(txn, el)
            .iter()
            .rev()
            .find_map(|item| match item {
                XmlOut::Element(item) if is_written(txn, item) => Some(
                    children(txn, item)
                        .iter()
                        .rev()
                        .find(|child| written(txn, child) != Some(Written::Blank))
                        .is_some_and(|last| ends_open(txn, last)),
                ),
                _ => None,
            })
            .unwrap_or(false),
        "blockquote" => children(txn, el)
            .last()
            .is_some_and(|last| ends_open(txn, last)),
        _ => false,
    }
}

/// Whether `next`, written on the line right below `prev`, reads as part of
/// `prev`.
fn read_into(prev: Written, next: Written, in_quote: bool) -> bool {
    use Written::{Blank, Closed, Line, List, Quote, Rule, Table};
    match (prev, next) {
        // Nothing goes on after a heading, code block or rule, and an empty
        // paragraph is a blank line already.
        (Closed | Rule | Blank, _) | (_, Blank) => false,
        (Line { .. }, Rule) => true,
        // The importer reads each line of a quote's paragraph as a
        // paragraph of its own, but drops an image that isn't alone in its
        // paragraph.
        (Line { image: false }, Line { .. }) if in_quote => false,
        (Line { .. }, Line { .. }) => true,
        (Line { .. }, List { interrupts, .. }) => !interrupts,
        (Line { .. }, Closed | Quote { .. } | Table) => false,
        // A paragraph line or a table goes on a list or quote that ends on
        // an open paragraph as a lazy continuation line, or on a table as
        // rows, and a quote right below a quote joins it. Any other block
        // start ends them, a list starting at any number included, and
        // after a list or quote whose last line isn't open (a heading, code
        // block or rule, an empty item, a quote's blank line) no paragraph
        // is left to go on.
        (
            List {
                ends_open: true, ..
            }
            | Quote { ends_open: true }
            | Table,
            Line { .. } | Table,
        ) => true,
        (Quote { .. }, Quote { .. }) => true,
        (List { .. } | Quote { .. } | Table, _) => false,
    }
}

/// The legacy export of `doc` when `doc` is healthy: the legacy export
/// succeeds and is a fixed point, `serialise(parse(m)) == m`, both legacy.
pub fn healthy(doc: &DocHandle) -> Option<String> {
    let md = serialise(&YrsEngine, doc).ok()?;
    let (again, _) = parse(&md).ok()?;
    (serialise(&YrsEngine, &again).ok()? == md).then_some(md)
}

/// The block group of `tools/schema.json`.
const BLOCKS: &[&str] = &[
    "paragraph",
    "heading",
    "blockquote",
    "code_block",
    "horizontal_rule",
    "bullet_list",
    "ordered_list",
    "excalidraw_board",
    "image",
    "attachment",
    "table",
];

/// What a node may contain, per `tools/schema.json`.
enum Content {
    /// `block*`, `block+` (`min: 1`) or `paragraph block*`.
    Blocks { min: usize, paragraph_first: bool },
    /// One or more of these nodes.
    OneOrMore(&'static [&'static str]),
    /// `inline*`: text and hard breaks.
    Inline,
    /// `text*`.
    Text,
    /// Nothing.
    Empty,
}

fn content(tag: &str) -> Option<Content> {
    Some(match tag {
        "paragraph" | "heading" => Content::Inline,
        "blockquote" | "table_cell" | "table_header" => Content::Blocks {
            min: 1,
            paragraph_first: false,
        },
        "list_item" => Content::Blocks {
            min: 1,
            paragraph_first: true,
        },
        "code_block" => Content::Text,
        "bullet_list" | "ordered_list" => Content::OneOrMore(&["list_item"]),
        "table" => Content::OneOrMore(&["table_row"]),
        "table_row" => Content::OneOrMore(&["table_cell", "table_header"]),
        "horizontal_rule" | "excalidraw_board" | "image" | "attachment" | "hard_break" => {
            Content::Empty
        }
        _ => return None,
    })
}

/// Violations of the content rules of `tools/schema.json`, one message per
/// offending node, such as
/// `doc > bullet_list[0] > list_item[0]: hard_break[1] not allowed in list_item`.
///
/// It checks `list_item: paragraph block*`, `table_cell`/`table_header:
/// block+`, `code_block: text*`, that no text or hard break sits directly
/// in a block container, and the remaining content rules and node names.
/// The top-level fragment is `block*`: empty Markdown imports to an empty
/// document, and the editor fills it on load.
pub fn schema_violations(doc: &yrs::Doc) -> Vec<String> {
    let txn = doc.transact();
    let mut out = Vec::new();
    if let Some(frag) = txn.get_xml_fragment("default") {
        let children: Vec<XmlOut> = (0..frag.len(&txn))
            .filter_map(|i| frag.get(&txn, i))
            .collect();
        let doc_content = Content::Blocks {
            min: 0,
            paragraph_first: false,
        };
        check(&txn, "doc", &doc_content, &children, "doc", &mut out);
    }
    out
}

fn tag_of(node: &XmlOut) -> Option<String> {
    match node {
        XmlOut::Element(el) => Some(el.tag().to_string()),
        _ => None,
    }
}

fn check<T: ReadTxn>(
    txn: &T,
    tag: &str,
    rule: &Content,
    children: &[XmlOut],
    path: &str,
    out: &mut Vec<String>,
) {
    // Whether `child` may sit in this node. Unknown elements pass here and
    // are reported once, as unknown, below.
    let allowed = |child: &XmlOut| -> bool {
        let child_tag = tag_of(child);
        if let Some(t) = &child_tag
            && content(t).is_none()
        {
            return true;
        }
        match rule {
            Content::Blocks { .. } => child_tag.is_some_and(|t| BLOCKS.contains(&t.as_str())),
            Content::OneOrMore(names) => child_tag.is_some_and(|t| names.contains(&t.as_str())),
            Content::Inline => match child {
                XmlOut::Text(_) => true,
                XmlOut::Element(_) => child_tag.as_deref() == Some("hard_break"),
                XmlOut::Fragment(_) => false,
            },
            Content::Text => matches!(child, XmlOut::Text(_)),
            Content::Empty => false,
        }
    };

    let min = match rule {
        Content::Blocks { min, .. } => *min,
        Content::OneOrMore(_) => 1,
        _ => 0,
    };
    if children.len() < min {
        out.push(format!("{path}: empty {tag}"));
    }
    if let Content::Blocks {
        paragraph_first: true,
        ..
    } = rule
        && !children.is_empty()
        && tag_of(&children[0]).as_deref() != Some("paragraph")
    {
        out.push(format!("{path}: {tag} doesn't start with a paragraph"));
    }
    for (i, child) in children.iter().enumerate() {
        if !allowed(child) {
            let what = match child {
                XmlOut::Element(el) => el.tag().to_string(),
                XmlOut::Text(_) => "text".to_string(),
                XmlOut::Fragment(_) => "fragment".to_string(),
            };
            out.push(format!("{path}: {what}[{i}] not allowed in {tag}"));
        }
    }

    for (i, child) in children.iter().enumerate() {
        let XmlOut::Element(el) = child else {
            continue;
        };
        let child_tag = el.tag().to_string();
        let child_path = format!("{path} > {child_tag}[{i}]");
        match content(&child_tag) {
            None => out.push(format!("{child_path}: unknown node {child_tag}")),
            Some(child_rule) => {
                let grandchildren: Vec<XmlOut> =
                    (0..el.len(txn)).filter_map(|j| el.get(txn, j)).collect();
                check(
                    txn,
                    &child_tag,
                    &child_rule,
                    &grandchildren,
                    &child_path,
                    out,
                );
            }
        }
    }
}

/// A canonical dump of a document, for comparing imports (importer rule):
/// - one line per element: its tag and its attributes sorted by name, with
///   values as strings, so `2` and `"2"` agree;
/// - one line per text run: the text, then its marks sorted by name, where
///   `{}` and `true` both mean "set" and other values are written with their
///   keys sorted. Adjacent runs of one `XmlText` with equal marks are one
///   line, and empty runs are left out.
pub fn normalised(doc: &yrs::Doc) -> String {
    let txn = doc.transact();
    let mut out = String::new();
    if let Some(frag) = txn.get_xml_fragment("default") {
        for i in 0..frag.len(&txn) {
            if let Some(child) = frag.get(&txn, i) {
                dump(&txn, &child, 0, &mut out);
            }
        }
    }
    out
}

/// One node of a document read in `txn` and all below it, as [`normalised`]
/// writes it.
pub fn normalised_node<T: ReadTxn>(txn: &T, node: &XmlOut) -> String {
    let mut out = String::new();
    dump(txn, node, 0, &mut out);
    out
}

fn dump<T: ReadTxn>(txn: &T, node: &XmlOut, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match node {
        XmlOut::Element(el) => {
            let mut attrs: Vec<(String, String)> = el
                .attributes(txn)
                .map(|(k, v)| (k.to_string(), out_string(txn, v)))
                .collect();
            attrs.sort();
            let _ = write!(out, "{pad}<{}", el.tag());
            for (k, v) in attrs {
                let _ = write!(out, " {k}={v:?}");
            }
            out.push_str(">\n");
            for i in 0..el.len(txn) {
                if let Some(child) = el.get(txn, i) {
                    dump(txn, &child, depth + 1, out);
                }
            }
        }
        XmlOut::Text(t) => {
            // (is text, content, marks); embeds never merge with text.
            let mut runs: Vec<(bool, String, Vec<String>)> = Vec::new();
            for chunk in t.diff(txn, YChange::identity) {
                let marks = chunk
                    .attributes
                    .as_deref()
                    .map(marks_of)
                    .unwrap_or_default();
                let (is_text, piece) = match chunk.insert {
                    Out::Any(Any::String(s)) => (true, s.to_string()),
                    other => (false, out_string(txn, other)),
                };
                match runs.last_mut() {
                    Some((true, text, m)) if is_text && *m == marks => text.push_str(&piece),
                    _ => runs.push((is_text, piece, marks)),
                }
            }
            for (is_text, piece, marks) in runs {
                if is_text && piece.is_empty() {
                    continue;
                }
                if is_text {
                    let _ = write!(out, "{pad}{piece:?}");
                } else {
                    let _ = write!(out, "{pad}embed {piece:?}");
                }
                for m in marks {
                    let _ = write!(out, " {m}");
                }
                out.push('\n');
            }
        }
        XmlOut::Fragment(f) => {
            let _ = writeln!(out, "{pad}<#fragment>");
            for i in 0..f.len(txn) {
                if let Some(child) = f.get(txn, i) {
                    dump(txn, &child, depth + 1, out);
                }
            }
        }
    }
}

fn marks_of(attrs: &Attrs) -> Vec<String> {
    let mut marks: Vec<String> = attrs
        .iter()
        .filter_map(|(k, v)| match v {
            Any::Null | Any::Undefined => None,
            Any::Bool(true) => Some(k.to_string()),
            Any::Map(m) if m.is_empty() => Some(k.to_string()),
            other => Some(format!("{k}={}", any_string(other))),
        })
        .collect();
    marks.sort();
    marks
}

fn out_string<T: ReadTxn>(txn: &T, value: Out) -> String {
    match value {
        Out::Any(a) => any_string(&a),
        other => other.to_string(txn),
    }
}

/// `Any` as a string: scalars as `Display` writes them, maps with their keys
/// sorted.
fn any_string(value: &Any) -> String {
    match value {
        Any::Map(m) => {
            let mut entries: Vec<(&String, &Any)> = m.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| format!("{k}: {:?}", any_string(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Any::Array(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|v| format!("{:?}", any_string(v)))
                .collect();
            format!("[{}]", inner.join(", "))
        }
        other => other.to_string(),
    }
}
