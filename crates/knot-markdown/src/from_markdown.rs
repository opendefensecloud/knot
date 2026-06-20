//! CommonMark → Y.Doc via pulldown-cmark.
//!
//! Targets the v0.1 schema only.

use std::collections::HashMap;
use std::sync::Arc;

use knot_crdt::{DocHandle, Engine, YrsEngine};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use thiserror::Error;
use uuid::Uuid;
use yrs::types::Attrs;
use yrs::{
    Any, Text, Transact, Xml, XmlElementPrelim, XmlElementRef, XmlFragment, XmlTextPrelim,
    XmlTextRef,
};

use crate::{BOARD_URL_PREFIX, BOARD_URL_SUFFIX, DEFAULT_BOARD_LABEL};

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("encode initial state: {0}")]
    Encode(String),
}

/// Parse Markdown into a fresh Y.Doc. Returns the doc plus the initial
/// update bytes the caller can persist as the first update.
pub fn parse(src: &str) -> Result<(DocHandle, Vec<u8>), ParseError> {
    let engine = YrsEngine;
    let doc = engine.new_doc();

    {
        let yrs_doc = doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();

        // Active block-element stack (innermost last).
        let mut stack: Vec<XmlElementRef> = Vec::new();
        // Currently-active text marks (Vec of (kind_name, optional inner map for link attrs)).
        let mut active_marks: Vec<(String, HashMap<String, Any>)> = Vec::new();
        // While Some, we are between Tag::Image and TagEnd::Image for a sentinel image.
        // Inner Text events append to `alt_buffer` instead of being emitted as paragraph
        // content.  Fields: (board_id, alt_buffer).
        let mut pending_board: Option<(String, String)> = None;
        // Mirror of `pending_board` for non-sentinel images.  Fields:
        // (src, alt_buffer, optional title).  At TagEnd::Image we promote to
        // `paragraph_image`; at TagEnd::Paragraph we commit if the paragraph
        // wrapped exactly this image.
        let mut pending_image: Option<(String, String, Option<String>)> = None;
        let mut paragraph_image: Option<(String, String, Option<String>)> = None;
        // Image nesting depth — incremented on every Tag::Image (sentinel or not) and
        // decremented on TagEnd::Image.  When > 0, Text events are suppressed from the
        // paragraph so that alt text of unrecognised images doesn't leak as inline
        // content (matches the silent-drop behaviour for unsupported images in v0.1).
        let mut image_depth: u32 = 0;
        // Set on TagEnd::Image when we just closed a sentinel image inside a paragraph
        // that was empty when the image started.  Fields: (board_id, label_alt_text).
        // At TagEnd::Paragraph we commit the sentinel only if the paragraph child count
        // is still 0 — i.e. no text or other inline content was added after the image.
        let mut paragraph_sentinel: Option<(String, String)> = None;
        // Number of images encountered in the current paragraph.  Used to disqualify
        // a sentinel candidate when more than one image appears in the same paragraph
        // (we suppress image alt text from the paragraph, so the "paragraph still
        // empty" check alone would incorrectly promote the first sentinel).
        let mut paragraph_image_count: u32 = 0;

        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        opts.insert(Options::ENABLE_TABLES);
        let parser = Parser::new_ext(src, opts);

        // Per-table alignment carried by `Tag::Table(Vec<Alignment>)`. We
        // stash it on table entry and consume per-cell via a column index.
        let mut table_align: Vec<Alignment> = Vec::new();
        let mut in_table_header = false;
        let mut table_col_idx: usize = 0;

        for event in parser {
            match event {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => {
                        let el = push_block(&frag, &stack, &mut txn, "paragraph", &[]);
                        stack.push(el);
                        paragraph_image_count = 0;
                    }
                    Tag::Heading { level, .. } => {
                        let l: u8 = match level {
                            HeadingLevel::H1 => 1,
                            HeadingLevel::H2 => 2,
                            HeadingLevel::H3 => 3,
                            HeadingLevel::H4 => 4,
                            HeadingLevel::H5 => 5,
                            HeadingLevel::H6 => 6,
                        };
                        let el = push_block(
                            &frag,
                            &stack,
                            &mut txn,
                            "heading",
                            &[("level", l.to_string())],
                        );
                        stack.push(el);
                    }
                    Tag::BlockQuote(_) => {
                        let el = push_block(&frag, &stack, &mut txn, "blockquote", &[]);
                        stack.push(el);
                    }
                    Tag::CodeBlock(kind) => {
                        let lang: String = match kind {
                            CodeBlockKind::Indented => String::new(),
                            CodeBlockKind::Fenced(s) => s.to_string(),
                        };
                        let attrs: Vec<(&str, String)> = if lang.is_empty() {
                            vec![]
                        } else {
                            vec![("language", lang)]
                        };
                        let el = push_block(&frag, &stack, &mut txn, "code_block", &attrs);
                        stack.push(el);
                    }
                    Tag::List(start) => {
                        let kind = if start.is_some() {
                            "ordered_list"
                        } else {
                            "bullet_list"
                        };
                        let attrs: Vec<(&str, String)> = match start {
                            Some(s) => vec![("start", s.to_string())],
                            None => vec![],
                        };
                        let el = push_block(&frag, &stack, &mut txn, kind, &attrs);
                        stack.push(el);
                    }
                    Tag::Item => {
                        let el = push_block(&frag, &stack, &mut txn, "list_item", &[]);
                        stack.push(el);
                    }
                    Tag::Emphasis => active_marks.push(("italic".into(), HashMap::new())),
                    Tag::Strong => active_marks.push(("bold".into(), HashMap::new())),
                    Tag::Strikethrough => active_marks.push(("strike".into(), HashMap::new())),
                    Tag::Image {
                        dest_url, title, ..
                    } => {
                        // Track every image (sentinel or not) so we can suppress its
                        // alt-text Text events from leaking into the paragraph.
                        image_depth = image_depth.saturating_add(1);
                        paragraph_image_count = paragraph_image_count.saturating_add(1);
                        // Sentinel branch: capture board id + alt buffer when the URL
                        // matches and the paragraph is currently empty.
                        if pending_board.is_none()
                            && paragraph_sentinel.is_none()
                            && let Some(id) = match_board_sentinel(&dest_url)
                            && let Some(para) = stack.last()
                            && para.tag().as_ref() == "paragraph"
                            && para.len(&txn) == 0
                        {
                            pending_board = Some((id, String::new()));
                        } else if pending_image.is_none()
                            && paragraph_image.is_none()
                            && match_board_sentinel(&dest_url).is_none()
                            && let Some(para) = stack.last()
                            && para.tag().as_ref() == "paragraph"
                            && para.len(&txn) == 0
                        {
                            // Non-sentinel image inside an empty paragraph — track for
                            // promotion to a block-level `image` node at TagEnd::Paragraph.
                            let title_opt = if title.is_empty() {
                                None
                            } else {
                                Some(title.to_string())
                            };
                            pending_image = Some((dest_url.to_string(), String::new(), title_opt));
                        }
                    }
                    Tag::Table(aligns) => {
                        table_align = aligns;
                        in_table_header = false;
                        table_col_idx = 0;
                        let el = push_block(&frag, &stack, &mut txn, "table", &[]);
                        stack.push(el);
                    }
                    Tag::TableHead => {
                        in_table_header = true;
                        table_col_idx = 0;
                        let el = push_block(&frag, &stack, &mut txn, "table_row", &[]);
                        stack.push(el);
                    }
                    Tag::TableRow => {
                        in_table_header = false;
                        table_col_idx = 0;
                        let el = push_block(&frag, &stack, &mut txn, "table_row", &[]);
                        stack.push(el);
                    }
                    Tag::TableCell => {
                        let kind = if in_table_header {
                            "table_header"
                        } else {
                            "table_cell"
                        };
                        let mut attrs: Vec<(&str, String)> = Vec::new();
                        let align = table_align
                            .get(table_col_idx)
                            .copied()
                            .unwrap_or(Alignment::None);
                        match align {
                            Alignment::Left => attrs.push(("align", "left".to_string())),
                            Alignment::Center => attrs.push(("align", "center".to_string())),
                            Alignment::Right => attrs.push(("align", "right".to_string())),
                            Alignment::None => {}
                        }
                        table_col_idx += 1;
                        let el = push_block(&frag, &stack, &mut txn, kind, &attrs);
                        stack.push(el);
                        // Tiptap/PM expects block content in a cell; pulldown sends
                        // inline Text events directly. Open an implicit paragraph
                        // so Text events land in a valid container.
                        let p = push_block(&frag, &stack, &mut txn, "paragraph", &[]);
                        stack.push(p);
                    }
                    Tag::Link {
                        dest_url, title, ..
                    } => {
                        let mut m = HashMap::new();
                        m.insert(
                            "href".to_string(),
                            Any::String(Arc::from(dest_url.as_ref())),
                        );
                        if !title.is_empty() {
                            m.insert("title".to_string(), Any::String(Arc::from(title.as_ref())));
                        }
                        active_marks.push(("link".into(), m));
                    }
                    _ => {}
                },
                Event::End(end) => match end {
                    TagEnd::Paragraph => {
                        let para = stack.pop();
                        // If this paragraph wrapped exactly one sentinel image and
                        // nothing else, replace it with an excalidraw_board block at
                        // the same fragment position.  Otherwise leave the (possibly
                        // mixed-content) paragraph in place; the dropped image is the
                        // same silent loss as any other non-sentinel image in v0.1.
                        if let Some((board_id, alt)) = paragraph_sentinel.take()
                            && let Some(p) = para.as_ref()
                            && p.len(&txn) == 0
                            && paragraph_image_count == 1
                        {
                            // Remove the just-popped (empty) paragraph from its parent.
                            remove_last_child(&frag, &stack, &mut txn);
                            let label = match alt.as_str() {
                                "" => None,
                                s if s == DEFAULT_BOARD_LABEL => None,
                                other => Some(other.to_string()),
                            };
                            let mut attrs: Vec<(&str, String)> = vec![("board_id", board_id)];
                            if let Some(l) = label {
                                attrs.push(("label", l));
                            }
                            let _ = push_block(&frag, &stack, &mut txn, "excalidraw_board", &attrs);
                        }
                        // If the board branch didn't fire, try the regular-image branch.
                        else if let Some((src, alt, title)) = paragraph_image.take()
                            && let Some(p) = para.as_ref()
                            && p.len(&txn) == 0
                            && paragraph_image_count == 1
                        {
                            remove_last_child(&frag, &stack, &mut txn);
                            let mut attrs: Vec<(&str, String)> = vec![("src", src)];
                            if !alt.is_empty() {
                                attrs.push(("alt", alt));
                            }
                            if let Some(t) = title {
                                attrs.push(("title", t));
                            }
                            let _ = push_block(&frag, &stack, &mut txn, "image", &attrs);
                        }
                        // Defensive: if we somehow exited the paragraph still holding
                        // an unfinished pending_board/pending_image (e.g. malformed
                        // input), discard so it can't leak into the next paragraph.
                        pending_board = None;
                        pending_image = None;
                        paragraph_image = None;
                    }
                    TagEnd::Heading(_)
                    | TagEnd::BlockQuote(_)
                    | TagEnd::CodeBlock
                    | TagEnd::List(_)
                    | TagEnd::Item => {
                        stack.pop();
                    }
                    TagEnd::Table => {
                        stack.pop();
                        table_align.clear();
                    }
                    TagEnd::TableHead | TagEnd::TableRow => {
                        stack.pop();
                    }
                    TagEnd::TableCell => {
                        // Pop the implicit paragraph then the cell.
                        stack.pop();
                        stack.pop();
                    }
                    TagEnd::Image => {
                        image_depth = image_depth.saturating_sub(1);
                        // Promote a captured sentinel to a paragraph-level candidate.
                        // If a second sentinel image appears in the same paragraph, the
                        // `is_none()` guards in Tag::Image and here ensure we don't
                        // overwrite the candidate.  In that case the candidate will
                        // still fail the "paragraph child count == 0" check at
                        // TagEnd::Paragraph (since the second image, even if dropped,
                        // means surrounding inlines exist), naturally degrading to
                        // "neither image is recognised".  Non-sentinel images are
                        // ignored in v0.1 (no `image` node in the schema).
                        if let Some((board_id, alt)) = pending_board.take()
                            && paragraph_sentinel.is_none()
                        {
                            paragraph_sentinel = Some((board_id, alt));
                        }
                        if let Some((src, alt, title)) = pending_image.take()
                            && paragraph_image.is_none()
                        {
                            paragraph_image = Some((src, alt, title));
                        }
                    }
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                        active_marks.pop();
                    }
                    _ => {}
                },
                Event::Text(s) => {
                    let text = s.to_string();
                    // Inside any image: never emit alt text as paragraph content.
                    // If a sentinel image is currently pending, also capture the alt
                    // text into its buffer for potential use as a board label.
                    if image_depth > 0 {
                        if let Some((_, alt)) = pending_board.as_mut() {
                            alt.push_str(&text);
                        }
                        if let Some((_, alt, _)) = pending_image.as_mut() {
                            alt.push_str(&text);
                        }
                        continue;
                    }
                    // Defensive: pulldown can emit Text events outside any
                    // open block in pathological inputs (malformed user
                    // imports). Drop the text rather than panic.
                    let Some(parent) = stack.last() else {
                        continue;
                    };
                    let parent_tag = parent.tag().to_string();

                    // Code block bodies have no marks; append as plain text.
                    if parent_tag == "code_block" {
                        // pulldown-cmark includes a trailing newline in the code text;
                        // the to_markdown serializer adds its own newline before ```.
                        // Strip the trailing newline to avoid double-newline.
                        let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
                        let text_ref = ensure_text_child(parent, &mut txn);
                        let pos = text_ref.len(&txn);
                        text_ref.insert(&mut txn, pos, &text);
                    } else if parent_tag == "list_item" {
                        // Tight list: pulldown-cmark emits Text directly inside Item
                        // (no paragraph wrapper). Our schema requires list_item → paragraph.
                        let para = ensure_para_child(parent, &mut txn);
                        let attrs = attrs_from_marks(&active_marks);
                        let text_ref = ensure_text_child(&para, &mut txn);
                        let pos = text_ref.len(&txn);
                        text_ref.insert_with_attributes(&mut txn, pos, &text, attrs);
                    } else {
                        // Inline text in a paragraph/heading.
                        // Attach current active marks as Attrs.
                        let attrs = attrs_from_marks(&active_marks);
                        let text_ref = ensure_text_child(parent, &mut txn);
                        let pos = text_ref.len(&txn);
                        text_ref.insert_with_attributes(&mut txn, pos, &text, attrs);
                    }
                }
                Event::Code(s) => {
                    let Some(parent) = stack.last() else {
                        continue;
                    };
                    let mut marks_with_code = active_marks.clone();
                    marks_with_code.push(("code".into(), HashMap::new()));
                    let attrs = attrs_from_marks(&marks_with_code);
                    // If inside list_item, wrap in paragraph first.
                    let text_ref = if parent.tag().as_ref() == "list_item" {
                        let para = ensure_para_child(parent, &mut txn);
                        ensure_text_child(&para, &mut txn)
                    } else {
                        ensure_text_child(parent, &mut txn)
                    };
                    let pos = text_ref.len(&txn);
                    text_ref.insert_with_attributes(&mut txn, pos, &s, attrs);
                }
                Event::HardBreak => {
                    let Some(parent) = stack.last() else {
                        continue;
                    };
                    parent.push_back(&mut txn, XmlElementPrelim::empty("hard_break"));
                }
                Event::Rule => {
                    // CommonMark horizontal rule. Always lives at the top level.
                    frag.push_back(&mut txn, XmlElementPrelim::empty("horizontal_rule"));
                }
                Event::TaskListMarker(checked) => {
                    // GFM task list. pulldown-cmark emits this just after
                    // opening the surrounding `list_item`. Tag the item with
                    // `checked` so to_markdown can emit the right prefix and
                    // the editor can render the checkbox.
                    if let Some(item) = stack
                        .iter()
                        .rev()
                        .find(|el| el.tag().as_ref() == "list_item")
                    {
                        item.insert_attribute(
                            &mut txn,
                            "checked",
                            if checked { "true" } else { "false" },
                        );
                    }
                }
                Event::SoftBreak => {
                    // Special case: inside a blockquote paragraph, pulldown-cmark
                    // treats `> line1\n> line2` as a single paragraph with SoftBreak.
                    // But our schema / serializer uses separate paragraphs for each
                    // blockquote line. So split on SoftBreak here.
                    //
                    // Stack depth >= 2 and parent is "paragraph" inside "blockquote"
                    // → pop the current paragraph and push a new one.
                    let in_bq_para = stack.len() >= 2
                        && stack[stack.len() - 1].tag().as_ref() == "paragraph"
                        && stack[stack.len() - 2].tag().as_ref() == "blockquote";

                    if in_bq_para {
                        // End current paragraph, start new one inside the blockquote.
                        stack.pop();
                        let Some(bq) = stack.last() else { continue };
                        let new_para = bq.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                        stack.push(new_para);
                    } else if let Some(parent) = stack.last() {
                        // Outside blockquote: treat as a space.
                        if parent.tag().as_ref() != "code_block" {
                            let attrs = attrs_from_marks(&active_marks);
                            let text_ref = ensure_text_child(parent, &mut txn);
                            let pos = text_ref.len(&txn);
                            text_ref.insert_with_attributes(&mut txn, pos, " ", attrs);
                        }
                    }
                }
                Event::InlineHtml(s) => {
                    let trimmed = s.trim();
                    if trimmed.eq_ignore_ascii_case("<u>") {
                        active_marks.push(("underline".into(), HashMap::new()));
                    } else if trimmed.eq_ignore_ascii_case("</u>") {
                        // Pop only if the top is underline; defensive.
                        if let Some((k, _)) = active_marks.last()
                            && k == "underline"
                        {
                            active_marks.pop();
                        }
                    }
                    // Other inline HTML: ignore for v0.1.
                }
                Event::Html(_) => {
                    // Block-level HTML: ignore for v0.1.
                }
                _ => {}
            }
        }
        drop(txn);
    }

    let initial = YrsEngine
        .encode_state_as_update(&doc, None)
        .map_err(|e| ParseError::Encode(format!("{e:?}")))?;
    Ok((doc, initial))
}

