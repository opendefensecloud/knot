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

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/interleave.rs"]
mod interleave;

use fixtures::{
    Rng, cross_client_parked, overlapping_skips, shifted_state_that_parks_nothing,
    shifted_state_yrs_refuses,
};
use interleave::{
    Client, delete_before_insert, deliveries, has_gaps, held, holds_blocks_past_a_gap, pending_ids,
};
use knot_crdt::nesting::MAX_NESTING;
use knot_crdt::pending::{
    PendingV1, delete_only_update, encode_without_pending, holds_any, may_be_shifted,
    names_blocks_past_a_gap, parks_when_applied, pending_update_v1, trusted_prefix,
};
use knot_crdt::{DocHandle, Engine, YrsEngine};
use yrs::block::ClientID;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, ID, IdSet, ReadTxn, StateVector, Transact, Update, WriteTxn, XmlElementPrelim,
    XmlFragment, XmlTextPrelim,
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
