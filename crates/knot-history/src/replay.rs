//! The replay engine: a yrs doc that applies a page's rows in rev order,
//! outside every room, and reports what each row changed (its *effect*:
//! the transaction's insert and delete sets) and what it parked.
//!
//! yrs 0.27.3 integrates a block whose own dependencies it holds even when
//! earlier blocks of its client are missing, behind a Skip ("past a gap").
//! Applying an update to such a doc is safe in general, since yrs trims
//! what the doc holds before integrating (`transaction.rs:825-827`), but
//! one shape panics in debug builds and corrupts the block list in release
//! builds (`block_store.rs:318`): see `Replayer::would_underflow`. The
//! replay refuses that shape before applying it ([`RowOutcome::Refused`]),
//! and after any apply that met blocks held past a gap it checks that it
//! holds nothing that neither the update brought nor was pending before
//! ([`RowOutcome::Corrupt`]).
//!
//! Checkpoints leave pending content out (the pending fallback, Rulings):
//! yrs 0.27.3 merges a newly parked remainder into a doc's pending update
//! without splitting the Skips, so the pending update can hold overlapping
//! Skips (`skip(11..=18), skip(13..=18)`), and its v1 encoding writes every
//! later pending block at a shifted clock (`'qq'` at 1#18 encoded at 1#23).
//! `encode_state_as_update_v1` merges exactly that encoding into a full
//! state (`merge_pending_v1`, `transaction.rs:375-391`), so a state that
//! carries pending content can name blocks the doc never had and turn
//! integrated blocks into pending ones. [`Replayer::encode_state`] writes
//! the doc's blocks and delete set only; a replay from a checkpoint first
//! re-applies the original rows of the claims open at it (its
//! `pending_revs`), which re-parks their content under its real ids.

use std::collections::BTreeMap;

use yrs::block::{
    BLOCK_GC_REF_NUMBER, BLOCK_SKIP_REF_NUMBER, ClientID, HAS_ORIGIN, HAS_PARENT_SUB,
    HAS_RIGHT_ORIGIN, ItemContent,
};
use yrs::encoding::read::{Cursor, Read};
use yrs::encoding::write::Write;
use yrs::updates::decoder::{Decode, Decoder, DecoderV1};
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{Doc, ID, IdSet, Options, ReadTxn, StateVector, Transact, Update};

use crate::pass::{Progress, ReplayHooks, Stage};

/// How the replay's doc keeps deleted content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayMode {
    /// `Doc::new()`: deleted content is garbage collected, as in a room.
    Gc,
    /// `skip_gc`: deleted content stays, so a diff can show what was removed.
    KeepDeleted,
}

impl ReplayMode {
    fn new_doc(self) -> Doc {
        match self {
            ReplayMode::Gc => Doc::new(),
            ReplayMode::KeepDeleted => Doc::with_options(Options {
                skip_gc: true,
                ..Default::default()
            }),
        }
    }
}

/// How an apply ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowOutcome {
    /// yrs applied the update (also when it only parked it).
    Applied,
    /// yrs rejected the update part-way; what integrated before the error
    /// is in the effect.
    ApplyError(String),
    /// The bytes are no v1 update; nothing changed.
    Undecodable(String),
    /// The update has the shape that reaches yrs 0.27.3's skip underflow;
    /// it was not applied and nothing changed.
    Refused,
    /// After the apply the replay holds blocks that neither the update
    /// brought nor were pending before it: its block list is corrupt, and a `Replayer` that returned this from
    /// [`Replayer::apply`] or [`Replayer::merge_state`] must not be used
    /// again.
    Corrupt,
}

impl RowOutcome {
    /// The `reason` label of `knot_history_rows_unapplied_total` for a row
    /// with this outcome; `None` for one that applied, and for `Corrupt`,
    /// which stops the pass with an error instead.
    pub fn metric_reason(&self) -> Option<&'static str> {
        match self {
            RowOutcome::Applied | RowOutcome::Corrupt => None,
            RowOutcome::ApplyError(_) => Some("apply_error"),
            RowOutcome::Undecodable(_) => Some("undecodable"),
            RowOutcome::Refused => Some("skip_underflow"),
        }
    }
}

