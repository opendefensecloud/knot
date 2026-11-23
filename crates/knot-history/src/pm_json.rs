//! A page as ProseMirror JSON, the shape `@tiptap/y-tiptap` 3.0.9's
//! `yXmlFragmentToProsemirrorJSON` gives (`y-tiptap.js:2464-2526`), so the
//! history viewer can load a past version without any Y data reaching the
//! client. `tests/pm_json.rs` holds the parity test against y-tiptap's own
//! output.
//!
//! The rule, as y-tiptap applies it:
//! - an element is `{"type": tag}`, plus `"attrs"` when it has at least one
//!   attribute (values through `Out::to_json`), plus `"content"` when it has
//!   at least one Y child (its children's nodes, flattened; possibly `[]`);
//! - an `XmlText` is one `{"type": "text", "text", "marks"?}` per diff chunk,
//!   empty strings skipped, never merged with a neighbour (ProseMirror joins
//!   equal-mark neighbours on load);
//! - a chunk's attribute is a mark `{"type": name, "attrs": value}`, `attrs`
//!   always present (the editor stores `code` as `{}`, the importer as
//!   `true`), `name` the key with y-tiptap's hashed suffix stripped
//!   (`y-tiptap.js:1490-1494`).
//!
//! What it does on purpose differently, so that every output is JSON that
//! ProseMirror's `Node.fromJSON` reads:
//! - the root's content is flattened like an element's (y-tiptap maps the
//!   root's children without `.flat()`, so an `XmlText` directly under the
//!   fragment becomes a nested array there);
//! - unknown tags and marks: a mark whose name is not in `tools/schema.json`
//!   is dropped (`Mark.fromJSON` throws on one), and a tag not in it, with
//!   its subtree, becomes [`placeholder`] text (inside a textblock) or a
//!   placeholder paragraph (anywhere else). Unknown node attributes pass
//!   through: ProseMirror ignores undeclared ones.
//!
//! Neither changes what the editor shows: y-tiptap's `Mark.fromJSON` would
//! fail on those shapes too. Marks are also sorted by `type` (yrs keeps a
//! chunk's attributes in a `HashMap`; ProseMirror re-sorts marks by rank on
//! load), and integral numbers are emitted as integers (`Any::Number` is an
//! `f64`; y-prosemirror stores JavaScript numbers, which JSON prints without
//! a fraction).
//!
//! [`pm_json_diff`] is the "Show changes" render: the same rule over the
//! union doc of an entry's diff ([`crate::diff`]), the entry's own text
//! changes as the marks [`HISTORY_INSERT`] / [`HISTORY_DELETE`] and its
//! element changes as the node attribute [`HISTORY_CHANGE`].

use std::sync::Arc;

use serde_json::{Map, Value, json};
use yrs::types::text::{ChangeKind, YChange};
use yrs::types::{Attrs, ToJson};
use yrs::{
    Any, BranchID, Doc, ID, Out, ReadTxn, Text, Transact, Xml, XmlElementRef, XmlFragment, XmlOut,
    XmlTextRef,
};

use crate::diff::{DiffError, EntryDiff, diff_text, lo_doc};
use crate::pass::Progress;

/// The version of the rendering rule. A cache of rendered versions keys on
/// it, so a change to the rule is a bump here.
pub const RENDERER_VERSION: u32 = 1;

/// The node kinds of `tools/schema.json`, in file order.
pub const KNOWN_NODES: &[&str] = &[
    "doc",
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
    "table_of_contents",
    "table",
    "table_row",
    "table_cell",
    "table_header",
];

/// The mark kinds of `tools/schema.json`, in file order.
pub const KNOWN_MARKS: &[&str] = &["bold", "italic", "code", "strike", "underline", "link"];

/// The nodes whose `tools/schema.json` content is `inline*` or `text*`: an
/// unknown element inside one becomes inline placeholder text.
const TEXTBLOCKS: &[&str] = &["paragraph", "heading", "code_block"];

