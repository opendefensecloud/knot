//! Encodings and checks of a doc's state that leave its pending content out.
//!
//! yrs 0.27.3's `encode_state_as_update_v1` writes the doc's blocks and
//! delete set, then merges in the v1 encoding of the doc's pending update
//! (`merge_pending_v1`, yrs `transaction.rs:375-391`). Once that pending
//! update holds overlapping Skips (yrs merges a parked remainder into it
//! without splitting its Skips, and `Update::merge` splits only
//! `Block::Item`), the encoding writes later pending blocks at shifted
//! clocks: the state names blocks the doc never had ("phantoms"). Every
//! state encoding of a live doc therefore goes through
//! [`encode_without_pending`], yrs's `encode_diff_v1`: the doc's own blocks
//! and delete set, without `merge_pending_v1`. Pending content travels as
//! the original bytes of the log rows that parked it.
//!
//! The rest of this module reads such bytes: whether state bytes written by
//! the shifted encoder may hold phantoms ([`may_be_shifted`]), the part of
//! them that cannot ([`trusted_prefix`]), and whether a doc's in-memory
//! pending update has a faithful v1 encoding ([`pending_update_v1`]).
//!
//! Every emptiness test of an [`IdSet`] goes through [`holds_any`]: a yrs
//! `IdSet` can keep clients whose ranges are empty, so `is_empty` would
//! call such a set non-empty.

use yrs::encoding::write::Write;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Doc, ID, IdSet, Options, ReadTxn, Snapshot, StateVector, Transact, Update};

use crate::engine::EngineError;

/// The doc's blocks from `sv` on and its whole delete set, as a v1 update,
/// without its pending content: `txn.encode_diff_v1(sv)`, which is
/// `write_blocks_from` plus `IdSet::from_store`, the same as yrs's
/// `encode_state_as_update_v1` minus `merge_pending_v1` (yrs
/// `transaction.rs:120-144`). It writes every block of a client the state
/// vector `sv` lacks, from its first block, blocks past a gap included.
pub fn encode_without_pending<T: ReadTxn>(txn: &T, sv: &StateVector) -> Vec<u8> {
    txn.encode_diff_v1(sv)
}

/// Whether `set` holds any id. Unlike `IdSet::is_empty`, a client whose
/// ranges are all empty does not count.
pub fn holds_any(set: &IdSet) -> bool {
    set.iter().any(|(_, ranges)| !ranges.is_empty())
}

/// The parts of `set` at or above each client's clock in `sv`.
pub fn at_or_above(set: &IdSet, sv: &StateVector) -> IdSet {
    let mut out = IdSet::new();
    for (client, ranges) in set.iter() {
        let clock = sv.get(client);
        for r in ranges.iter().filter(|r| r.end > clock) {
            let start = r.start.max(clock);
            out.insert(ID::new(*client, start), r.end - start);
        }
    }
    out
}

/// Whether the update `state` names a block at or past its client's first
/// gap: `Update::state_vector` stops at a client's first Skip, and counts a
/// client whose blocks start above clock 0 as clock 0 (yrs `update.rs:206-226`).
/// Shifted blocks sit there, since v1 writes clocks implicitly after each
/// Skip; so does real text a doc integrated past a lost edit. `false` when
/// the bytes do not decode.
pub fn names_blocks_past_a_gap(state: &[u8]) -> bool {
    let Ok(u) = Update::decode_v1(state) else {
        return false;
    };
    holds_any(&at_or_above(&u.insertions(true), &u.state_vector()))
}

/// Whether a fresh GC doc that applies `state` holds a pending update or a
/// pending delete set. `false` when the bytes do not decode.
pub fn parks_when_applied(state: &[u8]) -> bool {
    let Ok(u) = Update::decode_v1(state) else {
        return false;
    };
    let doc = Doc::new();
    let mut txn = doc.transact_mut();
    // A rejected block leaves what yrs parked before it parked.
    let _ = txn.apply_update(u);
    holds_pending(&txn)
}

/// Whether state bytes may hold blocks at shifted clocks: they name blocks
/// past a gap ([`names_blocks_past_a_gap`]). Shifted blocks sit past their
/// client's first gap, since v1 writes clocks implicitly after each Skip,
/// so bytes that name no block past a gap were not shifted. Real text a doc
/// integrated past a lost edit cannot be told from shifted blocks.
///
/// The plan's test also required that the bytes park content when applied
/// fresh ([`parks_when_applied`]); the spike's property found shifted states
/// that park nothing (the shift moved every pending block to ids whose
/// dependencies the bytes hold, so a fresh apply integrates the phantoms as
/// text) and shifted states that yrs refuses part way, so that test was
/// dropped (plan Task 1 Step 6, claim 1).
pub fn may_be_shifted(state: &[u8]) -> bool {
    names_blocks_past_a_gap(state)
}

