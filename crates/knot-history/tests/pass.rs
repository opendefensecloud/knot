//! Pass compute (page history M1.3, Task 4): attribution, the gap check,
//! checkpoints and genesis, as pure functions over rows and snapshots.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use common::underflow::skip_underflow_case;
use common::{Client, History, at, author};
use knot_history::canonical::visible_content;
use knot_history::{
    BaselineInput, CheckpointKind, CheckpointWrite, Claims, EntryDraft, EntryKind, GenesisError,
    GenesisInput, GenesisOutput, KeptGap, PassInput, PassOutput, Progress, ReplayError,
    ReplayHooks, ReplayMode, Replayer, RowOutcome, SnapshotProbe, Stage, WindowInput,
    build_genesis, checkpoint_threshold, replay_window, run_pass,
};
use knot_storage::{DocUpdate, UpdateSource};
use serde_json::json;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, IdSet, ReadTxn, StateVector, Text, Transact, Update, XmlElementPrelim,
    XmlFragment, XmlTextPrelim,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The state of an empty page: the genesis checkpoint of a new page.
fn empty_state() -> Vec<u8> {
    Replayer::new(ReplayMode::Gc).encode_state()
}

/// A pass from `checkpoint_state` at rev 0 over `rows`, everything else
/// empty: genesis at 0, nothing built, the rows reach the head.
fn input(checkpoint_state: Vec<u8>, rows: &[DocUpdate]) -> PassInput {
    PassInput {
        checkpoint_rev: 0,
        checkpoint_state,
        reapply: Vec::new(),
        replay: Vec::new(),
        genesis_rev: 0,
        built_rev: 0,
        rows: rows.to_vec(),
        rows_exhausted: true,
        open: Vec::new(),
        claims: Claims::default(),
        kept_gaps: Vec::new(),
        snapshots: Vec::new(),
        snapshot_checked_rev: 0,
        skip_revs: BTreeSet::new(),
        progress: Progress::default(),
        now: at(24 * 60),
        rows_budget: None,
        checkpoint_floor: 256,
        hooks: ReplayHooks::default(),
    }
}

fn pass(input: PassInput) -> PassOutput {
    run_pass(input).expect("the pass")
}

fn insertions_of(update: &[u8]) -> IdSet {
    Update::decode_v1(update)
        .expect("an update")
        .insertions(true)
}

fn clients_of(set: &IdSet) -> BTreeSet<u64> {
    set.iter()
        .filter(|(_, r)| !r.is_empty())
        .map(|(c, _)| c.get())
        .collect()
}

fn overlaps(a: &IdSet, b: &IdSet) -> bool {
    !a.intersect(b).is_empty()
}

fn contains(outer: &IdSet, inner: &IdSet) -> bool {
    inner.diff(outer).is_empty()
}

fn set_eq(a: &IdSet, b: &IdSet) -> bool {
    contains(a, b) && contains(b, a)
}

fn of_kind(entries: &[EntryDraft], kind: EntryKind) -> Vec<&EntryDraft> {
    entries.iter().filter(|e| e.kind == kind).collect()
}

fn sessions_of(entries: &[EntryDraft], who: usize) -> Vec<&EntryDraft> {
    entries
        .iter()
        .filter(|e| e.kind == EntryKind::Session && e.author == Some(author(who)))
        .collect()
}

/// The `default` fragment of a state, as XML.
fn xml_of_state(state: &[u8]) -> String {
    xml_of(
        Replayer::from_state(ReplayMode::Gc, state)
            .expect("a state")
            .doc(),
    )
}

fn xml_of(doc: &Doc) -> String {
    let txn = doc.transact();
    match txn.get_xml_fragment("default") {
        Some(f) => f.get_string(&txn),
        None => String::new(),
    }
}

/// `updates` merged into one.
fn merged(updates: &[Vec<u8>]) -> Vec<u8> {
    yrs::merge_updates_v1(updates).expect("merge updates")
}

/// An extra paragraph `text` that no row carries, typed by client 9 on its
/// view of `state`.
fn extra_paragraph(state: &[u8], text: &str) -> Vec<u8> {
    extra_paragraph_by(9, state, text)
}

/// [`extra_paragraph`] by client `client`.
fn extra_paragraph_by(client: u64, state: &[u8], text: &str) -> Vec<u8> {
    let c = Client::new(client);
    c.doc
        .transact_mut()
        .apply_update(Update::decode_v1(state).expect("a state"))
        .expect("the state applies");
    c.txn(|txn, frag| {
        let n = frag.len(txn);
        let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(text));
    })
}

fn snapshot(rev: i64, created_at: DateTime<Utc>, state: Vec<u8>) -> SnapshotProbe {
    SnapshotProbe {
        rev,
        created_at,
        state_bytes: Some(state),
    }
}

/// A history where A types a paragraph per row (about 120 bytes each, so
/// a 256-byte checkpoint floor places checkpoints), `n` rows a minute apart.
fn a_types(n: usize) -> History {
    let mut h = History::new(7);
    for i in 0..n {
        let text = format!("line {i} of a: {}", "lorem ipsum ".repeat(8));
        h.type_para(0, &text, at(i as i64));
    }
    h
}

/// Three independent rows: each of A, B and C types a paragraph on a doc
/// that has seen nothing of the others'.
fn three_independent() -> History {
    let mut h = History::new(7);
    for i in 0..3 {
        h.type_para(i, &format!("line {i} of a"), at(i as i64));
    }
    h
}

/// Every row with `from < rev <= to`.
fn rows_in(rows: &[DocUpdate], from: i64, to: i64) -> Vec<DocUpdate> {
    rows.iter()
        .filter(|r| r.rev.is_some_and(|rev| from < rev && rev <= to))
        .cloned()
        .collect()
}

/// The rows a replay from `cp` re-applies first: those of its
/// `pending_revs` (the pending fallback).
fn reapply_of(cp: &CheckpointWrite, rows: &[DocUpdate]) -> Vec<DocUpdate> {
    rows.iter()
        .filter(|r| r.rev.is_some_and(|rev| cp.pending_revs.contains(&rev)))
        .cloned()
        .collect()
}

fn checkpoint_at(cps: &[CheckpointWrite], rev: i64) -> Option<&CheckpointWrite> {
    cps.iter().find(|c| c.rev == rev)
}

// ---------------------------------------------------------------------------
// The store between passes
// ---------------------------------------------------------------------------

/// An entry's content, ids aside: what one big pass and many small ones
/// must agree on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StableKey {
    kind: &'static str,
    author: Option<uuid::Uuid>,
    start_rev: Option<i64>,
    end_rev: Option<i64>,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    sealed: bool,
    ins: Vec<u8>,
    del: Vec<u8>,
    meta: String,
}

fn stable_key(e: &EntryDraft) -> StableKey {
    StableKey {
        kind: e.kind.as_str(),
        author: e.author,
        start_rev: e.start_rev,
        end_rev: e.end_rev,
        started_at: e.started_at,
        ended_at: e.ended_at,
        sealed: e.sealed,
        ins: e.inserted.encode_v1(),
        del: e.deleted.encode_v1(),
        meta: e.meta.to_string(),
    }
}

/// Fails, naming only the entries one side has and the other lacks,
/// unless both hold the same entries (ids aside).
fn assert_same_entries(a: &[EntryDraft], b: &[EntryDraft], what: &str) {
    let (ka, kb) = (stable_keys(a), stable_keys(b));
    if ka != kb {
        let only_a: Vec<_> = ka.iter().filter(|k| !kb.contains(k)).collect();
        let only_b: Vec<_> = kb.iter().filter(|k| !ka.contains(k)).collect();
        panic!(
            "{what}: entries differ\n only in the first: {only_a:?}\n only in the second: {only_b:?}"
        );
    }
}

fn stable_keys(entries: &[EntryDraft]) -> Vec<StableKey> {
    let mut keys: Vec<StableKey> = entries.iter().map(stable_key).collect();
    keys.sort();
    keys
}

/// What a page's history tables hold, as the builder stores them between
/// passes.
struct Page {
    entries: BTreeMap<i64, EntryDraft>,
    next_id: i64,
    checkpoints: BTreeMap<i64, CheckpointWrite>,
    claims: Claims,
    genesis_rev: i64,
    built_rev: i64,
    snapshot_checked_rev: i64,
    skip_revs: BTreeSet<i64>,
    /// Passes that resumed from a periodic checkpoint holding pending content.
    pending_resumes: usize,
    passes: usize,
}

