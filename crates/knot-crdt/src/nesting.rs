//! How deep the shared types of a page may nest.
//!
//! yrs deletes and garbage-collects a nested type by recursing once per
//! level (`TransactionMut::delete` and `ItemContent::gc`, yrs 0.27), and so
//! does a walk of the Markdown writer once per block container. A stack
//! overflow aborts the whole server, so:
//! - the Markdown importer and writer follow a page only [`MAX_BLOCK_DEPTH`]
//!   block containers deep (`knot_markdown::to_markdown::MAX_DEPTH`), and
//!   write what lies deeper as its text;
//! - a room refuses an inbound update that would nest a type deeper than
//!   [`MAX_NESTING`] ([`check_update`]), before it applies, persists or fans
//!   out anything of it. The check has to come first: one crafted update can
//!   insert a deep subtree and delete it again, and yrs overflows applying
//!   it.
//!
//! So no edit can store a page that nests deeper. Pages stored before that
//! do are as they are; [`nests_deeper_than`] finds them (the real-data
//! check reports them).

use std::collections::{HashMap, HashSet};

use yrs::block::{
    BLOCK_GC_REF_NUMBER, BLOCK_SKIP_REF_NUMBER, HAS_ORIGIN, HAS_PARENT_SUB, HAS_RIGHT_ORIGIN,
    ItemContent,
};
use yrs::branch::BranchPtr;
use yrs::encoding::read::{Cursor, Read};
use yrs::types::text::YChange;
use yrs::updates::decoder::{Decoder, DecoderV1};
use yrs::updates::encoder::Encode;
use yrs::{
    ArrayRef, Assoc, BranchID, ClientID, ID, Map, MapRef, OffsetKind, Out, ReadTxn, StickyIndex,
    Text, Transact, Xml, XmlFragment,
};

use crate::engine::DocHandle;

/// How many block containers (blockquotes, list items, table cells) deep
/// the Markdown importer and writer follow a page. No page the editor makes
/// nests this deep; an import of `>` repeated, or a crafted update, can.
pub const MAX_BLOCK_DEPTH: usize = 128;

/// How deep the shared types of a page may nest: four types per block
/// container. A page within [`MAX_BLOCK_DEPTH`] nests at most three per
/// container (a table, its row and its cell; a list and its item; a
/// blockquote) plus a paragraph and its text, so every page the importer
/// stores, and every page the writer writes in full, is within it. A root
/// type is at depth 0, a type in it at depth 1. Deleting a subtree this deep
/// recurses well within a tokio worker's 2 MiB stack (`tests/nesting.rs`
/// deletes one twice as deep).
pub const MAX_NESTING: usize = 4 * MAX_BLOCK_DEPTH;

/// An update that would nest a type `depth` deep, past [`MAX_NESTING`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooDeep {
    /// The depth of the deepest type the check found, or the first it
    /// found past the limit.
    pub depth: usize,
}

/// Checks that applying `update`, a lib0 v1 update, to `doc` nests no type
/// deeper than [`MAX_NESTING`], without applying it.
///
/// The update's items are decoded as yrs decodes them, and each new type is
/// placed under its parent: one the update names, or the parent of the item
/// its origin points at. A parent in the update is followed up through the
/// update; a parent in `doc` is measured there. The structs yrs holds back
/// as pending (a dependency is missing) are read along with the update, as
/// it may be the one they wait for. A type whose parent is still missing
/// counts from depth 0, so a deep subtree is refused even before it can be
/// integrated.
///
/// The cost is a decode of the update (and of what is pending) and, for each
/// new type that hangs under existing content, a walk up from there. An
/// update that doesn't decode passes, since applying it fails the same way.
pub fn check_update(doc: &DocHandle, update: &[u8]) -> Result<(), TooDeep> {
    let txn = doc.inner().transact();
    let Ok(mut items) = decode_items(update) else {
        return Ok(());
    };
    if let Some(pending) = txn.store().pending_update()
        && let Ok(pending) = decode_items(&pending.update.encode_v1())
    {
        items.extend(pending);
    }
    let sv = txn.state_vector();
    // What `doc` already has, yrs leaves out of the update.
    items.retain(|item| item.id.clock + item.len > sv.get(&item.id.client));
    Placement::new(&txn, items, MAX_NESTING).check()
}

