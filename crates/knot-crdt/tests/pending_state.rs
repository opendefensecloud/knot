//! yrs 0.27.3's shifted pending encoding, and the pending-free encoders
//! that replace it (`knot_crdt::pending`).
//!
//! `encode_state_as_update_v1` merges the v1 encoding of the doc's pending
//! update into the state it writes. Once that pending update holds
//! overlapping Skips (`fixtures::overlapping_skips`), the encoding writes
//! pending blocks at shifted clocks: the state names blocks ("phantoms")
//! the doc neither holds nor has pending. The canary below fails once yrs
//! stops doing that.
//!
//! The property drives one live doc with the interleaving generator of
//! `effective_update.rs` (`common/interleave.rs`, pulling with yrs's
//! shifted encoder, as clients did before the fix) and checks, after every
//! step:
//! - (a) the pending-free state names only held blocks;
//! - (b) yrs's full state is flagged by `may_be_shifted` whenever it holds
//!   phantoms, whenever the doc's pending update encodes `Shifted`, and
//!   whenever yrs refuses it on a fresh apply, and its trusted prefix holds
//!   no phantom;
//! - (c) the prefix plus the log gives the content the log gives;
//! - (d) a `Faithful` pending update carries the right content;
//! - (e) legacy bytes that park content without being flagged give a
//!   `Faithful` pending update, which carries the right content (Ruling 19).
//!
//! Content is compared once yrs retried both docs' pending content
//! (`settle`): which content yrs leaves stuck depends on the order updates
//! arrived in.
//!
//! Boot (`knot_crdt::boot`, on the in-memory stores of
//! `common/mem_store.rs`) re-parks pending content from the rows a
//! snapshot names, decides which snapshots it trusts, carries legacy
//! pending content no row holds, and applies every row through the
//! skip-underflow guards. The reboot property checks that a room rebooted
//! from its pending-free snapshot and the rows it names holds what the live
//! room held (Ruling 17), through several boot generations, and that the
//! history builder finds no gap in what such rooms wrote.

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/interleave.rs"]
mod interleave;
#[path = "common/log_fixtures.rs"]
mod log_fixtures;
#[path = "common/mem_store.rs"]
mod mem_store;
#[allow(dead_code)]
#[path = "../src/test_metrics.rs"]
mod test_metrics;

use fixtures::{
    Rng, cross_client_parked, cross_client_parked_tail, overlapping_skips, overlapping_skips_as,
    para, shifted_state_that_parks_nothing, shifted_state_yrs_refuses, skip_underflow_case,
    skip_underflow_parts,
};
use interleave::{
    Client, delete_before_insert, deliveries, has_gaps, held, holds_blocks_past_a_gap,
    join_violations, pending_ids, reboot_violations,
};
use knot_crdt::nesting::MAX_NESTING;
use knot_crdt::pending::{
    self, PendingV1, delete_only_update, encode_without_pending, holds_any, may_be_shifted,
    names_blocks_past_a_gap, parks_when_applied, pending_update_v1, trusted_prefix,
};
use knot_crdt::{Boot, BootError, BootMode, DocHandle, Engine, YrsEngine};
use knot_storage::RowsAfter;
use log_fixtures::{assert_same_doc, text};
use mem_store::{MemStore, RowsRead, snapshot};
use yrs::block::ClientID;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, ID, IdSet, ReadTxn, StateVector, Text, Transact, Update, WriteTxn,
    XmlElementPrelim, XmlFragment, XmlTextPrelim,
};

/// Seeds the property runs, and steps per seed.
const SEEDS: u64 = 300;
const STEPS: usize = 60;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn insertions(update: &[u8]) -> IdSet {
    Update::decode_v1(update)
        .expect("an update")
        .insertions(true)
}

/// A fresh doc with `updates` applied in order.
fn doc_of(updates: &[&[u8]]) -> DocHandle {
    let d = YrsEngine.new_doc();
    for u in updates {
        YrsEngine.apply_update(&d, u).expect("the update applies");
    }
    d
}

/// yrs's own full state of `d`: what rooms wrote before the fix.
fn shifted_full(d: &Doc) -> Vec<u8> {
    d.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

/// `d`'s pending-free full state.
fn pending_free(d: &Doc) -> Vec<u8> {
    encode_without_pending(&d.transact(), &StateVector::default())
}

/// The ids below each client's clock in `sv`.
fn below(sv: &StateVector) -> IdSet {
    let mut out = IdSet::new();
    for (client, clock) in sv.iter() {
        out.insert(ID::new(*client, 0), *clock);
    }
    out
}

fn meets(a: &IdSet, b: &IdSet) -> bool {
    holds_any(&a.intersect(b))
}

fn within(a: &IdSet, b: &IdSet) -> bool {
    !holds_any(&a.diff(b))
}

fn same_ids(a: &IdSet, b: &IdSet) -> bool {
    within(a, b) && within(b, a)
}

/// The doc's pending structs and pending deletes, by set semantics.
fn pending_parts(d: &Doc) -> (IdSet, IdSet) {
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

fn holds_pending(d: &Doc) -> bool {
    let (structs, deletes) = pending_parts(d);
    holds_any(&structs) || holds_any(&deletes)
}

/// The state vector, delete set and `"default"` fragment's content.
fn content(d: &Doc) -> (StateVector, IdSet, String) {
    let frag = d.get_or_insert_xml_fragment("default");
    let txn = d.transact();
    (
        txn.state_vector(),
        txn.snapshot().delete_set,
        frag.get_string(&txn),
    )
}

/// Whether `a` and `b` hold the same state vector, delete set (by set
/// semantics) and content.
fn same_content(a: &Doc, b: &Doc) -> bool {
    let (sv_a, ds_a, text_a) = content(a);
    let (sv_b, ds_b, text_b) = content(b);
    sv_a == sv_b && same_ids(&ds_a, &ds_b) && text_a == text_b
}

/// Makes yrs retry `d`'s pending update, as boot's `retry_pending` does
/// (Ruling 8), until its pending content stops changing. yrs 0.27.3
/// retries a pending update only when a missing client's clock moves past
/// the clock it recorded (`transaction.rs:838-842`), so which content stays
/// stuck depends on the order updates arrived in (M1.3 Ruling 2). Two docs
/// that applied the same updates in different orders hold the same content
/// once both are settled.
fn settle(d: &Doc) {
    for _ in 0..8 {
        let before = fingerprint(d);
        if before == (None, None) {
            return;
        }
        {
            let mut txn = d.transact_mut();
            if let Some(p) = txn.store_mut().pending_update_mut() {
                let clients: Vec<ClientID> = p.missing.iter().map(|(c, _)| *c).collect();
                for c in clients {
                    p.missing.set_min(c, 0);
                }
            }
            let _ = txn.apply_update(Update::new());
        }
        if fingerprint(d) == before {
            return;
        }
    }
}

/// Whether `a` and `b` hold the same content (state vector, delete set,
/// `"default"` fragment) and the same pending structs and pending deletes,
/// by set semantics.
fn same_settled(a: &Doc, b: &Doc) -> bool {
    let (ps_a, pd_a) = pending_parts(a);
    let (ps_b, pd_b) = pending_parts(b);
    same_content(a, b) && same_ids(&ps_a, &ps_b) && same_ids(&pd_a, &pd_b)
}

/// `d`'s pending update and pending delete set, encoded.
fn fingerprint(d: &Doc) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let txn = d.transact();
    let store = txn.store();
    (
        store.pending_update().map(|p| p.update.encode_v1()),
        store.pending_ds().map(|ds| ds.encode_v1()),
    )
}

/// The pending-free assertions on `d`, as failure messages: its
/// pending-free state names no pending id and every id below its state
/// vector, and a fresh doc of it holds no pending content and the same
/// state vector, delete set, content and pending-free insertions.
fn pending_free_violations(d: &Doc) -> Vec<String> {
    let mut out = Vec::new();
    let state = pending_free(d);
    let ins = insertions(&state);
    // Pending structs only: a pending delete can name a held block (yrs
    // defers the deletion of a block past its client's gap).
    let (pending_structs, _) = pending_parts(d);
    if meets(&ins, &pending_structs) {
        out.push("the pending-free state names a pending struct".to_string());
    }
    let sv = d.transact().state_vector();
    if !within(&below(&sv), &ins) {
        out.push("the pending-free state lacks an id below the state vector".to_string());
    }
    let fresh = doc_of(&[&state]);
    if holds_pending(fresh.inner()) {
        out.push("the pending-free state parks content".to_string());
    }
    let (sv_f, ds_f, text_f) = content(fresh.inner());
    let (sv_d, ds_d, text_d) = content(d);
    if sv_f != sv_d {
        out.push(format!("state vectors differ: {sv_f:?} vs {sv_d:?}"));
    }
    if !same_ids(&ds_f, &ds_d) {
        out.push("delete sets differ".to_string());
    }
    if text_f != text_d {
        out.push(format!("content differs: {text_f:?} vs {text_d:?}"));
    }
    if !same_ids(&insertions(&pending_free(fresh.inner())), &ins) {
        out.push("the fresh doc's pending-free insertions differ".to_string());
    }
    out
}

/// The overlapping-Skips doc: `t0`, `t2` and `t3` applied.
fn overlapping_doc() -> DocHandle {
    let o = overlapping_skips();
    doc_of(&[&o.t0, &o.t2, &o.t3])
}

// ---------------------------------------------------------------------------
// The defect and the encoders
// ---------------------------------------------------------------------------

/// The canary: yrs 0.27.3's full state of a doc whose pending update holds
/// overlapping Skips names ids the doc neither holds nor has pending. When
/// this fails, yrs stopped shifting: see the plan's "After this fix".
#[test]
fn yrs_0_27_full_state_shifts_pending_blocks_behind_overlapping_skips() {
    let d = overlapping_doc();
    let txn = d.inner().transact();
    let full = insertions(&txn.encode_state_as_update_v1(&StateVector::default()));
    drop(txn);
    assert!(
        holds_any(&full.diff(&held(d.inner()).merge(&pending_ids(d.inner())))),
        "yrs still shifts"
    );
}

#[test]
fn the_pending_free_state_names_only_held_blocks() {
    let d = overlapping_doc();
    assert!(holds_pending(d.inner()), "fixture: t2 and t3's \"qq\" park");
    let found = pending_free_violations(d.inner());
    assert!(found.is_empty(), "{found:?}");
}

/// The reproducer through the production engine: the state every snapshot,
/// SyncStep2 and `ExportState` is encoded with names no block the doc
/// lacks, its pending content included.
#[test]
fn the_engines_state_names_no_block_the_doc_lacks() {
    let o = overlapping_skips();
    let d = YrsEngine.new_doc();
    for u in [&o.t0, &o.t2, &o.t3] {
        YrsEngine.apply_update(&d, u).expect("the update applies");
    }
    assert!(holds_pending(d.inner()), "fixture: t2 and t3's \"qq\" park");
    let state = YrsEngine
        .encode_state_as_update(&d, None)
        .expect("the state encodes");
    let named = insertions(&state).diff(&held(d.inner()));
    assert!(
        !holds_any(&named),
        "the engine's state names blocks the doc lacks: {named:?}"
    );
}

#[test]
fn names_blocks_past_a_gap_flags_the_shifted_state_only() {
    let d = overlapping_doc();
    assert!(names_blocks_past_a_gap(&shifted_full(d.inner())));

    let o = overlapping_skips();
    let drained = doc_of(&[&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3]);
    assert!(
        !holds_pending(drained.inner()),
        "fixture: in order, nothing parks"
    );
    assert!(!names_blocks_past_a_gap(&pending_free(drained.inner())));

    // One client whose blocks start at clock 5: the update of its second
    // transaction alone.
    let c = Doc::with_client_id(9);
    let frag = c.get_or_insert_xml_fragment("default");
    {
        let mut txn = c.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("abc"));
    }
    let sv = c.transact().state_vector();
    {
        let mut txn = c.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("d"));
    }
    let late = c.transact().encode_state_as_update_v1(&sv);
    let lower = Update::decode_v1(&late).unwrap().state_vector_lower();
    assert_eq!(
        lower.get(&ClientID::new(9)),
        5,
        "fixture: client 9 starts at clock 5"
    );
    assert!(names_blocks_past_a_gap(&late));

    assert!(!names_blocks_past_a_gap(b"not an update"));
}

