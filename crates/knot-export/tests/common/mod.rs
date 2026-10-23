#![allow(dead_code)]
//! Fixture tree builder shared by every knot-export test.

use std::collections::HashMap;
use std::sync::Arc;

use yrs::types::Attrs;
use yrs::{
    Any, Doc, Text, Transact, TransactionMut, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim,
};

pub enum N {
    El(&'static str, Vec<(&'static str, Any)>, Vec<N>),
    Text(Vec<(String, Vec<(&'static str, Any)>)>),
}

pub fn build(blocks: Vec<N>) -> Doc {
    let doc = Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    let mut txn = doc.transact_mut();
    for b in blocks {
        add(&frag, &mut txn, b);
    }
    drop(txn);
    doc
}

fn add<F: XmlFragment>(parent: &F, txn: &mut TransactionMut, n: N) {
    match n {
        N::El(tag, attrs, children) => {
            let el = parent.push_back(txn, XmlElementPrelim::empty(tag));
            for (k, v) in attrs {
                el.insert_attribute(txn, k, v);
            }
            for c in children {
                add(&el, txn, c);
            }
        }
        N::Text(chunks) => {
            let t = parent.push_back(txn, XmlTextPrelim::new(""));
            for (s, marks) in chunks {
                let attrs: Attrs = marks
                    .into_iter()
                    .map(|(k, v)| (Arc::from(k), v))
                    .collect::<HashMap<_, _>>();
                let pos = t.len(txn);
                t.insert_with_attributes(txn, pos, &s, attrs);
            }
        }
    }
}

pub fn el(tag: &'static str, attrs: Vec<(&'static str, Any)>, children: Vec<N>) -> N {
    N::El(tag, attrs, children)
}

pub fn t(s: &str) -> N {
    N::Text(vec![(s.to_owned(), vec![])])
}

pub fn p(s: &str) -> N {
    el("paragraph", vec![], vec![t(s)])
}

/// Heading with the level as an editor number (`f64`).
pub fn h(level: u8, s: &str) -> N {
    el(
        "heading",
        vec![("level", Any::Number(f64::from(level)))],
        vec![t(s)],
    )
}

/// Text carrying the given marks, each as an empty map (editor shape).
pub fn marked(s: &str, marks: &[&'static str]) -> N {
    N::Text(vec![(
        s.to_owned(),
        marks
            .iter()
            .map(|m| (*m, Any::Map(Arc::new(HashMap::new()))))
            .collect(),
    )])
}

pub fn linked(s: &str, href: &str) -> N {
    let mut m = HashMap::new();
    m.insert("href".to_owned(), Any::String(Arc::from(href)));
    N::Text(vec![(s.to_owned(), vec![("link", Any::Map(Arc::new(m)))])])
}