/// Whether a type of `doc`, under any of its roots, nests deeper than
/// `limit` (a root type is at depth 0). Read with an explicit stack, so that
/// no nesting can overflow it, and only `limit + 1` levels deep.
pub fn nests_deeper_than(doc: &DocHandle, limit: usize) -> bool {
    let txn = doc.inner().transact();
    let mut stack: Vec<(Out, usize)> = txn.root_refs().map(|(_, root)| (root, 0)).collect();
    while let Some((node, depth)) = stack.pop() {
        if depth > limit {
            return true;
        }
        stack.extend(
            nested(&txn, &node)
                .into_iter()
                .map(|child| (child, depth + 1)),
        );
    }
    false
}

/// The shared types directly in `node`: its children, attribute values and
/// embeds that are types.
fn nested<T: ReadTxn>(txn: &T, node: &Out) -> Vec<Out> {
    let mut found: Vec<Out> = match node {
        Out::YXmlFragment(f) => f.children(txn).map(Out::from).collect(),
        Out::YXmlElement(e) => e
            .children(txn)
            .map(Out::from)
            .chain(e.attributes(txn).map(|(_, v)| v))
            .collect(),
        Out::YXmlText(t) => t
            .diff(txn, YChange::identity)
            .into_iter()
            .map(|d| d.insert)
            .chain(t.attributes(txn).map(|(_, v)| v))
            .collect(),
        Out::YText(t) => t
            .diff(txn, YChange::identity)
            .into_iter()
            .map(|d| d.insert)
            .collect(),
        Out::YArray(a) => yrs::Array::iter(a, txn).collect(),
        Out::YMap(m) => m.iter(txn).map(|(_, v)| v).collect(),
        Out::UndefinedRef(b) => {
            let array = ArrayRef::from(*b);
            let map = MapRef::from(*b);
            yrs::Array::iter(&array, txn)
                .chain(map.iter(txn).map(|(_, v)| v))
                .collect()
        }
        _ => Vec::new(),
    };
    found.retain(|out| !matches!(out, Out::Any(_) | Out::YDoc(_)));
    found
}

/// An item of an update, as far as nesting goes.
#[derive(Debug, Clone)]
struct NewItem {
    id: ID,
    /// The clocks it spans.
    len: u32,
    origin: Option<ID>,
    right_origin: Option<ID>,
    parent: Parent,
    /// Whether it holds a shared type, which other items can nest in.
    is_type: bool,
}

/// The parent an item names.
#[derive(Debug, Clone)]
enum Parent {
    /// A root type.
    Root,
    /// The type of the item with this id.
    Item(ID),
    /// None: the item has the parent of its origin, or else of its right
    /// origin.
    OfOrigin,
}

/// The items of `update`, decoded as yrs decodes an update (yrs 0.27,
/// `Update::decode` and `decode_block`), without integrating them.
fn decode_items(update: &[u8]) -> Result<Vec<NewItem>, yrs::encoding::read::Error> {
    let mut decoder = DecoderV1::new(Cursor::new(update));
    let mut items = Vec::new();
    let clients: u32 = decoder.read_var()?;
    for _ in 0..clients {
        let blocks: u32 = decoder.read_var()?;
        let client = decoder.read_client()?;
        let mut clock: u32 = decoder.read_var()?;
        for _ in 0..blocks {
            let id = ID::new(client, clock);
            let info = decoder.read_info()?;
            match info {
                BLOCK_SKIP_REF_NUMBER => {
                    let len: u32 = decoder.read_var()?;
                    clock += len;
                }
                BLOCK_GC_REF_NUMBER => {
                    clock += decoder.read_len()?;
                }
                info => {
                    let item = decode_item(&mut decoder, id, info)?;
                    if let Some(item) = item {
                        clock += item.len;
                        items.push(item);
                    }
                }
            }
        }
    }
    Ok(items)
}

