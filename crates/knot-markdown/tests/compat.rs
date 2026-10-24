//! Differential compatibility harness (spec "Compatibility guarantees", 1).
//!
//! The serializer and the importer are compared with the frozen copies in
//! `knot_markdown::legacy`, over the corpus in `common::corpus`:
//! - a healthy document (its legacy export is a fixed point) exports byte for
//!   byte as before, unless it falls in a named `export_class`;
//! - a healthy Markdown input imports to the same tree as before, unless it
//!   falls in a named `import_class`;
//! - no export loses text.

mod common;

use std::collections::{BTreeSet, HashSet};

use common::corpus::{self, Case};
use common::{N, build, el, ela, hard_break, p, text};
use knot_crdt::{DocHandle, YrsEngine};
use knot_markdown::legacy::{self, compat};
use yrs::types::text::YChange;
use yrs::{Any, Out, ReadTxn, Text, Transact, Xml, XmlFragment, XmlOut};

/// Seeds of the random corpus, and documents per seed.
const RANDOM_SEEDS: std::ops::RangeInclusive<u64> = 1..=50;
const RANDOM_DOCS_PER_SEED: usize = 200;

/// Lower bounds on how much each differential compares, so a broken corpus
/// can't make the harness pass by comparing nothing. Far below today's counts
/// (about 5600 healthy documents, 2700 healthy inputs of which 27 are files,
/// 16800 exports), leaving room for the documents bug classes exclude.
const MIN_HEALTHY_DOCS: usize = 1_000;
const MIN_HEALTHY_INPUTS: usize = 500;
const MIN_HEALTHY_FILE_INPUTS: usize = 15;
const MIN_EXPORTED_DOCS: usize = 5_000;

/// How many failures a test prints before the total.
const SHOWN_FAILURES: usize = 10;

fn corpus_docs() -> Vec<Case> {
    let mut cases = corpus::generated();
    for seed in RANDOM_SEEDS {
        cases.extend(corpus::random(seed, RANDOM_DOCS_PER_SEED));
    }
    cases
}

fn assert_no_failures(what: &str, failures: &[String]) {
    if failures.is_empty() {
        return;
    }
    let shown: Vec<&str> = failures
        .iter()
        .take(SHOWN_FAILURES)
        .map(String::as_str)
        .collect();
    panic!(
        "{what}: {} case(s), first {}:\n\n{}",
        failures.len(),
        shown.len(),
        shown.join("\n\n")
    );
}

fn assert_at_least(what: &str, got: usize, min: usize) {
    assert!(
        got >= min,
        "only {got} {what} compared, expected at least {min}"
    );
}

#[test]
fn serializer_matches_legacy_on_healthy_docs() {
    let mut compared = 0;
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let Some(legacy_md) = compat::healthy(&case.doc) else {
            continue;
        };
        if compat::export_class(&case.doc).is_some() {
            continue;
        }
        compared += 1;
        match knot_markdown::serialise(&YrsEngine, &case.doc) {
            Ok(md) if md == legacy_md => {}
            Ok(md) => failures.push(format!(
                "{}\n  legacy: {legacy_md:?}\n  new:    {md:?}",
                case.name
            )),
            Err(e) => failures.push(format!(
                "{}\n  legacy: {legacy_md:?}\n  new:    error {e}",
                case.name
            )),
        }
    }
    assert_no_failures("healthy document exports differently", &failures);
    assert_at_least("healthy documents", compared, MIN_HEALTHY_DOCS);
}

#[test]
fn importer_matches_legacy_on_healthy_inputs() {
    // The fixtures and probes, then every healthy corpus export: Markdown the
    // product writes today, which must keep importing the same way.
    let file_inputs = corpus::markdown_inputs();
    let file_count = file_inputs.len();
    let mut inputs = file_inputs;
    for case in corpus_docs() {
        if let Some(md) = compat::healthy(&case.doc) {
            inputs.push((format!("export of {}", case.name), md));
        }
    }

    let mut seen = HashSet::new();
    let mut compared = 0;
    let mut compared_files = 0;
    let mut failures = Vec::new();
    for (i, (name, md)) in inputs.iter().enumerate() {
        if !seen.insert(md.as_str()) || compat::import_class(md).is_some() {
            continue;
        }
        let Ok((legacy_doc, _)) = legacy::parse(md) else {
            continue;
        };
        if !compat::schema_violations(legacy_doc.inner()).is_empty()
            || compat::healthy(&legacy_doc).is_none()
        {
            continue;
        }
        compared += 1;
        if i < file_count {
            compared_files += 1;
        }
        let want = compat::normalised(legacy_doc.inner());
        match knot_markdown::from_markdown::parse(md) {
            Ok((doc, _)) => {
                let got = compat::normalised(doc.inner());
                if got != want {
                    failures.push(format!("{name}: {md:?}\n  legacy:\n{want}\n  new:\n{got}"));
                }
            }
            Err(e) => failures.push(format!("{name}: {md:?}\n  new: error {e}")),
        }
    }
    assert_no_failures("healthy input imports differently", &failures);
    assert_at_least("healthy inputs", compared, MIN_HEALTHY_INPUTS);
    assert_at_least(
        "healthy fixture and probe inputs",
        compared_files,
        MIN_HEALTHY_FILE_INPUTS,
    );
}

