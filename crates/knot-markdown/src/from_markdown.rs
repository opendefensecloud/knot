//! CommonMark → Y.Doc via pulldown-cmark.
//!
//! Targets the v0.1 schema only.

use std::collections::HashMap;
use std::sync::Arc;

use knot_crdt::{DocHandle, Engine, YrsEngine};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use thiserror::Error;
use yrs::types::Attrs;
use yrs::{
    Any, Text, Transact, Xml, XmlElementPrelim, XmlElementRef, XmlFragment, XmlTextPrelim,
    XmlTextRef,
};

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

        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        let parser = Parser::new_ext(src, opts);

        for event in parser {
            match event {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => {
                        let el = push_block(&frag, &stack, &mut txn, "paragraph", &[]);
                        stack.push(el);
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
                    TagEnd::Paragraph
                    | TagEnd::Heading(_)
                    | TagEnd::BlockQuote(_)
                    | TagEnd::CodeBlock
                    | TagEnd::List(_)
                    | TagEnd::Item => {
                        stack.pop();
                    }
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                        active_marks.pop();
                    }
                    _ => {}
                },
                Event::Text(s) => {
                    let text = s.to_string();
                    let parent_tag = stack
                        .last()
                        .expect("text outside container")
                        .tag()
                        .to_string();

                    // Code block bodies have no marks; append as plain text.
                    if parent_tag == "code_block" {
                        // pulldown-cmark includes a trailing newline in the code text;
                        // the to_markdown serializer adds its own newline before ```.
                        // Strip the trailing newline to avoid double-newline.
                        let text = text.strip_suffix('\n').unwrap_or(&text).to_string();
                        let parent = stack.last().unwrap();
                        let text_ref = ensure_text_child(parent, &mut txn);
                        let pos = text_ref.len(&txn);
                        text_ref.insert(&mut txn, pos, &text);
                    } else if parent_tag == "list_item" {
                        // Tight list: pulldown-cmark emits Text directly inside Item
                        // (no paragraph wrapper). Our schema requires list_item → paragraph.
                        let para = ensure_para_child(stack.last().unwrap(), &mut txn);
                        let attrs = attrs_from_marks(&active_marks);
                        let text_ref = ensure_text_child(&para, &mut txn);
                        let pos = text_ref.len(&txn);
                        text_ref.insert_with_attributes(&mut txn, pos, &text, attrs);
                    } else {
                        // Inline text in a paragraph/heading.
                        // Attach current active marks as Attrs.
                        let attrs = attrs_from_marks(&active_marks);
                        let parent = stack.last().unwrap();
                        let text_ref = ensure_text_child(parent, &mut txn);
                        let pos = text_ref.len(&txn);
                        text_ref.insert_with_attributes(&mut txn, pos, &text, attrs);
                    }
                }
                Event::Code(s) => {
                    // Inline code becomes a "code" mark.
                    let parent = stack.last().expect("code outside container");
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
                    let parent = stack.last().expect("hard_break outside container");
                    parent.push_back(&mut txn, XmlElementPrelim::empty("hard_break"));
                }
                Event::Rule => {
                    // CommonMark horizontal rule. Always lives at the top level.
                    frag.push_back(&mut txn, XmlElementPrelim::empty("horizontal_rule"));
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
                        let bq = stack.last().expect("blockquote on stack");
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
