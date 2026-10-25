//! Walks the canonical "default" XmlFragment of a Y.Doc and emits Markdown.
//!
//! The export is tolerant: it writes every page, whatever it holds (spec
//! "Tolerant export").
//! - Text and hard breaks the old importer left directly in a block
//!   container are written as the paragraphs it split them from
//!   ([`crate::normalise`], P3).
//! - A node the schema doesn't name is written as its text (P4).
//!   [`serialise_with_report`] names them.

use std::collections::HashMap;
use std::sync::Arc;

use knot_crdt::{DocHandle, YrsEngine};
use thiserror::Error;
use yrs::types::text::YChange;
use yrs::{
    Any, GetString, Out, ReadTxn, Text, Transact, Xml, XmlElementRef, XmlFragment, XmlOut,
    XmlTextRef,
};

use crate::normalise::{Child, block_children};
use crate::schema::{MarkKind, mark_serialization};

/// Per-run formatting attributes: `mark_name → Any value`.
type RunAttrs = HashMap<Arc<str>, Any>;

/// The tags of the unknown nodes written so far, in document order.
type Unknown = Vec<String>;

/// Read an XML attribute as a plain `String`.
///
/// yrs 0.27 changed [`Xml::get_attribute`] to return `Option<Out>` instead of
/// the `Option<String>` it returned through 0.21. Attributes written by the
/// editor round-trip as `Any::String`, whose `Display` emits the raw value
/// (no JSON quoting), so stringifying restores the pre-0.27 behaviour for
/// every attribute this serialiser reads.
fn attr<X: Xml, T: ReadTxn>(el: &X, txn: &T, name: &str) -> Option<String> {
    el.get_attribute(txn, name).map(|v| v.to_string(txn))
}

#[derive(Debug, Error)]
pub enum SerError {
    #[error("yrs read: {0}")]
    Yrs(String),
    /// No longer returned: an unknown node is written as its text (P4).
    #[error("unsupported node: {0}")]
    UnsupportedNode(String),
}

pub fn serialise(engine: &YrsEngine, doc: &DocHandle) -> Result<String, SerError> {
    serialise_with_report(engine, doc).map(|(md, _)| md)
}

/// [`serialise`], with the tags of the nodes it wrote as their text because
/// the schema doesn't name them there, in document order (P4).
pub fn serialise_with_report(
    _engine: &YrsEngine,
    doc: &DocHandle,
) -> Result<(String, Vec<String>), SerError> {
    let yrs_doc = doc.inner();
    let txn = yrs_doc.transact();
    let mut unknown = Unknown::new();

    let frag = match txn.get_xml_fragment("default") {
        Some(f) => f,
        None => return Ok(("\n".to_string(), unknown)),
    };

    let mut buf = String::new();
    let children = written_children(&txn, &frag);
    let len = children.len();
    for (i, child) in children.iter().enumerate() {
        write_child(&mut buf, &txn, child, &mut unknown)?;
        if i + 1 < len {
            buf.push('\n');
        }
    }
    if !buf.ends_with('\n') {
        buf.push('\n');
    }
    Ok((buf, unknown))
}

/// The blocks of `parent`, a block container, as they are written: its
/// children repaired ([`block_children`]), without the runs that hold no
/// text, only hard breaks. Written without them ([`write_inlines`]), such a
/// paragraph would be an empty line between two blocks, which Markdown reads
/// as nothing. A container of nothing but such runs keeps one, as an empty
/// paragraph, so that it is written as one (an empty item `- `, a quote `>`).
fn written_children<T: ReadTxn, F: XmlFragment>(txn: &T, parent: &F) -> Vec<Child> {
    let mut children = block_children(txn, parent);
    let textless = |child: &Child| match child {
        Child::Para(inlines) => inlines.iter().all(|inline| match inline {
            XmlOut::Text(t) => t.len(txn) == 0,
            XmlOut::Element(el) => el.tag().as_ref() == "hard_break",
            XmlOut::Fragment(_) => false,
        }),
        Child::Block(_) => false,
    };
    if children.iter().all(textless) {
        children.truncate(1);
    } else {
        children.retain(|child| !textless(child));
    }
    children
}

/// Writes `child`, a block of a block container.
fn write_child<T: ReadTxn>(
    buf: &mut String,
    txn: &T,
    child: &Child,
    unknown: &mut Unknown,
) -> Result<(), SerError> {
    match child {
        Child::Block(el) => write_block(buf, txn, el, unknown),
        Child::Para(inlines) => {
            write_inlines(buf, txn, inlines, true, unknown);
            buf.push('\n');
            Ok(())
        }
    }
}

