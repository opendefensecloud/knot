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
//! call such a set non-empty. Every equality of pending ids goes through
//! [`PendingClaims`]'s set semantics for the same reason.
//!
//! The rest reads and replays pending content as boot and the room do:
//! - [`claims`] names a doc's pending structs and pending deletes, and
//!   [`ParkedRows`] keeps an oldest-first cover of them by the log rows that
//!   parked them (Ruling 4);
//! - [`GuardedDoc`] applies rows over a doc that may hold blocks past a gap
//!   behind the skip-underflow guards (Ruling 8): the pre-check
//!   [`would_underflow`] before an apply, and the post-check
//!   [`holds_only_what_it_was_given`] after one, both ported from
//!   knot-history's `Replayer`; [`PastGap`] keeps the blocks the guards
//!   watch current without re-encoding the doc;
//! - [`check_reparks`] replays a snapshot's bytes and the rows it names on a
//!   scratch doc, exactly as boot will, and [`carry_rows`] encodes pending
//!   content that no row carries (Ruling 19).

use std::collections::BTreeSet;

use yrs::block::ClientID;
use yrs::encoding::write::Write;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Doc, ID, IdSet, Options, ReadTxn, Snapshot, StateVector, Transact, Update, WriteTxn};

use crate::engine::{DocHandle, Engine, EngineError};

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
///    (`skip_gc`); a decode error is [`EngineError::Decode`]. yrs can
///    refuse shifted bytes part way (a shifted block names a string item
///    as its parent, `UpdateError::InvalidParent`), and then stops before
///    the delete set: the scratch doc keeps the blocks yrs integrated
///    before the refused one, and the delete set is applied alone.
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

// ---------------------------------------------------------------------------
// Pending claims
// ---------------------------------------------------------------------------

/// Pending structs and pending deletes, kept apart: one id can be pending
/// twice, as a parked struct and as a parked delete of it (Ruling 4).
///
/// Equality is set semantics, per set: an `IdSet` diff can leave a client
/// whose ranges are empty, and yrs's derived `PartialEq` (over its
/// `BTreeMap`) would then tell equal sets apart (Ruling 9).
#[derive(Clone, Debug, Default)]
pub struct PendingClaims {
    pub structs: IdSet,
    pub deletes: IdSet,
}

impl PartialEq for PendingClaims {
    fn eq(&self, other: &Self) -> bool {
        same_ids(&self.structs, &other.structs) && same_ids(&self.deletes, &other.deletes)
    }
}

impl PendingClaims {
    /// Whether either set holds an id.
    fn holds_any(&self) -> bool {
        holds_any(&self.structs) || holds_any(&self.deletes)
    }
}

/// The doc's pending claims: `structs` the insertions of its pending update,
/// `deletes` its pending delete set; `None` when both are empty. These are
/// in-memory ids, which yrs's encoding defect does not touch.
pub fn claims(doc: &DocHandle) -> Option<PendingClaims> {
    let claims = pending_of(&doc.0.transact());
    claims.holds_any().then_some(claims)
}

/// A row's claims: `structs` its `insertions(true)`, `deletes` its delete
/// set. `None` when the bytes do not decode.
pub fn row_claims(update: &[u8]) -> Option<PendingClaims> {
    let u = Update::decode_v1(update).ok()?;
    Some(PendingClaims {
        structs: u.insertions(true),
        deletes: u.delete_set().clone(),
    })
}

/// A doc's pending structs and pending deletes, possibly empty.
fn pending_of<T: ReadTxn>(txn: &T) -> PendingClaims {
    let store = txn.store();
    PendingClaims {
        structs: store
            .pending_update()
            .map(|p| p.update.insertions(true))
            .unwrap_or_default(),
        deletes: store.pending_ds().cloned().unwrap_or_default(),
    }
}