/// The item with `id` and `info`, read as yrs reads it, or `None` for an
/// empty one, which yrs drops.
fn decode_item(
    decoder: &mut DecoderV1,
    id: ID,
    info: u8,
) -> Result<Option<NewItem>, yrs::encoding::read::Error> {
    let no_origins = info & (HAS_ORIGIN | HAS_RIGHT_ORIGIN) == 0;
    let origin = if info & HAS_ORIGIN != 0 {
        Some(decoder.read_left_id()?)
    } else {
        None
    };
    let right_origin = if info & HAS_RIGHT_ORIGIN != 0 {
        Some(decoder.read_right_id()?)
    } else {
        None
    };
    let parent = if no_origins {
        if decoder.read_parent_info()? {
            decoder.read_string()?;
            Parent::Root
        } else {
            Parent::Item(decoder.read_left_id()?)
        }
    } else {
        Parent::OfOrigin
    };
    if no_origins && info & HAS_PARENT_SUB != 0 {
        decoder.read_string()?;
    }
    let content = ItemContent::decode(decoder, info)?;
    let len = content.len(OffsetKind::Utf16);
    if len == 0 {
        return Ok(None);
    }
    Ok(Some(NewItem {
        id,
        len,
        origin,
        right_origin,
        parent,
        is_type: matches!(content, ItemContent::Type(_)),
    }))
}

/// Where an item hangs.
#[derive(Debug, Clone, Copy)]
enum Attach {
    /// In a type of the doc, or a root, at this depth.
    Depth(usize),
    /// In the type of this new item.
    New(ID),
    /// Under nothing yrs can find yet (a missing dependency), or nothing yrs
    /// will keep it in: counted from depth 0.
    Missing,
}

/// The new items of an update placed under their parents, to measure how
/// deep the new types nest.
struct Placement<'a, T: ReadTxn> {
    txn: &'a T,
    limit: usize,
    /// The new items of each client, in clock order.
    items: HashMap<ClientID, Vec<NewItem>>,
    /// Where each new item hangs, by its id.
    attach: HashMap<ID, Attach>,
    /// The depth of each new type, by its id.
    depth: HashMap<ID, usize>,
    /// The depth of each type of the doc measured so far.
    existing: HashMap<BranchID, usize>,
}

impl<'a, T: ReadTxn> Placement<'a, T> {
    fn new(txn: &'a T, items: Vec<NewItem>, limit: usize) -> Self {
        let mut by_client: HashMap<ClientID, Vec<NewItem>> = HashMap::new();
        for item in items {
            by_client.entry(item.id.client).or_default().push(item);
        }
        for items in by_client.values_mut() {
            items.sort_by_key(|item| item.id.clock);
        }
        Self {
            txn,
            limit,
            items: by_client,
            attach: HashMap::new(),
            depth: HashMap::new(),
            existing: HashMap::new(),
        }
    }

    /// Whether every new type is within the limit.
    fn check(mut self) -> Result<(), TooDeep> {
        let types: Vec<ID> = self
            .items
            .values()
            .flatten()
            .filter(|item| item.is_type)
            .map(|item| item.id)
            .collect();
        for id in types {
            let depth = self.type_depth(id);
            if depth > self.limit {
                return Err(TooDeep { depth });
            }
        }
        Ok(())
    }

    /// The new item that spans `id`.
    fn find(&self, id: &ID) -> Option<&NewItem> {
        let items = self.items.get(&id.client)?;
        let at = items.partition_point(|item| item.id.clock <= id.clock);
        let item = items.get(at.checked_sub(1)?)?;
        (id.clock < item.id.clock + item.len).then_some(item)
    }