/// Writes `el`, an element of a block container. One the schema doesn't name
/// there (a node it doesn't know, or a list item, row or cell out of place)
/// is written as its text, a paragraph, as knot-export does (P4).
fn write_block<T: ReadTxn>(
    buf: &mut String,
    txn: &T,
    el: &XmlElementRef,
    unknown: &mut Unknown,
) -> Result<(), SerError> {
    let tag = el.tag().to_string();
    match tag.as_str() {
        "paragraph" => {
            write_inlines(buf, txn, &children(txn, el), false, unknown);
            buf.push('\n');
        }
        "heading" => {
            let level: u8 = attr(el, txn, "level")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
                .clamp(1, 6);
            for _ in 0..level {
                buf.push('#');
            }
            buf.push(' ');
            write_inlines(buf, txn, &children(txn, el), false, unknown);
            buf.push('\n');
        }
        "blockquote" => {
            let mut lines = Vec::new();
            write_quote(&mut lines, txn, el, Purpose::Export, unknown)?;
            write_lines(buf, lines);
        }
        "code_block" => write_code_block(buf, txn, el, "```"),
        "horizontal_rule" => {
            buf.push_str("---\n");
        }
        "table" => {
            // Collect rows.  Within each row, collect cells with (text, align).
            let len = el.len(txn);
            let mut rows: Vec<Vec<(String, Option<String>, bool)>> = Vec::new();
            for ri in 0..len {
                let row = el
                    .get(txn, ri)
                    .ok_or_else(|| SerError::Yrs("row missing".into()))?;
                let XmlOut::Element(row_el) = row else {
                    continue;
                };
                if row_el.tag().as_ref() != "table_row" {
                    continue;
                }
                let mut cells: Vec<(String, Option<String>, bool)> = Vec::new();
                let rlen = row_el.len(txn);
                for ci in 0..rlen {
                    let cell = row_el
                        .get(txn, ci)
                        .ok_or_else(|| SerError::Yrs("cell missing".into()))?;
                    let XmlOut::Element(cell_el) = cell else {
                        continue;
                    };
                    let is_header = cell_el.tag().as_ref() == "table_header";
                    let align = attr(&cell_el, txn, "align");
                    // Serialise cell content. Cells contain `block+` but for GFM
                    // we collapse to a single inline line: write each block then
                    // strip newlines and join with a space.
                    let mut text = String::new();
                    for child in written_children(txn, &cell_el) {
                        let mut inner = String::new();
                        write_cell_block(&mut inner, txn, &child, unknown)?;
                        let inline = inner.replace('|', "\\|").replace('\n', " ");
                        let inline = inline.trim();
                        if !text.is_empty() && !inline.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(inline);
                    }
                    cells.push((text, align, is_header));
                }
                rows.push(cells);
            }
            // Derive column count from the first row.
            let col_count = rows.first().map(|r| r.len()).unwrap_or(0);
            if col_count == 0 {
                // Empty table — emit nothing.
                return Ok(());
            }
            // Header row: first row if any of its cells is a table_header,
            // otherwise synthesise an empty header (GFM requires one).
            let first_is_header = rows.first().is_some_and(|r| r.iter().any(|(_, _, h)| *h));
            type CellTuple = (String, Option<String>, bool);
            let (header_row, body_rows): (Vec<CellTuple>, &[Vec<CellTuple>]) = if first_is_header {
                (rows[0].clone(), &rows[1..])
            } else {
                (vec![(String::new(), None, true); col_count], &rows[..])
            };
            // Per-column alignment: take from the header row's align attrs.
            let aligns: Vec<Option<String>> =
                header_row.iter().map(|(_, a, _)| a.clone()).collect();
            // Emit header row.
            buf.push('|');
            for (t, _, _) in &header_row {
                buf.push(' ');
                buf.push_str(t);
                buf.push_str(" |");
            }
            buf.push('\n');
            // Emit alignment row.
            buf.push('|');
            for a in &aligns {
                match a.as_deref() {
                    Some("left") => buf.push_str(" :--- |"),
                    Some("center") => buf.push_str(" :---: |"),
                    Some("right") => buf.push_str(" ---: |"),
                    _ => buf.push_str(" --- |"),
                }
            }
            buf.push('\n');
            // Emit body rows.
            for r in body_rows {
                buf.push('|');
                for (t, _, _) in r {
                    buf.push(' ');
                    buf.push_str(t);
                    buf.push_str(" |");
                }
                buf.push('\n');
            }
        }
        "attachment" => {
            // Emitted as a markdown link to the blob URL with the original
            // filename as the visible text. On import we'd reconstruct the
            // attachment node by matching the link's href shape; for now
            // the export is lossy w.r.t. content-type/size (recoverable
            // from the blob metadata).
            let url = attr(el, txn, "url").unwrap_or_default();
            let name = attr(el, txn, "name").unwrap_or_default();
            let label = if name.is_empty() { "attachment" } else { &name };
            buf.push_str(&format!("[{label}]({url})\n"));
        }
        "image" => {
            let src = attr(el, txn, "src").unwrap_or_default();
            let alt = attr(el, txn, "alt").unwrap_or_default();
            let title = attr(el, txn, "title");
            match title.as_deref() {
                Some(t) if !t.is_empty() => {
                    let escaped = t.replace('"', "\\\"");
                    buf.push_str(&format!("![{alt}]({src} \"{escaped}\")\n"));
                }
                _ => buf.push_str(&format!("![{alt}]({src})\n")),
            }
        }
        "excalidraw_board" => {
            let board_id = attr(el, txn, "board_id").unwrap_or_default();
            let label = attr(el, txn, "label");
            let display = match label.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => crate::DEFAULT_BOARD_LABEL,
            };
            let url = crate::board_sentinel_url(&board_id);
            buf.push_str(&format!("![{display}]({url})\n"));
        }
        "bullet_list" | "ordered_list" => {
            let mut lines = Vec::new();
            for (item, marker) in list_items(txn, el)? {
                write_list_item(&mut lines, txn, &item, &marker, unknown)?;
            }
            write_lines(buf, lines);
        }
        _ => {
            unknown.push(tag);
            buf.push_str(&flatten_text(txn, el));
            buf.push('\n');
        }
    }
    Ok(())
}

/// Writes `el`, a code block, between two `fence`s.
fn write_code_block<T: ReadTxn>(buf: &mut String, txn: &T, el: &XmlElementRef, fence: &str) {
    let lang = attr(el, txn, "language").unwrap_or_default();
    buf.push_str(fence);
    buf.push_str(&lang);
    buf.push('\n');
    buf.push_str(&code_text(txn, el));
    buf.push('\n');
    buf.push_str(fence);
    buf.push('\n');
}

