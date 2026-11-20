//! Callout boxes in Markdown: a blockquote with a `kind` is GitHub's
//! `> [!WARNING]` without a title and GitLab's `> [!warning] Title` with
//! one; both read back, in any case.

use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::{from_markdown, to_markdown};
use yrs::{
    GetString, ReadTxn, Transact, Xml, XmlElementPrelim, XmlElementRef, XmlFragment, XmlOut,
    XmlTextPrelim,
};

fn parse(md: &str) -> DocHandle {
    from_markdown::parse(md).expect("parse").0
}

fn markdown(doc: &DocHandle) -> String {
    to_markdown::serialise(&YrsEngine, doc).expect("serialise")
}

/// A document of one blockquote with `kind`/`title` holding paragraph `text`.
fn doc_with_box(kind: Option<&str>, title: Option<&str>, text: &str) -> DocHandle {
    let doc = YrsEngine.new_doc();
    {
        let y = doc.inner();
        let frag = y.get_or_insert_xml_fragment("default");
        let mut txn = y.transact_mut();
        let quote = frag.push_back(&mut txn, XmlElementPrelim::empty("blockquote"));
        if let Some(k) = kind {
            quote.insert_attribute(&mut txn, "kind", k);
        }
        if let Some(t) = title {
            quote.insert_attribute(&mut txn, "title", t);
        }
        let p = quote.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(text));
    }
    doc
}

/// A box as read back: (kind, title, text of its blocks).
#[derive(Debug, PartialEq)]
struct Shape {
    kind: Option<String>,
    title: Option<String>,
    text: String,
}

fn text_of<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> String {
    let mut out = String::new();
    for i in 0..el.len(txn) {
        match el.get(txn, i) {
            Some(XmlOut::Element(e)) => {
                if !out.is_empty() {
                    out.push('|');
                }
                out.push_str(&text_of(txn, &e));
            }
            Some(XmlOut::Text(t)) => out.push_str(&t.get_string(txn)),
            _ => {}
        }
    }
    out
}

fn shape<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> Shape {
    Shape {
        kind: el.get_attribute(txn, "kind").map(|v| v.to_string(txn)),
        title: el.get_attribute(txn, "title").map(|v| v.to_string(txn)),
        text: text_of(txn, el),
    }
}

/// The top-level elements as (tag, shape).
fn top(doc: &DocHandle) -> Vec<(String, Shape)> {
    let y = doc.inner();
    let txn = y.transact();
    let frag = txn.get_xml_fragment("default").expect("fragment");
    (0..frag.len(&txn))
        .filter_map(|i| match frag.get(&txn, i) {
            Some(XmlOut::Element(e)) => Some((e.tag().to_string(), shape(&txn, &e))),
            _ => None,
        })
        .collect()
}

fn boxed(kind: &str, title: Option<&str>, text: &str) -> (String, Shape) {
    (
        "blockquote".into(),
        Shape {
            kind: Some(kind.into()),
            title: title.map(Into::into),
            text: text.into(),
        },
    )
}

fn quote(text: &str) -> (String, Shape) {
    (
        "blockquote".into(),
        Shape {
            kind: None,
            title: None,
            text: text.into(),
        },
    )
}

#[test]
fn untitled_box_exports_github_form() {
    let md = markdown(&doc_with_box(Some("warning"), None, "x"));
    assert_eq!(md, "> [!WARNING]\n> x\n");
}

#[test]
fn titled_box_exports_gitlab_form() {
    let md = markdown(&doc_with_box(
        Some("warning"),
        Some("Wartung am Freitag"),
        "x",
    ));
    assert_eq!(md, "> [!warning] Wartung am Freitag\n> x\n");
}

#[test]
fn every_kind_round_trips() {
    for kind in ["note", "tip", "important", "warning", "caution"] {
        for title in [None, Some("Ein Titel")] {
            let once = markdown(&doc_with_box(Some(kind), title, "body"));
            let back = parse(&once);
            assert_eq!(top(&back), vec![boxed(kind, title, "body")], "{once}");
            assert_eq!(markdown(&back), once);
        }
    }
}

#[test]
fn lowercase_and_mixed_case_markers_import() {
    let doc = parse("> [!note]\n> a\n\n---\n\n> [!Tip]\n> b\n");
    let tops = top(&doc);
    assert_eq!(tops[0], boxed("note", None, "a"));
    assert_eq!(tops[2], boxed("tip", None, "b"));
}

#[test]
fn unknown_marker_stays_a_quote() {
    let doc = parse("> [!FOO]\n> x\n");
    let tops = top(&doc);
    assert_eq!(tops.len(), 1);
    assert_eq!(tops[0].1.kind, None);
    assert!(tops[0].1.text.contains("[!FOO]"), "{:?}", tops[0]);
}

#[test]
fn plain_quote_starting_with_marker_text_stays_a_quote() {
    let doc = doc_with_box(None, None, "[!NOTE] just text");
    let md = markdown(&doc);
    assert_eq!(top(&parse(&md)), vec![quote("[!NOTE] just text")], "{md}");
}

#[test]
fn marker_in_code_block_is_code() {
    let doc = parse("```\n> [!WARNING] x\n```\n");
    let tops = top(&doc);
    assert_eq!(tops.len(), 1);
    assert_eq!(tops[0].0, "code_block");
}

