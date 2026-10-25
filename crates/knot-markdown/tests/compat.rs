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
use common::{N, Run, build, el, ela, hard_break, p, read, run, text};
use knot_crdt::{DocHandle, YrsEngine};
use knot_markdown::legacy::{self, compat};
use knot_markdown::to_markdown::MAX_DEPTH;
use yrs::types::text::YChange;
use yrs::{Any, Out, ReadTxn, Text, Transact, Xml, XmlFragment, XmlOut};

/// Seeds of the random corpus, and documents per seed.
const RANDOM_SEEDS: std::ops::RangeInclusive<u64> = 1..=50;
const RANDOM_DOCS_PER_SEED: usize = 200;

/// Lower bounds on how much each differential compares, so a broken corpus
/// can't make the harness pass by comparing nothing. Far below today's counts
/// (about 6200 healthy documents outside the classes, 3200 healthy inputs of
/// which 28 are files, 23700 exports (every corpus document), 14900 of them
/// fixed points, 8600 stability inputs, 1060 healthy classed generated
/// documents and 1090 healthy classed documents in all), leaving room for the
/// documents bug classes exclude.
const MIN_HEALTHY_DOCS: usize = 1_000;
const MIN_HEALTHY_INPUTS: usize = 500;
const MIN_HEALTHY_FILE_INPUTS: usize = 15;
const MIN_EXPORTED_DOCS: usize = 5_000;
const MIN_STABLE_INPUTS: usize = 3_000;
const MIN_CLASSED_DOCS: usize = 200;
const MIN_FIXED_POINTS: usize = 5_000;

/// How many failures a test prints before the total.
const SHOWN_FAILURES: usize = 10;

