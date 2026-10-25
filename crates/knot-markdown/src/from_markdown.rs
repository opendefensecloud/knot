//! CommonMark → Y.Doc via pulldown-cmark.
//!
//! Targets the v0.1 schema only.
//!
//! The importer follows Markdown only [`MAX_DEPTH`] block containers deep,
//! as the writer does ([`crate::to_markdown::MAX_DEPTH`]): a blockquote,
//! list or table nested that deep or deeper is stored as one paragraph of
//! its text ([`Flattened`]). yrs deletes a nested type by recursing once per
//! level, so a page stored from `>` repeated 100,000 times overflowed the
//! stack, and aborted the server, when it was replaced, restored or deleted
//! (F1 review). Every import goes through [`parse`]: the doc page's import,
//! from-template, the workspace import and history restore.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use knot_crdt::{DocHandle, Engine, YrsEngine};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use thiserror::Error;
use uuid::Uuid;
use yrs::types::Attrs;
use yrs::types::text::YChange;
use yrs::{
    Any, Out, ReadTxn, Text, Transact, TransactionMut, Xml, XmlElementPrelim, XmlElementRef,
    XmlFragment, XmlOut, XmlTextPrelim, XmlTextRef,
};

use crate::to_markdown::MAX_DEPTH;
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
        // Between Tag::Image and TagEnd::Image for an image or board in a
        // table cell, which becomes a block of the cell (`split_cell`).
        let mut cell_image: Option<ImageBlock> = None;
        // Whether the current paragraph of a table cell follows an image or
        // board of the cell.
        let mut cell_after_image = false;
        // Between Tag::HtmlBlock and TagEnd::HtmlBlock. Block-level HTML is
        // ignored, and so is the text pulldown-cmark emits inside it (the
        // block's extra indentation), which would otherwise land directly
        // in the enclosing list item or quote.
        let mut in_html_block = false;
        // Between the start and the end of a container nested `MAX_DEPTH`
        // deep, which is read as its text.
        let mut flattened: Option<Flattened> = None;

        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        opts.insert(Options::ENABLE_TABLES);
        let parser = TightParagraphs::new(Parser::new_ext(src, opts));

        // Per-table alignment carried by `Tag::Table(Vec<Alignment>)`. We
        // stash it on table entry and consume per-cell via a column index.
        let mut table_align: Vec<Alignment> = Vec::new();
        let mut in_table_header = false;
        let mut table_col_idx: usize = 0;

        for event in parser {
            if let Some(container) = flattened.as_mut() {
                if let Some(text) = container.read(&event) {
                    flattened = None;
                    if !text.is_empty() {
                        let p = push_block(&frag, &stack, &mut txn, "paragraph", &[]);
                        p.push_back(&mut txn, XmlTextPrelim::new(text));
                    }
                }
                continue;
            }
            if matches!(
                event,
                Event::Start(Tag::BlockQuote(_) | Tag::List(_) | Tag::Table(_))
            ) && container_depth(&stack) >= MAX_DEPTH
            {
                flattened = Some(Flattened::default());
                continue;
            }
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
                        // In a table cell, an image or board becomes a block of the
                        // cell, wherever it is on the cell's line; an image in its
                        // alt text is alt text. Nothing there is a candidate for
                        // promotion at TagEnd::Paragraph, which a cell never sees.
                        let in_cell = in_cell_paragraph(&stack);
                        if in_cell && image_depth == 0 {
                            cell_image = Some(ImageBlock::new(&dest_url, &title));
                        }
                        // Track every image (sentinel or not) so we can suppress its
                        // alt-text Text events from leaking into the paragraph.
                        image_depth = image_depth.saturating_add(1);
                        paragraph_image_count = paragraph_image_count.saturating_add(1);
                        // Sentinel branch: capture board id + alt buffer when the URL
                        // matches and the paragraph is currently empty.
                        if !in_cell
                            && pending_board.is_none()
                            && paragraph_sentinel.is_none()
                            && let Some(id) = match_board_sentinel(&dest_url)
                            && let Some(para) = stack.last()
                            && para.tag().as_ref() == "paragraph"
                            && para.len(&txn) == 0
                        {
                            pending_board = Some((id, String::new()));
                        } else if !in_cell
                            && pending_image.is_none()
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
                        // A cell is no paragraph: no promotion candidate goes in
                        // or out of it.
                        paragraph_image_count = 0;
                        paragraph_image = None;
                        paragraph_sentinel = None;
                        cell_after_image = false;
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
                    Tag::HtmlBlock => in_html_block = true,
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
                            let (tag, attrs) = ImageBlock::Board { board_id, alt }.block();
                            let _ = push_block(&frag, &stack, &mut txn, tag, &attrs);
                        }
                        // If the board branch didn't fire, try the regular-image branch.
                        else if let Some((src, alt, title)) = paragraph_image.take()
                            && let Some(p) = para.as_ref()
                            && p.len(&txn) == 0
                            && paragraph_image_count == 1
                        {
                            remove_last_child(&frag, &stack, &mut txn);
                            let (tag, attrs) = ImageBlock::Image { src, alt, title }.block();
                            let _ = push_block(&frag, &stack, &mut txn, tag, &attrs);
                        }
                        // Defensive: if we somehow exited the paragraph still holding
                        // an unfinished pending_board/pending_image (e.g. malformed
                        // input), discard so it can't leak into the next paragraph.
                        pending_board = None;
                        pending_image = None;
                        paragraph_image = None;
                    }
                    TagEnd::Item => {
                        // The schema is `list_item: paragraph block*`, so an
                        // empty item (`-`, `- [ ]`) or one starting with
                        // another block (`- # h`, an image) gets an empty
                        // paragraph first.
                        if let Some(item) = stack.pop()
                            && !matches!(
                                item.get(&txn, 0),
                                Some(yrs::XmlOut::Element(ref el)) if el.tag().as_ref() == "paragraph"
                            )
                        {
                            item.insert(&mut txn, 0, XmlElementPrelim::empty("paragraph"));
                        }
                    }
                    TagEnd::BlockQuote(_) => {
                        // The schema is `blockquote: block+`, so a quote without
                        // blocks (a lone `>`, or one holding only an HTML block,
                        // which is dropped) gets an empty paragraph, written as `>`.
                        if let Some(quote) = stack.pop()
                            && quote.len(&txn) == 0
                        {
                            quote.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                        }
                    }
                    TagEnd::CodeBlock => {
                        // The text ends with the line break before the closing
                        // fence, which the serializer writes back itself. In a list
                        // item or blockquote, and in an indented block, pulldown-cmark
                        // emits the text line by line, so only the end of the whole
                        // text is the fence's.
                        if let Some(code) = stack.pop()
                            && let Some(XmlOut::Text(t)) = code.get(&txn, 0)
                            && plain_text(&t, &txn).ends_with('\n')
                        {
                            let len = t.len(&txn);
                            t.remove_range(&mut txn, len - 1, 1);
                        }
                    }
                    TagEnd::Heading(_) | TagEnd::List(_) => {
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
                        // Pop the cell's last paragraph, then the cell.
                        if let Some(paragraph) = stack.pop()
                            && let Some(cell) = stack.last()
                        {
                            end_cell_paragraph(cell, &paragraph, &mut txn, cell_after_image, false);
                        }
                        stack.pop();
                        // Nothing of the cell is left for a later paragraph.
                        cell_image = None;
                        cell_after_image = false;
                        paragraph_image_count = 0;
                        paragraph_image = None;
                        paragraph_sentinel = None;
                    }
                    TagEnd::Image => {
                        image_depth = image_depth.saturating_sub(1);
                        if image_depth == 0
                            && let Some(image) = cell_image.take()
                        {
                            split_cell(&mut stack, &mut txn, image, cell_after_image);
                            cell_after_image = true;
                        }
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
                    TagEnd::HtmlBlock => in_html_block = false,
                    _ => {}
                },
                Event::Text(_) if in_html_block => {}
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
                        if let Some(image) = cell_image.as_mut() {
                            image.alt_mut().push_str(&text);
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

                    // Code block bodies have no marks; append as plain text. The
                    // line break before the closing fence goes at
                    // TagEnd::CodeBlock.
                    if parent_tag == "code_block" {
                        let text_ref = ensure_text_child(parent, &mut txn);
                        let pos = text_ref.len(&txn);
                        text_ref.insert(&mut txn, pos, &text);
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
                    // Code in the alt text of a table cell's image is alt text.
                    if let Some(image) = cell_image.as_mut() {
                        image.alt_mut().push_str(&s);
                        continue;
                    }
                    let Some(parent) = stack.last() else {
                        continue;
                    };
                    let mut marks_with_code = active_marks.clone();
                    marks_with_code.push(("code".into(), HashMap::new()));
                    let attrs = attrs_from_marks(&marks_with_code);
                    let text_ref = ensure_text_child(parent, &mut txn);
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
                    // CommonMark horizontal rule, written into its container:
                    // the top level, a list item or a blockquote.
                    let _ = push_block(&frag, &stack, &mut txn, "horizontal_rule", &[]);
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

/// How many block containers (blockquotes, list items, table cells) the
/// innermost open block sits in, or is: the depth at which a block opening
/// now sits, as the writer counts it.
fn container_depth(stack: &[XmlElementRef]) -> usize {
    stack
        .iter()
        .filter(|el| {
            matches!(
                el.tag().as_ref(),
                "blockquote" | "list_item" | "table_cell" | "table_header"
            )
        })
        .count()
}

/// A container nested [`MAX_DEPTH`] deep, read from its start to its end as
/// the paragraph of its text that the writer writes for such a container
/// (`capped_text`): its words in order, one space between each two, on one
/// line. Every character of its text is kept but its blanks: the text of
/// every block in it, code, and the alt text of images. Its structure,
/// marks, links' targets, rules, checkboxes and HTML go, as HTML does
/// elsewhere.
#[derive(Default)]
struct Flattened {
    /// The tags opened in the container and still open.
    open: usize,
    /// Between the start and the end of an HTML block, whose text is HTML.
    in_html_block: bool,
    text: String,
}

impl Flattened {
    /// Reads `event`, the next event in the container. Returns its text once
    /// the event ends the container.
    fn read(&mut self, event: &Event<'_>) -> Option<String> {
        // A block starts or ends, or a line breaks: the words on either side
        // are two.
        if matches!(role(event), Role::Open(_) | Role::Close | Role::Leaf)
            || matches!(event, Event::SoftBreak | Event::HardBreak)
        {
            self.text.push(' ');
        }
        match event {
            Event::Start(tag) => {
                self.in_html_block |= matches!(tag, Tag::HtmlBlock);
                self.open += 1;
            }
            Event::End(end) => {
                self.in_html_block &= !matches!(end, TagEnd::HtmlBlock);
                if self.open == 0 {
                    let words: Vec<&str> = self.text.split_whitespace().collect();
                    return Some(words.join(" "));
                }
                self.open -= 1;
            }
            Event::Text(text) if !self.in_html_block => self.text.push_str(text),
            Event::Code(code) => self.text.push_str(code),
            _ => {}
        }
        None
    }
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

/// An image or board sentinel the importer writes as a block of its own.
enum ImageBlock {
    /// An `excalidraw_board`: the board id, and the alt text, its label.
    Board { board_id: String, alt: String },
    /// An `image`.
    Image {
        src: String,
        alt: String,
        title: Option<String>,
    },
}

impl ImageBlock {
    /// The block for the image `Tag::Image` opens, its alt text still to
    /// come.
    fn new(dest_url: &str, title: &str) -> Self {
        match match_board_sentinel(dest_url) {
            Some(board_id) => ImageBlock::Board {
                board_id,
                alt: String::new(),
            },
            None => ImageBlock::Image {
                src: dest_url.to_string(),
                alt: String::new(),
                title: (!title.is_empty()).then(|| title.to_string()),
            },
        }
    }

    fn alt_mut(&mut self) -> &mut String {
        match self {
            ImageBlock::Board { alt, .. } | ImageBlock::Image { alt, .. } => alt,
        }
    }

    /// The block's tag and attributes. An empty alt text is left out, and so
    /// is a board's default label, which the serializer writes for a board
    /// without one.
    fn block(self) -> (&'static str, Vec<(&'static str, String)>) {
        match self {
            ImageBlock::Board { board_id, alt } => {
                let mut attrs = vec![("board_id", board_id)];
                if !alt.is_empty() && alt != DEFAULT_BOARD_LABEL {
                    attrs.push(("label", alt));
                }
                ("excalidraw_board", attrs)
            }
            ImageBlock::Image { src, alt, title } => {
                let mut attrs = vec![("src", src)];
                if !alt.is_empty() {
                    attrs.push(("alt", alt));
                }
                if let Some(t) = title {
                    attrs.push(("title", t));
                }
                ("image", attrs)
            }
        }
    }
}

/// Whether the innermost open block is the paragraph of a table cell.
fn in_cell_paragraph(stack: &[XmlElementRef]) -> bool {
    matches!(
        stack,
        [.., cell, paragraph]
            if paragraph.tag().as_ref() == "paragraph"
                && matches!(cell.tag().as_ref(), "table_cell" | "table_header")
    )
}

/// Splits the line of a table cell at `image`, one of its images or boards.
/// A GFM cell is one line, onto which the serializer joins the cell's blocks
/// with a space, so its images are blocks of their own: the cell's current
/// paragraph ends ([`end_cell_paragraph`]; `after_image` when it follows
/// another image), `image` becomes a block of the cell, and a new paragraph
/// opens for the rest of the line.
fn split_cell(
    stack: &mut Vec<XmlElementRef>,
    txn: &mut TransactionMut,
    image: ImageBlock,
    after_image: bool,
) {
    if !in_cell_paragraph(stack) {
        return;
    }
    let Some(paragraph) = stack.pop() else {
        return;
    };
    let Some(cell) = stack.last() else {
        return;
    };
    end_cell_paragraph(cell, &paragraph, txn, after_image, true);
    let (tag, attrs) = image.block();
    let el = cell.push_back(txn, XmlElementPrelim::empty(tag));
    for (k, v) in attrs {
        el.insert_attribute(txn, k, v);
    }
    let next = cell.push_back(txn, XmlElementPrelim::empty("paragraph"));
    stack.push(next);
}

/// Ends `paragraph`, the last block of table cell `cell`. Next to an image or
/// board of the cell, at its start when it follows one (`after_image`) and at
/// its end when one follows (`before_image`), the spaces and tabs are the
/// serializer's join and go, and the paragraph goes when no text is left. A
/// cell without images keeps its one paragraph as it is.
fn end_cell_paragraph(
    cell: &XmlElementRef,
    paragraph: &XmlElementRef,
    txn: &mut TransactionMut,
    after_image: bool,
    before_image: bool,
) {
    const BLANK: [char; 2] = [' ', '\t'];
    if after_image && let Some(XmlOut::Text(t)) = paragraph.get(txn, 0) {
        let text = plain_text(&t, txn);
        let n = (text.len() - text.trim_start_matches(BLANK).len()) as u32;
        if n > 0 {
            t.remove_range(txn, 0, n);
        }
    }
    if before_image
        && let Some(XmlOut::Text(t)) = paragraph.get(txn, paragraph.len(txn).saturating_sub(1))
    {
        let text = plain_text(&t, txn);
        let n = (text.len() - text.trim_end_matches(BLANK).len()) as u32;
        if n > 0 {
            let len = t.len(txn);
            t.remove_range(txn, len - n, n);
        }
    }
    let no_text = (0..paragraph.len(txn))
        .all(|i| matches!(paragraph.get(txn, i), Some(XmlOut::Text(t)) if t.len(txn) == 0));
    if no_text && (after_image || before_image) {
        let len = cell.len(txn);
        cell.remove_range(txn, len - 1, 1);
    }
}

/// The text of `t` without its marks.
fn plain_text<T: ReadTxn>(t: &XmlTextRef, txn: &T) -> String {
    t.diff(txn, YChange::identity)
        .into_iter()
        .filter_map(|chunk| match chunk.insert {
            Out::Any(Any::String(s)) => Some(s.to_string()),
            _ => None,
        })
        .collect()
}

/// Gives a tight list item's inline content its paragraph back.
///
/// pulldown-cmark leaves out the `Paragraph` tags of a tight list item and
/// emits its inline events straight under `Item`. The schema is
/// `list_item: paragraph block*`, so this adapter wraps each such *run* of
/// inline events in a paragraph: it opens one before the run's first event
/// and closes it before the next block-level event (a block start or end,
/// `Rule`, `Html`) or `End(Item)`. `TaskListMarker` passes through, so the
/// item keeps its checkbox. The importer then sees the same events as for a
/// loose list, image and board promotion included.
///
/// A run is wrapped only when it holds something the importer keeps: text,
/// code or a line break, or a lone image or board, which the importer
/// promotes to a block (only when it is alone in its paragraph, see
/// `paragraph_image_count`). A run of only inline HTML, empty links, or
/// images that share a line yields no paragraph, as with the legacy
/// importer. Otherwise an empty paragraph would follow the item's earlier
/// blocks, and its blank line would vanish on the next import. An item left
/// without a paragraph still gets one at `End(Item)`. Until the run shows
/// which it is, its events are held back; their order never changes.
struct TightParagraphs<'a, I: Iterator<Item = Event<'a>>> {
    inner: I,
    /// The open block-level tags, innermost last: `true` for an `Item`.
    open_blocks: Vec<bool>,
    /// Whether this adapter has opened a paragraph that is still open.
    in_paragraph: bool,
    /// The run directly in an `Item` that has no content yet.
    run: Run<'a>,
    /// Events ready to be returned, in order.
    out: VecDeque<Event<'a>>,
}

/// A run of inline events directly in an `Item`, held back while it has no
/// content.
#[derive(Default)]
struct Run<'a> {
    events: Vec<Event<'a>>,
    /// `Start(Image)` events in the run, nested ones included, as the
    /// importer counts them for promotion.
    images: usize,
    /// How many images are open: text inside one is alt text.
    image_depth: usize,
}

/// How an event relates to the paragraph structure.
enum Role {
    /// Inline content of a paragraph or heading.
    Inline(Inline),
    /// A block-level tag opens; `true` for an `Item`.
    Open(bool),
    /// A block-level tag closes.
    Close,
    /// A block-level event with no children (`Rule`, `Html`).
    Leaf,
    /// Passes through untouched.
    Marker,
}

/// What an inline event leaves in the importer's tree.
enum Inline {
    /// Text, unless it is an image's alt text.
    Text,
    /// Content wherever it is: code and line breaks, which the importer
    /// writes even inside an image. Events the importer's options never
    /// produce (math, footnote references) count as content too, so if they
    /// are ever enabled, whatever they write lands in a paragraph.
    Content,
    /// An image starts: promoted when it is alone in its paragraph.
    ImageStart,
    ImageEnd,
    /// Nothing of its own: a mark opening or closing, and inline HTML,
    /// which at most opens or closes the underline mark.
    Nothing,
}

impl<'a, I: Iterator<Item = Event<'a>>> TightParagraphs<'a, I> {
    fn new(inner: I) -> Self {
        Self {
            inner,
            open_blocks: Vec::new(),
            in_paragraph: false,
            run: Run::default(),
            out: VecDeque::new(),
        }
    }

    /// Ends the current run before a block-level event: closes the paragraph
    /// this adapter opened, or releases the held events, wrapped in a
    /// paragraph only when they hold a lone image or board.
    fn end_run(&mut self) {
        let run = std::mem::take(&mut self.run);
        if self.in_paragraph {
            self.in_paragraph = false;
            self.out.push_back(Event::End(TagEnd::Paragraph));
        } else if run.images == 1 {
            self.out.push_back(Event::Start(Tag::Paragraph));
            self.out.extend(run.events);
            self.out.push_back(Event::End(TagEnd::Paragraph));
        } else {
            self.out.extend(run.events);
        }
    }
}

