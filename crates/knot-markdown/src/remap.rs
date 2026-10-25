//! Points a copied page at the copies of what it references (Duplicate, P14).
//!
//! Duplicate copies a page's CRDT state, so the copy still references the
//! source's boards and files, and links to the source's subpages.
//! [`references`] lists those references, and [`remap_refs`] rewrites the
//! ones whose ids a [`Remap`] maps. A reference is:
//! - the attribute `board_id` of an element, a board's id (in the editor
//!   schema, an `excalidraw_board`);
//! - the attribute `src` or `url` of an element, `/api/blobs/<id>` (an
//!   `image`, an `attachment`);
//! - the `href` of a `link` mark, `knot://doc/<id>` or `/api/blobs/<id>`.
//!
//! The id is the whole value after its prefix, parsed as a UUID in any
//! spelling, as knot-export reads it (`crates/knot-export/src/walk.rs`). An
//! id inside a longer value, such as an external URL, is not a reference.
//! A rewritten id is written hyphenated, and everything else in the page
//! stays as it is.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use uuid::Uuid;
use yrs::types::Attrs;
use yrs::types::text::YChange;
use yrs::{
    Any, Doc, OffsetKind, Out, ReadTxn, Text, Transact, TransactionMut, Xml, XmlElementRef,
    XmlFragment, XmlOut, XmlTextRef,
};

use crate::DOC_URL_PREFIX;

/// The URL of a file: `/api/blobs/<id>`.
const BLOB_URL_PREFIX: &str = "/api/blobs/";

/// Old id → new id, by kind of reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Remap {
    pub docs: HashMap<Uuid, Uuid>,
    pub boards: HashMap<Uuid, Uuid>,
    pub blobs: HashMap<Uuid, Uuid>,
}

/// The ids a page references, by kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    pub docs: HashSet<Uuid>,
    pub boards: HashSet<Uuid>,
    pub blobs: HashSet<Uuid>,
}

/// Every reference in `doc` that [`remap_refs`] can rewrite.
pub fn references(doc: &Doc) -> Refs {
    let offsets = doc.offset_kind();
    let txn = doc.transact();
    let mut refs = Refs::default();
    for site in sites(&txn, offsets) {
        let set = match site.kind {
            Kind::Doc => &mut refs.docs,
            Kind::Board => &mut refs.boards,
            Kind::Blob => &mut refs.blobs,
        };
        set.insert(site.id);
    }
    refs
}

/// Rewrites every reference in `doc` whose id `map` maps, for its kind, to
/// the new id, and returns how many it rewrote: one per attribute, and one
/// per link (a run of text under one link mark, whatever other marks its
/// parts have).
pub fn remap_refs(doc: &Doc, map: &Remap) -> usize {
    let offsets = doc.offset_kind();
    let mut txn = doc.transact_mut();
    let mut rewrites = 0;
    for site in sites(&txn, offsets) {
        let ids = match site.kind {
            Kind::Doc => &map.docs,
            Kind::Board => &map.boards,
            Kind::Blob => &map.blobs,
        };
        let Some(new) = ids.get(&site.id) else {
            continue;
        };
        let value = format!("{}{}", site.kind.prefix(), new.hyphenated());
        rewrite(&mut txn, site.at, value);
        rewrites += 1;
    }
    rewrites
}

#[derive(Clone, Copy)]
enum Kind {
    Doc,
    Board,
    Blob,
}

impl Kind {
    /// What a reference of this kind writes before the id.
    fn prefix(self) -> &'static str {
        match self {
            Kind::Doc => DOC_URL_PREFIX,
            Kind::Board => "",
            Kind::Blob => BLOB_URL_PREFIX,
        }
    }
}

/// One reference: where it is, and the id it holds.
struct Site {
    at: At,
    kind: Kind,
    id: Uuid,
}

