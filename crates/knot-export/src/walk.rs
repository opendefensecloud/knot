//! Walks the canonical "default" XmlFragment of a Y.Doc into an [`ExportDoc`].
//!
//! The walk never panics and never fails: anything it does not understand
//! becomes a plain paragraph and is reported in [`Walked::unknown_elements`].

use sha2::{Digest, Sha256};
use url::Url;
use yrs::types::text::YChange;
use yrs::{Any, Out, ReadTxn, Text, Transact, Xml, XmlFragment, XmlOut, XmlTextRef};

use crate::model::{Block, ExportDoc, Inline, LinkTarget, Marks, Run};

/// Result of walking a document.
#[derive(Debug, Clone, PartialEq)]
pub struct Walked {
    pub doc: ExportDoc,
    /// Tags of block elements the walker did not recognise, in document order.
    pub unknown_elements: Vec<String>,
}

pub fn walk(doc: &yrs::Doc) -> Walked {
    let txn = doc.transact();
    let mut unknown = Vec::new();
    let blocks = match txn.get_xml_fragment("default") {
        Some(frag) => walk_blocks(&frag, &txn, &mut unknown),
        None => Vec::new(),
    };
    Walked {
        doc: ExportDoc { blocks },
        unknown_elements: unknown,
    }
}

/// Read an attribute as a string. Handles the editor's numbers and the
/// import's strings alike.
fn attr<X: Xml, T: ReadTxn>(el: &X, txn: &T, name: &str) -> Option<String> {
    el.get_attribute(txn, name).map(|v| v.to_string(txn))
}

fn walk_blocks<F: XmlFragment, T: ReadTxn>(
    parent: &F,
    txn: &T,
    unknown: &mut Vec<String>,
) -> Vec<Block> {
    let mut out = Vec::new();
    for i in 0..parent.len(txn) {
        match parent.get(txn, i) {
            Some(XmlOut::Element(el)) => out.push(walk_element(&el, txn, unknown)),
            Some(XmlOut::Text(t)) => out.push(Block::Paragraph(text_inlines(&t, txn))),
            Some(XmlOut::Fragment(f)) => out.extend(walk_blocks(&f, txn, unknown)),
            None => {}
        }
    }
    out
}

fn walk_element<T: ReadTxn>(el: &yrs::XmlElementRef, txn: &T, unknown: &mut Vec<String>) -> Block {
    let tag = el.tag().to_string();
    match tag.as_str() {
        "paragraph" => Block::Paragraph(inlines(el, txn)),
        "heading" => {
            let level = attr(el, txn, "level")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .map(|n| n.round().clamp(1.0, 6.0) as u8)
                .unwrap_or(1);
            Block::Heading {
                level,
                content: inlines(el, txn),
            }
        }
        "blockquote" => Block::Quote(walk_blocks(el, txn, unknown)),
        "code_block" => {
            let language = attr(el, txn, "language").unwrap_or_default();
            let text = flatten_text(el, txn);
            if language.trim().eq_ignore_ascii_case("mermaid") {
                let key = format!("mermaid-{:x}", Sha256::digest(text.as_bytes()));
                Block::Mermaid { key, source: text }
            } else {
                Block::Code { language, text }
            }
        }
        "horizontal_rule" => Block::Rule,
        _ => {
            unknown.push(tag);
            Block::Paragraph(vec![Inline::Run(Run {
                text: flatten_text(el, txn),
                marks: Marks::default(),
            })])
        }
    }
}

/// Inline content of a textblock. Unknown inline elements keep their text.
fn inlines<F: XmlFragment, T: ReadTxn>(parent: &F, txn: &T) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut last_href: Option<String> = None;
    for i in 0..parent.len(txn) {
        match parent.get(txn, i) {
            Some(XmlOut::Text(t)) => push_text(&mut out, &mut last_href, &t, txn),
            Some(XmlOut::Element(el)) => {
                last_href = None;
                if el.tag().as_ref() == "hard_break" {
                    out.push(Inline::Break);
                } else {
                    let text = flatten_text(&el, txn);
                    if !text.is_empty() {
                        out.push(Inline::Run(Run {
                            text,
                            marks: Marks::default(),
                        }));
                    }
                }
            }
            Some(XmlOut::Fragment(f)) => {
                last_href = None;
                out.extend(inlines(&f, txn));
            }
            None => {}
        }
    }
    out
}

fn text_inlines<T: ReadTxn>(t: &XmlTextRef, txn: &T) -> Vec<Inline> {
    let mut out = Vec::new();
    push_text(&mut out, &mut None, t, txn);
    out
}

fn push_text<T: ReadTxn>(
    out: &mut Vec<Inline>,
    last_href: &mut Option<String>,
    t: &XmlTextRef,
    txn: &T,
) {
    for d in t.diff(txn, YChange::identity) {
        let text = match d.insert {
            Out::Any(Any::String(s)) => s.to_string(),
            _ => continue,
        };
        let attrs = d.attributes.as_deref();
        let has = |k: &str| attrs.is_some_and(|m| m.contains_key(k));
        let run = Run {
            text,
            marks: Marks {
                bold: has("bold"),
                italic: has("italic"),
                underline: has("underline"),
                strike: has("strike"),
                code: has("code"),
            },
        };
        let href = attrs.and_then(|m| match m.get("link") {
            Some(Any::Map(link)) => match link.get("href") {
                Some(Any::String(s)) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        });
        match href {
            None => {
                *last_href = None;
                out.push(Inline::Run(run));
            }
            Some(h) => {
                if last_href.as_deref() == Some(h.as_str())
                    && let Some(Inline::Link { runs, .. }) = out.last_mut()
                {
                    runs.push(run);
                } else {
                    out.push(Inline::Link {
                        target: classify(&h),
                        runs: vec![run],
                    });
                    *last_href = Some(h);
                }
            }
        }
    }
}

fn classify(href: &str) -> LinkTarget {
    if let Some(rest) = href.strip_prefix("knot://doc/") {
        return if uuid::Uuid::parse_str(rest).is_ok() {
            LinkTarget::Doc(rest.to_owned())
        } else {
            LinkTarget::Plain
        };
    }
    if let Some(rest) = href.strip_prefix("knot://user/") {
        return if uuid::Uuid::parse_str(rest).is_ok() {
            LinkTarget::User(rest.to_owned())
        } else {
            LinkTarget::Plain
        };
    }
    match Url::parse(href) {
        Ok(u) if matches!(u.scheme(), "http" | "https" | "mailto") => {
            LinkTarget::Web(u.as_str().to_owned())
        }
        _ => LinkTarget::Plain,
    }
}

/// All plain text below `node`, concatenated (mark-free).
fn flatten_text<F: XmlFragment, T: ReadTxn>(node: &F, txn: &T) -> String {
    let mut s = String::new();
    for i in 0..node.len(txn) {
        match node.get(txn, i) {
            Some(XmlOut::Text(t)) => {
                for d in t.diff(txn, YChange::identity) {
                    if let Out::Any(Any::String(x)) = d.insert {
                        s.push_str(&x);
                    }
                }
            }
            Some(XmlOut::Element(e)) => s.push_str(&flatten_text(&e, txn)),
            Some(XmlOut::Fragment(f)) => s.push_str(&flatten_text(&f, txn)),
            None => {}
        }
    }
    s
}