/// The generated and random documents, and the legacy import of every
/// fixture and probe: the pages the old importer stored, damaged shapes
/// included.
fn corpus_docs() -> Vec<Case> {
    let mut cases = corpus::generated();
    for seed in RANDOM_SEEDS {
        cases.extend(corpus::random(seed, RANDOM_DOCS_PER_SEED));
    }
    for (name, md) in corpus::markdown_inputs() {
        let (doc, _) = legacy::parse(&md).expect("legacy parse");
        cases.push(Case {
            name: format!("legacy import of {name}"),
            doc,
        });
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

/// The new import of every fixture and probe, and of the legacy export of
/// every corpus document (the Markdown the product wrote), is schema-valid.
#[test]
fn new_import_is_schema_valid_on_every_input() {
    let mut inputs = corpus::markdown_inputs();
    for case in corpus_docs() {
        if let Ok(md) = legacy::serialise(&YrsEngine, &case.doc) {
            inputs.push((format!("legacy export of {}", case.name), md));
        }
    }
    let mut failures = Vec::new();
    for (name, md) in inputs {
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

/// Structural idempotence (spec "Testing"): the import of the export of an
/// import is the first import again, `parse(serialise(parse(md))) ==
/// parse(md)` under `normalised`.
///
/// The inputs are the fixtures and probes, the legacy and the new export of
/// every healthy corpus document (the Markdown the product writes today and
/// from now on), and the new export of every generated document, which
/// holds the nested checklists and multi-block items legacy damaged, and the
/// images in cells and empty quotes the importer lost (Task 4b). Left out
/// are inputs with text the writer can't write back yet
/// (`compat::stability_exempt`, a follow-up), and the random corpus, whose
/// text is full of Markdown syntax: the same follow-up.
#[test]
fn import_export_import_is_stable() {
    let file_inputs = corpus::markdown_inputs();
    let file_count = file_inputs.len();
    let mut inputs = file_inputs;
    for case in corpus_docs() {
        if let Some(legacy_md) = compat::healthy(&case.doc) {
            inputs.push((format!("legacy export of {}", case.name), legacy_md));
            if let Ok(md) = knot_markdown::serialise(&YrsEngine, &case.doc) {
                inputs.push((format!("export of healthy {}", case.name), md));
            }
        }
    }
    for case in corpus::generated() {
        if let Ok(md) = knot_markdown::serialise(&YrsEngine, &case.doc) {
            inputs.push((format!("export of {}", case.name), md));
        }
    }

    let mut seen = HashSet::new();
    let mut checked = 0;
    let mut checked_files = 0;
    let mut failures = Vec::new();
    for (i, (name, md)) in inputs.iter().enumerate() {
        if !seen.insert(md.as_str()) || compat::stability_exempt(md).is_some() {
            continue;
        }
        checked += 1;
        if i < file_count {
            checked_files += 1;
        }
        let (first, _) = knot_markdown::from_markdown::parse(md).expect("parse");
        let want = compat::normalised(first.inner());
        let again = match knot_markdown::serialise(&YrsEngine, &first) {
            Ok(again) => again,
            Err(e) => {
                failures.push(format!("{name}: {md:?}\n  its import doesn't export: {e}"));
                continue;
            }
        };
        let (second, _) = knot_markdown::from_markdown::parse(&again).expect("parse");
        let got = compat::normalised(second.inner());
        if got != want {
            failures.push(format!(
                "{name}: {md:?}\n  exports as {again:?}\n  first import:\n{want}\n  import of the export:\n{got}"
            ));
        }
    }
    assert_no_failures("import of the export differs from the import", &failures);
    assert_at_least("inputs", checked, MIN_STABLE_INPUTS);
    assert_at_least(
        "fixture and probe inputs",
        checked_files,
        MIN_HEALTHY_FILE_INPUTS,
    );
}

/// Each export class holds the fixed-point property under the new code: a
/// healthy generated document in a class exports to Markdown that the new
/// importer and serializer give back unchanged. That includes items the old
/// importer stored without their leading paragraph, which the importer adds
/// back: their export already starts with it.
#[test]
fn export_classes_hold_the_fixed_point() {
    let mut compared = 0;
    let mut failures = Vec::new();
    for case in corpus::generated() {
        let Some(class) = compat::export_class(&case.doc) else {
            continue;
        };
        if compat::healthy(&case.doc).is_none() {
            continue;
        }
        compared += 1;
        let md = knot_markdown::serialise(&YrsEngine, &case.doc).expect("serialise");
        let (back, _) = knot_markdown::from_markdown::parse(&md).expect("parse");
        let again = knot_markdown::serialise(&YrsEngine, &back).expect("serialise");
        if again != md {
            failures.push(format!(
                "{} ({class})\n  export:    {md:?}\n  re-export: {again:?}",
                case.name
            ));
        }
    }
    assert_no_failures("export class export is not a fixed point", &failures);
    assert_at_least("classed healthy documents", compared, MIN_CLASSED_DOCS);
}

// ---------------------------------------------------------------------------
// Serializer rule 2: the new export is a fixed point
// ---------------------------------------------------------------------------

/// Why the new export of a document may not be a fixed point yet: a limit of
/// Markdown or of the writer (a follow-up), each with the repair that takes
/// that cause out of the document. The repairs apply one after the other, in
/// this order, and a document whose export isn't a fixed point must become
/// one on the way; it counts for the class whose repair got it there. So the
/// classes explain every such document together, by their causes, and any
/// other cause fails the test.
///
/// Measured over `corpus_docs()`: 23,705 documents, 14,861 of them fixed
/// points as they are. Then, per class: code_block_in_cell 1,904,
/// indented_image_in_cell 10, table_in_cell 101, hard_break_in_heading 177,
/// edge_hard_break 799, empty_paragraph 1,630, unwritten_item 15,
/// adjacent_lists 73, empty_last_item_in_quote 0 (it is needed, before
/// markdown_escaping, by one document), markdown_escaping 3,430,
/// mark_delimiters 705. The importer's own causes are gone (Task 4b): code
/// blocks in containers, images in cells and empty quotes read back as
/// written.
const FIXED_POINT_CLASSES: &[(&str, Repair)] = &[
    ("code_block_in_cell", code_block_in_cell),
    ("indented_image_in_cell", indented_image_in_cell),
    ("table_in_cell", table_in_cell),
    ("hard_break_in_heading", hard_break_in_heading),
    ("edge_hard_break", edge_hard_break),
    ("empty_paragraph", empty_paragraph),
    ("unwritten_item", unwritten_item),
    ("adjacent_lists", adjacent_lists),
    ("empty_last_item_in_quote", empty_last_item_in_quote),
    ("markdown_escaping", markdown_escaping),
    ("mark_delimiters", mark_delimiters),
];

/// A document's blocks with one cause of a non-fixed-point export taken out.
type Repair = fn(&[N]) -> Vec<N>;

const CELLS: &[&str] = &["table_cell", "table_header"];

/// The tags of `tools/schema.json`'s block group.
const BLOCK_TAGS: &[&str] = &[
    "paragraph",
    "heading",
    "blockquote",
    "code_block",
    "horizontal_rule",
    "bullet_list",
    "ordered_list",
    "excalidraw_board",
    "image",
    "attachment",
    "table",
];

/// `blocks` with every node replaced, bottom-up, by what `f` returns for it:
/// `f` gets the node with its children already replaced, and the tags of its
/// ancestors, outermost first.
fn rewrite(blocks: &[N], f: &dyn Fn(N, &[&'static str]) -> Vec<N>) -> Vec<N> {
    fn go(
        nodes: &[N],
        ancestors: &mut Vec<&'static str>,
        f: &dyn Fn(N, &[&'static str]) -> Vec<N>,
    ) -> Vec<N> {
        let mut out = Vec::new();
        for node in nodes {
            let node = match node {
                N::El(tag, attrs, children) => {
                    ancestors.push(tag);
                    let children = go(children, ancestors, f);
                    ancestors.pop();
                    N::El(tag, attrs.clone(), children)
                }
                text => text.clone(),
            };
            out.extend(f(node, ancestors));
        }
        out
    }
    go(blocks, &mut Vec::new(), f)
}

/// What replaces the children of a block container, given its tag (`None`
/// at the top level).
type ChildrenRewrite<'a> = &'a dyn Fn(Vec<N>, Option<&str>) -> Vec<N>;

/// `blocks` with `f` applied to the top-level blocks and to the children of
/// every block container.
fn rewrite_containers(blocks: &[N], f: ChildrenRewrite) -> Vec<N> {
    let inner = rewrite(blocks, &|node, _| match node {
        N::El(tag, attrs, children)
            if matches!(tag, "list_item" | "blockquote") || CELLS.contains(&tag) =>
        {
            vec![N::El(tag, attrs, f(children, Some(tag)))]
        }
        node => vec![node],
    });
    f(inner, None)
}

fn text_of(node: &N) -> String {
    match node {
        N::El(_, _, children) => children.iter().map(text_of).collect(),
        N::Text(runs) => runs.iter().map(|r| r.text.as_str()).collect(),
    }
}

fn is_tag(node: &N, want: &str) -> bool {
    matches!(node, N::El(tag, _, _) if *tag == want)
}

fn is_empty_text(node: &N) -> bool {
    matches!(node, N::Text(runs) if runs.iter().all(|r| r.text.is_empty()))
}

/// Whether `node`, a child of a block container, is written as no text: an
/// empty paragraph, a node the schema doesn't name without text, or a stray
/// hard break or empty text.
fn writes_nothing(node: &N) -> bool {
    match node {
        N::El("paragraph", _, children) => children.iter().all(is_empty_text),
        N::El("hard_break", _, _) => true,
        N::El(tag, _, _) => {
            !BLOCK_TAGS.contains(tag)
                && !matches!(
                    *tag,
                    "list_item" | "table_row" | "table_cell" | "table_header"
                )
                && text_of(node).is_empty()
        }
        N::Text(_) => is_empty_text(node),
    }
}

/// A paragraph of `text`, without marks.
fn paragraph_of(text: String) -> N {
    el("paragraph", vec![N::Text(vec![run(&text, Vec::new())])])
}

/// The document as the writer reads it: a node the schema doesn't name in a
/// paragraph or heading as its text (P4). Not a class: it writes the same
/// Markdown, which the test checks.
fn as_written(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, ancestors| match node {
        N::El(tag, _, _)
            if tag != "hard_break"
                && matches!(ancestors.last(), Some(&("paragraph" | "heading"))) =>
        {
            vec![N::Text(vec![run(&text_of(&node), Vec::new())])]
        }
        node => vec![node],
    })
}

/// A code block in a table cell: a GFM cell is one line, so its fences and
/// lines are joined onto it and read back as inline code.
fn code_block_in_cell(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, ancestors| match node {
        N::El("code_block", ..) if ancestors.iter().any(|a| CELLS.contains(a)) => {
            vec![paragraph_of(text_of(&node).replace('\n', " "))]
        }
        node => vec![node],
    })
}

/// An image or board in a list item in a table cell: a GFM cell is one line,
/// so the item's lines join it with their indentation, a run of spaces before
/// or after the image, and the importer, which splits the line into blocks at
/// the image, reads that run as the one space the blocks are joined with.
fn indented_image_in_cell(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, ancestors| {
        let image = is_tag(&node, "image") || is_tag(&node, "excalidraw_board");
        let in_cell_item = ancestors
            .iter()
            .position(|a| CELLS.contains(a))
            .is_some_and(|cell| ancestors[cell..].contains(&"list_item"));
        if image && in_cell_item {
            Vec::new()
        } else {
            vec![node]
        }
    })
}

/// A table in a table cell: it is flattened onto the cell's line with its
/// pipes escaped, and the escapes of a table nested two deep read back one
/// level less escaped (the writer doesn't escape backslashes: the escaping
/// follow-up).
fn table_in_cell(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, ancestors| match node {
        N::El("table", ..) if ancestors.iter().any(|a| CELLS.contains(a)) => {
            vec![paragraph_of(text_of(&node))]
        }
        node => vec![node],
    })
}

/// A hard break in a heading: an ATX heading is one line, so the text after
/// the break reads back as a paragraph below it.
fn hard_break_in_heading(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, _| match node {
        N::El("heading", attrs, children) => vec![N::El(
            "heading",
            attrs,
            children
                .into_iter()
                .filter(|c| !is_tag(c, "hard_break"))
                .collect(),
        )],
        node => vec![node],
    })
}

/// A hard break before the first text of a paragraph or heading, after its
/// last, or right after another: it writes an empty line, which ends the
/// paragraph, and Markdown has no line break at either end of one.
fn edge_hard_break(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, _| match node {
        N::El(tag @ ("paragraph" | "heading"), attrs, children) => {
            let mut kept: Vec<N> = Vec::new();
            for child in children.into_iter().filter(|c| !is_empty_text(c)) {
                let is_break = is_tag(&child, "hard_break");
                if is_break && kept.last().is_none_or(|last| is_tag(last, "hard_break")) {
                    continue;
                }
                kept.push(child);
            }
            while kept.last().is_some_and(|last| is_tag(last, "hard_break")) {
                kept.pop();
            }
            vec![N::El(tag, attrs, kept)]
        }
        node => vec![node],
    })
}

/// A block written as no text, an empty paragraph above all: it leaves a
/// blank line, which Markdown reads as nothing, so the blocks around it close
/// up. Markdown has no empty paragraph. A container of nothing else reads
/// back as one empty paragraph: an empty item `- `, a quote `>`.
fn empty_paragraph(blocks: &[N]) -> Vec<N> {
    rewrite_containers(blocks, &|children, container| {
        let had = !children.is_empty();
        let mut kept: Vec<N> = children
            .into_iter()
            .filter(|c| !writes_nothing(c))
            .collect();
        if had && kept.is_empty() && container.is_some() {
            kept.push(el("paragraph", Vec::new()));
        }
        kept
    })
}

/// A list item without blocks or checkbox: it is written as nothing, as
/// legacy wrote it (a bullet list round-trips that way), so in an ordered list
/// the numbers after it skip one and read back renumbered.
fn unwritten_item(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, _| match node {
        N::El(tag @ ("bullet_list" | "ordered_list"), attrs, items) => {
            let items: Vec<N> = items
                .into_iter()
                .filter(|item| {
                    !matches!(item, N::El("list_item", attrs, children)
                        if children.is_empty() && !attrs.iter().any(|(k, _)| *k == "checked"))
                })
                .collect();
            if items.is_empty() {
                Vec::new()
            } else {
                vec![N::El(tag, attrs, items)]
            }
        }
        node => vec![node],
    })
}

/// Two lists of the same kind, one right after the other: Markdown has no way
/// to keep them apart, so they read back as one list.
fn adjacent_lists(blocks: &[N]) -> Vec<N> {
    rewrite_containers(blocks, &|children, _| {
        let mut out: Vec<N> = Vec::new();
        for child in children {
            // Blocks between them that are written as nothing don't keep
            // them apart.
            let mut prev = out.len();
            while prev > 0 && writes_nothing(&out[prev - 1]) {
                prev -= 1;
            }
            if let (Some(N::El(prev_tag, ..)), N::El(tag, _, items)) =
                (prev.checked_sub(1).and_then(|i| out.get(i)), &child)
                && prev_tag == tag
                && matches!(*tag, "bullet_list" | "ordered_list")
            {
                let items = items.clone();
                out.truncate(prev);
                if let Some(N::El(_, _, prev_items)) = out.last_mut() {
                    prev_items.extend(items);
                }
                continue;
            }
            out.push(child);
        }
        out
    })
}

/// An empty item without checkbox last in a loose list in a blockquote in a
/// list item: when a blank line follows, pulldown-cmark ends that list item
/// there too, and the blocks after it read back outside (an importer quirk).
fn empty_last_item_in_quote(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, ancestors| match node {
        N::El(tag @ ("bullet_list" | "ordered_list"), attrs, mut items)
            if ancestors
                .iter()
                .position(|a| *a == "list_item")
                .is_some_and(|i| ancestors[i..].contains(&"blockquote")) =>
        {
            if items.len() > 1
                && let Some(N::El("list_item", item_attrs, children)) = items.last()
                && !item_attrs.iter().any(|(k, _)| *k == "checked")
                && children.iter().all(writes_nothing)
            {
                items.pop();
            }
            vec![N::El(tag, attrs, items)]
        }
        node => vec![node],
    })
}