impl Page {
    /// A new page after genesis: an empty checkpoint at rev 0.
    fn new() -> Page {
        let g = build_genesis(GenesisInput {
            page_created_at: at(0),
            history_since: at(0),
            genesis_rev: 0,
            baselines: Vec::new(),
            rows: Vec::new(),
            skip_revs: BTreeSet::new(),
            progress: Progress::default(),
            hooks: ReplayHooks::default(),
        })
        .expect("genesis");
        let mut page = Page {
            entries: BTreeMap::new(),
            next_id: 1,
            checkpoints: BTreeMap::new(),
            claims: g.claims,
            genesis_rev: 0,
            built_rev: 0,
            snapshot_checked_rev: 0,
            skip_revs: BTreeSet::new(),
            pending_resumes: 0,
            passes: 0,
        };
        page.checkpoints.insert(0, g.checkpoint);
        page
    }

    /// The latest checkpoint at or below `built_rev`.
    fn start(&self) -> &CheckpointWrite {
        self.checkpoints
            .range(..=self.built_rev)
            .next_back()
            .map(|(_, c)| c)
            .expect("a checkpoint at or below built_rev")
    }

    /// The input of the next pass: up to `max_rows` new rows, the given
    /// snapshots the store would select.
    fn input(
        &mut self,
        rows: &[DocUpdate],
        snapshots: &[SnapshotProbe],
        max_rows: usize,
        floor: u64,
        now: DateTime<Utc>,
    ) -> PassInput {
        let start = self.start().clone();
        let reapply = reapply_of(&start, rows);
        if start.kind == CheckpointKind::Periodic && !reapply.is_empty() {
            let mut r = Replayer::from_state(ReplayMode::Gc, &start.state_bytes).unwrap();
            for row in &reapply {
                r.apply(&row.update_bytes);
            }
            if r.has_pending() {
                self.pending_resumes += 1;
            }
        }
        let new: Vec<DocUpdate> = rows
            .iter()
            .filter(|r| r.rev.unwrap() > self.built_rev)
            .take(max_rows)
            .cloned()
            .collect();
        let head = rows.last().and_then(|r| r.rev).unwrap_or(0);
        let last = new.last().and_then(|r| r.rev).unwrap_or(self.built_rev);
        let upto = self.built_rev.max(last);
        let snapshots: Vec<SnapshotProbe> = snapshots
            .iter()
            .filter(|s| self.snapshot_checked_rev < s.rev && s.rev <= upto)
            .cloned()
            .collect();
        PassInput {
            checkpoint_rev: start.rev,
            checkpoint_state: start.state_bytes.clone(),
            reapply,
            replay: rows_in(rows, start.rev, self.built_rev),
            genesis_rev: self.genesis_rev,
            built_rev: self.built_rev,
            rows: new,
            rows_exhausted: last == head,
            open: self
                .entries
                .values()
                .filter(|e| e.kind == EntryKind::Session && !e.sealed)
                .cloned()
                .collect(),
            claims: self.claims.clone(),
            kept_gaps: Vec::new(),
            snapshots,
            snapshot_checked_rev: self.snapshot_checked_rev,
            skip_revs: self.skip_revs.clone(),
            progress: Progress::default(),
            now,
            rows_budget: None,
            checkpoint_floor: floor,
            hooks: ReplayHooks::default(),
        }
    }

    /// Writes what a pass built, as the builder's write transaction does.
    fn absorb(&mut self, out: &PassOutput) {
        for e in &out.entries {
            let mut e = e.clone();
            let id = *e.id.get_or_insert_with(|| {
                self.next_id += 1;
                self.next_id
            });
            self.entries.insert(id, e);
        }
        for cp in &out.checkpoints {
            // ON CONFLICT DO UPDATE only for a gap checkpoint.
            let keep = self.checkpoints.contains_key(&cp.rev) && cp.kind != CheckpointKind::Gap;
            if !keep {
                self.checkpoints.insert(cp.rev, cp.clone());
            }
        }
        self.claims = out.claims.clone();
        self.built_rev = out.built_rev;
        self.snapshot_checked_rev = out.snapshot_checked_rev;
        self.skip_revs.extend(out.refused_revs.iter().copied());
        self.passes += 1;
    }

    fn entries(&self) -> Vec<EntryDraft> {
        self.entries.values().cloned().collect()
    }

    fn checkpoints(&self) -> Vec<CheckpointWrite> {
        self.checkpoints.values().cloned().collect()
    }
}

/// What `build_all` built.
struct Built {
    entries: Vec<EntryDraft>,
    checkpoints: Vec<CheckpointWrite>,
    skip_revs: BTreeSet<i64>,
    pending_resumes: usize,
}

/// Builds `rows` from genesis with passes of at most `max_rows` rows; each
/// snapshot is handed to passes from index `visible_after_pass` on.
fn build_all(
    rows: &[DocUpdate],
    snapshots: &[(usize, SnapshotProbe)],
    max_rows: usize,
    checkpoint_floor: u64,
) -> Built {
    let mut page = Page::new();
    let head = rows.last().and_then(|r| r.rev).unwrap_or(0);
    let now = rows.last().map_or(at(0), |r| r.created_at) + chrono::Duration::hours(2);
    let last_visible = snapshots.iter().map(|(v, _)| *v).max().unwrap_or(0);
    let mut p = 0;
    loop {
        let visible: Vec<SnapshotProbe> = snapshots
            .iter()
            .filter(|(v, _)| *v <= p)
            .map(|(_, s)| s.clone())
            .collect();
        let pending_snapshot = visible
            .iter()
            .any(|s| s.rev > page.snapshot_checked_rev && s.rev <= head);
        if page.built_rev >= head && p > last_visible && !pending_snapshot {
            break;
        }
        let inp = page.input(rows, &visible, max_rows, checkpoint_floor, now);
        let out = run_pass(inp).expect("a pass");
        page.absorb(&out);
        p += 1;
        assert!(p < 10_000, "build_all does not terminate");
    }
    Built {
        entries: page.entries(),
        checkpoints: page.checkpoints(),
        skip_revs: page.skip_revs.clone(),
        pending_resumes: page.pending_resumes,
    }
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

#[test]
fn overlapping_sessions_are_credited_to_their_own_authors() {
    let h = History::overlap_case();
    let out = pass(input(empty_state(), h.rows()));
    let a = sessions_of(&out.entries, 0);
    let b = sessions_of(&out.entries, 1);
    assert_eq!((a.len(), b.len()), (1, 1), "{:?}", out.entries);
    assert_eq!(clients_of(&a[0].inserted), BTreeSet::from([1]));
    assert_eq!(clients_of(&b[0].inserted), BTreeSet::from([2]));
    assert_eq!((a[0].start_rev, a[0].end_rev), (Some(1), Some(6)));
    assert_eq!((b[0].start_rev, b[0].end_rev), (Some(2), Some(4)));
    assert_eq!((a[0].started_at, a[0].ended_at), (at(0), at(40)));
    assert_eq!((b[0].started_at, b[0].ended_at), (at(10), at(20)));
    for r in h.rows() {
        let ins = insertions_of(&r.update_bytes);
        let own = if r.by_user_id == Some(author(0)) {
            a[0]
        } else {
            b[0]
        };
        assert!(contains(&own.inserted, &ins), "rev {:?}", r.rev);
    }
}

#[test]
fn a_reconnect_resend_credits_only_what_is_new() {
    let mut h = History::new(3);
    h.type_para(0, "online one", at(0));
    h.type_para(0, "online two", at(1));
    // A goes offline and types; on reconnect it resends its whole state.
    let offline = h.clients[0].txn(|txn, frag| {
        let n = frag.len(txn);
        let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("typed offline"));
    });
    let resend = h.clients[0]
        .doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    h.edit_at(0, resend, at(90));
    let out = pass(input(empty_state(), h.rows()));
    let a = sessions_of(&out.entries, 0);
    assert_eq!(a.len(), 2, "{:?}", out.entries);
    let late = a
        .iter()
        .find(|e| e.start_rev == Some(3))
        .expect("the resend's session");
    assert!(set_eq(&late.inserted, &insertions_of(&offline)));
    assert!(late.deleted.is_empty());
}

