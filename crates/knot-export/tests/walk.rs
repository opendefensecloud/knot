mod common;

use common::*;
use knot_export::{
    Block, DiagramRef, ImageSource, Inline, LinkTarget, List, ListItem, Marks, Run, blob_ids,
    diagrams, user_ids, walk,
};
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

fn para(text: &str) -> Block {
    Block::Paragraph(vec![Inline::Run(run(text))])
}

fn item(text: &str) -> N {
    el("list_item", vec![], vec![p(text)])
}

fn only_list(w: &knot_export::Walked) -> &List {
    match &w.doc.blocks[..] {
        [Block::List(l)] => l,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn ordered_list_start_from_number_or_string() {
    let w = walk(&build(vec![
        el(
            "ordered_list",
            vec![("start", Any::Number(5.0))],
            vec![item("a")],
        ),
        el("ordered_list", vec![("start", "3".into())], vec![item("b")]),
        el("ordered_list", vec![], vec![item("c")]),
    ]));
    let lists: Vec<(bool, u32)> = w
        .doc
        .blocks
        .iter()
        .map(|b| match b {
            Block::List(l) => (l.ordered, l.start),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(lists, vec![(true, 5), (true, 3), (true, 1)]);
}

#[test]
fn checked_accepts_bool_and_string() {
    let w = walk(&build(vec![el(
        "bullet_list",
        vec![],
        vec![
            el(
                "list_item",
                vec![("checked", Any::Bool(true))],
                vec![p("a")],
            ),
            el("list_item", vec![("checked", "false".into())], vec![p("b")]),
            item("c"),
        ],
    )]));
    let l = only_list(&w);
    assert!(!l.ordered);
    let checked: Vec<Option<bool>> = l.items.iter().map(|i| i.checked).collect();
    assert_eq!(checked, vec![Some(true), Some(false), None]);
}

#[test]
fn nested_mixed_lists_keep_structure() {
    let w = walk(&build(vec![el(
        "bullet_list",
        vec![],
        vec![el(
            "list_item",
            vec![],
            vec![
                p("top"),
                el(
                    "ordered_list",
                    vec![("start", Any::Number(2.0))],
                    vec![el(
                        "list_item",
                        vec![],
                        vec![
                            p("mid"),
                            el(
                                "bullet_list",
                                vec![],
                                vec![
                                    el("list_item", vec![("checked", "true".into())], vec![p("x")]),
                                    el("list_item", vec![("checked", "no".into())], vec![p("y")]),
                                ],
                            ),
                        ],
                    )],
                ),
            ],
        )],
    )]));
    assert_eq!(
        w.doc.blocks,
        vec![Block::List(List {
            ordered: false,
            start: 1,
            items: vec![ListItem {
                checked: None,
                blocks: vec![
                    para("top"),
                    Block::List(List {
                        ordered: true,
                        start: 2,
                        items: vec![ListItem {
                            checked: None,
                            blocks: vec![
                                para("mid"),
                                Block::List(List {
                                    ordered: false,
                                    start: 1,
                                    items: vec![
                                        ListItem {
                                            checked: Some(true),
                                            blocks: vec![para("x")]
                                        },
                                        ListItem {
                                            checked: Some(false),
                                            blocks: vec![para("y")]
                                        },
                                    ],
                                }),
                            ],
                        }],
                    }),
                ],
            }],
        })]
    );
    assert!(w.unknown_elements.is_empty());
}

#[test]
fn text_directly_under_list_item_is_wrapped() {
    let w = walk(&build(vec![el(
        "bullet_list",
        vec![],
        vec![el(
            "list_item",
            vec![],
            vec![t("a"), el("hard_break", vec![], vec![]), t("b")],
        )],
    )]));
    assert_eq!(
        only_list(&w).items,
        vec![ListItem {
            checked: None,
            blocks: vec![Block::Paragraph(vec![
                Inline::Run(run("a")),
                Inline::Break,
                Inline::Run(run("b")),
            ])],
        }]
    );
    assert!(w.unknown_elements.is_empty());
}

#[test]
fn text_under_blockquote_is_wrapped() {
    let w = walk(&build(vec![el(
        "blockquote",
        vec![],
        vec![t("loose"), p("real")],
    )]));
    assert_eq!(
        w.doc.blocks,
        vec![Block::Quote(vec![para("loose"), para("real")])]
    );
    assert!(w.unknown_elements.is_empty());
}

const BLOB_A: &str = "11111111-2222-4333-8444-555555555555";
const BLOB_B: &str = "66666666-7777-4888-8999-aaaaaaaaaaaa";

fn image(src: &str) -> N {
    el(
        "image",
        vec![("src", src.into()), ("alt", "pic".into())],
        vec![],
    )
}

fn mermaid(source: &str) -> N {
    el(
        "code_block",
        vec![("language", "mermaid".into())],
        vec![t(source)],
    )
}

fn board(id: &str) -> N {
    el("excalidraw_board", vec![("board_id", id.into())], vec![])
}

#[test]
fn image_sources_are_classified() {
    let blob = format!("/api/blobs/{BLOB_A}");
    let w = walk(&build(vec![
        image(&blob),
        image("https://x/y.png"),
        image("http://x/y.png"),
        image("data:image/png;base64,AA"),
        image("img/a.png"),
    ]));
    let sources: Vec<ImageSource> = w
        .doc
        .blocks
        .iter()
        .map(|b| match b {
            Block::Image { source, alt } => {
                assert_eq!(alt, "pic");
                source.clone()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        sources,
        vec![
            ImageSource::Blob(BLOB_A.into()),
            ImageSource::External("https://x/y.png".into()),
            ImageSource::External("http://x/y.png".into()),
            ImageSource::Unsupported,
            ImageSource::Unsupported,
        ]
    );
    assert!(w.unknown_elements.is_empty());
}

#[test]
fn board_and_attachment_attributes() {
    let w = walk(&build(vec![
        board("b1"),
        el(
            "excalidraw_board",
            vec![("board_id", "b2".into()), ("label", "Flow".into())],
            vec![],
        ),
        el(
            "attachment",
            vec![
                ("url", "/api/blobs/x".into()),
                ("name", "a.pdf".into()),
                ("size", Any::Number(2048.0)),
            ],
            vec![],
        ),
        el(
            "attachment",
            vec![
                ("url", "/api/blobs/y".into()),
                ("name", "b.pdf".into()),
                ("size", Any::Number(0.0)),
            ],
            vec![],
        ),
    ]));
    assert_eq!(
        w.doc.blocks,
        vec![
            Block::Board {
                key: "board-b1".into(),
                board_id: "b1".into(),
                label: "Diagram".into()
            },
            Block::Board {
                key: "board-b2".into(),
                board_id: "b2".into(),
                label: "Flow".into()
            },
            Block::Attachment {
                url: "/api/blobs/x".into(),
                name: "a.pdf".into(),
                size: Some(2048)
            },
            Block::Attachment {
                url: "/api/blobs/y".into(),
                name: "b.pdf".into(),
                size: None
            },
        ]
    );
}

fn cell_of(children: Vec<N>) -> N {
    el(
        "table",
        vec![],
        vec![el(
            "table_row",
            vec![],
            vec![el("table_cell", vec![], children)],
        )],
    )
}

#[test]
fn diagrams_are_listed_once_in_order() {
    let w = walk(&build(vec![
        mermaid("graph TD; A-->B"),
        mermaid("graph TD; A-->B"),
        cell_of(vec![board("b1"), board("b1")]),
    ]));
    let key = format!("mermaid-{:x}", Sha256::digest(b"graph TD; A-->B"));
    assert_eq!(
        diagrams(&w.doc),
        vec![
            DiagramRef::Mermaid {
                key,
                source: "graph TD; A-->B".into()
            },
            DiagramRef::Board {
                key: "board-b1".into(),
                board_id: "b1".into(),
                label: "Diagram".into()
            },
        ]
    );
}

#[test]
fn blob_and_user_ids_are_collected_from_nested_blocks() {
    let w = walk(&build(vec![
        el(
            "bullet_list",
            vec![],
            vec![el(
                "list_item",
                vec![],
                vec![image(&format!("/api/blobs/{BLOB_B}"))],
            )],
        ),
        image(&format!("/api/blobs/{BLOB_A}")),
        image(&format!("/api/blobs/{BLOB_B}")),
        image("https://x/y.png"),
        cell_of(vec![el(
            "paragraph",
            vec![],
            vec![linked("@u", &format!("knot://user/{USER_ID}"))],
        )]),
        el(
            "paragraph",
            vec![],
            vec![linked("@u", &format!("knot://user/{USER_ID}"))],
        ),
    ]));
    assert_eq!(
        blob_ids(&w.doc),
        vec![BLOB_B.to_string(), BLOB_A.to_string()]
    );
    assert_eq!(user_ids(&w.doc), vec![USER_ID.to_string()]);
}

#[test]
fn unknown_block_element_degrades_and_is_reported() {
    let w = walk(&build(vec![el("callout", vec![], vec![p("hi")])]));
    assert_eq!(
        w.doc.blocks,
        vec![Block::Paragraph(vec![Inline::Run(run("hi"))])]
    );
    assert_eq!(w.unknown_elements, vec!["callout".to_string()]);
}
