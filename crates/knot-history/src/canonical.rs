//! A page's visible content as plain JSON, for comparing two states by what
//! a reader sees rather than by their bytes or block structure (Rulings: the
//! "state at rollout" entry is written only when genesis differs from the
//! newest baseline by this comparison).
//!
//! The `default` fragment becomes an array of nodes: an element is
//! `{"tag", "attrs", "children"}`, a text is `{"text": [{"insert", "attrs"}]}`
//! (its `XmlTextRef::diff` chunks), and a nested fragment is
//! `{"children"}`. Attributes are `Out::to_json` values, keyed in sorted
//! order, so two states that differ only in the order attributes were set,
//! or in how their text is split into items, compare equal.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use yrs::types::ToJson;
use yrs::types::text::YChange;
use yrs::{Any, Doc, Out, ReadTxn, Text, Transact, Xml, XmlFragment, XmlOut};

/// The `default` fragment of `doc` as nested JSON; `[]` when the doc has no
/// such fragment.
pub fn visible_content(doc: &Doc) -> Value {
    let txn = doc.transact();
    match txn.get_xml_fragment("default") {
        Some(root) => nodes(&txn, root.children(&txn)),
        None => Value::Array(Vec::new()),
    }
}

fn nodes<T: ReadTxn>(txn: &T, children: impl Iterator<Item = XmlOut>) -> Value {
    Value::Array(children.map(|child| node(txn, child)).collect())
}

fn node<T: ReadTxn>(txn: &T, n: XmlOut) -> Value {
    match n {
        XmlOut::Element(e) => {
            let attrs: BTreeMap<String, Value> = e
                .attributes(txn)
                .map(|(k, v)| (k.to_string(), any_json(&v.to_json(txn))))
                .collect();
            json!({
                "tag": e.tag().as_ref(),
                "attrs": sorted(attrs),
                "children": nodes(txn, e.children(txn)),
            })
        }
        XmlOut::Text(t) => {
            let chunks: Vec<Value> = t
                .diff(txn, YChange::identity)
                .into_iter()
                .map(|d| {
                    let attrs: BTreeMap<String, Value> = d
                        .attributes
                        .map(|a| {
                            a.iter()
                                .map(|(k, v)| (k.to_string(), any_json(v)))
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({ "insert": out_json(txn, &d.insert), "attrs": sorted(attrs) })
                })
                .collect();
            json!({ "text": chunks })
        }
        XmlOut::Fragment(f) => json!({ "children": nodes(txn, f.children(txn)) }),
    }
}

/// A text chunk's insert: its string, or the JSON of an embed.
fn out_json<T: ReadTxn>(txn: &T, out: &Out) -> Value {
    match out {
        Out::Any(Any::String(s)) => Value::String(s.to_string()),
        other => any_json(&other.to_json(txn)),
    }
}

fn any_json(any: &Any) -> Value {
    serde_json::to_value(any).unwrap_or(Value::Null)
}

/// An object whose keys are in sorted order, whatever `serde_json`'s map
/// keeps.
fn sorted(attrs: BTreeMap<String, Value>) -> Value {
    Value::Object(attrs.into_iter().collect::<Map<String, Value>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::types::Attrs;
    use yrs::{XmlElementPrelim, XmlTextPrelim};

    fn page(attr_order: &[(&str, &str)], split_text: bool, bold: bool) -> Doc {
        let doc = Doc::new();
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("heading"));
        for (k, v) in attr_order {
            p.insert_attribute(&mut txn, *k, *v);
        }
        let t = p.push_back(&mut txn, XmlTextPrelim::new(""));
        if split_text {
            t.insert(&mut txn, 0, "wor");
            t.insert(&mut txn, 3, "ld");
            t.insert(&mut txn, 0, "hello ");
        } else {
            t.insert(&mut txn, 0, "hello world");
        }
        if bold {
            let attrs: Attrs = [("bold".into(), Any::Bool(true))].into_iter().collect();
            t.format(&mut txn, 6, 5, attrs);
        }
        drop(txn);
        doc
    }

    #[test]
    fn equal_content_compares_equal_whatever_its_history() {
        let a = page(&[("level", "1"), ("id", "x")], false, false);
        let b = page(&[("id", "x"), ("level", "1")], true, false);
        assert_eq!(visible_content(&a), visible_content(&b));
        assert_eq!(
            visible_content(&a),
            json!([{
                "tag": "heading",
                "attrs": {"id": "x", "level": "1"},
                "children": [{"text": [{"insert": "hello world", "attrs": {}}]}],
            }])
        );
    }

    #[test]
    fn formatting_and_attributes_make_a_difference() {
        let plain = page(&[("level", "1")], false, false);
        let bold = page(&[("level", "1")], false, true);
        let other = page(&[("level", "2")], false, false);
        assert_ne!(visible_content(&plain), visible_content(&bold));
        assert_ne!(visible_content(&plain), visible_content(&other));
        assert_eq!(
            visible_content(&bold)[0]["children"][0]["text"][1],
            json!({"insert": "world", "attrs": {"bold": true}})
        );
    }

    #[test]
    fn a_doc_without_the_fragment_is_empty() {
        assert_eq!(visible_content(&Doc::new()), json!([]));
    }
}