#[test]
fn title_with_markdown_characters_round_trips() {
    let title = "*a* <b> ]x[ `c` & _d_";
    let md = markdown(&doc_with_box(Some("tip"), Some(title), "x"));
    assert_eq!(
        top(&parse(&md)),
        vec![boxed("tip", Some(title), "x")],
        "{md}"
    );
}

#[test]
fn unknown_kind_exports_as_plain_quote() {
    let md = markdown(&doc_with_box(Some("danger"), Some("T"), "x"));
    assert_eq!(md, "> x\n");
}

#[test]
fn nested_box_and_box_in_list_keep_text() {
    let src =
        "> [!NOTE]\n> outer\n>\n> > [!TIP]\n> > inner\n\n- item\n\n  > [!WARNING]\n  > in list\n";
    let doc = parse(src);
    let md = markdown(&doc);
    let again = parse(&md);
    assert_eq!(markdown(&again), md);
    let y = again.inner();
    let txn = y.transact();
    let frag = txn.get_xml_fragment("default").expect("fragment");
    let XmlOut::Element(outer) = frag.get(&txn, 0).unwrap() else {
        panic!()
    };
    assert_eq!(shape(&txn, &outer).kind.as_deref(), Some("note"), "{md}");
    let XmlOut::Element(inner) = outer.get(&txn, 1).unwrap() else {
        panic!()
    };
    assert_eq!(
        shape(&txn, &inner),
        Shape {
            kind: Some("tip".into()),
            title: None,
            text: "inner".into()
        },
        "{md}"
    );
    let XmlOut::Element(list) = frag.get(&txn, 1).unwrap() else {
        panic!()
    };
    let XmlOut::Element(item) = list.get(&txn, 0).unwrap() else {
        panic!()
    };
    let XmlOut::Element(in_list) = item.get(&txn, 1).unwrap() else {
        panic!("{md}")
    };
    assert_eq!(
        shape(&txn, &in_list).kind.as_deref(),
        Some("warning"),
        "{md}"
    );
}

#[test]
fn box_in_table_cell_exports_its_text() {
    let doc = YrsEngine.new_doc();
    {
        let y = doc.inner();
        let frag = y.get_or_insert_xml_fragment("default");
        let mut txn = y.transact_mut();
        let table = frag.push_back(&mut txn, XmlElementPrelim::empty("table"));
        let row = table.push_back(&mut txn, XmlElementPrelim::empty("table_row"));
        let cell = row.push_back(&mut txn, XmlElementPrelim::empty("table_header"));
        let quote = cell.push_back(&mut txn, XmlElementPrelim::empty("blockquote"));
        quote.insert_attribute(&mut txn, "kind", "warning");
        quote.insert_attribute(&mut txn, "title", "T");
        let p = quote.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("cell text"));
    }
    let md = markdown(&doc);
    assert!(md.contains("cell text"), "{md}");
    assert!(!md.contains("[!"), "{md}");
}

#[test]
fn empty_box_imports_with_one_paragraph() {
    let doc = parse("> [!TIP]\n");
    assert_eq!(top(&doc), vec![boxed("tip", None, "")]);
}

#[test]
fn a_marker_pulldown_does_not_read_keeps_its_text_and_its_title() {
    // Two spaces after `>`: not an alert for pulldown-cmark, so the quote
    // stays a quote with its whole first line, and the box after it keeps
    // its own (empty) title.
    let doc = parse(">  [!note] Lost\n> a\n\n> [!WARNING]\n> b\n");
    let tops = top(&doc);
    assert_eq!(tops[0].1.kind, None, "{tops:?}");
    assert!(tops[0].1.text.contains("[!note] Lost"), "{tops:?}");
    assert_eq!(tops[1], boxed("warning", None, "b"));
}

#[test]
fn an_indented_code_line_in_a_quote_is_not_a_marker() {
    let doc = parse(">      [!note] code\n\n> [!TIP]\n> h\n");
    let tops = top(&doc);
    assert!(tops[0].1.text.contains("[!note] code"), "{tops:?}");
    assert_eq!(tops[1], boxed("tip", None, "h"));
}

#[test]
fn plain_quote_with_a_space_before_marker_text_stays_a_quote() {
    let md = markdown(&doc_with_box(None, None, " [!note] x"));
    let tops = top(&parse(&md));
    assert_eq!(tops.len(), 1, "{md}");
    assert_eq!(tops[0].1.kind, None, "{md}");
    assert_eq!(tops[0].1.text.trim(), "[!note] x", "{md}");
}

#[test]
fn plain_quote_starting_with_a_link_keeps_the_link() {
    let md = markdown(&parse("> [!x](https://e.com) rest\n"));
    assert_eq!(md, "> [!x](https://e.com) rest\n");
}

#[test]
fn a_rule_under_a_titled_marker_is_a_rule_in_the_box() {
    let doc = parse("> [!note] T\n> ---\n");
    let tops = top(&doc);
    assert_eq!(tops.len(), 1, "{tops:?}");
    assert_eq!(tops[0].1.kind.as_deref(), Some("note"));
    assert_eq!(tops[0].1.title.as_deref(), Some("T"));
    assert!(
        markdown(&doc).starts_with("> [!note] T\n>"),
        "{}",
        markdown(&doc)
    );
    assert!(markdown(&doc).contains("---"), "{}", markdown(&doc));
}