/// A doc's pending update in v1 ([`pending_update_v1`]).
#[derive(Debug, PartialEq)]
pub enum PendingV1 {
    /// The doc holds no pending update.
    Empty,
    /// `pending.update.encode_v1()`, which decodes to the same
    /// `insertions(true)` as the in-memory pending update.
    Faithful(Vec<u8>),
    /// The encoding names other ids than the in-memory pending update: yrs
    /// shifted it (overlapping Skips).
    Shifted,
}

/// The doc's pending update encoded in v1, judged by its ids: `Faithful`
/// when the encoding decodes to the same `insertions(true)` as the
/// in-memory pending update (by set semantics), `Shifted` otherwise, and
/// `Empty` without a pending update. The id test cannot see content placed
/// wrongly within the same ids.
pub fn pending_update_v1<T: ReadTxn>(txn: &T) -> PendingV1 {
    let Some(pending) = txn.store().pending_update() else {
        return PendingV1::Empty;
    };
    let bytes = pending.update.encode_v1();
    match Update::decode_v1(&bytes) {
        Ok(decoded) if same_ids(&decoded.insertions(true), &pending.update.insertions(true)) => {
            PendingV1::Faithful(bytes)
        }
        _ => PendingV1::Shifted,
    }
}

/// A v1 update that holds no blocks and the delete set `ds`.
pub fn delete_only_update(ds: &IdSet) -> Vec<u8> {
    let mut encoder = EncoderV1::new();
    encoder.write_var(0u32);
    ds.encode(&mut encoder);
    encoder.to_vec()
}

/// The part of `state` that cannot hold a shifted block: every block below
/// its client's first gap, as the doc that applies `state` holds it, with
/// the deletions among them, and without the blocks whose origins lie
/// outside that part.
///
/// 1. Apply `state` to a scratch doc that keeps deleted content
///    (`skip_gc`); a decode error is [`EngineError::Decode`].
/// 2. Take its state vector without zero-clock entries (yrs's
///    `write_blocks_to` computes `clock - 1`). It stops at each client's
///    first gap, and holds no pending content.
/// 3. Take the scratch doc's delete set, cut to the ids below that state
///    vector.
/// 4. Encode the scratch doc's blocks up to that state vector with that
///    delete set (`encode_state_from_snapshot`).
/// 5. Apply those bytes to a fresh GC doc and return its
///    [`encode_without_pending`]: a prefix block whose origin lies outside
///    the prefix parks there and is dropped.
pub fn trusted_prefix(state: &[u8]) -> Result<Vec<u8>, EngineError> {
    let update = Update::decode_v1(state).map_err(|e| EngineError::Decode(e.to_string()))?;
    let scratch = Doc::with_options(Options {
        skip_gc: true,
        ..Options::default()
    });
    let deletes = update.delete_set().clone();
    let prefix = {
        let mut txn = scratch.transact_mut();
        if txn.apply_update(update).is_err() {
            // yrs refused a block (a shifted block can name a misplaced
            // parent) and stopped there, before the delete set: the doc
            // holds the blocks it integrated before that one. Apply the
            // delete set alone, so they keep their deletions.
            let deletes = Update::decode_v1(&delete_only_update(&deletes))
                .map_err(|e| EngineError::Decode(e.to_string()))?;
            txn.apply_update(deletes)
                .map_err(|e| EngineError::Apply(e.to_string()))?;
        }
        let sv: StateVector = txn
            .state_vector()
            .iter()
            .filter(|(_, clock)| **clock > 0)
            .map(|(client, clock)| (*client, *clock))
            .collect();
        let mut below = IdSet::new();
        for (client, clock) in sv.iter() {
            below.insert(ID::new(*client, 0), *clock);
        }
        let ds = txn.snapshot().delete_set.intersect(&below);
        let mut encoder = EncoderV1::new();
        txn.encode_state_from_snapshot(&Snapshot::new(sv, ds), &mut encoder)
            .map_err(|e| EngineError::Encode(e.to_string()))?;
        encoder.to_vec()
    };
    let fresh = Doc::new();
    let mut txn = fresh.transact_mut();
    let prefix = Update::decode_v1(&prefix).map_err(|e| EngineError::Decode(e.to_string()))?;
    txn.apply_update(prefix)
        .map_err(|e| EngineError::Apply(e.to_string()))?;
    Ok(encode_without_pending(&txn, &StateVector::default()))
}

/// Whether the doc holds a pending update or a pending delete set.
fn holds_pending<T: ReadTxn>(txn: &T) -> bool {
    let store = txn.store();
    store
        .pending_update()
        .is_some_and(|p| holds_any(&p.update.insertions(true)))
        || store.pending_ds().is_some_and(holds_any)
}

/// Whether `a` and `b` hold the same ids (set semantics: empty client
/// entries do not count).
fn same_ids(a: &IdSet, b: &IdSet) -> bool {
    !holds_any(&a.diff(b)) && !holds_any(&b.diff(a))
}