/// The `default` fragment of `doc` as ProseMirror JSON `{"type": "doc",
/// "content": [...]}`; an empty or absent fragment has `"content": []`.
pub fn pm_json(doc: &Doc) -> Value {
    let txn = doc.transact();
    let mut content = Vec::new();
    if let Some(root) = txn.get_xml_fragment("default") {
        for child in root.children(&txn) {
            push_node(&txn, child, "doc", &mut content);
        }
    }
    json!({"type": "doc", "content": content})
}

/// The mark name of a text attribute key: y-tiptap's
/// `/(.*)(--[a-zA-Z0-9+/=]{8})$/` with the suffix stripped, the key itself
/// when it does not match. `.` does not match a line terminator, so, as in
/// JavaScript, the name starts after the last one.
pub fn mark_name(attr_key: &str) -> &str {
    let bytes = attr_key.as_bytes();
    let Some(head_len) = bytes.len().checked_sub(10) else {
        return attr_key;
    };
    let is_hash = |c: &u8| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=');
    if &bytes[head_len..head_len + 2] != b"--" || !bytes[head_len + 2..].iter().all(is_hash) {
        return attr_key;
    }
    // The ten matched bytes are ASCII, so `head_len` is a char boundary.
    let head = &attr_key[..head_len];
    head.rsplit(['\n', '\r', '\u{2028}', '\u{2029}'])
        .next()
        .unwrap_or(head)
}

/// What an element with the unknown tag `tag` renders as inside a node of
/// type `parent` (`"doc"` at the root): the text node `Unsupported content
/// (<tag>)` inside a textblock (`paragraph`, `heading`, `code_block`), else
/// a paragraph holding it.
pub fn placeholder(tag: &str, parent: &str) -> Value {
    let text = json!({"type": "text", "text": format!("Unsupported content ({tag})")});
    if TEXTBLOCKS.contains(&parent) {
        text
    } else {
        json!({"type": "paragraph", "content": [text]})
    }
}

/// Appends the nodes of `node`, a child of a node of type `parent`, to
/// `out`: one for an element, one per text chunk for an `XmlText`.
fn push_node<T: ReadTxn>(txn: &T, node: XmlOut, parent: &str, out: &mut Vec<Value>) {
    match node {
        XmlOut::Element(el) => out.push(element_json(txn, &el, parent)),
        XmlOut::Text(text) => {
            for chunk in text.diff(txn, YChange::identity) {
                // An embed is not text; the editor never writes one.
                if let Out::Any(Any::String(s)) = &chunk.insert {
                    push_text(out, s, marks_json(chunk.attributes.as_deref()));
                }
            }
        }
        // A nested fragment, which the editor never writes (y-tiptap throws
        // on one): nothing to show.
        XmlOut::Fragment(_) => {}
    }
}

fn element_json<T: ReadTxn>(txn: &T, el: &XmlElementRef, parent: &str) -> Value {
    let tag = el.tag();
    if !renders_as_itself(tag) {
        return placeholder(tag, parent);
    }
    let mut node = Map::new();
    node.insert("type".into(), Value::String(tag.to_string()));
    if let Some(attrs) = attrs_json(txn, el) {
        node.insert("attrs".into(), Value::Object(attrs));
    }
    let children: Vec<XmlOut> = el.children(txn).collect();
    if !children.is_empty() {
        let mut content = Vec::new();
        for child in children {
            push_node(txn, child, tag, &mut content);
        }
        node.insert("content".into(), Value::Array(content));
    }
    Value::Object(node)
}

/// Whether an element tagged `tag` renders as a node of that type, not as
/// a [`placeholder`]: `tag` is a node kind of `tools/schema.json`, and not
/// `doc` or `text`, which are schema kinds but never an element's.
fn renders_as_itself(tag: &str) -> bool {
    KNOWN_NODES.contains(&tag) && !matches!(tag, "doc" | "text")
}

/// `el`'s attributes as JSON; `None` when it has none.
pub(crate) fn attrs_json<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> Option<Map<String, Value>> {
    let attrs: Map<String, Value> = el
        .attributes(txn)
        .map(|(k, v)| (k.to_string(), any_json(&v.to_json(txn))))
        .collect();
    (!attrs.is_empty()).then_some(attrs)
}