/// The code of `el`, a code block.
fn code_text<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> String {
    let mut code = String::new();
    for i in 0..el.len(txn) {
        if let Some(XmlOut::Text(t)) = el.get(txn, i) {
            code.push_str(&t.get_string(txn));
        }
    }
    code
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

fn children<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> Vec<XmlOut> {
    (0..el.len(txn)).filter_map(|i| el.get(txn, i)).collect()
}

/// Whether `el` is a paragraph written as no text: it holds no text, line
/// break or unknown node with text.
fn is_empty_paragraph<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> bool {
    el.tag().as_ref() == "paragraph"
        && children(txn, el).iter().all(|inline| match inline {
            XmlOut::Text(t) => t.len(txn) == 0,
            XmlOut::Element(el) => {
                el.tag().as_ref() != "hard_break" && flatten_text(txn, el).is_empty()
            }
            XmlOut::Fragment(_) => false,
        })
}

/// All text below `el`, in document order and without marks, as knot-export
/// keeps an element it doesn't know.
fn flatten_text<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> String {
    fn walk<T: ReadTxn>(txn: &T, node: &XmlOut, out: &mut String) {
        match node {
            XmlOut::Text(t) => {
                for (text, _) in yrs_text_chunks(t, txn) {
                    out.push_str(&text);
                }
            }
            XmlOut::Element(el) => {
                for child in children(txn, el) {
                    walk(txn, &child, out);
                }
            }
            XmlOut::Fragment(f) => {
                for child in (0..f.len(txn)).filter_map(|i| f.get(txn, i)) {
                    walk(txn, &child, out);
                }
            }
        }
    }
    let mut out = String::new();
    for child in children(txn, el) {
        walk(txn, &child, &mut out);
    }
    out
}

/// The items of `list`, a bullet or an ordered list, each with the marker it
/// is written after: `- `, or its number and `. `, counted from `start` (1
/// when it is missing or not a number).
fn list_items<T: ReadTxn>(
    txn: &T,
    list: &XmlElementRef,
) -> Result<Vec<(XmlElementRef, String)>, SerError> {
    let ordered = list.tag().as_ref() == "ordered_list";
    let mut number: u64 = if ordered {
        attr(list, txn, "start")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    } else {
        1
    };
    let mut items = Vec::new();
    for i in 0..list.len(txn) {
        let item = list
            .get(txn, i)
            .ok_or_else(|| SerError::Yrs("li missing".into()))?;
        let yrs::XmlOut::Element(item) = item else {
            continue;
        };
        let marker = if ordered {
            format!("{number}. ")
        } else {
            "- ".to_string()
        };
        items.push((item, marker));
        number += 1;
    }
    Ok(items)
}

/// Writes `lines`, each ended by a newline.
fn write_lines(buf: &mut String, lines: Vec<Line>) {
    for line in lines {
        if let Line::Text { text, .. } = line {
            buf.push_str(&text);
        }
        buf.push('\n');
    }
}

/// The checkbox that starts the content of a GFM task-list item, from its
/// `checked` attribute.
fn checkbox<T: ReadTxn>(item: &XmlElementRef, txn: &T) -> &'static str {
    match attr(item, txn, "checked").as_deref() {
        Some("true") => "[x] ",
        Some(_) => "[ ] ",
        None => "",
    }
}

/// Writes `node`, a block in a table cell, for the cell to join onto its one
/// line. A cell's lines end up as runs of spaces in its text, and nothing in
/// it reads back as block structure, so lists and quotes there keep the
/// layout they always had: no blank lines between blocks, and continuation
/// lines indented by the whole item prefix, checkbox included. Only the
/// checkbox of an ordered item is new (A8).
fn write_cell_block<T: ReadTxn>(
    buf: &mut String,
    txn: &T,
    child: &Child,
    unknown: &mut Unknown,
) -> Result<(), SerError> {
    let Child::Block(el) = child else {
        return write_child(buf, txn, child, unknown);
    };
    let tag = el.tag().to_string();
    match tag.as_str() {
        "blockquote" => {
            for child in written_children(txn, el) {
                let mut inner = String::new();
                write_cell_block(&mut inner, txn, &child, unknown)?;
                for line in inner.trim_end_matches('\n').split('\n') {
                    if line.is_empty() {
                        buf.push_str(">\n");
                    } else {
                        buf.push_str("> ");
                        buf.push_str(line);
                        buf.push('\n');
                    }
                }
            }
        }
        "bullet_list" | "ordered_list" => {
            for (item, marker) in list_items(txn, el)? {
                let prefix = format!("{marker}{}", checkbox(&item, txn));
                let pad = " ".repeat(prefix.chars().count());
                for (k, child) in written_children(txn, &item).iter().enumerate() {
                    let mut inner = String::new();
                    write_cell_block(&mut inner, txn, child, unknown)?;
                    for (j, line) in inner.trim_end_matches('\n').split('\n').enumerate() {
                        buf.push_str(if k == 0 && j == 0 { &prefix } else { &pad });
                        buf.push_str(line);
                        buf.push('\n');
                    }
                }
            }
        }
        _ => write_block(buf, txn, el, unknown)?,
    }
    Ok(())
}