#[test]
fn parks_when_applied_flags_pending_content_only() {
    let d = overlapping_doc();
    assert!(parks_when_applied(&shifted_full(d.inner())));
    assert!(!parks_when_applied(&pending_free(d.inner())));
    assert!(!parks_when_applied(b"not an update"));
}

/// `may_be_shifted` is `names_blocks_past_a_gap` alone (Task 1 Step 6's
/// fallback for claim 1): shifted states can park nothing when applied
/// fresh, and yrs can refuse them part way.
#[test]
fn may_be_shifted_flags_every_state_that_names_a_block_past_a_gap() {
    let d = overlapping_doc();
    assert!(may_be_shifted(&shifted_full(d.inner())));

    // Shifted, and parks nothing when applied fresh (claim 1 fails).
    let quiet = shifted_state_that_parks_nothing();
    assert!(!parks_when_applied(&quiet));
    assert!(names_blocks_past_a_gap(&quiet));
    assert!(may_be_shifted(&quiet));

    // Shifted, and refused part way by a fresh apply.
    let refused = shifted_state_yrs_refuses();
    assert!(
        YrsEngine
            .apply_update(&YrsEngine.new_doc(), &refused)
            .is_err()
    );
    assert!(may_be_shifted(&refused));

    // Real text past a gap, nothing parked: flagged too (the fallback's
    // cost, Ruling 7).
    let o = overlapping_skips();
    let gapped = doc_of(&[&o.t0, &o.t3]);
    let state = pending_free(gapped.inner());
    assert!(!parks_when_applied(&state));
    assert!(may_be_shifted(&state), "t3's paragraph past t1's gap");

    // Parked, no block past a gap.
    let (_, u2) = cross_client_parked();
    let parked = doc_of(&[&u2]);
    let full = shifted_full(parked.inner());
    assert!(parks_when_applied(&full));
    assert!(!names_blocks_past_a_gap(&full));
    assert!(!may_be_shifted(&full));
}

/// A state yrs refuses part way still has a trusted prefix: the blocks
/// yrs integrated before the refused one, below each client's first gap,
/// with their deletions.
#[test]
fn the_trusted_prefix_of_a_state_yrs_refuses_applies() {
    let refused = shifted_state_yrs_refuses();
    let prefix = trusted_prefix(&refused).expect("the prefix");
    let d = doc_of(&[&prefix]);
    assert!(!holds_pending(d.inner()));
    assert!(!names_blocks_past_a_gap(&prefix));
    assert!(
        holds_any(&insertions(&prefix)),
        "the prefix keeps some text"
    );
    let quiet = trusted_prefix(&shifted_state_that_parks_nothing()).expect("the prefix");
    assert!(!names_blocks_past_a_gap(&quiet));
}

#[test]
fn pending_update_v1_tells_empty_faithful_and_shifted_apart() {
    let fresh = YrsEngine.new_doc();
    assert_eq!(
        pending_update_v1(&fresh.inner().transact()),
        PendingV1::Empty
    );

    let d = overlapping_doc();
    assert_eq!(pending_update_v1(&d.inner().transact()), PendingV1::Shifted);

    let (_, u2) = cross_client_parked();
    let parked = doc_of(&[&u2]);
    let PendingV1::Faithful(bytes) = pending_update_v1(&parked.inner().transact()) else {
        panic!("a single parked update encodes faithfully");
    };
    assert!(same_ids(&insertions(&bytes), &insertions(&u2)));
}

#[test]
fn delete_only_update_round_trips() {
    let mut ds = IdSet::new();
    ds.insert(ID::new(ClientID::new(3), 2), 4);
    ds.insert(ID::new(ClientID::new(7), 0), 1);
    let bytes = delete_only_update(&ds);
    let u = Update::decode_v1(&bytes).expect("decodes");
    assert!(!holds_any(&u.insertions(true)), "no blocks");
    assert_eq!(u.delete_set(), &ds);
}

#[test]
fn the_trusted_prefix_of_the_shifted_state_holds_no_phantom() {
    let d = overlapping_doc();
    let full = shifted_full(d.inner());
    let prefix = trusted_prefix(&full).expect("the prefix");
    assert!(within(&insertions(&prefix), &held(d.inner())));
    assert!(!holds_pending(doc_of(&[&prefix]).inner()));
    assert!(matches!(
        trusted_prefix(b"not an update"),
        Err(knot_crdt::EngineError::Decode(_))
    ));
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

/// What the property found, and the seeds of each.
#[derive(Default)]
struct Tally {
    steps: usize,
    pending_free_violations: Vec<(u64, usize, String)>,
    // (b)
    phantom_steps: usize,
    phantom_seeds: Vec<u64>,
    /// Claim 1 of the plan's test: informational since the fallback (see
    /// `pending::may_be_shifted`).
    phantom_but_not_parking: Vec<u64>,
    phantom_but_no_gap: Vec<u64>,
    phantom_not_flagged: Vec<u64>,
    prefix_meets_phantom: Vec<u64>,
    prefix_not_held: Vec<u64>,
    /// Steps whose live pending update encodes `Shifted`, and those of them
    /// whose full state `may_be_shifted` does not flag.
    shifted_steps: usize,
    shifted_not_flagged: Vec<u64>,
    /// Steps whose full state yrs refuses to apply to a fresh doc, and
    /// those of them `may_be_shifted` does not flag.
    rejected_steps: Vec<u64>,
    rejected_not_flagged: Vec<u64>,
    // (c)
    c_run: usize,
    c_run_with_phantoms: usize,
    c_skipped: usize,
    c_mismatch: Vec<u64>,
    // (d)
    d_run_single: usize,
    d_run_merged: usize,
    d_skipped: usize,
    d_mismatch_single: Vec<u64>,
    d_mismatch_merged: Vec<u64>,
    // (e)
    e_parking: usize,
    e_flagged: usize,
    e_flagged_shifted: usize,
    e_unflagged_shifted: Vec<u64>,
    e_run: usize,
    e_skipped: usize,
    e_reference_refused: Vec<String>,
    e_mismatch: Vec<u64>,
}

fn seeds(v: &[u64]) -> String {
    let mut s = v.to_vec();
    s.dedup();
    format!("{} {:?}", v.len(), &s[..s.len().min(20)])
}

/// One step of the property: every check on the live doc after `delivered`.
struct Step<'a> {
    seed: u64,
    step: usize,
    live: &'a DocHandle,
    delivered: &'a [Vec<u8>],
    clients: &'a [Client],
    /// Pending-changing applies since the live doc last held no pending
    /// content.
    pending_applies: usize,
}

impl Step<'_> {
    /// What every client has not delivered yet, applied after the rest.
    fn outstanding(&self) -> Vec<Vec<u8>> {
        self.clients.iter().flat_map(|c| c.outstanding()).collect()
    }

    /// A fresh doc that applies `first`, then every delivery so far (when
    /// `with_deliveries`), then every client's outstanding updates; `Err`
    /// names the first update yrs refused (part of it may be applied).
    fn rebuilt(&self, first: &[&[u8]], with_deliveries: bool) -> Result<DocHandle, String> {
        let d = YrsEngine.new_doc();
        for u in first {
            YrsEngine
                .apply_update(&d, u)
                .map_err(|e| format!("the base: {e}"))?;
        }
        if with_deliveries {
            for u in self.delivered {
                YrsEngine
                    .apply_update(&d, u)
                    .map_err(|e| format!("a delivery: {e}"))?;
            }
        }
        for u in self.outstanding() {
            YrsEngine
                .apply_update(&d, &u)
                .map_err(|e| format!("an outstanding update: {e}"))?;
        }
        Ok(d)
    }

    /// Whether `doc` (an `Err` counts as a mismatch) holds what `reference`
    /// holds, once yrs retried both docs' pending content ([`settle`]).
    fn matches(&self, doc: Result<DocHandle, String>, reference: &DocHandle) -> bool {
        doc.is_ok_and(|d| {
            settle(d.inner());
            settle(reference.inner());
            same_settled(d.inner(), reference.inner())
        })
    }

    fn check(&self, t: &mut Tally) {
        let live = self.live.inner();
        let seed = self.seed;
        t.steps += 1;

        // (a)
        for v in pending_free_violations(live) {
            t.pending_free_violations.push((seed, self.step, v));
        }

        // (b) Phantoms are ids yrs's full state names that the doc neither
        // holds nor has pending. Every state with phantoms, every state of
        // a doc whose pending update encodes `Shifted` and every state yrs
        // refuses on a fresh apply must be flagged, and the prefix must
        // hold no phantom.
        let full = shifted_full(live);
        let phantoms = insertions(&full).diff(&held(live)).diff(&pending_ids(live));
        let has_phantoms = holds_any(&phantoms);
        let flagged = may_be_shifted(&full);
        let prefix = trusted_prefix(&full).expect("the trusted prefix of yrs's full state");
        let prefix_ins = insertions(&prefix);
        // Stuck pending content (yrs retries a pending update only when a
        // missing clock moves past its record) integrates on a fresh apply,
        // so the bound is Ruling 17's: held or pending structs.
        let (pending_structs, _) = pending_parts(live);
        if !within(&prefix_ins, &held(live).merge(&pending_structs)) {
            t.prefix_not_held.push(seed);
        }
        if YrsEngine.apply_update(&YrsEngine.new_doc(), &full).is_err() {
            t.rejected_steps.push(seed);
            if !flagged {
                t.rejected_not_flagged.push(seed);
            }
        }
        if pending_update_v1(&live.transact()) == PendingV1::Shifted {
            t.shifted_steps += 1;
            if !flagged {
                t.shifted_not_flagged.push(seed);
            }
        }
        if has_phantoms {
            t.phantom_steps += 1;
            t.phantom_seeds.push(seed);
            if !parks_when_applied(&full) {
                t.phantom_but_not_parking.push(seed);
            }
            if !names_blocks_past_a_gap(&full) {
                t.phantom_but_no_gap.push(seed);
            }
            if !flagged {
                t.phantom_not_flagged.push(seed);
            }
            if meets(&prefix_ins, &phantoms) {
                t.prefix_meets_phantom.push(seed);
            }
        }

        // (c) and (d) compare with the same reference: every delivery so
        // far, then every client's outstanding updates, built once.
        let mut reference: Option<DocHandle> = None;
        let reference_doc = |reference: &mut Option<DocHandle>| -> DocHandle {
            reference.take().unwrap_or_else(|| {
                self.rebuilt(&[], true).unwrap_or_else(|e| {
                    panic!("seed {} step {}: the reference: {e}", self.seed, self.step)
                })
            })
        };

        if has_phantoms || self.step.is_multiple_of(10) {
            let r = reference_doc(&mut reference);
            if holds_blocks_past_a_gap(r.inner()) {
                t.c_skipped += 1;
            } else {
                t.c_run += 1;
                if has_phantoms {
                    t.c_run_with_phantoms += 1;
                }
                if !self.matches(self.rebuilt(&[&prefix], true), &r) {
                    t.c_mismatch.push(seed);
                }
            }
            reference = Some(r);
        }

        if let PendingV1::Faithful(bytes) = pending_update_v1(&live.transact()) {
            let r = reference_doc(&mut reference);
            if holds_blocks_past_a_gap(r.inner()) {
                t.d_skipped += 1;
            } else {
                let (_, deletes) = pending_parts(live);
                let base = pending_free(live);
                let delete_only = delete_only_update(&deletes);
                let mut first: Vec<&[u8]> = vec![&base, &bytes];
                if holds_any(&deletes) {
                    first.push(&delete_only);
                }
                let ok = self.matches(self.rebuilt(&first, false), &r);
                if self.pending_applies <= 1 {
                    t.d_run_single += 1;
                    if !ok {
                        t.d_mismatch_single.push(seed);
                    }
                } else {
                    t.d_run_merged += 1;
                    if !ok {
                        t.d_mismatch_merged.push(seed);
                    }
                }
            }
        }

        // (e) Ruling 19's premise, over the legacy bytes of this step: the
        // live doc's full state and every client's.
        let mut candidates = vec![full];
        candidates.extend(self.clients.iter().map(|c| shifted_full(&c.doc)));
        for s in &candidates {
            if !parks_when_applied(s) {
                continue;
            }
            t.e_parking += 1;
            let f = YrsEngine.new_doc();
            let _ = YrsEngine.apply_update(&f, s);
            let v1 = pending_update_v1(&f.inner().transact());
            if may_be_shifted(s) {
                // Flagged bytes boot from their trusted prefix (Ruling 7),
                // never carried, so no content check: applying them and
                // later updates can crash yrs (seed 177, step 61: SIGBUS).
                t.e_flagged += 1;
                if v1 == PendingV1::Shifted {
                    t.e_flagged_shifted += 1;
                }
                continue;
            }
            if v1 == PendingV1::Shifted {
                t.e_unflagged_shifted.push(seed);
                continue;
            }
            let reference = match self.rebuilt(&[s], false) {
                Ok(r) => r,
                Err(e) => {
                    t.e_reference_refused
                        .push(format!("seed {seed} step {}: {e}", self.step));
                    continue;
                }
            };
            if holds_blocks_past_a_gap(reference.inner()) {
                t.e_skipped += 1;
                continue;
            }
            let (_, deletes) = pending_parts(f.inner());
            let base = pending_free(f.inner());
            let delete_only = delete_only_update(&deletes);
            let mut first: Vec<&[u8]> = vec![&base];
            if let PendingV1::Faithful(bytes) = &v1 {
                first.push(bytes);
            }
            if holds_any(&deletes) {
                first.push(&delete_only);
            }
            t.e_run += 1;
            if !self.matches(self.rebuilt(&first, false), &reference) {
                t.e_mismatch.push(seed);
            }
        }
    }
}

