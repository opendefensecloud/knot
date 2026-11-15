//! `pm_json` parity with y-tiptap's `yXmlFragmentToProsemirrorJSON` (the
//! architecture's open spike "pm_json parity"), over two fixtures in
//! `tests/fixtures`:
//!
//! - `pm_json_golden.json`: hand-built cases written by y-tiptap itself
//!   (`web/src/features/docs/history/pmJsonGolden.test.ts`);
//! - `pm_json_corpus.json`: a sample of the knot-markdown differential
//!   corpus (Editor and Import shapes, random documents, the Markdown
//!   fixtures), whose updates this file writes and whose `expected` the
//!   vitest fills in.
//!
//! Regenerate both (from the repo root):
//!
//! ```text
//! cd web && KNOT_UPDATE_GOLDEN=1 pnpm vitest run src/features/docs/history/pmJsonGolden.test.ts -t golden
//! cd .. && KNOT_UPDATE_GOLDEN=1 cargo nextest run -p knot-history --test pm_json -E 'test(the_corpus_fixture_is_the_current_corpus)'
//! cd web && KNOT_UPDATE_GOLDEN=1 pnpm vitest run src/features/docs/history/pmJsonGolden.test.ts
//! ```

#[path = "../../knot-markdown/tests/common/mod.rs"]
mod md;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use knot_crdt::DocHandle;
use knot_history::pm_json::{KNOWN_MARKS, KNOWN_NODES, mark_name, placeholder, pm_json};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use yrs::types::text::YChange;
use yrs::types::{Attrs, ToJson};
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Doc, ReadTxn, StateVector, Text, Transact, Update, Xml, XmlElementPrelim, XmlFragment,
    XmlOut, XmlTextPrelim,
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
const MD_TESTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../knot-markdown/tests");

/// Neither corpus file may grow past this (Rulings: narrow the sample).
const MAX_FIXTURE_BYTES: usize = 4 * 1024 * 1024;

/// One fixture case. `expected` is y-tiptap's JSON of the `default`
/// fragment (`null` in the corpus until the vitest fills it in);
/// `divergence` explains a difference `pm_json` cannot avoid.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Case {
    name: String,
    update: String,
    expected: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    divergence: Option<String>,
}

fn update_golden() -> bool {
    std::env::var_os("KNOT_UPDATE_GOLDEN").is_some_and(|v| v == "1")
}

fn fixture_path(file: &str) -> PathBuf {
    Path::new(FIXTURES).join(file)
}

fn read_cases(file: &str) -> Vec<Case> {
    let path = fixture_path(file);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// `value` as compact JSON with a final newline, keys sorted (serde_json
/// without `preserve_order`), as the vitest writes the corpus file.
fn compact(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).expect("to JSON");
    serde_json::to_string(&value).expect("JSON") + "\n"
}

fn assert_fits(file: &str, text: &str) {
    assert!(
        text.len() <= MAX_FIXTURE_BYTES,
        "{file} is {} bytes, over {MAX_FIXTURE_BYTES}: narrow the corpus sample (Rulings)",
        text.len()
    );
}

fn encode(doc: &Doc) -> String {
    STANDARD.encode(
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default()),
    )
}

fn decode(update: &str) -> Doc {
    let bytes = STANDARD.decode(update).expect("base64 update");
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(&bytes).expect("v1 update"))
        .expect("apply update");
    doc
}

// ---------------------------------------------------------------------------
// The corpus sample and its staleness check
// ---------------------------------------------------------------------------

/// Whether a `generated()` case is in the sample (Rulings): every family
/// but the combinatorial repeats (`ending-in-*`, pairs inside containers,
/// nests deeper than two, the kitchen sink).
fn sampled(name: &str) -> bool {
    let rest = name.split_once('/').map_or(name, |(_, rest)| rest);
    let (family, tail) = rest.split_once('/').unwrap_or((rest, ""));
    match family {
        "paragraph" | "heading" | "block" | "pair" | "lists" | "damaged" => true,
        "nest" => tail
            .split('/')
            .next()
            .is_some_and(|label| label.matches('>').count() <= 1),
        f if f.starts_with("in-") => !tail.contains('+'),
        _ => false,
    }
}