/// Writes one list item after `marker` (`"- "`, `"10. "`).
///
/// The item's content starts after the marker, so its continuation lines are
/// indented by the marker's width only (A4). A GFM checkbox (`[x] `, `[ ] `,
/// from the `checked` attribute) is the start of that content, in bullet and
/// ordered lists alike (A8): it opens the item's first paragraph. In front
/// of another block it would turn that block into text, so an item that
/// doesn't start with a paragraph or an attachment (written as a paragraph
/// of one link) gets its checkbox on a line of its own, which reads back as
/// an empty first paragraph (A7). Only the old importer stored such items.
///
/// The same goes for an item without its first paragraph whose content has
/// a blank line between two of its lines, or that starts with a rule (`- ---`
/// is a rule, not an item): the marker goes on a line of its own, as the
/// empty paragraph the importer adds back writes it, so the export is a fixed
/// point. Legacy wrote no empty line inside an item (it indented every line),
/// and its `- ---` read back as a rule, so such an item never exported as
/// before anyway; one without blank lines (`- ## h`) keeps its bytes.
///
/// That empty paragraph, in front of a block the item could start with, is
/// written as the item without it is, for the same reason: an item read back
/// from `- ## h` holds it (A7), and must export as `- ## h` again.
///
/// An empty item keeps its trailing space (`- `, `- [ ] `, `2. `, P2), and
/// an item without children or checkbox writes nothing, as before.
fn write_list_item<T: ReadTxn>(
    buf: &mut Vec<Line>,
    txn: &T,
    item: &XmlElementRef,
    marker: &str,
    unknown: &mut Unknown,
) -> Result<(), SerError> {
    let children = written_children(txn, item);
    let checkbox = checkbox(item, txn);
    write_item(
        buf,
        txn,
        marker,
        checkbox,
        children,
        Purpose::Export,
        unknown,
    )
}

/// Writes a list item of `children` after `marker` and `checkbox` (`""` for
/// none), as [`write_list_item`] describes, for `purpose`.
fn write_item<T: ReadTxn>(
    buf: &mut Vec<Line>,
    txn: &T,
    marker: &str,
    checkbox: &str,
    mut children: Vec<Child>,
    purpose: Purpose,
    unknown: &mut Unknown,
) -> Result<(), SerError> {
    let pad = " ".repeat(marker.chars().count());
    if let [Child::Block(first), Child::Block(second), ..] = children.as_slice()
        && is_empty_paragraph(txn, first)
        && !matches!(second.tag().as_ref(), "paragraph" | "attachment")
        && BLOCKS.contains(&second.tag().as_ref())
    {
        children.remove(0);
    }
    let mut lines = Vec::new();
    write_container(&mut lines, txn, &children, false, purpose, unknown)?;
    // A node the schema doesn't name is written as a paragraph of its text.
    let first_tag = match children.first() {
        Some(Child::Block(first)) if BLOCKS.contains(&first.tag().as_ref()) => {
            Some(first.tag().to_string())
        }
        Some(_) => Some("paragraph".to_string()),
        None => None,
    };
    if !matches!(first_tag.as_deref(), Some("paragraph" | "attachment")) {
        let blank_between = matches!(lines.first(), Some(Line::Text { .. }))
            && lines
                .windows(2)
                .any(|pair| matches!(pair, [Line::Blank, Line::Text { .. }]));
        let rule_first = first_tag.as_deref() == Some("horizontal_rule");
        if !checkbox.is_empty() || blank_between || rule_first {
            lines.insert(0, Line::Blank);
        }
    }
    // An item whose first line holds no text ends at a blank line right
    // below it, which would leave the rest of a task's text outside it.
    //
    // A line below nothing but blank lines was escaped as one below a blank
    // line (`task_text_line`), and sits right below the checkbox once they
    // go: the first of them becomes the checkbox's line. The escape is right
    // there too. Markdown goes on with no paragraph after a checkbox without
    // text (`===` right below it underlines nothing, and a line six spaces
    // in is code), so the line starts a paragraph either way. Only `/tasks`
    // reads this Markdown, and the line lists as main listed it wherever
    // its Markdown is its text: main wrote it six spaces in, as code
    // (`task_lines_read_as_code`), and listed it raw, so only a link, mark,
    // bracket or backtick run in it lists otherwise.
    if purpose == Purpose::TaskText && lines.first().is_some_and(Line::is_blank) {
        let blanks = lines[1..].iter().take_while(|line| line.is_blank()).count();
        lines.drain(1..1 + blanks);
    }
    for (i, line) in lines.into_iter().enumerate() {
        buf.push(match line {
            Line::Text { text, open } if i == 0 => Line::Text {
                text: format!("{marker}{checkbox}{text}"),
                open,
            },
            // The marker alone, or with a checkbox, opens no paragraph.
            Line::Blank if i == 0 => Line::Text {
                text: format!("{marker}{checkbox}"),
                open: false,
            },
            Line::Text { text, open } => Line::Text {
                text: format!("{pad}{text}"),
                open,
            },
            Line::Blank => Line::Blank,
        });
    }
    Ok(())
}

/// The Markdown `/tasks` reads a task item's text, assignee and due date
/// from (planning decision P1): `- [ ] ` and the item's own blocks, written
/// as the item writes them. A block that is, or holds, a list item is left
/// out: a nested list, or a table or quote holding a checklist. Its items
/// are tasks of their own, or plain bullets, and none of their content is
/// the item's.
///
/// Its text is written so that it reads as the text it is, never as block
/// syntax ([`Purpose::TaskText`]): a line `- milk` or `1. slides` would
/// otherwise start a nested list, whose item takes the place of the task,
/// so that the task would be listed without its text, assignee and due date.
/// This Markdown is never stored or exported, so the escaping costs nothing.
pub(crate) fn task_item_markdown<T: ReadTxn>(txn: &T, item: &XmlElementRef) -> String {
    let children: Vec<Child> = written_children(txn, item)
        .into_iter()
        .filter(|child| !holds_list_item(txn, child))
        .collect();
    let mut lines = Vec::new();
    // Writing fails only on a child that vanishes during the read, which one
    // transaction rules out; the item is then listed without text.
    let written = write_item(
        &mut lines,
        txn,
        "- ",
        "[ ] ",
        children,
        Purpose::TaskText,
        &mut Unknown::new(),
    );
    if written.is_err() {
        return "- [ ] \n".to_string();
    }
    let mut buf = String::new();
    write_lines(&mut buf, lines);
    buf
}

