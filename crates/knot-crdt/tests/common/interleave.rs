//! The interleaving generator of `tests/effective_update.rs` (page history
//! M1.2): three editors whose transactions reach one live doc as live
//! deliveries, reconnect resends and late, out-of-order deliveries that
//! park pending content or open gaps; and the pending-free reads of a doc
//! that the pending-encoding tests (`tests/pending_state.rs`) compare with.
//! The one copy of each: tests include this file by path
//! (`#[path = "common/interleave.rs"] mod interleave;`) next to
//! `common/fixtures.rs`, which they must include as `mod fixtures` at their
//! root, since this file uses `crate::fixtures`.
//!
//! Every helper that reads a doc takes `&yrs::Doc`, so a caller holding a
//! `DocHandle` passes `handle.inner()`; only the generator's own guard
//! [`has_gaps`] keeps the `DocHandle` it was written for.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use knot_crdt::DocHandle;
use knot_crdt::pending::{at_or_above, encode_without_pending, holds_any};
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Doc, ID, IdSet, ReadTxn, StateVector, Subscription, Text, Transact, Update,
    XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
};

use crate::fixtures::Rng;

/// One editor: a yrs doc with its own client id, the updates of its own
/// transactions that it has not delivered yet, and the updates it holds back
/// to deliver late.
pub struct Client {
    pub doc: Doc,
    pub frag: XmlFragmentRef,
    /// Each local transaction's own update (`observe_update_v1`), in order,
    /// since the client's last delivery.
    undelivered: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Off while the client applies what it pulls from the server, so only
    /// its own transactions are recorded.
    recording: Arc<AtomicBool>,
    pub held: Vec<Vec<u8>>,
    _sub: Subscription,
}

impl Client {
    pub fn new(id: u64) -> Self {
        let doc = Doc::with_client_id(id);
        let frag = doc.get_or_insert_xml_fragment("default");
        let undelivered = Arc::new(Mutex::new(Vec::new()));
        let recording = Arc::new(AtomicBool::new(true));
        let sub = {
            let undelivered = undelivered.clone();
            let recording = recording.clone();
            doc.observe_update_v1(move |_, e| {
                if recording.load(Ordering::SeqCst) {
                    undelivered.lock().unwrap().push(e.update.clone());
                }
            })
            .expect("observe_update_v1")
        };
        Client {
            doc,
            frag,
            undelivered,
            recording,
            held: Vec::new(),
            _sub: sub,
        }
    }

    pub fn take_undelivered(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.undelivered.lock().unwrap())
    }

    /// Everything this client has not delivered, left in place: its
    /// held-back updates, then its own transactions since its last delivery
    /// (a held-back update can depend on them).
    pub fn outstanding(&self) -> Vec<Vec<u8>> {
        let mut out = self.held.clone();
        out.extend(self.undelivered.lock().unwrap().iter().cloned());
        out
    }

    /// Apply what the server holds that this client lacks, as a client does
    /// on sync, without recording it as the client's own. `encode` is the
    /// server's state encoding against the client's state vector: yrs's
    /// `encode_state_as_update_v1` for what rooms sent before the fix, or
    /// `pending::encode_without_pending` for what fixed rooms send.
    pub fn pull(&self, live: &DocHandle, encode: fn(&DocHandle, &StateVector) -> Vec<u8>) {
        let sv = self.doc.transact().state_vector();
        let diff = encode(live, &sv);
        self.recording.store(false, Ordering::SeqCst);
        self.doc
            .transact_mut()
            .apply_update(Update::decode_v1(&diff).expect("decode pull"))
            .expect("apply pull");
        self.recording.store(true, Ordering::SeqCst);
    }

    /// The text of the paragraph at `i`, if it is a paragraph with one.
    pub fn text_of(&self, txn: &impl ReadTxn, i: u32) -> Option<XmlTextRef> {
        let Some(XmlOut::Element(p)) = self.frag.get(txn, i) else {
            return None;
        };
        match p.get(txn, 0) {
            Some(XmlOut::Text(t)) => Some(t),
            _ => None,
        }
    }

    /// One local edit, in its own transaction: insert a paragraph, insert or
    /// delete text inside one, format a range, or remove a paragraph.
    pub fn edit(&self, rng: &mut Rng) {
        let mut txn = self.doc.transact_mut();
        let paragraphs = self.frag.len(&txn);
        if paragraphs == 0 || rng.chance(25) {
            let at = rng.below(paragraphs as usize + 1) as u32;
            let p = self
                .frag
                .insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(rng.word()));
            return;
        }
        let i = rng.below(paragraphs as usize) as u32;
        let Some(text) = self.text_of(&txn, i) else {
            self.frag.remove_range(&mut txn, i, 1);
            return;
        };
        let len = text.len(&txn);
        match rng.below(10) {
            0..=3 => {
                let at = rng.below(len as usize + 1) as u32;
                text.insert(&mut txn, at, &rng.word());
            }
            4..=6 if len > 0 => {
                let at = rng.below(len as usize) as u32;
                let n = 1 + rng.below((len - at) as usize) as u32;
                text.remove_range(&mut txn, at, n);
            }
            7..=8 if len > 0 => {
                let at = rng.below(len as usize) as u32;
                let n = 1 + rng.below((len - at) as usize) as u32;
                let mark = ["bold", "italic", "code"][rng.below(3)];
                let value = if rng.chance(70) {
                    Any::Bool(true)
                } else {
                    Any::Null
                };
                let attrs: Attrs = [(mark.into(), value)].into_iter().collect();
                text.format(&mut txn, at, n, attrs);
            }
            _ => self.frag.remove_range(&mut txn, i, 1),
        }
    }
}