/// What one apply did to the replay.
#[derive(Debug, Clone, PartialEq)]
pub struct RowEffect {
    pub outcome: RowOutcome,
    /// Blocks the apply integrated (the transaction's insert set), parked
    /// content it drained included.
    pub inserted: IdSet,
    /// Items the apply deleted (the transaction's delete set).
    pub deleted: IdSet,
    /// Blocks the apply newly parked as pending.
    pub parked_inserts: IdSet,
    /// Deletions of items the replay lacks, newly parked as pending.
    pub parked_deletes: IdSet,
}

impl RowEffect {
    fn nothing(outcome: RowOutcome) -> Self {
        RowEffect {
            outcome,
            inserted: IdSet::new(),
            deleted: IdSet::new(),
            parked_inserts: IdSet::new(),
            parked_deletes: IdSet::new(),
        }
    }

    /// Whether the apply integrated and deleted nothing (it may still have
    /// parked something).
    pub fn is_empty(&self) -> bool {
        self.inserted.is_empty() && self.deleted.is_empty()
    }
}

/// Why a replay stopped.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The starting checkpoint does not decode or apply.
    #[error("the starting state does not apply: {0}")]
    State(String),
    /// A row at or below `built_rev` that the replay prefix re-applies is
    /// `Corrupt` or `Refused`, or whose parked content's retry corrupts the
    /// replay at any point of the pass: the checkpoint is blamed, never the
    /// row.
    #[error("row {rev} of the replay prefix failed")]
    Prefix { rev: i64 },
    /// A new row corrupted the replay, on its apply or on a retry of its
    /// parked content (whatever input's apply triggered that retry).
    #[error("row {rev} corrupted the replay")]
    Corrupt { rev: i64 },
    /// A gap merge of a snapshot corrupted the replay.
    #[error("the gap merge of the snapshot at rev {snapshot_rev} corrupted the replay")]
    CorruptSnapshot { snapshot_rev: i64 },
    /// A gap state kept across a rebuild that `merge_gap_state` does not
    /// take, or that corrupts the replay.
    #[error("the gap state at rev {rev} does not merge")]
    KeptGap { rev: i64 },
    /// The builder cancelled the timed-out compute; its result is discarded.
    #[error("the replay was cancelled")]
    Cancelled,
}

/// A replay of one page.
pub struct Replayer {
    mode: ReplayMode,
    doc: Doc,
    /// Blocks the doc holds beyond its state-vector clock (behind a Skip).
    /// Each range starts where a Skip ends.
    past_gap: IdSet,
}

impl Replayer {
    pub fn new(mode: ReplayMode) -> Replayer {
        Replayer {
            mode,
            doc: mode.new_doc(),
            past_gap: IdSet::new(),
        }
    }

    /// A replay that starts from `state` (a checkpoint: the blocks and the
    /// delete set, no pending content; the caller re-applies the rows of
    /// the checkpoint's `pending_revs`).
    pub fn from_state(mode: ReplayMode, state: &[u8]) -> Result<Replayer, ReplayError> {
        let update = Update::decode_v1(state).map_err(|e| ReplayError::State(e.to_string()))?;
        let doc = mode.new_doc();
        doc.transact_mut()
            .apply_update(update)
            .map_err(|e| ReplayError::State(e.to_string()))?;
        let past_gap = {
            let txn = doc.transact();
            let held = held(&txn)
                .ok_or_else(|| ReplayError::State("the state does not encode again".to_string()))?;
            at_or_above(&held, &txn.state_vector())
        };
        Ok(Replayer {
            mode,
            doc,
            past_gap,
        })
    }

    /// Applies one row.
    pub fn apply(&mut self, update: &[u8]) -> RowEffect {
        match Update::decode_v1(update) {
            Ok(u) => self.apply_decoded(u),
            Err(e) => RowEffect::nothing(RowOutcome::Undecodable(e.to_string())),
        }
    }