/// Whether `child`, a block of a block container, is or holds a list item.
fn holds_list_item<T: ReadTxn>(txn: &T, child: &Child) -> bool {
    fn holds<T: ReadTxn>(txn: &T, node: &XmlOut) -> bool {
        match node {
            XmlOut::Element(el) => {
                el.tag().as_ref() == "list_item"
                    || children(txn, el).iter().any(|child| holds(txn, child))
            }
            XmlOut::Fragment(f) => (0..f.len(txn))
                .filter_map(|i| f.get(txn, i))
                .any(|child| holds(txn, &child)),
            XmlOut::Text(_) => false,
        }
    }
    match child {
        Child::Block(el) => holds(txn, &XmlOut::Element(el.clone())),
        Child::Para(inlines) => inlines.iter().any(|inline| holds(txn, inline)),
    }
}

/// Writes a blockquote for `purpose`: each line of its blocks after `> `,
/// and a blank line or an empty line of a code block as `>`.
fn write_quote<T: ReadTxn>(
    buf: &mut Vec<Line>,
    txn: &T,
    quote: &XmlElementRef,
    purpose: Purpose,
    unknown: &mut Unknown,
) -> Result<(), SerError> {
    let mut lines = Vec::new();
    let children = written_children(txn, quote);
    write_container(&mut lines, txn, &children, true, purpose, unknown)?;
    buf.extend(lines.into_iter().map(|line| match line {
        Line::Text { text, open } if !text.is_empty() => Line::Text {
            text: format!("> {text}"),
            open,
        },
        // A blank line of the quote: no paragraph goes on after it.
        Line::Text { .. } | Line::Blank => Line::Text {
            text: ">".to_string(),
            open: false,
        },
    }));
    Ok(())
}

/// A line of a list or blockquote, or of a list item's or blockquote's
/// content before the item's indent or the quote's `> ` goes in front of it.
enum Line {
    /// A line of one of its blocks. Every line of a code block is one, an
    /// empty one too: its indent is part of the code block, as it always
    /// was. It is `open` when it is a line of a paragraph (an attachment,
    /// image or board is written as one) or a table row: a line right below
    /// it goes on that paragraph as a lazy continuation line, or on the
    /// table as another row. A heading, code block or rule ends on its own
    /// line, and so do a marker without content (`- `, `- [x] `) and a
    /// quote's blank line (`>`).
    Text { text: String, open: bool },
    /// A blank line: between two blocks, or an empty line outside a code
    /// block. An item writes it without indent, a quote as `>`.
    Blank,
}

impl Line {
    /// Whether Markdown reads the line as a blank line: it is one, or holds
    /// nothing but blanks.
    fn is_blank(&self) -> bool {
        match self {
            Line::Text { text, .. } => text.trim().is_empty(),
            Line::Blank => true,
        }
    }
}

/// Writes `children`, the blocks of a list item or, when `in_quote`, a
/// blockquote ([`block_children`]), into `buf`, for the caller to put the
/// item's indent or the quote's `> ` in front of each line.
///
/// Legacy wrote the blocks one right below the other, where Markdown reads
/// some of them as part of the block above (A5, A6, A9): a paragraph runs on
/// into the next paragraph, image or `3.` list, `---` makes the paragraph
/// above it a heading, and a paragraph after a nested list or quote that
/// ends on an open paragraph becomes a lazy continuation line of it. A blank
/// line now goes between two blocks exactly where the second would be read
/// that way ([`Block::needs_blank_line_before`]); everywhere else the output
/// is as before.
///
/// For [`Purpose::TaskText`], the lines of text are written as
/// [`task_text_line`] says, and a paragraph, image or board right below
/// another goes on as more lines of it: no blank line goes between them.
/// Lines of text are those of a paragraph, and those of a heading but its
/// first: a line break in a heading starts a paragraph below it.
fn write_container<T: ReadTxn>(
    buf: &mut Vec<Line>,
    txn: &T,
    children: &[Child],
    in_quote: bool,
    purpose: Purpose,
    unknown: &mut Unknown,
) -> Result<(), SerError> {
    let task_text = purpose == Purpose::TaskText;
    let mut prev: Option<Block> = None;
    for child in children {
        let (block, lines) = block_lines(txn, child, purpose, unknown)?;
        let goes_on = task_text && block.is_text() && prev.is_some_and(Block::is_text);
        if !goes_on && prev.is_some_and(|prev| prev.needs_blank_line_before(block, in_quote)) {
            buf.push(Line::Blank);
        }
        let text_from = match block {
            _ if !task_text => None,
            Block::Text | Block::Image => Some(0),
            Block::Heading => Some(1),
            _ => None,
        };
        for (i, line) in lines.into_iter().enumerate() {
            let line = match text_from {
                Some(from) if i >= from => {
                    // An item's first line follows its checkbox. One below
                    // blank lines only goes right below the checkbox later
                    // (`write_item`), where it starts a paragraph too.
                    let after_checkbox = !in_quote && buf.is_empty();
                    task_text_line(buf.last(), line, after_checkbox)
                }
                _ => line,
            };
            buf.push(line);
        }
        prev = Some(block);
    }
    Ok(())
}