#[test]
fn parked_content_is_credited_to_the_row_that_delivered_it() {
    let mut h = History::new(4);
    // B makes a paragraph and does not deliver it yet; A sees it directly
    // and types into it.
    let b_para = h.clients[1].txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("base"));
    });
    h.clients[0]
        .doc
        .transact_mut()
        .apply_update(Update::decode_v1(&b_para).unwrap())
        .unwrap();
    let a_text = h.clients[0].txn(|txn, frag| {
        common::fixtures::text_at(frag, txn, 0)
            .unwrap()
            .insert(txn, 4, " and more");
    });
    h.edit_at(0, a_text.clone(), at(0)); // parks
    h.edit_at(1, b_para.clone(), at(60)); // drains A's row
    let out = pass(input(empty_state(), h.rows()));
    assert_eq!(out.stats.parked, 1);
    let a = sessions_of(&out.entries, 0);
    let b = sessions_of(&out.entries, 1);
    assert_eq!((a.len(), b.len()), (1, 1), "{:?}", out.entries);
    assert_eq!((a[0].start_rev, a[0].end_rev), (Some(2), Some(2)));
    assert_eq!(a[0].started_at, at(60));
    assert!(set_eq(&a[0].inserted, &insertions_of(&a_text)));
    assert!(set_eq(&b[0].inserted, &insertions_of(&b_para)));
    assert!(out.claims.is_empty());
}

#[test]
fn a_row_parked_for_good_creates_no_entry() {
    let mut h = History::new(5);
    let b_para = h.clients[1].txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("never delivered"));
    });
    h.clients[0]
        .doc
        .transact_mut()
        .apply_update(Update::decode_v1(&b_para).unwrap())
        .unwrap();
    let a_text = h.clients[0].txn(|txn, frag| {
        common::fixtures::text_at(frag, txn, 0)
            .unwrap()
            .insert(txn, 0, "x");
    });
    h.edit_at(0, a_text, at(0));
    h.type_para(2, "c types", at(1));
    let out = pass(input(empty_state(), h.rows()));
    assert!(sessions_of(&out.entries, 0).is_empty(), "{:?}", out.entries);
    assert_eq!(sessions_of(&out.entries, 2).len(), 1);
    assert_eq!(out.claims.len(), 1);
    assert_eq!(out.built_rev, 2);
    assert_eq!(out.stats.parked, 1);
}

// ---------------------------------------------------------------------------
// Refused rows and the union fallback
// ---------------------------------------------------------------------------

/// The refused row at rev 1 over `doc_state`, with the room state as a
/// snapshot at rev 1.
fn refused_case() -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<DocUpdate>) {
    let (doc_state, update, room_state) = skip_underflow_case();
    let mut h = History::new(6);
    h.edit_at(0, update.clone(), at(0));
    (doc_state, update, room_state, h.rows().to_vec())
}

#[test]
fn a_refused_row_surfaces_as_a_gap_entry() {
    let (doc_state, update, room_state, rows) = refused_case();
    let mut inp = input(doc_state.clone(), &rows);
    inp.snapshots = vec![snapshot(1, at(5), room_state)];
    let out = pass(inp);
    assert_eq!(out.stats.unapplied.get("skip_underflow"), Some(&1));
    assert!(sessions_of(&out.entries, 0).is_empty(), "{:?}", out.entries);
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 1, "{:?}", out.entries);
    assert_eq!(gaps[0].author, None);
    let new = insertions_of(&update).diff(&insertions_of(&doc_state));
    assert!(!new.is_empty());
    assert!(contains(&gaps[0].inserted, &new));
    for s in of_kind(&out.entries, EntryKind::Session) {
        assert!(!overlaps(&s.inserted, &new));
    }
    assert!(out.stats.snapshots_unchecked.is_empty());
    let cp = checkpoint_at(&out.checkpoints, 1).expect("a gap checkpoint at 1");
    assert_eq!(cp.kind, CheckpointKind::Gap);
    assert_eq!(out.refused_revs, vec![1]);

    // The next pass resumes from that gap checkpoint, skipping rev 1.
    let next_row = extra_paragraph(&cp.state_bytes, "after the gap");
    let mut h = History::new(6);
    h.edit_at(0, rows[0].update_bytes.clone(), at(0));
    h.edit_at(0, next_row.clone(), at(10));
    let mut inp = input(cp.state_bytes.clone(), &h.rows()[1..]);
    inp.checkpoint_rev = 1;
    inp.built_rev = 1;
    inp.snapshot_checked_rev = 1;
    inp.claims = out.claims.clone();
    inp.skip_revs = BTreeSet::from([1]);
    let next = pass(inp);
    assert_eq!(next.built_rev, 2);
    let a = sessions_of(&next.entries, 0);
    assert_eq!(a.len(), 1);
    assert!(set_eq(&a[0].inserted, &insertions_of(&next_row)));
}

#[test]
fn a_window_over_a_union_gap_checkpoint_merges_it() {
    let (doc_state, _update, room_state, rows) = refused_case();
    let mut inp = input(doc_state.clone(), &rows);
    inp.snapshots = vec![snapshot(1, at(5), room_state)];
    let gap = checkpoint_at(&pass(inp).checkpoints, 1)
        .expect("the union gap checkpoint")
        .state_bytes
        .clone();

    let mut h = History::new(6);
    h.edit_at(0, rows[0].update_bytes.clone(), at(0));
    let two = extra_paragraph(&gap, "two");
    h.edit_at(0, two.clone(), at(10));
    let three = {
        let mut r = Replayer::from_state(ReplayMode::Gc, &gap).unwrap();
        assert_eq!(r.apply(&two).outcome, RowOutcome::Applied);
        extra_paragraph(&r.encode_state(), "three")
    };
    h.edit_at(0, three.clone(), at(11));

    let window = |gap_states: Vec<(i64, Vec<u8>)>| {
        replay_window(WindowInput {
            mode: ReplayMode::KeepDeleted,
            checkpoint_state: doc_state.clone(),
            reapply: Vec::new(),
            rows: h.rows().to_vec(),
            gap_states,
            skip_revs: BTreeSet::from([1]),
            end_rev: 3,
            merge_gap_at_end: false,
            progress: Progress::default(),
        })
    };
    let replayed = window(vec![(1, gap.clone())]).expect("the window replays");
    let mut expected = Replayer::from_state(ReplayMode::Gc, &gap).unwrap();
    for u in [&two, &three] {
        assert_eq!(expected.apply(u).outcome, RowOutcome::Applied);
    }
    assert_eq!(
        visible_content(replayed.doc()),
        visible_content(expected.doc())
    );

    // The same loop with a plain `merge_state` for the gap state: refused.
    let mut plain = Replayer::from_state(ReplayMode::KeepDeleted, &doc_state).unwrap();
    let mut refused_at = None;
    for row in h.rows() {
        let rev = row.rev.unwrap();
        if rev == 2 {
            let e = plain.merge_state(&gap);
            if e.outcome != RowOutcome::Applied {
                refused_at = Some((1, e.outcome));
                break;
            }
        }
        if rev != 1 {
            plain.apply(&row.update_bytes);
        }
    }
    assert_eq!(refused_at, Some((1, RowOutcome::Refused)));
}

/// Rows 1..=3 by A and a snapshot at rev 3 that holds an extra paragraph:
/// the rows, the gap checkpoint at 3, and A's client doc.
fn gap_at_three() -> (History, Vec<u8>, Vec<u8>) {
    let mut h = a_types(3);
    let x = extra_paragraph(&h.room_snapshot(), "extra paragraph");
    let snap = merged(&[h.room_snapshot(), x.clone()]);
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![snapshot(3, at(4), snap)];
    let out = pass(inp);
    let gap = checkpoint_at(&out.checkpoints, 3)
        .filter(|c| c.kind == CheckpointKind::Gap)
        .expect("a gap checkpoint at 3")
        .state_bytes
        .clone();
    h.pull(0);
    (h, gap, x)
}

#[test]
fn replay_window_merges_a_gap_at_its_end_only_when_asked() {
    let (mut h, gap, _x) = gap_at_three();
    let window = |rows: &[DocUpdate], end_rev: i64, merge_gap_at_end: bool| {
        let r = replay_window(WindowInput {
            mode: ReplayMode::KeepDeleted,
            checkpoint_state: empty_state(),
            reapply: Vec::new(),
            rows: rows.to_vec(),
            gap_states: vec![(3, gap.clone())],
            skip_revs: BTreeSet::new(),
            end_rev,
            merge_gap_at_end,
            progress: Progress::default(),
        })
        .expect("the window replays");
        xml_of(r.doc())
    };
    assert!(!window(h.rows(), 3, false).contains("extra paragraph"));
    assert!(window(h.rows(), 3, true).contains("extra paragraph"));
    h.type_para(0, "four", at(5));
    for merge_at_end in [false, true] {
        let xml = window(h.rows(), 4, merge_at_end);
        assert!(xml.contains("extra paragraph") && xml.contains("four"));
    }
}

// ---------------------------------------------------------------------------
// Budget, cancellation and failures
// ---------------------------------------------------------------------------

