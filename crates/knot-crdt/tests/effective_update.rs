//! The effective update (page history M1.2, architecture `attribution` →
//! TRUSTWORTHY LOG, open spike "Effective update equals the raw update").
//!
//! A room persists and fans out what an inbound apply changed
//! (`ApplyMeasured::persist`: the transaction's own `encode_update_v1`, taken
//! after commit) instead of the bytes a client sent, so a reconnect resend no
//! longer writes other people's items or the whole delete set into the log.
//! The raw bytes are persisted instead (`raw_fallback`) when the apply parked
//! or drained pending structs or deletes, which `encode_update_v1` omits, and
//! when `encode_update_v1` does not hold exactly the blocks the apply
//! integrated: yrs 0.27 integrates a block past a gap in its client's clock
//! (behind a Skip, parking nothing), which the state vector and so
//! `encode_update_v1` do not cover, and an apply that fills such a gap would
//! re-send the blocks past it. The property test found that case (seed 1).
//!
//! The property: for random three-client histories over one ProseMirror
//! fragment (live deliveries, reconnect resends and late, out-of-order
//! deliveries that park pending content or open gaps), a doc that applies
//! every raw update, the room's doc that applies them measured, and a doc
//! that replays only the persisted bytes always hold the same state, pending
//! content included.

#[path = "common/fixtures.rs"]
mod fixtures;
#[path = "common/interleave.rs"]
mod interleave;

use fixtures::Rng;
use interleave::{Client, delete_before_insert, deliveries, has_gaps};
use knot_crdt::nesting::MAX_NESTING;
use knot_crdt::{ApplyMeasured, DocHandle, Engine, YrsEngine};
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Any, GetString, ReadTxn, StateVector, Text, Transact, Update, XmlElementPrelim, XmlFragment,
    XmlTextPrelim,
};

/// Seeds the property runs, and steps per seed.
const SEEDS: u64 = 300;
const STEPS: usize = 60;

/// The three server-side docs: `raw` applies every delivered byte string,
/// `live` (the room's doc) applies it measured, and `log` applies only what
/// `live`'s apply says to persist.
struct Server {
    raw: DocHandle,
    live: DocHandle,
    log: DocHandle,
}

#[derive(Default)]
struct Counts {
    parked_structs: usize,
    parked_deletes_only: usize,
    /// Raw fallbacks that changed nothing pending: the apply integrated a
    /// block past a gap in its client's clock (an update of that client held
    /// back, which the block does not depend on), or filled such a gap.
    past_a_gap: usize,
    trimmed: usize,
    /// Seeds that ended with nothing pending in any doc.
    settled_seeds: usize,
    /// Seeds that ended with content yrs keeps parked behind a gap.
    stuck_seeds: usize,
}

/// The pending structs and pending delete set, each encoded.
fn pending(d: &DocHandle) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let txn = d.inner().transact();
    let store = txn.store();
    (
        store.pending_update().map(|p| p.update.encode_v1()),
        store.pending_ds().map(|ds| ds.encode_v1()),
    )
}

impl Server {
    fn new() -> Self {
        Server {
            raw: YrsEngine.new_doc(),
            live: YrsEngine.new_doc(),
            log: YrsEngine.new_doc(),
        }
    }