enum At {
    /// The attribute `name` of an element.
    Attr(XmlElementRef, &'static str),
    /// A link mark over `len` units of a text from `index`, in the
    /// document's offset units: bytes in knot's documents, where an embed
    /// counts 1. `link` is the mark's value.
    Link {
        text: XmlTextRef,
        index: u32,
        len: u32,
        link: LinkMark,
    },
}

/// The value of a link mark: `href`, and from the editor `target`, `rel`,
/// `class` and `title`.
type LinkMark = Arc<HashMap<String, Any>>;

/// Points `at` at `value`: the attribute becomes `value`, and the link mark
/// becomes its whole value with only `href` replaced, so the editor's
/// `target`, `rel`, `class` and `title` stay.
fn rewrite(txn: &mut TransactionMut, at: At, value: String) {
    match at {
        At::Attr(el, name) => {
            el.insert_attribute(txn, name, value);
        }
        At::Link {
            text,
            index,
            len,
            link,
        } => {
            let mut link = (*link).clone();
            link.insert("href".to_string(), Any::from(value));
            let attrs = Attrs::from([(Arc::from("link"), Any::Map(Arc::new(link)))]);
            text.format(txn, index, len, attrs);
        }
    }
}

/// Every reference in the document read in `txn`, in document order.
/// `offsets` is the document's offset kind, which link positions are in.
fn sites<T: ReadTxn>(txn: &T, offsets: OffsetKind) -> Vec<Site> {
    let mut out = Vec::new();
    let Some(frag) = txn.get_xml_fragment("default") else {
        return out;
    };
    // Iterative, so it is safe at any depth: children go on the stack in
    // reverse, so they pop in document order.
    let mut stack = Vec::new();
    push_children(&mut stack, txn, &frag);
    while let Some(node) = stack.pop() {
        match node {
            XmlOut::Element(el) => {
                element(txn, &el, &mut out);
                push_children(&mut stack, txn, &el);
            }
            XmlOut::Fragment(f) => push_children(&mut stack, txn, &f),
            XmlOut::Text(t) => links(txn, &t, offsets, &mut out),
        }
    }
    out
}

fn push_children<T: ReadTxn, F: XmlFragment>(stack: &mut Vec<XmlOut>, txn: &T, parent: &F) {
    let children: Vec<XmlOut> = parent.children(txn).collect();
    stack.extend(children.into_iter().rev());
}

/// The references in the attributes of `el`.
fn element<T: ReadTxn>(txn: &T, el: &XmlElementRef, out: &mut Vec<Site>) {
    for name in ["board_id", "src", "url"] {
        let Some(value) = el.get_attribute(txn, name) else {
            continue;
        };
        let value = value.to_string(txn);
        let found = if name == "board_id" {
            Uuid::parse_str(&value).ok().map(|id| (Kind::Board, id))
        } else {
            id_after(&value, BLOB_URL_PREFIX).map(|id| (Kind::Blob, id))
        };
        if let Some((kind, id)) = found {
            out.push(Site {
                at: At::Attr(el.clone(), name),
                kind,
                id,
            });
        }
    }
}

/// The references in the link marks of `text`. A link is a run of
/// consecutive chunks with an equal link mark: a text's diff is split at
/// every format item, so one link can span several chunks.
fn links<T: ReadTxn>(txn: &T, text: &XmlTextRef, offsets: OffsetKind, out: &mut Vec<Site>) {
    // (index, len, link mark): the runs of chunks with an equal link mark.
    let mut runs: Vec<(u32, u32, Option<LinkMark>)> = Vec::new();
    let mut index = 0;
    for chunk in text.diff(txn, YChange::identity) {
        let (len, link) = match &chunk.insert {
            Out::Any(Any::String(s)) => (units(s, offsets), link_mark(chunk.attributes.as_deref())),
            // An embed counts 1, and ends a link.
            _ => (1, None),
        };
        match runs.last_mut() {
            Some((_, run_len, Some(run_link))) if link.as_ref() == Some(run_link) => {
                *run_len += len;
            }
            _ => runs.push((index, len, link)),
        }
        index += len;
    }
    // The diff shows an embedded string as text, so a text holding one has
    // offsets it can't tell. Its links are left alone rather than marked
    // wrongly.
    if index != text.len(txn) {
        return;
    }
    for (index, len, link) in runs {
        if let Some(link) = link
            && let Some((kind, id)) = href_ref(&link)
        {
            out.push(Site {
                at: At::Link {
                    text: text.clone(),
                    index,
                    len,
                    link,
                },
                kind,
                id,
            });
        }
    }
}

/// The value of the link mark among a chunk's `attrs`, if it has one.
fn link_mark(attrs: Option<&Attrs>) -> Option<LinkMark> {
    match attrs?.get("link")? {
        Any::Map(link) => Some(link.clone()),
        _ => None,
    }
}

/// The reference a link mark's `href` holds, if any.
fn href_ref(link: &HashMap<String, Any>) -> Option<(Kind, Uuid)> {
    let Some(Any::String(href)) = link.get("href") else {
        return None;
    };
    [Kind::Doc, Kind::Blob]
        .into_iter()
        .find_map(|kind| id_after(href, kind.prefix()).map(|id| (kind, id)))
}

/// The id of `value` when it is `prefix` followed by a UUID and nothing else.
fn id_after(value: &str, prefix: &str) -> Option<Uuid> {
    Uuid::parse_str(value.strip_prefix(prefix)?).ok()
}

/// The length of `s` in the document's offset units.
fn units(s: &str, offsets: OffsetKind) -> u32 {
    let n = match offsets {
        OffsetKind::Bytes => s.len(),
        OffsetKind::Utf16 => s.encode_utf16().count(),
    };
    n as u32
}
