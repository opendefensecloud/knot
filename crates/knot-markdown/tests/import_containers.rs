//! The importer keeps what a container or table cell holds (Task 4b). Each
//! of these lost content on every pass of a page through Markdown: history
//! restore, from-template, zip import (A10) and Markdown import.
//! - pulldown-cmark emits the text of a code block in a list item or
//!   blockquote line by line (an indented code block too), and the importer
//!   stripped the line break of every line, so `x\n\ny` came back as `xy`.
//! - A lone `>` imported as a blockquote without blocks, which the schema
//!   (`blockquote: block+`) forbids.
//! - An image or board in a table cell was dropped, and one that started a
//!   cell stayed behind as the image a later paragraph was given in place of
//!   its own.

mod common;

use common::{N, build, el, ela, p, text};
use knot_crdt::{DocHandle, YrsEngine};
use knot_markdown::legacy::compat::{normalised, schema_violations};
use yrs::Any;

const BOARD_ID: &str = "11111111-2222-3333-4444-555555555555";

fn export(doc: &DocHandle) -> String {
    knot_markdown::serialise(&YrsEngine, doc).expect("serialise")
}

fn import(md: &str) -> DocHandle {
    let (doc, _) = knot_markdown::from_markdown::parse(md).expect("parse");
    doc
}

/// Asserts that `blocks` export as `want`, and that `want` imports as
/// `blocks` again, schema-valid, and exports as `want` again.
fn assert_round_trips(what: &str, blocks: &[N], want: &str) {
    let doc = build(blocks);
    let md = export(&doc);
    assert_eq!(md, want, "{what}");
    let back = import(&md);
    assert_eq!(
        schema_violations(back.inner()),
        Vec::<String>::new(),
        "{what}: {md:?} imports as a page that breaks the schema"
    );
    assert_eq!(
        normalised(back.inner()),
        normalised(doc.inner()),
        "{what}: {md:?} reads back as another tree"
    );
    assert_eq!(export(&back), md, "{what}: {md:?} is not a fixed point");
}

/// Asserts that `md` imports as `blocks`, schema-valid.
fn assert_imports_as(md: &str, blocks: &[N]) {
    let doc = import(md);
    assert_eq!(
        schema_violations(doc.inner()),
        Vec::<String>::new(),
        "{md:?} imports as a page that breaks the schema"
    );
    assert_eq!(
        normalised(doc.inner()),
        normalised(build(blocks).inner()),
        "{md:?} imports as another tree"
    );
}

fn in_item(kids: Vec<N>) -> Vec<N> {
    vec![el("bullet_list", vec![el("list_item", kids)])]
}

fn quote(kids: Vec<N>) -> N {
    el("blockquote", kids)
}

fn code(body: &str) -> N {
    el("code_block", vec![text(body)])
}

fn image(name: &str) -> N {
    ela(
        "image",
        vec![
            ("src", Any::from(format!("{name}.png"))),
            ("alt", Any::from(name)),
        ],
        Vec::new(),
    )
}

/// A board with the default label, which is written as `Diagram` and not
/// stored.
fn board() -> N {
    ela(
        "excalidraw_board",
        vec![("board_id", Any::from(BOARD_ID))],
        Vec::new(),
    )
}

/// A table of a header row of `header` and, when there is one, a body row of
/// `body`, one cell per entry.
fn table(header: Vec<Vec<N>>, body: Option<Vec<Vec<N>>>) -> N {
    let row = |tag: &'static str, cells: Vec<Vec<N>>| {
        el(
            "table_row",
            cells.into_iter().map(|kids| el(tag, kids)).collect(),
        )
    };
    let mut rows = vec![row("table_header", header)];
    rows.extend(body.map(|cells| row("table_cell", cells)));
    el("table", rows)
}

#[test]
fn code_block_in_item_and_quote_keeps_line_breaks() {
    assert_round_trips(
        "in an item",
        &in_item(vec![p("a"), code("x\n\ny")]),
        "- a\n  ```\n  x\n  \n  y\n  ```\n",
    );
    assert_round_trips(
        "in a quote",
        &[quote(vec![p("a"), code("x\n\ny")])],
        "> a\n> ```\n> x\n>\n> y\n> ```\n",
    );
    assert_round_trips(
        "in a quote in an item",
        &in_item(vec![p("a"), quote(vec![code("x\ny")])]),
        "- a\n  > ```\n  > x\n  > y\n  > ```\n",
    );
    // An indented code block comes line by line, at the top level too.
    assert_imports_as("    x\n\n    y\n", &[code("x\n\ny")]);
    // Only the line break before the closing fence is the fence's.
    assert_imports_as(
        "- a\n  ```\n  x\n  \n  ```\n",
        &in_item(vec![p("a"), code("x\n")]),
    );
}

