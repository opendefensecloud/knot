#![allow(dead_code)]
//! Document builder and corpus shared by the knot-markdown differential tests.

pub mod corpus;

use std::sync::Arc;

use knot_crdt::{DocHandle, Engine, YrsEngine};
use yrs::types::Attrs;
use yrs::{Any, Text, Transact, TransactionMut, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim};

/// A node to build: an element with attributes and children, or one
/// `XmlText` made of formatted runs.
#[derive(Clone, Debug)]
pub enum N {
    El(&'static str, Vec<(&'static str, Any)>, Vec<N>),
    Text(Vec<Run>),
}

/// One run of an `XmlText`: its text and its marks (`mark name → value`).
#[derive(Clone, Debug)]
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
