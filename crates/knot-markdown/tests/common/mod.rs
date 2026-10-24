#![allow(dead_code)]
//! Document builder and corpus shared by the knot-markdown differential tests.

pub mod corpus;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use knot_crdt::{DocHandle, Engine, YrsEngine};
use yrs::types::Attrs;
use yrs::types::text::YChange;
use yrs::{
    Any, Out, ReadTxn, Text, Transact, TransactionMut, Xml, XmlElementPrelim, XmlFragment, XmlOut,
    XmlTextPrelim,
};

/// A node to build: an element with attributes and children, or one
/// `XmlText` made of formatted runs.
#[derive(Clone, Debug, PartialEq)]
pub enum N {
    El(&'static str, Vec<(&'static str, Any)>, Vec<N>),
    Text(Vec<Run>),
}

/// One run of an `XmlText`: its text and its marks (`mark name → value`).
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub marks: Vec<(&'static str, Any)>,
}

pub fn el(tag: &'static str, children: Vec<N>) -> N {
    N::El(tag, Vec::new(), children)
}

pub fn ela(tag: &'static str, attrs: Vec<(&'static str, Any)>, children: Vec<N>) -> N {
    N::El(tag, attrs, children)
}

/// An `XmlText` with one unmarked run.
pub fn text(s: &str) -> N {
    N::Text(vec![run(s, Vec::new())])
}

pub fn run(s: &str, marks: Vec<(&'static str, Any)>) -> Run {
    Run {
        text: s.to_string(),
        marks,
    }
}

/// A paragraph holding one unmarked `XmlText`.
pub fn p(s: &str) -> N {
    el("paragraph", vec![text(s)])
}

pub fn hard_break() -> N {
    el("hard_break", Vec::new())
}

/// Build a document whose "default" fragment holds `blocks`.
pub fn build(blocks: &[N]) -> DocHandle {
    let doc = YrsEngine.new_doc();
    {
        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        for block in blocks {
            add(&frag, &mut txn, block);
        }
    }
    doc
}

/// The blocks of `doc`'s "default" fragment, the inverse of [`build`]: each
/// `XmlText` as one `N::Text` of its formatted runs. Tags and attribute
/// names are interned, as `N` holds them as `&'static str`. Nested
/// fragments, which the editor never writes, are left out.
pub fn read(doc: &DocHandle) -> Vec<N> {
    fn node<T: ReadTxn>(txn: &T, n: &XmlOut) -> Option<N> {
        match n {
            XmlOut::Element(el) => {
                let attrs = el
                    .attributes(txn)
                    .filter_map(|(k, v)| match v {
                        Out::Any(a) => Some((intern(k), a)),
                        _ => None,
                    })
                    .collect();
                let children = (0..el.len(txn))
                    .filter_map(|i| el.get(txn, i))
                    .filter_map(|c| node(txn, &c))
                    .collect();
                Some(N::El(intern(el.tag()), attrs, children))
            }
            XmlOut::Text(t) => Some(N::Text(
                t.diff(txn, YChange::identity)
                    .into_iter()
                    .filter_map(|d| match d.insert {
                        Out::Any(Any::String(s)) => Some(Run {
                            text: s.to_string(),
                            marks: d
                                .attributes
                                .into_iter()
                                .flat_map(|a| a.into_iter())
                                .map(|(k, v)| (intern(&k), v))
                                .collect(),
                        }),
                        _ => None,
                    })
                    .collect(),
            )),
            XmlOut::Fragment(_) => None,
        }
    }
    let ydoc = doc.inner();
    let txn = ydoc.transact();
    let Some(frag) = txn.get_xml_fragment("default") else {
        return Vec::new();
    };
    (0..frag.len(&txn))
        .filter_map(|i| frag.get(&txn, i))
        .filter_map(|c| node(&txn, &c))
        .collect()
}

/// `s` as a `&'static str`, leaked once per distinct name.
fn intern(s: &str) -> &'static str {
    static NAMES: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut names = NAMES.lock().expect("names");
    match names.get(s) {
        Some(name) => name,
        None => {
            let name: &'static str = Box::leak(s.to_owned().into_boxed_str());
            names.insert(name);
            name
        }
    }
}

fn add<F: XmlFragment>(parent: &F, txn: &mut TransactionMut, node: &N) {
    match node {
        N::El(tag, attrs, children) => {
            let el = parent.push_back(txn, XmlElementPrelim::empty(*tag));
            for (k, v) in attrs {
                el.insert_attribute(txn, *k, v.clone());
            }
            for child in children {
                add(&el, txn, child);
            }
        }
        N::Text(runs) => {
            let t = parent.push_back(txn, XmlTextPrelim::new(""));
            for r in runs {
                // Explicit attributes on every run, so a run never inherits
                // the marks of the one before it.
                let attrs: Attrs = r
                    .marks
                    .iter()
                    .map(|(k, v)| (Arc::from(*k), v.clone()))
                    .collect();
                let pos = t.len(txn);
                t.insert_with_attributes(txn, pos, &r.text, attrs);
            }
        }
    }
}