    fn deliver(&self, bytes: &[u8], seed: u64, counts: &mut Counts) -> ApplyMeasured {
        YrsEngine
            .apply_update(&self.raw, bytes)
            .unwrap_or_else(|e| panic!("seed {seed}: raw apply: {e}"));
        let before = pending(&self.live);
        let m = YrsEngine
            .apply_update_measuring(&self.live, bytes, MAX_NESTING)
            .unwrap_or_else(|e| panic!("seed {seed}: measured apply: {e}"));
        assert_eq!(m.rejected, None, "seed {seed}: yrs rejected an update");
        let after = pending(&self.live);
        let parked_structs = after.0.is_some() && after.0 != before.0;
        let parked_deletes = after.1.is_some() && after.1 != before.1;
        if parked_structs {
            counts.parked_structs += 1;
        } else if parked_deletes {
            counts.parked_deletes_only += 1;
        } else if m.raw_fallback && after == before {
            counts.past_a_gap += 1;
        }
        if parked_structs || parked_deletes {
            assert!(
                m.raw_fallback,
                "seed {seed}: parked content, no raw fallback"
            );
        }
        if m.raw_fallback {
            assert_eq!(
                m.persist, bytes,
                "seed {seed}: the fallback persists the raw bytes"
            );
        }
        if !m.changed {
            assert!(
                m.persist.is_empty(),
                "seed {seed}: an apply that changed nothing persists nothing"
            );
        }
        if !m.persist.is_empty() {
            if m.persist.len() < bytes.len() {
                counts.trimmed += 1;
            }
            YrsEngine
                .apply_update(&self.log, &m.persist)
                .unwrap_or_else(|e| panic!("seed {seed}: log apply: {e}"));
        }
        m
    }
}

/// Fails, naming the seed, unless `a` and `b` hold the same state: state
/// vector, delete set, the fragment's content, and the same pending structs
/// and pending deletes (not only whether either is set).
fn assert_same_state(a: &DocHandle, b: &DocHandle, seed: u64) {
    let state = |d: &DocHandle| {
        let frag = d.inner().get_or_insert_xml_fragment("default");
        let txn = d.inner().transact();
        (
            txn.state_vector(),
            txn.snapshot().delete_set.encode_v1(),
            frag.get_string(&txn),
        )
    };
    let (sv_a, ds_a, text_a) = state(a);
    let (sv_b, ds_b, text_b) = state(b);
    assert_eq!(sv_a, sv_b, "seed {seed}: state vectors differ");
    assert_eq!(ds_a, ds_b, "seed {seed}: delete sets differ");
    assert_eq!(text_a, text_b, "seed {seed}: content differs");
    let (pu_a, pd_a) = pending(a);
    let (pu_b, pd_b) = pending(b);
    assert_eq!(pu_a, pu_b, "seed {seed}: pending structs differ");
    assert_eq!(pd_a, pd_b, "seed {seed}: pending deletes differ");
}

/// yrs's own state encoding, pending content merged in: what rooms sent
/// clients before the fix, and what this property's histories were
/// generated with.
fn shifted(d: &DocHandle, sv: &StateVector) -> Vec<u8> {
    d.inner().transact().encode_state_as_update_v1(sv)
}

