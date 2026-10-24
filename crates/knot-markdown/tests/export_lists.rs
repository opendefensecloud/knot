//! The serializer writes nested and multi-block list items and blockquotes so
//! that they read back as the same tree (spec A4, A5, A6, A8, A9), and keeps
//! the shapes that round-trip today byte for byte (P2).

mod common;

use common::{N, build, el, ela, p, text};
use knot_crdt::{DocHandle, YrsEngine};
use knot_markdown::legacy::{self, compat::normalised};
use yrs::Any;

fn export(doc: &DocHandle) -> String {
    knot_markdown::serialise(&YrsEngine, doc).expect("serialise")
}

fn import(md: &str) -> DocHandle {
    let (doc, _) = knot_markdown::from_markdown::parse(md).expect("parse");
    doc
}

/// Exports `doc`, asserts that the export reads back as the same tree and is
/// a fixed point, and returns it.
fn assert_round_trips(what: &str, doc: &DocHandle) -> String {
    let md = export(doc);
    let back = import(&md);
    assert_eq!(
        normalised(back.inner()),
        normalised(doc.inner()),
        "{what}: {md:?} reads back as a different tree"
    );
    assert_eq!(export(&back), md, "{what}: {md:?} is not a fixed point");
    md
}

/// How the `checked` attribute is stored: a boolean by the editor, a string
/// by the importer. The output must not depend on it.
const CHECKED: [fn(bool) -> Any; 2] = [Any::Bool, |b| Any::from(b.to_string())];

fn bullets(items: Vec<N>) -> N {
    el("bullet_list", items)
}

fn ordered(start: i64, items: Vec<N>) -> N {
    ela(
        "ordered_list",
        vec![("start", Any::Number(start as f64))],
        items,
    )
}

fn item(kids: Vec<N>) -> N {
    el("list_item", kids)
}

fn task(checked: Any, kids: Vec<N>) -> N {
    ela("list_item", vec![("checked", checked)], kids)
}

fn quote(kids: Vec<N>) -> N {
    el("blockquote", kids)
}

fn image(src: &str, alt: &str) -> N {
    ela(
        "image",
        vec![("src", Any::from(src)), ("alt", Any::from(alt))],
        Vec::new(),
    )
}

fn rule() -> N {
    el("horizontal_rule", Vec::new())
}

#[test]
fn nested_checklist_indents_by_marker_width() {
    for checked in CHECKED {
        let doc = build(&[bullets(vec![task(
            checked(false),
            vec![
                p("a"),
                bullets(vec![task(
                    checked(true),
                    vec![p("b"), bullets(vec![task(checked(false), vec![p("c")])])],
                )]),
            ],
        )])]);
        let md = assert_round_trips("nested checklist", &doc);
        assert_eq!(md, "- [ ] a\n  - [x] b\n    - [ ] c\n");
    }
}

#[test]
fn ordered_items_keep_checkboxes() {
    let md = "1. [ ] a\n2. [x] b\n";
    assert_eq!(export(&import(md)), md);

    for checked in CHECKED {
        let doc = build(&[ordered(
            1,
            vec![
                task(checked(false), vec![p("a")]),
                task(checked(true), vec![p("b")]),
            ],
        )]);
        assert_eq!(assert_round_trips("ordered checklist", &doc), md);

        // Content under a wide marker is indented by the marker's width.
        let doc = build(&[ordered(
            9,
            vec![
                task(checked(true), vec![p("a")]),
                task(
                    checked(false),
                    vec![p("b"), bullets(vec![item(vec![p("c")])])],
                ),
            ],
        )]);
        assert_eq!(
            assert_round_trips("wide ordered checklist", &doc),
            "9. [x] a\n10. [ ] b\n    - c\n"
        );
    }
}

/// A checkbox opens the item's first paragraph. In front of another block it
/// would turn that block into text, so an item the old importer stored
/// without its paragraph (it starts with a heading or a code block) gets its
/// checkbox on a line of its own, which reads back as the empty first
/// paragraph the importer adds (A7).
#[test]
fn checkbox_before_another_block_gets_a_line_of_its_own() {
    let empty = || el("paragraph", Vec::new());
    let heading = || ela("heading", vec![("level", Any::from("2"))], vec![text("h")]);
    let code = || el("code_block", vec![text("c")]);
    for checked in CHECKED {
        for (what, blocks, healed, want) in [
            (
                "a task item starting with a heading",
                vec![bullets(vec![task(checked(true), vec![heading()])])],
                vec![bullets(vec![task(checked(true), vec![empty(), heading()])])],
                "- [x] \n  ## h\n",
            ),
            (
                "an ordered task item starting with a code block",
                vec![ordered(1, vec![task(checked(false), vec![code()])])],
                vec![ordered(
                    1,
                    vec![task(checked(false), vec![empty(), code()])],
                )],
                "1. [ ] \n   ```\n   c\n   ```\n",
            ),
        ] {
            let md = export(&build(&blocks));
            assert_eq!(md, want, "{what}");
            let healed = build(&healed);
            assert_eq!(
                normalised(import(&md).inner()),
                normalised(healed.inner()),
                "{what}: {md:?} reads back as a different tree"
            );
            assert_eq!(assert_round_trips(what, &healed), md);
        }
    }
}