/// The blocks a doc holds, deleted ones and blocks past a gap included: the
/// insertions of its pending-free state.
fn held<T: ReadTxn>(txn: &T) -> IdSet {
    Update::decode_v1(&encode_without_pending(txn, &StateVector::default()))
        .map(|u| u.insertions(true))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The skip-underflow guards (Ruling 8)
// ---------------------------------------------------------------------------

/// The blocks `doc` holds at or above its clients' state-vector clocks:
/// past each client's first gap.
pub fn past_gap(doc: &DocHandle) -> IdSet {
    let txn = doc.0.transact();
    at_or_above(&held(&txn), &txn.state_vector())
}

/// Every block `clients` hold in the doc of `txn`, blocks past a gap and
/// deleted ones included: the insertions of `encode_diff_v1` from the doc's
/// state vector without `clients`' entries, kept to `clients`. yrs writes
/// every block of a client the remote state vector lacks, from its first
/// block (`diff_state_vectors`), and no block of a client whose clock it
/// already has, so only `clients`' blocks are encoded; the delete set
/// (`IdSet::from_store`) still walks every block of the doc.
pub fn held_of<T: ReadTxn>(txn: &T, clients: &BTreeSet<ClientID>) -> IdSet {
    let from: StateVector = txn
        .state_vector()
        .iter()
        .filter(|(client, _)| !clients.contains(client))
        .map(|(client, clock)| (*client, *clock))
        .collect();
    let Ok(update) = Update::decode_v1(&txn.encode_diff_v1(&from)) else {
        return IdSet::new();
    };
    keep_clients(&update.insertions(true), clients)
}

/// The part of `set` of `clients`.
fn keep_clients(set: &IdSet, clients: &BTreeSet<ClientID>) -> IdSet {
    let mut out = IdSet::new();
    for (client, ranges) in set.iter().filter(|(c, _)| clients.contains(c)) {
        for r in ranges.iter() {
            out.insert(ID::new(*client, r.start), r.end - r.start);
        }
    }
    out
}

/// Whether applying `update` to a doc that holds `past_gap` past its
/// clients' first gaps would reach yrs 0.27.3's skip underflow
/// (`block_store.rs:318`): some GC block of the update starts before the
/// start of a `past_gap` range of its client and ends after it. Ported from
/// knot-history's `Replayer::would_underflow`, whose doc comment explains
/// the shape (`tests/common/fixtures.rs`' `skip_underflow_case`). A decoded
/// update's items are never marked deleted, so its GC blocks are
/// `insertions(true) - insertions(false)`.
pub fn would_underflow(past_gap: &IdSet, update: &Update) -> bool {
    if !holds_any(past_gap) {
        return false;
    }
    let gc = update.insertions(true).diff(&update.insertions(false));
    gc_block_straddles_a_skip_end(&gc, past_gap)
}

/// Whether a range of `gc` starts before and ends after the start of a
/// range of `past_gap` of the same client.
fn gc_block_straddles_a_skip_end(gc: &IdSet, past_gap: &IdSet) -> bool {
    gc.iter().any(|(client, ranges)| {
        past_gap.iter().any(|(c, gap)| {
            c == client
                && ranges
                    .iter()
                    .any(|r| gap.iter().any(|g| r.start < g.start && g.start < r.end))
        })
    })
}

/// The post-apply check, ported from knot-history's
/// `holds_only_what_it_was_given`: after an apply to a doc that held blocks
/// past a gap, the transaction integrated nothing the doc already held (yrs
/// trims what a doc holds before integrating, so a correct apply never
/// does), the doc holds only what it held before and what it was `given`,
/// and no client's highest held clock is past both. `given` is what the
/// update brought and what was pending before the apply: an apply may drain
/// parked content the update does not carry. A release build wraps the
/// underflow instead of panicking, which only the first condition sees.
///
/// The sets may be restricted to the clients an apply can change
/// ([`held_of`]): every condition is per client.
pub fn holds_only_what_it_was_given(
    held_before: &IdSet,
    held_after: &IdSet,
    given: &IdSet,
    inserted: &IdSet,
) -> bool {
    if holds_any(&inserted.intersect(held_before)) {
        return false;
    }
    let allowed = held_before.merge(given);
    if holds_any(&held_after.diff(&allowed)) {
        return false;
    }
    held_after.iter().all(|(client, ranges)| {
        let highest = ranges.iter().map(|r| r.end).max().unwrap_or(0);
        highest <= highest_clock(held_before, client).max(highest_clock(given, client))
    })
}

/// The end of `client`'s last range in `set`, 0 when it has none.
fn highest_clock(set: &IdSet, client: &ClientID) -> u32 {
    set.iter()
        .find(|(c, _)| *c == client)
        .and_then(|(_, ranges)| ranges.iter().map(|r| r.end).max())
        .unwrap_or(0)
}

/// The blocks a doc holds past its clients' first gap ([`past_gap`]), kept
/// current without re-encoding the doc: each apply merges its insert set and
/// cuts the result at the doc's new state vector (Rulings 8 and 25).
#[derive(Clone, Debug, Default)]
pub struct PastGap(IdSet);

impl PastGap {
    /// [`past_gap`] of `doc`: one encode of the whole doc.
    pub fn of(doc: &DocHandle) -> PastGap {
        PastGap(past_gap(doc))
    }

    pub fn ids(&self) -> &IdSet {
        &self.0
    }

    /// Whether `update` meets the skip underflow over these blocks
    /// ([`would_underflow`]). `false` at once while the set holds nothing,
    /// and for bytes that do not decode (their apply fails as it does
    /// without the guard).
    pub fn refuses(&self, update: &[u8]) -> bool {
        if !holds_any(&self.0) {
            return false;
        }
        match Update::decode_v1(update) {
            Ok(u) => would_underflow(&self.0, &u),
            Err(_) => false,
        }
    }

    /// Takes in what an apply to `doc` `inserted`.
    pub fn note(&mut self, doc: &DocHandle, inserted: &IdSet) {
        let sv = doc.0.transact().state_vector();
        self.note_at(inserted, &sv);
    }

    fn note_at(&mut self, inserted: &IdSet, sv: &StateVector) {
        if !holds_any(inserted) && !holds_any(&self.0) {
            return;
        }
        self.0.merge_with(inserted.clone());
        self.0 = at_or_above(&self.0, sv);
    }
}

/// Why [`GuardedDoc`] did not apply a row cleanly.
#[derive(Debug, PartialEq)]
pub enum Refused {
    /// The pre-check: the row meets the skip underflow. Nothing was applied.
    Underflow,
    /// The post-check: the apply integrated something the doc did not hold
    /// and was not given (a wrapped underflow). The doc holds it.
    Corrupt,
    /// The row does not decode, or yrs rejected it (part way: the doc keeps
    /// what yrs integrated before the rejected block).
    Apply(String),
}

/// A doc whose every apply goes through the skip-underflow guards
/// (Ruling 8): boot's, and the re-park check's scratch doc.
pub struct GuardedDoc {
    doc: DocHandle,
    past_gap: PastGap,
}

impl GuardedDoc {
    /// Encodes `doc` once, for its [`PastGap`].
    pub fn new(doc: DocHandle) -> GuardedDoc {
        let past_gap = PastGap::of(&doc);
        GuardedDoc { doc, past_gap }
    }

    /// Applies `row` in one yrs transaction, as knot-history's `Replayer`
    /// does: refused before the apply when it meets the skip underflow
    /// ([`would_underflow`]), and checked after it
    /// ([`holds_only_what_it_was_given`]) when it can change a client that
    /// holds blocks past a gap. Those clients are the row's (its insertions
    /// and its delete set) and those of the pending structs before the
    /// apply, which a drain integrates; `given` is the row's insertions and
    /// those pending structs.
    pub fn apply(&mut self, row: &[u8]) -> Result<(), Refused> {
        let update = Update::decode_v1(row).map_err(|e| Refused::Apply(e.to_string()))?;
        if would_underflow(self.past_gap.ids(), &update) {
            return Err(Refused::Underflow);
        }
        let brought = update.insertions(true);
        let clients = brought
            .client_ids()
            .chain(update.delete_set().client_ids())
            .collect();
        self.guarded(clients, brought, |txn| txn.apply_update(update))
    }

    /// Ruling 8's closing retry: makes yrs re-apply the doc's pending
    /// update, as the arrival of a missing dependency does live. yrs 0.27.3
    /// retries only when a missing client's clock (the end of its block
    /// list) moves past the clock it recorded (`transaction.rs:838-842`),
    /// so content parked on an id that a later row filled below that end
    /// stays stuck. Setting every recorded clock to 0 and applying an empty
    /// update forces the retry (`transaction.rs:874-880`), which takes the
    /// pending update out and parks again only what still cannot integrate.
    /// A no-op without a pending update. The pre-check runs over the pending
    /// update, the post-check as for a row of the pending update's clients.
    pub fn retry_pending(&mut self) -> Result<(), Refused> {
        let pending = {
            let txn = self.doc.0.transact();
            let Some(p) = txn.store().pending_update() else {
                return Ok(());
            };
            if would_underflow(self.past_gap.ids(), &p.update) {
                return Err(Refused::Underflow);
            }
            p.update.insertions(true)
        };
        let clients = pending.client_ids().collect();
        self.guarded(clients, IdSet::new(), |txn| {
            if let Some(p) = txn.store_mut().pending_update_mut() {
                let missing: Vec<ClientID> = p.missing.iter().map(|(c, _)| *c).collect();
                for client in missing {
                    p.missing.set_min(client, 0);
                }
            }
            txn.apply_update(Update::new())
        })
    }

    /// The re-apply pass after the closing retry (Ruling 8's fallback):
    /// each of `rows` (the rows the doc applied, in rev order) whose claims
    /// meet the doc's remaining pending structs or pending deletes is
    /// applied once more. yrs parks a client's blocks behind the first one
    /// it cannot integrate in one merged pending update, and its retry
    /// stops at that block again: content behind it whose own dependencies
    /// arrived later (in a row applied after it in rev order, but before it
    /// in the live room) stays parked. Its own row, applied alone, carries
    /// no such block. A row the pre-check refuses is skipped; ends at once
    /// when nothing is pending.
    pub fn reapply_parking_rows<'a>(
        &mut self,
        rows: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<(), Refused> {
        for row in rows {
            let Some(pending) = claims(&self.doc) else {
                return Ok(());
            };
            let Some(row_claims) = row_claims(row) else {
                continue;
            };
            if !holds_any(&row_claims.structs.intersect(&pending.structs))
                && !holds_any(&row_claims.deletes.intersect(&pending.deletes))
            {
                continue;
            }
            match self.apply(row) {
                Ok(()) | Err(Refused::Underflow) => {}
                Err(refused) => return Err(refused),
            }
        }
        Ok(())
    }

    /// What every boot rule and the re-park check end with: the closing
    /// [`GuardedDoc::retry_pending`], then
    /// [`GuardedDoc::reapply_parking_rows`] over `rows`. `Underflow` when
    /// the retry's pre-check refused it (the doc is as it was, and the pass
    /// still ran); `Corrupt` or `Apply` from either.
    pub fn close<'a>(&mut self, rows: impl IntoIterator<Item = &'a [u8]>) -> Result<(), Refused> {
        let retried = self.retry_pending();
        if let Err(refused @ (Refused::Corrupt | Refused::Apply(_))) = retried {
            return Err(refused);
        }
        self.reapply_parking_rows(rows)?;
        retried
    }

    /// Runs `apply` in one transaction and keeps `past_gap` current. When
    /// `clients`, or the clients of the pending structs before the apply,
    /// hold blocks past a gap, it reads what those clients hold before and
    /// after ([`held_of`]) and runs the post-check with `given` merged with
    /// those pending structs.
    fn guarded(
        &mut self,
        mut clients: BTreeSet<ClientID>,
        given: IdSet,
        apply: impl FnOnce(&mut yrs::TransactionMut) -> Result<(), yrs::error::UpdateError>,
    ) -> Result<(), Refused> {
        let mut txn = self.doc.0.transact_mut();
        let pending_before = pending_of(&txn).structs;
        clients.extend(pending_before.client_ids());
        let checks = self
            .past_gap
            .ids()
            .iter()
            .any(|(c, r)| !r.is_empty() && clients.contains(c));
        let held_before = checks.then(|| held_of(&txn, &clients));
        let result = apply(&mut txn);
        let inserted = txn.insert_set().clone();
        txn.commit();
        let sv = txn.state_vector();
        let corrupt = held_before.is_some_and(|before| {
            let after = held_of(&txn, &clients);
            let given = given.merge(&pending_before);
            !holds_only_what_it_was_given(&before, &after, &given, &inserted)
        });
        drop(txn);
        self.past_gap.note_at(&inserted, &sv);
        if corrupt {
            return Err(Refused::Corrupt);
        }
        result.map_err(|e| Refused::Apply(e.to_string()))
    }

    /// Whether every id `row` names is held or pending in the doc
    /// ([`holds`]): skipping it changes nothing.
    pub fn holds(&self, row: &[u8]) -> bool {
        holds(&self.doc, row)
    }

    pub fn doc(&self) -> &DocHandle {
        &self.doc
    }

    pub fn into_parts(self) -> (DocHandle, PastGap) {
        (self.doc, self.past_gap)
    }
}
/// Whether `doc` holds every id `row` names (Rulings 8 and 25): its
/// insertions are held (the insertions of the doc's pending-free state) or
/// pending structs, and its delete set is deleted in the doc
/// (`snapshot().delete_set`) or pending deletes. Applying such a row
/// changes nothing. `false` when the row does not decode.
pub fn holds(doc: &DocHandle, row: &[u8]) -> bool {
    let Some(row) = row_claims(row) else {
        return false;
    };
    let txn = doc.0.transact();
    let pending = pending_of(&txn);
    let structs = held(&txn).merge(&pending.structs);
    let deletes = txn.snapshot().delete_set.merge(&pending.deletes);
    !holds_any(&row.structs.diff(&structs)) && !holds_any(&row.deletes.diff(&deletes))
}

