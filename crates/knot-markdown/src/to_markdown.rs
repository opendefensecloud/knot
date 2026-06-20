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
            let level: u8 = el
                .get_attribute(txn, "level")
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
            let len = el.len(txn);
            for i in 0..len {
                let child = el
                    .get(txn, i)
                    .ok_or_else(|| SerError::Yrs("bq child missing".into()))?;
                let mut inner = String::new();
                write_block(&mut inner, txn, &child)?;
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
        "code_block" => {
            let lang = el.get_attribute(txn, "language").unwrap_or_default();
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
                    let align = cell_el.get_attribute(txn, "align");
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
                        write_block(&mut inner, txn, &child)?;
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
            let url = el.get_attribute(txn, "url").unwrap_or_default();
            let name = el.get_attribute(txn, "name").unwrap_or_default();
            let label = if name.is_empty() { "attachment" } else { &name };
            buf.push_str(&format!("[{label}]({url})\n"));
        }
        "image" => {
            let src = el.get_attribute(txn, "src").unwrap_or_default();
            let alt = el.get_attribute(txn, "alt").unwrap_or_default();
            let title = el.get_attribute(txn, "title");
            match title.as_deref() {
                Some(t) if !t.is_empty() => {
                    let escaped = t.replace('"', "\\\"");
                    buf.push_str(&format!("![{alt}]({src} \"{escaped}\")\n"));
                }
                _ => buf.push_str(&format!("![{alt}]({src})\n")),
            }
        }
        "excalidraw_board" => {
            let board_id = el.get_attribute(txn, "board_id").unwrap_or_default();
            let label = el.get_attribute(txn, "label");
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
                // GFM task-list items carry a `checked` attr. Bullets without
                // the attr emit a plain `- ` prefix.
                let prefix = match item_el.get_attribute(txn, "checked").as_deref() {
                    Some("true") => "- [x] ",
                    Some(_) => "- [ ] ",
                    None => "- ",
                };
                write_list_item(buf, txn, &item_el, prefix)?;
            }
        }
        "ordered_list" => {
            let mut idx: u64 = el
                .get_attribute(txn, "start")
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
                let prefix = format!("{idx}. ");
                write_list_item(buf, txn, &item_el, &prefix)?;
                idx += 1;
            }
        }
        other => return Err(SerError::UnsupportedNode(other.into())),
    }
    Ok(())
}

fn write_list_item<T: ReadTxn>(
    buf: &mut String,
    txn: &T,
    item: &yrs::XmlElementRef,
    prefix: &str,
) -> Result<(), SerError> {
    let pad: String = " ".repeat(prefix.chars().count());
    let len = item.len(txn);
    for i in 0..len {
        let child = item
            .get(txn, i)
            .ok_or_else(|| SerError::Yrs("li body missing".into()))?;
        let mut inner = String::new();
        write_block(&mut inner, txn, &child)?;
        for (j, line) in inner.trim_end_matches('\n').split('\n').enumerate() {
            if i == 0 && j == 0 {
                buf.push_str(prefix);
            } else {
                buf.push_str(&pad);
            }
            buf.push_str(line);
            buf.push('\n');
        }
    }
    Ok(())
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