    /// Applies a full state (a gap state): the same as [`Replayer::apply`].
    pub fn merge_state(&mut self, state: &[u8]) -> RowEffect {
        self.apply(state)
    }

    /// What applying `state` would do, on a copy made from
    /// [`Replayer::encode_state`] (so without the replay's pending content);
    /// the replay itself is left untouched.
    pub fn probe(&self, state: &[u8]) -> RowEffect {
        let update = match Update::decode_v1(state) {
            Ok(u) => u,
            Err(e) => return RowEffect::nothing(RowOutcome::Undecodable(e.to_string())),
        };
        if self.would_underflow(&update) {
            return RowEffect::nothing(RowOutcome::Refused);
        }
        match Replayer::from_state(self.mode, &self.encode_state()) {
            Ok(mut copy) => copy.apply_decoded(update),
            // The replay's own state does not decode again.
            Err(_) => RowEffect::nothing(RowOutcome::Corrupt),
        }
    }

    /// Takes `state` when [`Replayer::probe`] refuses it: decodes `state`
    /// into a fresh doc of the same mode (which holds no blocks past a gap)
    /// and applies the replay's state to it, with the same pre-validation
    /// and post-check as [`Replayer::apply`]. When that applies (also with
    /// an `ApplyError`) the fresh doc replaces the replay, and the effect is
    /// what it holds that the replay did not: held insertions, delete set
    /// and pending content, each after − before. Any other outcome leaves
    /// the replay untouched. The replay's own pending content does not
    /// survive (its state carries none): the caller re-applies the rows of
    /// the claims still open to park it again.
    pub fn union(&mut self, state: &[u8]) -> RowEffect {
        let mut fresh = match Replayer::from_state(self.mode, state) {
            Ok(fresh) => fresh,
            Err(e) => return RowEffect::nothing(RowOutcome::Undecodable(e.to_string())),
        };
        let own = match Update::decode_v1(&self.encode_state()) {
            Ok(u) => u,
            Err(_) => return RowEffect::nothing(RowOutcome::Corrupt),
        };
        let outcome = fresh.apply_decoded(own).outcome;
        if !matches!(outcome, RowOutcome::Applied | RowOutcome::ApplyError(_)) {
            return RowEffect::nothing(outcome);
        }
        let (before, after) = (self.doc.transact(), fresh.doc.transact());
        let (Some(held_before), Some(held_after)) = (held(&before), held(&after)) else {
            return RowEffect::nothing(RowOutcome::Corrupt);
        };
        let past_gap = at_or_above(&held_after, &after.state_vector());
        let (pending_before, pending_after) = (Pending::of(&before), Pending::of(&after));
        let effect = RowEffect {
            outcome,
            inserted: held_after.diff(&held_before),
            deleted: after
                .snapshot()
                .delete_set
                .diff(&before.snapshot().delete_set),
            parked_inserts: pending_after.inserts.diff(&pending_before.inserts),
            parked_deletes: pending_after.deletes.diff(&pending_before.deletes),
        };
        drop((before, after));
        fresh.past_gap = past_gap;
        *self = fresh;
        effect
    }

    /// What a replay that passes a gap checkpoint's rev applies:
    /// [`Replayer::merge_state`], and [`Replayer::union`] when that is
    /// `Refused`.
    pub fn merge_gap_state(&mut self, state: &[u8]) -> RowEffect {
        let effect = self.merge_state(state);
        if effect.outcome == RowOutcome::Refused {
            return self.union(state);
        }
        effect
    }