/// yrs's shifted encoder: what rooms sent clients before the fix. The
/// generator pulls with it, so the histories are those of
/// `effective_update.rs` and the states are those legacy snapshots hold.
fn shifted(d: &DocHandle, sv: &StateVector) -> Vec<u8> {
    d.inner().transact().encode_state_as_update_v1(sv)
}

#[test]
fn pending_free_states_and_prefixes_hold_no_phantom() {
    let mut t = Tally::default();
    for seed in 1..=SEEDS {
        let mut rng = Rng::new(seed);
        let mut clients: Vec<Client> = (1..=3).map(Client::new).collect();
        let live = YrsEngine.new_doc();
        let mut delivered: Vec<Vec<u8>> = Vec::new();
        let mut pending_applies = 0usize;
        let deliver = |bytes: Vec<u8>, delivered: &mut Vec<Vec<u8>>, n: &mut usize| {
            let before = fingerprint(live.inner());
            YrsEngine
                .apply_update_measuring(&live, &bytes, MAX_NESTING)
                .unwrap_or_else(|e| panic!("seed {seed}: measured apply: {e}"));
            delivered.push(bytes);
            if !holds_pending(live.inner()) {
                *n = 0;
            } else if fingerprint(live.inner()) != before {
                *n += 1;
            }
        };
        let fixed_step = rng.below(STEPS);
        for step in 0..STEPS {
            let editor = rng.below(clients.len());
            if rng.chance(40) && !has_gaps(&live) {
                clients[editor].pull(&live, shifted);
            }
            for _ in 0..1 + rng.below(3) {
                clients[editor].edit(&mut rng);
            }
            let c = rng.below(clients.len());
            if step == fixed_step {
                let u2 = delete_before_insert(&mut clients[c]);
                deliver(u2, &mut delivered, &mut pending_applies);
            } else {
                for bytes in deliveries(&mut clients[c], &live, &mut rng) {
                    deliver(bytes, &mut delivered, &mut pending_applies);
                }
            }
            Step {
                seed,
                step,
                live: &live,
                delivered: &delivered,
                clients: &clients,
                pending_applies,
            }
            .check(&mut t);
        }
        // Everything held back arrives, late and in random order, with what
        // each client has not sent yet; the checks run after each.
        let mut step = STEPS;
        for i in 0..clients.len() {
            let own = clients[i].take_undelivered();
            if !own.is_empty() {
                clients[i]
                    .held
                    .push(yrs::merge_updates_v1(&own).expect("merge own updates"));
            }
            while !clients[i].held.is_empty() {
                let at = rng.below(clients[i].held.len());
                let bytes = clients[i].held.remove(at);
                deliver(bytes, &mut delivered, &mut pending_applies);
                Step {
                    seed,
                    step,
                    live: &live,
                    delivered: &delivered,
                    clients: &clients,
                    pending_applies,
                }
                .check(&mut t);
                step += 1;
            }
        }
    }

    eprintln!("steps checked: {}", t.steps);
    eprintln!(
        "(a) pending-free violations: {} {:?}",
        t.pending_free_violations.len(),
        &t.pending_free_violations[..t.pending_free_violations.len().min(5)]
    );
    eprintln!(
        "(b) steps with phantoms: {} (seeds {}); claim 1, parks when applied fresh, fails \
         on: {}; claim 2, names a block past a gap, fails on: {}; not flagged by \
         may_be_shifted: {}; prefix meets a phantom: {}; prefix names an id neither held nor \
         pending: {}",
        t.phantom_steps,
        seeds(&t.phantom_seeds),
        seeds(&t.phantom_but_not_parking),
        seeds(&t.phantom_but_no_gap),
        seeds(&t.phantom_not_flagged),
        seeds(&t.prefix_meets_phantom),
        seeds(&t.prefix_not_held),
    );
    eprintln!(
        "(b') steps whose pending update is Shifted: {}, not flagged: {}; full states yrs \
         refuses fresh: {}, not flagged: {}",
        t.shifted_steps,
        seeds(&t.shifted_not_flagged),
        seeds(&t.rejected_steps),
        seeds(&t.rejected_not_flagged),
    );
    eprintln!(
        "(c) run {} ({} with phantoms), skipped {}, mismatched {}",
        t.c_run,
        t.c_run_with_phantoms,
        t.c_skipped,
        seeds(&t.c_mismatch)
    );
    eprintln!(
        "(d) run single-apply {}, merged {}; skipped {}; mismatched single-apply {}, merged {}",
        t.d_run_single,
        t.d_run_merged,
        t.d_skipped,
        seeds(&t.d_mismatch_single),
        seeds(&t.d_mismatch_merged)
    );
    eprintln!(
        "(e) parking candidates {}, flagged {} ({} of them Shifted, not content-checked), \
         unflagged Shifted {}; content checks run {}, skipped {} (past a gap), references \
         refused {} {:?}, mismatched {}",
        t.e_parking,
        t.e_flagged,
        t.e_flagged_shifted,
        seeds(&t.e_unflagged_shifted),
        t.e_run,
        t.e_skipped,
        t.e_reference_refused.len(),
        &t.e_reference_refused[..t.e_reference_refused.len().min(5)],
        seeds(&t.e_mismatch)
    );

    assert!(t.pending_free_violations.is_empty(), "(a) violated");
    assert!(
        t.phantom_steps > 0,
        "the generator never produced a phantom"
    );
    assert!(t.phantom_but_no_gap.is_empty(), "(b) claim 2 violated");
    assert!(
        t.phantom_not_flagged.is_empty(),
        "(b) a state with phantoms is not flagged"
    );
    assert!(
        t.shifted_not_flagged.is_empty(),
        "(b) the state of a doc whose pending update is Shifted is not flagged"
    );
    assert!(
        t.rejected_not_flagged.is_empty(),
        "(b) a state yrs refuses is not flagged"
    );
    assert!(
        t.prefix_meets_phantom.is_empty(),
        "(b) the prefix kept a phantom"
    );
    assert!(
        t.prefix_not_held.is_empty(),
        "(b) the prefix names an id neither held nor pending"
    );
    assert!(t.c_mismatch.is_empty(), "(c) mismatched");
    assert!(
        t.c_run_with_phantoms > 0,
        "(c) never ran on a step with phantoms"
    );
    assert!(
        t.d_mismatch_single.is_empty(),
        "(d) single-apply mismatched"
    );
    assert!(t.d_mismatch_merged.is_empty(), "(d) merged mismatched");
    assert!(
        t.d_run_merged > 0,
        "(d) never ran on merged pending content"
    );
    assert!(
        t.e_unflagged_shifted.is_empty(),
        "(e) an unflagged state gave Shifted"
    );
    assert!(t.e_mismatch.is_empty(), "(e) mismatched");
    assert!(
        t.e_reference_refused.is_empty(),
        "(e) an unflagged reference refused an update"
    );
    assert!(t.e_run > 0, "(e) never ran on an unflagged candidate");
}

