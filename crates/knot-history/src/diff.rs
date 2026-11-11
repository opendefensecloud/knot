//! One entry's diff: the page at the entry's end next to the page without
//! the entry's own changes, on a union doc that holds both (architecture,
//! view_and_diff, "SHOW CHANGES: ONLY X'S OWN CHANGES").
//!
//! The replay `h` is a skip-GC doc replayed to the entry's `end_rev`, so
//! items deleted during the entry's window keep their content. With `I` and
//! `D` the entry's insert and delete sets and `S` the snapshot of `h`:
//!
//! - `removed = D − I`: pre-existing items the entry deleted;
//! - `added = I − S.delete_set`: the entry's insertions still alive;
//! - `S_lo = (S.state_map, S.delete_set − removed + I)`: the page at the
//!   entry's end without the entry's changes, in which another author's
//!   interleaved inserts are visible as in `S`, and another author's
//!   deletions are visible in neither, so they never show;
//! - the union doc is a fresh skip-GC doc holding `h`'s blocks with the
//!   delete set `S.delete_set − removed`, so its `default` fragment yields
//!   what either state shows (elements the entry removed included).
//!
//! [`text_chunks`] diffs every text of the union doc between `S` and
//! `S_lo`. Word counts ([`crate::words`]) and M1.4's "Show changes" view are
//! built on exactly this diff, so the counts match what the view highlights.

use yrs::block::ClientID;
use yrs::types::text::{ChangeKind, YChange};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{
    Any, Doc, IdSet, Options, Out, ReadTxn, Snapshot, StateVector, Text, Transact, Update,
    XmlFragment, XmlOut, XmlTextRef,
};

/// An entry's diff, ready to walk.
pub struct EntryDiff {
    /// The page at the entry's end (the snapshot of `h`).
    pub s: Snapshot,
    /// The page at the entry's end without the entry's own changes.
    pub s_lo: Snapshot,
    /// A skip-GC doc that shows what `s` or `s_lo` shows.
    pub union: Doc,
    /// The entry's insertions still alive at its end.
    pub added: IdSet,
    /// Pre-existing items the entry deleted.
    pub removed: IdSet,
}

/// Why an entry's diff could not be built.
#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    /// The union doc's state did not encode, decode or apply.
    #[error("the union doc does not build: {0}")]
    Encode(String),
    /// The replay garbage-collects deleted content, so removed text is gone.
    #[error("the replay does not keep deleted content")]
    NotKeepDeleted,
}

/// How a text chunk differs between the page without the entry's changes
/// and the page at its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    /// Visible in both states.
    Plain,
    /// Inserted by the entry and still there at its end.
    Added,
    /// There before the entry, deleted by it.
    Removed,
}

/// A run of text of one kind, as yrs's diff returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    /// The run of consecutive string inserts this chunk belongs to: a new
    /// segment starts at each text and after each non-string insert (an
    /// embedded node), so words never join across a paragraph or an inline
    /// node.
    pub segment: usize,
    pub kind: ChunkKind,
    pub text: String,
}

/// The diff of the entry whose insert set is `inserted` and whose delete
/// set is `deleted`, on `h`: a skip-GC doc replayed to the entry's
/// `end_rev`.
pub fn entry_diff(h: &Doc, inserted: &IdSet, deleted: &IdSet) -> Result<EntryDiff, DiffError> {
    if !h.skip_gc() {
        return Err(DiffError::NotKeepDeleted);
    }
    let removed = deleted.diff(inserted);
    let (s, state) = {
        let txn = h.transact();
        let s = txn.snapshot();
        let s_union = Snapshot::new(
            strip_zero_clocks(s.state_map.clone()),
            s.delete_set.diff(&removed),
        );
        let mut encoder = EncoderV1::new();
        txn.encode_state_from_snapshot(&s_union, &mut encoder)
            .map_err(|e| DiffError::Encode(e.to_string()))?;
        (s, encoder.to_vec())
    };
    let added = inserted.diff(&s.delete_set);
    let s_lo = Snapshot::new(
        s.state_map.clone(),
        s.delete_set.diff(&removed).merge(inserted),
    );
    let union = Doc::with_options(Options {
        skip_gc: true,
        ..Default::default()
    });
    let update = Update::decode_v1(&state).map_err(|e| DiffError::Encode(e.to_string()))?;
    union
        .transact_mut()
        .apply_update(update)
        .map_err(|e| DiffError::Encode(e.to_string()))?;
    pad_short_clients(&union, &s.state_map)?;
    Ok(EntryDiff {
        s,
        s_lo,
        union,
        added,
        removed,
    })
}