#[test]
fn the_rows_budget_starts_at_the_first_new_row() {
    let h = a_types(53);
    let mut inp = input(empty_state(), &h.rows()[50..]);
    inp.replay = h.rows()[..50].to_vec();
    inp.built_rev = 50;
    inp.rows_budget = Some(Duration::ZERO);
    let out = pass(inp);
    assert_eq!(out.stats.rows, 1);
    assert!(out.stats.stopped_early);
    assert_eq!(out.built_rev, 51);
    // Not sealed: the pass stopped early.
    assert!(out.entries.iter().all(|e| !e.sealed));

    let mut inp = input(empty_state(), &h.rows()[50..51]);
    inp.replay = h.rows()[..50].to_vec();
    inp.built_rev = 50;
    inp.rows_budget = Some(Duration::ZERO);
    let out = pass(inp);
    assert!(!out.stats.stopped_early);
    assert_eq!(out.stats.rows, 1);
    let a = sessions_of(&out.entries, 0);
    assert_eq!(a.len(), 1);
    assert!(a[0].sealed, "the idle session is sealed");
}

#[test]
fn a_cancelled_pass_stops_at_the_next_row() {
    let h = a_types(3);
    let inp = input(empty_state(), h.rows());
    inp.progress.set(Stage::Row(7));
    let (stage, _) = inp.progress.cancel();
    assert_eq!(stage, Stage::Row(7));
    let progress = inp.progress.clone();
    assert!(matches!(run_pass(inp), Err(ReplayError::Cancelled)));
    progress.set(Stage::Emit(9));
    assert_eq!(progress.get(), Stage::Row(7));
    assert!(progress.is_cancelled());
}

#[test]
fn a_failing_prefix_row_blames_the_checkpoint_not_the_row() {
    let h = a_types(7);
    let first = pass(input(empty_state(), &h.rows()[..6]));
    let c = first
        .checkpoints
        .iter()
        .filter(|c| c.kind == CheckpointKind::Periodic && c.rev < 6)
        .map(|c| c.rev)
        .next_back()
        .expect("a periodic checkpoint below 6");
    let cp = checkpoint_at(&first.checkpoints, c)
        .unwrap()
        .state_bytes
        .clone();

    let mut inp = input(cp.clone(), &h.rows()[6..]);
    inp.checkpoint_rev = c;
    inp.replay = rows_in(h.rows(), c, 6);
    inp.built_rev = 6;
    inp.hooks.corrupt_at_rev = Some(c + 1);
    assert!(matches!(run_pass(inp), Err(ReplayError::Prefix { rev }) if rev == c + 1));

    let mut inp = input(cp, &rows_in(h.rows(), c, 7));
    inp.checkpoint_rev = c;
    inp.built_rev = c;
    inp.hooks.corrupt_at_rev = Some(c + 1);
    assert!(matches!(run_pass(inp), Err(ReplayError::Corrupt { rev }) if rev == c + 1));
}

#[test]
fn an_oversized_snapshot_is_left_unchecked() {
    let h = a_types(3);
    let too_large = SnapshotProbe {
        rev: 2,
        created_at: at(2),
        state_bytes: None,
    };
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![too_large.clone()];
    let out = pass(inp);
    assert_eq!(out.stats.snapshots_unchecked.get("too_large"), Some(&1));
    assert_eq!(out.snapshot_checked_rev, 2);
    assert!(of_kind(&out.entries, EntryKind::Gap).is_empty());

    let x = extra_paragraph(&h.room_snapshot(), "extra");
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![
        too_large,
        snapshot(3, at(3), merged(&[h.room_snapshot(), x])),
    ];
    let out = pass(inp);
    assert_eq!(out.stats.snapshots_unchecked.get("too_large"), Some(&1));
    assert_eq!(out.snapshot_checked_rev, 3);
    assert_eq!(of_kind(&out.entries, EntryKind::Gap).len(), 1);
}

#[test]
fn a_pass_with_nothing_to_replay_decodes_no_checkpoint() {
    let mut inp = input(vec![0xff, 0xff], &[]);
    let now = inp.now;
    inp.open = vec![EntryDraft {
        id: Some(17),
        kind: EntryKind::Session,
        author: Some(author(0)),
        started_at: now - chrono::Duration::minutes(40),
        ended_at: now - chrono::Duration::minutes(30),
        start_rev: Some(1),
        end_rev: Some(1),
        sealed: false,
        inserted: IdSet::new(),
        deleted: IdSet::new(),
        meta: json!({}),
        baseline_id: None,
    }];
    let out = run_pass(inp).expect("no decode, no error");
    assert_eq!(out.entries.len(), 1);
    assert!(out.entries[0].sealed);
    assert_eq!(out.entries[0].id, Some(17));
    assert!(out.checkpoints.is_empty());
}

// ---------------------------------------------------------------------------
// Gap checks
// ---------------------------------------------------------------------------

#[test]
fn a_gap_merge_parks_as_a_gap_claim() {
    let mut h = History::new(8);
    h.type_para(0, "base", at(0));
    // C's paragraph (never in the room or the snapshot) and B's text in it.
    let origin = h.clients[2].txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("origin"));
    });
    h.clients[1]
        .doc
        .transact_mut()
        .apply_update(Update::decode_v1(&origin).unwrap())
        .unwrap();
    let b_text = h.clients[1].txn(|txn, frag| {
        common::fixtures::text_at(frag, txn, 0)
            .unwrap()
            .insert(txn, 0, "bee ");
    });
    let x = extra_paragraph(&h.room_snapshot(), "extra");
    let snap = merged(&[h.room_snapshot(), x.clone(), b_text.clone()]);
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![snapshot(1, at(2), snap.clone())];
    let out = pass(inp);
    let gap_claims: Vec<_> = out.claims.iter().filter(|c| c.gap).collect();
    assert_eq!((out.claims.len(), gap_claims.len()), (1, 1));
    assert_eq!(gap_claims[0].author, None);
    assert!(set_eq(&gap_claims[0].inserts, &insertions_of(&b_text)));
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 1);
    assert!(set_eq(&gaps[0].inserted, &insertions_of(&x)));

    // C delivers the origin in the same pass: C's entry holds only C's
    // content, and the drained text is a gap entry of its own at C's rev.
    h.edit_at(2, origin.clone(), at(10));
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![snapshot(1, at(2), snap)];
    let out = pass(inp);
    let c = sessions_of(&out.entries, 2);
    assert_eq!(c.len(), 1);
    assert!(set_eq(&c[0].inserted, &insertions_of(&origin)));
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 2, "{:?}", out.entries);
    assert_eq!((gaps[1].start_rev, gaps[1].end_rev), (Some(2), Some(2)));
    assert!(set_eq(&gaps[1].inserted, &insertions_of(&b_text)));
    assert!(out.claims.is_empty());
}

#[test]
fn a_snapshot_whose_probe_corrupts_or_is_undecodable_is_left_unchecked() {
    let h = a_types(3);
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![snapshot(2, at(2), vec![0xff, 0xff, 0xff])];
    let out = pass(inp);
    assert_eq!(out.stats.snapshots_unchecked.get("undecodable"), Some(&1));
    assert_eq!(out.snapshot_checked_rev, 2);
    let a = sessions_of(&out.entries, 0);
    assert_eq!(a.len(), 1);
    for r in h.rows() {
        assert!(contains(&a[0].inserted, &insertions_of(&r.update_bytes)));
    }
    assert!(of_kind(&out.entries, EntryKind::Gap).is_empty());
}

#[test]
fn a_corrupt_gap_merge_blames_the_snapshot() {
    let h = a_types(3);
    let x = extra_paragraph(&h.room_snapshot(), "extra");
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![snapshot(2, at(2), merged(&[h.room_snapshot(), x]))];
    inp.hooks.corrupt_merge_at = Some(2);
    assert!(matches!(
        run_pass(inp),
        Err(ReplayError::CorruptSnapshot { snapshot_rev: 2 })
    ));
}

#[test]
fn a_corrupt_row_stops_the_pass_with_its_rev() {
    let h = a_types(4);
    let mut inp = input(empty_state(), h.rows());
    inp.hooks.corrupt_at_rev = Some(3);
    assert!(matches!(
        run_pass(inp),
        Err(ReplayError::Corrupt { rev: 3 })
    ));

    let mut inp = input(empty_state(), h.rows());
    inp.hooks.corrupt_at_rev = Some(3);
    inp.skip_revs = BTreeSet::from([3]);
    let out = pass(inp);
    assert_eq!(out.stats.unapplied.get("skipped"), Some(&1));
    assert_eq!(out.built_rev, 4);
}