    /// Applies `rows` again (the original bytes of rows whose content was
    /// parked, in rev order) and adds what they integrate now to `effect`.
    ///
    /// yrs 0.27.3 retries its pending structs only when a missing clock it
    /// recorded falls below that client's clock (`transaction.rs:836-843`),
    /// and the structs it keeps pending can no longer integrate in some
    /// cases where the row's own bytes do; so whether parked content
    /// integrates once everything it needs is there would depend on how the
    /// replay got there (a replay resumed from a checkpoint re-parks the
    /// rows afresh). Every replay retries its parked rows through
    /// `Parked` after each apply that integrated something, re-applying
    /// those that gained something they wait for: parked content then
    /// integrates at the first apply after which it can, whatever the
    /// replay's past. Already-held blocks are trimmed, so a row that drained
    /// long ago changes nothing. `false` when one of them corrupts the
    /// replay, which must not be used again.
    pub fn retry<'a>(
        &mut self,
        rows: impl IntoIterator<Item = &'a [u8]>,
        effect: &mut RowEffect,
    ) -> bool {
        for bytes in rows {
            let e = self.apply(bytes);
            if e.outcome == RowOutcome::Corrupt {
                return false;
            }
            effect.inserted.merge_with(e.inserted);
            effect.deleted.merge_with(e.deleted);
        }
        true
    }

    /// The replay's state: its blocks and delete set, without pending
    /// content (module docs: the pending fallback).
    pub fn encode_state(&self) -> Vec<u8> {
        own_state(&self.doc.transact())
    }

    /// The replay's state vector, v1-encoded in client order (yrs's own
    /// `encode_v1` follows a hash map's order, so equal state vectors could
    /// differ as bytes).
    pub fn state_vector(&self) -> Vec<u8> {
        let sv = self.doc.transact().state_vector();
        let mut pairs: Vec<(u64, u32)> = sv.iter().map(|(c, clock)| (c.get(), *clock)).collect();
        pairs.sort_unstable();
        let mut encoder = EncoderV1::new();
        encoder.write_var(pairs.len());
        for (client, clock) in pairs {
            encoder.write_var(client);
            encoder.write_var(clock);
        }
        encoder.to_vec()
    }

    pub fn doc(&self) -> &Doc {
        &self.doc
    }

    /// Whether the replay holds parked structs or parked deletions.
    pub fn has_pending(&self) -> bool {
        let txn = self.doc.transact();
        let store = txn.store();
        store.pending_update().is_some() || store.pending_ds().is_some()
    }

    fn apply_decoded(&mut self, update: Update) -> RowEffect {
        if self.would_underflow(&update) {
            return RowEffect::nothing(RowOutcome::Refused);
        }
        let brought = update.insertions(true);
        // The post-apply check runs only when the replay holds blocks past
        // a gap of a client the update has blocks of: no other apply can
        // reach the underflow.
        let check = brought
            .client_ids()
            .any(|client| self.past_gap.get(&client).is_some());
        let held_before = if check {
            match held(&self.doc.transact()) {
                Some(held) => Some(held),
                None => return RowEffect::nothing(RowOutcome::Corrupt),
            }
        } else {
            None
        };

        let mut txn = self.doc.transact_mut();
        let pending_before = Pending::of(&txn);
        let result = txn.apply_update(update);
        let inserted = txn.insert_set().clone();
        let deleted = txn.delete_set().clone();
        let pending_after = Pending::of(&txn);
        txn.commit();
        let sv = txn.state_vector();
        drop(txn);

        self.past_gap.merge_with(inserted.clone());
        self.past_gap = at_or_above(&self.past_gap, &sv);
        let mut outcome = match result {
            Ok(()) => RowOutcome::Applied,
            Err(e) => RowOutcome::ApplyError(e.to_string()),
        };
        if let Some(held_before) = held_before {
            let txn = self.doc.transact();
            let ok = held(&txn).is_some_and(|held_after| {
                let given = brought.merge(&pending_before.inserts);
                holds_only_what_it_was_given(&held_before, &held_after, &given, &inserted)
            });
            if !ok {
                outcome = RowOutcome::Corrupt;
            }
        }
        RowEffect {
            outcome,
            inserted,
            deleted,
            parked_inserts: pending_after.inserts.diff(&pending_before.inserts),
            parked_deletes: pending_after.deletes.diff(&pending_before.deletes),
        }
    }

    /// Whether applying `update` would reach yrs 0.27.3's skip underflow
    /// (`block_store.rs:318`, `skip.next_clock() - block.next_clock()`).
    ///
    /// yrs trims the blocks a doc holds out of an update before integrating
    /// it (`BlockSet::exclude`, `update.rs:76-115`): for each range the doc
    /// holds it splits the update's block at the range's start and replaces
    /// what the range covers with a Skip. Splitting an item works, but
    /// splitting a GC block (`Block::splice`, `block.rs:355-360`) returns its
    /// right half and leaves the left one at full length. When a held range
    /// starts inside a GC block of the update, the whole block is then
    /// integrated into the doc's Skip that ends where the held range starts,
    /// and the Skip's remainder after the block is negative. A held range
    /// starts after clock 0 only where a Skip of the doc ends, which is
    /// where a range of `past_gap` starts. So: some GC block of the update
    /// starts before the start of a `past_gap` range of its client and ends
    /// after it. `tests/common/underflow.rs` holds the reproducer: a GC
    /// block 2#0..2#3 over the doc's Skip 2#0..2#2 and its held 2#2.
    ///
    /// yrs does not expose an update's blocks; a decoded update's items are
    /// never marked deleted (yrs marks them on integration), so its GC
    /// blocks are `insertions(true) − insertions(false)`. Adjacent GC blocks
    /// read as one range there, so two that meet exactly at a Skip's end
    /// are refused although yrs would split them correctly; yrs's encoder
    /// squashes adjacent GC blocks, so such an update is not expected.
    fn would_underflow(&self, update: &Update) -> bool {
        if self.past_gap.is_empty() {
            return false;
        }
        let gc = update.insertions(true).diff(&update.insertions(false));
        gc_block_straddles_a_skip_end(&gc, &self.past_gap)
    }
}