/// A text chunk's attributes as marks: names unhashed, those not in
/// `tools/schema.json` dropped, sorted by `type` (then by `attrs`, so two
/// keys that unhash to one name keep one order).
pub(crate) fn marks_json(attrs: Option<&Attrs>) -> Vec<Value> {
    let mut marks: Vec<(&str, Value)> = attrs
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| {
            let name = mark_name(key);
            KNOWN_MARKS.contains(&name).then(|| (name, any_json(value)))
        })
        .collect();
    marks.sort_by(|a, b| {
        a.0.cmp(b.0)
            .then_with(|| a.1.to_string().cmp(&b.1.to_string()))
    });
    marks
        .into_iter()
        .map(|(name, attrs)| json!({"type": name, "attrs": attrs}))
        .collect()
}

/// Appends one text node, `"marks"` only when there are any; an empty
/// string adds nothing. Never merges with the node before it.
pub(crate) fn push_text(out: &mut Vec<Value>, text: &str, marks: Vec<Value>) {
    if text.is_empty() {
        return;
    }
    let mut node = Map::new();
    node.insert("type".into(), Value::String("text".into()));
    node.insert("text".into(), Value::String(text.into()));
    if !marks.is_empty() {
        node.insert("marks".into(), Value::Array(marks));
    }
    out.push(Value::Object(node));
}

/// The mark on text the entry inserted and that is still there at its end.
pub const HISTORY_INSERT: &str = "historyInsert";

/// The mark on text that was there before the entry and that it deleted.
pub const HISTORY_DELETE: &str = "historyDelete";

/// The node attribute saying how the entry changed an element: `"added"`,
/// `"removed"` or `"changed"` (absent when it did not).
pub const HISTORY_CHANGE: &str = "historyChange";

/// The page at the end of `d`'s entry as ProseMirror JSON with the entry's
/// own changes marked: [`pm_json`]'s rule applied to `d`'s union doc, which
/// shows what the page shows with or without the entry's changes.
///
/// `h` is the skip-GC replay `d` was built on ([`crate::diff::entry_diff`]).
/// - Text: each text is diffed between the page at the entry's end and the
///   page without its changes (`diff::diff_text`); an added chunk gets the mark
///   [`HISTORY_INSERT`], a removed one [`HISTORY_DELETE`], both without
///   `attrs` and sorted with the other marks by `type`. Formatting renders
///   as of the entry's end; formatting the entry changed is not marked.
/// - Elements: [`HISTORY_CHANGE`] is `"added"` when the element's item is
///   one of the entry's insertions still alive (`d.added`), `"removed"`
///   when it is a pre-existing item the entry deleted (`d.removed`), and
///   `"changed"` when it is alive at the entry's end and its attributes
///   differ from those it has in [`lo_doc`]. Attributes come from `h` for
///   an element alive there and from the union doc for a removed one.
///   Elements the entry inserted and deleted again, or that another author
///   deleted, are deleted in the union doc and not rendered.
/// - Unknown tags and marks follow [`pm_json`]: a placeholder paragraph
///   carries its element's `historyChange`, inline placeholder text the
///   mark of an added or removed element (`changed` has no inline form).
///
/// Checks `progress` before diffing each text and returns
/// [`DiffError::Cancelled`] once it is cancelled.
pub fn pm_json_diff(h: &Doc, d: &EntryDiff, progress: &Progress) -> Result<Value, DiffError> {
    // The whole tree is read first, and the union doc's read transaction
    // dropped: every `diff_text` takes the union doc's write lock, which a
    // read transaction held on this thread would block for good.
    let root = {
        let union = d.union.transact();
        let h = h.transact();
        match union.get_xml_fragment("default") {
            Some(root) => collect_union(&union, &h, d, root.children(&union)),
            None => Vec::new(),
        }
    };
    let lo = lo_doc(h, d)?;
    let lo = lo.transact();
    let mut content = Vec::new();
    for node in root {
        push_diff_node(d, &lo, progress, node, "doc", &mut content)?;
    }
    Ok(json!({"type": "doc", "content": content}))
}

/// How an entry changed an element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Added,
    Removed,
    Changed,
}