#[test]
fn a_duplicated_row_creates_no_second_entry() {
    let mut h = History::new(9);
    let rev = h.type_para(0, "typed once", at(0));
    let bytes = h.rows()[rev as usize - 1].update_bytes.clone();
    h.edit_at(0, bytes, at(60));
    let restore = h.clients[0].txn(|txn, frag| {
        frag.remove_range(txn, 0, 1);
        let p = frag.insert(txn, 0, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("restored"));
    });
    for minutes in [120, 180] {
        h.push_at(
            Some(author(0)),
            restore.clone(),
            Some(UpdateSource::Restore),
            None,
            at(minutes),
        );
    }
    let out = pass(input(empty_state(), h.rows()));
    let a = sessions_of(&out.entries, 0);
    assert_eq!(a.len(), 1, "{:?}", out.entries);
    assert_eq!((a[0].start_rev, a[0].end_rev), (Some(1), Some(1)));
    let restores = of_kind(&out.entries, EntryKind::Restore);
    assert_eq!(restores.len(), 1);
    assert_eq!(restores[0].end_rev, Some(3));
}

#[test]
fn rows_without_effect_do_not_seal_a_session() {
    let mut h = History::new(10);
    h.type_para(0, "first", at(0));
    let first = h.rows()[0].update_bytes.clone();
    h.push_at(
        Some(author(0)),
        first,
        Some(UpdateSource::Restore),
        None,
        at(5),
    );
    h.type_para(0, "second", at(10));
    let mut inp = input(empty_state(), h.rows());
    inp.now = at(15);
    let out = pass(inp);
    assert!(of_kind(&out.entries, EntryKind::Restore).is_empty());
    let a = sessions_of(&out.entries, 0);
    assert_eq!(a.len(), 1, "{:?}", out.entries);
    assert!(!a[0].sealed);
    assert_eq!((a[0].start_rev, a[0].end_rev), (Some(1), Some(3)));
}

#[test]
fn snapshots_of_the_log_report_no_gap() {
    for seed in 1..=300u64 {
        let mut h = History::new(seed);
        let mut snapshots = Vec::new();
        while h.rows().len() < 60 {
            let before = h.rows().len();
            h.step();
            let n = h.rows().len();
            if n != before && n.is_multiple_of(20) {
                snapshots.push(snapshot(n as i64, h.now(), h.room_snapshot()));
            }
        }
        let mut inp = input(empty_state(), h.rows());
        inp.snapshots = snapshots;
        let out = pass(inp);
        assert_eq!(out.stats.gaps, 0, "seed {seed}");
        assert!(
            of_kind(&out.entries, EntryKind::Gap).is_empty(),
            "seed {seed}"
        );
        assert_eq!(out.snapshot_checked_rev, 60, "seed {seed}");
    }
}

#[test]
fn a_snapshot_with_content_no_row_carries_is_a_gap_entry() {
    let mut h = a_types(3);
    let x = extra_paragraph(&h.room_snapshot(), "extra paragraph");
    let snap = merged(&[h.room_snapshot(), x.clone()]);
    // Later rows type and remove long paragraphs: the row bytes grow, the
    // state stays small, so periodic checkpoints follow the gap.
    for i in 3..12 {
        let text = format!("a later line, number {i}: {}", "dolor sit amet ".repeat(8));
        h.type_para(0, &text, at(i));
        let gone = h.clients[0].txn(|txn, frag| {
            let n = frag.len(txn);
            frag.remove_range(txn, n - 1, 1);
        });
        h.edit_at(0, gone, at(i));
    }
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![snapshot(3, at(3), snap)];
    let out = pass(inp);
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].author, None);
    assert_eq!((gaps[0].start_rev, gaps[0].end_rev), (Some(3), Some(3)));
    assert!(set_eq(&gaps[0].inserted, &insertions_of(&x)));
    assert_eq!(out.stats.gaps, 1);
    assert_eq!(
        checkpoint_at(&out.checkpoints, 3).map(|c| c.kind),
        Some(CheckpointKind::Gap)
    );
    let later: Vec<_> = out.checkpoints.iter().filter(|c| c.rev > 3).collect();
    assert!(!later.is_empty(), "a periodic checkpoint after the gap");
    for c in later {
        assert!(xml_of_state(&c.state_bytes).contains("extra paragraph"));
    }
    for s in of_kind(&out.entries, EntryKind::Session) {
        assert!(!overlaps(&s.inserted, &insertions_of(&x)));
    }
}

#[test]
fn a_snapshot_written_after_its_rev_was_built_is_still_gap_checked() {
    let n = 5;
    let mut h = a_types(n);
    let built = pass(input(empty_state(), h.rows()));
    let cp = built.checkpoints.last().cloned();
    let (cp_rev, cp_state) = cp.map_or((0, empty_state()), |c| (c.rev, c.state_bytes));
    let room_n = h.room_snapshot();
    let x = extra_paragraph(&room_n, "late extra");
    let late = snapshot(n as i64, at(30), merged(&[room_n.clone(), x.clone()]));
    let prefix = rows_in(h.rows(), cp_rev, n as i64);
    let resume = |rows: &[DocUpdate], snapshots: Vec<SnapshotProbe>| {
        let mut inp = input(cp_state.clone(), rows);
        inp.checkpoint_rev = cp_rev;
        inp.replay = prefix.clone();
        inp.built_rev = n as i64;
        inp.open = built
            .entries
            .iter()
            .filter(|e| !e.sealed)
            .cloned()
            .collect();
        inp.snapshots = snapshots;
        pass(inp)
    };

    // (a) a pass with no new rows.
    let out = resume(&[], vec![late.clone()]);
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].end_rev, Some(n as i64));
    assert!(set_eq(&gaps[0].inserted, &insertions_of(&x)));
    assert_eq!(
        checkpoint_at(&out.checkpoints, n as i64).map(|c| c.kind),
        Some(CheckpointKind::Gap)
    );
    assert_eq!(out.snapshot_checked_rev, n as i64);

    // (b) one more row: the same gap entry, and the row's own entry.
    h.type_para(0, "one more", at(31));
    let out = resume(&h.rows()[n..], vec![late.clone()]);
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].end_rev, Some(n as i64));
    let a = sessions_of(&out.entries, 0);
    assert!(a.iter().any(|e| e.end_rev == Some(n as i64 + 1)));

    // As a build_all case: the snapshot visible from the second pass.
    let b = build_all(&h.rows()[..n], &[(1, late.clone())], 3, 256);
    assert_eq!(of_kind(&b.entries, EntryKind::Gap).len(), 1);

    // (c) two late snapshots with different extra paragraphs.
    let y = extra_paragraph_by(10, &room_n, "another extra");
    let rows_before: Vec<Vec<u8>> = h.rows()[..n - 1]
        .iter()
        .map(|r| r.update_bytes.clone())
        .collect();
    let earlier = snapshot(
        n as i64 - 1,
        at(29),
        merged(&[merged(&rows_before), y.clone()]),
    );
    let out = resume(&[], vec![earlier, late]);
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 2, "{:?}", out.entries);
    assert!(gaps.iter().all(|g| g.end_rev == Some(n as i64)));
    let gap_cps: Vec<_> = out
        .checkpoints
        .iter()
        .filter(|c| c.kind == CheckpointKind::Gap)
        .collect();
    assert_eq!(gap_cps.len(), 1);
    assert_eq!(gap_cps[0].rev, n as i64);
    let xml = xml_of_state(&gap_cps[0].state_bytes);
    assert!(xml.contains("late extra") && xml.contains("another extra"));
}

#[test]
fn a_gap_merge_credits_the_rows_it_drains() {
    let mut h = History::new(11);
    h.type_para(0, "a starts", at(0));
    // P: a paragraph no row carries; B types inside it.
    let p = h.clients[2].txn(|txn, frag| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("pee"));
    });
    h.clients[1]
        .doc
        .transact_mut()
        .apply_update(Update::decode_v1(&p).unwrap())
        .unwrap();
    let b_text = h.clients[1].txn(|txn, frag| {
        common::fixtures::text_at(frag, txn, 0)
            .unwrap()
            .insert(txn, 3, " bee");
    });
    h.edit_at(1, b_text.clone(), at(1)); // parks
    h.type_para(0, "a goes on", at(2));
    let snap = merged(&[h.room_snapshot(), p.clone()]);
    let mut inp = input(empty_state(), h.rows());
    inp.snapshots = vec![snapshot(3, at(3), snap)];
    let out = pass(inp);
    assert!(out.claims.is_empty());
    let b = sessions_of(&out.entries, 1);
    assert_eq!(b.len(), 1, "{:?}", out.entries);
    assert!(set_eq(&b[0].inserted, &insertions_of(&b_text)));
    assert_eq!(b[0].end_rev, Some(3));
    assert_eq!(b[0].meta["gap_credit_rev"], json!(3));
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 1);
    assert!(set_eq(&gaps[0].inserted, &insertions_of(&p)));

    let gap = checkpoint_at(&out.checkpoints, 3)
        .unwrap()
        .state_bytes
        .clone();
    let window = |merge_gap_at_end: bool| {
        let r = replay_window(WindowInput {
            mode: ReplayMode::KeepDeleted,
            checkpoint_state: empty_state(),
            reapply: Vec::new(),
            rows: h.rows().to_vec(),
            gap_states: vec![(3, gap.clone())],
            skip_revs: BTreeSet::new(),
            end_rev: 3,
            merge_gap_at_end,
            progress: Progress::default(),
        })
        .unwrap();
        xml_of(r.doc())
    };
    assert!(window(true).contains("pee bee"));
    assert!(!window(false).contains("bee"));
}