/// The overlapping-Skips fixture: applied in order, its five transactions
/// hold no pending content. Delivered late (`t1` and `t2b` after `t0`,
/// `t2` and `t3`), "pp" and "qq" stay stuck in either order: yrs 0.27.3
/// retries a pending update only when a missing client's clock (the end of
/// its block list) moves past the recorded clock, and `t3`'s paragraph
/// already sits past the gap. A forced retry (boot's `retry_pending`,
/// Ruling 8) drains it.
#[test]
fn the_overlapping_skips_fixture_drains_in_order_or_on_a_forced_retry() {
    let o = overlapping_skips();
    let in_order = doc_of(&[&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3]);
    assert!(!holds_pending(in_order.inner()));
    let frag = in_order.inner().get_or_insert_xml_fragment("default");
    {
        let txn = in_order.inner().transact();
        let text = fixtures::text_at(&frag, &txn, 0).expect("a text");
        assert_eq!(text.get_string(&txn), "zzbaseWqqWWpp");
    }
    for late in [
        doc_of(&[&o.t0, &o.t2, &o.t3, &o.t1, &o.t2b]),
        doc_of(&[&o.t0, &o.t2, &o.t3, &o.t2b, &o.t1]),
    ] {
        let (structs, _) = pending_parts(late.inner());
        assert!(holds_any(&structs), "\"pp\" and \"qq\" stay stuck");
        settle(late.inner());
        assert!(!holds_pending(late.inner()));
        assert!(same_content(in_order.inner(), late.inner()));
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

/// Boots the in-memory page in `mode`.
async fn boot_in(store: &MemStore, mode: BootMode) -> Result<Boot, BootError> {
    knot_crdt::boot(&YrsEngine, store, store, uuid::Uuid::nil(), mode).await
}

/// A room-mode boot of the in-memory page.
async fn room_boot(store: &MemStore) -> Boot {
    boot_in(store, BootMode::Room)
        .await
        .expect("the room boots")
}

/// The update of one transaction `f` runs on `doc`.
fn own_update(doc: &Doc, f: impl FnOnce(&mut yrs::TransactionMut)) -> Vec<u8> {
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sub = {
        let out = out.clone();
        doc.observe_update_v1(move |_, e| *out.lock().unwrap() = e.update.clone())
            .expect("observe_update_v1")
    };
    f(&mut doc.transact_mut());
    drop(sub);
    let update = std::mem::take(&mut *out.lock().unwrap());
    assert!(!update.is_empty(), "the transaction changed something");
    update
}

/// A doc of `client` that applied `updates`.
fn client_doc(client: u64, updates: &[&[u8]]) -> Doc {
    let doc = Doc::with_client_id(client);
    for u in updates {
        doc.transact_mut()
            .apply_update(Update::decode_v1(u).expect("decodes"))
            .expect("applies");
    }
    doc
}

/// An edit by `client`, on a doc that applied `updates`: `word` at index 2
/// of the first paragraph's text ("ba|se").
fn edit_inside_base(client: u64, updates: &[&[u8]], word: &str) -> Vec<u8> {
    let doc = client_doc(client, updates);
    let frag = doc.get_or_insert_xml_fragment("default");
    own_update(&doc, |txn| {
        let text = fixtures::text_at(&frag, txn, 0).expect("a first paragraph");
        text.insert(txn, 2, word);
    })
}

/// A paragraph `word` by `doc` (appended), in its own transaction.
fn paragraph_by(doc: &Doc, word: &str) -> Vec<u8> {
    let frag = doc.get_or_insert_xml_fragment("default");
    own_update(doc, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(word));
    })
}

/// A doc that applied `updates` through the room's measured apply.
fn live_of(updates: &[&[u8]]) -> DocHandle {
    let live = YrsEngine.new_doc();
    for u in updates {
        YrsEngine
            .apply_update_measuring(&live, u, MAX_NESTING)
            .expect("the measured apply");
    }
    live
}

/// The insertions of every update in `updates`.
fn insertions_of(updates: &[&[u8]]) -> IdSet {
    updates
        .iter()
        .fold(IdSet::new(), |acc, u| acc.merge(&insertions(u)))
}

/// The reason, `whole_log`, `refused` and `dropped == 0 && dropped_past_gap
/// == 0` of a boot's fallback.
fn fell_back(boot: &Boot) -> Option<(&'static str, bool, u64, bool)> {
    boot.fallback.as_ref().map(|f| {
        (
            f.reason,
            f.whole_log,
            f.refused,
            f.dropped == 0 && f.dropped_past_gap == 0,
        )
    })
}

/// The overlapping-Skips rows `t0`, `t2` and `t3` at revs 1-3, and the
/// pending-free snapshot of a doc that applied them, at rev 3, naming revs
/// 2 and 3.
fn reparking_page() -> (MemStore, DocHandle) {
    let o = overlapping_skips();
    let store = MemStore::new();
    for (rev, row) in [(1, &o.t0), (2, &o.t2), (3, &o.t3)] {
        store.push_row(rev, row.clone());
    }
    let live = live_of(&[&o.t0, &o.t2, &o.t3]);
    store.push_snapshot(snapshot(
        Some(3),
        3,
        pending_free(live.inner()),
        Some(vec![2, 3]),
        false,
    ));
    (store, live)
}

#[tokio::test]
async fn boot_reapplies_the_pending_rows_after_the_snapshot() {
    let (store, live) = reparking_page();
    let boot = room_boot(&store).await;
    assert!(pending::claims(&live).is_some(), "fixture: pp and qq park");
    assert_eq!(pending::claims(&boot.doc), pending::claims(&live));
    assert_eq!(boot.parked.revs(), vec![2, 3]);
    assert_eq!(
        store.reads(),
        vec![RowsRead::After(RowsAfter::Rev(3)), RowsRead::At(vec![2, 3])]
    );
    assert_eq!(boot.fallback, None);
    assert!(!boot.unlogged);
    assert!(boot.carry.is_empty());
}

#[tokio::test]
async fn read_only_boot_loads_what_a_room_boot_loads() {
    let (store, _) = reparking_page();
    let room = room_boot(&store).await;
    let read_only = boot_in(&store, BootMode::ReadOnly).await.expect("loads");
    assert_same_doc(&read_only.doc, &room.doc, "read-only vs room");
    assert!(read_only.parked.is_empty(), "only rooms derive parked rows");
}

/// The underflow shape at boot: the snapshot holds T2 past a gap and names
/// the resend, whose GC block runs into T2.
#[tokio::test]
async fn boot_refuses_a_pending_row_that_would_underflow() {
    let (state, resend, _) = skip_underflow_case();
    assert!(!parks_when_applied(&state), "fixture: .0 parks nothing");
    let store = MemStore::new();
    store.push_row(1, resend.clone());
    store.push_row(2, state.clone());
    store.push_snapshot(snapshot(Some(2), 2, state.clone(), Some(vec![1]), false));

    let recorder = test_metrics::CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let boot = room_boot(&store).await;
    assert_eq!(fell_back(&boot), Some(("underflow", true, 0, true)));
    assert!(
        boot.unlogged,
        "the resend is a failed row, not a skipped one"
    );
    let fresh = doc_of(&[&state]);
    assert_same_doc(&boot.doc, &fresh, "the doc is .0");
    assert!(matches!(
        boot_in(&store, BootMode::ReadOnly).await,
        Err(BootError::Apply(_))
    ));
    assert_eq!(
        recorder.counter("knot_room_boot_fallbacks_total", &[("reason", "underflow")]),
        1,
        "the room boot counts; the read-only boot, which fails, does not"
    );
}

/// Rule 3's `All` path (an unstamped legacy snapshot), which replayed
/// unguarded before the fix: `seq 1 <= snapshot_seq` does not mean the
/// snapshot holds row 1.
#[tokio::test]
async fn an_unstamped_snapshot_that_lacks_a_lower_row_does_not_skip_it() {
    let (state, resend, _) = skip_underflow_case();
    let (paras, t2_only) = skip_underflow_parts();
    assert!(!may_be_shifted(&paras), "fixture: paras boot by rule 3");
    let store = MemStore::new();
    store.push_row(1, t2_only.clone());
    store.push_row(2, resend.clone());
    store.push_snapshot(snapshot(None, 2, paras.clone(), None, false));
    let boot = room_boot(&store).await;
    assert_eq!(fell_back(&boot), Some(("underflow", true, 0, true)));
    assert!(boot.unlogged);
    assert_eq!(text(&boot.doc), text(&doc_of(&[&state])));
    assert!(matches!(
        boot_in(&store, BootMode::ReadOnly).await,
        Err(BootError::Apply(_))
    ));

    // The brief's own fixture: an unstamped snapshot of .0, which names a
    // block past a gap, so `may_be_shifted` (Task 1) sends it to its
    // trusted prefix and the whole log, where the resend applies before
    // T2 and nothing is refused.
    let store = MemStore::new();
    store.push_row(1, resend);
    store.push_row(2, state.clone());
    store.push_snapshot(snapshot(None, 2, state, None, false));
    let boot = room_boot(&store).await;
    assert_eq!(boot.fallback.as_ref().map(|f| f.reason), Some("legacy"));
    assert!(!boot.unlogged);
}

#[tokio::test]
async fn a_refused_row_over_a_prefix_base_is_not_skipped() {
    let o = overlapping_skips_as(5);
    let (state, resend, _) = skip_underflow_case();
    let store = MemStore::new();
    let rows: [&[u8]; 5] = [&o.t0, &o.t2, &o.t3, &state, &resend];
    for (rev, row) in (1..).zip(rows) {
        store.push_row(rev, row.to_vec());
    }
    let first_four = doc_of(&rows[..4]);
    let bytes = shifted_full(first_four.inner());
    assert!(may_be_shifted(&bytes));
    store.push_snapshot(snapshot(Some(5), 5, bytes, None, false));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("a fallback");
    assert_eq!((f.reason, f.whole_log, f.refused), ("legacy", true, 0));
    assert!(boot.unlogged);
    assert!(within(&held(boot.doc.inner()), &insertions_of(&rows[..4])));
}

#[tokio::test]
async fn a_refused_row_over_a_flagged_base_is_not_skipped() {
    let (state, resend, _) = skip_underflow_case();
    let store = MemStore::new();
    store.push_row(1, state.clone());
    store.push_row(2, resend);
    store.push_snapshot(snapshot(Some(2), 2, state, None, true));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("a fallback");
    assert_eq!((f.reason, f.whole_log, f.refused), ("rollout", true, 0));
    assert!(boot.unlogged);
}

#[tokio::test]
async fn a_page_without_a_snapshot_replays_its_log_through_the_guards() {
    let (state, resend, _) = skip_underflow_case();
    let store = MemStore::new();
    store.push_row(1, state);
    store.push_row(2, resend);
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("a fallback");
    assert_eq!((f.reason, f.whole_log, f.refused), ("underflow", true, 0));
    assert!(boot.unlogged);

    let (store, _) = reparking_page();
    let store = {
        let rows = store.rows();
        let fresh = MemStore::new();
        for r in rows {
            fresh.push_row(r.rev.unwrap(), r.update_bytes);
        }
        fresh
    };
    let boot = room_boot(&store).await;
    assert_eq!(boot.fallback, None);
    assert!(!boot.unlogged);
    assert_eq!(store.reads(), vec![RowsRead::After(RowsAfter::All)]);
}

/// Ruling 8's order: the kept rows after the rows after the snapshot. The
/// snapshot holds `paras` and names `t2_only`; the row after it is the GC
/// resend `.1`. Applied first, the kept row would integrate T2 past the gap
/// 2#0..2#2 and the resend would then meet the underflow; applied last, it
/// is trimmed by the resend's GC block.
///
/// The brief's live model (T2 still pending after `paras`) is not reachable
/// with these bytes: T2 parks with `missing = {1: 0}`, and `paras` moves
/// client 1's clock past 0, so yrs retries and T2 drains. The test pins the
/// boot order on the store alone, and the counterfactual order on a guarded
/// doc.
#[tokio::test]
async fn a_kept_row_is_applied_after_the_rows_after_the_snapshot() {
    let (paras, t2_only) = skip_underflow_parts();
    let (state, resend, _) = skip_underflow_case();
    let live = live_of(&[&t2_only, &paras]);
    assert!(
        pending::claims(&live).is_none(),
        "T2 drains live when paras arrives (the brief's live model is unreachable)"
    );
    assert_same_doc(&live, &doc_of(&[&state]), "the live doc holds .0");

    let mut kept_first = pending::GuardedDoc::new(doc_of(&[&paras]));
    assert_eq!(kept_first.apply(&t2_only), Ok(()));
    assert_eq!(
        kept_first.apply(&resend),
        Err(pending::Refused::Underflow),
        "kept rows first meet the underflow"
    );

    let store = MemStore::new();
    store.push_row(1, t2_only.clone());
    store.push_row(2, paras.clone());
    store.push_row(3, resend.clone());
    store.push_snapshot(snapshot(Some(2), 2, paras.clone(), Some(vec![1]), false));
    let boot = room_boot(&store).await;
    assert_eq!(boot.fallback, None);
    assert!(!boot.unlogged);
    assert_eq!(
        store.reads(),
        vec![RowsRead::After(RowsAfter::Rev(2)), RowsRead::At(vec![1])]
    );
    let reference = doc_of(&[&paras, &resend, &t2_only]);
    assert_same_doc(&boot.doc, &reference, "the snapshot, row 3, then row 1");
    let again = room_boot(&store).await;
    assert_same_doc(&again.doc, &boot.doc, "a second boot");
    assert!(!again.unlogged);
    let read_only = boot_in(&store, BootMode::ReadOnly).await.expect("loads");
    assert_eq!(text(&read_only.doc), text(&boot.doc));
}