#[test]
fn effective_update_applies_like_the_raw_update() {
    let mut counts = Counts::default();
    for seed in 1..=SEEDS {
        let mut rng = Rng::new(seed);
        let mut clients: Vec<Client> = (1..=3).map(Client::new).collect();
        let server = Server::new();
        let fixed_step = rng.below(STEPS);
        for step in 0..STEPS {
            // Someone edits, sometimes after pulling what the server has.
            let editor = rng.below(clients.len());
            if rng.chance(40) && !has_gaps(&server.live) {
                clients[editor].pull(&server.live, shifted);
            }
            for _ in 0..1 + rng.below(3) {
                clients[editor].edit(&mut rng);
            }
            let c = rng.below(clients.len());
            if step == fixed_step {
                // U2 alone parks a delete set and no structs; U1 is held back
                // and arrives later.
                let u2 = delete_before_insert(&mut clients[c]);
                let before = counts.parked_deletes_only;
                server.deliver(&u2, seed, &mut counts);
                assert_eq!(
                    counts.parked_deletes_only,
                    before + 1,
                    "seed {seed}: U2 alone should park a delete set and no structs"
                );
            } else {
                for bytes in deliveries(&mut clients[c], &server.live, &mut rng) {
                    server.deliver(&bytes, seed, &mut counts);
                }
            }
            assert_same_state(&server.raw, &server.live, seed);
            assert_same_state(&server.live, &server.log, seed);
        }
        // Everything held back arrives, late and in random order, with what
        // each client has not sent yet (a held-back update can depend on it).
        for c in clients.iter_mut() {
            let own = c.take_undelivered();
            if !own.is_empty() {
                c.held
                    .push(yrs::merge_updates_v1(&own).expect("merge own updates"));
            }
            while !c.held.is_empty() {
                let i = rng.below(c.held.len());
                let bytes = c.held.remove(i);
                server.deliver(&bytes, seed, &mut counts);
            }
        }
        assert_same_state(&server.raw, &server.live, seed);
        assert_same_state(&server.live, &server.log, seed);
        if has_gaps(&server.live) {
            // Content is stuck behind a gap: yrs 0.27 retries pending
            // structs only once a clock it recorded as missing moves past
            // that record, which can be a clock no update reaches while the
            // parked block's own range is a gap, and neither a resend (see
            // `has_gaps`) nor a re-delivery of the pending structs (which
            // yrs re-encodes with wrong parent ids) is safe to apply. The
            // three docs hold the same pending content (asserted above).
            counts.stuck_seeds += 1;
            continue;
        }
        // Every client reconnects (a resend against the server's state
        // vector), then types a new paragraph: its clock moves past any
        // stale record, so yrs drops structs it still lists as pending that
        // are integrated by now.
        for c in clients.iter_mut() {
            c.take_undelivered();
            let sv = server.live.inner().transact().state_vector();
            let resend = c.doc.transact().encode_state_as_update_v1(&sv);
            server.deliver(&resend, seed, &mut counts);
        }
        for c in clients.iter_mut() {
            {
                let mut txn = c.doc.transact_mut();
                let at = c.frag.len(&txn);
                let p = c
                    .frag
                    .insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
                p.push_back(&mut txn, XmlTextPrelim::new(rng.word()));
            }
            for bytes in c.take_undelivered() {
                server.deliver(&bytes, seed, &mut counts);
            }
        }
        assert_same_state(&server.raw, &server.live, seed);
        assert_same_state(&server.live, &server.log, seed);
        for (name, d) in [
            ("raw", &server.raw),
            ("live", &server.live),
            ("log", &server.log),
        ] {
            assert_eq!(
                pending(d),
                (None, None),
                "seed {seed}: {name} still holds pending content"
            );
        }
        counts.settled_seeds += 1;
    }
    eprintln!(
        "parked structs {}, parked deletes alone {}, past a gap {}, trimmed {}; \
         seeds settled {}, stuck behind a gap {}",
        counts.parked_structs,
        counts.parked_deletes_only,
        counts.past_a_gap,
        counts.trimmed,
        counts.settled_seeds,
        counts.stuck_seeds
    );
    assert!(
        counts.parked_structs > 0,
        "the generator never parked structs"
    );
    assert!(
        counts.parked_deletes_only > 0,
        "the generator never parked a delete set alone"
    );
    assert!(
        counts.past_a_gap > 0,
        "the generator never had a block integrated past a gap"
    );
    assert!(counts.trimmed > 0, "no resend was ever trimmed");
    assert!(
        counts.settled_seeds > counts.stuck_seeds,
        "most seeds should settle with nothing pending"
    );
}

/// The case the property found (seed 1): yrs 0.27 integrates a block whose
/// own dependencies it has even when earlier blocks of the same client are
/// missing, behind a Skip, and parks nothing. The state vector stops at the
/// gap, so `encode_update_v1` (blocks from the transaction's before-state)
/// omits the block. The raw update is persisted instead. The update that
/// fills the gap is persisted raw too: its `encode_update_v1` would re-send
/// the block past the gap, and yrs 0.27 can panic applying a block that runs
/// from a gap into blocks a doc already holds (seed 5 of the property, on
/// the log doc).
#[test]
fn a_block_integrated_past_a_gap_persists_the_raw_update() {
    let client = Client::new(2);
    {
        let mut txn = client.doc.transact_mut();
        let p = client
            .frag
            .insert(&mut txn, 0, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("hello"));
    }
    let base = client.take_undelivered();
    // T1 appends to the text; T2 inserts at its start and depends only on
    // what the server has, not on T1.
    for (at, word) in [(5, "!"), (0, ">")] {
        let mut txn = client.doc.transact_mut();
        let text = client.text_of(&txn, 0).unwrap();
        text.insert(&mut txn, at, word);
    }
    let mut own = client.take_undelivered();
    let t2 = own.pop().unwrap();
    let t1 = own.pop().unwrap();

    let server = Server::new();
    let mut counts = Counts::default();
    for u in &base {
        server.deliver(u, 0, &mut counts);
    }
    let m = server.deliver(&t2, 0, &mut counts);
    assert_eq!(
        pending(&server.live),
        (None, None),
        "fixture: T2 integrates, nothing is parked"
    );
    assert!(m.changed);
    assert!(m.raw_fallback, "T2 integrated past a gap: persist it raw");
    assert_eq!(m.persist, t2);
    assert_same_state(&server.live, &server.log, 0);
    let m = server.deliver(&t1, 0, &mut counts);
    assert!(m.raw_fallback, "T1 fills the gap: persist it raw");
    assert_eq!(m.persist, t1);
    assert_same_state(&server.raw, &server.live, 0);
    assert_same_state(&server.live, &server.log, 0);
    assert_eq!(
        client.frag.get_string(&client.doc.transact()),
        server
            .log
            .inner()
            .get_or_insert_xml_fragment("default")
            .get_string(&server.log.inner().transact()),
    );
}

