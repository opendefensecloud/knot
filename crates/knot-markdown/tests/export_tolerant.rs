//! The serializer exports damaged and unknown content instead of failing the
//! page (spec "Tolerant export", P3, P4). The shapes are the ones the old
//! importer stored: stray text and hard breaks directly in a block container,
//! where the schema allows only blocks. Their export reads back as the
//! schema-valid page the old importer should have built, so the next restore
//! or import heals the page.

mod common;

use common::{N, build, el, ela, hard_break, p, run, text};
use knot_crdt::{DocHandle, YrsEngine};
use knot_markdown::legacy::compat::{normalised, schema_violations};
use knot_markdown::normalise::{Child, block_children};
use yrs::{Any, ReadTxn, Transact, XmlFragment, XmlOut};

fn export(doc: &DocHandle) -> String {
    knot_markdown::serialise(&YrsEngine, doc).expect("serialise")
}

fn import(md: &str) -> DocHandle {
    let (doc, _) = knot_markdown::from_markdown::parse(md).expect("parse");
    doc
}

/// Asserts that damaged `blocks` export as `want`, that `want` imports as
/// the schema-valid `healed` blocks, and that it is a fixed point.
fn assert_heals(what: &str, blocks: &[N], want: &str, healed: &[N]) {
    let md = export(&build(blocks));
    assert_eq!(md, want, "{what}");
    let back = import(&md);
    assert_eq!(
        schema_violations(back.inner()),
        Vec::<String>::new(),
        "{what}: {md:?} imports as a page that breaks the schema"
    );
    assert_eq!(
        normalised(back.inner()),
        normalised(build(healed).inner()),
        "{what}: {md:?} reads back as another tree"
    );
    assert_eq!(export(&back), md, "{what}: {md:?} is not a fixed point");
}

fn in_item(kids: Vec<N>) -> Vec<N> {
    vec![el("bullet_list", vec![el("list_item", kids)])]
}

