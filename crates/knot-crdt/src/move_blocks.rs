//! Moving top-level blocks of a page into a new page ("Move to subpage",
//! `2026-11-13-move-to-subpage-design.md`).
//!
//! The new page's content is the page's full state with every other
//! top-level block deleted, so its items keep their ids and the
//! RelativePositions of the comments and suggestions on the moved blocks
//! resolve there unchanged. The room ([`crate::Event::MoveBlocksOut`]) holds
//! the doc while [`Commit`] stores the new page, then cuts the blocks and
//! leaves a link in their place.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use yrs::types::Attrs;
use yrs::types::TypeRef;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Doc, ReadTxn, StateVector, StickyIndex, Text, Transact, TransactionMut, Update, Xml,
    XmlElementPrelim, XmlElementRef, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim,
    XmlTextRef,
};

/// The root XmlFragment holding a page's content.
const FRAGMENT: &str = "default";

#[derive(Debug, thiserror::Error)]
pub enum MoveBlocksError {
    #[error("decode: {0}")]
    Decode(String),
    /// An anchor doesn't resolve to live content of the page, or the end
    /// lies before the start.
    #[error("anchor lost")]
    AnchorLost,
    /// [`Commit`] refused; nothing changed.
    #[error("refused: {0}")]
    Refused(String),
    /// The new page is stored but the cut wasn't: the room evicted itself.
    #[error("persist: {0}")]
    Persist(String),
}

/// What [`Commit`] gets: the new page's content as one update.
pub struct Prepared {
    pub child_state: Vec<u8>,
}

/// What [`Commit`] answers once the new page is stored: the link left in the
/// source, and the `meta` of the cut's history row.
pub struct Link {
    pub title: String,
    pub href: String,
    pub meta: serde_json::Value,
}

/// Stores the new page while the room holds the doc. `Err` changes nothing.
pub type Commit = Box<dyn FnOnce(Prepared) -> BoxFuture<'static, Result<Link, String>> + Send>;

/// The top-level blocks `first..=last` of `frag` that the anchors `start`
/// and `end` (raw RelativePosition v1 bytes) lie in.
pub fn block_range<T: ReadTxn>(
    txn: &T,
    frag: &XmlFragmentRef,
    start: &[u8],
    end: &[u8],
) -> Result<(u32, u32), MoveBlocksError> {
    let first = top_block(txn, frag, start)?;
    let last = top_block(txn, frag, end)?;
    if first > last {
        return Err(MoveBlocksError::AnchorLost);
    }
    Ok((first, last))
}

/// `state` (a page's full state) with every top-level block but
/// `first..=last` deleted.
pub fn child_state(state: &[u8], first: u32, last: u32) -> Result<Vec<u8>, MoveBlocksError> {
    let doc = load(state)?;
    let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
    let mut txn = doc.transact_mut();
    let len = frag.len(&txn);
    if len > last + 1 {
        frag.remove_range(&mut txn, last + 1, len - last - 1);
    }
    if first > 0 {
        frag.remove_range(&mut txn, 0, first);
    }
    Ok(txn.encode_state_as_update_v1(&StateVector::default()))
}

/// A page's state as a doc to read anchors against.
pub fn load(state: &[u8]) -> Result<Doc, MoveBlocksError> {
    let doc = Doc::new();
    // Before the update: gives the root its type, so the walk in
    // `top_block` recognizes it.
    doc.get_or_insert_xml_fragment(FRAGMENT);
    let update = Update::decode_v1(state).map_err(|e| MoveBlocksError::Decode(e.to_string()))?;
    doc.transact_mut()
        .apply_update(update)
        .map_err(|e| MoveBlocksError::Decode(e.to_string()))?;
    Ok(doc)
}

/// Whether both anchors resolve to live content of `doc` (a [`load`]ed new
/// page): the range lies entirely inside the moved blocks.
pub fn anchors_inside(doc: &Doc, start: &[u8], end: &[u8]) -> bool {
    let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
    let txn = doc.transact();
    block_range(&txn, &frag, start, end).is_ok()
}

/// Replaces blocks `first..=last` of `frag` with a paragraph linking `title`
/// to `href`.
pub fn cut(
    frag: &XmlFragmentRef,
    txn: &mut TransactionMut,
    first: u32,
    last: u32,
    title: &str,
    href: &str,
) {
    frag.remove_range(txn, first, last - first + 1);
    let paragraph = frag.insert(txn, first, XmlElementPrelim::empty("paragraph"));
    let text = paragraph.push_back(txn, XmlTextPrelim::new(""));
    let link = HashMap::from([("href".to_string(), Any::from(href))]);
    let attrs = Attrs::from([(Arc::from("link"), Any::Map(Arc::new(link)))]);
    text.insert_with_attributes(txn, 0, title, attrs);
}