/// Match `knot://board/<uuid>.svg` and return the captured UUID string.
fn match_board_sentinel(url: &str) -> Option<String> {
    let rest = url.strip_prefix(BOARD_URL_PREFIX)?;
    let id = rest.strip_suffix(BOARD_URL_SUFFIX)?;
    Uuid::parse_str(id).ok().map(|_| id.to_string())
}

/// Remove the last child of the current parent (fragment if stack is empty,
/// else the innermost element on the stack).  In debug builds, asserts that
/// the last child is an empty paragraph — the only shape we ever remove
/// (when reifying a sentinel image into an `excalidraw_board` block).
fn remove_last_child(
    frag: &yrs::XmlFragmentRef,
    stack: &[XmlElementRef],
    txn: &mut yrs::TransactionMut,
) {
    use yrs::XmlOut;
    match stack.last() {
        Some(parent) => {
            let len = parent.len(txn);
            if len > 0 {
                debug_assert!(
                    matches!(parent.get(txn, len - 1), Some(XmlOut::Element(ref el))
                        if el.tag().as_ref() == "paragraph" && el.len(txn) == 0),
                    "remove_last_child: expected empty paragraph as last child of parent",
                );
                parent.remove_range(txn, len - 1, 1);
            }
        }
        None => {
            let len = frag.len(txn);
            if len > 0 {
                debug_assert!(
                    matches!(frag.get(txn, len - 1), Some(XmlOut::Element(ref el))
                        if el.tag().as_ref() == "paragraph" && el.len(txn) == 0),
                    "remove_last_child: expected empty paragraph as last child of fragment",
                );
                frag.remove_range(txn, len - 1, 1);
            }
        }
    }
}

