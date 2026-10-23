#![allow(dead_code)]
//! Fixture tree builder and package readers shared by every knot-export test.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;

use knot_export::assets::{Assets, ImageAsset};
use knot_export::{Block, ExportDoc, ImageSource};
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

/// Every entry of a zip archive, keyed by name. Entries are read with
/// `ZipArchive::by_name` only.
pub fn unzip(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("a zip archive");
    let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
    names
        .into_iter()
        .map(|name| {
            let mut data = Vec::new();
            zip.by_name(&name)
                .expect("listed entry")
                .read_to_end(&mut data)
                .expect("readable entry");
            (name, data)
        })
        .collect()
}

/// Parse one package part; panics when it is not well-formed.
pub fn xml(s: &str) -> roxmltree::Document<'_> {
    roxmltree::Document::parse(s).unwrap_or_else(|e| panic!("malformed XML: {e}"))
}

pub const UNAVAILABLE_BLOB: &str = "5f1e2d3c-4b5a-4968-8776-655443322110";
pub const OVER_BUDGET_BLOB: &str = "6a7b8c9d-0e1f-4a2b-9c3d-4e5f60718293";
pub const FIXTURE_BOARD: &str = "1d2c3b4a-5968-4776-8584-93a2b1c0d9e8";

/// A page in which every diagram and image falls back: a Mermaid block and a
/// board without pictures, plus images that are unavailable, over the image
/// budget, and of an unsupported source.
pub fn fallback_fixture() -> (ExportDoc, Assets) {
    let doc = ExportDoc {
        blocks: vec![
            Block::Mermaid {
                key: "mermaid-3f6c1e0b9a4d7f2e8c5b1a0d9e7f6c4b3a2d1e0f9c8b7a6d5e4f3c2b1a0d9e8f"
                    .into(),
                source: "graph TD\n  A --> B".into(),
            },
            Block::Board {
                key: format!("board-{FIXTURE_BOARD}"),
                board_id: FIXTURE_BOARD.into(),
                label: "Roadmap".into(),
            },
            Block::Image {
                source: ImageSource::Blob(UNAVAILABLE_BLOB.into()),
                alt: "Org chart".into(),
            },
            Block::Image {
                source: ImageSource::Blob(OVER_BUDGET_BLOB.into()),
                alt: "Team photo".into(),
            },
            Block::Image {
                source: ImageSource::Unsupported,
                alt: "Local file".into(),
            },
        ],
    };
    let mut assets = Assets::default();
    assets
        .images
        .insert(UNAVAILABLE_BLOB.into(), ImageAsset::Unavailable);
    assets
        .images
        .insert(OVER_BUDGET_BLOB.into(), ImageAsset::OverBudget);
    (doc, assets)
}