/// Text holding Markdown syntax, or spaces Markdown drops: the writer doesn't
/// escape text (a non-goal and follow-up issue). The repair makes every
/// ASCII punctuation character, space and tab an `x`.
fn markdown_escaping(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, _| match node {
        N::Text(runs) => vec![N::Text(
            runs.into_iter()
                .map(|r| Run {
                    text: r
                        .text
                        .chars()
                        .map(|c| {
                            if c.is_ascii_punctuation() || c == ' ' || c == '\t' {
                                'x'
                            } else {
                                c
                            }
                        })
                        .collect(),
                    marks: r.marks,
                })
                .collect(),
        )],
        node => vec![node],
    })
}

/// Marks next to each other: the writer closes and reopens the delimiters at
/// every run (`~~a~~~~b~~`), which CommonMark reads as other delimiters. A
/// writer that nests marks across runs is the follow-up of the escaping one.
/// The repair drops the marks.
fn mark_delimiters(blocks: &[N]) -> Vec<N> {
    rewrite(blocks, &|node, _| match node {
        N::Text(runs) => vec![N::Text(
            runs.into_iter()
                .map(|r| Run {
                    text: r.text,
                    marks: Vec::new(),
                })
                .collect(),
        )],
        node => vec![node],
    })
}

/// The new export of `doc`, if it is a fixed point, or else the export and the
/// re-export.
fn fixed_point(doc: &DocHandle) -> Result<String, (String, String)> {
    let md = knot_markdown::serialise(&YrsEngine, doc).expect("serialise");
    let (back, _) = knot_markdown::from_markdown::parse(&md).expect("parse");
    let again = knot_markdown::serialise(&YrsEngine, &back).expect("serialise");
    if again == md {
        Ok(md)
    } else {
        Err((md, again))
    }
}