/// One `XmlText` holding `s`, with the marks `marks`.
fn marked(s: &str, marks: &[(&'static str, Any)]) -> N {
    N::Text(vec![run(s, marks.to_vec())])
}

/// The value of a mark without attributes: `{}` from the editor, `true` from
/// the importer. The output must not depend on it.
fn flags() -> [Any; 2] {
    [
        Any::Map(std::sync::Arc::new(std::collections::HashMap::new())),
        Any::Bool(true),
    ]
}

/// A table with a header row and one body cell holding `kids`.
fn cell(kids: Vec<N>) -> N {
    el(
        "table",
        vec![
            el("table_row", vec![el("table_header", vec![p("h")])]),
            el("table_row", vec![el("table_cell", kids)]),
        ],
    )
}

#[test]
fn damaged_hard_break_item_exports_healed() {
    assert_heals(
        "list_item[p, hard_break, p]",
        &in_item(vec![p("first"), hard_break(), p("second")]),
        "- first  \n  second\n",
        &in_item(vec![el(
            "paragraph",
            vec![text("first"), hard_break(), text("second")],
        )]),
    );
}

#[test]
fn damaged_soft_break_item_exports_healed() {
    assert_heals(
        "list_item[p, \" \", p]",
        &in_item(vec![p("line one"), text(" "), p("continued")]),
        "- line one continued\n",
        &in_item(vec![p("line one continued")]),
    );
}

#[test]
fn damaged_bold_runs_merge() {
    for first in flags() {
        for stray in flags() {
            let bold = |s: &str, flag: &Any| marked(s, &[("bold", flag.clone())]);
            assert_heals(
                "bold paragraphs glued by a bold space",
                &in_item(vec![
                    el("paragraph", vec![bold("bold", &first)]),
                    bold(" ", &stray),
                    el("paragraph", vec![bold("more", &first)]),
                ]),
                "- **bold more**\n",
                &in_item(vec![el(
                    "paragraph",
                    vec![marked("bold more", &[("bold", Any::Bool(true))])],
                )]),
            );
        }
        let bold = |s: &str| marked(s, &[("bold", first.clone())]);
        assert_heals(
            "bold paragraphs glued by an unmarked space",
            &in_item(vec![
                el("paragraph", vec![bold("bold")]),
                text(" "),
                el("paragraph", vec![bold("more")]),
            ]),
            "- **bold** **more**\n",
            &in_item(vec![el(
                "paragraph",
                vec![N::Text(vec![
                    run("bold", vec![("bold", Any::Bool(true))]),
                    run(" ", Vec::new()),
                    run("more", vec![("bold", Any::Bool(true))]),
                ])],
            )]),
        );
    }
}

#[test]
fn damaged_chain_becomes_one_paragraph() {
    assert_heals(
        "list_item[p, hard_break, p, hard_break, p]",
        &in_item(vec![p("a"), hard_break(), p("b"), hard_break(), p("c")]),
        "- a  \n  b  \n  c\n",
        &in_item(vec![el(
            "paragraph",
            vec![text("a"), hard_break(), text("b"), hard_break(), text("c")],
        )]),
    );
}

#[test]
fn stray_text_at_top_level_and_in_cells_becomes_paragraph() {
    assert_heals(
        "text at the top level",
        &[text("stray")],
        "stray\n",
        &[p("stray")],
    );
    assert_heals(
        "a soft break between paragraphs at the top level",
        &[p("a"), text(" "), p("b")],
        "a b\n",
        &[p("a b")],
    );
    assert_heals(
        "text after a paragraph at the top level",
        &[p("a"), text("b")],
        "a\n\nb\n",
        &[p("a"), p("b")],
    );
    assert_heals(
        "text in a table cell",
        &[cell(vec![text("x")])],
        "| h |\n| --- |\n| x |\n",
        &[cell(vec![p("x")])],
    );
    assert_heals(
        "a soft break between paragraphs in a table cell",
        &[cell(vec![p("a"), text(" "), p("b")])],
        "| h |\n| --- |\n| a b |\n",
        &[cell(vec![p("a b")])],
    );
    assert_heals(
        "a soft break between paragraphs in a quote",
        &[el("blockquote", vec![p("a"), text(" "), p("b")])],
        "> a b\n",
        &[el("blockquote", vec![p("a b")])],
    );
}

#[test]
fn unknown_node_degrades_to_text_and_is_reported() {
    let doc = build(&[el("callout", vec![p("hi")])]);
    let (md, unknown) =
        knot_markdown::serialise_with_report(&YrsEngine, &doc).expect("serialise_with_report");
    assert_eq!(md, "hi\n");
    assert_eq!(unknown, vec!["callout".to_string()]);
    assert_eq!(export(&doc), "hi\n");

    // Unknown inline nodes keep their text too, and a known page reports
    // nothing.
    let doc = build(&[
        el(
            "paragraph",
            vec![
                text("a "),
                ela("mention", vec![("id", Any::from("u1"))], vec![text("@bob")]),
                text(" b"),
            ],
        ),
        el(
            "bullet_list",
            vec![el(
                "list_item",
                vec![p("x"), el("details", vec![p("hidden")])],
            )],
        ),
    ]);
    let (md, unknown) =
        knot_markdown::serialise_with_report(&YrsEngine, &doc).expect("serialise_with_report");
    assert_eq!(md, "a @bob b\n\n- x\n\n  hidden\n");
    assert_eq!(unknown, vec!["mention".to_string(), "details".to_string()]);
    let (_, unknown) = knot_markdown::serialise_with_report(&YrsEngine, &build(&[p("known")]))
        .expect("serialise_with_report");
    assert_eq!(unknown, Vec::<String>::new());
}

/// The glue rule's test vectors (P3), shared with knot-export's
/// `walk_glue_vectors`: `t` is stray text, `hb` a stray hard break, `p` a
/// paragraph, and the number is how many paragraphs the container holds after
/// the repair. A chain `paragraph (run paragraph)+` is one paragraph; any
/// other run is a paragraph of its own.
const GLUE_VECTORS: &[(&[&str], usize)] = &[
    (&["t", "p"], 2),
    (&["p", "t", "p"], 1),
    (&["p", "hb", "p", "hb", "p"], 1),
    (&["t"], 1),
    (&["p", "t"], 2),
    (&["p", "t", "hb", "p"], 1),
];

fn vector_nodes(vector: &[&str]) -> Vec<N> {
    vector
        .iter()
        .map(|node| match *node {
            "t" => text("t"),
            "hb" => hard_break(),
            "p" => p("p"),
            other => panic!("unknown vector node {other}"),
        })
        .collect()
}

/// How many paragraphs `block_children` makes of the children of the first
/// element of the doc's top-level fragment at `path`, or of the fragment
/// itself when `path` is empty.
fn paragraphs_in(doc: &DocHandle, path: &[u32]) -> usize {
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    let frag = txn.get_xml_fragment("default").expect("fragment");
    let children = match path.split_first() {
        None => block_children(&txn, &frag),
        Some((first, rest)) => {
            let Some(XmlOut::Element(mut el)) = frag.get(&txn, *first) else {
                panic!("no element at {first}");
            };
            for i in rest {
                let Some(XmlOut::Element(child)) = el.get(&txn, *i) else {
                    panic!("no element at {i}");
                };
                el = child;
            }
            block_children(&txn, &el)
        }
    };
    children
        .iter()
        .filter(|child| match child {
            Child::Para(_) => true,
            Child::Block(el) => el.tag().as_ref() == "paragraph",
        })
        .count()
}

#[test]
fn block_children_glue_vectors() {
    for (vector, want) in GLUE_VECTORS {
        let nodes = vector_nodes(vector);
        let containers: [(&str, Vec<N>, &[u32]); 4] = [
            ("the top level", nodes.clone(), &[]),
            ("a list item", in_item(nodes.clone()), &[0, 0]),
            ("a blockquote", vec![el("blockquote", nodes.clone())], &[0]),
            ("a table cell", vec![cell(nodes.clone())], &[0, 1, 0]),
        ];
        for (what, blocks, path) in containers {
            let doc = build(&blocks);
            assert_eq!(paragraphs_in(&doc, path), *want, "{vector:?} in {what}");
            // Every vector exports, and no text is lost.
            let md = export(&doc);
            let texts = vector.iter().filter(|n| **n != "hb").count();
            assert_eq!(
                md.matches(['t', 'p']).count(),
                texts,
                "{vector:?} in {what}: {md:?}"
            );
        }
    }
}

/// A stray hard break without text next to it would write an empty line,
/// which ends a paragraph and reads back as nothing: it is left out, and a
/// container of nothing else is written as one empty paragraph.
#[test]
fn stray_hard_breaks_without_text_are_left_out() {
    let empty = || el("paragraph", Vec::new());
    for (what, blocks, want, healed) in [
        (
            "after an item's paragraph",
            in_item(vec![p("a"), hard_break()]),
            "- a\n",
            in_item(vec![p("a")]),
        ),
        (
            "alone in an item",
            in_item(vec![hard_break()]),
            "- \n",
            in_item(vec![empty()]),
        ),
        (
            "before a paragraph at the top level",
            vec![hard_break(), p("a")],
            "a\n",
            vec![p("a")],
        ),
        (
            "before text, in a run of its own",
            in_item(vec![p("a"), hard_break(), text("b"), hard_break()]),
            "- a\n\n  b\n",
            in_item(vec![p("a"), p("b")]),
        ),
    ] {
        assert_heals(what, &blocks, want, &healed);
    }
    // A quote of nothing but a hard break is written as a quote of an empty
    // paragraph.
    assert_eq!(
        export(&build(&[el("blockquote", vec![hard_break()])])),
        ">\n"
    );
}

/// An item the old importer stored without its first paragraph (`- ## h`)
/// reads back with an empty one in front (A7), and that item must export as
/// `- ## h` again: an empty first paragraph before a block the item can
/// start with is written as the item without it.
#[test]
fn item_with_an_empty_first_paragraph_exports_as_the_item_without_it() {
    let empty = || el("paragraph", Vec::new());
    let heading = || ela("heading", vec![("level", Any::from("2"))], vec![text("h")]);
    let rule = || el("horizontal_rule", Vec::new());
    for (what, without, want) in [
        ("a heading", vec![heading(), p("x")], "- ## h\n  x\n"),
        (
            "two quotes in a quote",
            vec![el(
                "blockquote",
                vec![
                    el("blockquote", vec![p("q")]),
                    el("blockquote", vec![p("q")]),
                ],
            )],
            "- > > q\n  >\n  > > q\n",
        ),
        // `- ---` is a rule, not an item: the marker gets a line of its own.
        ("a rule", vec![rule()], "- \n  ---\n"),
    ] {
        let mut with = vec![empty()];
        with.extend(without.clone());
        assert_heals(what, &in_item(without), want, &in_item(with.clone()));
        assert_eq!(export(&build(&in_item(with))), want, "{what}");
    }
    // The healthy `- ## h` keeps its legacy bytes.
    let doc = build(&in_item(vec![heading()]));
    assert_eq!(
        export(&doc),
        knot_markdown::legacy::serialise(&YrsEngine, &doc).expect("legacy serialise")
    );
}

/// A node the schema doesn't name is written as a paragraph of its text, a
/// task item's first line included.
#[test]
fn unknown_first_block_of_a_task_item_is_its_first_line() {
    let task = ela(
        "list_item",
        vec![("checked", Any::Bool(true))],
        vec![el("callout", vec![p("note")])],
    );
    assert_eq!(
        export(&build(&[el("bullet_list", vec![task])])),
        "- [x] note\n"
    );
}