#[tokio::test]
async fn a_row_after_the_snapshot_that_the_guards_refuse_falls_back() {
    let (state, resend, _) = skip_underflow_case();
    let store = MemStore::new();
    store.push_row(1, state.clone());
    store.push_row(2, resend);
    store.push_snapshot(snapshot(Some(1), 1, state, Some(vec![]), false));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("a fallback");
    assert_eq!((f.reason, f.whole_log, f.refused), ("underflow", true, 0));
    assert!(boot.unlogged);
}

/// Ruling 8's `missing_row`: a pre-0.9.9 page's GC took the row that parked
/// t3's "qq", and a row was deleted by hand.
#[tokio::test]
async fn a_named_row_the_log_lacks_is_skipped_and_the_rows_after_the_snapshot_load() {
    let o = overlapping_skips();
    let edit = edit_inside_base(3, &[&o.t0], "EDIT");
    let after = para(4, "after");
    let store = MemStore::new();
    store.push_row(2, edit.clone());
    store.push_row(3, after);
    let base = doc_of(&[&o.t0, &o.t3, &edit]);
    store.push_snapshot(snapshot(
        Some(2),
        2,
        pending_free(base.inner()),
        Some(vec![1]),
        false,
    ));
    let boot = room_boot(&store).await;
    assert_eq!(fell_back(&boot), Some(("missing_row", false, 0, true)));
    assert_eq!(
        store.reads(),
        vec![RowsRead::After(RowsAfter::Rev(2)), RowsRead::At(vec![1])]
    );
    assert_eq!(boot.through_rev, 3);
    let page = text(&boot.doc);
    for word in ["ba", "new", "EDIT", "after"] {
        assert!(page.contains(word), "{word}: {page}");
    }
    let read_only = boot_in(&store, BootMode::ReadOnly).await.expect("loads");
    assert_eq!(text(&read_only.doc), page);
}

/// Task 1 Step 6: `may_be_shifted` flags every state that names a block
/// past a gap, real text past a lost edit included, so this legacy snapshot
/// boots from its trusted prefix and, with no row carrying t3, loses "new"
/// (the plan's `..._and_nothing_pending_keeps_it`, whose expectation that
/// fallback changed).
#[tokio::test]
async fn a_legacy_snapshot_with_text_past_a_gap_and_nothing_pending_boots_from_its_prefix() {
    let o = overlapping_skips();
    let gapped = doc_of(&[&o.t0, &o.t3]);
    let state = pending_free(gapped.inner());
    assert!(!parks_when_applied(&state), "fixture: nothing pending");
    let store = MemStore::new();
    store.push_snapshot(snapshot(None, 1, state, None, false));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("a fallback");
    assert_eq!((f.reason, f.whole_log), ("legacy", true));
    assert_eq!(f.dropped, 0);
    assert!(f.dropped_past_gap > 0, "\"new\" is past the gap");
    let page = text(&boot.doc);
    assert!(page.contains("base") && !page.contains("new"), "{page}");
}

#[tokio::test]
async fn an_untrusted_snapshot_past_a_gap_reports_what_it_dropped() {
    let o = overlapping_skips();
    let store = MemStore::new();
    let shifted = shifted_full(doc_of(&[&o.t0, &o.t2, &o.t3]).inner());
    store.push_snapshot(snapshot(Some(3), 3, shifted, None, false));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("a fallback");
    assert_eq!((f.reason, f.whole_log), ("legacy", true));
    assert_eq!(f.dropped, 0);
    assert!(f.dropped_past_gap > 0);
    assert!(within(
        &held(boot.doc.inner()),
        &insertions_of(&[&o.t0, &o.t2, &o.t3])
    ));
    assert!(text(&boot.doc).contains("base"));
}

/// Ruling 7's round-4 ruling: bytes that park content but name no block
/// past a gap boot from the rows after them.
#[tokio::test]
async fn a_stamped_legacy_snapshot_that_parks_without_shift_reads_only_the_rows_after_it() {
    let (state, resend, _) = skip_underflow_case();
    let (_, u2) = cross_client_parked();
    let store = MemStore::new();
    store.push_row(1, state);
    store.push_row(2, resend);
    store.push_row(3, u2.clone());
    store.push_row(4, para(4, "four"));
    let bytes = shifted_full(doc_of(&[&u2, &para(3, "three")]).inner());
    assert!(parks_when_applied(&bytes) && !may_be_shifted(&bytes));
    store.push_snapshot(snapshot(Some(3), 3, bytes, None, false));
    let boot = room_boot(&store).await;
    assert_eq!(boot.fallback, None);
    assert!(!boot.unlogged);
    assert_eq!(store.reads(), vec![RowsRead::After(RowsAfter::Rev(3))]);
    let claims = pending::claims(&boot.doc).expect("u2 pending");
    assert!(same_ids(&claims.structs, &insertions(&u2)));
    assert!(text(&boot.doc).contains("four"));
    assert!(!boot.carry.is_empty(), "no applied row covers u2");
    let read_only = boot_in(&store, BootMode::ReadOnly).await.expect("loads");
    assert_eq!(text(&read_only.doc), text(&boot.doc));
}

/// Ruling 6: an older pod's snapshot is replaced by the newest trusted one.
#[tokio::test]
async fn a_flagged_snapshot_boots_from_the_newest_trusted_one_and_keeps_its_text() {
    let o = overlapping_skips();
    let s1_doc = doc_of(&[&o.t0, &o.t3]);
    let s1 = pending_free(s1_doc.inner());
    let edit = edit_inside_base(3, &[&s1], "EDIT");
    let s2 = pending_free(doc_of(&[&s1, &edit]).inner());
    let rows = || {
        let store = MemStore::new();
        store.push_row(1, o.t3.clone());
        store.push_row(2, edit.clone());
        store.push_snapshot(snapshot(Some(2), 2, s2.clone(), None, true));
        store
    };
    let reference = doc_of(&[&o.t0, &o.t3, &edit]);

    let store = rows();
    store.push_snapshot(snapshot(Some(1), 1, s1.clone(), Some(vec![1]), false));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log), ("rollout", false));
    let page = text(&boot.doc);
    for word in ["ba", "se", "new", "EDIT"] {
        assert!(page.contains(word), "{word}: {page}");
    }
    assert_eq!(pending::claims(&boot.doc), pending::claims(&reference));
    assert_eq!(
        store.reads(),
        vec![RowsRead::After(RowsAfter::Rev(1)), RowsRead::At(vec![1])]
    );

    // 0.9.13's GC deleted S1: the whole log over S2.
    let store = rows();
    let boot = room_boot(&store).await;
    assert_eq!(fell_back(&boot), Some(("rollout", true, 0, true)));
    let page = text(&boot.doc);
    for word in ["ba", "se", "new", "EDIT"] {
        assert!(page.contains(word), "{word}: {page}");
    }
    assert_eq!(
        pending::claims(&boot.doc),
        pending::claims(&reference),
        "\"qq\" pending again, from row 1"
    );
}

/// A client-3 doc's paragraph "one" and an edit inside it, as rows 1 and 2.
fn one_and_its_edit() -> (Vec<u8>, Vec<u8>) {
    let doc = Doc::with_client_id(3);
    let one = paragraph_by(&doc, "one");
    let frag = doc.get_or_insert_xml_fragment("default");
    let edit = own_update(&doc, |txn| {
        let text = fixtures::text_at(&frag, txn, 0).expect("the paragraph");
        text.insert(txn, 1, "EDIT");
    });
    (one, edit)
}

#[tokio::test]
async fn a_flagged_snapshot_over_a_pre_migration_snapshot_boots_from_it() {
    let (one, edit) = one_and_its_edit();
    let store = MemStore::new();
    store.push_row(1, one.clone());
    store.push_row(2, edit.clone());
    store.push_snapshot(snapshot(
        Some(1),
        1,
        shifted_full(doc_of(&[&one]).inner()),
        None,
        false,
    ));
    store.push_snapshot(snapshot(
        Some(2),
        2,
        shifted_full(doc_of(&[&one, &edit]).inner()),
        None,
        true,
    ));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log), ("rollout", false));
    assert_eq!(store.reads(), vec![RowsRead::After(RowsAfter::Rev(1))]);
    assert!(boot.parked.is_empty() && boot.carry.is_empty());
    assert_eq!(text(&boot.doc), text(&doc_of(&[&one, &edit])));
}

#[tokio::test]
async fn a_flagged_snapshot_over_a_pre_migration_snapshot_with_pending_content_carries_it() {
    let (_, u2) = cross_client_parked();
    let (one, edit) = one_and_its_edit();
    let store = MemStore::new();
    store.push_row(1, one.clone());
    store.push_row(2, edit.clone());
    let t = shifted_full(doc_of(&[&u2, &one]).inner());
    assert!(parks_when_applied(&t) && !may_be_shifted(&t));
    store.push_snapshot(snapshot(Some(1), 1, t, None, false));
    store.push_snapshot(snapshot(
        Some(2),
        2,
        shifted_full(doc_of(&[&one, &edit]).inner()),
        None,
        true,
    ));
    let boot = room_boot(&store).await;
    assert_eq!(fell_back(&boot), Some(("rollout", false, 0, true)));
    assert_eq!(store.reads(), vec![RowsRead::After(RowsAfter::Rev(1))]);
    assert!(!boot.carry.is_empty());
    let claims = pending::claims(&boot.doc).expect("u2 pending");
    assert!(same_ids(&claims.structs, &insertions(&u2)));
    assert_eq!(text(&boot.doc), text(&doc_of(&[&one, &edit])));
}

#[tokio::test]
async fn a_flagged_snapshot_falls_back_from_the_trusted_bytes() {
    let o = overlapping_skips();
    let edit = edit_inside_base(3, &[&o.t0], "EDIT");
    let three = para(4, "three");
    let store = MemStore::new();
    store.push_row(2, edit.clone());
    store.push_row(3, three.clone());
    let t = pending_free(doc_of(&[&o.t0, &edit]).inner());
    store.push_snapshot(snapshot(Some(2), 2, t.clone(), Some(vec![1]), false));
    let f_state = shifted_full(doc_of(&[&t, &three, &para(7, "only flagged")]).inner());
    store.push_snapshot(snapshot(Some(3), 3, f_state, None, true));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log), ("rollout", false));
    assert_eq!(
        store.reads(),
        vec![RowsRead::After(RowsAfter::Rev(2)), RowsRead::At(vec![1])]
    );
    let page = text(&boot.doc);
    for word in ["ba", "EDIT", "three"] {
        assert!(page.contains(word), "{word}: {page}");
    }
    assert!(!page.contains("only flagged"), "{page}");
}