fn push_block(
    frag: &yrs::XmlFragmentRef,
    stack: &[XmlElementRef],
    txn: &mut yrs::TransactionMut,
    kind: &str,
    attrs: &[(&str, String)],
) -> XmlElementRef {
    let el = match stack.last() {
        Some(parent) => parent.push_back(txn, XmlElementPrelim::empty(kind)),
        None => frag.push_back(txn, XmlElementPrelim::empty(kind)),
    };
    for (k, v) in attrs {
        el.insert_attribute(txn, *k, v.clone());
    }
    el
}

/// Find the last XmlElement child of `parent` with tag "paragraph", or create one.
/// Used to wrap tight-list inline content in the required paragraph container.
fn ensure_para_child(parent: &XmlElementRef, txn: &mut yrs::TransactionMut) -> XmlElementRef {
    use yrs::XmlOut;
    let len = parent.len(txn);
    if len > 0
        && let Some(XmlOut::Element(el)) = parent.get(txn, len - 1)
        && el.tag().as_ref() == "paragraph"
    {
        return el;
    }
    parent.push_back(txn, XmlElementPrelim::empty("paragraph"))
}

/// Find the last XmlText child of `parent`, or create one if none.
/// Subsequent calls always return the same single XmlText so we can append.
fn ensure_text_child(parent: &XmlElementRef, txn: &mut yrs::TransactionMut) -> XmlTextRef {
    use yrs::XmlOut;
    let len = parent.len(txn);
    if len > 0
        && let Some(XmlOut::Text(t)) = parent.get(txn, len - 1)
    {
        return t;
    }
    parent.push_back(txn, XmlTextPrelim::new(""))
}