/// What a list item's or blockquote's blocks are written for.
#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    /// The export: Markdown that reads back as the page.
    Export,
    /// The Markdown `/tasks` reads a task item's text from
    /// ([`task_item_markdown`]): only the text it reads as matters, and that
    /// must be the item's text as it is ([`task_text_line`]).
    TaskText,
}

/// `line`, a line of a paragraph, or of an image, board or attachment
/// written as one, written for [`Purpose::TaskText`] so that it reads as the
/// text it is, never as the start of another block:
/// - right below a line of text, it goes on that paragraph, indented by four
///   more spaces: a line indented that far starts no block. Legacy wrote
///   every line of an item but its first so, which is why a task's text
///   reads as it always did;
/// - right after the item's checkbox (`after_checkbox`), it is as it is:
///   nothing there starts a block, and neither does a line of nothing but
///   blanks;
/// - elsewhere it starts a paragraph ([`escape_block_start`]), right below
///   a checkbox without text too.
fn task_text_line(prev: Option<&Line>, line: Line, after_checkbox: bool) -> Line {
    let Line::Text { text, open } = line else {
        return line;
    };
    let goes_on = matches!(prev, Some(Line::Text { text, open: true }) if !text.trim().is_empty());
    let text = if text.trim().is_empty() || after_checkbox {
        text
    } else if goes_on {
        format!("    {text}")
    } else {
        escape_block_start(&text)
    };
    Line::Text { text, open }
}

/// `text`, the first line of a paragraph, without the blanks it starts with
/// (four would make it code, and Markdown drops them anyway), written so
/// that it doesn't start a list item, heading, blockquote, code fence, rule,
/// HTML block or link reference definition:
/// - a backslash goes before the character that would start one: `\- milk`,
///   `\# h`, `\<u>`;
/// - an ordered list marker gets an empty HTML tag in front of it, which
///   `/tasks` leaves out: `<span></span>1. slides` lists as `1. slides`, as
///   main listed it. Markdown has no escape for a digit, and a backslash
///   before the `.` splits the text, which `extract_tasks` joins with a
///   space: `1\. slides` lists as `1 . slides`. Followed by text, the tag
///   starts no HTML block.
///
/// A paragraph starts below other text only after a heading, code block,
/// rule, table or blockquote, or in a blockquote, or after a blank line, or
/// right below a checkbox without text.
fn escape_block_start(text: &str) -> String {
    let text = text.trim_start_matches([' ', '\t']);
    // A marker ends the line, or a blank follows it.
    let ends_marker = |rest: &str| rest.is_empty() || rest.starts_with([' ', '\t']);
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let number = &text[digits..];
    // `[label]: target`.
    let definition = text.starts_with('[')
        && text
            .find(']')
            .is_some_and(|end| text[end + 1..].starts_with(':'));
    // An HTML block runs on to the next blank line.
    let html = text
        .strip_prefix('<')
        .is_some_and(|tag| tag.starts_with(|c: char| c.is_ascii_alphabetic() || "/!?".contains(c)));
    if (1..=9).contains(&digits) && number.starts_with(['.', ')']) && ends_marker(&number[1..]) {
        format!("<span></span>{text}")
    } else if (text.starts_with(['-', '+', '*']) && ends_marker(&text[1..]))
        || text.starts_with(['#', '>'])
        || text.starts_with("```")
        || text.starts_with("~~~")
        || is_rule(text)
        || definition
        || html
    {
        format!("\\{text}")
    } else {
        text.to_string()
    }
}

/// Whether `text` is a rule: three or more `*`, `-` or `_`, and nothing else
/// but blanks.
fn is_rule(text: &str) -> bool {
    let Some(mark) = text.chars().next().filter(|c| matches!(c, '*' | '-' | '_')) else {
        return false;
    };
    text.chars().all(|c| c == mark || c == ' ' || c == '\t')
        && text.chars().filter(|&c| c == mark).count() >= 3
}

/// `child`, a block of a list item or blockquote, as lines without the blank
/// lines it ends with, and as a [`Block`]. A block written as nothing is one
/// blank line. A blockquote is written for `purpose`.
fn block_lines<T: ReadTxn>(
    txn: &T,
    child: &Child,
    purpose: Purpose,
    unknown: &mut Unknown,
) -> Result<(Block, Vec<Line>), SerError> {
    let tag = match child {
        Child::Block(el) => el.tag().to_string(),
        Child::Para(_) => "paragraph".to_string(),
    };
    let mut lines = Vec::new();
    match (child, tag.as_str()) {
        (Child::Block(el), "blockquote") => write_quote(&mut lines, txn, el, purpose, unknown)?,
        (Child::Block(el), "bullet_list" | "ordered_list") => {
            for (item, marker) in list_items(txn, el)? {
                write_list_item(&mut lines, txn, &item, &marker, unknown)?;
            }
        }
        // A line of the code that is a fence would end the block early.
        (Child::Block(el), "code_block") if purpose == Purpose::TaskText => {
            let code = code_text(txn, el);
            let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            let mut text = String::new();
            write_code_block(&mut text, txn, el, &fence);
            lines.extend(
                text.trim_end_matches('\n')
                    .split('\n')
                    .map(|line| Line::Text {
                        text: line.to_string(),
                        open: false,
                    }),
            );
        }
        _ => {
            let mut text = String::new();
            write_child(&mut text, txn, child, unknown)?;
            let code = tag == "code_block";
            let open = !matches!(tag.as_str(), "heading" | "code_block" | "horizontal_rule");
            for line in text.trim_end_matches('\n').split('\n') {
                lines.push(if line.is_empty() && !code {
                    Line::Blank
                } else {
                    Line::Text {
                        text: line.to_string(),
                        open,
                    }
                });
            }
        }
    }
    while matches!(lines.last(), Some(Line::Blank)) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Line::Blank);
    }
    Ok((Block::of(&tag, &lines), lines))
}