// ---------------------------------------------------------------------------
// Kept gap states
// ---------------------------------------------------------------------------

#[test]
fn a_kept_gap_state_is_merged_at_its_rev() {
    let mut h = a_types(3);
    let x = extra_paragraph(&h.room_snapshot(), "kept extra");
    let snap = snapshot(3, at(3), merged(&[h.room_snapshot(), x.clone()]));
    for i in 3..6 {
        h.type_para(0, &format!("a writes line {i} after the gap"), at(i));
    }
    let first = build_all(h.rows(), &[(0, snap.clone())], usize::MAX, 256);
    let k = checkpoint_at(&first.checkpoints, 3)
        .filter(|c| c.kind == CheckpointKind::Gap)
        .expect("the gap checkpoint K")
        .clone();
    let kept = KeptGap {
        rev: 3,
        created_at: snap.created_at,
        state_bytes: k.state_bytes.clone(),
    };

    let rebuild = |rows: &[DocUpdate], snapshots: Vec<SnapshotProbe>| {
        let mut inp = input(empty_state(), rows);
        inp.kept_gaps = vec![kept.clone()];
        inp.snapshots = snapshots;
        pass(inp)
    };
    let out = rebuild(h.rows(), Vec::new());
    let gaps = of_kind(&out.entries, EntryKind::Gap);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].end_rev, Some(3));
    assert_eq!(gaps[0].started_at, snap.created_at);
    assert!(set_eq(&gaps[0].inserted, &insertions_of(&x)));
    let cp = checkpoint_at(&out.checkpoints, 3).expect("a checkpoint at 3");
    assert_eq!(cp.kind, CheckpointKind::Gap);
    assert_eq!(
        visible_content(
            Replayer::from_state(ReplayMode::Gc, &cp.state_bytes)
                .unwrap()
                .doc()
        ),
        visible_content(
            Replayer::from_state(ReplayMode::Gc, &k.state_bytes)
                .unwrap()
                .doc()
        )
    );
    for c in out.checkpoints.iter().filter(|c| c.rev > 3) {
        assert!(xml_of_state(&c.state_bytes).contains("kept extra"));
    }
    assert_eq!(stable_keys(&out.entries), stable_keys(&first.entries));

    let out = rebuild(h.rows(), vec![snap]);
    assert_eq!(of_kind(&out.entries, EntryKind::Gap).len(), 1);

    let out = rebuild(&h.rows()[..2], Vec::new());
    assert!(of_kind(&out.entries, EntryKind::Gap).is_empty());
    assert!(checkpoint_at(&out.checkpoints, 3).is_none());
}

#[test]
fn a_kept_gap_that_does_not_merge_fails_the_pass_with_its_rev() {
    let h = a_types(4);
    let mut inp = input(empty_state(), h.rows());
    inp.kept_gaps = vec![KeptGap {
        rev: 3,
        created_at: at(3),
        state_bytes: vec![0xff, 0xff],
    }];
    assert!(matches!(
        run_pass(inp),
        Err(ReplayError::KeptGap { rev: 3 })
    ));

    let mut inp = input(empty_state(), h.rows());
    inp.kept_gaps = vec![KeptGap {
        rev: 3,
        created_at: at(3),
        state_bytes: h.room_snapshot(),
    }];
    inp.hooks.corrupt_kept_gap_at = Some(3);
    assert!(matches!(
        run_pass(inp),
        Err(ReplayError::KeptGap { rev: 3 })
    ));
}

// ---------------------------------------------------------------------------
// Genesis content, copies and skipped rows
// ---------------------------------------------------------------------------

/// Content `origin` (by C, never a row) and B's text typed into it.
fn text_on_unlogged_origin(h: &History, b_says: &str) -> (Vec<u8>, Vec<u8>) {
    let origin = h.clients[2].txn(|txn, frag| {
        let n = frag.len(txn);
        let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("origin"));
    });
    h.clients[1]
        .doc
        .transact_mut()
        .apply_update(Update::decode_v1(&origin).unwrap())
        .unwrap();
    let b_says = b_says.to_string();
    let text = h.clients[1].txn(move |txn, frag| {
        let n = frag.len(txn);
        common::fixtures::text_at(frag, txn, n - 1)
            .unwrap()
            .insert(txn, 0, &b_says);
    });
    (origin, text)
}

fn genesis_input(genesis_rev: i64, rows: &[DocUpdate]) -> GenesisInput {
    GenesisInput {
        page_created_at: at(-24 * 60),
        history_since: at(-60),
        genesis_rev,
        baselines: Vec::new(),
        rows: rows.to_vec(),
        skip_revs: BTreeSet::new(),
        progress: Progress::default(),
        hooks: ReplayHooks::default(),
    }
}

#[test]
fn genesis_pending_content_is_never_credited() {
    let mut h = History::new(12);
    let (origin, legacy) = text_on_unlogged_origin(&h, "legacy ");
    h.edit_at(1, legacy.clone(), at(-120));
    let g = build_genesis(genesis_input(1, h.rows())).expect("genesis");
    let claims: Vec<_> = g.claims.iter().collect();
    assert_eq!(claims.len(), 1);
    assert_eq!(
        (claims[0].rev, claims[0].author, claims[0].gap),
        (1, None, false)
    );

    h.edit_at(0, origin.clone(), at(0));
    let mut inp = input(g.checkpoint.state_bytes.clone(), &h.rows()[1..]);
    inp.checkpoint_rev = 1;
    inp.genesis_rev = 1;
    inp.built_rev = 1;
    inp.snapshot_checked_rev = 1;
    inp.claims = g.claims.clone();
    inp.reapply = reapply_of(&g.checkpoint, h.rows());
    assert_eq!(inp.reapply.len(), 1);
    let out = pass(inp);
    let a = sessions_of(&out.entries, 0);
    assert_eq!(a.len(), 1);
    assert!(set_eq(&a[0].inserted, &insertions_of(&origin)));
    for e in &out.entries {
        assert!(!overlaps(&e.inserted, &insertions_of(&legacy)), "{e:?}");
    }
    assert!(out.claims.is_empty());
}

#[test]
fn an_empty_first_copy_row_still_creates_its_entry() {
    let empty = Doc::new()
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let meta = json!({"source_doc_id": uuid::Uuid::nil(), "source_title": "Source"});
    let mut h = History::new(13);
    h.push_at(
        Some(author(0)),
        empty.clone(),
        Some(UpdateSource::Copy),
        Some(meta.clone()),
        at(0),
    );
    h.push_at(
        Some(author(0)),
        empty.clone(),
        Some(UpdateSource::Copy),
        Some(meta.clone()),
        at(1),
    );
    h.push_at(
        Some(author(0)),
        empty.clone(),
        Some(UpdateSource::Restore),
        None,
        at(2),
    );
    let out = pass(input(empty_state(), h.rows()));
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let copy = &out.entries[0];
    assert_eq!(copy.kind, EntryKind::Copy);
    assert!(copy.sealed);
    assert!(copy.inserted.is_empty() && copy.deleted.is_empty());
    assert_eq!(copy.meta, meta);
    assert_eq!((copy.start_rev, copy.end_rev), (Some(1), Some(1)));

    let mut other = History::new(14);
    other.push_at(
        Some(author(1)),
        empty,
        Some(UpdateSource::Template),
        None,
        at(0),
    );
    let out = pass(input(empty_state(), other.rows()));
    assert!(out.entries.is_empty(), "{:?}", out.entries);
}