impl Change {
    /// The value of [`HISTORY_CHANGE`].
    fn attr(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Removed => "removed",
            Change::Changed => "changed",
        }
    }

    /// The mark an inline placeholder of the element carries.
    fn mark(self) -> Option<&'static str> {
        match self {
            Change::Added => Some(HISTORY_INSERT),
            Change::Removed => Some(HISTORY_DELETE),
            Change::Changed => None,
        }
    }
}

/// A node of the union doc's tree, as read before any text is diffed.
enum UnionNode {
    Element(UnionElement),
    Text(XmlTextRef),
    /// A nested fragment: a Y child that renders as nothing, as in
    /// [`pm_json`].
    Fragment,
}

struct UnionElement {
    tag: Arc<str>,
    /// From `h` when the element is alive there, else from the union doc.
    attrs: Option<Map<String, Value>>,
    /// `Added` or `Removed` from the entry's sets; `Changed` is decided at
    /// render time against `D_lo`.
    change: Option<Change>,
    /// The element's id when it is alive in `h` and neither added nor
    /// removed, so its attributes are compared with `D_lo`'s.
    compare: Option<ID>,
    /// Empty for an element that renders as a placeholder.
    children: Vec<UnionNode>,
}

fn collect_union<U: ReadTxn, H: ReadTxn>(
    union: &U,
    h: &H,
    d: &EntryDiff,
    nodes: impl Iterator<Item = XmlOut>,
) -> Vec<UnionNode> {
    nodes
        .map(|node| {
            let id = match node.id() {
                BranchID::Nested(id) => Some(id),
                BranchID::Root(_) => None,
            };
            match node {
                XmlOut::Text(text) => UnionNode::Text(text),
                XmlOut::Fragment(_) => UnionNode::Fragment,
                XmlOut::Element(el) => {
                    let change = id.and_then(|id| {
                        if d.added.contains(&id) {
                            Some(Change::Added)
                        } else if d.removed.contains(&id) {
                            Some(Change::Removed)
                        } else {
                            None
                        }
                    });
                    let in_h = id.and_then(|id| alive_element(h, &id));
                    let attrs = match &in_h {
                        Some(el) => attrs_json(h, el),
                        None => attrs_json(union, &el),
                    };
                    let tag = el.tag().clone();
                    let children = if renders_as_itself(&tag) {
                        collect_union(union, h, d, el.children(union))
                    } else {
                        Vec::new()
                    };
                    UnionNode::Element(UnionElement {
                        tag,
                        attrs,
                        change,
                        compare: id.filter(|_| change.is_none() && in_h.is_some()),
                        children,
                    })
                }
            }
        })
        .collect()
}

/// The element with item id `id` in `txn`'s doc, when it is there and not
/// deleted. The lookup alone also finds deleted elements: a skip-GC doc
/// keeps their content.
fn alive_element<T: ReadTxn>(txn: &T, id: &ID) -> Option<XmlElementRef> {
    BranchID::Nested(*id)
        .get_branch(txn)
        .filter(|branch| !branch.is_deleted())
        .map(XmlElementRef::from)
}

/// [`push_node`] for a node of the union doc: appends its nodes, a child
/// of a node of type `parent`, to `out`, texts diffed and marked.
fn push_diff_node<T: ReadTxn>(
    d: &EntryDiff,
    lo: &T,
    progress: &Progress,
    node: UnionNode,
    parent: &str,
    out: &mut Vec<Value>,
) -> Result<(), DiffError> {
    match node {
        UnionNode::Element(el) => out.push(diff_element_json(d, lo, progress, el, parent)?),
        UnionNode::Text(text) => {
            if progress.is_cancelled() {
                return Err(DiffError::Cancelled);
            }
            for chunk in diff_text(d, &text) {
                // An embed is not text; the editor never writes one.
                if let Out::Any(Any::String(s)) = &chunk.insert {
                    let mut marks = marks_json(chunk.attributes.as_deref());
                    match chunk.ychange.map(|c| c.kind) {
                        Some(ChangeKind::Added) => insert_mark(&mut marks, HISTORY_INSERT),
                        Some(ChangeKind::Removed) => insert_mark(&mut marks, HISTORY_DELETE),
                        None => {}
                    }
                    push_text(out, s, marks);
                }
            }
        }
        UnionNode::Fragment => {}
    }
    Ok(())
}