/// Ruling 19: rule 2 with no trusted row, then rule 4 over the flagged
/// row's own bytes.
#[tokio::test]
async fn a_flagged_unshifted_snapshot_with_uncovered_pending_content_carries_it() {
    let (_, u2) = cross_client_parked();
    let one = para(3, "one");
    let store = MemStore::new();
    store.push_row(1, one.clone());
    let f_state = shifted_full(doc_of(&[&u2, &one]).inner());
    assert!(parks_when_applied(&f_state) && !may_be_shifted(&f_state));
    store.push_snapshot(snapshot(Some(1), 1, f_state, None, true));
    let boot = room_boot(&store).await;
    assert_eq!(fell_back(&boot), Some(("rollout", true, 0, true)));
    assert!(!boot.carry.is_empty());
    let claims = pending::claims(&boot.doc).expect("u2 pending");
    assert!(same_ids(&claims.structs, &insertions(&u2)));
    let read_only = boot_in(&store, BootMode::ReadOnly).await.expect("loads");
    assert_same_doc(&read_only.doc, &boot.doc, "read-only vs room");
    assert!(read_only.carry.is_empty());
}

/// The rows and the flagged snapshot of a 0.9.13 room open across the
/// migration on a page whose pending update holds overlapping Skips.
fn shifted_rollout_page() -> (MemStore, DocHandle, Vec<Vec<u8>>, Vec<u8>) {
    let o = overlapping_skips();
    let edit = edit_inside_base(3, &[&o.t0], "EDIT");
    let rows = vec![o.t0.clone(), o.t2.clone(), o.t3.clone(), edit];
    let store = MemStore::new();
    for (rev, row) in (1..).zip(&rows) {
        store.push_row(rev, row.clone());
    }
    let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    let live = live_of(&refs);
    store.push_snapshot(snapshot(Some(4), 4, shifted_full(live.inner()), None, true));
    let t = shifted_full(doc_of(&refs[..3]).inner());
    assert!(may_be_shifted(&t), "fixture: T may be shifted");
    (store, live, rows, t)
}

async fn assert_boots_from_a_prefix(store: &MemStore, live: &DocHandle, rows: &[Vec<u8>]) {
    let boot = room_boot(store).await;
    let f = boot.fallback.clone().expect("rollout");
    assert_eq!((f.reason, f.whole_log, f.refused), ("rollout", true, 0));
    assert_eq!(store.reads(), vec![RowsRead::After(RowsAfter::All)]);
    let refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    assert!(
        within(&held(boot.doc.inner()), &insertions_of(&refs)),
        "a phantom"
    );
    assert!(boot.carry.is_empty(), "a prefix base never carries");
    assert_eq!(pending::claims(&boot.doc), pending::claims(live));
    let page = text(&boot.doc);
    assert!(page.contains("ba") && page.contains("EDIT"), "{page}");
    assert_eq!(boot.through_rev, 4);
    let read_only = boot_in(store, BootMode::ReadOnly).await.expect("loads");
    assert_eq!(text(&read_only.doc), page);
}

#[tokio::test]
async fn a_flagged_snapshot_over_a_shifted_pre_migration_snapshot_boots_from_its_prefix() {
    let (store, live, rows, t) = shifted_rollout_page();
    store.push_snapshot(snapshot(Some(3), 3, t, None, false));
    assert_boots_from_a_prefix(&store, &live, &rows).await;
}

#[tokio::test]
async fn a_shifted_flagged_snapshot_without_a_trusted_one_boots_from_its_prefix() {
    let (store, live, rows, _) = shifted_rollout_page();
    assert_boots_from_a_prefix(&store, &live, &rows).await;
}

/// One user per author in the history checks.
fn user(n: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(n)
}

/// `knot_history::run_pass` over `rows` (`(rev, bytes, author)`, a minute
/// apart) and one probe per `(rev, state)` snapshot, from an empty page's
/// genesis at rev 0 (`knot-history/tests/pass.rs::input`).
fn history_pass(
    rows: &[(i64, Vec<u8>, Option<uuid::Uuid>)],
    snapshots: &[(i64, Vec<u8>)],
) -> knot_history::PassOutput {
    let minute = |n: i64| {
        let mut row = knot_storage::DocUpdate {
            seq: 0,
            doc_id: uuid::Uuid::nil(),
            update_bytes: Vec::new(),
            by_user_id: None,
            created_at: Default::default(),
            rev: None,
            source: None,
            meta: None,
            epoch: None,
        };
        row.created_at += std::time::Duration::from_secs(60 * n.max(0) as u64);
        row.created_at
    };
    let rows: Vec<knot_storage::DocUpdate> = rows
        .iter()
        .map(|(rev, bytes, by)| knot_storage::DocUpdate {
            seq: *rev,
            doc_id: uuid::Uuid::nil(),
            update_bytes: bytes.clone(),
            by_user_id: *by,
            created_at: minute(*rev),
            rev: Some(*rev),
            source: Some(knot_storage::UpdateSource::Edit),
            meta: None,
            epoch: Some(0),
        })
        .collect();
    let snapshots = snapshots
        .iter()
        .map(|(rev, state)| knot_history::SnapshotProbe {
            rev: *rev,
            created_at: minute(*rev),
            state_bytes: Some(state.clone()),
        })
        .collect();
    let last = rows.last().map_or(0, |r| r.rev.unwrap_or(0));
    knot_history::run_pass(knot_history::PassInput {
        checkpoint_rev: 0,
        checkpoint_state: knot_history::Replayer::new(knot_history::ReplayMode::Gc).encode_state(),
        reapply: Vec::new(),
        replay: Vec::new(),
        genesis_rev: 0,
        built_rev: 0,
        rows,
        rows_exhausted: true,
        open: Vec::new(),
        claims: knot_history::Claims::default(),
        kept_gaps: Vec::new(),
        snapshots,
        snapshot_checked_rev: 0,
        skip_revs: std::collections::BTreeSet::new(),
        progress: knot_history::Progress::default(),
        now: minute(last + 24 * 60),
        rows_budget: None,
        checkpoint_floor: 256,
        hooks: knot_history::ReplayHooks::default(),
    })
    .expect("the pass")
}

/// Whether a pass found gap content: a gap fed, a gap entry, or a snapshot
/// left unchecked.
fn builder_gaps(out: &knot_history::PassOutput) -> Vec<String> {
    let mut found = Vec::new();
    if out.stats.gaps > 0 {
        found.push(format!("{} gap(s) fed", out.stats.gaps));
    }
    if out
        .entries
        .iter()
        .any(|e| e.kind == knot_history::EntryKind::Gap)
    {
        found.push("a gap entry".into());
    }
    if !out.stats.snapshots_unchecked.is_empty() {
        found.push(format!(
            "snapshots unchecked: {:?}",
            out.stats.snapshots_unchecked
        ));
    }
    found
}

/// Rulings 11 and 17: content a live room keeps stuck (yrs 0.27.3 does not
/// retry the overlapping-Skips pending update when `t1` and `t2b` arrive
/// late) integrates at boot, when the kept rows re-apply over a snapshot
/// that holds their dependencies; the guards pass it, it stays inside the
/// reboot bound, and the builder credits it to the row that parked it.
///
/// The brief built this from `skip_underflow_parts` (T2 parked, then
/// `paras`), but T2 drains live there (see
/// `a_kept_row_is_applied_after_the_rows_after_the_snapshot`); Task 1's
/// overlapping-Skips fixture is the deterministic stuck shape.
#[tokio::test]
async fn a_kept_row_that_integrates_stuck_content_at_boot_passes_the_guards() {
    let o = overlapping_skips();
    let (a, b) = (Some(user(1)), Some(user(2)));
    let mut rows: Vec<(i64, Vec<u8>, Option<uuid::Uuid>)> = vec![
        (1, o.t0.clone(), b),
        (2, o.t2.clone(), a),
        (3, o.t3.clone(), a),
        (4, o.t1.clone(), b),
        (5, o.t2b.clone(), b),
    ];
    let refs: Vec<&[u8]> = rows.iter().map(|(_, r, _)| r.as_slice()).collect();
    let live = live_of(&refs);
    let pp = insertions(&o.t2);
    let claims = pending::claims(&live).expect("fixture: pp and qq stay stuck live");
    assert!(within(&pp, &claims.structs), "fixture: pp is stuck");
    let s1 = pending_free(live.inner());
    let fresh = doc_of(&[&s1, &o.t2, &o.t3]);
    assert!(
        !holds_pending(fresh.inner()) && within(&pp, &held(fresh.inner())),
        "fixture: a fresh doc integrates the kept rows"
    );
    assert_eq!(
        pending::check_reparks(&YrsEngine, &s1, &[(2, &o.t2), (3, &o.t3)], &claims),
        Ok(())
    );

    let store = MemStore::new();
    for (rev, bytes, _) in &rows {
        store.push_row(*rev, bytes.clone());
    }
    store.push_snapshot(snapshot(Some(5), 5, s1.clone(), Some(vec![2, 3]), false));
    let boot = room_boot(&store).await;
    assert_eq!(boot.fallback, None);
    let violations = reboot_violations(boot.doc.inner(), live.inner());
    assert!(violations.is_empty(), "{violations:#?}");
    assert!(
        pending::claims(&boot.doc).is_none(),
        "nothing pending at boot"
    );
    assert!(within(&pp, &held(boot.doc.inner())));

    let later = para(3, "later");
    YrsEngine.apply_update(&boot.doc, &later).unwrap();
    let s2 = pending_free(boot.doc.inner());
    rows.push((6, later, b));
    let out = history_pass(&rows, &[(5, s1), (6, s2)]);
    let gaps = builder_gaps(&out);
    assert!(gaps.is_empty(), "{gaps:?}");
    assert!(
        out.entries
            .iter()
            .any(|e| e.author == a && holds_any(&e.inserted.intersect(&pp))),
        "pp is credited to its author: {:#?}",
        out.entries
    );
}

/// Ruling 8's closing retry: a row after the snapshot (R') waits on a kept
/// row's content, which integrates only when the kept rows apply last.
#[tokio::test]
async fn a_row_after_the_snapshot_that_waits_on_a_kept_row_drains_at_boot() {
    let o = overlapping_skips();
    let all = client_doc(9, &[&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3]);
    let frag = all.get_or_insert_xml_fragment("default");
    let r_prime = own_update(&all, |txn| {
        let text = fixtures::text_at(&frag, txn, 0).expect("the first paragraph");
        // "zzbaseWqqWWpp": between t2's two p's.
        text.insert(txn, 12, "R");
    });
    let rows: Vec<Vec<u8>> = vec![
        o.t0.clone(),
        o.t2.clone(),
        o.t3.clone(),
        r_prime.clone(),
        o.t1.clone(),
        o.t2b.clone(),
    ];
    let live = YrsEngine.new_doc();
    let mut snapshot_state = None;
    for (rev, row) in (1..).zip(&rows) {
        YrsEngine
            .apply_update_measuring(&live, row, MAX_NESTING)
            .expect("the measured apply");
        if rev == 3 {
            snapshot_state = Some(pending_free(live.inner()));
        }
        if rev == 4 {
            let claims = pending::claims(&live).expect("R' parks");
            assert!(
                within(&insertions(&r_prime), &claims.structs),
                "fixture: R' parks on the live doc"
            );
        }
    }
    // The brief expected the live doc to drain here. yrs 0.27.3 keeps the
    // overlapping-Skips content stuck when `t1` and `t2b` arrive late
    // (Task 1, Concern 2), and R' with it: the live room shows them only
    // after a reboot (Ruling 11). Its settled copy (yrs's retry forced, as
    // boot's closing retry forces it) is the content the reboot must show.
    let stuck = pending::claims(&live).expect("pp, qq and R' stay stuck live");
    assert!(within(&insertions(&r_prime), &stuck.structs));
    let settled = doc_of(&[&pending_free(live.inner())]);
    for row in &rows {
        YrsEngine.apply_update(&settled, row).unwrap();
    }
    settle(settled.inner());
    assert!(
        pending::claims(&settled).is_none(),
        "fixture: the settled live doc holds nothing pending"
    );
    let store = MemStore::new();
    for (rev, row) in (1..).zip(&rows) {
        store.push_row(rev, row.clone());
    }
    store.push_snapshot(snapshot(
        Some(3),
        3,
        snapshot_state.unwrap(),
        Some(vec![2, 3]),
        false,
    ));
    let boot = room_boot(&store).await;
    assert_eq!(boot.fallback, None);
    assert_eq!(
        store.reads(),
        vec![RowsRead::After(RowsAfter::Rev(3)), RowsRead::At(vec![2, 3])]
    );
    assert!(pending::claims(&boot.doc).is_none());
    let violations = reboot_violations(boot.doc.inner(), live.inner());
    assert!(violations.is_empty(), "{violations:#?}");
    assert_eq!(text(&boot.doc), text(&settled));
    let read_only = boot_in(&store, BootMode::ReadOnly).await.expect("loads");
    assert_eq!(text(&read_only.doc), text(&settled));
}