/// The sampled corpus, in fixture order.
fn corpus() -> Vec<(String, DocHandle)> {
    let mut out: Vec<(String, DocHandle)> = md::corpus::generated()
        .into_iter()
        .filter(|c| sampled(&c.name))
        .map(|c| (c.name, c.doc))
        .collect();
    for seed in 1..=4 {
        out.extend(
            md::corpus::random(seed, 25)
                .into_iter()
                .map(|c| (c.name, c.doc)),
        );
    }
    for dir in ["fixtures", "probes"] {
        let files = md::corpus::md_files(&Path::new(MD_TESTS).join(dir));
        assert!(
            !files.is_empty(),
            "no Markdown files in knot-markdown/tests/{dir}"
        );
        for path in files {
            let src = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let (doc, _) = knot_markdown::from_markdown::parse(&src)
                .unwrap_or_else(|e| panic!("parse {}: {e:?}", path.display()));
            let file = path.file_name().expect("file name").to_string_lossy();
            out.push((format!("md/{dir}/{file}"), doc));
        }
    }
    out
}

/// A canonical dump of `doc`'s Y tree: elements as `{"tag", "attrs",
/// "children"}`, each `XmlText` as `{"text": [[chunk, attrs]…]}`, in
/// document order, so adjacent texts and root text stay distinct. Object
/// keys are sorted, so no `HashMap` order leaks in; update bytes cannot be
/// compared (yrs writes maps in `HashMap` order).
fn tree_dump(doc: &Doc) -> Value {
    fn any(a: &Any) -> Value {
        serde_json::to_value(a).expect("Any to JSON")
    }
    fn node<T: ReadTxn>(txn: &T, n: XmlOut) -> Value {
        match n {
            XmlOut::Element(e) => {
                let attrs: Map<String, Value> = e
                    .attributes(txn)
                    .map(|(k, v)| (k.to_string(), any(&v.to_json(txn))))
                    .collect();
                let children: Vec<Value> = e.children(txn).map(|c| node(txn, c)).collect();
                json!({"tag": e.tag().as_ref(), "attrs": attrs, "children": children})
            }
            XmlOut::Text(t) => {
                let chunks: Vec<Value> = t
                    .diff(txn, YChange::identity)
                    .into_iter()
                    .map(|d| {
                        let attrs: Map<String, Value> = d
                            .attributes
                            .iter()
                            .flat_map(|a| a.iter())
                            .map(|(k, v)| (k.to_string(), any(v)))
                            .collect();
                        json!([any(&d.insert.to_json(txn)), attrs])
                    })
                    .collect();
                json!({ "text": chunks })
            }
            XmlOut::Fragment(f) => {
                let children: Vec<Value> = f.children(txn).map(|c| node(txn, c)).collect();
                json!({ "children": children })
            }
        }
    }
    let txn = doc.transact();
    match txn.get_xml_fragment("default") {
        Some(root) => Value::Array(root.children(&txn).map(|c| node(&txn, c)).collect()),
        None => json!([]),
    }
}

#[test]
fn the_corpus_fixture_is_the_current_corpus() {
    const FILE: &str = "pm_json_corpus.json";
    let computed: Vec<(String, Value, String)> = corpus()
        .into_iter()
        .map(|(name, doc)| {
            let doc = doc.inner();
            (name, tree_dump(doc), encode(doc))
        })
        .collect();
    if update_golden() {
        let old: HashMap<String, Case> = if fixture_path(FILE).exists() {
            read_cases(FILE)
                .into_iter()
                .map(|c| (c.name.clone(), c))
                .collect()
        } else {
            HashMap::new()
        };
        let cases: Vec<Case> = computed
            .into_iter()
            .map(|(name, dump, update)| match old.get(&name) {
                Some(kept) if tree_dump(&decode(&kept.update)) == dump => kept.clone(),
                _ => Case {
                    name,
                    update,
                    expected: Value::Null,
                    divergence: None,
                },
            })
            .collect();
        let text = compact(&cases);
        assert_fits(FILE, &text);
        fs::write(fixture_path(FILE), text).expect("write the corpus fixture");
        return;
    }
    let text = fs::read_to_string(fixture_path(FILE)).expect("read the corpus fixture");
    assert_fits(FILE, &text);
    let fixture: Vec<Case> = serde_json::from_str(&text).expect("parse the corpus fixture");
    let names: Vec<&str> = fixture.iter().map(|c| c.name.as_str()).collect();
    let computed_names: Vec<&str> = computed.iter().map(|(n, _, _)| n.as_str()).collect();
    if let Some(i) =
        (0..names.len().max(computed_names.len())).find(|&i| names.get(i) != computed_names.get(i))
    {
        panic!(
            "{FILE} is stale at case {i}: the fixture has {:?}, the corpus {:?}; regenerate it",
            names.get(i),
            computed_names.get(i)
        );
    }
    for (case, (name, dump, _)) in fixture.iter().zip(&computed) {
        assert!(
            tree_dump(&decode(&case.update)) == *dump,
            "{FILE}: {name} changed in the corpus; regenerate it"
        );
    }
}

