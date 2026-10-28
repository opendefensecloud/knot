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
//! Attributes are matched by name, on any element, not by the element's
//! tag. `tools/schema.json` gives these attributes to those three nodes
//! only, so for a page that follows the schema the two are the same.
//!
//! The id is the whole value after its prefix, parsed as a UUID in any
//! spelling, as knot-export reads it (`crates/knot-export/src/walk.rs`). An
//! id inside a longer value, such as an external URL, is not a reference.
//! A rewritten id is written hyphenated, and everything else in the page
//! stays as it is.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use knot_crdt::update_items;
use uuid::Uuid;
use yrs::block::ItemContent;
use yrs::branch::Branch;
use yrs::types::text::YChange;
use yrs::types::{Attrs, TypeRef};
use yrs::{
    Any, Assoc, BranchID, Doc, OffsetKind, Out, ReadTxn, StateVector, StickyIndex, Text, Transact,
    TransactionMut, Xml, XmlElementRef, XmlFragment, XmlOut, XmlTextRef,
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
    // Read once, when a text first needs it.
    let mut held = None;
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
            XmlOut::Text(t) => links(txn, &t, offsets, &mut held, &mut out),
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
///
/// A chunk's length depends on what its item holds: a string counts its
/// length, and an embed (whatever its value) or an embedded type counts 1.
/// The diff shows an embedded string just like text, so the embeds of the
/// text are read from its items (`held`): a chunk at an embed's offset is
/// that embed. A text holding content the diff leaves out ([`Held`]) has
/// offsets the diff can't tell, and its links are left alone rather than
/// marked wrongly. So are the links of every text when the items can't be
/// read ([`HeldItems::read`], which logs it).
fn links<T: ReadTxn>(
    txn: &T,
    text: &XmlTextRef,
    offsets: OffsetKind,
    held: &mut Option<Option<HeldItems>>,
    out: &mut Vec<Site>,
) {
    let chunks = text.diff(txn, YChange::identity);
    // Only a text with a reference needs its offsets.
    let refers =
        |attrs: Option<&Attrs>| link_mark(attrs).is_some_and(|link| href_ref(&link).is_some());
    if !chunks
        .iter()
        .any(|chunk| refers(chunk.attributes.as_deref()))
    {
        return;
    }
    let Some(held) = held.get_or_insert_with(|| HeldItems::read(txn)) else {
        return;
    };
    let held = held.of(text);
    if held.iter().any(|&(_, held)| held == Held::Hidden) {
        return;
    }
    let embeds: HashSet<u32> = held
        .iter()
        .filter(|&&(_, held)| held == Held::Embed)
        .map(|&(at, _)| at)
        .collect();
    // (index, len, link mark): the runs of chunks with an equal link mark.
    let mut runs: Vec<(u32, u32, Option<LinkMark>)> = Vec::new();
    let mut index = 0;
    for chunk in chunks {
        let (len, link) = match &chunk.insert {
            Out::Any(Any::String(s)) if !embeds.contains(&index) => {
                (units(s, offsets), link_mark(chunk.attributes.as_deref()))
            }
            // An embed or an embedded type counts 1, and ends a link.
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

/// What an item of a text holds, where the text's diff can't tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    /// An embed, which counts 1 whatever its value. The diff shows an
    /// embedded string like text.
    Embed,
    /// Content the diff leaves out, which has a length too: `Any`, `JSON`,
    /// `Binary` and sub-document items. Only a crafted update puts them into
    /// a text.
    Hidden,
}

impl Held {
    /// What an item holding `content` holds, if the diff can't tell.
    fn of(content: &ItemContent) -> Option<Self> {
        match content {
            ItemContent::Embed(_) => Some(Held::Embed),
            ItemContent::Any(_)
            | ItemContent::JSON(_)
            | ItemContent::Binary(_)
            | ItemContent::Doc(..) => Some(Held::Hidden),
            _ => None,
        }
    }
}

/// The live items of a document that hold an embed or hidden content (see
/// [`Held`]), by the text they are in, with their offsets in it.
struct HeldItems(HashMap<BranchID, Vec<(u32, Held)>>);

impl HeldItems {
    /// Reads the items from the document's own state, decoded as yrs
    /// decodes an update, without integrating it
    /// ([`knot_crdt::update_items::decode`]).
    fn read<T: ReadTxn>(txn: &T) -> Option<Self> {
        let state = txn.encode_state_as_update_v1(&StateVector::default());
        Self::from_state(txn, &state)
    }

    /// Reads the items from `state`, the document's state as an update, and
    /// places each live one that is part of a text's content in that text,
    /// at its offset, found by its id (`StickyIndex::get_offset`).
    ///
    /// `None`, logged, if the state doesn't decode: the decoder follows
    /// yrs's own encoding, so a yrs that encodes differently ends here, and
    /// every link is then left as it is.
    fn from_state<T: ReadTxn>(txn: &T, state: &[u8]) -> Option<Self> {
        let mut found = Vec::new();
        for item in update_items::decode(state) {
            match item {
                Ok(item) => {
                    if let Some(held) = Held::of(&item.content) {
                        found.push((item.id, held));
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "remap: the page's state doesn't decode; its links are left as they are"
                    );
                    return None;
                }
            }
        }
        let mut texts: HashMap<BranchID, Vec<(u32, Held)>> = HashMap::new();
        for (id, held) in found {
            let Some(at) = StickyIndex::from_id(id, Assoc::After).get_offset(txn) else {
                continue;
            };
            // Not an item of a text: an element's attribute, for one.
            if !matches!(at.branch.type_ref(), TypeRef::XmlText) {
                continue;
            }
            // An attribute of the text is an item of it too, but not of its
            // content, and a deleted item is no longer part of it: only a
            // live item of its content is the one found at its offset.
            let first = StickyIndex::at(txn, at.branch, at.index, Assoc::After);
            if first.as_ref().and_then(StickyIndex::id) == Some(&id) {
                texts
                    .entry(at.branch.id())
                    .or_default()
                    .push((at.index, held));
            }
        }
        Some(Self(texts))
    }

    /// The items of `text`, by offset.
    fn of(&self, text: &XmlTextRef) -> &[(u32, Held)] {
        let branch: &Branch = text.as_ref();
        self.0.get(&branch.id()).map_or(&[], Vec::as_slice)
    }
}

/// The length of `s` in the document's offset units.
fn units(s: &str, offsets: OffsetKind) -> u32 {
    let n = match offsets {
        OffsetKind::Bytes => s.len(),
        OffsetKind::Utf16 => s.encode_utf16().count(),
    };
    n as u32
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Level, Metadata, Subscriber};
    use yrs::{Doc, Transact};

    use super::HeldItems;

    /// Counts the warnings logged while it is the subscriber.
    struct Warnings(Arc<AtomicUsize>);

    impl Subscriber for Warnings {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() == Level::WARN {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    #[test]
    fn a_state_that_does_not_decode_is_logged_and_read_as_no_items() {
        let doc = Doc::new();
        let txn = doc.transact();
        let warnings = Arc::new(AtomicUsize::new(0));
        // One client, then the buffer ends.
        let read = tracing::subscriber::with_default(Warnings(warnings.clone()), || {
            HeldItems::from_state(&txn, &[1])
        });
        assert!(read.is_none(), "no items are read");
        assert_eq!(warnings.load(Ordering::SeqCst), 1, "the failure is logged");
    }
}