#[test]
fn a_skipped_rev_is_not_applied_and_is_counted() {
    let h = three_independent();
    let row2 = insertions_of(&h.rows()[1].update_bytes);
    // A snapshot of rows 1 and 3 plus an extra paragraph: its gap
    // checkpoint shows what the replay holds.
    let without_two = {
        let mut r = Replayer::new(ReplayMode::Gc);
        for i in [0, 2] {
            r.apply(&h.rows()[i].update_bytes);
        }
        r.encode_state()
    };
    let x = extra_paragraph(&without_two, "extra");
    let mut inp = input(empty_state(), h.rows());
    inp.skip_revs = BTreeSet::from([2]);
    inp.snapshots = vec![snapshot(3, at(3), merged(&[without_two, x]))];
    let out = pass(inp);
    assert_eq!(out.built_rev, 3);
    assert_eq!(out.stats.unapplied.get("skipped"), Some(&1));
    assert_eq!(out.stats.rows, 2);
    for e in &out.entries {
        assert!(!overlaps(&e.inserted, &row2), "{e:?}");
    }
    let cp = checkpoint_at(&out.checkpoints, 3).expect("the gap checkpoint");
    let xml = xml_of_state(&cp.state_bytes);
    assert!(xml.contains("line 0 of a") && xml.contains("line 2 of a"));
    assert!(!xml.contains("line 1 of a"));
}

#[test]
fn a_rev_gap_stops_the_pass() {
    let h = a_types(4);
    let rows = vec![
        h.rows()[0].clone(),
        h.rows()[1].clone(),
        h.rows()[3].clone(),
    ];
    let out = pass(input(empty_state(), &rows));
    assert_eq!(out.built_rev, 2);
    assert!(out.stats.rev_gap);
    assert!(out.entries.iter().all(|e| !e.sealed));
}

// ---------------------------------------------------------------------------
// Properties: many passes, checkpoints
// ---------------------------------------------------------------------------

/// A generated history of about `n` rows with room snapshots at every 20th
/// rev.
fn generated(seed: u64, n: usize) -> (Vec<DocUpdate>, Vec<(usize, SnapshotProbe)>) {
    let mut h = History::new(seed);
    let mut snapshots = Vec::new();
    let mut steps = 0;
    while h.rows().len() < n && steps < 4 * n {
        let before = h.rows().len();
        h.step();
        steps += 1;
        let len = h.rows().len();
        if len != before && len.is_multiple_of(20) {
            snapshots.push((0, snapshot(len as i64, h.now(), h.room_snapshot())));
        }
    }
    (h.rows().to_vec(), snapshots)
}

#[test]
fn many_small_passes_equal_one_pass() {
    let mut pending_resumes = 0;
    for seed in 1..=200u64 {
        let (rows, snapshots) = generated(seed, 50);
        let one = build_all(&rows, &snapshots, usize::MAX, 256);
        let many = build_all(&rows, &snapshots, 3, 256);
        assert_same_entries(&one.entries, &many.entries, &format!("seed {seed}"));
        assert_eq!(one.skip_revs, many.skip_revs, "seed {seed}");
        pending_resumes += many.pending_resumes;
    }
    eprintln!("resumes from a periodic checkpoint holding pending content: {pending_resumes}");
    assert!(
        pending_resumes >= 50,
        "only {pending_resumes} resumes held pending content"
    );
}

/// The replay from `cp` (its pending rows re-applied) through `end`, gap
/// states merged at their revs.
fn window(
    cp: &CheckpointWrite,
    rows: &[DocUpdate],
    end: i64,
    gaps: &[CheckpointWrite],
    skip: &BTreeSet<i64>,
) -> Replayer {
    let from = cp.rev;
    replay_window(WindowInput {
        mode: ReplayMode::Gc,
        checkpoint_state: cp.state_bytes.clone(),
        reapply: reapply_of(cp, rows),
        rows: rows_in(rows, from, end),
        gap_states: gaps
            .iter()
            .filter(|g| g.kind == CheckpointKind::Gap && from < g.rev && g.rev <= end)
            .map(|g| (g.rev, g.state_bytes.clone()))
            .collect(),
        skip_revs: skip.clone(),
        end_rev: end,
        merge_gap_at_end: true,
        progress: Progress::default(),
    })
    .expect("the window replays")
}

fn same_replay(a: &Replayer, b: &Replayer) -> bool {
    let ds = |r: &Replayer| r.doc().transact().snapshot().delete_set;
    let sv = |r: &Replayer| r.doc().transact().state_vector();
    sv(a) == sv(b) && ds(a) == ds(b) && visible_content(a.doc()) == visible_content(b.doc())
}

#[test]
fn checkpoints_equal_a_fresh_replay_to_their_rev() {
    let mut checked = 0;
    for seed in 1..=100u64 {
        let (rows, snapshots) = generated(seed, 50);
        let b = build_all(&rows, &snapshots, 3, 256);
        let genesis = &b.checkpoints[0];
        let head = rows.last().and_then(|r| r.rev).unwrap_or(0);
        for cp in &b.checkpoints {
            let decoded = Replayer::from_state(ReplayMode::Gc, &cp.state_bytes).unwrap();
            let fresh = window(genesis, &rows, cp.rev, &b.checkpoints, &b.skip_revs);
            assert!(same_replay(&decoded, &fresh), "seed {seed}, rev {}", cp.rev);
            let resumed = window(cp, &rows, cp.rev, &b.checkpoints, &b.skip_revs);
            assert!(same_replay(&resumed, &fresh), "seed {seed}, rev {}", cp.rev);
            assert_eq!(resumed.has_pending(), fresh.has_pending(), "seed {seed}");
            let on = window(cp, &rows, head, &b.checkpoints, &b.skip_revs);
            let all = window(genesis, &rows, head, &b.checkpoints, &b.skip_revs);
            assert!(same_replay(&on, &all), "seed {seed}, from rev {}", cp.rev);
            assert_eq!(on.has_pending(), all.has_pending(), "seed {seed}");
            checked += 1;
        }
    }
    assert!(checked > 300, "only {checked} checkpoints");
}

#[test]
fn checkpoints_follow_the_policy() {
    let floor = 256;
    for seed in 1..=100u64 {
        let (rows, _) = generated(seed, 60);
        let genesis = empty_state();
        let out = pass(input(genesis.clone(), &rows));
        let bytes_of = |from: i64, to: i64| -> u64 {
            rows_in(&rows, from, to)
                .iter()
                .map(|r| r.update_bytes.len() as u64)
                .sum()
        };
        let mut prev_rev = 0;
        let mut prev_size = genesis.len();
        for cp in out.checkpoints.iter() {
            assert_eq!(cp.kind, CheckpointKind::Periodic, "seed {seed}");
            let bytes = bytes_of(prev_rev, cp.rev);
            assert!(
                bytes >= checkpoint_threshold(floor, cp.state_bytes.len()),
                "seed {seed}, rev {}",
                cp.rev
            );
            let encodes: Vec<_> = out
                .stats
                .encodes
                .iter()
                .filter(|(r, _)| prev_rev < *r && *r <= cp.rev)
                .collect();
            let before = encodes
                .iter()
                .filter(|(r, _)| *r < cp.rev)
                .map(|(_, s)| *s)
                .next_back()
                .unwrap_or(prev_size);
            // Encodes are at least `floor` row bytes apart (Rulings), so a
            // crossing can wait up to `floor` bytes for its encode.
            let row = bytes_of(cp.rev - 1, cp.rev);
            assert!(
                bytes <= checkpoint_threshold(floor, before) + floor + row,
                "seed {seed}, rev {}",
                cp.rev
            );
            assert!(
                encodes.len() as u64 <= bytes / floor + 2,
                "seed {seed}, rev {}: {} encodes over {bytes} bytes since rev {prev_rev}: {encodes:?}; rows {:?}",
                cp.rev,
                encodes.len(),
                rows_in(&rows, prev_rev, cp.rev)
                    .iter()
                    .map(|r| r.update_bytes.len())
                    .collect::<Vec<_>>()
            );
            prev_rev = cp.rev;
            prev_size = cp.state_bytes.len();
        }
        assert!(!out.checkpoints.is_empty(), "seed {seed}");
    }
}

// ---------------------------------------------------------------------------
// Genesis
// ---------------------------------------------------------------------------

fn state_of(rows: &[&[u8]]) -> Vec<u8> {
    let mut r = Replayer::new(ReplayMode::Gc);
    for u in rows {
        r.apply(u);
    }
    r.encode_state()
}