/// Rows whose content may still be pending, each retried from its original
/// bytes ([`Replayer::retry`]) only once something it waits for arrived.
///
/// Re-applying a row can integrate something only when the replay newly
/// holds an id the row's integration depends on: yrs trims the blocks a doc
/// holds, integrates a block once it holds the block's origin, right origin
/// and parent (`Update::missing_dependency`, `update.rs:381-417`; a block
/// behind a missing one of its client is integrated behind a Skip), and
/// applies a deletion once it holds the deleted id. A row's *triggers* are
/// those ids: its own blocks, every origin, right origin and parent id its
/// items name, and its delete set. Every apply's insert set is checked
/// against them ([`Parked::mark`]); a row none of whose triggers the replay
/// gained since its last apply would integrate nothing, so it is not
/// re-applied. A row parked for good (a resend of the whole page that waits
/// for content that never comes) then costs a set intersection per apply,
/// not a decode and trim of its bytes.
///
/// A re-apply that panics, hangs or corrupts the replay is the retried
/// row's own failure, never the input whose apply triggered the retry: a
/// watched `Parked` ([`Parked::watch`]) publishes the retried row's stage
/// while it re-applies it, and a failed drain names that row.
pub(crate) struct Parked<'a> {
    rows: BTreeMap<i64, Waiting<'a>>,
    /// Test hook: retry every row in every round (the gate off).
    every: bool,
    /// Test hooks: panic in, or report `Corrupt` from, that row's re-apply.
    panic_at: Option<i64>,
    corrupt_at: Option<i64>,
    /// Where each re-apply's stage is published: `Stage::Row(rev)` for a
    /// row above the rev, `Stage::Prefix(rev)` for one at or below it.
    watch: Option<(Progress, i64)>,
    /// Rows re-applied by [`Parked::retry`].
    pub(crate) retried: u64,
}

struct Waiting<'a> {
    bytes: &'a [u8],
    /// `None`: the bytes do not decode as blocks (never seen for a row
    /// that parked); such a row is retried in every round.
    triggers: Option<IdSet>,
    /// The replay gained a trigger since the row's last apply.
    dirty: bool,
}