// ---------------------------------------------------------------------------
// The re-park check and carried rows
// ---------------------------------------------------------------------------

/// Why [`check_reparks`] failed.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// After the replay, these live pending ids are neither held nor
    /// pending (structs) or neither deleted nor pending (deletes).
    Uncovered(PendingClaims),
    /// The guards refused the row at this rev.
    Underflow(i64),
}

/// Pending ids a cover leaves out ([`ParkedRows::prune`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Uncovered {
    pub missing: PendingClaims,
}

/// Replays a snapshot's `state` and the `rows` it names (in the given rev
/// order) on a scratch doc exactly as boot will (Ruling 8): `state` on
/// `engine.new_doc()` (pending-free bytes need no guard), each row through
/// a [`GuardedDoc`], then its closing [`GuardedDoc::close`] over the same
/// rows. Any refusal of a row is `Underflow(rev)`; the closing retry is
/// skipped on its pre-check's refusal (it changed nothing), and any other
/// refusal while closing is `Underflow` of the last row's rev. Then every struct of `pending` must be held or
/// pending in the scratch doc, and every delete of it deleted or pending
/// there; `Uncovered` names the rest. Bytes that do not apply leave every
/// pending id uncovered.
pub fn check_reparks(
    engine: &dyn Engine,
    state: &[u8],
    rows: &[(i64, &[u8])],
    pending: &PendingClaims,
) -> Result<(), Refusal> {
    let doc = engine.new_doc();
    if engine.apply_update(&doc, state).is_err() {
        return Err(Refusal::Uncovered(pending.clone()));
    }
    let mut scratch = GuardedDoc::new(doc);
    for &(rev, bytes) in rows {
        scratch.apply(bytes).map_err(|_| Refusal::Underflow(rev))?;
    }
    match scratch.close(rows.iter().map(|(_, bytes)| *bytes)) {
        Ok(()) | Err(Refused::Underflow) => {}
        Err(_) => {
            return Err(match rows.last() {
                Some(&(rev, _)) => Refusal::Underflow(rev),
                None => Refusal::Uncovered(pending.clone()),
            });
        }
    }
    let txn = scratch.doc.0.transact();
    let have = pending_of(&txn);
    let missing = PendingClaims {
        structs: pending.structs.diff(&held(&txn).merge(&have.structs)),
        deletes: pending
            .deletes
            .diff(&txn.snapshot().delete_set.merge(&have.deletes)),
    };
    if missing.holds_any() {
        return Err(Refusal::Uncovered(missing));
    }
    Ok(())
}