/// Whether `d` holds blocks past a gap in their client's clock. yrs 0.27
/// integrates a block whose own dependencies it has even when earlier
/// blocks of its client are missing, behind a Skip, and its state vector
/// stops at the gap. Such a doc's state diff from that clock re-sends the
/// blocks past the gap, and yrs 0.27 panics applying a block that runs from
/// inside a gap into blocks the doc already holds (`block_store.rs:318`),
/// whether the doc is a client pulling from the server or the server taking
/// a client's reconnect resend. The generator does neither while the server
/// holds a gap: that panic is yrs's, on the raw update as much as on the
/// effective one, so it says nothing about the property.
pub fn has_gaps(d: &DocHandle) -> bool {
    let txn = d.inner().transact();
    let sv = txn.state_vector();
    // Blocks past the state vector, less the pending structs, which the
    // full state also re-encodes but which are not in the block store.
    let full = txn.encode_state_as_update_v1(&StateVector::default());
    let held = Update::decode_v1(&full)
        .expect("decode full state")
        .insertions(true);
    let mut past = IdSet::new();
    for (client, ranges) in held.iter() {
        let clock = sv.get(client);
        for r in ranges.iter().filter(|r| r.end > clock) {
            let start = r.start.max(clock);
            past.insert(ID::new(*client, start), r.end - start);
        }
    }
    if let Some(p) = txn.store().pending_update() {
        past.diff_with(&p.update.insertions(true));
    }
    past.iter().any(|(_, ranges)| !ranges.is_empty())
}

/// The fixed delete-before-insert step: `c` creates a paragraph with text
/// (U1) and deletes part of that text (U2). U2's update carries only a
/// delete set over items the server lacks, so a server that gets U2 alone
/// first parks a pending delete set and no structs. U1 is pushed into
/// `c.held`, to arrive later; U2 is returned for the caller to deliver.
pub fn delete_before_insert(c: &mut Client) -> Vec<u8> {
    {
        let mut txn = c.doc.transact_mut();
        let at = c.frag.len(&txn);
        let p = c
            .frag
            .insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("abcdef"));
    }
    {
        let mut txn = c.doc.transact_mut();
        let at = c.frag.len(&txn) - 1;
        let text = c.text_of(&txn, at).expect("the paragraph just made");
        text.remove_range(&mut txn, 1, 3);
    }
    let mut own = c.undelivered.lock().unwrap();
    let u2 = own.pop().expect("U2 recorded");
    let u1 = own.pop().expect("U1 recorded");
    drop(own);
    c.held.push(u1);
    u2
}