#[test]
fn genesis_applies_rows_the_baseline_lacks_below_its_seq() {
    let mut h = History::new(15);
    for i in 0..10 {
        h.type_para(i % 3, &format!("legacy line {i}"), at(-200 + i as i64));
    }
    // The baseline (snapshot_seq 10) lacks row 8: 0.9.x's late-seq skip.
    let without_b: Vec<&[u8]> = h
        .rows()
        .iter()
        .filter(|r| r.rev != Some(8))
        .map(|r| r.update_bytes.as_slice())
        .collect();
    let baseline = state_of(&without_b);
    assert!(!xml_of_state(&baseline).contains("legacy line 7"));
    let mut inp = genesis_input(10, h.rows());
    inp.baselines = vec![BaselineInput {
        id: 1,
        snapshot_seq: 10,
        state_bytes: Some(baseline),
        created_at: at(-150),
    }];
    let g = build_genesis(inp).expect("genesis");
    assert!(xml_of_state(&g.checkpoint.state_bytes).contains("legacy line 7"));

    h.type_para(0, "after rollout", at(0));
    let mut inp = input(g.checkpoint.state_bytes.clone(), &h.rows()[10..]);
    inp.checkpoint_rev = 10;
    inp.genesis_rev = 10;
    inp.built_rev = 10;
    inp.snapshot_checked_rev = 10;
    inp.snapshots = vec![snapshot(11, at(1), h.room_snapshot())];
    let out = pass(inp);
    assert_eq!(out.stats.gaps, 0);
    assert!(of_kind(&out.entries, EntryKind::Gap).is_empty());
    assert_eq!(out.snapshot_checked_rev, 11);
}

fn rollout_entries(g: &GenesisOutput) -> Vec<&EntryDraft> {
    g.entries
        .iter()
        .filter(|e| e.meta.get("rollout").is_some())
        .collect()
}

#[test]
fn genesis_writes_baseline_entries_and_the_rollout_state_only_when_it_differs() {
    let mut h = History::new(16);
    h.type_para(0, "in the baseline", at(-300));
    let baseline = h.room_snapshot();
    h.type_para(1, "after the baseline", at(-200));
    let baselines = |with_bytes: bool| {
        vec![
            BaselineInput {
                id: 1,
                snapshot_seq: 0,
                state_bytes: None,
                created_at: at(-400),
            },
            BaselineInput {
                id: 2,
                snapshot_seq: 1,
                state_bytes: with_bytes.then(|| baseline.clone()),
                created_at: at(-250),
            },
        ]
    };

    // Genesis differs from the baseline it started from.
    let mut inp = genesis_input(2, h.rows());
    inp.baselines = baselines(true);
    let g = build_genesis(inp).expect("genesis");
    let base: Vec<_> = g
        .entries
        .iter()
        .filter(|e| e.baseline_id.is_some())
        .collect();
    assert_eq!(base.len(), 2);
    for (e, (id, t)) in base.iter().zip([(1, at(-400)), (2, at(-250))]) {
        assert_eq!(e.kind, EntryKind::Baseline);
        assert_eq!(e.baseline_id, Some(id));
        assert_eq!((e.started_at, e.ended_at), (t, t));
        assert_eq!((e.author, e.start_rev, e.end_rev), (None, None, None));
        assert!(e.sealed && e.inserted.is_empty() && e.deleted.is_empty());
        assert_eq!(e.meta, json!({}));
    }
    let rollout = rollout_entries(&g);
    assert_eq!(rollout.len(), 1);
    let r = rollout[0];
    assert_eq!(r.kind, EntryKind::Baseline);
    assert_eq!(r.meta, json!({"rollout": true}));
    assert_eq!((r.baseline_id, r.author, r.start_rev), (None, None, None));
    assert_eq!(r.end_rev, Some(2));
    assert_eq!((r.started_at, r.ended_at), (at(-60), at(-60)));
    assert!(r.sealed);

    // Equal content: no rollout entry.
    let mut inp = genesis_input(1, &h.rows()[..1]);
    inp.baselines = baselines(true);
    let g = build_genesis(inp).expect("genesis");
    assert!(rollout_entries(&g).is_empty());
    assert_eq!(g.entries.len(), 2);

    // No baseline to compare with: the rollout entry.
    let g = build_genesis(genesis_input(1, &h.rows()[..1])).expect("genesis");
    assert_eq!(rollout_entries(&g).len(), 1);
}

#[test]
fn genesis_of_a_new_page_is_an_empty_checkpoint_at_rev_zero() {
    let g = build_genesis(GenesisInput {
        page_created_at: at(0),
        history_since: at(0),
        genesis_rev: 0,
        baselines: Vec::new(),
        rows: Vec::new(),
        skip_revs: BTreeSet::new(),
        progress: Progress::default(),
        hooks: ReplayHooks::default(),
    })
    .expect("genesis");
    assert_eq!(g.checkpoint.rev, 0);
    assert_eq!(g.checkpoint.kind, CheckpointKind::Genesis);
    let r = Replayer::from_state(ReplayMode::Gc, &g.checkpoint.state_bytes).unwrap();
    assert_eq!(r.state_vector(), StateVector::default().encode_v1());
    assert_eq!(g.checkpoint.state_vector, r.state_vector());
    assert!(g.entries.is_empty());
    assert!(g.claims.is_empty());
    assert!(g.refused_revs.is_empty());
}

#[test]
fn a_legacy_row_that_corrupts_genesis_is_skipped_and_no_checkpoint_is_written() {
    let mut h = History::new(17);
    for i in 0..3 {
        h.type_para(i, &format!("legacy {i}"), at(-100 + i as i64));
    }
    let mut inp = genesis_input(3, h.rows());
    inp.hooks.corrupt_at_rev = Some(2);
    assert!(matches!(
        build_genesis(inp),
        Err(GenesisError::Corrupt { rev: 2 })
    ));

    let mut inp = genesis_input(3, h.rows());
    inp.hooks.corrupt_at_rev = Some(2);
    inp.skip_revs = BTreeSet::from([2]);
    let g = build_genesis(inp).expect("genesis");
    assert_eq!(g.stats.unapplied.get("skipped"), Some(&1));
    let xml = xml_of_state(&g.checkpoint.state_bytes);
    assert!(xml.contains("legacy 0") && xml.contains("legacy 2"));
    assert!(!xml.contains("legacy 1"));
}

#[test]
fn genesis_records_a_claim_per_parking_legacy_row() {
    let mut h = History::new(18);
    let (_, first) = text_on_unlogged_origin(&h, "one ");
    h.edit_at(1, first, at(-100));
    h.type_para(0, "independent", at(-99));
    let (_, third) = text_on_unlogged_origin(&h, "three ");
    h.edit_at(1, third, at(-98));
    let g = build_genesis(genesis_input(3, h.rows())).expect("genesis");
    assert_eq!(g.claims.revs(), vec![1, 3]);
    assert!(g.claims.iter().all(|c| c.author.is_none() && !c.gap));
    assert_eq!(g.stats.parked, 2);
}

#[test]
fn genesis_reports_a_baseline_that_does_not_apply() {
    let mut h = History::new(19);
    h.type_para(0, "old baseline", at(-300));
    let older = h.room_snapshot();
    h.type_para(1, "a legacy row", at(-200));
    let rejected = vec![0xff, 0xff, 0xff];
    let baseline = |id: i64, seq: i64, bytes: Option<Vec<u8>>| BaselineInput {
        id,
        snapshot_seq: seq,
        state_bytes: bytes,
        created_at: at(-250 + id),
    };

    let mut inp = genesis_input(2, h.rows());
    inp.baselines = vec![baseline(1, 1, None), baseline(2, 2, Some(rejected))];
    assert!(matches!(
        build_genesis(inp),
        Err(GenesisError::BaselineRejected { id: 2 })
    ));

    let mut inp = genesis_input(2, h.rows());
    inp.baselines = vec![baseline(1, 1, Some(older.clone())), baseline(2, 2, None)];
    let g = build_genesis(inp).expect("genesis");
    let rows: Vec<&[u8]> = h.rows().iter().map(|r| r.update_bytes.as_slice()).collect();
    let mut expected = Replayer::new(ReplayMode::Gc);
    expected.merge_state(&older);
    for u in rows {
        expected.apply(u);
    }
    let got = Replayer::from_state(ReplayMode::Gc, &g.checkpoint.state_bytes).unwrap();
    assert!(same_replay(&got, &expected));
    assert_eq!(
        g.entries.iter().filter(|e| e.baseline_id.is_some()).count(),
        2
    );
}

#[test]
fn checkpoint_kinds_use_their_sql_spellings() {
    for k in [
        CheckpointKind::Genesis,
        CheckpointKind::Periodic,
        CheckpointKind::Gap,
    ] {
        assert_eq!(k.as_str().parse::<CheckpointKind>().unwrap(), k);
    }
    assert!("other".parse::<CheckpointKind>().is_err());
}