/// Serializer rule 2: the new export of every document is a fixed point of
/// the new importer and serializer, `serialise(parse(e)) == e`, except where
/// a cause in [`FIXED_POINT_CLASSES`] explains why not. Every class's repair
/// must change a document the classes explain, so a class whose cause is
/// fixed is noticed and removed.
#[test]
fn new_export_is_a_fixed_point() {
    let mut fixed = 0;
    // Per class: the documents it is the last repair of, and whether its
    // repair took something out of any document the classes explain.
    let mut classed = vec![0usize; FIXED_POINT_CLASSES.len()];
    let mut used = vec![false; FIXED_POINT_CLASSES.len()];
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let (md, again) = match fixed_point(&case.doc) {
            Ok(_) => {
                fixed += 1;
                continue;
            }
            Err(exports) => exports,
        };
        let mut blocks = as_written(&read(&case.doc));
        let read_back = knot_markdown::serialise(&YrsEngine, &build(&blocks)).expect("serialise");
        if read_back != md {
            failures.push(format!(
                "{}: the document as read exports otherwise\n  export:  {md:?}\n  as read: {read_back:?}",
                case.name
            ));
            continue;
        }
        let mut changed = Vec::new();
        let class = FIXED_POINT_CLASSES.iter().position(|(_, repair)| {
            let repaired = repair(&blocks);
            changed.push(repaired != blocks);
            // An unchanged document stays as it was: no fixed point.
            if repaired == blocks {
                return false;
            }
            blocks = repaired;
            fixed_point(&build(&blocks)).is_ok()
        });
        match class {
            Some(i) => {
                classed[i] += 1;
                for (used, changed) in used.iter_mut().zip(changed) {
                    *used |= changed;
                }
            }
            None => failures.push(format!(
                "{}\n  export:    {md:?}\n  re-export: {again:?}",
                case.name
            )),
        }
    }
    assert_no_failures("export is not a fixed point", &failures);
    assert_at_least("fixed points", fixed, MIN_FIXED_POINTS);
    let unused: Vec<&str> = FIXED_POINT_CLASSES
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((name, _), _)| *name)
        .collect();
    assert!(
        unused.is_empty(),
        "fixed-point classes whose repair changes no document they explain, to remove: {unused:?}"
    );
}

