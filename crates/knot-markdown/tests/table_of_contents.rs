//! `table_of_contents` in Markdown: `[[_TOC_]]` (GitLab's own marker) for
//! the default depth 3, with `<!-- knot:toc maxLevel=N -->` before it for
//! any other depth; one line in a table cell. A missing or invalid comment
//! means 3.

use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::{from_markdown, to_markdown};
use yrs::{
    Any, GetString, ReadTxn, Text, Transact, Xml, XmlElementPrelim, XmlElementRef, XmlFragment,
    XmlOut, XmlTextPrelim,
};

fn parse(md: &str) -> DocHandle {
    from_markdown::parse(md).expect("parse").0
}

fn markdown(doc: &DocHandle) -> String {
    to_markdown::serialise(&YrsEngine, doc).expect("serialise")
}

/// `a`, a table of contents with `max_level` (none: no attribute), `b`.
fn doc_with_toc(max_level: Option<Any>) -> DocHandle {
    let doc = YrsEngine.new_doc();
    {
        let y = doc.inner();
        let frag = y.get_or_insert_xml_fragment("default");
        let mut txn = y.transact_mut();
        let a = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        a.push_back(&mut txn, XmlTextPrelim::new("a"));
        let toc = frag.push_back(&mut txn, XmlElementPrelim::empty("table_of_contents"));
        if let Some(level) = max_level {
            toc.insert_attribute(&mut txn, "maxLevel", level);
        }
        let b = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        b.push_back(&mut txn, XmlTextPrelim::new("b"));
    }
    doc
}

/// `(tag, maxLevel)` of each element under `el`'s children, or of the
/// fragment's when `el` is none.
fn tags_of<T: ReadTxn>(txn: &T, children: Vec<XmlOut>) -> Vec<(String, Option<String>)> {
    children
        .into_iter()
        .filter_map(|c| match c {
            XmlOut::Element(el) => Some((
                el.tag().to_string(),
                el.get_attribute(txn, "maxLevel").map(|v| v.to_string(txn)),
            )),
            _ => None,
        })
        .collect()
}

fn top(doc: &DocHandle) -> Vec<(String, Option<String>)> {
    let y = doc.inner();
    let txn = y.transact();
    let frag = txn.get_xml_fragment("default").expect("fragment");
    let children = (0..frag.len(&txn))
        .filter_map(|i| frag.get(&txn, i))
        .collect();
    tags_of(&txn, children)
}

/// The children of the first cell of the first table at the top level.
fn first_cell(doc: &DocHandle) -> Vec<(String, Option<String>)> {
    let y = doc.inner();
    let txn = y.transact();
    let frag = txn.get_xml_fragment("default").expect("fragment");
    let first_el = |el: &XmlElementRef| -> XmlElementRef {
        match el.get(&txn, 0) {
            Some(XmlOut::Element(e)) => e,
            other => panic!("expected an element, got {other:?}"),
        }
    };
    let table = (0..frag.len(&txn))
        .filter_map(|i| match frag.get(&txn, i) {
            Some(XmlOut::Element(e)) if e.tag().as_ref() == "table" => Some(e),
            _ => None,
        })
        .next()
        .expect("a table");
    let row = first_el(&table);
    // The header row's cell holds the marker in these tests.
    let cell = first_el(&row);
    let children = (0..cell.len(&txn))
        .filter_map(|i| cell.get(&txn, i))
        .collect();
    tags_of(&txn, children)
}

fn toc(level: &str) -> (String, Option<String>) {
    ("table_of_contents".into(), Some(level.into()))
}

fn para() -> (String, Option<String>) {
    ("paragraph".into(), None)
}

#[test]
fn toc_default_is_bare_marker() {
    let md = markdown(&doc_with_toc(Some(Any::from("3"))));
    assert!(md.contains("\n[[_TOC_]]\n"), "{md}");
    assert!(!md.contains("knot:toc"), "{md}");
}

#[test]
fn toc_without_attribute_is_bare_marker() {
    let md = markdown(&doc_with_toc(None));
    assert!(md.contains("\n[[_TOC_]]\n"), "{md}");
    assert!(!md.contains("knot:toc"), "{md}");
}

#[test]
fn toc_depth_writes_comment() {
    let md = markdown(&doc_with_toc(Some(Any::from("2"))));
    assert!(
        md.contains("\n<!-- knot:toc maxLevel=2 -->\n[[_TOC_]]\n"),
        "{md}"
    );
}

#[test]
fn toc_depth_from_the_editor_is_a_number() {
    // The editor stores attributes as JSON numbers.
    let md = markdown(&doc_with_toc(Some(Any::Number(2.0))));
    assert!(
        md.contains("<!-- knot:toc maxLevel=2 -->\n[[_TOC_]]\n"),
        "{md}"
    );
}

#[test]
fn bare_marker_imports_as_depth_3_and_round_trips() {
    let src = "a\n\n[[_TOC_]]\n\nb\n";
    let doc = parse(src);
    assert_eq!(top(&doc), vec![para(), toc("3"), para()]);
    assert_eq!(markdown(&doc), src);
}

#[test]
fn depth_comment_imports_and_round_trips() {
    let src = "a\n\n<!-- knot:toc maxLevel=2 -->\n[[_TOC_]]\n\nb\n";
    let doc = parse(src);
    assert_eq!(top(&doc), vec![para(), toc("2"), para()]);
    assert_eq!(markdown(&doc), src);
}

#[test]
fn invalid_toc_comment_means_3() {
    for bad in ["99", "0", "abc", ""] {
        let src = format!("<!-- knot:toc maxLevel={bad} -->\n[[_TOC_]]\n");
        assert_eq!(top(&parse(&src)), vec![toc("3")], "maxLevel={bad}");
    }
}

#[test]
fn marker_inside_text_stays_text() {
    let doc = parse("see [[_TOC_]] here\n");
    assert_eq!(top(&doc), vec![para()]);
}

#[test]
fn a_comment_without_marker_is_dropped_and_forgotten() {
    let doc = parse("<!-- knot:toc maxLevel=2 -->\n\nx\n\n[[_TOC_]]\n");
    assert_eq!(top(&doc), vec![para(), toc("3")]);
}

#[test]
fn toc_in_table_cell_round_trips() {
    let src = "| <!-- knot:toc maxLevel=1 --> [[_TOC_]] | b |\n| --- | --- |\n| c | d |\n";
    let doc = parse(src);
    assert_eq!(first_cell(&doc), vec![toc("1")]);
    let once = markdown(&doc);
    assert!(
        once.contains("<!-- knot:toc maxLevel=1 --> [[_TOC_]]"),
        "{once}"
    );
    let again = parse(&once);
    assert_eq!(first_cell(&again), vec![toc("1")]);
    assert_eq!(markdown(&again), once);
}

#[test]
fn bare_marker_in_table_cell() {
    let doc = parse("| [[_TOC_]] | b |\n| --- | --- |\n| c | d |\n");
    assert_eq!(first_cell(&doc), vec![toc("3")]);
}

// Silence unused-import lints for helpers used only on some paths.
#[allow(dead_code)]
fn _uses(t: &yrs::TextRef, txn: &yrs::Transaction) -> String {
    t.get_string(txn) + &t.len(txn).to_string()
}
