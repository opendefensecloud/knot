//! Markdown round-trip suite over fixtures.
//!
//! Each fixture file's contents are compared exactly. Tests construct a
//! Y.Doc with a small builder, serialize to Markdown via `to_markdown`,
//! and assert byte-equality.

use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

use knot_crdt::{DocHandle, Engine, YrsEngine};
use yrs::types::Attrs;
use yrs::{Any, Text, Transact, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim};

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