/// Hand-built documents with the export class each falls in: the documents
/// whose export a class changes, and neighbouring documents that export byte
/// for byte as before.
fn class_cases() -> Vec<(&'static str, Vec<N>, Option<&'static str>)> {
    let task =
        |checked: bool, kids: Vec<N>| ela("list_item", vec![("checked", Any::Bool(checked))], kids);
    let item = |kids: Vec<N>| el("list_item", kids);
    let bullets = |items: Vec<N>| el("bullet_list", items);
    let ordered = |start: i64, items: Vec<N>| {
        ela(
            "ordered_list",
            vec![("start", Any::Number(start as f64))],
            items,
        )
    };
    let quote = |kids: Vec<N>| el("blockquote", kids);
    let cell = |kids: Vec<N>| {
        el(
            "table",
            vec![
                el("table_row", vec![el("table_header", vec![p("h")])]),
                el("table_row", vec![el("table_cell", kids)]),
            ],
        )
    };
    let image = || {
        ela(
            "image",
            vec![("src", Any::from("x.png")), ("alt", Any::from("x"))],
            Vec::new(),
        )
    };
    let attachment = || {
        ela(
            "attachment",
            vec![("url", Any::from("/api/blobs/1")), ("name", Any::from("f"))],
            Vec::new(),
        )
    };
    let rule = || el("horizontal_rule", Vec::new());
    let heading = || ela("heading", vec![("level", Any::from("2"))], vec![text("h")]);
    let code = || el("code_block", vec![text("c")]);
    let line_break = || el("paragraph", vec![text("a"), hard_break(), text("b")]);
    let nested = |inner: N| vec![p("a"), bullets(vec![inner])];
    let code_in = |lead: &str| vec![p(lead), code()];
    // `n` blockquotes, one in the other, and `n` lists, each in an item of
    // the one before. Every list level is two elements deep.
    let quotes = |n: usize| (0..n).fold(vec![p("deep")], |kids, _| vec![quote(kids)]);
    let lists = |n: usize| {
        (0..n).fold(Vec::new(), |kids, _| {
            vec![bullets(vec![item([vec![p("x")], kids].concat())])]
        })
    };

    vec![
        // Under the limit, as before; past it, written as text.
        (
            "quotes nested just under the depth limit",
            quotes(MAX_DEPTH - 1),
            None,
        ),
        (
            "quotes nested past the depth limit",
            quotes(MAX_DEPTH + 1),
            Some("depth_limit"),
        ),
        (
            "lists nested just under the depth limit",
            lists((MAX_DEPTH - 1) / 2),
            None,
        ),
        (
            "lists nested past the depth limit",
            lists(MAX_DEPTH + 1),
            Some("depth_limit"),
        ),
        (
            "nested checklist",
            vec![bullets(vec![task(false, nested(task(true, vec![p("b")])))])],
            Some("nested_tasklist"),
        ),
        (
            "task item with a line break",
            vec![bullets(vec![task(false, vec![line_break()])])],
            Some("nested_tasklist"),
        ),
        (
            "task item starting with a heading",
            vec![bullets(vec![task(true, vec![heading()])])],
            Some("nested_tasklist"),
        ),
        (
            "task item without blocks",
            vec![bullets(vec![task(true, Vec::new())])],
            Some("nested_tasklist"),
        ),
        (
            "one-line task items",
            vec![bullets(vec![
                task(false, vec![p("a")]),
                task(true, vec![p("b")]),
            ])],
            None,
        ),
        (
            "task item holding an attachment",
            vec![bullets(vec![task(true, vec![attachment()])])],
            None,
        ),
        (
            "bullet with a nested bullet",
            vec![bullets(vec![item(nested(item(vec![p("b")])))])],
            None,
        ),
        (
            "nested checklist in a table cell",
            vec![cell(vec![bullets(vec![task(
                false,
                nested(task(true, vec![p("b")])),
            )])])],
            None,
        ),
        (
            "two paragraphs in an item",
            vec![bullets(vec![item(vec![p("a"), p("b")])])],
            Some("multi_block_item"),
        ),
        (
            "a paragraph and an image in an item",
            vec![bullets(vec![item(vec![p("a"), image()])])],
            Some("multi_block_item"),
        ),
        (
            "a paragraph and a list at 3 in an item",
            vec![ordered(
                2,
                vec![item(vec![p("a"), ordered(3, vec![item(vec![p("o")])])])],
            )],
            Some("multi_block_item"),
        ),
        (
            "a paragraph after a nested list",
            vec![bullets(vec![item(vec![
                p("a"),
                bullets(vec![item(vec![p("b")])]),
                p("c"),
            ])])],
            Some("multi_block_item"),
        ),
        (
            "a paragraph after a quote in an item",
            vec![bullets(vec![item(vec![
                p("a"),
                quote(vec![p("q")]),
                p("c"),
            ])])],
            Some("multi_block_item"),
        ),
        (
            "blocks that end or interrupt the block above in an item",
            vec![bullets(vec![item(vec![
                p("a"),
                code(),
                p("b"),
                ordered(1, vec![item(vec![p("o")])]),
                ordered(3, vec![item(vec![p("t")])]),
                heading(),
                p("c"),
                cell(vec![p("d")]),
            ])])],
            None,
        ),
        (
            "two paragraphs in a table cell's item",
            vec![cell(vec![bullets(vec![item(vec![p("a"), p("b")])])])],
            None,
        ),
        (
            "a list then a paragraph in a quote",
            vec![quote(vec![bullets(vec![item(vec![p("a")])]), p("b")])],
            Some("quote_block_sequence"),
        ),
        (
            "a paragraph then a list at 3 in a quote",
            vec![quote(vec![p("a"), ordered(3, vec![item(vec![p("o")])])])],
            Some("quote_block_sequence"),
        ),
        (
            "two quotes in a quote",
            vec![quote(vec![quote(vec![p("a")]), quote(vec![p("b")])])],
            Some("quote_block_sequence"),
        ),
        (
            "an image then a paragraph in a quote",
            vec![quote(vec![image(), p("b")])],
            Some("quote_block_sequence"),
        ),
        (
            "paragraphs, an image and an attachment in a quote",
            vec![
                quote(vec![p("a"), p("b"), image()]),
                quote(vec![p("a"), attachment(), p("c")]),
            ],
            None,
        ),
        (
            "an ordered task item",
            vec![ordered(1, vec![task(true, vec![p("a")])])],
            Some("ordered_checkbox"),
        ),
        (
            "an ordered task item in a table cell",
            vec![cell(vec![ordered(1, vec![task(false, vec![p("a")])])])],
            Some("ordered_checkbox"),
        ),
        (
            "an ordered list",
            vec![ordered(1, vec![item(vec![p("a")]), item(vec![p("b")])])],
            None,
        ),
        (
            "a rule under a paragraph in an item",
            vec![bullets(vec![item(vec![p("a"), rule()])])],
            Some("rule_in_container"),
        ),
        (
            "a rule under an image in a quote",
            vec![quote(vec![image(), rule()])],
            Some("rule_in_container"),
        ),
        (
            "rules after a heading, a list and a quote",
            vec![
                quote(vec![heading(), rule()]),
                bullets(vec![item(vec![
                    p("a"),
                    bullets(vec![item(vec![p("b")])]),
                    rule(),
                    quote(vec![p("q")]),
                    rule(),
                ])]),
            ],
            None,
        ),
        // A paragraph, image or table right below a list or quote whose last
        // line is a heading's or a code block's has no paragraph to go on:
        // legacy's output reads back as written.
        (
            "a paragraph after a list ending in a code block, in a quote",
            vec![quote(vec![bullets(vec![item(code_in("b"))]), p("para")])],
            None,
        ),
        (
            "a paragraph after a list ending in a code block, in an item",
            vec![bullets(vec![item(vec![
                p("x"),
                bullets(vec![item(code_in("b"))]),
                p("para"),
            ])])],
            None,
        ),
        (
            "a paragraph after a quoted heading, in a quote",
            vec![quote(vec![quote(vec![heading()]), p("para")])],
            None,
        ),
        (
            "a paragraph after a quote ending in a code block, in a quote",
            vec![quote(vec![quote(code_in("q")), p("para")])],
            None,
        ),
        (
            "a paragraph after a quote ending in a code block, in an item",
            vec![bullets(vec![item(vec![
                p("x"),
                quote(code_in("q")),
                p("para"),
            ])])],
            None,
        ),
        (
            "a paragraph after a list ending in a heading, in an item",
            vec![bullets(vec![item(vec![
                p("x"),
                bullets(vec![item(vec![p("b"), heading()])]),
                p("para"),
            ])])],
            None,
        ),
        (
            "an image after a list ending in a code block, in a quote",
            vec![quote(vec![bullets(vec![item(code_in("b"))]), image()])],
            None,
        ),
        (
            "a table after a quoted heading, in an item",
            vec![bullets(vec![item(vec![
                p("x"),
                quote(vec![heading()]),
                cell(vec![p("c")]),
            ])])],
            None,
        ),
        (
            "a paragraph after a list ending in a paragraph, in a quote",
            vec![quote(vec![
                bullets(vec![item(vec![p("b"), code(), p("c")])]),
                p("para"),
            ])],
            Some("quote_block_sequence"),
        ),
        // A list's first line is its first item that is written at all: an
        // item without blocks or checkbox is written as nothing.
        (
            "a nested list starting with an item without blocks",
            vec![bullets(vec![item(vec![
                p("x"),
                bullets(vec![item(Vec::new()), item(vec![p("b")])]),
            ])])],
            None,
        ),
        (
            "a nested list at 0 starting with an item without blocks",
            vec![bullets(vec![item(vec![
                p("x"),
                ordered(0, vec![item(Vec::new()), item(vec![p("b")])]),
            ])])],
            None,
        ),
        (
            "a nested list at 1 starting with an item without blocks",
            vec![bullets(vec![item(vec![
                p("x"),
                ordered(1, vec![item(Vec::new()), item(vec![p("b")])]),
            ])])],
            Some("multi_block_item"),
        ),
    ]
}