#[test]
fn list_item_with_several_blocks_round_trips() {
    let contents: Vec<(&str, Vec<N>)> = vec![
        ("two paragraphs", vec![p("a"), p("b")]),
        (
            "a paragraph and an image",
            vec![p("a"), image("x.png", "x")],
        ),
        (
            "a paragraph and an ordered list starting at 3",
            vec![p("a"), ordered(3, vec![item(vec![p("o")])])],
        ),
        (
            "a paragraph after a nested list",
            vec![p("a"), bullets(vec![item(vec![p("b")])]), p("c")],
        ),
        (
            "a paragraph after a quote",
            vec![p("a"), quote(vec![p("q")]), p("c")],
        ),
    ];
    for (what, kids) in contents {
        let next = || item(vec![p("next")]);
        for (list, doc) in [
            (
                "bullet",
                build(&[bullets(vec![item(kids.clone()), next()])]),
            ),
            (
                "task",
                build(&[bullets(vec![task(Any::Bool(false), kids.clone()), next()])]),
            ),
            (
                "ordered",
                build(&[ordered(10, vec![item(kids.clone()), next()])]),
            ),
        ] {
            assert_round_trips(&format!("{list} item with {what}"), &doc);
        }
    }
}

#[test]
fn blockquote_list_then_paragraph_round_trips() {
    let doc = build(&[quote(vec![bullets(vec![item(vec![p("a")])]), p("b")])]);
    assert_eq!(
        assert_round_trips("quote with a list and a paragraph", &doc),
        "> - a\n>\n> b\n"
    );

    let doc = build(&[quote(vec![p("a"), ordered(3, vec![item(vec![p("o")])])])]);
    assert_round_trips("quote with a paragraph and an ordered list at 3", &doc);
}

#[test]
fn rule_in_item_or_quote_is_not_turned_into_a_heading() {
    for (what, doc) in [
        ("item", build(&[bullets(vec![item(vec![p("a"), rule()])])])),
        ("quote", build(&[quote(vec![p("a"), rule()])])),
    ] {
        assert_round_trips(&format!("rule after a paragraph in a {what}"), &doc);
    }
}

/// The shapes that round-trip today export exactly as the legacy serializer
/// writes them (P2): a quote's paragraph followed by a paragraph, an image,
/// a board or an attachment, and empty items with their trailing space.
#[test]
fn healthy_quote_and_empty_items_are_byte_identical() {
    let legacy_export = |doc: &DocHandle| legacy::serialise(&YrsEngine, doc).expect("legacy");
    for md in [
        "> a\n> b\n",
        "> a\n> ![x](u)\n",
        "- \n",
        "- [ ] \n",
        "2. \n",
    ] {
        let doc = import(md);
        assert_eq!(export(&doc), md);
        assert_eq!(export(&doc), legacy_export(&doc), "{md:?}");
    }

    let board = ela(
        "excalidraw_board",
        vec![(
            "board_id",
            Any::from("11111111-2222-3333-4444-555555555555"),
        )],
        Vec::new(),
    );
    let attachment = ela(
        "attachment",
        vec![
            (
                "url",
                Any::from("/api/blobs/22222222-3333-4444-5555-666666666666"),
            ),
            ("name", Any::from("f.pdf")),
        ],
        Vec::new(),
    );
    for (what, block, md) in [
        ("an image", image("u", "x"), "> a\n> ![x](u)\n"),
        (
            "a board",
            board,
            "> a\n> ![Diagram](knot://board/11111111-2222-3333-4444-555555555555.svg)\n",
        ),
        (
            "an attachment",
            attachment,
            "> a\n> [f.pdf](/api/blobs/22222222-3333-4444-5555-666666666666)\n",
        ),
    ] {
        let doc = build(&[quote(vec![p("a"), block])]);
        assert_eq!(export(&doc), md, "a quote's paragraph and {what}");
        assert_eq!(export(&doc), legacy_export(&doc), "{what}");
    }
}