impl<'a> Parked<'a> {
    pub(crate) fn new(hooks: &ReplayHooks) -> Self {
        Parked {
            rows: BTreeMap::new(),
            every: hooks.retry_every_row,
            panic_at: hooks.panic_at_retry,
            corrupt_at: hooks.corrupt_at_retry,
            watch: None,
            retried: 0,
        }
    }

    /// Publishes each re-apply's stage to `progress`: `Stage::Row(rev)`
    /// for a row after `built_rev` (new in this replay), `Stage::Prefix(rev)`
    /// for one at or below it. The stage current before a drain is set
    /// again after it.
    pub(crate) fn watch(mut self, progress: Progress, built_rev: i64) -> Self {
        self.watch = Some((progress, built_rev));
        self
    }

    /// Adds the row `rev` that just parked content. It is retried at the
    /// next round once whatever it waits for, as every row was before the
    /// gate: one extra apply per parking row.
    pub(crate) fn add(&mut self, rev: i64, bytes: &'a [u8]) {
        self.rows.entry(rev).or_insert_with(|| Waiting {
            bytes,
            triggers: retry_triggers(bytes),
            dirty: true,
        });
    }

    /// Keeps only the rows `keep` names (the open claims' rows).
    pub(crate) fn retain(&mut self, keep: &[i64]) {
        self.rows.retain(|rev, _| keep.binary_search(rev).is_ok());
    }

    /// Records that the replay gained `inserted`.
    pub(crate) fn mark(&mut self, inserted: &IdSet) {
        if inserted.is_empty() {
            return;
        }
        for w in self.rows.values_mut().filter(|w| !w.dirty) {
            w.dirty = w
                .triggers
                .as_ref()
                .is_none_or(|t| !t.intersect(inserted).is_empty());
        }
    }

    /// Retries the rows the replay gained a trigger of, in rev order, until
    /// none is left (a retry that integrates something marks the others
    /// again), adding what they integrate to `effect`. `Err(rev)` when the
    /// re-apply of row `rev` corrupts the replay, which must not be used
    /// again.
    pub(crate) fn retry(
        &mut self,
        replayer: &mut Replayer,
        effect: &mut RowEffect,
    ) -> Result<(), i64> {
        if self.every {
            self.rows.values_mut().for_each(|w| w.dirty = true);
        }
        self.drain(replayer, effect)
    }

    /// Retries every row: after a `union`, which dropped the replay's
    /// pending content, this parks it again.
    pub(crate) fn retry_all(
        &mut self,
        replayer: &mut Replayer,
        effect: &mut RowEffect,
    ) -> Result<(), i64> {
        self.rows.values_mut().for_each(|w| w.dirty = true);
        self.drain(replayer, effect)
    }

    fn drain(&mut self, replayer: &mut Replayer, effect: &mut RowEffect) -> Result<(), i64> {
        let before = self.watch.as_ref().map(|(p, _)| p.get());
        while let Some(rev) = self.rows.iter().find(|(_, w)| w.dirty).map(|(r, _)| *r) {
            let w = self.rows.get_mut(&rev).expect("a dirty row");
            w.dirty = false;
            let bytes = w.bytes;
            if let Some((progress, built_rev)) = &self.watch {
                progress.set(if rev > *built_rev {
                    Stage::Row(rev)
                } else {
                    Stage::Prefix(rev)
                });
            }
            if self.panic_at == Some(rev) {
                panic!("test hook: panic re-applying row {rev}");
            }
            let mut e = RowEffect::nothing(RowOutcome::Applied);
            if !replayer.retry([bytes], &mut e) || self.corrupt_at == Some(rev) {
                return Err(rev);
            }
            self.retried += 1;
            self.mark(&e.inserted);
            effect.inserted.merge_with(e.inserted);
            effect.deleted.merge_with(e.deleted);
        }
        if let (Some((progress, _)), Some(stage)) = (&self.watch, before) {
            progress.set(stage);
        }
        Ok(())
    }
}

