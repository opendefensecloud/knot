//! Markdown round-trip suite over fixtures.
//!
//! Each fixture file's contents are compared exactly. Tests construct a
//! Y.Doc with a small builder, serialize to Markdown via `to_markdown`,
//! and assert byte-equality.

use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

use knot_crdt::{DocHandle, Engine, YrsEngine};
use yrs::types::Attrs;
use yrs::{
    Any, GetString, ReadTxn, Text, Transact, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim,
};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> String {
    let path = fixtures_dir().join(name);
    let mut s =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Builder helper. Each method appends to the "default" XmlFragment.
struct DocBuilder {
    engine: YrsEngine,
    doc: DocHandle,
}

impl DocBuilder {
    fn new() -> Self {
        let engine = YrsEngine;
        let doc = engine.new_doc();
        Self { engine, doc }
    }

    fn paragraph(self, text: &str) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(text));
        drop(txn);
        self
    }

    fn heading(self, level: u8, text: &str) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let h = frag.push_back(&mut txn, XmlElementPrelim::empty("heading"));
        h.insert_attribute(&mut txn, "level", level.to_string());
        h.push_back(&mut txn, XmlTextPrelim::new(text));
        drop(txn);
        self
    }

    fn to_markdown(&self) -> String {
        knot_markdown::to_markdown::serialise(&self.engine, &self.doc).expect("serialise")
    }
}

#[test]
fn paragraph_fixture() {
    let got = DocBuilder::new()
        .paragraph("hello world")
        .paragraph("second line")
        .to_markdown();
    assert_eq!(got, fixture("paragraph.md"));
}

#[test]
fn heading_fixture() {
    let got = DocBuilder::new()
        .heading(1, "one")
        .heading(2, "two")
        .heading(6, "six")
        .to_markdown();
    assert_eq!(got, fixture("headings.md"));
}

impl DocBuilder {
    fn blockquote(self, paras: &[&str]) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let bq = frag.push_back(&mut txn, XmlElementPrelim::empty("blockquote"));
        for p in paras {
            let pp = bq.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            pp.push_back(&mut txn, XmlTextPrelim::new(*p));
        }
        drop(txn);
        self
    }

    fn code_block(self, language: &str, code: &str) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let cb = frag.push_back(&mut txn, XmlElementPrelim::empty("code_block"));
        if !language.is_empty() {
            cb.insert_attribute(&mut txn, "language", language);
        }
        cb.push_back(&mut txn, XmlTextPrelim::new(code));
        drop(txn);
        self
    }

    fn hr(self) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        frag.push_back(&mut txn, XmlElementPrelim::empty("horizontal_rule"));
        drop(txn);
        self
    }

    fn hard_break_paragraph(self, parts: &[&str]) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        for (i, part) in parts.iter().enumerate() {
            p.push_back(&mut txn, XmlTextPrelim::new(*part));
            if i + 1 < parts.len() {
                p.push_back(&mut txn, XmlElementPrelim::empty("hard_break"));
            }
        }
        drop(txn);
        self
    }

    fn bullet_list(self, items: &[&str]) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let list = frag.push_back(&mut txn, XmlElementPrelim::empty("bullet_list"));
        for item in items {
            let li = list.push_back(&mut txn, XmlElementPrelim::empty("list_item"));
            let pp = li.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            pp.push_back(&mut txn, XmlTextPrelim::new(*item));
        }
        drop(txn);
        self
    }

    fn ordered_list(self, start: u32, items: &[&str]) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let list = frag.push_back(&mut txn, XmlElementPrelim::empty("ordered_list"));
        list.insert_attribute(&mut txn, "start", start.to_string());
        for item in items {
            let li = list.push_back(&mut txn, XmlElementPrelim::empty("list_item"));
            let pp = li.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            pp.push_back(&mut txn, XmlTextPrelim::new(*item));
        }
        drop(txn);
        self
    }
}

#[test]
fn blockquote_fixture() {
    let got = DocBuilder::new()
        .blockquote(&["first quoted line", "second line same block"])
        .paragraph("next paragraph")
        .to_markdown();
    assert_eq!(got, fixture("blockquote.md"));
}

#[test]
fn code_block_fixture() {
    let got = DocBuilder::new()
        .code_block("go", "package main\n\nfunc main() {}")
        .paragraph("after")
        .to_markdown();
    assert_eq!(got, fixture("code_block.md"));
}

#[test]
fn horizontal_rule_fixture() {
    let got = DocBuilder::new()
        .paragraph("before")
        .hr()
        .paragraph("after")
        .to_markdown();
    assert_eq!(got, fixture("horizontal_rule.md"));
}

#[test]
fn hard_break_fixture() {
    let got = DocBuilder::new()
        .hard_break_paragraph(&["line one", "line two"])
        .to_markdown();
    assert_eq!(got, fixture("hard_break.md"));
}