/// A child block of a list item or blockquote as written, for what the line
/// right below it can do to it, and what its own first line does right
/// below another block.
#[derive(Clone, Copy, PartialEq)]
enum Block {
    /// A heading. It ends on its own line, which ends a paragraph above it.
    Heading,
    /// A code block. It ends on its own last line, its first line ends a
    /// paragraph above it, and its empty lines are part of it.
    Code,
    /// A rule. It ends on its own line, but `---` right below a paragraph
    /// line makes that line a heading.
    Rule,
    /// A paragraph written as no text: a blank line.
    Blank,
    /// A paragraph, an attachment, written as a paragraph of one link, or a
    /// node the schema doesn't name, written as its text (P4).
    Text,
    /// An image or board, written as a paragraph of one image, which the
    /// importer turns back into a block only when nothing else is in its
    /// paragraph.
    Image,
    /// A list. Its first line `interrupts` a paragraph above it when its
    /// first item has content and, if ordered, is numbered 1 (CommonMark).
    /// It `ends_open` when its last line is a line of an open paragraph or a
    /// table row ([`Line::Text`]).
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

impl Block {
    /// Whether the block is written as a paragraph of text: a paragraph, an
    /// attachment, an image or board, or a node the schema doesn't name.
    fn is_text(self) -> bool {
        matches!(self, Block::Text | Block::Image)
    }

    /// A block with tag `tag`, written as `lines`. A tag the schema doesn't
    /// name was written as its text, a paragraph.
    fn of(tag: &str, lines: &[Line]) -> Self {
        let ends_open = matches!(lines.last(), Some(Line::Text { open: true, .. }));
        match tag {
            "heading" => Block::Heading,
            "code_block" => Block::Code,
            "horizontal_rule" => Block::Rule,
            "image" | "excalidraw_board" => Block::Image,
            "bullet_list" | "ordered_list" => Block::List {
                interrupts: list_interrupts(lines, tag == "ordered_list"),
                ends_open,
            },
            "blockquote" => Block::Quote { ends_open },
            "table" => Block::Table,
            _ if matches!(lines, [Line::Blank]) => Block::Blank,
            _ => Block::Text,
        }
    }