/// Each export class names the documents whose export it changes, and
/// leaves alone neighbouring documents, which export byte for byte as
/// before.
#[test]
fn export_classes_name_their_documents() {
    let mut failures = Vec::new();
    for (what, blocks, want) in class_cases() {
        let doc = build(&blocks);
        let got = compat::export_class(&doc);
        if got != want {
            failures.push(format!("{what}: class {got:?}, expected {want:?}"));
        }
        let legacy_md = legacy::serialise(&YrsEngine, &doc).expect("legacy serialise");
        let md = knot_markdown::serialise(&YrsEngine, &doc).expect("serialise");
        if (md != legacy_md) != want.is_some() {
            failures.push(format!(
                "{what}: class {want:?}, but the export changed: {}\n  legacy: {legacy_md:?}\n  new:    {md:?}",
                md != legacy_md
            ));
        }
    }
    assert_no_failures("export class is wrong", &failures);
}

/// Every healthy document an export class names is one legacy damaged: its
/// legacy export reads back under legacy as another tree, or reads back
/// under the new importer as another tree than the new export does. The
/// byte-identity differential skips classed documents, so a class that also
/// named a document legacy wrote losslessly would let the writer change that
/// healthy page's bytes unseen.
#[test]
fn export_classes_name_only_damaged_documents() {
    let import = |md: &str| {
        let (doc, _) = knot_markdown::from_markdown::parse(md).expect("parse");
        compat::normalised(doc.inner())
    };
    let mut docs: Vec<(String, DocHandle)> = corpus_docs()
        .into_iter()
        .map(|case| (case.name, case.doc))
        .collect();
    docs.extend(
        class_cases()
            .into_iter()
            .map(|(what, blocks, _)| (format!("case: {what}"), build(&blocks))),
    );
    let mut compared = 0;
    let mut failures = Vec::new();
    for (name, doc) in docs {
        let Some(class) = compat::export_class(&doc) else {
            continue;
        };
        let Some(legacy_md) = compat::healthy(&doc) else {
            continue;
        };
        compared += 1;
        let (legacy_back, _) = legacy::parse(&legacy_md).expect("legacy parse");
        if compat::normalised(legacy_back.inner()) != compat::normalised(doc.inner()) {
            continue;
        }
        let md = knot_markdown::serialise(&YrsEngine, &doc).expect("serialise");
        if import(&legacy_md) != import(&md) {
            continue;
        }
        failures.push(format!(
            "{name} ({class}): legacy's {legacy_md:?} reads back as written, and the new {md:?} reads back the same"
        ));
    }
    assert_no_failures("export class names a lossless document", &failures);
    assert_at_least("classed healthy documents", compared, MIN_CLASSED_DOCS);
}