fn attrs_from_marks(marks: &[(String, HashMap<String, Any>)]) -> Attrs {
    let mut attrs: Attrs = Attrs::new();
    for (kind, kv) in marks {
        let v: Any = if kv.is_empty() {
            Any::Bool(true)
        } else {
            Any::Map(Arc::new(kv.clone()))
        };
        attrs.insert(Arc::from(kind.as_str()), v);
    }
    attrs
}

#[cfg(test)]
mod panic_resistance {
    use super::parse;

    /// Hand-picked pathological markdown inputs that historically (or
    /// plausibly) emit text/code/hardbreak events with an empty stack.
    /// These used to panic via `stack.last().unwrap()`; the dropping
    /// guards keep them well-formed (parse returns Ok, body may be empty).
    #[test]
    fn does_not_panic_on_pathological_inputs() {
        let inputs: &[&str] = &[
            "",
            "
",
            "   ",
            "[](knot://board/00000000-0000-0000-0000-000000000000.svg)",
            "![](knot://board/00000000-0000-0000-0000-000000000000.svg)",
            // Empty link text + unusual nesting
            "[]()",
            "**

_",
            // Bare backslashes / control chars
            "\\ {0} {1} {2}",
            // Just a hard break / soft break
            "  
",
        ];
        for md in inputs {
            // Each call must return — panic is the failure mode under test.
            let _ = parse(md);
        }
    }
}