/// Rule 4 over a stamped base steps over a missing rev at or below the
/// stamp (Ruling 8).
#[tokio::test]
async fn a_whole_log_fallback_over_a_stamped_snapshot_steps_over_a_missing_rev_below_it() {
    let (state, resend, _) = skip_underflow_case();
    assert!(!parks_when_applied(&state));
    let store = MemStore::new();
    store.push_row(2, resend);
    store.push_row(3, state.clone());
    store.push_row(4, para(3, "four"));
    store.push_row(5, para(4, "five"));
    store.push_snapshot(snapshot(Some(3), 3, state, Some(vec![2]), false));
    let boot = room_boot(&store).await;
    let f = boot.fallback.clone().expect("a fallback");
    assert_eq!((f.reason, f.whole_log), ("underflow", true));
    assert!(boot.unlogged);
    assert_eq!(boot.through_rev, 5);
    let page = text(&boot.doc);
    assert!(page.contains("four") && page.contains("five"), "{page}");
    assert!(matches!(
        boot_in(&store, BootMode::ReadOnly).await,
        Err(BootError::Apply(_))
    ));
}

/// Ruling 8's per-client post-check on a whole-log boot (rule 0): 5,000
/// rows that each name a client holding blocks past a gap, over a doc of
/// 100,000 blocks.
#[tokio::test]
async fn a_whole_log_boot_of_5000_past_gap_rows_stays_bounded() {
    let bulk = {
        let doc = Doc::with_client_id(40);
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        let text = p.push_back(&mut txn, XmlTextPrelim::new(""));
        for _ in 0..100_000 {
            text.insert(&mut txn, 0, "b");
        }
        drop(txn);
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    };
    assert!(bulk.len() > 500_000, "fixture: {} bytes", bulk.len());
    eprintln!("bulk: {} bytes", bulk.len());
    let o = overlapping_skips();
    let typist = client_doc(1, &[&o.t0, &o.t1, &o.t2, &o.t2b, &o.t3]);
    let frag = typist.get_or_insert_xml_fragment("default");
    let typed: Vec<Vec<u8>> = (0..5_000)
        .map(|i| {
            own_update(&typist, |txn| {
                let text = fixtures::text_at(&frag, txn, 1).expect("t3's paragraph");
                let end = text.len(txn);
                text.insert(txn, end, if i % 2 == 0 { "x" } else { "y" });
            })
        })
        .collect();

    let mut rows: Vec<&[u8]> = vec![&bulk, &o.t0, &o.t3];
    rows.extend(typed.iter().map(Vec::as_slice));
    let live = YrsEngine.new_doc();
    let mut typed_ids = IdSet::new();
    for (i, row) in rows.iter().enumerate() {
        YrsEngine
            .apply_update_measuring(&live, row, MAX_NESTING)
            .expect("the measured apply");
        if i >= 3 {
            let ins = insertions(row);
            assert!(holds_any(&ins), "fixture: typed row {i} inserts");
            typed_ids.merge_with(ins);
        }
    }
    // Held once all are applied: a block integrates once and stays (one
    // encode of the doc instead of one per row).
    assert!(
        within(&typed_ids, &held(live.inner())),
        "fixture: the typed rows integrate past client 1's gap"
    );
    eprintln!("fixture built");
    let store = MemStore::new();
    for (rev, row) in (1..).zip(&rows) {
        store.push_row(rev, row.to_vec());
    }
    let started = std::time::Instant::now();
    let boot = room_boot(&store).await;
    let took = started.elapsed();
    eprintln!("whole-log boot of {} rows: {took:?}", rows.len());
    assert_eq!(boot.fallback, None);
    assert!(!boot.unlogged);
    let page = text(&boot.doc);
    assert_eq!(page.matches(['x', 'y']).count(), 5_000);
    assert!(took < std::time::Duration::from_secs(60), "{took:?}");
}

// ---------------------------------------------------------------------------
// The reboot property
// ---------------------------------------------------------------------------

/// Seeds the reboot property runs, and steps per seed.
const REBOOT_SEEDS: u64 = 200;
const REBOOT_STEPS: usize = 60;

/// How often a relation was violated, and the first seed, step and message.
type Violations = (usize, Option<(u64, usize, String)>);

/// What the reboot property found.
#[derive(Default)]
struct RebootTally {
    /// Per relation.
    violations: std::collections::BTreeMap<&'static str, Violations>,
    snapshots: usize,
    snapshot_boots: usize,
    after_rows_boots: usize,
    after_rows_with_pending_revs: usize,
    retry_changed: usize,
    first_retry_changed: Option<(u64, usize)>,
    generations: usize,
    deepest: u32,
    stuck_integrated: usize,
    first_stuck: Option<(u64, usize)>,
    probes: usize,
    probes_by_booted: usize,
    probes_by_stuck: usize,
    joins: usize,
    join_frames_refused: usize,
    final_refused: usize,
    settled_seeds: usize,
    builder_runs: usize,
}

impl RebootTally {
    fn violate(&mut self, what: &'static str, seed: u64, step: usize, message: String) {
        let entry = self.violations.entry(what).or_default();
        entry.0 += 1;
        entry.1.get_or_insert((seed, step, message));
    }
}

/// The live room: its doc, the rows that park its pending content, how many
/// boots it descends from, and whether one of them integrated stuck content.
struct Model {
    live: DocHandle,
    parked: pending::ParkedRows,
    generation: u32,
    stuck: bool,
}

/// A written snapshot, for later boots and the builder's probes.
struct Written {
    rev: i64,
    state: Vec<u8>,
    revs: Vec<i64>,
    booted: bool,
    stuck: bool,
}

/// One seed of the reboot property.
struct Run<'t> {
    seed: u64,
    step: usize,
    rng: Rng,
    store: MemStore,
    next_rev: i64,
    authors: std::collections::BTreeMap<i64, Option<uuid::Uuid>>,
    model: Model,
    /// Own rows handed to the writer and not acknowledged: their tokens,
    /// bytes and authors.
    queue: std::collections::VecDeque<(Option<pending::RowToken>, Vec<u8>, Option<uuid::Uuid>)>,
    joined: Vec<pending::GuardedDoc>,
    written: Vec<Written>,
    t: &'t mut RebootTally,
}

impl Run<'_> {
    fn violate(&mut self, what: &'static str, message: String) {
        self.t.violate(what, self.seed, self.step, message);
    }

    /// A delivery from a client: the live room applies it, and a changed
    /// apply is a row, the room's own (handed to the writer) or another
    /// replica's (its rev known at once). Joined docs get the row.
    fn deliver(&mut self, bytes: &[u8], author: Option<uuid::Uuid>) {
        let m = YrsEngine
            .apply_update_measuring(&self.model.live, bytes, MAX_NESTING)
            .unwrap_or_else(|e| panic!("seed {}: measured apply: {e}", self.seed));
        if let Some(error) = m.rejected {
            self.violate("rejected", error);
            return;
        }
        if m.persist.is_empty() {
            return;
        }
        for joined in &mut self.joined {
            if joined.apply(&m.persist).is_err() {
                self.t.join_frames_refused += 1;
            }
        }
        let claims = pending::claims(&self.model.live);
        let records = m.pending_changed && claims.is_some();
        if self.rng.chance(70) {
            let token = if records {
                self.model.parked.record(None, m.persist.clone())
            } else {
                None
            };
            self.queue.push_back((token, m.persist, author));
        } else {
            let rev = self.push_row(m.persist.clone(), author);
            if records {
                self.model.parked.record(Some(rev), m.persist);
            }
        }
        if m.pending_changed {
            let _ = self.model.parked.prune(claims.as_ref());
        }
    }

    fn push_row(&mut self, bytes: Vec<u8>, author: Option<uuid::Uuid>) -> i64 {
        let rev = self.next_rev;
        self.next_rev += 1;
        self.store.push_row(rev, bytes);
        self.authors.insert(rev, author);
        rev
    }

    /// The writer acknowledges up to `n` queued rows, in order.
    fn ack(&mut self, n: usize) {
        for _ in 0..n {
            let Some((token, bytes, author)) = self.queue.pop_front() else {
                return;
            };
            let rev = self.push_row(bytes, author);
            if let Some(token) = token {
                self.model.parked.assign(token, rev);
            }
        }
    }

    fn last_rev(&self) -> i64 {
        self.next_rev - 1
    }

    /// A snapshot with the queue empty: the cover and the re-park check
    /// must hold, and a boot from it must reboot the live room.
    async fn snapshot(&mut self) {
        let claims = pending::claims(&self.model.live);
        if let Err(uncovered) = self.model.parked.prune(claims.as_ref()) {
            self.violate("cover", format!("{uncovered:?}"));
            return;
        }
        let state = pending_free(self.model.live.inner());
        if let Some(claims) = &claims {
            let rows = self.model.parked.checked_rows();
            if let Err(refusal) = pending::check_reparks(&YrsEngine, &state, &rows, claims) {
                self.violate("re-park check", format!("{refusal:?}"));
                return;
            }
        }
        let rev = self.last_rev();
        let revs = self.model.parked.revs();
        self.store.push_snapshot(snapshot(
            Some(rev),
            rev,
            state.clone(),
            Some(revs.clone()),
            false,
        ));
        self.written.push(Written {
            rev,
            state,
            revs,
            booted: self.model.generation > 0,
            stuck: self.model.stuck,
        });
        self.t.snapshots += 1;
        self.t.snapshot_boots += 1;
        let boot = room_boot(&self.store).await;
        self.check_boot(boot, "snapshot boot");
    }

    /// A boot from the latest snapshot and the rows after it, as a room
    /// that died without its exit snapshot.
    async fn boot_after_rows(&mut self) {
        let latest = self.written.last().expect("a snapshot");
        self.t.after_rows_boots += 1;
        if !latest.revs.is_empty() {
            self.t.after_rows_with_pending_revs += 1;
        }
        // Boot's own order, to see what its close changed.
        let mut replay = pending::GuardedDoc::new(doc_of(&[&latest.state]));
        let rows = self.store.rows();
        let mut applied: Vec<&knot_storage::DocUpdate> = Vec::new();
        for row in rows.iter().filter(|r| r.rev.unwrap() > latest.rev) {
            if replay.apply(&row.update_bytes).is_ok() {
                applied.push(row);
            }
        }
        for row in rows
            .iter()
            .filter(|r| latest.revs.contains(&r.rev.unwrap()))
        {
            if replay.apply(&row.update_bytes).is_ok() {
                applied.push(row);
            }
        }
        applied.sort_by_key(|r| r.rev);
        let before = pending::claims(replay.doc());
        let _ = replay.close(applied.iter().map(|r| r.update_bytes.as_slice()));
        if pending::claims(replay.doc()) != before {
            self.t.retry_changed += 1;
            self.t
                .first_retry_changed
                .get_or_insert((self.seed, self.step));
        }
        let boot = room_boot(&self.store).await;
        self.check_boot(boot, "boot after rows");
    }

    /// `fallback` is `None` and the boot reboots the live doc (Ruling 17);
    /// half the time the booted room becomes the live model.
    fn check_boot(&mut self, boot: Boot, what: &'static str) {
        if let Some(f) = &boot.fallback {
            self.violate("fallback", format!("{what}: {f:?}"));
        }
        if boot.unlogged {
            self.violate("unlogged", what.to_string());
        }
        for v in reboot_violations(boot.doc.inner(), self.model.live.inner()) {
            self.violate("reboot", format!("{what}: {v}"));
        }
        let (ps_boot, _) = pending_parts(boot.doc.inner());
        let (ps_live, _) = pending_parts(self.model.live.inner());
        let integrated = within(&ps_boot, &ps_live) && !within(&ps_live, &ps_boot);
        if integrated {
            self.t.stuck_integrated += 1;
            self.t.first_stuck.get_or_insert((self.seed, self.step));
        }
        if self.rng.chance(50) {
            let generation = self.model.generation + 1;
            self.model = Model {
                live: boot.doc,
                parked: boot.parked,
                generation,
                stuck: self.model.stuck || integrated,
            };
            self.t.generations += 1;
            self.t.deepest = self.t.deepest.max(generation);
        }
    }

    /// A client joins: the pending-free state, then the kept rows' frames.
    fn join(&mut self) {
        let mut joined =
            pending::GuardedDoc::new(doc_of(&[&pending_free(self.model.live.inner())]));
        for frame in self.model.parked.frames() {
            if joined.apply(&frame).is_err() {
                self.t.join_frames_refused += 1;
            }
        }
        self.joined.push(joined);
        self.t.joins += 1;
    }

    fn check_joined(&mut self) {
        let found: Vec<String> = self
            .joined
            .iter()
            .flat_map(|j| join_violations(j.doc().inner(), self.model.live.inner()))
            .collect();
        for v in found {
            self.violate("join bound", v);
        }
    }
}