/// What a re-apply of `update` depends on ([`Parked`]): the ids of its
/// blocks, the origin, right origin and parent ids its items name, and its
/// delete set. `None` when the bytes do not decode.
fn retry_triggers(update: &[u8]) -> Option<IdSet> {
    let u = Update::decode_v1(update).ok()?;
    let mut triggers = u.insertions(true);
    triggers.merge_with(u.delete_set().clone());
    // yrs does not expose an update's blocks: walk them as
    // `Update::decode_block` (`update.rs:544-597`) reads them.
    let mut d = DecoderV1::new(Cursor::new(update));
    let clients: u32 = d.read_var().ok()?;
    for _ in 0..clients {
        let blocks: u32 = d.read_var().ok()?;
        d.read_client().ok()?;
        let _clock: u32 = d.read_var().ok()?;
        for _ in 0..blocks {
            match d.read_info().ok()? {
                BLOCK_SKIP_REF_NUMBER => {
                    let _len: u32 = d.read_var().ok()?;
                }
                BLOCK_GC_REF_NUMBER => {
                    d.read_len().ok()?;
                }
                info => {
                    let cant_copy_parent_info = info & (HAS_ORIGIN | HAS_RIGHT_ORIGIN) == 0;
                    if info & HAS_ORIGIN != 0 {
                        triggers.insert(d.read_left_id().ok()?, 1);
                    }
                    if info & HAS_RIGHT_ORIGIN != 0 {
                        triggers.insert(d.read_right_id().ok()?, 1);
                    }
                    if cant_copy_parent_info {
                        if d.read_parent_info().ok()? {
                            d.read_string().ok()?;
                        } else {
                            triggers.insert(d.read_left_id().ok()?, 1);
                        }
                        if info & HAS_PARENT_SUB != 0 {
                            d.read_string().ok()?;
                        }
                    }
                    ItemContent::decode(&mut d, info).ok()?;
                }
            }
        }
    }
    Some(triggers)
}

/// Whether a range of `gc` starts before and ends after the start of a
/// range of `past_gap` of the same client.
fn gc_block_straddles_a_skip_end(gc: &IdSet, past_gap: &IdSet) -> bool {
    let skip_ends: BTreeMap<_, Vec<u32>> = past_gap
        .iter()
        .map(|(client, ranges)| (*client, ranges.iter().map(|r| r.start).collect()))
        .collect();
    gc.iter().any(|(client, ranges)| {
        skip_ends.get(client).is_some_and(|ends| {
            ranges
                .iter()
                .any(|r| ends.iter().any(|&end| r.start < end && end < r.end))
        })
    })
}