/// A doc whose paragraph holds one run marked bold, italic and a link with
/// `title`, and a table cell with three attributes. `reversed` sets every
/// map in the other order.
fn ordered_doc(title: &str, reversed: bool) -> Doc {
    fn order<T>(mut v: Vec<T>, reversed: bool) -> Vec<T> {
        if reversed {
            v.reverse();
        }
        v
    }
    let link: HashMap<String, Any> = order(
        vec![
            ("href".to_string(), Any::from("https://example.com")),
            ("title".to_string(), Any::from(title)),
        ],
        reversed,
    )
    .into_iter()
    .collect();
    let marks: Attrs = order(
        vec![
            (Arc::from("bold"), Any::Map(Arc::new(HashMap::new()))),
            (Arc::from("italic"), Any::Bool(true)),
            (Arc::from("link"), Any::Map(Arc::new(link))),
        ],
        reversed,
    )
    .into_iter()
    .collect();
    let doc = Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    let mut txn = doc.transact_mut();
    let para = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
    let t = para.push_back(&mut txn, XmlTextPrelim::new(""));
    t.insert(&mut txn, 0, "plain ");
    t.insert_with_attributes(&mut txn, 6, "marked", marks);
    let cell = frag.push_back(&mut txn, XmlElementPrelim::empty("table_cell"));
    for (k, v) in order(
        vec![
            ("colspan", Any::Number(2.0)),
            ("rowspan", Any::Number(1.0)),
            ("align", Any::from("center")),
        ],
        reversed,
    ) {
        cell.insert_attribute(&mut txn, k, v);
    }
    drop(txn);
    doc
}

#[test]
fn the_tree_dump_ignores_encoding_order() {
    let doc = ordered_doc("T", false);
    let bytes = encode(&doc);
    let dump = tree_dump(&doc);
    assert_eq!(tree_dump(&decode(&bytes)), dump);
    assert_eq!(tree_dump(&decode(&bytes)), dump);
    assert_eq!(tree_dump(&ordered_doc("T", true)), dump);
    assert_ne!(tree_dump(&ordered_doc("U", false)), dump);
    for (name, doc) in corpus() {
        let doc = doc.inner();
        assert_eq!(
            tree_dump(&decode(&encode(doc))),
            tree_dump(doc),
            "{name}: the tree dump does not survive an encode/decode round trip"
        );
    }
}

// ---------------------------------------------------------------------------
// Parity
// ---------------------------------------------------------------------------

/// y-tiptap's JSON brought to `pm_json`'s deliberate differences (Rulings):
/// the root's content flattened one level (y-tiptap leaves an `XmlText`
/// under the fragment as a nested array), marks sorted by `type` and those
/// not in `tools/schema.json` dropped (a text left without marks loses the
/// key), unknown nodes replaced by `placeholder`. It merges nothing.
fn normalize(expected: &Value) -> Value {
    let mut content = Vec::new();
    for item in expected["content"].as_array().expect("doc content") {
        match item {
            Value::Array(items) => content.extend(items.iter().cloned()),
            other => content.push(other.clone()),
        }
    }
    let content: Vec<Value> = content.iter().map(|n| normalize_node(n, "doc")).collect();
    json!({"type": "doc", "content": content})
}