/// Rows that carry the doc's pending content again (Rulings 9 and 19): its
/// pending structs as their faithful v1 encoding ([`PendingV1::Faithful`]),
/// if any, and its pending deletes as a delete-only update, if any. `None`
/// when the pending structs' encoding is [`PendingV1::Shifted`].
pub fn carry_rows(doc: &DocHandle) -> Option<Vec<Vec<u8>>> {
    let txn = doc.0.transact();
    let mut rows = Vec::new();
    match pending_update_v1(&txn) {
        PendingV1::Empty => {}
        PendingV1::Faithful(bytes) => rows.push(bytes),
        PendingV1::Shifted => return None,
    }
    if let Some(ds) = txn.store().pending_ds().filter(|ds| holds_any(ds)) {
        rows.push(delete_only_update(ds));
    }
    Some(rows)
}

// ---------------------------------------------------------------------------
// The rows that park the pending content (Ruling 4)
// ---------------------------------------------------------------------------

/// Ties an own row a room recorded before its rev was known to the writer
/// job that persists it (Ruling 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowToken(u64);

/// A recorded row.
#[derive(Debug)]
struct ParkedRow {
    token: RowToken,
    rev: Option<i64>,
    bytes: Vec<u8>,
    claims: PendingClaims,
    /// The row's GC blocks (`insertions(true) - insertions(false)`), cached
    /// for the underflow pre-check.
    #[allow(dead_code)] // read by no check yet: the room's re-park check decodes the bytes.
    gc: IdSet,
}