/// `damaged_shape` names the pages with text or a hard break directly in a
/// block container, and only pages legacy couldn't export at all: it excuses
/// no healthy page's export.
#[test]
fn damaged_shape_names_pages_legacy_could_not_export() {
    let item = |kids: Vec<N>| vec![el("bullet_list", vec![el("list_item", kids)])];
    let cell = |kids: Vec<N>| {
        vec![el(
            "table",
            vec![el("table_row", vec![el("table_cell", kids)])],
        )]
    };
    let cases: Vec<(&str, Vec<N>, Option<&str>)> = vec![
        (
            "a line break split out of an item",
            item(vec![p("a"), hard_break(), p("b")]),
            Some("damaged_shape"),
        ),
        (
            "a soft break split out of an item",
            item(vec![p("a"), text(" "), p("b")]),
            Some("damaged_shape"),
        ),
        (
            "text at the top level",
            vec![text("x")],
            Some("damaged_shape"),
        ),
        (
            "a hard break in a quote",
            vec![el("blockquote", vec![p("a"), hard_break()])],
            Some("damaged_shape"),
        ),
        (
            "text in a cell",
            cell(vec![text("x")]),
            Some("damaged_shape"),
        ),
        (
            "a line break in an item's paragraph",
            item(vec![el(
                "paragraph",
                vec![text("a"), hard_break(), text("b")],
            )]),
            None,
        ),
        (
            "a hard break in a code block",
            vec![el("code_block", vec![text("x"), hard_break()])],
            None,
        ),
        ("an unknown node", vec![el("callout", vec![p("hi")])], None),
    ];
    let mut failures = Vec::new();
    for (what, blocks, want) in cases {
        let got = compat::export_class(&build(&blocks));
        if got != want {
            failures.push(format!("{what}: class {got:?}, expected {want:?}"));
        }
    }
    let mut named = 0;
    for case in corpus_docs() {
        if compat::export_class(&case.doc) != Some("damaged_shape") {
            continue;
        }
        named += 1;
        if let Ok(md) = legacy::serialise(&YrsEngine, &case.doc) {
            failures.push(format!("{}: legacy exports {md:?}", case.name));
        }
    }
    assert_no_failures("damaged_shape is wrong", &failures);
    assert!(named > 0, "no corpus document is a damaged shape");
}