/// One delivery from `c`, chosen at random: its incremental update since
/// its last delivery (merged, or each transaction's own update in turn,
/// possibly holding one transaction or the whole increment back), a
/// reconnect resend against `live`'s state vector, or one of its held-back
/// updates, late.
pub fn deliveries(c: &mut Client, live: &DocHandle, rng: &mut Rng) -> Vec<Vec<u8>> {
    let pick = rng.below(100);
    if pick < 20 && !c.held.is_empty() {
        let i = rng.below(c.held.len());
        return vec![c.held.remove(i)];
    }
    if pick < 45 && !has_gaps(live) {
        // Reconnect: SyncStep2 against the server's state vector carries
        // everything the server lacks plus the client's whole delete set.
        c.take_undelivered();
        let sv = live.inner().transact().state_vector();
        return vec![c.doc.transact().encode_state_as_update_v1(&sv)];
    }
    let mut own = c.take_undelivered();
    if own.is_empty() {
        return Vec::new();
    }
    if own.len() > 1 && rng.chance(20) {
        let i = rng.below(own.len());
        c.held.push(own.remove(i));
    }
    if rng.chance(50) {
        return own;
    }
    let merged = yrs::merge_updates_v1(&own).expect("merge own updates");
    if rng.chance(10) {
        c.held.push(merged);
        return Vec::new();
    }
    vec![merged]
}

/// The blocks `d` holds, deleted ones and blocks past a gap included: the
/// insertions of its pending-free state. Its pending structs are not in
/// the block store, so they are not in it.
pub fn held(d: &Doc) -> IdSet {
    let state = encode_without_pending(&d.transact(), &StateVector::default());
    Update::decode_v1(&state)
        .expect("decode the pending-free state")
        .insertions(true)
}

/// `d`'s pending structs merged with its pending deletes, read from yrs's
/// in-memory pending update (which the encoding defect does not touch).
/// Merging the two loses which is which: for the phantom arithmetic only.
pub fn pending_ids(d: &Doc) -> IdSet {
    let txn = d.transact();
    let store = txn.store();
    let structs = store
        .pending_update()
        .map(|p| p.update.insertions(true))
        .unwrap_or_default();
    match store.pending_ds() {
        Some(ds) => structs.merge(ds),
        None => structs,
    }
}

/// Whether `d` holds blocks past a gap in their client's clock, read from
/// its pending-free state. Unlike [`has_gaps`] it never reads yrs's shifted
/// full state, so blocks that encoding names at shifted clocks (phantoms)
/// do not count as gaps.
pub fn holds_blocks_past_a_gap(d: &Doc) -> bool {
    let sv = d.transact().state_vector();
    holds_any(&at_or_above(&held(d), &sv))
}

/// `d`'s pending structs and pending deletes, apart.
pub fn pending_parts(d: &Doc) -> (IdSet, IdSet) {
    let txn = d.transact();
    let store = txn.store();
    (
        store
            .pending_update()
            .map(|p| p.update.insertions(true))
            .unwrap_or_default(),
        store.pending_ds().cloned().unwrap_or_default(),
    )
}

/// `d`'s pending structs net of the blocks it holds, and its pending
/// deletes. yrs can keep a block in its pending update after the block
/// integrated (it drops such a stale entry only at a later retry of the
/// pending update), so the raw pending structs can name held blocks, which
/// are not pending content.
pub fn pending_content(d: &Doc) -> (IdSet, IdSet) {
    let (structs, deletes) = pending_parts(d);
    (structs.diff(&held(d)), deletes)
}

/// `d`'s delete set.
pub fn deleted(d: &Doc) -> IdSet {
    d.transact().snapshot().delete_set
}

/// Whether every id of `a` is in `b` (set semantics).
fn within(a: &IdSet, b: &IdSet) -> bool {
    !holds_any(&a.diff(b))
}