/// The text chunks of every text reachable from the union doc's `default`
/// fragment, depth-first, each diffed between `s` (hi) and `s_lo` (lo).
/// Only string inserts become chunks; an embedded node ends the current
/// segment.
///
/// Each text is diffed in a transaction of its own: yrs 0.27.3's
/// `diff_range` splits the whole doc by both snapshots on every call and
/// records every block of their delete sets in the transaction's
/// `merge_blocks` (`transaction.rs:1336-1362`), which only a commit drains,
/// so one transaction over all texts held (texts × those blocks) ids at
/// once: about 500 MiB on a 4 MiB page of interleaved typing (Task 5's
/// spike, `tests/diff_memory.rs`).
pub fn text_chunks(d: &EntryDiff) -> Vec<TextChunk> {
    let texts = {
        let txn = d.union.transact();
        let Some(root) = txn.get_xml_fragment("default") else {
            return Vec::new();
        };
        let mut texts = Vec::new();
        collect_texts(&txn, root.children(&txn), &mut texts);
        texts
    };

    let mut chunks = Vec::new();
    let mut segment = 0;
    let mut segment_used = false;
    for text in texts {
        if segment_used {
            segment += 1;
            segment_used = false;
        }
        let mut txn = d.union.transact_mut();
        for diff in text.diff_range(&mut txn, Some(&d.s), Some(&d.s_lo), |c: YChange| c) {
            match diff.insert {
                Out::Any(Any::String(s)) => {
                    let kind = match diff.ychange.map(|c| c.kind) {
                        None => ChunkKind::Plain,
                        Some(ChangeKind::Added) => ChunkKind::Added,
                        Some(ChangeKind::Removed) => ChunkKind::Removed,
                    };
                    chunks.push(TextChunk {
                        segment,
                        kind,
                        text: s.to_string(),
                    });
                    segment_used = true;
                }
                _ => {
                    if segment_used {
                        segment += 1;
                        segment_used = false;
                    }
                }
            }
        }
    }
    chunks
}

/// Pushes every text under `nodes`, depth-first in document order.
fn collect_texts<T: ReadTxn>(
    txn: &T,
    nodes: impl Iterator<Item = XmlOut>,
    out: &mut Vec<XmlTextRef>,
) {
    for node in nodes {
        match node {
            XmlOut::Text(t) => out.push(t),
            XmlOut::Element(e) => collect_texts(txn, e.children(txn), out),
            XmlOut::Fragment(f) => collect_texts(txn, f.children(txn), out),
        }
    }
}

/// The root that holds [`pad_short_clients`]'s blocks; never rendered.
const PAD_ROOT: &str = "knot-diff-pad";

/// Gives every client of `sv` whose clock is 1 or 2 one more block in the
/// union doc, at that clock, under [`PAD_ROOT`].
///
/// yrs 0.27.3's `diff_range` splits the doc at each snapshot clock
/// (`TransactionMut::split_by_snapshot`, `transaction.rs:1339-1347`), which
/// looks up the block at the client's full clock, past its last block.
/// `ClientBlockList::find_index` (`block_store.rs:49-74`) guesses the index
/// as `(clock / end) * right`, `end` being the last block's inclusive end:
/// at clock 1 that divides by zero (a client that typed one character), and
/// at clock 2 over two blocks (a client that only made an empty paragraph)
/// it indexes past the list. A block starting exactly at the clock takes
/// the lookup's first branch instead. The pad lives under its own root, so
/// no text of `default` shows it.
fn pad_short_clients(union: &Doc, sv: &StateVector) -> Result<(), DiffError> {
    use yrs::block::BLOCK_ITEM_ANY_REF_NUMBER;
    use yrs::encoding::write::Write;

    let short: Vec<(ClientID, u32)> = {
        let txn = union.transact();
        let held = txn.state_vector();
        sv.iter()
            .filter(|&(client, &clock)| (1..=2).contains(&clock) && held.get(client) == clock)
            .map(|(client, clock)| (*client, *clock))
            .collect()
    };
    if short.is_empty() {
        return Ok(());
    }
    // One update, one block per client: an `Any::Null` under the pad root
    // at the client's clock (no origins, so the parent is named), and an
    // empty delete set, so nothing the union doc holds changes.
    let mut encoder = EncoderV1::new();
    encoder.write_var(short.len() as u32);
    for &(client, clock) in &short {
        encoder.write_var(1u32);
        encoder.write_client(client);
        encoder.write_var(clock);
        encoder.write_info(BLOCK_ITEM_ANY_REF_NUMBER);
        encoder.write_parent_info(true);
        encoder.write_string(PAD_ROOT);
        encoder.write_len(1);
        encoder.write_any(&Any::Null);
    }
    encoder.write_var(0u32);
    let pad =
        Update::decode_v1(&encoder.to_vec()).map_err(|e| DiffError::Encode(format!("pad: {e}")))?;
    union
        .transact_mut()
        .apply_update(pad)
        .map_err(|e| DiffError::Encode(format!("pad: {e}")))?;
    Ok(())
}

/// `sv` without its zero clocks: yrs 0.27.3's `Store::write_blocks_to`
/// computes `find_index(clock - 1)` for every client of the snapshot that
/// the doc holds blocks of (`store.rs:179`), which underflows at clock 0.
/// A zero clock names no block, so dropping it changes nothing else.
pub fn strip_zero_clocks(sv: StateVector) -> StateVector {
    let kept: Vec<(ClientID, u32)> = sv
        .iter()
        .filter(|(_, clock)| **clock > 0)
        .map(|(client, clock)| (*client, *clock))
        .collect();
    let mut out = StateVector::default();
    for (client, clock) in kept {
        out.set_max(client, clock);
    }
    out
}
