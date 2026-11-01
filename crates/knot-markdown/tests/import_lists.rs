//! The importer keeps a tight list item's content inside its paragraph
//! (spec A1, A2, A3), gives an empty item its paragraph (A7) and writes a
//! rule into its container (A9).

use knot_crdt::YrsEngine;
use knot_markdown::legacy::compat::{normalised, schema_violations};

/// `md` imported, after asserting the import is schema-valid.
fn import(md: &str) -> knot_crdt::DocHandle {
    let (doc, _) = knot_markdown::from_markdown::parse(md).expect("parse");
    assert_eq!(
        schema_violations(doc.inner()),
        Vec::<String>::new(),
        "{md:?} imports to an invalid tree:\n{}",
        normalised(doc.inner())
    );
    doc
}

fn export(doc: &knot_crdt::DocHandle) -> String {
    knot_markdown::serialise(&YrsEngine, doc).expect("serialise")
}

#[test]
fn hard_break_in_tight_item_stays_in_its_paragraph() {
    let doc = import("- a  \n  b\n");
    assert_eq!(
        normalised(doc.inner()),
        concat!(
            "<bullet_list>\n",
            "  <list_item>\n",
            "    <paragraph>\n",
            "      \"a\"\n",
            "      <hard_break>\n",
            "      \"b\"\n",
        )
    );
    assert_eq!(export(&doc), "- a  \n  b\n");
}

#[test]
fn soft_break_in_tight_item_joins_with_space() {
    let doc = import("- line one\n  continued\n");
    assert_eq!(
        normalised(doc.inner()),
        concat!(
            "<bullet_list>\n",
            "  <list_item>\n",
            "    <paragraph>\n",
            "      \"line one continued\"\n",
        )
    );
}

#[test]
fn breaks_work_in_ordered_task_nested_and_quoted_lists() {
    for md in [
        "1. one  \n   two\n",
        "1. one\n   two\n",
        "- [ ] buy  \n  milk\n",
        "- [ ] buy\n  milk\n",
        "1. [x] done  \n   today\n",
        "- a\n  - b  \n    c\n",
        "- a\n  - b\n    c\n",
        "> - a  \n>   b\n",
        "> - a\n>   b\n",
    ] {
        // `import` asserts the tree is schema-valid.
        let doc = import(md);
        assert!(
            knot_markdown::serialise(&YrsEngine, &doc).is_ok(),
            "{md:?} doesn't export"
        );
    }
}

#[test]
fn image_and_board_in_tight_items_are_kept() {
    let board = "![l](knot://board/00000000-0000-0000-0000-000000000001.svg)";
    let board_item = format!("- {board}\n");
    // As a run of its own after another block in the item, too.
    let board_after_heading = format!("- a\n  # h\n  {board}\n");
    for (md, kept) in [
        ("- ![a](x.png)\n", "![a](x.png)"),
        (board_item.as_str(), board),
        ("- a\n  # h\n  ![x](y.png)\n", "![x](y.png)"),
        (board_after_heading.as_str(), board),
    ] {
        let doc = import(md);
        let out = export(&doc);
        assert!(out.contains(kept), "{md:?} exports {out:?}");
    }
}

/// A run directly in a tight item that holds nothing the importer keeps
/// (inline HTML, an empty link, images that share a line) gets no paragraph
/// after another block, as with the legacy importer: an empty paragraph there
/// would export as a blank line that the next import drops.
#[test]
fn item_run_without_content_adds_no_paragraph() {
    for md in [
        "- a\n  # h\n  <span></span>\n",
        "- a\n  # h\n  ![x](a.png)![y](b.png)\n",
        "- a\n  # h\n  [](x)\n",
    ] {
        let doc = import(md);
        assert_eq!(
            normalised(doc.inner()),
            concat!(
                "<bullet_list>\n",
                "  <list_item>\n",
                "    <paragraph>\n",
                "      \"a\"\n",
                "    <heading level=\"1\">\n",
                "      \"h\"\n",
            ),
            "{md:?}"
        );
        assert_eq!(export(&doc), "- a\n  # h\n", "{md:?}");
    }
}

#[test]
fn html_comment_between_item_lines_keeps_lines_apart() {
    let doc = import("- a\n  <!-- c -->\n  b\n");
    assert_eq!(
        normalised(doc.inner()),
        concat!(
            "<bullet_list>\n",
            "  <list_item>\n",
            "    <paragraph>\n",
            "      \"a\"\n",
            "    <paragraph>\n",
            "      \"b\"\n",
        )
    );
}

/// pulldown-cmark emits an HTML block's extra indentation as text inside the
/// block. The importer ignores HTML blocks, so that text is ignored too
/// instead of landing directly in the item.
#[test]
fn indented_html_block_in_item_leaves_no_stray_text() {
    for (md, want) in [
        (
            "- a\n    <!-- c -->\n",
            concat!(
                "<bullet_list>\n",
                "  <list_item>\n",
                "    <paragraph>\n",
                "      \"a\"\n",
            ),
        ),
        (
            "- [ ] <!-- c -->\n    <!-- c -->\n",
            concat!(
                "<bullet_list>\n",
                "  <list_item checked=\"false\">\n",
                "    <paragraph>\n",
            ),
        ),
    ] {
        assert_eq!(normalised(import(md).inner()), want, "{md:?}");
    }
}

#[test]
fn empty_item_gets_an_empty_paragraph() {
    let doc = import("- a\n-\n- c\n");
    assert_eq!(
        normalised(doc.inner()),
        concat!(
            "<bullet_list>\n",
            "  <list_item>\n",
            "    <paragraph>\n",
            "      \"a\"\n",
            "  <list_item>\n",
            "    <paragraph>\n",
            "  <list_item>\n",
            "    <paragraph>\n",
            "      \"c\"\n",
        )
    );
    // The empty item survives the second pass too.
    let first = export(&doc);
    assert_eq!(first, "- a\n- \n- c\n");
    assert_eq!(export(&import(&first)), first);
}

#[test]
fn rule_inside_item_or_quote_stays_in_its_container() {
    for (md, container, pad) in [
        ("> a\n>\n> ***\n>\n> b\n", "<blockquote>\n", "  "),
        (
            "- a\n\n  ***\n\n  b\n",
            "<bullet_list>\n  <list_item>\n",
            "    ",
        ),
    ] {
        let doc = import(md);
        let want = format!(
            "{container}{pad}<paragraph>\n{pad}  \"a\"\n{pad}<horizontal_rule>\n{pad}<paragraph>\n{pad}  \"b\"\n"
        );
        assert_eq!(normalised(doc.inner()), want, "{md:?}");
    }
}