#[test]
fn new_import_is_schema_valid_on_every_input() {
    let mut failures = Vec::new();
    for (name, md) in corpus::markdown_inputs() {
        match knot_markdown::from_markdown::parse(&md) {
            Ok((doc, _)) => {
                let violations = compat::schema_violations(doc.inner());
                if !violations.is_empty() {
                    failures.push(format!(
                        "{name}: {md:?}\n  {}\n{}",
                        violations.join("\n  "),
                        compat::normalised(doc.inner())
                    ));
                }
            }
            Err(e) => failures.push(format!("{name}: {md:?}\n  error {e}")),
        }
    }
    assert_no_failures("new import breaks the schema", &failures);
}

/// Each import class names the inputs it is about, and leaves alone
/// neighbouring inputs that import the same way as before.
#[test]
fn import_classes_name_their_inputs() {
    let cases: &[(&str, Option<&str>)] = &[
        ("\\*not em\\*\n", Some("markdown_escaping")),
        ("- \\[x\\] not a task\n", Some("markdown_escaping")),
        ("a &amp; b\n", Some("markdown_escaping")),
        ("| a |\n| --- |\n| b \\| c |\n", None),
        ("`\\*`\n", None),
        ("- first\\\n  second\n", None),
        ("- \\\n  b\n", Some("leading_hard_break")),
        ("\\\nb\n", Some("leading_hard_break")),
        ("*\\\nb*\n", Some("leading_hard_break")),
        ("![a](x.png)\\\nb\n", Some("leading_hard_break")),
        ("a  \nb\n", None),
        ("- # h\n", Some("item_first_child_not_paragraph")),
        ("- a\n-\n- c\n", Some("item_first_child_not_paragraph")),
        ("- ![a](x.png)\n", Some("item_first_child_not_paragraph")),
        ("- a\n  # h\n", None),
        ("> a\n>\n> ***\n>\n> b\n", Some("rule_in_container")),
        ("- a\n\n  ***\n\n  b\n", Some("rule_in_container")),
        ("> a\n\n***\n", None),
        ("- a\n  <!-- c -->\n  b\n", Some("html_between_item_lines")),
        ("- a\n  <!-- c -->\n", None),
        ("- a\n\n  <!-- c -->\n\n  b\n", None),
        ("<!-- c -->\n\na\n", None),
        ("- a  \n  b\n", None),
        ("- a\n  - b\n", None),
    ];
    let mut failures = Vec::new();
    for (md, want) in cases {
        let got = compat::import_class(md);
        if got != *want {
            failures.push(format!("{md:?}: class {got:?}, expected {want:?}"));
        }
    }
    assert_no_failures("import class is wrong", &failures);
}

/// Every text character, in document order, from every `XmlText`.
fn doc_text(doc: &DocHandle) -> String {
    fn walk<T: ReadTxn>(txn: &T, node: &XmlOut, out: &mut String) {
        match node {
            XmlOut::Element(el) => {
                for i in 0..el.len(txn) {
                    if let Some(child) = el.get(txn, i) {
                        walk(txn, &child, out);
                    }
                }
            }
            XmlOut::Text(t) => {
                for chunk in t.diff(txn, YChange::identity) {
                    if let Out::Any(Any::String(s)) = chunk.insert {
                        out.push_str(&s);
                    }
                }
            }
            XmlOut::Fragment(f) => {
                for i in 0..f.len(txn) {
                    if let Some(child) = f.get(txn, i) {
                        walk(txn, &child, out);
                    }
                }
            }
        }
    }
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    let mut out = String::new();
    if let Some(frag) = txn.get_xml_fragment("default") {
        for i in 0..frag.len(&txn) {
            if let Some(child) = frag.get(&txn, i) {
                walk(&txn, &child, &mut out);
            }
        }
    }
    out
}