/// Every pulldown-cmark event, tag and tag end is matched by name, so a new
/// one fails to compile here instead of being misread.
fn role(event: &Event<'_>) -> Role {
    match event {
        Event::Start(tag) => match tag {
            Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Superscript
            | Tag::Subscript
            | Tag::Link { .. } => Role::Inline(Inline::Nothing),
            Tag::Image { .. } => Role::Inline(Inline::ImageStart),
            Tag::Item => Role::Open(true),
            Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::MetadataBlock(_) => Role::Open(false),
        },
        Event::End(end) => match end {
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Link => Role::Inline(Inline::Nothing),
            TagEnd::Image => Role::Inline(Inline::ImageEnd),
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::MetadataBlock(_) => Role::Close,
        },
        Event::Text(_) => Role::Inline(Inline::Text),
        Event::Code(_)
        | Event::SoftBreak
        | Event::HardBreak
        | Event::InlineMath(_)
        | Event::DisplayMath(_)
        | Event::FootnoteReference(_) => Role::Inline(Inline::Content),
        Event::InlineHtml(_) => Role::Inline(Inline::Nothing),
        Event::Html(_) | Event::Rule => Role::Leaf,
        Event::TaskListMarker(_) => Role::Marker,
    }
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for TightParagraphs<'a, I> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        loop {
            if let Some(event) = self.out.pop_front() {
                return Some(event);
            }
            let Some(event) = self.inner.next() else {
                // pulldown-cmark closes every item, so a run never outlasts
                // the input; release one defensively all the same.
                self.end_run();
                return self.out.pop_front();
            };
            match role(&event) {
                Role::Marker if self.run.events.is_empty() => return Some(event),
                Role::Marker => self.run.events.push(event),
                Role::Inline(_) if self.in_paragraph || self.open_blocks.last() != Some(&true) => {
                    return Some(event);
                }
                Role::Inline(inline) => {
                    let run = &mut self.run;
                    let content = match inline {
                        Inline::Text => run.image_depth == 0,
                        Inline::Content => true,
                        Inline::ImageStart => {
                            run.images += 1;
                            run.image_depth += 1;
                            false
                        }
                        Inline::ImageEnd => {
                            run.image_depth = run.image_depth.saturating_sub(1);
                            false
                        }
                        Inline::Nothing => false,
                    };
                    if content {
                        let run = std::mem::take(&mut self.run);
                        self.in_paragraph = true;
                        self.out.push_back(Event::Start(Tag::Paragraph));
                        self.out.extend(run.events);
                        self.out.push_back(event);
                    } else {
                        self.run.events.push(event);
                    }
                }
                Role::Open(is_item) => {
                    self.end_run();
                    self.open_blocks.push(is_item);
                    self.out.push_back(event);
                }
                Role::Close => {
                    self.end_run();
                    self.open_blocks.pop();
                    self.out.push_back(event);
                }
                Role::Leaf => {
                    self.end_run();
                    self.out.push_back(event);
                }
            }
        }
    }
}

/// Find the last XmlText child of `parent`, or create one if none.
/// Subsequent calls always return the same single XmlText so we can append.
fn ensure_text_child(parent: &XmlElementRef, txn: &mut yrs::TransactionMut) -> XmlTextRef {
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