/// The log rows whose original bytes re-park a doc's pending content
/// (Ruling 4): rows whose apply changed the pending content, trimmed to an
/// oldest-first cover of the pending structs and the pending deletes.
#[derive(Debug, Default)]
pub struct ParkedRows {
    rows: Vec<ParkedRow>,
    next_token: u64,
    /// Revs a re-park check refused: never kept again (Ruling 9).
    excluded: BTreeSet<i64>,
}

impl ParkedRows {
    /// Records a row (an own row has no rev yet). Decodes its claims and its
    /// GC set once; `None`, and nothing recorded, for bytes that do not
    /// decode.
    pub fn record(&mut self, rev: Option<i64>, bytes: Vec<u8>) -> Option<RowToken> {
        let update = Update::decode_v1(&bytes).ok()?;
        let structs = update.insertions(true);
        let gc = structs.diff(&update.insertions(false));
        let claims = PendingClaims {
            structs,
            deletes: update.delete_set().clone(),
        };
        let token = RowToken(self.next_token);
        self.next_token += 1;
        self.rows.push(ParkedRow {
            token,
            rev,
            bytes,
            claims,
            gc,
        });
        Some(token)
    }

    /// Sets the rev of the row recorded with `token`; a no-op when `prune`
    /// dropped it.
    pub fn assign(&mut self, token: RowToken, rev: i64) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.token == token) {
            row.rev = Some(rev);
        }
    }

    /// The row at `rev` is never kept again (Ruling 9).
    pub fn exclude(&mut self, rev: i64) {
        self.excluded.insert(rev);
    }

    pub fn is_excluded(&self, rev: i64) -> bool {
        self.excluded.contains(&rev)
    }

    /// Trims the rows to Ruling 4's cover of `pending`: walking them in rev
    /// order (unassigned rows last, in record order), a row is kept only
    /// when its structs meet a pending struct, or its deletes a pending
    /// delete, that no row kept before it covers in the same set; excluded
    /// rows are never kept. `None` (nothing pending) clears the rows.
    /// `Uncovered` names the pending ids no kept row claims; the rows are
    /// trimmed either way.
    pub fn prune(&mut self, pending: Option<&PendingClaims>) -> Result<(), Uncovered> {
        let Some(pending) = pending else {
            self.rows.clear();
            return Ok(());
        };
        // Stable: unassigned rows keep their record order.
        self.rows.sort_by_key(|r| (r.rev.is_none(), r.rev));
        let mut covered = PendingClaims::default();
        let excluded = &self.excluded;
        self.rows.retain(|row| {
            if row.rev.is_some_and(|rev| excluded.contains(&rev)) {
                return false;
            }
            let structs = row
                .claims
                .structs
                .intersect(&pending.structs)
                .diff(&covered.structs);
            let deletes = row
                .claims
                .deletes
                .intersect(&pending.deletes)
                .diff(&covered.deletes);
            let keep = holds_any(&structs) || holds_any(&deletes);
            if keep {
                covered.structs.merge_with(structs);
                covered.deletes.merge_with(deletes);
            }
            keep
        });
        let missing = PendingClaims {
            structs: pending.structs.diff(&covered.structs),
            deletes: pending.deletes.diff(&covered.deletes),
        };
        if missing.holds_any() {
            return Err(Uncovered { missing });
        }
        Ok(())
    }

    /// The rows of `rows` (in rev order) whose claims meet `pending`
    /// (structs with structs, deletes with deletes), recorded with their
    /// revs and pruned to the cover: boot's derivation from the rows it
    /// applied.
    pub fn derive<'a>(
        rows: impl IntoIterator<Item = (i64, &'a [u8])>,
        pending: &PendingClaims,
    ) -> (ParkedRows, Result<(), Uncovered>) {
        let mut parked = ParkedRows::default();
        for (rev, bytes) in rows {
            let Some(claims) = row_claims(bytes) else {
                continue;
            };
            if holds_any(&claims.structs.intersect(&pending.structs))
                || holds_any(&claims.deletes.intersect(&pending.deletes))
            {
                parked.record(Some(rev), bytes.to_vec());
            }
        }
        let covered = parked.prune(Some(pending));
        (parked, covered)
    }

    /// The kept rows' revs, ascending. Only snapshot code calls it, once no
    /// row is in flight, so every kept row has its rev.
    pub fn revs(&self) -> Vec<i64> {
        debug_assert!(
            self.rows.iter().all(|r| r.rev.is_some()),
            "a kept row has no rev yet"
        );
        self.revs_assigned()
    }

    /// The kept rows' assigned revs, ascending, for readers that run while
    /// rows are in flight.
    pub fn revs_assigned(&self) -> Vec<i64> {
        let mut revs: Vec<i64> = self.rows.iter().filter_map(|r| r.rev).collect();
        revs.sort_unstable();
        revs
    }

    /// The kept rows with a rev, in rev order, for [`check_reparks`].
    pub fn checked_rows(&self) -> Vec<(i64, &[u8])> {
        let mut rows: Vec<(i64, &[u8])> = self
            .rows
            .iter()
            .filter_map(|r| r.rev.map(|rev| (rev, r.bytes.as_slice())))
            .collect();
        rows.sort_by_key(|(rev, _)| *rev);
        rows
    }

    /// The kept rows' bytes in cover order.
    pub fn frames(&self) -> Vec<Vec<u8>> {
        self.rows.iter().map(|r| r.bytes.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::YrsEngine;
    use crate::test_fixtures::{
        cross_client_parked, cross_client_parked_tail, overlapping_skips, overlapping_skips_as,
        para, skip_underflow_case, skip_underflow_parts,
    };

    /// A fresh doc with `updates` applied in order.
    fn doc_of(updates: &[&[u8]]) -> DocHandle {
        let d = YrsEngine.new_doc();
        for u in updates {
            YrsEngine.apply_update(&d, u).expect("the update applies");
        }
        d
    }

    fn apply(d: &DocHandle, update: &[u8]) {
        YrsEngine
            .apply_update(d, update)
            .expect("the update applies");
    }

    fn insertions(update: &[u8]) -> IdSet {
        Update::decode_v1(update)
            .expect("an update")
            .insertions(true)
    }

    fn within(a: &IdSet, b: &IdSet) -> bool {
        !holds_any(&a.diff(b))
    }

    /// yrs's own full state of `d`, pending content included: a client's
    /// re-send of everything it holds.
    fn resend(d: &DocHandle) -> Vec<u8> {
        d.0.transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    /// A delete-only update of `len` clocks of `client` from `clock`.
    fn deleting(client: u64, clock: u32, len: u32) -> Vec<u8> {
        let mut ds = IdSet::new();
        ds.insert(ID::new(ClientID::new(client), clock), len);
        delete_only_update(&ds)
    }

    fn one_id(client: u64, clock: u32) -> IdSet {
        let mut set = IdSet::new();
        set.insert(ID::new(ClientID::new(client), clock), 1);
        set
    }

    #[test]
    fn a_cover_keeps_the_oldest_rows_that_claim_each_pending_id() {
        let (_, u2, tail) = cross_client_parked_tail(2);
        let live = doc_of(&[&u2, &tail[0], &tail[1]]);
        let pending = claims(&live).expect("u2 and its tail park");
        let everything = resend(&live);
        // Recorded newest first: the cover still walks them in rev order.
        let mut parked = ParkedRows::default();
        parked.record(Some(4), tail[1].clone());
        parked.record(Some(3), everything.clone());
        parked.record(Some(2), tail[0].clone());
        parked.record(Some(1), u2.clone());
        assert_eq!(parked.prune(Some(&pending)), Ok(()));
        // u2 covers its own ids, rev 2 its character; the re-send at rev 3
        // covers the last character, and rev 4 adds nothing past it.
        assert_eq!(parked.revs(), vec![1, 2, 3]);
        assert_eq!(parked.frames(), vec![u2, tail[0].clone(), everything]);
    }

    /// Review Focus 1: re-sends of content that stays pending add no row.
    #[test]
    fn a_resend_of_the_pending_content_adds_no_row_to_the_cover() {
        let (_, u2) = cross_client_parked();
        let live = doc_of(&[&u2]);
        let pending = claims(&live).expect("u2 parks");
        let mut parked = ParkedRows::default();
        parked.record(Some(1), u2.clone());
        assert_eq!(parked.prune(Some(&pending)), Ok(()));
        for rev in 2..=51 {
            parked.record(Some(rev), resend(&live));
            assert_eq!(parked.prune(Some(&pending)), Ok(()));
        }
        assert_eq!(parked.revs(), vec![1]);
    }

    #[test]
    fn rows_whose_content_drained_are_dropped() {
        let (u1, u2) = cross_client_parked();
        let o = overlapping_skips_as(5);
        let live = doc_of(&[&u2, &o.t2]);
        let mut parked = ParkedRows::default();
        parked.record(Some(1), u2.clone());
        parked.record(Some(2), o.t2.clone());
        assert_eq!(parked.prune(claims(&live).as_ref()), Ok(()));
        assert_eq!(parked.revs(), vec![1, 2]);
        apply(&live, &u1);
        assert_eq!(parked.prune(claims(&live).as_ref()), Ok(()));
        assert_eq!(parked.revs(), vec![2], "u2 drained");
    }

    #[test]
    fn uncovered_pending_content_is_an_error() {
        let (_, u2) = cross_client_parked();
        let live = doc_of(&[&u2, &deleting(11, 0, 2)]);
        let pending = claims(&live).expect("u2 and the delete park");
        assert!(holds_any(&pending.deletes), "the delete parks");
        let mut parked = ParkedRows::default();
        let Err(Uncovered { missing }) = parked.prune(Some(&pending)) else {
            panic!("nothing covers the pending content");
        };
        assert_eq!(
            missing,
            PendingClaims {
                structs: insertions(&u2),
                deletes: {
                    let mut ds = IdSet::new();
                    ds.insert(ID::new(ClientID::new(11), 0), 2);
                    ds
                },
            }
        );
    }

    #[test]
    fn nothing_pending_clears_the_rows() {
        let (_, u2) = cross_client_parked();
        let mut parked = ParkedRows::default();
        parked.record(Some(1), u2);
        assert_eq!(parked.len(), 1);
        assert_eq!(parked.prune(None), Ok(()));
        assert!(parked.is_empty());
    }

    #[test]
    fn an_excluded_row_is_never_kept() {
        let (_, u2) = cross_client_parked();
        let live = doc_of(&[&u2]);
        let pending = claims(&live).expect("u2 parks");
        let mut parked = ParkedRows::default();
        parked.record(Some(1), u2.clone());
        parked.record(Some(2), resend(&live));
        parked.exclude(1);
        assert!(parked.is_excluded(1));
        assert_eq!(parked.prune(Some(&pending)), Ok(()));
        assert_eq!(parked.revs(), vec![2]);
        parked.exclude(2);
        assert!(parked.prune(Some(&pending)).is_err());
        assert!(parked.is_empty());
    }

    #[test]
    fn a_pruned_unassigned_row_does_not_take_another_rows_rev() {
        let (_, u2) = cross_client_parked();
        let pending = claims(&doc_of(&[&u2])).expect("u2 parks");
        let mut parked = ParkedRows::default();
        let a = parked.record(None, u2.clone()).expect("decodes");
        assert_eq!(parked.prune(None), Ok(()));
        let b = parked.record(None, u2.clone()).expect("decodes");
        assert_eq!(parked.prune(Some(&pending)), Ok(()));
        parked.assign(a, 5);
        assert_eq!(parked.revs_assigned(), Vec::<i64>::new());
        parked.assign(b, 6);
        assert_eq!(parked.revs(), vec![6]);
    }

    #[test]
    fn a_drain_between_hand_over_and_applied_keeps_revs_apart() {
        let (u1, u2) = cross_client_parked();
        let c_row = overlapping_skips_as(5).t2;
        let live = YrsEngine.new_doc();
        let mut parked = ParkedRows::default();
        apply(&live, &u2);
        let a = parked.record(None, u2.clone()).expect("decodes");
        assert_eq!(parked.prune(claims(&live).as_ref()), Ok(()));
        apply(&live, &u1);
        let b = parked.record(None, u1.clone()).expect("decodes");
        assert!(claims(&live).is_none(), "u1 drains u2");
        assert_eq!(parked.prune(None), Ok(()));
        apply(&live, &c_row);
        let c = parked.record(None, c_row.clone()).expect("decodes");
        assert_eq!(parked.prune(claims(&live).as_ref()), Ok(()));
        parked.assign(a, 1);
        parked.assign(b, 2);
        parked.assign(c, 3);
        assert_eq!(parked.revs(), vec![3]);
        assert_eq!(parked.frames(), vec![c_row]);
    }

    /// One id pending twice: as a parked struct (u2's first character,
    /// 12#0) and as a parked delete of it.
    fn struct_and_delete_of_the_same_id(delete_first: bool) -> (Vec<u8>, Vec<u8>, PendingClaims) {
        let (_, u2) = cross_client_parked();
        let delete = deleting(12, 0, 1);
        let order: [&[u8]; 2] = if delete_first {
            [&delete, &u2]
        } else {
            [&u2, &delete]
        };
        let pending = claims(&doc_of(&order)).expect("both park");
        let x = one_id(12, 0);
        assert!(within(&x, &pending.structs) && within(&x, &pending.deletes));
        (u2, delete, pending)
    }

    #[test]
    fn a_delete_only_row_does_not_cover_a_parked_insert() {
        let (u2, delete, pending) = struct_and_delete_of_the_same_id(true);
        let (both, _) = ParkedRows::derive([(1, delete.as_slice()), (2, u2.as_slice())], &pending);
        assert_eq!(both.revs(), vec![1, 2]);
        let (only_delete, covered) = ParkedRows::derive([(1, delete.as_slice())], &pending);
        assert_eq!(only_delete.revs(), vec![1]);
        let Err(Uncovered { missing }) = covered else {
            panic!("the parked insert is uncovered");
        };
        assert!(within(&one_id(12, 0), &missing.structs));
        assert!(!holds_any(&missing.deletes));
    }

    #[test]
    fn a_parked_insert_does_not_cover_a_parked_delete() {
        let (u2, delete, pending) = struct_and_delete_of_the_same_id(false);
        let (both, _) = ParkedRows::derive([(1, u2.as_slice()), (2, delete.as_slice())], &pending);
        assert_eq!(both.revs(), vec![1, 2]);
        let (only_insert, covered) = ParkedRows::derive([(1, u2.as_slice())], &pending);
        assert_eq!(only_insert.revs(), vec![1]);
        let Err(Uncovered { missing }) = covered else {
            panic!("the parked delete is uncovered");
        };
        assert!(within(&one_id(12, 0), &missing.deletes));
        assert!(!holds_any(&missing.structs));
    }

    #[test]
    fn a_guarded_apply_refuses_the_reproduced_underflow_shape() {
        let (state, update, _) = skip_underflow_case();
        let mut guarded = GuardedDoc::new(doc_of(&[&state]));
        assert!(holds_any(guarded.past_gap.ids()), "T2 is held past a gap");
        assert_eq!(guarded.apply(&update), Err(Refused::Underflow));
        let mut empty = GuardedDoc::new(YrsEngine.new_doc());
        assert_eq!(empty.apply(&update), Ok(()));
    }

    #[test]
    fn a_guarded_doc_holds_only_rows_whose_every_id_it_holds_or_parks() {
        let (state, update, _) = skip_underflow_case();
        let (paras, _) = skip_underflow_parts();
        let mut guarded = GuardedDoc::new(doc_of(&[&state]));
        assert!(guarded.holds(&paras));
        assert!(!guarded.holds(&update), "it names T1's 2#0..2#2");
        let delete = deleting(1, 2, 1);
        assert!(!guarded.holds(&delete));
        assert_eq!(guarded.apply(&delete), Ok(()));
        assert!(guarded.holds(&delete));

        let (_, u2) = cross_client_parked();
        let parked = GuardedDoc::new(doc_of(&[&u2]));
        assert!(parked.holds(&u2), "its ids are pending");
    }

    #[test]
    fn pending_claims_compare_as_sets() {
        let x = insertions(&overlapping_skips().t0);
        eprintln!(
            "yrs keeps empty clients after x.diff(&x): {}",
            x.diff(&x) != IdSet::new()
        );
        assert_eq!(
            PendingClaims {
                structs: x.diff(&x),
                deletes: IdSet::new(),
            },
            PendingClaims::default()
        );
        let mut y = x.clone();
        y.insert(ID::new(ClientID::new(99), 0), 1);
        let claims_of = |structs: &IdSet| PendingClaims {
            structs: structs.clone(),
            deletes: IdSet::new(),
        };
        assert_ne!(claims_of(&x), claims_of(&y));
    }

    /// Ported from knot-history's `post_apply_check_flags_held_ranges_the_
    /// update_did_not_bring` (`replay.rs:783-795`), with `held_of` reading a
    /// real doc: content pending before an apply drained into it, so it is
    /// fine with the pending content in `given` and reads as corrupt
    /// without it, and a transaction that integrated a held block again is
    /// corrupt.
    #[test]
    fn the_post_apply_check_flags_held_blocks_an_apply_brought_again() {
        let o = overlapping_skips();
        let mut guarded = GuardedDoc::new(doc_of(&[&o.t0, &o.t2, &o.t3]));
        assert!(holds_any(guarded.past_gap.ids()), "t3's paragraph");
        assert_eq!(guarded.apply(&o.t1), Ok(()));
        assert_eq!(guarded.apply(&o.t2b), Ok(()));
        let one: BTreeSet<ClientID> = [ClientID::new(1)].into();
        let (before, pending) = {
            let txn = guarded.doc().0.transact();
            (held_of(&txn, &one), pending_of(&txn).structs)
        };
        assert!(holds_any(&pending), "\"pp\" and \"qq\" stay stuck");
        // The retry drains them; the guard's own post-check passes.
        assert_eq!(guarded.retry_pending(), Ok(()));
        assert!(claims(guarded.doc()).is_none());
        let after = held_of(&guarded.doc().0.transact(), &one);
        let inserted = after.diff(&before);
        assert!(holds_only_what_it_was_given(
            &before, &after, &pending, &inserted
        ));
        assert!(!holds_only_what_it_was_given(
            &before,
            &after,
            &IdSet::new(),
            &inserted
        ));
        assert!(!holds_only_what_it_was_given(
            &before, &after, &pending, &after
        ));
    }

    #[test]
    fn held_of_names_every_block_of_the_given_clients_only() {
        let (state, _, _) = skip_underflow_case();
        let d = doc_of(&[&state, &para(3, "x")]);
        let txn = d.0.transact();
        let all = held(&txn);
        let two: BTreeSet<ClientID> = [ClientID::new(2)].into();
        let held_two = held_of(&txn, &two);
        assert!(same_ids(&held_two, &keep_clients(&all, &two)));
        assert!(
            held_two.contains(&ID::new(ClientID::new(2), 2)),
            "2#2 past the gap"
        );
        let one_two: BTreeSet<ClientID> = [ClientID::new(1), ClientID::new(2)].into();
        let three: BTreeSet<ClientID> = [ClientID::new(3)].into();
        assert!(!holds_any(&keep_clients(&held_of(&txn, &one_two), &three)));
        assert!(same_ids(
            &held_of(&txn, &one_two),
            &keep_clients(&all, &one_two)
        ));
    }

    #[test]
    fn check_reparks_accepts_the_rows_that_parked_the_content() {
        let o = overlapping_skips();
        let live = doc_of(&[&o.t0, &o.t2, &o.t3]);
        let state = encode_without_pending(&live.0.transact(), &StateVector::default());
        let pending = claims(&live).expect("pp and qq park");
        assert_eq!(
            check_reparks(&YrsEngine, &state, &[(2, &o.t2), (3, &o.t3)], &pending),
            Ok(())
        );
        let Err(Refusal::Uncovered(missing)) =
            check_reparks(&YrsEngine, &state, &[(2, &o.t2)], &pending)
        else {
            panic!("t3's parked ids are not re-parked");
        };
        assert!(holds_any(&missing.structs));
        assert!(within(&missing.structs, &insertions(&o.t3)));
    }
}
