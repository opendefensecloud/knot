mod common;

use common::*;
use knot_export::{Block, Inline, LinkTarget, Marks, Run, walk};
use sha2::{Digest, Sha256};
use yrs::Any;

const DOC_ID: &str = "0b1c2d3e-4f50-4162-8374-8596a7b8c9d0";
const USER_ID: &str = "7a6b5c4d-3e2f-4101-9283-746556473829";

fn run(text: &str) -> Run {
    Run {
        text: text.into(),
        marks: Marks::default(),
    }
}

fn single_link(href: &str) -> Inline {
    let w = walk(&build(vec![el(
        "paragraph",
        vec![],
        vec![linked("x", href)],
    )]));
    match &w.doc.blocks[..] {
        [Block::Paragraph(i)] if i.len() == 1 => i[0].clone(),
        other => panic!("unexpected {other:?}"),
    }
}

fn target(href: &str) -> LinkTarget {
    match single_link(href) {
        Inline::Link { target, .. } => target,
        other => panic!("not a link: {other:?}"),
    }
}

#[test]
fn empty_doc_walks_to_no_blocks() {
    assert_eq!(walk(&build(vec![])).doc.blocks, vec![]);
}

#[test]
fn headings_accept_number_and_string_levels() {
    let w = walk(&build(vec![
        h(2, "a"),
        el("heading", vec![("level", "3".into())], vec![t("b")]),
        el("heading", vec![("level", "9".into())], vec![t("c")]),
    ]));
    let levels: Vec<u8> = w
        .doc
        .blocks
        .iter()
        .map(|b| match b {
            Block::Heading { level, .. } => *level,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(levels, vec![2, 3, 6]);
    assert!(w.unknown_elements.is_empty());
}

#[test]
fn marks_are_read_by_key_presence() {
    let all = Marks {
        bold: true,
        italic: true,
        underline: true,
        strike: true,
        code: true,
    };
    let names = ["bold", "italic", "underline", "strike", "code"];
    let bools = N::Text(vec![(
        "y".into(),
        names.iter().map(|n| (*n, Any::Bool(true))).collect(),
    )]);
    let w = walk(&build(vec![el(
        "paragraph",
        vec![],
        vec![marked("x", &names), bools],
    )]));
    assert_eq!(
        w.doc.blocks,
        vec![Block::Paragraph(vec![
            Inline::Run(Run {
                text: "x".into(),
                marks: all.clone()
            }),
            Inline::Run(Run {
                text: "y".into(),
                marks: all
            }),
        ])]
    );
}

#[test]
fn link_targets_are_classified() {
    assert_eq!(
        target("https://a.example"),
        LinkTarget::Web("https://a.example/".into())
    );
    assert_eq!(target("mailto:x@y"), LinkTarget::Web("mailto:x@y".into()));
    assert_eq!(
        target(&format!("knot://doc/{DOC_ID}")),
        LinkTarget::Doc(DOC_ID.into())
    );
    assert_eq!(
        target(&format!("knot://user/{USER_ID}")),
        LinkTarget::User(USER_ID.into())
    );
    assert_eq!(
        target("knot://time/2026-10-23T10:00:00Z"),
        LinkTarget::Plain
    );
    assert_eq!(target("javascript:alert(1)"), LinkTarget::Plain);
    assert_eq!(target("/relative"), LinkTarget::Plain);
    assert_eq!(target("https://bad host/with space"), LinkTarget::Plain);
    assert_eq!(
        target("HTTPS://A.example"),
        LinkTarget::Web("https://a.example/".into())
    );
    assert_eq!(target("knot://doc/not-a-uuid"), LinkTarget::Plain);
    assert_eq!(target("knot://user/a b"), LinkTarget::Plain);
}

#[test]
fn adjacent_runs_with_same_href_merge() {
    let w = walk(&build(vec![el(
        "paragraph",
        vec![],
        vec![
            linked("a", "https://a.example"),
            N::Text(vec![(
                "b".into(),
                vec![
                    ("bold", Any::Bool(true)),
                    ("link", {
                        let mut m = std::collections::HashMap::new();
                        m.insert("href".to_owned(), Any::String("https://a.example".into()));
                        Any::Map(std::sync::Arc::new(m))
                    }),
                ],
            )]),
            linked("c", "https://other.example"),
        ],
    )]));
    let Block::Paragraph(inl) = &w.doc.blocks[0] else {
        panic!()
    };
    assert_eq!(inl.len(), 2);
    match &inl[0] {
        Inline::Link { runs, .. } => {
            assert_eq!(runs.len(), 2);
            assert_eq!(runs[0].text, "a");
            assert!(runs[1].marks.bold);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn code_quote_rule_and_hard_break() {
    let w = walk(&build(vec![
        el(
            "code_block",
            vec![("language", "rust".into())],
            vec![t("a\n\tb")],
        ),
        el("blockquote", vec![], vec![p("one"), p("two")]),
        el("horizontal_rule", vec![], vec![]),
        el(
            "paragraph",
            vec![],
            vec![t("a"), el("hard_break", vec![], vec![]), t("b")],
        ),
    ]));
    assert_eq!(
        w.doc.blocks,
        vec![
            Block::Code {
                language: "rust".into(),
                text: "a\n\tb".into()
            },
            Block::Quote(vec![
                Block::Paragraph(vec![Inline::Run(run("one"))]),
                Block::Paragraph(vec![Inline::Run(run("two"))]),
            ]),
            Block::Rule,
            Block::Paragraph(vec![
                Inline::Run(run("a")),
                Inline::Break,
                Inline::Run(run("b")),
            ]),
        ]
    );
    assert!(w.unknown_elements.is_empty());
}

#[test]
fn mermaid_key_hashes_plain_text_content() {
    let source = "graph TD; A-->B";
    let w = walk(&build(vec![el(
        "code_block",
        vec![("language", "mermaid".into())],
        vec![t("graph TD; "), marked("A-->B", &["bold"])],
    )]));
    assert_eq!(
        w.doc.blocks,
        vec![Block::Mermaid {
            key: format!("mermaid-{:x}", Sha256::digest(source.as_bytes())),
            source: source.into(),
        }]
    );
}

#[test]
fn unknown_block_becomes_paragraph_and_is_reported() {
    let w = walk(&build(vec![el(
        "mystery",
        vec![],
        vec![el("paragraph", vec![], vec![t("kept")])],
    )]));
    assert_eq!(
        w.doc.blocks,
        vec![Block::Paragraph(vec![Inline::Run(run("kept"))])]
    );
    assert_eq!(w.unknown_elements, vec!["mystery".to_string()]);
}