/// The first non-whitespace character of `text` that `md` doesn't contain in
/// order, with its char index. Whitespace is skipped: exports legitimately
/// rewrite it (soft breaks, table cells joined onto one line).
fn first_lost_char(text: &str, md: &str) -> Option<(usize, char)> {
    let mut rest = md.chars();
    for (i, c) in text.chars().enumerate() {
        if c.is_whitespace() {
            continue;
        }
        if !rest.any(|m| m == c) {
            return Some((i, c));
        }
    }
    None
}

#[test]
fn no_text_is_lost() {
    let mut exported = 0;
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let md = match knot_markdown::serialise(&YrsEngine, &case.doc) {
            Ok(md) => md,
            Err(e) => {
                // A failed export loses everything. It may only fail where
                // the legacy export already failed (damaged and unknown
                // content, fixed by the tolerant export).
                if let Ok(legacy_md) = legacy::serialise(&YrsEngine, &case.doc) {
                    failures.push(format!(
                        "{}: legacy exports {legacy_md:?}, new fails: {e}",
                        case.name
                    ));
                }
                continue;
            }
        };
        exported += 1;
        let text = doc_text(&case.doc);
        if let Some((i, c)) = first_lost_char(&text, &md) {
            failures.push(format!(
                "{}: {c:?} (char {i} of the text) is missing\n  text:   {text:?}\n  export: {md:?}",
                case.name
            ));
        }
    }
    assert_no_failures("export loses text", &failures);
    assert_at_least("exported documents", exported, MIN_EXPORTED_DOCS);
}

fn in_item(kids: Vec<N>) -> Vec<N> {
    vec![el("bullet_list", vec![el("list_item", kids)])]
}

#[test]
fn schema_check_finds_the_damaged_shapes() {
    let violations = compat::schema_violations(
        build(&in_item(vec![p("first"), hard_break(), p("second")])).inner(),
    );
    assert!(
        violations.iter().any(|v| v.contains("hard_break")),
        "list_item[paragraph, hard_break, paragraph] not reported: {violations:?}"
    );

    let damaged: Vec<(&str, Vec<N>)> = vec![
        (
            "text between an item's paragraphs",
            in_item(vec![p("line one"), text(" "), p("continued")]),
        ),
        ("an empty item", in_item(vec![])),
        (
            "an item starting with a heading",
            in_item(vec![ela(
                "heading",
                vec![("level", Any::from("1"))],
                vec![text("h")],
            )]),
        ),
        ("text at the top level", vec![text("stray")]),
        (
            "a hard break in a blockquote",
            vec![el("blockquote", vec![p("a"), hard_break()])],
        ),
        (
            "an empty table cell",
            vec![el(
                "table",
                vec![el("table_row", vec![el("table_cell", vec![])])],
            )],
        ),
        (
            "text in a table cell",
            vec![el(
                "table",
                vec![el("table_row", vec![el("table_cell", vec![text("x")])])],
            )],
        ),
        (
            "a hard break in a code block",
            vec![el("code_block", vec![text("x"), hard_break()])],
        ),
        (
            "a paragraph in a paragraph",
            vec![el("paragraph", vec![p("inner")])],
        ),
        ("an empty list", vec![el("bullet_list", vec![])]),
        ("an unknown node", vec![el("callout", vec![p("hi")])]),
    ];
    for (what, blocks) in damaged {
        let violations = compat::schema_violations(build(&blocks).inner());
        assert!(!violations.is_empty(), "{what}: no violation reported");
    }

    // The checker reports nothing for schema-valid shapes.
    let valid = vec![
        p("para"),
        el(
            "bullet_list",
            vec![ela(
                "list_item",
                vec![("checked", Any::Bool(true))],
                vec![
                    el("paragraph", vec![text("a"), hard_break(), text("b")]),
                    el("ordered_list", vec![el("list_item", vec![p("nested")])]),
                    el("blockquote", vec![p("q"), el("horizontal_rule", vec![])]),
                ],
            )],
        ),
        el(
            "table",
            vec![el(
                "table_row",
                vec![
                    el("table_header", vec![p("h")]),
                    el("table_cell", vec![p("c"), p("d")]),
                ],
            )],
        ),
        el("code_block", vec![text("fn main() {}")]),
        el("code_block", vec![]),
    ];
    assert_eq!(
        compat::schema_violations(build(&valid).inner()),
        Vec::<String>::new()
    );
}

/// The bug this plan fixes first (spec A1): the old importer splits a tight
/// list item's line break into `paragraph, hard_break, paragraph`, which the
/// schema forbids and the serializer can't export.
#[test]
fn legacy_import_produces_schema_violations_today() {
    let (doc, _) = legacy::parse("- a  \n  b\n").expect("legacy parse");
    assert_eq!(
        compat::normalised(doc.inner()),
        concat!(
            "<bullet_list>\n",
            "  <list_item>\n",
            "    <paragraph>\n",
            "      \"a\"\n",
            "    <hard_break>\n",
            "    <paragraph>\n",
            "      \"b\"\n",
        )
    );
    assert!(
        !compat::schema_violations(doc.inner()).is_empty(),
        "the damaged shape is not reported"
    );
}