/// Ruling 17's reboot relations between a rebooted doc and the doc it
/// reboots (`reference`), up to yrs's retry path dependence, one line per
/// violated relation, structs and deletes apart. Pending structs are each
/// doc's pending content ([`pending_content`]: stale entries of held blocks
/// left out):
/// 1. its state vector is at least the reference's;
/// 2. its pending structs are within the reference's, and so are its
///    pending deletes;
/// 3. the reference's pending structs are held or pending in it, and the
///    reference's pending deletes deleted or pending in it;
/// 4. it holds nothing the reference lacks except stuck content a kept row
///    integrated: its held blocks are within the reference's held blocks
///    and pending structs, and its delete set within the reference's delete
///    set, pending deletes and pending structs (the phantom check). A
///    pending struct is in the delete set once integrated when its parent
///    was deleted meanwhile (yrs deletes an item it integrates into a
///    deleted parent), with no delete row naming it.
pub fn reboot_violations(rebooted: &Doc, reference: &Doc) -> Vec<String> {
    let mut out = Vec::new();
    let (sv_b, sv_r) = (
        rebooted.transact().state_vector(),
        reference.transact().state_vector(),
    );
    if sv_r.iter().any(|(client, clock)| sv_b.get(client) < *clock) {
        out.push(format!(
            "1: the state vector is below the reference's: {sv_b:?} / {sv_r:?}"
        ));
    }
    let (ps_b, pd_b) = pending_content(rebooted);
    let (ps_r, pd_r) = pending_content(reference);
    let (held_b, held_r) = (held(rebooted), held(reference));
    let (ds_b, ds_r) = (deleted(rebooted), deleted(reference));
    if !within(&ps_b, &ps_r) {
        out.push(format!(
            "2: pending structs the reference lacks: {:?}",
            ps_b.diff(&ps_r)
        ));
    }
    if !within(&pd_b, &pd_r) {
        out.push(format!(
            "2: pending deletes the reference lacks: {:?}",
            pd_b.diff(&pd_r)
        ));
    }
    if !within(&ps_r, &held_b.merge(&ps_b)) {
        out.push(format!(
            "3: the reference's pending structs neither held nor pending: {:?}",
            ps_r.diff(&held_b.merge(&ps_b))
        ));
    }
    if !within(&pd_r, &ds_b.merge(&pd_b)) {
        out.push(format!(
            "3: the reference's pending deletes neither deleted nor pending: {:?}",
            pd_r.diff(&ds_b.merge(&pd_b))
        ));
    }
    if !within(&held_b, &held_r.merge(&ps_r)) {
        out.push(format!(
            "4: held blocks the reference neither holds nor has pending: {:?}",
            held_b.diff(&held_r.merge(&ps_r))
        ));
    }
    let deletes_r = ds_r.merge(&pd_r).merge(&ps_r);
    if !within(&ds_b, &deletes_r) {
        out.push(format!(
            "4: deletes the reference neither has nor has pending: {:?}",
            ds_b.diff(&deletes_r)
        ));
    }
    out
}

/// Panics with [`reboot_violations`] and `what` when there are any.
#[track_caller]
pub fn assert_reboots(rebooted: &Doc, reference: &Doc, what: &str) {
    let violations = reboot_violations(rebooted, reference);
    assert!(violations.is_empty(), "{what}: {violations:#?}");
}

/// Ruling 17's join bound: a joined doc's held blocks, delete set and
/// pending content are within the live doc's held blocks and pending
/// structs (structs) or its delete set and pending deletes (deletes). Its
/// delete set may also hold live pending structs it integrated into a
/// deleted parent (see [`reboot_violations`], relation 4).
pub fn join_violations(joined: &Doc, live: &Doc) -> Vec<String> {
    let mut out = Vec::new();
    let (ps_l, pd_l) = pending_parts(live);
    let structs = held(live).merge(&ps_l);
    let deletes = deleted(live).merge(&pd_l);
    let integrated_deletes = deletes.merge(&ps_l);
    let (ps_j, pd_j) = pending_parts(joined);
    if !within(&held(joined), &structs) {
        out.push(format!(
            "held blocks the live doc neither holds nor has pending: {:?}",
            held(joined).diff(&structs)
        ));
    }
    if !within(&deleted(joined), &integrated_deletes) {
        out.push(format!(
            "deletes the live doc neither has nor has pending: {:?}",
            deleted(joined).diff(&integrated_deletes)
        ));
    }
    if !within(&ps_j, &structs) {
        out.push(format!(
            "pending structs the live doc neither holds nor has pending: {:?}",
            ps_j.diff(&structs)
        ));
    }
    if !within(&pd_j, &deletes) {
        out.push(format!(
            "pending deletes the live doc neither has nor has pending: {:?}",
            pd_j.diff(&deletes)
        ));
    }
    out
}