/// The post-apply check: after an apply to a replay that held blocks past a
/// gap, the transaction integrated nothing the replay already held (yrs
/// trims what a doc holds before integrating, so a correct apply never
/// does), the replay holds only what it held before and what it was
/// `given`, and no client's highest held clock is past both. `given` is
/// what the update brought and what was pending before the apply: an apply
/// may drain parked content the update does not itself carry. A release
/// build wraps the underflow instead of panicking: on the reproducer it
/// integrates the whole GC block over the held 2#2 and inserts a wrapped
/// Skip after it, which only the first condition sees.
fn holds_only_what_it_was_given(
    held_before: &IdSet,
    held_after: &IdSet,
    given: &IdSet,
    inserted: &IdSet,
) -> bool {
    if !inserted.intersect(held_before).is_empty() {
        return false;
    }
    let allowed = held_before.merge(given);
    if !held_after.diff(&allowed).is_empty() {
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

/// A doc's blocks and delete set as a v1 update, without its pending
/// content (`encode_state_as_update` with an encoder skips
/// `merge_pending_v1`; module docs).
fn own_state<T: ReadTxn>(txn: &T) -> Vec<u8> {
    let mut encoder = EncoderV1::new();
    txn.encode_state_as_update(&StateVector::default(), &mut encoder);
    encoder.to_vec()
}

/// The blocks a doc holds, deleted ones included (its pending structs are
/// not in the block store). `None` when the doc's own state does not decode
/// again.
fn held<T: ReadTxn>(txn: &T) -> Option<IdSet> {
    Some(Update::decode_v1(&own_state(txn)).ok()?.insertions(true))
}

/// The parts of `set` at or above each client's clock in `sv`.
fn at_or_above(set: &IdSet, sv: &StateVector) -> IdSet {
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

/// A doc's pending structs and pending deletions.
struct Pending {
    inserts: IdSet,
    deletes: IdSet,
}

impl Pending {
    fn of<T: ReadTxn>(txn: &T) -> Pending {
        let store = txn.store();
        Pending {
            inserts: store
                .pending_update()
                .map(|p| p.update.insertions(true))
                .unwrap_or_default(),
            deletes: store.pending_ds().cloned().unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ranges: &[(u64, u32, u32)]) -> IdSet {
        let mut s = IdSet::new();
        for &(client, start, end) in ranges {
            s.insert(ID::new(ClientID::new(client), start), end - start);
        }
        s
    }

    #[test]
    fn post_apply_check_flags_held_ranges_the_update_did_not_bring() {
        let before = set(&[(1, 0, 5), (2, 0, 2), (2, 4, 6)]);
        let brought = set(&[(2, 2, 4)]);
        let check = |after: &IdSet, inserted: &IdSet| {
            holds_only_what_it_was_given(&before, after, &brought, inserted)
        };
        let filled = set(&[(1, 0, 5), (2, 0, 6)]);
        // The gap filled: fine.
        assert!(check(&filled, &brought));
        // Nothing changed: fine.
        assert!(check(&before, &IdSet::new()));
        // A range neither held before nor brought: corrupt.
        assert!(!check(&set(&[(1, 0, 5), (2, 0, 7)]), &brought));
        // Another client's range grew: corrupt.
        assert!(!check(&set(&[(1, 0, 9), (2, 0, 6)]), &brought));
        // A client nobody mentioned: corrupt.
        assert!(!check(&set(&[(1, 0, 5), (2, 0, 6), (3, 0, 1)]), &brought));
        // The transaction integrated a block the replay held (the wrapped
        // underflow): corrupt, although what it holds looks right.
        assert!(!check(&filled, &set(&[(2, 2, 5)])));

        // Content pending before the apply drained into it: the caller
        // passes it in `given` with what the update brought, so a client
        // the update does not name may grow, up to what was pending.
        let pending = set(&[(3, 0, 2)]);
        let given = brought.merge(&pending);
        let drained = set(&[(1, 0, 5), (2, 0, 6), (3, 0, 2)]);
        let inserted = set(&[(2, 2, 4), (3, 0, 2)]);
        assert!(holds_only_what_it_was_given(
            &before, &drained, &given, &inserted
        ));
        // Without the pending content it reads as corrupt.
        assert!(!check(&drained, &inserted));
        // Past what was pending: still corrupt.
        assert!(!holds_only_what_it_was_given(
            &before,
            &set(&[(1, 0, 5), (2, 0, 6), (3, 0, 3)]),
            &given,
            &inserted
        ));
    }

    #[test]
    fn a_gc_block_over_a_skip_end_is_the_underflow_shape() {
        let past_gap = set(&[(2, 2, 3)]);
        assert!(gc_block_straddles_a_skip_end(&set(&[(2, 0, 3)]), &past_gap));
        assert!(gc_block_straddles_a_skip_end(&set(&[(2, 1, 5)]), &past_gap));
        // Ending at the Skip's end, or starting there: trimmed correctly.
        assert!(!gc_block_straddles_a_skip_end(
            &set(&[(2, 0, 2)]),
            &past_gap
        ));
        assert!(!gc_block_straddles_a_skip_end(
            &set(&[(2, 2, 5)]),
            &past_gap
        ));
        // Another client's GC block.
        assert!(!gc_block_straddles_a_skip_end(
            &set(&[(1, 0, 9)]),
            &past_gap
        ));
    }
}