/// How a stored value is represented, for the coverage check below.
fn shape_of(v: &Any) -> &'static str {
    match v {
        Any::String(_) => "string",
        Any::Number(_) | Any::BigInt(_) => "number",
        Any::Bool(true) => "true",
        Any::Bool(false) => "false",
        Any::Map(m) if m.is_empty() => "{}",
        Any::Map(m) if m.contains_key("target") => "editor link",
        Any::Map(_) => "import link",
        Any::Array(_) => "array",
        Any::Null | Any::Undefined => "null",
        Any::Buffer(_) => "buffer",
    }
}

const CONTAINERS: &[&str] = &["blockquote", "bullet_list", "ordered_list", "table"];

/// The generated corpus has what the harness relies on: every node and mark,
/// containers nested four deep, and every attribute and mark in both the
/// editor's and the importer's representation (review focus 1).
#[test]
fn corpus_covers_every_node_mark_and_attribute_shape() {
    #[derive(Default)]
    struct Seen {
        tags: BTreeSet<String>,
        marks: BTreeSet<(String, &'static str)>,
        attrs: BTreeSet<(String, String, &'static str)>,
        depth: usize,
    }
    fn walk<T: ReadTxn>(txn: &T, node: &XmlOut, depth: usize, seen: &mut Seen) {
        match node {
            XmlOut::Element(el) => {
                let tag = el.tag().to_string();
                let depth = depth + usize::from(CONTAINERS.contains(&tag.as_str()));
                seen.depth = seen.depth.max(depth);
                for (k, v) in el.attributes(txn) {
                    if let Out::Any(a) = v {
                        seen.attrs
                            .insert((tag.clone(), k.to_string(), shape_of(&a)));
                    }
                }
                for i in 0..el.len(txn) {
                    if let Some(child) = el.get(txn, i) {
                        walk(txn, &child, depth, seen);
                    }
                }
                seen.tags.insert(tag);
            }
            XmlOut::Text(t) => {
                seen.tags.insert("text".into());
                for chunk in t.diff(txn, YChange::identity) {
                    for (k, v) in chunk.attributes.iter().flat_map(|a| a.iter()) {
                        seen.marks.insert((k.to_string(), shape_of(v)));
                    }
                }
            }
            XmlOut::Fragment(_) => {}
        }
    }

    let mut seen = Seen::default();
    for case in corpus::generated() {
        let ydoc = case.doc.inner();
        let txn = ydoc.transact();
        if let Some(frag) = txn.get_xml_fragment("default") {
            for i in 0..frag.len(&txn) {
                if let Some(child) = frag.get(&txn, i) {
                    walk(&txn, &child, 0, &mut seen);
                }
            }
        }
    }

    for tag in [
        "paragraph",
        "heading",
        "blockquote",
        "code_block",
        "horizontal_rule",
        "bullet_list",
        "ordered_list",
        "list_item",
        "text",
        "hard_break",
        "excalidraw_board",
        "image",
        "attachment",
        "table",
        "table_row",
        "table_cell",
        "table_header",
    ] {
        assert!(seen.tags.contains(tag), "no {tag} in the corpus");
    }
    for mark in ["bold", "italic", "code", "strike", "underline"] {
        for shape in ["{}", "true"] {
            assert!(
                seen.marks.contains(&(mark.to_string(), shape)),
                "no {mark} mark stored as {shape}"
            );
        }
    }
    for shape in ["editor link", "import link"] {
        assert!(
            seen.marks.contains(&("link".to_string(), shape)),
            "no link stored as {shape}"
        );
    }
    for (tag, attr, shapes) in [
        ("heading", "level", &["number", "string"][..]),
        ("ordered_list", "start", &["number", "string"]),
        ("list_item", "checked", &["true", "false", "string"]),
        ("table_cell", "colspan", &["number", "string"]),
        ("table_header", "colspan", &["number", "string"]),
        ("table_cell", "colwidth", &["array", "string"]),
        ("table_cell", "align", &["string"]),
        ("attachment", "size", &["number", "string"]),
    ] {
        for shape in shapes {
            assert!(
                seen.attrs
                    .contains(&(tag.to_string(), attr.to_string(), *shape)),
                "no {tag}.{attr} stored as {shape}"
            );
        }
    }
    assert!(seen.depth >= 4, "containers nest only {} deep", seen.depth);
}