/// The user a generator client's rows are credited to.
fn author_of(client: usize) -> Option<uuid::Uuid> {
    Some(user(100 + client as u128))
}

/// Ruling 17's reboot equality and join bound, through room generations:
/// a live room model (deliveries from the interleaving generator, its own
/// rows through a modelled writer queue, other replicas' rows with their
/// revs at once) writes pending-free snapshots that name its kept rows,
/// and every boot from them, or from them and the rows after, reboots it.
#[tokio::test]
async fn a_pending_free_snapshot_and_its_rows_reboot_the_room() {
    let mut t = RebootTally::default();
    for seed in 1..=REBOOT_SEEDS {
        let mut rng = Rng::new(seed);
        let mut clients: Vec<Client> = (1..=3).map(Client::new).collect();
        let fixed_step = rng.below(REBOOT_STEPS);
        let mut run = Run {
            seed,
            step: 0,
            rng: Rng::new(seed ^ 0x5eed),
            store: MemStore::new(),
            next_rev: 1,
            authors: Default::default(),
            model: Model {
                live: YrsEngine.new_doc(),
                parked: pending::ParkedRows::default(),
                generation: 0,
                stuck: false,
            },
            queue: Default::default(),
            joined: Vec::new(),
            written: Vec::new(),
            t: &mut t,
        };
        for step in 0..REBOOT_STEPS {
            run.step = step;
            let editor = rng.below(clients.len());
            if rng.chance(40) && !has_gaps(&run.model.live) {
                clients[editor].pull(&run.model.live, fixed_pull);
            }
            for _ in 0..1 + rng.below(3) {
                clients[editor].edit(&mut rng);
            }
            let c = rng.below(clients.len());
            let batch = if step == fixed_step {
                vec![delete_before_insert(&mut clients[c])]
            } else {
                deliveries(&mut clients[c], &run.model.live, &mut rng)
            };
            for bytes in batch {
                run.deliver(&bytes, author_of(c));
            }
            let acks = run.rng.below(4);
            run.ack(acks);
            if run.queue.is_empty() && run.last_rev() > 0 {
                if run.rng.chance(15) {
                    run.snapshot().await;
                } else if run.rng.chance(10)
                    && run.written.last().is_some_and(|w| w.rev < run.last_rev())
                {
                    run.boot_after_rows().await;
                }
            }
            if run.rng.chance(10) {
                run.join();
            }
            run.check_joined();
        }

        // The final phase: the queue drains, the latest snapshot boots, and
        // it and the live room get the same final deliveries.
        run.step = REBOOT_STEPS;
        run.ack(usize::MAX);
        let mut fin = pending::GuardedDoc::new(room_boot(&run.store).await.doc);
        let feed = |run: &mut Run, fin: &mut pending::GuardedDoc, bytes: &[u8], c: usize| {
            run.deliver(bytes, author_of(c));
            if fin.apply(bytes).is_err() {
                run.t.final_refused += 1;
            }
        };
        for (i, c) in clients.iter_mut().enumerate() {
            let own = c.take_undelivered();
            if !own.is_empty() {
                c.held
                    .push(yrs::merge_updates_v1(&own).expect("merge own updates"));
            }
            while !c.held.is_empty() {
                let at = rng.below(c.held.len());
                let bytes = c.held.remove(at);
                feed(&mut run, &mut fin, &bytes, i);
            }
        }
        if !has_gaps(&run.model.live) && !holds_blocks_past_a_gap(fin.doc().inner()) {
            for (i, c) in clients.iter_mut().enumerate() {
                c.take_undelivered();
                let sv = run.model.live.inner().transact().state_vector();
                let resend = c.doc.transact().encode_state_as_update_v1(&sv);
                feed(&mut run, &mut fin, &resend, i);
            }
            for (i, c) in clients.iter_mut().enumerate() {
                {
                    let mut txn = c.doc.transact_mut();
                    let at = c.frag.len(&txn);
                    let p = c
                        .frag
                        .insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
                    p.push_back(&mut txn, XmlTextPrelim::new(rng.word()));
                }
                for bytes in c.take_undelivered() {
                    feed(&mut run, &mut fin, &bytes, i);
                }
            }
            let live = run.model.live.inner();
            settle(live);
            settle(fin.doc().inner());
            let booted_differs = !same_content(live, fin.doc().inner());
            let differ = run
                .joined
                .iter()
                .filter(|j| {
                    settle(j.doc().inner());
                    !same_content(live, j.doc().inner())
                })
                .count();
            if booted_differs {
                run.violate("final equality", "the booted room".into());
            }
            if differ > 0 {
                run.violate("final equality", format!("{differ} joined doc(s)"));
            }
            run.t.settled_seeds += 1;
        }
        run.ack(usize::MAX);

        // The builder over the log and the fixed snapshots.
        let rows: Vec<(i64, Vec<u8>, Option<uuid::Uuid>)> = run
            .store
            .rows()
            .into_iter()
            .map(|r| {
                let rev = r.rev.unwrap();
                (rev, r.update_bytes, run.authors[&rev])
            })
            .collect();
        let mut probes: std::collections::BTreeMap<i64, Vec<u8>> = Default::default();
        for w in &run.written {
            probes.insert(w.rev, w.state.clone());
        }
        run.t.probes += probes.len();
        for rev in probes.keys() {
            let w = run.written.iter().rev().find(|w| w.rev == *rev).unwrap();
            run.t.probes_by_booted += usize::from(w.booted);
            run.t.probes_by_stuck += usize::from(w.stuck);
        }
        if !rows.is_empty() {
            let probes: Vec<(i64, Vec<u8>)> = probes.into_iter().collect();
            let out = history_pass(&rows, &probes);
            run.t.builder_runs += 1;
            for gap in builder_gaps(&out) {
                run.violate("builder", gap);
            }
        }
    }

    eprintln!(
        "snapshots {}, snapshot boots {}, boots after rows {} ({} with pending_revs, closing \
         retry changed {} {:?})",
        t.snapshots,
        t.snapshot_boots,
        t.after_rows_boots,
        t.after_rows_with_pending_revs,
        t.retry_changed,
        t.first_retry_changed
    );
    eprintln!(
        "generations {}, deepest {}; boots that integrated stuck content {} (first {:?})",
        t.generations, t.deepest, t.stuck_integrated, t.first_stuck
    );
    eprintln!(
        "probes {}, written by a booted room {}, by a room whose boot integrated stuck \
         content {}; builder runs {}",
        t.probes, t.probes_by_booted, t.probes_by_stuck, t.builder_runs
    );
    eprintln!(
        "joins {}, frames a joined doc's guards refused {}, final deliveries the booted \
         room refused {}, settled seeds {}",
        t.joins, t.join_frames_refused, t.final_refused, t.settled_seeds
    );
    for (what, (n, first)) in &t.violations {
        eprintln!("violations of {what}: {n}, first {first:?}");
    }
    assert!(
        t.violations.is_empty(),
        "violations: {:?}",
        t.violations.keys()
    );
    assert!(t.deepest >= 3, "no seed reached generation 3");
    assert!(
        t.after_rows_with_pending_revs > 0,
        "no boot after appended rows had a non-empty pending_revs"
    );
}

/// What a fixed room sends a pulling client: its pending-free state.
fn fixed_pull(d: &DocHandle, sv: &StateVector) -> Vec<u8> {
    encode_without_pending(&d.inner().transact(), sv)
}

/// Ruling 4's k: a cover of 3,000 keystroke rows parked behind one lost
/// update. Recording and pruning each row as the room does stays bounded,
/// and one re-park check, which the room actor runs inline on every
/// passing snapshot attempt, stays under 250 ms in a release build.
#[test]
fn a_cover_of_3000_keystroke_rows_stays_cheap() {
    use std::time::{Duration, Instant};
    let (_, u2, tail) = cross_client_parked_tail(3000);
    let live = YrsEngine.new_doc();
    let mut parked = pending::ParkedRows::default();
    let started = Instant::now();
    for (rev, row) in (1..).zip(std::iter::once(&u2).chain(&tail)) {
        let measured = YrsEngine
            .apply_update_measuring(&live, row, MAX_NESTING)
            .expect("the row applies");
        if measured.pending_changed {
            let claims = pending::claims(&live);
            if claims.is_some() {
                parked.record(Some(rev), row.clone());
            }
            let _ = parked.prune(claims.as_ref());
        }
    }
    let recording = started.elapsed();
    let claims = pending::claims(&live).expect("every row parks");
    let state = encode_without_pending(&live.inner().transact(), &StateVector::default());
    let started = Instant::now();
    let checked = pending::check_reparks(&YrsEngine, &state, &parked.checked_rows(), &claims);
    let check = started.elapsed();
    println!(
        "k = {}: recording {recording:?}, one check {check:?} ({} build)",
        parked.len(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    assert_eq!(parked.len(), 3001);
    assert!(checked.is_ok(), "{checked:?}");
    assert!(
        recording < Duration::from_secs(60),
        "recording took {recording:?}"
    );
    assert!(check < Duration::from_secs(20), "the check took {check:?}");
    if !cfg!(debug_assertions) {
        assert!(
            check < Duration::from_millis(250),
            "the check took {check:?}"
        );
    }
}