    /// Whether `next`, written on the line right below this block, would be
    /// read as part of it, so that a blank line must go between them.
    fn needs_blank_line_before(self, next: Block, in_quote: bool) -> bool {
        use Block::{Blank, Code, Heading, Image, List, Quote, Rule, Table, Text};
        match (self, next) {
            // Nothing goes on after a heading, code block or rule, and an
            // empty paragraph is a blank line already.
            (Heading | Code | Rule | Blank, _) | (_, Blank) => false,
            // In a quote, the importer reads each line of a paragraph as a
            // paragraph of its own, an image or board alone on its line
            // included (P2).
            (Text, Text | Image) if in_quote => false,
            // A paragraph line goes on with the next line unless that line
            // starts a block that can interrupt a paragraph, as a table can
            // (GFM). A rule can't.
            (
                Text | Image,
                Heading
                | Code
                | Quote { .. }
                | Table
                | List {
                    interrupts: true, ..
                },
            ) => false,
            (Text | Image, _) => true,
            // A paragraph line or a table goes on a list or quote that ends
            // on an open paragraph as a lazy continuation line, or on a table
            // as rows, and a quote right below a quote joins it. Any other
            // block start ends them, a list starting at any number included,
            // and after a list or quote whose last line isn't open (a
            // heading, code block or rule, an empty item, a quote's blank
            // line) there is no paragraph to go on.
            (
                List {
                    ends_open: true, ..
                }
                | Quote { ends_open: true }
                | Table,
                Text | Image | Table,
            )
            | (Quote { .. }, Quote { .. }) => true,
            (List { .. } | Quote { .. } | Table, _) => false,
        }
    }
}

/// Whether a list written as `lines` can interrupt a paragraph: CommonMark
/// lets it when its first item has content and, if ordered, is numbered 1.
fn list_interrupts(lines: &[Line], ordered: bool) -> bool {
    let Some(Line::Text { text: first, .. }) = lines.first() else {
        return false;
    };
    let Some((marker, content)) = first.split_once(' ') else {
        return false;
    };
    !content.trim().is_empty() && (!ordered || marker == "1.")
}

/// A piece of inline content as it is written: text with its marks, or a
/// line break.
enum Piece {
    Text(String, Marks),
    Break,
}

/// Writes `inlines`, the inline nodes of a paragraph or heading. An element
/// other than a hard break is written as its text (P4).
///
/// With `repaired`, the content of a [`Child::Para`] that the repair put
/// together (P3), two things are written as the importer would have stored
/// them:
/// - adjacent text that is written with the same marks is one run, so
///   `**a**** ****b**` is never written: the paragraphs and runs of a glued
///   line are separate `XmlText`s;
/// - a hard break before the first text or after the last is left out: it
///   would write an empty line, which ends the paragraph, and Markdown has no
///   line break at either end of one.
///
/// Stored paragraphs are written as before, piece by piece: merging their
/// runs would change pages that round-trip today.
fn write_inlines<T: ReadTxn>(
    buf: &mut String,
    txn: &T,
    inlines: &[XmlOut],
    repaired: bool,
    unknown: &mut Unknown,
) {
    let mut pieces = Vec::new();
    inline_pieces(txn, inlines, repaired, unknown, &mut pieces);
    if repaired {
        let is_text = |piece: &Piece| matches!(piece, Piece::Text(text, _) if !text.is_empty());
        let first = pieces.iter().position(is_text).unwrap_or(pieces.len());
        let last = pieces.iter().rposition(is_text).unwrap_or(0);
        pieces = pieces
            .into_iter()
            .enumerate()
            .filter(|(i, piece)| !matches!(piece, Piece::Break) || (first < *i && *i < last))
            .map(|(_, piece)| piece)
            .collect();
    }
    for piece in pieces {
        match piece {
            Piece::Text(text, marks) => write_run(buf, &text, &marks),
            Piece::Break => buf.push_str("  \n"),
        }
    }
}

/// Appends the pieces of `inlines` to `out`. With `merge`, adjacent text of
/// equal marks is one piece, and empty text none.
fn inline_pieces<T: ReadTxn>(
    txn: &T,
    inlines: &[XmlOut],
    merge: bool,
    unknown: &mut Unknown,
    out: &mut Vec<Piece>,
) {
    let push = |out: &mut Vec<Piece>, text: String, marks: Marks| match out.last_mut() {
        _ if merge && text.is_empty() => {}
        Some(Piece::Text(prev, prev_marks)) if merge && *prev_marks == marks => {
            prev.push_str(&text)
        }
        _ => out.push(Piece::Text(text, marks)),
    };
    for node in inlines {
        match node {
            XmlOut::Text(t) => {
                for (text, attrs) in yrs_text_chunks(t, txn) {
                    push(out, text, Marks::of(attrs.as_ref()));
                }
            }
            XmlOut::Element(el) if el.tag().as_ref() == "hard_break" => out.push(Piece::Break),
            XmlOut::Element(el) => {
                unknown.push(el.tag().to_string());
                push(out, flatten_text(txn, el), Marks::default());
            }
            XmlOut::Fragment(f) => {
                let nested: Vec<XmlOut> = (0..f.len(txn)).filter_map(|i| f.get(txn, i)).collect();
                inline_pieces(txn, &nested, merge, unknown, out);
            }
        }
    }
}

/// Iterate a `XmlTextRef` using `diff` and return `(text_chunk, Option<attrs>)` pairs.
///
/// yrs 0.21.3 API:
///   `text.diff(&txn, YChange::identity)` → `Vec<Diff<YChange>>`
///   `Diff.insert: Out`  (typically `Out::Any(Any::String(_))` for plain text)
///   `Diff.attributes: Option<Box<Attrs>>`   where `Attrs = HashMap<Arc<str>, Any>`
fn yrs_text_chunks<T: ReadTxn>(t: &XmlTextRef, txn: &T) -> Vec<(String, Option<RunAttrs>)> {
    t.diff(txn, YChange::identity)
        .into_iter()
        .filter_map(|d| {
            // We only care about text chunks (Out::Any contains the string value).
            let text = match d.insert {
                Out::Any(Any::String(s)) => s.to_string(),
                _ => return None,
            };
            let attrs = d.attributes.map(|boxed| *boxed);
            Some((text, attrs))
        })
        .collect()
}

/// The marks of a run as they are written: the delimited ones in the order
/// they open, and a link's target and title. Runs with equal `Marks` are
/// written alike, whether a mark is stored as `{}` or as `true`.
#[derive(Default, PartialEq)]
struct Marks {
    non_link: Vec<MarkKind>,
    link_href: Option<String>,
    link_title: Option<String>,
}

impl Marks {
    fn of(attrs: Option<&RunAttrs>) -> Self {
        let mut marks = Marks::default();
        if let Some(map) = attrs {
            for (k, v) in map.iter() {
                match k.as_ref() {
                    "bold" => marks.non_link.push(MarkKind::Bold),
                    "italic" => marks.non_link.push(MarkKind::Italic),
                    "code" => marks.non_link.push(MarkKind::Code),
                    "strike" => marks.non_link.push(MarkKind::Strike),
                    "underline" => marks.non_link.push(MarkKind::Underline),
                    "link" => {
                        if let Any::Map(m) = v {
                            if let Some(Any::String(s)) = m.get("href") {
                                marks.link_href = Some(s.to_string());
                            }
                            if let Some(Any::String(s)) = m.get("title") {
                                marks.link_title = Some(s.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Sort marks alphabetically by kind name for deterministic output.
        marks.non_link.sort_by_key(|k| k.as_str());
        marks
    }
}

fn write_run(buf: &mut String, text: &str, marks: &Marks) {
    let Marks {
        non_link,
        link_href,
        link_title,
    } = marks;

    // Opening delimiters / tags.
    for kind in non_link {
        let meta = mark_serialization(*kind);
        if !meta.open_tag.is_empty() {
            buf.push_str(meta.open_tag);
        } else {
            buf.push_str(meta.delimiter);
        }
    }

    // Link open bracket.
    if link_href.is_some() {
        buf.push('[');
    }

    buf.push_str(text);

    // Link close.
    if let Some(href) = link_href {
        if let Some(title) = link_title {
            buf.push_str(&format!("]({href} \"{title}\")"));
        } else {
            buf.push_str(&format!("]({href})"));
        }
    }

    // Closing delimiters / tags (reversed).
    for kind in non_link.iter().rev() {
        let meta = mark_serialization(*kind);
        if !meta.close_tag.is_empty() {
            buf.push_str(meta.close_tag);
        } else {
            buf.push_str(meta.delimiter);
        }
    }
}