fn normalize_node(node: &Value, parent: &str) -> Value {
    let ty = node["type"].as_str().expect("node type");
    if ty == "text" {
        let mut node = node.clone();
        let obj = node.as_object_mut().expect("text node");
        if let Some(Value::Array(marks)) = obj.remove("marks") {
            let mut marks: Vec<Value> = marks
                .into_iter()
                .filter(|m| KNOWN_MARKS.contains(&m["type"].as_str().expect("mark type")))
                .collect();
            marks.sort_by_key(|m| {
                (
                    m["type"].as_str().map(str::to_owned),
                    m["attrs"].to_string(),
                )
            });
            if !marks.is_empty() {
                obj.insert("marks".into(), Value::Array(marks));
            }
        }
        return node;
    }
    if ty != "doc" && !KNOWN_NODES.contains(&ty) {
        return placeholder(ty, parent);
    }
    let mut node = node.clone();
    if let Some(Value::Array(children)) = node.get_mut("content") {
        *children = children.iter().map(|c| normalize_node(c, ty)).collect();
    }
    node
}

fn assert_parity(file: &str, cases: &[Case]) {
    for case in cases {
        assert!(
            case.divergence.is_none(),
            "{file}: {} has a divergence, which this test does not compare yet",
            case.name
        );
        assert!(
            !case.expected.is_null(),
            "{file}: {} has no expected JSON; run the vitest with KNOT_UPDATE_GOLDEN=1",
            case.name
        );
        let actual = pm_json(&decode(&case.update));
        let expected = normalize(&case.expected);
        assert!(
            actual == expected,
            "{file}: {} differs from y-tiptap\n  pm_json: {actual}\n  y-tiptap: {expected}",
            case.name
        );
    }
}

#[test]
fn pm_json_matches_y_tiptap_on_the_golden_corpus() {
    let cases = read_cases("pm_json_golden.json");
    assert_eq!(cases.len(), 21);
    assert_parity("pm_json_golden.json", &cases);
}

#[test]
fn pm_json_matches_y_tiptap_on_the_markdown_corpus() {
    let cases = read_cases("pm_json_corpus.json");
    assert!(!cases.is_empty());
    assert_parity("pm_json_corpus.json", &cases);
}

/// `pm_json` of every golden case (as `golden/<name>`), then of every
/// sampled corpus case: Task 9's viewer test reads it.
#[test]
fn the_rendered_fixture_is_current() {
    const FILE: &str = "pm_json_rendered.json";
    let rendered: Vec<Value> = read_cases("pm_json_golden.json")
        .into_iter()
        .map(|c| (format!("golden/{}", c.name), c.update))
        .chain(
            read_cases("pm_json_corpus.json")
                .into_iter()
                .map(|c| (c.name, c.update)),
        )
        .map(|(name, update)| json!({"name": name, "doc": pm_json(&decode(&update))}))
        .collect();
    let text = compact(&rendered);
    assert_fits(FILE, &text);
    if update_golden() {
        fs::write(fixture_path(FILE), text).expect("write the rendered fixture");
        return;
    }
    let current = fs::read_to_string(fixture_path(FILE)).expect("read the rendered fixture");
    let fixture: Vec<Value> = serde_json::from_str(&current).expect("parse the rendered fixture");
    assert_eq!(
        fixture.len(),
        rendered.len(),
        "{FILE} is stale; regenerate it"
    );
    for (old, new) in fixture.iter().zip(&rendered) {
        assert!(
            old == new,
            "{FILE} is stale at {}; regenerate it",
            new["name"]
        );
    }
    assert!(
        current == text,
        "{FILE} is not in its canonical form; regenerate it"
    );
}

fn case(file: &str, name: &str) -> Value {
    let case = read_cases(file)
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{file} has no case {name}"));
    pm_json(&decode(&case.update))
}

#[test]
fn a_hashed_mark_name_is_unhashed() {
    assert_eq!(mark_name("bold--AbCd1234"), "bold");
    assert_eq!(mark_name("bold"), "bold");
    assert_eq!(mark_name("x--short"), "x--short");
    assert_eq!(mark_name("link--a+b/c=d9"), "link");
    assert_eq!(mark_name("bold--AbCd123!"), "bold--AbCd123!");
}

