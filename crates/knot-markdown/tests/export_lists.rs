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

/// A paragraph, image or table right below a list or quote goes on it only
/// when its last line is a paragraph's or a table row. After one ending in
/// a heading or code block nothing is open, so legacy's output reads back
/// as written and stays byte for byte as it was.
#[test]
fn lists_and_quotes_ending_in_a_closed_block_keep_their_bytes() {
    for level in [Any::from("2"), Any::Number(2.0)] {
        let heading = || ela("heading", vec![("level", level.clone())], vec![text("h")]);
        let code = || el("code_block", vec![text("c")]);
        let table = || {
            el(
                "table",
                vec![
                    el("table_row", vec![el("table_header", vec![p("h")])]),
                    el("table_row", vec![el("table_cell", vec![p("c")])]),
                ],
            )
        };
        let list_ending_in_code = || bullets(vec![item(vec![p("b"), code()])]);
        for (what, blocks, want) in [
            (
                "a paragraph after a list ending in a code block, in a quote",
                vec![quote(vec![list_ending_in_code(), p("para")])],
                "> - b\n>   ```\n>   c\n>   ```\n> para\n",
            ),
            (
                "a paragraph after a list ending in a code block, in an item",
                vec![bullets(vec![item(vec![
                    p("x"),
                    list_ending_in_code(),
                    p("para"),
                ])])],
                "- x\n  - b\n    ```\n    c\n    ```\n  para\n",
            ),
            (
                "a paragraph after a quoted heading, in a quote",
                vec![quote(vec![quote(vec![heading()]), p("para")])],
                "> > ## h\n> para\n",
            ),
            (
                "a paragraph after a quote ending in a code block, in a quote",
                vec![quote(vec![quote(vec![p("q"), code()]), p("para")])],
                "> > q\n> > ```\n> > c\n> > ```\n> para\n",
            ),
            (
                "a paragraph after a quote ending in a code block, in an item",
                vec![bullets(vec![item(vec![
                    p("x"),
                    quote(vec![p("q"), code()]),
                    p("para"),
                ])])],
                "- x\n  > q\n  > ```\n  > c\n  > ```\n  para\n",
            ),
            (
                "a paragraph after a list ending in a heading, in an item",
                vec![bullets(vec![item(vec![
                    p("x"),
                    bullets(vec![item(vec![p("b"), heading()])]),
                    p("para"),
                ])])],
                "- x\n  - b\n    ## h\n  para\n",
            ),
            (
                "an image after a list ending in a code block, in a quote",
                vec![quote(vec![list_ending_in_code(), image("u", "x")])],
                "> - b\n>   ```\n>   c\n>   ```\n> ![x](u)\n",
            ),
            (
                "a table after a quoted heading, in an item",
                vec![bullets(vec![item(vec![
                    p("x"),
                    quote(vec![heading()]),
                    table(),
                ])])],
                "- x\n  > ## h\n  | h |\n  | --- |\n  | c |\n",
            ),
        ] {
            let doc = build(&blocks);
            let legacy_md = legacy::serialise(&YrsEngine, &doc).expect("legacy");
            assert_eq!(legacy_md, want, "{what}: legacy");
            assert_eq!(assert_round_trips(what, &doc), want, "{what}");
        }
    }
}

/// The blank lines a list ends with are left out of it, so a nested list
/// whose last item ends in an empty paragraph ends on that item's last
/// paragraph line, and a paragraph after it gets its blank line.
#[test]
fn a_paragraph_after_a_list_ending_in_an_empty_paragraph_stays_apart() {
    let empty = || el("paragraph", Vec::new());
    let doc = build(&[bullets(vec![item(vec![
        p("x"),
        bullets(vec![item(vec![p("b"), empty()])]),
        p("para"),
    ])])]);
    let md = export(&doc);
    assert_eq!(md, "- x\n  - b\n\n  para\n");
    // The importer drops an empty paragraph at the end of an item, as
    // before; the paragraph after the list stays a block of its own.
    let without_empty = build(&[bullets(vec![item(vec![
        p("x"),
        bullets(vec![item(vec![p("b")])]),
        p("para"),
    ])])]);
    assert_eq!(
        normalised(import(&md).inner()),
        normalised(without_empty.inner())
    );
    assert_eq!(
        assert_round_trips("the list without its empty paragraph", &without_empty),
        md
    );
}

/// The importer gives an item the old importer stored without its first
/// paragraph an empty one first (A7). When such an item's export changes
/// anyway (a blank line goes between two of its lines), its marker goes on a
/// line of its own, as that empty paragraph writes it, so the export is a
/// fixed point. One without blank lines keeps legacy's bytes.
#[test]
fn item_without_its_first_paragraph_gets_an_empty_first_line_before_blank_lines() {
    let empty = || el("paragraph", Vec::new());
    let table = || {
        el(
            "table",
            vec![
                el("table_row", vec![el("table_header", vec![p("h")])]),
                el("table_row", vec![el("table_cell", vec![p("c")])]),
            ],
        )
    };
    for (what, kids, bullet_md, ordered_md) in [
        (
            "two quotes",
            vec![quote(vec![p("q")]), quote(vec![p("q")])],
            "- \n  > q\n\n  > q\n",
            "3. \n   > q\n\n   > q\n",
        ),
        (
            "two tables",
            vec![table(), table()],
            "- \n  | h |\n  | --- |\n  | c |\n\n  | h |\n  | --- |\n  | c |\n",
            "3. \n   | h |\n   | --- |\n   | c |\n\n   | h |\n   | --- |\n   | c |\n",
        ),
    ] {
        let healed_kids = [vec![empty()], kids.clone()].concat();
        for (list, doc, healed, want) in [
            (
                "bullet",
                build(&[bullets(vec![item(kids.clone())])]),
                build(&[bullets(vec![item(healed_kids.clone())])]),
                bullet_md,
            ),
            (
                "ordered",
                build(&[ordered(3, vec![item(kids.clone())])]),
                build(&[ordered(3, vec![item(healed_kids.clone())])]),
                ordered_md,
            ),
        ] {
            let what = format!("{list} item of {what}");
            let md = export(&doc);
            assert_eq!(md, want, "{what}");
            assert_eq!(
                normalised(import(&md).inner()),
                normalised(healed.inner()),
                "{what}: {md:?} reads back as a different tree"
            );
            assert_eq!(assert_round_trips(&what, &healed), md, "{what}");
        }
    }

    let heading = || ela("heading", vec![("level", Any::from("2"))], vec![text("h")]);
    for (what, kids, want) in [
        ("a heading", vec![heading()], "- ## h\n"),
        (
            "a heading and a paragraph",
            vec![heading(), p("para")],
            "- ## h\n  para\n",
        ),
    ] {
        let doc = build(&[bullets(vec![item(kids)])]);
        let legacy_md = legacy::serialise(&YrsEngine, &doc).expect("legacy");
        assert_eq!(legacy_md, want, "{what}: legacy");
        assert_eq!(export(&doc), want, "{what}");
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