/// A reconnecting client with nothing new resends what the server already
/// has (its whole state, deletes included): nothing to persist. With one new
/// edit, only that edit is persisted, and it applies like the resend.
#[test]
fn effective_update_of_a_resend_is_empty_or_new_only() {
    let client = Client::new(1);
    for word in ["hello", "world"] {
        let mut txn = client.doc.transact_mut();
        let at = client.frag.len(&txn);
        let p = client
            .frag
            .insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(word));
    }
    {
        let mut txn = client.doc.transact_mut();
        let text = client.text_of(&txn, 0).unwrap();
        text.remove_range(&mut txn, 1, 2);
        let attrs: Attrs = [("bold".into(), Any::Bool(true))].into_iter().collect();
        text.format(&mut txn, 0, 2, attrs);
    }
    let full = || {
        client
            .doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    };

    let server = YrsEngine.new_doc();
    YrsEngine.apply_update(&server, &full()).unwrap();

    // Nothing new: the full state and the SyncStep2 against the server's
    // state vector (just the delete set) both persist nothing.
    let sv = server.inner().transact().state_vector();
    let step2 = client.doc.transact().encode_state_as_update_v1(&sv);
    for resend in [full(), step2] {
        let m = YrsEngine
            .apply_update_measuring(&server, &resend, MAX_NESTING)
            .unwrap();
        assert!(!m.changed);
        assert!(m.persist.is_empty(), "a no-op resend persists nothing");
        assert!(!m.raw_fallback);
    }

    // One new edit, resent with the whole state: only the edit is persisted.
    {
        let mut txn = client.doc.transact_mut();
        let text = client.text_of(&txn, 1).unwrap();
        text.insert(&mut txn, 5, "!");
    }
    let resend = full();
    let before = server
        .inner()
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let m = YrsEngine
        .apply_update_measuring(&server, &resend, MAX_NESTING)
        .unwrap();
    assert!(m.changed);
    assert!(!m.raw_fallback);
    assert!(
        m.persist.len() < resend.len(),
        "the persisted update ({} bytes) should be smaller than the resend ({} bytes)",
        m.persist.len(),
        resend.len()
    );
    let persisted = Update::decode_v1(&m.persist).unwrap();
    assert!(
        persisted.delete_set().is_empty(),
        "the resend's known deletes are not persisted again"
    );
    assert_eq!(
        persisted.state_vector_lower(),
        sv,
        "only content past the server's state is persisted"
    );
    // Replaying the log (the server's earlier state, then the persisted
    // update) gives the room's state.
    let replay = YrsEngine.new_doc();
    YrsEngine.apply_update(&replay, &before).unwrap();
    YrsEngine.apply_update(&replay, &m.persist).unwrap();
    assert_same_state(&server, &replay, 0);
    assert_eq!(
        server
            .inner()
            .get_or_insert_xml_fragment("default")
            .get_string(&server.inner().transact()),
        client.frag.get_string(&client.doc.transact()),
    );
}