#[test]
fn lone_quote_marker_imports_schema_valid() {
    let empty = || el("paragraph", Vec::new());
    // Each input, the blocks it imports as, and their export.
    for (md, blocks, want) in [
        (">\n", vec![quote(vec![empty()])], ">\n"),
        ("> \n>\n", vec![quote(vec![empty()])], ">\n"),
        // An HTML block is dropped, which leaves the quote without blocks.
        ("> <!-- c -->\n", vec![quote(vec![empty()])], ">\n"),
        (
            "- >\n",
            in_item(vec![empty(), quote(vec![empty()])]),
            "- >\n",
        ),
        ("> >\n", vec![quote(vec![quote(vec![empty()])])], "> >\n"),
    ] {
        assert_imports_as(md, &blocks);
        let doc = import(md);
        let exported = export(&doc);
        assert_eq!(exported, want, "{md:?}");
        assert_eq!(
            normalised(import(&exported).inner()),
            normalised(doc.inner()),
            "{md:?}: import, export and import again is another tree"
        );
    }
}

#[test]
fn image_and_board_next_to_text_in_cell_are_kept() {
    let sentinel = format!("![Diagram](knot://board/{BOARD_ID}.svg)");
    assert_round_trips(
        "text and an image",
        &[table(
            vec![vec![p("h")]],
            Some(vec![vec![p("a"), image("x")]]),
        )],
        "| h |\n| --- |\n| a ![x](x.png) |\n",
    );
    assert_round_trips(
        "text and a board",
        &[table(vec![vec![p("h")]], Some(vec![vec![p("a"), board()]]))],
        &format!("| h |\n| --- |\n| a {sentinel} |\n"),
    );
    assert_round_trips(
        "a board and text, in a header cell",
        &[table(vec![vec![board(), p("b")]], None)],
        &format!("| {sentinel} b |\n| --- |\n"),
    );
    assert_round_trips(
        "an image between two paragraphs, and two images",
        &[table(
            vec![vec![p("h")], vec![p("i")]],
            Some(vec![
                vec![p("a"), image("x"), p("b")],
                vec![image("x"), image("y")],
            ]),
        )],
        "| h | i |\n| --- | --- |\n| a ![x](x.png) b | ![x](x.png) ![y](y.png) |\n",
    );
    // The space around an image is the one the cell's blocks are joined with.
    assert_imports_as(
        "| a  ![x](x.png)  b |\n| --- |\n",
        &[table(vec![vec![p("a"), image("x"), p("b")]], None)],
    );
}

#[test]
fn cell_image_candidate_does_not_leak() {
    assert_round_trips(
        "an image alone in a cell, then a paragraph of an image",
        &[
            table(
                vec![vec![p("h")], vec![p("i")]],
                Some(vec![vec![image("x")], vec![p("b")]]),
            ),
            image("z"),
        ],
        "| h | i |\n| --- | --- |\n| ![x](x.png) | b |\n\n![z](z.png)\n",
    );
    assert_round_trips(
        "a board alone in a cell, then a paragraph of an image",
        &[table(vec![vec![board()]], None), image("z")],
        &format!("| ![Diagram](knot://board/{BOARD_ID}.svg) |\n| --- |\n\n![z](z.png)\n"),
    );
    assert_round_trips(
        "an image starting a cell, then a paragraph of text",
        &[
            table(vec![vec![image("x"), p("b")]], None),
            p("text"),
            image("z"),
        ],
        "| ![x](x.png) b |\n| --- |\n\ntext\n\n![z](z.png)\n",
    );
    let mut blocks = vec![table(vec![vec![image("x")]], None)];
    blocks.extend(in_item(vec![el("paragraph", Vec::new()), image("z")]));
    assert_round_trips(
        "an image alone in a cell, then an item of an image",
        &blocks,
        "| ![x](x.png) |\n| --- |\n\n- ![z](z.png)\n",
    );
}