/// The index in `frag` of the top-level block `anchor` lies in, when it and
/// every block around it is live.
fn top_block<T: ReadTxn>(
    txn: &T,
    frag: &XmlFragmentRef,
    anchor: &[u8],
) -> Result<u32, MoveBlocksError> {
    let anchor =
        StickyIndex::decode_v1(anchor).map_err(|e| MoveBlocksError::Decode(e.to_string()))?;
    let offset = anchor.get_offset(txn).ok_or(MoveBlocksError::AnchorLost)?;
    let mut node = match offset.branch.type_ref() {
        TypeRef::XmlText => XmlOut::Text(XmlTextRef::from(offset.branch)),
        TypeRef::XmlElement(_) => XmlOut::Element(XmlElementRef::from(offset.branch)),
        _ => return Err(MoveBlocksError::AnchorLost),
    };
    let root = XmlOut::Fragment(frag.clone()).as_ptr().id();
    let block = loop {
        if node.as_ptr().is_deleted() {
            return Err(MoveBlocksError::AnchorLost);
        }
        let parent = match &node {
            XmlOut::Text(t) => t.parent(),
            XmlOut::Element(e) => e.parent(),
            XmlOut::Fragment(_) => None,
        };
        match parent {
            Some(f @ XmlOut::Fragment(_)) if f.as_ptr().id() == root => break node,
            Some(parent) => node = parent,
            None => return Err(MoveBlocksError::AnchorLost),
        }
    };
    let id = block.as_ptr().id();
    (0..frag.len(txn))
        .find(|&i| frag.get(txn, i).is_some_and(|b| b.as_ptr().id() == id))
        .ok_or(MoveBlocksError::AnchorLost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yrs::updates::encoder::Encode;
    use yrs::{GetString, IndexedSequence};

    fn page(paragraphs: &[&str]) -> Doc {
        let doc = Doc::new();
        let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
        let mut txn = doc.transact_mut();
        for text in paragraphs {
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(*text));
        }
        drop(txn);
        doc
    }

    fn anchor(doc: &Doc, block: u32, index: u32, assoc: yrs::Assoc) -> Vec<u8> {
        let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
        let txn = doc.transact();
        let Some(XmlOut::Element(p)) = frag.get(&txn, block) else {
            panic!()
        };
        let Some(XmlOut::Text(t)) = p.get(&txn, 0) else {
            panic!()
        };
        t.sticky_index(&txn, index, assoc).unwrap().encode_v1()
    }

    fn texts(doc: &Doc) -> Vec<String> {
        let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
        let txn = doc.transact();
        (0..frag.len(&txn))
            .map(|i| match frag.get(&txn, i) {
                Some(XmlOut::Element(p)) => p.get_string(&txn),
                _ => panic!(),
            })
            .collect()
    }

    #[test]
    fn range_child_and_cut() {
        let doc = page(&["a1", "b2", "c3", "d4"]);
        let start = anchor(&doc, 1, 1, yrs::Assoc::After);
        let end = anchor(&doc, 2, 1, yrs::Assoc::Before);
        let frag = doc.get_or_insert_xml_fragment(FRAGMENT);
        let (first, last) = block_range(&doc.transact(), &frag, &start, &end).unwrap();
        assert_eq!((first, last), (1, 2));
        // Reversed: refused.
        assert!(block_range(&doc.transact(), &frag, &end, &start).is_err());

        let state = doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let child = load(&child_state(&state, first, last).unwrap()).unwrap();
        assert_eq!(
            texts(&child),
            ["<paragraph>b2</paragraph>", "<paragraph>c3</paragraph>"]
        );
        assert!(anchors_inside(&child, &start, &end));
        let outside = anchor(&doc, 0, 0, yrs::Assoc::After);
        assert!(!anchors_inside(&child, &outside, &end));

        cut(
            &frag,
            &mut doc.transact_mut(),
            first,
            last,
            "T",
            "knot://doc/x",
        );
        assert_eq!(texts(&doc).len(), 3);
        assert!(block_range(&doc.transact(), &frag, &start, &end).is_err());
    }
}
