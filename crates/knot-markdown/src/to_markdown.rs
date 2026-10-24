//! Walks the canonical "default" XmlFragment of a Y.Doc and emits Markdown.

use std::collections::HashMap;
use std::sync::Arc;

use knot_crdt::{DocHandle, YrsEngine};
use thiserror::Error;
use yrs::types::text::YChange;
use yrs::{
    Any, GetString, Out, ReadTxn, Text, Transact, Xml, XmlElementRef, XmlFragment, XmlTextRef,
};

/// Per-run formatting attributes: `mark_name → Any value`.
type RunAttrs = HashMap<Arc<str>, Any>;

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
    #[error("unsupported node: {0}")]
    UnsupportedNode(String),
}

pub fn serialise(_engine: &YrsEngine, doc: &DocHandle) -> Result<String, SerError> {
    let yrs_doc = doc.inner();
    let txn = yrs_doc.transact();

    let frag = match txn.get_xml_fragment("default") {
        Some(f) => f,
        None => return Ok("\n".to_string()),
    };

    let mut buf = String::new();
    let len = frag.len(&txn);
    for i in 0..len {
        let child = frag
            .get(&txn, i)
            .ok_or_else(|| SerError::Yrs("child missing".into()))?;
        write_block(&mut buf, &txn, &child)?;
        if i + 1 < len {
            buf.push('\n');
        }
    }
    if !buf.ends_with('\n') {
        buf.push('\n');
    }
    Ok(buf)
}