    /// The depth of the new type with `id`, its parents' depths on the way.
    fn type_depth(&mut self, id: ID) -> usize {
        // The new types from `id` up, until one whose depth is known or
        // that hangs in the doc.
        let mut path: Vec<ID> = Vec::new();
        let mut on_path: HashSet<ID> = HashSet::new();
        let mut cur = id;
        let base = loop {
            if let Some(&depth) = self.depth.get(&cur) {
                break depth;
            }
            if !on_path.insert(cur) {
                // A cycle, which yrs can't integrate.
                break 0;
            }
            path.push(cur);
            match self.attach_of(cur) {
                Attach::Depth(depth) => break depth,
                Attach::Missing => break 0,
                Attach::New(parent) => cur = parent,
            }
        };
        let mut depth = base;
        for id in path.into_iter().rev() {
            depth += 1;
            self.depth.insert(id, depth);
        }
        self.depth[&id]
    }

    /// Where the new item with `id` hangs, following origins through the
    /// update.
    fn attach_of(&mut self, id: ID) -> Attach {
        let mut chain: Vec<ID> = Vec::new();
        let mut seen: HashSet<ID> = HashSet::new();
        let mut cur = id;
        let attach = loop {
            if let Some(&attach) = self.attach.get(&cur) {
                break attach;
            }
            if !seen.insert(cur) {
                break Attach::Missing;
            }
            chain.push(cur);
            let Some(item) = self.find(&cur).cloned() else {
                break Attach::Missing;
            };
            match item.parent {
                Parent::Root => break Attach::Depth(0),
                Parent::Item(parent) => break self.type_at(&parent),
                Parent::OfOrigin => {
                    // yrs takes the parent of the origin, or, when that is
                    // missing or collected, of the right origin.
                    let mut next = None;
                    let mut found = None;
                    for origin in [item.origin, item.right_origin].into_iter().flatten() {
                        if let Some(new) = self.find(&origin) {
                            next = Some(new.id);
                            break;
                        }
                        if let Some(parent) = self.parent_in_doc(&origin) {
                            found = Some(Attach::Depth(self.existing_depth(parent)));
                            break;
                        }
                    }
                    match (next, found) {
                        (Some(next), _) => cur = next,
                        (None, Some(found)) => break found,
                        (None, None) => break Attach::Missing,
                    }
                }
            }
        };
        for id in chain {
            self.attach.insert(id, attach);
        }
        attach
    }

    /// Where an item whose parent is the item `parent` hangs: in that new
    /// type, or in that type of the doc.
    fn type_at(&mut self, parent: &ID) -> Attach {
        if let Some(item) = self.find(parent) {
            return if item.is_type {
                Attach::New(item.id)
            } else {
                Attach::Missing
            };
        }
        match BranchID::get_nested(self.txn, parent) {
            Some(branch) => Attach::Depth(self.existing_depth(branch)),
            None => Attach::Missing,
        }
    }

    /// The type of the doc that holds the item with `id`, if the doc has
    /// the item.
    fn parent_in_doc(&self, id: &ID) -> Option<BranchPtr> {
        StickyIndex::from_id(*id, Assoc::After)
            .get_offset(self.txn)
            .map(|offset| offset.branch)
    }

    /// The depth of `branch`, a type of the doc, read up to its root, or
    /// `limit + 1` when it is deeper than the limit: the walk goes no
    /// further.
    fn existing_depth(&mut self, branch: BranchPtr) -> usize {
        let mut chain: Vec<BranchID> = Vec::new();
        let mut cur = branch;
        let base = loop {
            let id = cur.id();
            if let Some(&depth) = self.existing.get(&id) {
                break depth;
            }
            let BranchID::Nested(item) = &id else {
                break 0;
            };
            chain.push(id.clone());
            if chain.len() > self.limit {
                // Too deep to measure: the types on the way keep no depth.
                return self.limit + 1;
            }
            match self.parent_in_doc(item) {
                Some(parent) => cur = parent,
                None => break 0,
            }
        };
        let mut depth = base;
        for id in chain.into_iter().rev() {
            depth += 1;
            self.existing.insert(id, depth);
        }
        self.existing.get(&branch.id()).copied().unwrap_or(base)
    }
}