/// The stability exemptions name the inputs they are about, and none of them
/// is an import class: those inputs import as they always did.
#[test]
fn stability_exemptions_name_their_inputs() {
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
    ];
    let mut failures = Vec::new();
    for (md, want) in cases {
        let got = compat::stability_exempt(md);
        if got != *want {
            failures.push(format!("{md:?}: exemption {got:?}, expected {want:?}"));
        }
        if let Some(class) = compat::import_class(md)
            && matches!(class, "markdown_escaping" | "leading_hard_break")
        {
            failures.push(format!("{md:?}: import class {class}"));
        }
    }
    assert_no_failures("stability exemption is wrong", &failures);
}

/// Each import class names the inputs it is about, and leaves alone
/// neighbouring inputs that import the same way as before.
#[test]
fn import_classes_name_their_inputs() {
    let cases: &[(&str, Option<&str>)] = &[
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
        (
            "- a\n  # h\n  ![x](y.png)\n",
            Some("image_after_block_in_tight_item"),
        ),
        (
            "- a\n  # h\n  ![l](knot://board/00000000-0000-0000-0000-000000000001.svg)\n",
            Some("image_after_block_in_tight_item"),
        ),
        (
            "- a\n  ```\n  c\n  ```\n  *![x](y.png)*\n",
            Some("image_after_block_in_tight_item"),
        ),
        ("- a\n  # h\n  b\n", None),
        ("- a\n  # h\n  ![x](y.png) b\n", None),
        ("- a\n  # h\n  ![x](a.png)![y](b.png)\n", None),
        ("- a\n  # h\n  <span></span>\n", None),
        ("- a\n\n  # h\n\n  ![x](y.png)\n", None),
        (
            "- a\n  ```\n  x\n  y\n  ```\n",
            Some("code_block_line_breaks"),
        ),
        (
            "> ```\n> x\n>\n> y\n> ```\n",
            Some("code_block_line_breaks"),
        ),
        ("    x\n    y\n", Some("code_block_line_breaks")),
        (
            "- a\n  ```\n  x\n  \n  ```\n",
            Some("code_block_line_breaks"),
        ),
        ("- a\n  ```\n  x\n  ```\n", None),
        ("```\nx\n\ny\n```\n", None),
        ("```\nx\r\ny\r\n```\n", None),
        ("    x\n", None),
        (">\n", Some("empty_blockquote")),
        ("> <!-- c -->\n", Some("empty_blockquote")),
        ("> >\n", Some("empty_blockquote")),
        ("> a\n>\n", None),
        (
            "| h | i |\n| --- | --- |\n| ![x](x.png) | b |\n\n![z](z.png)\n",
            Some("cell_image_candidate_leak"),
        ),
        (
            "| ![x](x.png) b |\n| --- |\n\n> ![z](z.png)\n",
            Some("cell_image_candidate_leak"),
        ),
        (
            "| ![l](knot://board/00000000-0000-0000-0000-000000000001.svg) |\n| --- |\n\n![z](z.png)\n",
            Some("cell_image_candidate_leak"),
        ),
        // An image left by a cell loses to the paragraph's own board, and
        // any candidate goes when a paragraph ends.
        (
            "| ![x](x.png) |\n| --- |\n\n![l](knot://board/00000000-0000-0000-0000-000000000001.svg)\n",
            Some("image_in_cell"),
        ),
        (
            "| ![x](x.png) |\n| --- |\n\na\n\n![z](z.png)\n",
            Some("image_in_cell"),
        ),
        // Text before the image: the cell leaves no candidate.
        (
            "| a ![x](x.png) |\n| --- |\n\n![z](z.png)\n",
            Some("image_in_cell"),
        ),
        ("| h |\n| --- |\n| a ![x](x.png) |\n", Some("image_in_cell")),
        (
            "| ![l](knot://board/00000000-0000-0000-0000-000000000001.svg) b |\n| --- |\n",
            Some("image_in_cell"),
        ),
        ("| h |\n| --- |\n| a |\n\n![x](x.png)\n", None),
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

/// Serializer rule 2: no export loses a text character of its page
/// (`compat::doc_text`, `compat::first_lost_char`, which the real-data
/// check applies too).
#[test]
fn no_text_is_lost() {
    let mut exported = 0;
    let mut failures = Vec::new();
    for case in corpus_docs() {
        let md = match knot_markdown::serialise(&YrsEngine, &case.doc) {
            Ok(md) => md,
            Err(e) => {
                // A failed export loses everything. The tolerant export
                // writes damaged and unknown content too, so it never fails.
                failures.push(format!("{}: the export fails: {e}", case.name));
                continue;
            }
        };
        exported += 1;
        let text = compat::doc_text(&case.doc);
        if let Some((i, c)) = compat::first_lost_char(&text, &md) {
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