/// [`element_json`] for an element of the union doc, with its
/// [`HISTORY_CHANGE`].
fn diff_element_json<T: ReadTxn>(
    d: &EntryDiff,
    lo: &T,
    progress: &Progress,
    el: UnionElement,
    parent: &str,
) -> Result<Value, DiffError> {
    let change = el.change.or_else(|| {
        let before = alive_element(lo, &el.compare?)?;
        (attrs_json(lo, &before) != el.attrs).then_some(Change::Changed)
    });
    if !renders_as_itself(&el.tag) {
        let mut node = placeholder(&el.tag, parent);
        match change {
            Some(change) if node["type"] == "text" => {
                if let Some(mark) = change.mark() {
                    node["marks"] = json!([{"type": mark}]);
                }
            }
            Some(change) => node["attrs"] = json!({HISTORY_CHANGE: change.attr()}),
            None => {}
        }
        return Ok(node);
    }
    let mut node = Map::new();
    node.insert("type".into(), Value::String(el.tag.to_string()));
    let mut attrs = el.attrs.unwrap_or_default();
    if let Some(change) = change {
        attrs.insert(HISTORY_CHANGE.into(), Value::String(change.attr().into()));
    }
    if !attrs.is_empty() {
        node.insert("attrs".into(), Value::Object(attrs));
    }
    if !el.children.is_empty() {
        let mut content = Vec::new();
        for child in el.children {
            push_diff_node(d, lo, progress, child, &el.tag, &mut content)?;
        }
        node.insert("content".into(), Value::Array(content));
    }
    Ok(Value::Object(node))
}

/// Inserts the mark `name` (without `attrs`) into `marks`, which are sorted
/// by `type`, keeping them sorted.
fn insert_mark(marks: &mut Vec<Value>, name: &str) {
    let at = marks.partition_point(|m| m["type"].as_str().is_some_and(|t| t < name));
    marks.insert(at, json!({"type": name}));
}

/// `any` as JSON, an integral finite number within JavaScript's safe
/// integer range as an integer; a non-finite number is `null`, as
/// `JSON.stringify` writes it.
fn any_json(any: &Any) -> Value {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    match any {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => Value::Bool(*b),
        Any::Number(f) if f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER => Value::from(*f as i64),
        Any::Number(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
        Any::BigInt(i) => Value::from(*i),
        Any::String(s) => Value::String(s.to_string()),
        Any::Buffer(b) => Value::Array(b.iter().map(|byte| Value::from(*byte)).collect()),
        Any::Array(items) => Value::Array(items.iter().map(any_json).collect()),
        Any::Map(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.to_string(), any_json(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn numbers_follow_json_stringify() {
        assert_eq!(any_json(&Any::Number(2.0)), json!(2));
        assert!(any_json(&Any::Number(-0.0)).is_i64());
        assert_eq!(any_json(&Any::Number(1.5)), json!(1.5));
        assert_eq!(any_json(&Any::Number(f64::NAN)), Value::Null);
        assert_eq!(any_json(&Any::Number(f64::INFINITY)), Value::Null);
        assert_eq!(any_json(&Any::Number(1e300)), json!(1e300));
        let nested = Any::Map(Arc::new(HashMap::from([(
            "w".to_string(),
            Any::Array(Arc::from(vec![Any::Number(120.0)])),
        )])));
        assert_eq!(any_json(&nested), json!({"w": [120]}));
    }

    #[test]
    fn a_mark_name_starts_after_a_line_break() {
        assert_eq!(mark_name("a\nbold--AbCd1234"), "bold");
        assert_eq!(mark_name("--AbCd1234"), "");
    }

    #[test]
    fn empty_text_is_skipped() {
        let mut out = Vec::new();
        push_text(&mut out, "", Vec::new());
        push_text(&mut out, "a", Vec::new());
        push_text(&mut out, "a", Vec::new());
        assert_eq!(out, vec![json!({"type": "text", "text": "a"}); 2]);
    }

    #[test]
    fn an_empty_doc_has_empty_content() {
        assert_eq!(pm_json(&Doc::new()), json!({"type": "doc", "content": []}));
    }
}