#[test]
fn unknown_tags_become_a_placeholder_paragraph() {
    assert_eq!(
        case("pm_json_golden.json", "unknown-tag")["content"],
        json!([{"type": "paragraph", "content": [{"type": "text", "text": "Unsupported content (callout)"}]}])
    );
}

#[test]
fn unknown_inline_elements_become_placeholder_text() {
    assert_eq!(
        case("pm_json_corpus.json", "editor/damaged/unknown-inline")["content"][0]["content"],
        json!([
            {"type": "text", "text": "a"},
            {"type": "text", "text": "Unsupported content (mention_chip)"},
            {"type": "text", "text": "b"},
        ])
    );
}

#[test]
fn root_text_is_flattened() {
    assert_eq!(
        case("pm_json_corpus.json", "editor/damaged/top-level-text")["content"],
        json!([{"type": "text", "text": "stray"}])
    );
}

#[test]
fn importer_code_keeps_its_other_marks() {
    assert_eq!(
        case("pm_json_golden.json", "import-code-link")["content"][0]["content"][0]["marks"],
        json!([
            {"type": "code", "attrs": true},
            {"type": "link", "attrs": {"href": "https://example.com"}},
        ])
    );
    assert_eq!(
        case("pm_json_golden.json", "import-code-bold")["content"][0]["content"][0]["marks"],
        json!([{"type": "bold", "attrs": true}, {"type": "code", "attrs": true}])
    );
}

#[test]
fn integral_numbers_are_integers() {
    let level = &case("pm_json_golden.json", "headings")["content"][0]["attrs"]["level"];
    assert_eq!(*level, json!(1));
    assert!(level.is_i64(), "{level} is not an integer");
}

#[test]
fn known_kinds_match_tools_schema_json() {
    let schema: Value =
        serde_json::from_str(include_str!("../../../tools/schema.json")).expect("schema.json");
    let kinds = |key: &str| -> Vec<String> {
        schema[key]
            .as_array()
            .expect("kinds")
            .iter()
            .map(|k| k["kind"].as_str().expect("kind").to_owned())
            .collect()
    };
    assert_eq!(KNOWN_NODES, kinds("nodes"));
    assert_eq!(KNOWN_MARKS, kinds("marks"));
}

#[test]
fn the_markdown_corpus_renders_without_placeholders() {
    let src = "\
# Title

| a | b |
| :-- | --: |
| one | **two** |

- [x] done
- [ ] todo

1. first\\
   second line
   - nested\\
     break

> quoted
> > deeper

```rust
fn main() {}
```

[![alt text](https://example.com/a.png \"Title\")](https://example.com)
";
    let (doc, _) = knot_markdown::from_markdown::parse(src).expect("parse");
    let rendered = pm_json(doc.inner());
    fn walk(node: &Value, types: &mut Vec<String>, texts: &mut Vec<String>) {
        if let Some(ty) = node["type"].as_str() {
            types.push(ty.to_owned());
        }
        if let Some(text) = node["text"].as_str() {
            texts.push(text.to_owned());
        }
        for mark in node["marks"].as_array().into_iter().flatten() {
            types.push(format!(
                "mark:{}",
                mark["type"].as_str().expect("mark type")
            ));
        }
        for child in node["content"].as_array().into_iter().flatten() {
            walk(child, types, texts);
        }
    }
    let (mut types, mut texts) = (Vec::new(), Vec::new());
    walk(&rendered, &mut types, &mut texts);
    for expected in [
        "table",
        "list_item",
        "hard_break",
        "blockquote",
        "code_block",
        "image",
    ] {
        assert!(
            types.iter().any(|t| t == expected),
            "no {expected} in {rendered}"
        );
    }
    for ty in &types {
        let known = match ty.strip_prefix("mark:") {
            Some(mark) => KNOWN_MARKS.contains(&mark),
            None => KNOWN_NODES.contains(&ty.as_str()),
        };
        assert!(known, "{ty} is not in tools/schema.json: {rendered}");
    }
    assert!(
        texts.iter().all(|t| !t.contains("Unsupported content")),
        "a placeholder in {rendered}"
    );
}