fn write_block<T: ReadTxn>(buf: &mut String, txn: &T, node: &yrs::XmlOut) -> Result<(), SerError> {
    use yrs::XmlOut;
    let el = match node {
        XmlOut::Element(el) => el,
        _ => {
            return Err(SerError::UnsupportedNode(
                "non-element at block level".into(),
            ));
        }
    };
    let tag = el.tag().to_string();
    match tag.as_str() {
        "paragraph" => {
            write_inlines(buf, txn, el)?;
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
            write_inlines(buf, txn, el)?;
            buf.push('\n');
        }
        "blockquote" => {
            let mut lines = Vec::new();
            write_container(&mut lines, txn, el, true)?;
            for line in lines {
                match line {
                    Line::Text(text) if !text.is_empty() => {
                        buf.push_str("> ");
                        buf.push_str(&text);
                    }
                    Line::Text(_) | Line::Blank => buf.push('>'),
                }
                buf.push('\n');
            }
        }
        "code_block" => {
            let lang = attr(el, txn, "language").unwrap_or_default();
            buf.push_str("```");
            buf.push_str(&lang);
            buf.push('\n');
            let len = el.len(txn);
            for i in 0..len {
                if let Some(XmlOut::Text(t)) = el.get(txn, i) {
                    let s = t.get_string(txn);
                    buf.push_str(&s);
                }
            }
            buf.push_str("\n```\n");
        }
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
                    let clen = cell_el.len(txn);
                    let mut text = String::new();
                    for k in 0..clen {
                        let child = cell_el
                            .get(txn, k)
                            .ok_or_else(|| SerError::Yrs("cell child".into()))?;
                        let mut inner = String::new();
                        write_cell_block(&mut inner, txn, &child)?;
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
        "bullet_list" => {
            let len = el.len(txn);
            for i in 0..len {
                let item = el
                    .get(txn, i)
                    .ok_or_else(|| SerError::Yrs("li missing".into()))?;
                let XmlOut::Element(item_el) = item else {
                    continue;
                };
                write_list_item(buf, txn, &item_el, "- ")?;
            }
        }
        "ordered_list" => {
            let mut idx: u64 = attr(el, txn, "start")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let len = el.len(txn);
            for i in 0..len {
                let item = el
                    .get(txn, i)
                    .ok_or_else(|| SerError::Yrs("li missing".into()))?;
                let XmlOut::Element(item_el) = item else {
                    continue;
                };
                write_list_item(buf, txn, &item_el, &format!("{idx}. "))?;
                idx += 1;
            }
        }
        other => return Err(SerError::UnsupportedNode(other.into())),
    }
    Ok(())
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
    node: &yrs::XmlOut,
) -> Result<(), SerError> {
    let yrs::XmlOut::Element(el) = node else {
        return write_block(buf, txn, node);
    };
    let tag = el.tag().to_string();
    match tag.as_str() {
        "blockquote" => {
            for i in 0..el.len(txn) {
                let child = el
                    .get(txn, i)
                    .ok_or_else(|| SerError::Yrs("bq child missing".into()))?;
                let mut inner = String::new();
                write_cell_block(&mut inner, txn, &child)?;
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
            let mut idx: u64 = attr(el, txn, "start")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            for i in 0..el.len(txn) {
                let item = el
                    .get(txn, i)
                    .ok_or_else(|| SerError::Yrs("li missing".into()))?;
                let yrs::XmlOut::Element(item) = item else {
                    continue;
                };
                let marker = if tag == "ordered_list" {
                    format!("{idx}. ")
                } else {
                    "- ".to_string()
                };
                let prefix = format!("{marker}{}", checkbox(&item, txn));
                let pad = " ".repeat(prefix.chars().count());
                for k in 0..item.len(txn) {
                    let child = item
                        .get(txn, k)
                        .ok_or_else(|| SerError::Yrs("li body missing".into()))?;
                    let mut inner = String::new();
                    write_cell_block(&mut inner, txn, &child)?;
                    for (j, line) in inner.trim_end_matches('\n').split('\n').enumerate() {
                        buf.push_str(if k == 0 && j == 0 { &prefix } else { &pad });
                        buf.push_str(line);
                        buf.push('\n');
                    }
                }
                idx += 1;
            }
        }
        _ => write_block(buf, txn, node)?,
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
/// An empty item keeps its trailing space (`- `, `- [ ] `, `2. `, P2), and
/// an item without children or checkbox writes nothing, as before.
fn write_list_item<T: ReadTxn>(
    buf: &mut String,
    txn: &T,
    item: &XmlElementRef,
    marker: &str,
) -> Result<(), SerError> {
    let pad = " ".repeat(marker.chars().count());
    let checkbox = checkbox(item, txn);
    let mut lines = Vec::new();
    let starts_with_text = matches!(
        item.get(txn, 0),
        Some(yrs::XmlOut::Element(first))
            if matches!(first.tag().as_ref(), "paragraph" | "attachment")
    );
    if !checkbox.is_empty() && !starts_with_text {
        lines.push(Line::Blank);
    }
    write_container(&mut lines, txn, item, false)?;
    for (i, line) in lines.into_iter().enumerate() {
        match line {
            _ if i == 0 => {
                buf.push_str(marker);
                buf.push_str(checkbox);
                if let Line::Text(text) = line {
                    buf.push_str(&text);
                }
            }
            Line::Text(text) => {
                buf.push_str(&pad);
                buf.push_str(&text);
            }
            Line::Blank => {}
        }
        buf.push('\n');
    }
    Ok(())
}

/// A line of a list item's or blockquote's content, before the item's indent
/// or the quote's `> ` goes in front of it.
enum Line {
    /// A line of one of its blocks. Every line of a code block is one, an
    /// empty one too: its indent is part of the code block, as it always
    /// was.
    Text(String),
    /// A blank line: between two blocks, or an empty line outside a code
    /// block. An item writes it without indent, a quote as `>`.
    Blank,
}

/// Writes the child blocks of a list item or, when `in_quote`, a blockquote
/// into `buf`, for the caller to put the item's indent or the quote's `> `
/// in front of each line.
///
/// Legacy wrote the blocks one right below the other, where Markdown reads
/// some of them as part of the block above (A5, A6, A9): a paragraph runs on
/// into the next paragraph, image or `3.` list, `---` makes the paragraph
/// above it a heading, and a paragraph after a nested list or quote becomes
/// a lazy continuation line of it. A blank line now goes between two blocks
/// exactly where the second would be read that way
/// ([`Block::needs_blank_line_before`]); everywhere else the output is as
/// before.
fn write_container<T: ReadTxn>(
    buf: &mut Vec<Line>,
    txn: &T,
    parent: &XmlElementRef,
    in_quote: bool,
) -> Result<(), SerError> {
    let mut prev: Option<Block> = None;
    for i in 0..parent.len(txn) {
        let child = parent
            .get(txn, i)
            .ok_or_else(|| SerError::Yrs("container child missing".into()))?;
        let mut inner = String::new();
        write_block(&mut inner, txn, &child)?;
        let text = inner.trim_end_matches('\n');
        let block = Block::of(&child, text);
        if prev.is_some_and(|prev| prev.needs_blank_line_before(block, in_quote)) {
            buf.push(Line::Blank);
        }
        for line in text.split('\n') {
            buf.push(if line.is_empty() && block != Block::Code {
                Line::Blank
            } else {
                Line::Text(line.to_string())
            });
        }
        prev = Some(block);
    }
    Ok(())
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
    /// A paragraph, or an attachment, written as a paragraph of one link.
    Text,
    /// An image or board, written as a paragraph of one image, which the
    /// importer turns back into a block only when nothing else is in its
    /// paragraph.
    Image,
    /// A list. Its first line `interrupts` a paragraph above it when its
    /// first item has content and, if ordered, is numbered 1 (CommonMark).
    List {
        interrupts: bool,
    },
    Quote,
    Table,
    /// A block the schema doesn't name.
    Other,
}

impl Block {
    /// `node`, written as `text`.
    fn of(node: &yrs::XmlOut, text: &str) -> Self {
        let yrs::XmlOut::Element(el) = node else {
            return Block::Other;
        };
        match el.tag().as_ref() {
            "heading" => Block::Heading,
            "code_block" => Block::Code,
            "horizontal_rule" => Block::Rule,
            "paragraph" if text.is_empty() => Block::Blank,
            "paragraph" | "attachment" => Block::Text,
            "image" | "excalidraw_board" => Block::Image,
            "bullet_list" => Block::List {
                interrupts: list_interrupts(text, false),
            },
            "ordered_list" => Block::List {
                interrupts: list_interrupts(text, true),
            },
            "blockquote" => Block::Quote,
            "table" => Block::Table,
            _ => Block::Other,
        }
    }

    /// Whether `next`, written on the line right below this block, would be
    /// read as part of it, so that a blank line must go between them.
    fn needs_blank_line_before(self, next: Block, in_quote: bool) -> bool {
        use Block::{Blank, Code, Heading, Image, List, Other, Quote, Rule, Table, Text};
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
            (Text | Image, Heading | Code | Quote | Table | List { interrupts: true }) => false,
            (Text | Image, _) => true,
            // A paragraph line or a table goes on a list or quote as a lazy
            // continuation line, or on a table as rows, and a quote right
            // below a quote joins it. Any other block start ends them, a list
            // starting at any number included.
            (List { .. } | Quote | Table, Text | Image | Table | Other) | (Quote, Quote) => true,
            (List { .. } | Quote | Table, _) => false,
            (Other, _) => true,
        }
    }
}

/// Whether a list written as `text` can interrupt a paragraph: CommonMark
/// lets it when its first item has content and, if ordered, is numbered 1.
fn list_interrupts(text: &str, ordered: bool) -> bool {
    let first = text.split('\n').next().unwrap_or("");
    let Some((marker, content)) = first.split_once(' ') else {
        return false;
    };
    !content.trim().is_empty() && (!ordered || marker == "1.")
}

fn write_inlines<T: ReadTxn>(
    buf: &mut String,
    txn: &T,
    parent: &XmlElementRef,
) -> Result<(), SerError> {
    use yrs::XmlOut;
    let len = parent.len(txn);
    for i in 0..len {
        let child = parent
            .get(txn, i)
            .ok_or_else(|| SerError::Yrs("inline missing".into()))?;
        match child {
            XmlOut::Text(t) => {
                let chunks = yrs_text_chunks(t, txn);
                for (text, attrs) in chunks {
                    write_run(buf, &text, attrs.as_ref());
                }
            }
            XmlOut::Element(el) => {
                let tag = el.tag().as_ref();
                match tag {
                    "hard_break" => buf.push_str("  \n"),
                    other => return Err(SerError::UnsupportedNode(format!("inline {other}"))),
                }
            }
            _ => return Err(SerError::UnsupportedNode("inline".into())),
        }
    }
    Ok(())
}

/// Iterate a `XmlTextRef` using `diff` and return `(text_chunk, Option<attrs>)` pairs.
///
/// yrs 0.21.3 API:
///   `text.diff(&txn, YChange::identity)` → `Vec<Diff<YChange>>`
///   `Diff.insert: Out`  (typically `Out::Any(Any::String(_))` for plain text)
///   `Diff.attributes: Option<Box<Attrs>>`   where `Attrs = HashMap<Arc<str>, Any>`
fn yrs_text_chunks<T: ReadTxn>(t: XmlTextRef, txn: &T) -> Vec<(String, Option<RunAttrs>)> {
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

fn write_run(buf: &mut String, text: &str, attrs: Option<&RunAttrs>) {
    use crate::schema::{MarkKind, mark_serialization};

    let mut non_link: Vec<MarkKind> = Vec::new();
    let mut link_href: Option<String> = None;
    let mut link_title: Option<String> = None;

    if let Some(map) = attrs {
        for (k, v) in map.iter() {
            match k.as_ref() {
                "bold" => non_link.push(MarkKind::Bold),
                "italic" => non_link.push(MarkKind::Italic),
                "code" => non_link.push(MarkKind::Code),
                "strike" => non_link.push(MarkKind::Strike),
                "underline" => non_link.push(MarkKind::Underline),
                "link" => {
                    if let Any::Map(m) = v {
                        if let Some(Any::String(s)) = m.get("href") {
                            link_href = Some(s.to_string());
                        }
                        if let Some(Any::String(s)) = m.get("title") {
                            link_title = Some(s.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Sort marks alphabetically by kind name for deterministic output.
    non_link.sort_by_key(|k| k.as_str());

    // Opening delimiters / tags.
    for kind in &non_link {
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