#[test]
fn lists_fixture() {
    let got = DocBuilder::new()
        .bullet_list(&["alpha", "beta", "gamma"])
        .ordered_list(1, &["one", "two", "three"])
        .to_markdown();
    assert_eq!(got, fixture("lists.md"));
}

/// `(mark_name, &[(attr_key, attr_val)])` pairs describing a single text run's marks.
type RunMark<'a> = (&'a str, &'a [(&'a str, &'a str)]);
/// One text run: `(text, &[marks])`.
type TextRun<'a> = (&'a str, &'a [RunMark<'a>]);

impl DocBuilder {
    /// Append a paragraph with multiple text runs, each optionally carrying formatting marks.
    ///
    /// `runs` is a slice of `(text, marks)` where `marks` is a slice of
    /// `(mark_name, attr_kv_pairs)`.  If `attr_kv_pairs` is empty the attribute
    /// value is `Any::Bool(true)`; otherwise it becomes an `Any::Map`.
    fn marked_para(self, runs: &[TextRun<'_>]) -> Self {
        let yrs_doc = self.doc.inner();
        let frag = yrs_doc.get_or_insert_xml_fragment("default");
        let mut txn = yrs_doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        let text_ref = p.push_back(&mut txn, XmlTextPrelim::new(""));
        for (s, marks) in runs {
            if marks.is_empty() {
                // Plain text — insert with empty Attrs so yrs keeps it as a
                // distinct (unformatted) run, preventing merging with adjacent
                // formatted runs.
                let pos = text_ref.len(&txn);
                text_ref.insert_with_attributes(&mut txn, pos, s, Attrs::new());
            } else {
                let mut attrs: Attrs = HashMap::new();
                for (mark_name, kv) in *marks {
                    let value: Any = if kv.is_empty() {
                        Any::Bool(true)
                    } else {
                        let mut obj: HashMap<String, Any> = HashMap::new();
                        for (k, v) in *kv {
                            obj.insert((*k).to_string(), Any::String(Arc::from(*v)));
                        }
                        Any::Map(Arc::new(obj))
                    };
                    attrs.insert(Arc::from(*mark_name), value);
                }
                let pos = text_ref.len(&txn);
                text_ref.insert_with_attributes(&mut txn, pos, s, attrs);
            }
        }
        drop(txn);
        self
    }
}

#[test]
fn round_trip_all_fixtures() {
    let fixtures = [
        "paragraph.md",
        "headings.md",
        "blockquote.md",
        "code_block.md",
        "horizontal_rule.md",
        "hard_break.md",
        "lists.md",
        "marks.md",
        "mixed.md",
        "boards.md",
        "tasklists.md",
        "images.md",
        "tables.md",
        "datetime.md",
    ];
    for name in fixtures {
        let raw = fixture(name);
        let (doc, _initial) = knot_markdown::from_markdown::parse(&raw)
            .unwrap_or_else(|e| panic!("parse {name}: {e:?}"));
        let got = knot_markdown::to_markdown::serialise(&YrsEngine, &doc)
            .unwrap_or_else(|e| panic!("serialise {name}: {e:?}"));
        assert_eq!(got, raw, "round-trip mismatch for {name}");
    }
}

#[test]
fn boards_sentinel_parses_to_excalidraw_board() {
    use yrs::XmlOut;
    let raw = fixture("boards.md");
    let (doc, _initial) = knot_markdown::from_markdown::parse(&raw).expect("parse boards");
    let yrs_doc = doc.inner();
    let txn = yrs_doc.transact();
    let frag = txn.get_xml_fragment("default").expect("default fragment");

    // Collect top-level (tag, board_id?, label?) of each child.
    let mut found: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    for i in 0..frag.len(&txn) {
        if let Some(XmlOut::Element(el)) = frag.get(&txn, i) {
            let tag = el.tag().to_string();
            let board_id = el.get_attribute(&txn, "board_id");
            let label = el.get_attribute(&txn, "label");
            found.push((tag, board_id, label));
        }
    }

    // Expect: paragraph, excalidraw_board(label=Some), excalidraw_board(label=None), paragraph.
    assert_eq!(found.len(), 4, "top-level child count: {found:?}");
    assert_eq!(found[0].0, "paragraph");
    assert_eq!(found[1].0, "excalidraw_board");
    assert_eq!(
        found[1].1.as_deref(),
        Some("11111111-2222-3333-4444-555555555555")
    );
    assert_eq!(found[1].2.as_deref(), Some("Architecture overview"));
    assert_eq!(found[2].0, "excalidraw_board");
    assert_eq!(
        found[2].1.as_deref(),
        Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
    );
    // Alt text was "Diagram" — stored as None per the sentinel rule.
    assert_eq!(found[2].2, None);
    assert_eq!(found[3].0, "paragraph");
}

/// A sentinel image embedded in a paragraph alongside other text MUST NOT
/// trigger the excalidraw_board promotion — the surrounding text must be
/// preserved.  (The non-sentinel image is silently dropped, matching v0.1
/// behaviour for any unsupported image.)
#[test]
fn boards_sentinel_in_mixed_paragraph_preserves_text() {
    use yrs::XmlOut;
    let raw = "before ![Diagram](knot://board/11111111-2222-3333-4444-555555555555.svg) after\n";
    let (doc, _initial) = knot_markdown::from_markdown::parse(raw).expect("parse mixed");
    let yrs_doc = doc.inner();
    let txn = yrs_doc.transact();
    let frag = txn.get_xml_fragment("default").expect("default fragment");

    // Expect exactly one top-level child: a paragraph (no excalidraw_board).
    assert_eq!(frag.len(&txn), 1, "expected single top-level paragraph");
    let Some(XmlOut::Element(el)) = frag.get(&txn, 0) else {
        panic!("expected element");
    };
    assert_eq!(el.tag().as_ref(), "paragraph");

    // Collect the text content of the paragraph.
    let mut text = String::new();
    for j in 0..el.len(&txn) {
        if let Some(XmlOut::Text(t)) = el.get(&txn, j) {
            text.push_str(&t.get_string(&txn));
        }
    }
    assert!(
        text.contains("before") && text.contains("after"),
        "surrounding text lost: {text:?}",
    );
    // The image's alt text must not leak into the paragraph — unrecognised
    // images are silently dropped in v0.1, including their alt text.
    assert!(
        !text.contains("Diagram"),
        "image alt text leaked into paragraph: {text:?}",
    );
}

/// Two sentinel images in the same paragraph: neither should be recognised
/// (the second image makes the paragraph not-only-this-image), and parsing
/// must not panic.
#[test]
fn boards_two_sentinels_one_paragraph_recognises_neither() {
    use yrs::XmlOut;
    let raw = concat!(
        "![A](knot://board/11111111-2222-3333-4444-555555555555.svg)",
        "![B](knot://board/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.svg)\n",
    );
    let (doc, _initial) = knot_markdown::from_markdown::parse(raw).expect("parse double");
    let yrs_doc = doc.inner();
    let txn = yrs_doc.transact();
    let frag = txn.get_xml_fragment("default").expect("default fragment");

    // No excalidraw_board should be produced.
    // Collect all paragraph text along the way to confirm no alt leak.
    let mut all_text = String::new();
    for i in 0..frag.len(&txn) {
        if let Some(XmlOut::Element(el)) = frag.get(&txn, i) {
            assert_ne!(
                el.tag().as_ref(),
                "excalidraw_board",
                "second sentinel image should disqualify the first",
            );
            for j in 0..el.len(&txn) {
                if let Some(XmlOut::Text(t)) = el.get(&txn, j) {
                    all_text.push_str(&t.get_string(&txn));
                }
            }
        }
    }
    // Neither image's alt text should leak into the paragraph.
    assert!(
        !all_text.contains('A') && !all_text.contains('B'),
        "sentinel alt text leaked into paragraph: {all_text:?}",
    );
}

/// A sentinel image with a non-default label, embedded inside mixed content,
/// must drop the alt label entirely rather than emitting it as paragraph text.
#[test]
fn boards_sentinel_with_label_in_mixed_paragraph_drops_alt() {
    use yrs::XmlOut;
    let raw = "see ![MyLabel](knot://board/11111111-2222-3333-4444-555555555555.svg) here\n";
    let (doc, _initial) = knot_markdown::from_markdown::parse(raw).expect("parse mixed");
    let yrs_doc = doc.inner();
    let txn = yrs_doc.transact();
    let frag = txn.get_xml_fragment("default").expect("default fragment");

    assert_eq!(frag.len(&txn), 1, "expected single top-level paragraph");
    let Some(XmlOut::Element(el)) = frag.get(&txn, 0) else {
        panic!("expected element");
    };
    assert_eq!(el.tag().as_ref(), "paragraph");

    let mut text = String::new();
    for j in 0..el.len(&txn) {
        if let Some(XmlOut::Text(t)) = el.get(&txn, j) {
            text.push_str(&t.get_string(&txn));
        }
    }
    assert!(text.contains("see") && text.contains("here"));
    assert!(
        !text.contains("MyLabel"),
        "non-default label leaked into paragraph: {text:?}",
    );
}

#[test]
fn marks_fixture() {
    let got = DocBuilder::new()
        .marked_para(&[
            ("bold", &[("bold", &[][..])][..]),
            (" ", &[][..]),
            ("italic", &[("italic", &[][..])][..]),
            (" ", &[][..]),
            ("code", &[("code", &[][..])][..]),
            (" ", &[][..]),
            ("strike", &[("strike", &[][..])][..]),
            (" ", &[][..]),
            ("underline", &[("underline", &[][..])][..]),
            (" ", &[][..]),
            (
                "text",
                &[(
                    "link",
                    &[("href", "https://ex.com"), ("title", "title")][..],
                )][..],
            ),
        ])
        .to_markdown();
    assert_eq!(got, fixture("marks.md"));
}
