//! Page history M1.3, Task 5: replay throughput, the checkpoint policy and
//! the doc-memory factor F that set `DEFAULT_HISTORY_REPLAY_MAX_BYTES`;
//! M1.4, Task 3: the memory factors of a version view; M2, Task 2: the
//! revert's memory factor and its time cap.
//!
//! ```text
//! cargo run --release -p knot-history --example replay_throughput
//! ```
//!
//! A counting global allocator keeps the current and the peak heap, so
//! every workload reports its peak heap above the level it started at (its
//! inputs already loaded), not an encoded size. The workloads:
//!
//! 1. typing: 3 clients, 100,000 rows of one to three characters, each at
//!    the end of the client's own paragraph, one `run_pass` (GC replay);
//! 2. pastes: 300 rows of 100 KiB paragraphs, then 100 rows each deleting
//!    one of them, one `run_pass` (GC replay);
//! 3. policy: a page typed to 1 MiB of state, built in passes of 5,000
//!    rows, so the real checkpoint policy places the checkpoints; then a
//!    replay from the latest checkpoint at or before each of 20 heads (and
//!    the rev before each checkpoint, each interval's worst head) to that
//!    head, with the GC replay of a pass prefix and the skip-GC replay of a
//!    word count or a view;
//! 4. memory, on two 4 MiB states (interleaved three-client one-character
//!    typing, whose items never squash, and pastes with deletions):
//!    (a) one `run_pass` from a checkpoint with a 1 MiB prefix, 1 MiB of new
//!    rows, a late snapshot whose probe finds a gap, a snapshot whose probe
//!    is refused so `union` runs (`skip_underflow_case`'s shape), and the
//!    gap checkpoints those emit; (b) one word count: `replay_window` with
//!    `KeepDeleted` over the same input, then `entry_word_counts` on a
//!    session holding a third of the new rows. F is the largest of the four
//!    factors (input bytes + peak heap above the start) ÷ input bytes,
//!    rounded up: the builder holds its loaded input while it computes;
//! 5. a page holding a 1 MiB row parked for good (its paragraph's creator
//!    never delivered it) under 22,000 typing rows, built in passes of
//!    5,000 rows, against the same typing without it: `PassStats::retried`
//!    and rows/s (Task 4's review);
//! 6. memory again on the same two 4 MiB states, for inputs that are mostly
//!    the starting checkpoint (final review round 1: workload 4's input was
//!    mostly rows and snapshots, which cost less per byte than a decoded
//!    checkpoint): (a) a pass over the checkpoint and 20 rows, (b) the same
//!    with a late snapshot whose probe finds a gap, (c) a word count over
//!    (b)'s output. F is the largest factor of workloads 4 and 6;
//! 7. views and restores (M1.4, Task 3), on workload 6's two pages: the
//!    plain and the Show changes view of a session's window, a baseline
//!    view of the state, a restore (the replay, `encode_state` and the
//!    Markdown round trip), and the Show changes views of one row deleting
//!    one client's text (an undo) or the whole page; then a plain view of
//!    a page whose every other word is bold. Each view's span includes the
//!    JSON's serialization. They set `view::VIEW_MEMORY_FACTOR` and
//!    `view::PLAIN_MEMORY_FACTOR`, and time the Show changes views on a
//!    page of the diff cap's size. Then (M1.4, Task 8) a baseline that may
//!    be shifted, near 4 MiB: (h) its view and (i) its restore, each
//!    through `view::state_input`, summarized apart from the others.
//! 8. reverts (M2, Task 2): `revert::plan` on workload 6's two pages
//!    grown to 4 MiB plus a session of 20 small rows, against the live
//!    room's state: (a) to the version a quarter of the way in (the large
//!    undo), (b) to the version before the session (the small undo), (c)
//!    (a) by the whole-page replace (`skip_undo`), (d) to the head (the
//!    unchanged check) and (c′) (a) with an undo that fails verification
//!    and then the replace (`corrupt_undo`), input = the version's state
//!    plus the live state: they set `view::REVERT_MEMORY_FACTOR`; then (e)
//!    (a) and (c′) timed on pages scaled to 1×, 2× and 4× the memory cap at
//!    the default replay cap, which set `view::REVERT_TIME_CAP_BYTES`; then
//!    (f) a restore's replay with M2's finishes (`revert::target`,
//!    `revert::copy_state`) on a session's window and the state of both
//!    pages and on workload 7's shifted baseline, checked against
//!    `PLAIN_MEMORY_FACTOR` and `SHIFTED_MEMORY_FACTOR`.
//!
//! The cap rule: the largest power of two at most min(the slower of
//! workloads 1-2 × 2 s, (512 MiB − 256 MiB) ÷ (1 worker × F)), at most
//! 32 MiB. One worker is the default; the [8, 32] MiB clamp of the M1.3
//! plan is gone, since at F > 32 its 8 MiB floor does not fit even one
//! worker in the chart's 512 Mi limit.

#[path = "../../knot-crdt/tests/common/fixtures.rs"]
mod fixtures;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use chrono::{DateTime, Duration, TimeZone, Utc};
use knot_crdt::{Engine, YrsEngine};
use knot_history::revert::{self, RevertHooks, RevertKind, RevertOutcome, RevertTarget};
use knot_history::view::{
    BUILD_MEMORY_FACTOR, PLAIN_MEMORY_FACTOR, REVERT_MEMORY_FACTOR, REVERT_TIME_CAP_BYTES,
    RenderHooks, SHIFTED_MEMORY_FACTOR, VIEW_MEMORY_FACTOR, ViewInput, render,
    revert_max_input_bytes, state_input, view_max_input_bytes,
};
use knot_history::{
    CHECKPOINT_FLOOR_BYTES, CheckpointKind, CheckpointWrite, Claims, EntryDraft, EntryKind,
    PassInput, PassOutput, Progress, ReplayHooks, ReplayMode, Replayer, RowOutcome, SnapshotProbe,
    WindowInput, checkpoint_threshold, count_words, entry_diff, replay_window, run_pass,
    text_chunks,
};
use knot_markdown::{from_markdown, to_markdown};
use knot_storage::{DocUpdate, UpdateSource};
use uuid::Uuid;
use yrs::block::ClientID;
use yrs::encoding::write::Write;
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{
    Any, Doc, ID, IdSet, ReadTxn, StateVector, Text, Transact, Update, XmlElementPrelim,
    XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
};

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrank(by: usize) {
    CURRENT.fetch_sub(by, Ordering::Relaxed);
}

/// `System`, keeping the current and the peak allocated bytes.
struct Counting;

// SAFETY: every call forwards to `System` with the caller's arguments and
// only adds bookkeeping on the side.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                shrank(layout.size() - new_size);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Starts a measurement: the peak becomes the current level, returned.
fn reset_peak() -> usize {
    let now = CURRENT.load(Ordering::Relaxed);
    PEAK.store(now, Ordering::Relaxed);
    now
}

/// A computation's result, wall time and peak heap above its start.
struct Measured<T> {
    value: T,
    secs: f64,
    peak: usize,
}

fn measure<T>(f: impl FnOnce() -> T) -> Measured<T> {
    let start = reset_peak();
    let t = Instant::now();
    let value = f();
    let secs = t.elapsed().as_secs_f64();
    Measured {
        value,
        secs,
        peak: PEAK.load(Ordering::Relaxed).saturating_sub(start),
    }
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

const MIB: usize = 1024 * 1024;
const KIB: usize = 1024;
/// `PASS_MAX_ROWS` (Global Constraints; Task 7 defines it in the builder).
const PASS_MAX_ROWS: usize = 5_000;
/// The chart's memory limit, the web server's allowance and the workers
/// the default cap must fit (Rulings).
const MEMORY_LIMIT: usize = 512 * MIB;
const WEB_ALLOWANCE: usize = 256 * MIB;
const CAP_WORKERS: usize = 1;
/// Seconds of the slower replay workload the cap may hold.
const CAP_SECONDS: f64 = 2.0;
/// The bound on workload 3's worst replay.
const POLICY_BOUND_SECS: f64 = 0.5;

const DOC_ID: Uuid = Uuid::from_u128(0x6b6e_6f74_0000_0000_0000_0000_0000_0005);
/// Client ids in the range real yrs clients pick (32-bit), so ids cost
/// what they do in production rows.
const CLIENTS: [u64; 3] = [0x3a41_7c21, 0x9b02_5e17, 0x5cd3_08f4];

fn author(i: usize) -> Uuid {
    Uuid::from_u128(0xa000 + i as u128)
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 1, 9, 0, 0).unwrap() + Duration::seconds(secs)
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / MIB as f64
}

fn bytes_of(rows: &[DocUpdate]) -> usize {
    rows.iter().map(|r| r.update_bytes.len()).sum()
}

fn rev_of(row: &DocUpdate) -> i64 {
    row.rev.expect("numbered row")
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// `n` characters of words and spaces.
    fn text(&mut self, n: usize) -> String {
        (0..n)
            .map(|_| {
                if self.below(6) == 0 {
                    ' '
                } else {
                    char::from(b'a' + self.below(26) as u8)
                }
            })
            .collect()
    }
}

/// The page's log: rows one second apart, so every author's session stays
/// open while the workload runs.
#[derive(Default)]
struct Log {
    rows: Vec<DocUpdate>,
}

impl Log {
    fn push(&mut self, who: usize, bytes: Vec<u8>) -> i64 {
        let rev = self.rows.len() as i64 + 1;
        self.rows.push(DocUpdate {
            seq: rev,
            doc_id: DOC_ID,
            update_bytes: bytes,
            by_user_id: Some(author(who)),
            created_at: at(rev),
            rev: Some(rev),
            source: Some(UpdateSource::Edit),
            meta: None,
            epoch: Some(0),
        });
        rev
    }

    fn bytes(&self) -> usize {
        bytes_of(&self.rows)
    }
}

/// Renumbers `rows` from rev 1, one second apart.
fn renumbered(rows: Vec<DocUpdate>) -> Vec<DocUpdate> {
    rows.into_iter()
        .enumerate()
        .map(|(i, mut r)| {
            let rev = i as i64 + 1;
            r.seq = rev;
            r.rev = Some(rev);
            r.created_at = at(rev);
            r
        })
        .collect()
}

/// A doc's blocks and delete set, without pending content (what a room
/// snapshot and a checkpoint hold).
fn own_state(doc: &Doc) -> Vec<u8> {
    let mut encoder = EncoderV1::new();
    doc.transact()
        .encode_state_as_update(&StateVector::default(), &mut encoder);
    encoder.to_vec()
}

fn apply(doc: &Doc, bytes: &[u8]) {
    doc.transact_mut()
        .apply_update(Update::decode_v1(bytes).expect("decode"))
        .expect("apply");
}

/// The text of the last paragraph of `frag`.
fn last_text(frag: &XmlFragmentRef, txn: &impl ReadTxn) -> Option<XmlTextRef> {
    let n = frag.len(txn);
    if n == 0 {
        return None;
    }
    match frag.get(txn, n - 1)? {
        XmlOut::Element(p) => match p.get(txn, 0)? {
            XmlOut::Text(t) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

/// Three editors whose every transaction is delivered to the others at
/// once, as in a room: their docs are the room's state after each row.
struct Editors {
    docs: Vec<Doc>,
    frags: Vec<XmlFragmentRef>,
    /// Each client's own paragraph (`type_own`).
    cur: Vec<Option<XmlTextRef>>,
    para_len: Vec<usize>,
    para_target: Vec<usize>,
    rng: Rng,
}

impl Editors {
    fn new(seed: u64) -> Editors {
        let docs: Vec<Doc> = CLIENTS.iter().map(|c| Doc::with_client_id(*c)).collect();
        let frags = docs
            .iter()
            .map(|d| d.get_or_insert_xml_fragment("default"))
            .collect();
        Editors {
            docs,
            frags,
            cur: vec![None, None, None],
            para_len: vec![0; 3],
            para_target: vec![0; 3],
            rng: Rng::new(seed),
        }
    }

    fn deliver(&self, from: usize, bytes: &[u8]) {
        for (i, doc) in self.docs.iter().enumerate() {
            if i != from {
                apply(doc, bytes);
            }
        }
    }

    /// The room's state.
    fn state(&self) -> Vec<u8> {
        own_state(&self.docs[0])
    }

    fn paragraphs(&self) -> u32 {
        self.frags[0].len(&self.docs[0].transact())
    }

    /// Client `c` types one to three characters at the end of its own
    /// paragraph, opening a new one every 40 to 200 characters: its items
    /// squash, as one person's typing does.
    fn type_own(&mut self, c: usize) -> Vec<u8> {
        let n = 1 + self.rng.below(3);
        let s = self.rng.text(n);
        let bytes = {
            let mut txn = self.docs[c].transact_mut();
            if self.cur[c].is_none() || self.para_len[c] >= self.para_target[c] {
                let p = self.frags[c].push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                self.cur[c] = Some(p.push_back(&mut txn, XmlTextPrelim::new("")));
                self.para_len[c] = 0;
                self.para_target[c] = 40 + self.rng.below(160);
            }
            let t = self.cur[c].as_ref().expect("a paragraph");
            let len = t.len(&txn);
            t.insert(&mut txn, len, &s);
            self.para_len[c] += n;
            txn.encode_update_v1()
        };
        self.deliver(c, &bytes);
        bytes
    }

    /// Client `c` types one character at the end of the page's last
    /// paragraph, after the other clients' characters: with three clients
    /// taking turns no two items of one client are adjacent, so none
    /// squash. A new paragraph every 400 characters.
    fn type_shared(&mut self, c: usize) -> Vec<u8> {
        let s = self.rng.text(1);
        let bytes = {
            let mut txn = self.docs[c].transact_mut();
            let t = match last_text(&self.frags[c], &txn) {
                Some(t) if t.len(&txn) < 400 => t,
                _ => {
                    let p = self.frags[c].push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                    p.push_back(&mut txn, XmlTextPrelim::new(""))
                }
            };
            let len = t.len(&txn);
            t.insert(&mut txn, len, &s);
            txn.encode_update_v1()
        };
        self.deliver(c, &bytes);
        bytes
    }

    /// Client `c` pastes a new paragraph of `n` bytes at the end.
    fn paste(&mut self, c: usize, n: usize) -> Vec<u8> {
        let s = self.rng.text(n);
        let bytes = {
            let mut txn = self.docs[c].transact_mut();
            let p = self.frags[c].push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(s));
            txn.encode_update_v1()
        };
        self.deliver(c, &bytes);
        bytes
    }

    /// Client `c` deletes a random paragraph.
    fn delete_paragraph(&mut self, c: usize) -> Vec<u8> {
        let i = self.rng.below(self.paragraphs() as usize) as u32;
        let bytes = {
            let mut txn = self.docs[c].transact_mut();
            self.frags[c].remove_range(&mut txn, i, 1);
            txn.encode_update_v1()
        };
        self.deliver(c, &bytes);
        bytes
    }
}

// ---------------------------------------------------------------------------
// Passes
// ---------------------------------------------------------------------------

fn empty_state() -> Vec<u8> {
    Replayer::new(ReplayMode::Gc).encode_state()
}

/// One pass from `checkpoint_state` at rev 0 over every row, as the first
/// pass after a new page's genesis.
fn one_pass(checkpoint_state: Vec<u8>, rows: Vec<DocUpdate>) -> PassInput {
    let now = rows.last().map_or(at(0), |r| r.created_at) + Duration::hours(2);
    PassInput {
        checkpoint_rev: 0,
        checkpoint_state,
        reapply: Vec::new(),
        replay: Vec::new(),
        genesis_rev: 0,
        built_rev: 0,
        rows,
        rows_exhausted: true,
        open: Vec::new(),
        claims: Claims::default(),
        kept_gaps: Vec::new(),
        snapshots: Vec::new(),
        snapshot_checked_rev: 0,
        skip_revs: BTreeSet::new(),
        progress: Progress::default(),
        now,
        rows_budget: None,
        checkpoint_floor: CHECKPOINT_FLOOR_BYTES,
        hooks: ReplayHooks::default(),
    }
}

fn rows_in(rows: &[DocUpdate], after: i64, through: i64) -> Vec<DocUpdate> {
    rows.iter()
        .filter(|r| after < rev_of(r) && rev_of(r) <= through)
        .cloned()
        .collect()
}

fn reapply_of(cp: &CheckpointWrite, rows: &[DocUpdate]) -> Vec<DocUpdate> {
    rows.iter()
        .filter(|r| cp.pending_revs.contains(&rev_of(r)))
        .cloned()
        .collect()
}

/// A page built the way the builder stores it, pass after pass.
struct Page {
    checkpoints: BTreeMap<i64, CheckpointWrite>,
    open: Vec<EntryDraft>,
    claims: Claims,
    built_rev: i64,
    passes: usize,
    encodes: usize,
    retried: u64,
    compute_secs: f64,
    peak: usize,
}

impl Page {
    fn new() -> Page {
        let empty = Replayer::new(ReplayMode::Gc);
        let genesis = CheckpointWrite {
            rev: 0,
            kind: CheckpointKind::Genesis,
            state_bytes: empty.encode_state(),
            state_vector: empty.state_vector(),
            pending_revs: Vec::new(),
        };
        Page {
            checkpoints: BTreeMap::from([(0, genesis)]),
            open: Vec::new(),
            claims: Claims::default(),
            built_rev: 0,
            passes: 0,
            encodes: 0,
            retried: 0,
            compute_secs: 0.0,
            peak: 0,
        }
    }

    /// The latest checkpoint at or below `rev`.
    fn start(&self, rev: i64) -> &CheckpointWrite {
        self.checkpoints
            .range(..=rev)
            .next_back()
            .map(|(_, c)| c)
            .expect("a checkpoint")
    }

    /// Builds every row, `max_rows` new rows per pass.
    fn build(rows: &[DocUpdate], max_rows: usize) -> Page {
        let mut page = Page::new();
        let head = rows.last().map_or(0, rev_of);
        let now = rows.last().map_or(at(0), |r| r.created_at) + Duration::hours(2);
        while page.built_rev < head {
            let start = page.start(page.built_rev).clone();
            let new: Vec<DocUpdate> = rows
                .iter()
                .filter(|r| rev_of(r) > page.built_rev)
                .take(max_rows)
                .cloned()
                .collect();
            let last = new.last().map_or(page.built_rev, rev_of);
            let input = PassInput {
                checkpoint_rev: start.rev,
                checkpoint_state: start.state_bytes.clone(),
                reapply: reapply_of(&start, rows),
                replay: rows_in(rows, start.rev, page.built_rev),
                genesis_rev: 0,
                built_rev: page.built_rev,
                rows: new,
                rows_exhausted: last == head,
                open: page.open.clone(),
                claims: page.claims.clone(),
                kept_gaps: Vec::new(),
                snapshots: Vec::new(),
                snapshot_checked_rev: 0,
                skip_revs: BTreeSet::new(),
                progress: Progress::default(),
                now,
                rows_budget: None,
                checkpoint_floor: CHECKPOINT_FLOOR_BYTES,
                hooks: ReplayHooks::default(),
            };
            let m = measure(|| run_pass(input).expect("a pass"));
            page.absorb(m.value);
            page.compute_secs += m.secs;
            page.peak = page.peak.max(m.peak);
        }
        page
    }

    fn absorb(&mut self, out: PassOutput) {
        for cp in out.checkpoints {
            self.checkpoints.insert(cp.rev, cp);
        }
        let mut open: BTreeMap<Option<Uuid>, EntryDraft> =
            self.open.drain(..).map(|e| (e.author, e)).collect();
        for e in out
            .entries
            .into_iter()
            .filter(|e| e.kind == EntryKind::Session)
        {
            if e.sealed {
                open.remove(&e.author);
            } else {
                open.insert(e.author, e);
            }
        }
        self.open = open.into_values().collect();
        self.claims = out.claims;
        self.built_rev = out.built_rev;
        self.passes += 1;
        self.encodes += out.stats.encodes.len();
        self.retried += out.stats.retried;
    }
}

/// A window replay from the latest checkpoint at or before `head` to it.
fn window(page: &Page, rows: &[DocUpdate], head: i64, mode: ReplayMode) -> WindowInput {
    let cp = page.start(head);
    WindowInput {
        mode,
        checkpoint_state: cp.state_bytes.clone(),
        reapply: reapply_of(cp, rows),
        rows: rows_in(rows, cp.rev, head),
        gap_states: Vec::new(),
        skip_revs: BTreeSet::new(),
        end_rev: head,
        merge_gap_at_end: false,
        progress: Progress::default(),
        hooks: ReplayHooks::default(),
    }
}

// ---------------------------------------------------------------------------
// Workloads 1 and 2: throughput
// ---------------------------------------------------------------------------

struct Throughput {
    mib_per_sec: f64,
}

fn throughput(name: &str, rows: Vec<DocUpdate>, peak_doc: usize) -> Throughput {
    let n = rows.len();
    let input_bytes = bytes_of(&rows);
    // The rows alone, without attribution or the checkpoint policy.
    let plain = measure(|| {
        let mut r = Replayer::new(ReplayMode::Gc);
        for row in &rows {
            r.apply(&row.update_bytes);
        }
    });
    let m = measure(|| run_pass(one_pass(empty_state(), rows)).expect("the pass"));
    let out = m.value;
    let rows_per_sec = n as f64 / m.secs;
    let mib_per_sec = mib(input_bytes) / m.secs;
    println!("== {name}");
    println!(
        "  rows {n}, input {:.2} MiB (mean row {} B), checkpoints {}, encodes {}",
        mib(input_bytes),
        input_bytes / n.max(1),
        out.checkpoints.len(),
        out.stats.encodes.len(),
    );
    println!(
        "  pass {:.3} s: {rows_per_sec:.0} rows/s, {mib_per_sec:.2} MiB/s",
        m.secs
    );
    println!(
        "  plain Replayer::apply of the rows {:.3} s: {:.2} MiB/s",
        plain.secs,
        mib(input_bytes) / plain.secs
    );
    println!(
        "  peak replay-doc bytes (encode_state) {:.2} MiB, peak heap {:.2} MiB, peak heap / input {:.2}",
        mib(peak_doc),
        mib(m.peak),
        m.peak as f64 / input_bytes as f64,
    );
    Throughput { mib_per_sec }
}

fn typing() -> Throughput {
    let mut ed = Editors::new(1);
    let mut log = Log::default();
    for _ in 0..100_000 {
        let c = ed.rng.below(3);
        let bytes = ed.type_own(c);
        log.push(c, bytes);
    }
    let peak_doc = ed.state().len();
    throughput(
        "1. typing (3 clients, 100,000 rows of 1-3 characters, GC replay)",
        log.rows,
        peak_doc,
    )
}

fn pastes() -> Throughput {
    let mut ed = Editors::new(2);
    let mut log = Log::default();
    for i in 0..300 {
        let bytes = ed.paste(i % 3, 100 * KIB);
        log.push(i % 3, bytes);
    }
    let peak_doc = ed.state().len();
    for i in 0..100 {
        let bytes = ed.delete_paragraph(i % 3);
        log.push(i % 3, bytes);
    }
    throughput(
        "2. pastes (300 rows of 100 KiB paragraphs, then 100 rows deleting a third of them, GC replay)",
        log.rows,
        peak_doc,
    )
}

// ---------------------------------------------------------------------------
// Workload 3: the checkpoint policy
// ---------------------------------------------------------------------------

struct Policy {
    worst_secs: f64,
}

fn policy() -> Policy {
    let mut ed = Editors::new(3);
    let mut log = Log::default();
    while !log.rows.len().is_multiple_of(10_000) || ed.state().len() < MIB {
        let c = ed.rng.below(3);
        let bytes = ed.type_own(c);
        log.push(c, bytes);
    }
    let rows = log.rows;
    let head = rows.last().map_or(0, rev_of);
    let log_bytes = bytes_of(&rows);
    let page = Page::build(&rows, PASS_MAX_ROWS);

    let periodic: Vec<&CheckpointWrite> = page
        .checkpoints
        .values()
        .filter(|c| c.kind == CheckpointKind::Periodic)
        .collect();
    let checkpoint_bytes: usize = periodic.iter().map(|c| c.state_bytes.len()).sum();
    let mean_threshold = periodic
        .iter()
        .map(|c| checkpoint_threshold(CHECKPOINT_FLOOR_BYTES, c.state_bytes.len()) as f64)
        .sum::<f64>()
        / periodic.len().max(1) as f64;
    let expected = log_bytes as f64 / mean_threshold;
    // Task 4's Ruling 3 spaces encodes at least a floor of row bytes apart,
    // so an interval can overshoot its threshold by up to a floor and a row.
    let max_row = rows.iter().map(|r| r.update_bytes.len()).max().unwrap_or(0);
    let expected_low =
        log_bytes as f64 / (mean_threshold + CHECKPOINT_FLOOR_BYTES as f64 + max_row as f64);
    let last_cp = periodic.last().map_or(0, |c| c.rev);
    let mean_interval = bytes_of(&rows_in(&rows, 0, last_cp)) as f64 / periodic.len().max(1) as f64;

    // 20 heads spread over the log, and each interval's worst head: the
    // rev before each checkpoint, and the head.
    let mut heads: BTreeSet<i64> = (1..=20).map(|i| head * i / 20).collect();
    heads.extend(periodic.iter().map(|c| c.rev - 1));
    heads.insert(head);
    let mut worst = [(0.0f64, 0i64); 2];
    for &h in &heads {
        for (i, mode) in [ReplayMode::Gc, ReplayMode::KeepDeleted]
            .into_iter()
            .enumerate()
        {
            let w = window(&page, &rows, h, mode);
            let t = Instant::now();
            let r = replay_window(w).expect("the window");
            let secs = t.elapsed().as_secs_f64();
            drop(r);
            if secs > worst[i].0 {
                worst[i] = (secs, h);
            }
        }
    }
    let worst_secs = worst[0].0.max(worst[1].0);

    println!("== 3. policy (typing to 1 MiB of state, passes of {PASS_MAX_ROWS} rows)");
    println!(
        "  rows {}, log {:.2} MiB, final state {:.2} MiB, passes {}, build {:.2} s ({:.0} rows/s), peak heap {:.2} MiB",
        rows.len(),
        mib(log_bytes),
        mib(ed.state().len()),
        page.passes,
        page.compute_secs,
        rows.len() as f64 / page.compute_secs,
        mib(page.peak),
    );
    println!(
        "  checkpoints {} periodic (+ genesis), their bytes / log bytes {:.2}, mean threshold {:.0} KiB, mean interval {:.0} KiB",
        periodic.len(),
        checkpoint_bytes as f64 / log_bytes as f64,
        mean_threshold / KIB as f64,
        mean_interval / KIB as f64,
    );
    println!(
        "  log / mean threshold {expected:.2}, log / (mean threshold + floor + largest row) {expected_low:.2}"
    );
    println!(
        "  stats.encodes.len() {} over the passes (bound 2 x checkpoints + passes = {})",
        page.encodes,
        2 * periodic.len() + page.passes
    );
    println!(
        "  worst replay over {} heads: GC {:.3} s (head {}), skip-GC {:.3} s (head {}); bound {POLICY_BOUND_SECS} s",
        heads.len(),
        worst[0].0,
        worst[0].1,
        worst[1].0,
        worst[1].1,
    );
    let count = periodic.len() as f64;
    assert!(
        expected_low - 1.0 <= count && count <= expected + 1.0,
        "{count} checkpoints, expected {expected_low:.2} - 1 ..= {expected:.2} + 1"
    );
    assert!(
        page.encodes <= 2 * periodic.len() + page.passes,
        "{} encodes",
        page.encodes
    );
    Policy { worst_secs }
}

// ---------------------------------------------------------------------------
// Workload 4: the doc-memory factor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Growth {
    /// Interleaved three-client one-character typing (unsquashed items).
    Typing,
    /// 100 KiB pastes, every fourth row deleting a paragraph.
    Pastes,
}

impl Growth {
    fn name(self) -> &'static str {
        match self {
            Growth::Typing => "interleaved one-character typing",
            Growth::Pastes => "pastes with deletions",
        }
    }

    fn row(self, ed: &mut Editors, i: usize) -> (usize, Vec<u8>) {
        let c = i % 3;
        let bytes = match self {
            Growth::Typing => ed.type_shared(c),
            Growth::Pastes if i % 4 == 3 => ed.delete_paragraph(c),
            Growth::Pastes => ed.paste(c, 100 * KIB),
        };
        (c, bytes)
    }
}

/// Peak heap factors of (a) a pass and (b) a word count.
fn memory(growth: Growth) -> [f64; 2] {
    let (doc_state, refused_row, refused_room) = fixtures::skip_underflow_case();
    let mut ed = Editors::new(4);
    let mut log = Log::default();
    let mut i = 0;
    // The checkpoint: the page grown to 4 MiB of state, plus the
    // underflow case's doc, which holds a block past a gap.
    let check_every = match growth {
        Growth::Typing => 20_000,
        Growth::Pastes => 1,
    };
    loop {
        let (c, bytes) = growth.row(&mut ed, i);
        log.push(c, bytes);
        i += 1;
        if i % check_every == 0 && ed.state().len() >= 4 * MIB {
            break;
        }
    }
    let checkpoint_rev = log.rows.len() as i64;
    let checkpoint_state = {
        let mut r = Replayer::from_state(ReplayMode::Gc, &doc_state).expect("the case's doc");
        assert_eq!(r.apply(&ed.state()).outcome, RowOutcome::Applied);
        r.encode_state()
    };
    // The 1 MiB prefix.
    let mark = log.bytes();
    while log.bytes() - mark < MIB {
        let (c, bytes) = growth.row(&mut ed, i);
        log.push(c, bytes);
        i += 1;
    }
    let built_rev = log.rows.len() as i64;
    // A late snapshot at `built_rev` holding a paragraph no row carries.
    let late = {
        let x = Doc::with_client_id(0x77);
        apply(&x, &ed.state());
        let frag = x.get_or_insert_xml_fragment("default");
        let mut txn = x.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(ed.rng.text(4 * KIB)));
        drop(txn);
        own_state(&x)
    };
    // 1 MiB of new rows: first the refused row, then the page's rows; a
    // snapshot halfway that holds the refused row's content squashed with
    // the blocks after it, so its probe is refused too and `union` runs.
    let refused_rev = log.push(0, refused_row);
    let mark = log.bytes();
    let mut union_snapshot = None;
    while log.bytes() - mark < MIB {
        let (c, bytes) = growth.row(&mut ed, i);
        log.push(c, bytes);
        i += 1;
        if union_snapshot.is_none() && log.bytes() - mark >= MIB / 2 {
            let y = Doc::new();
            apply(&y, &refused_room);
            apply(&y, &ed.state());
            union_snapshot = Some((log.rows.len() as i64, own_state(&y)));
        }
    }
    let (union_rev, union_state) = union_snapshot.expect("a snapshot halfway");
    let rows = log.rows;
    let prefix = rows_in(&rows, checkpoint_rev, built_rev);
    let new = rows_in(&rows, built_rev, i64::MAX);
    let snapshots = vec![
        SnapshotProbe {
            rev: built_rev,
            created_at: at(built_rev) + Duration::seconds(30),
            state_bytes: Some(late.clone()),
            legacy: false,
        },
        SnapshotProbe {
            rev: union_rev,
            created_at: at(union_rev) + Duration::seconds(30),
            state_bytes: Some(union_state.clone()),
            legacy: false,
        },
    ];
    let pass_input_bytes = checkpoint_state.len()
        + bytes_of(&prefix)
        + bytes_of(&new)
        + late.len()
        + union_state.len();
    let input = PassInput {
        checkpoint_rev,
        checkpoint_state: checkpoint_state.clone(),
        reapply: Vec::new(),
        replay: prefix.clone(),
        genesis_rev: 0,
        built_rev,
        rows: new.clone(),
        rows_exhausted: true,
        open: Vec::new(),
        claims: Claims::default(),
        kept_gaps: Vec::new(),
        snapshots,
        snapshot_checked_rev: checkpoint_rev,
        skip_revs: BTreeSet::new(),
        progress: Progress::default(),
        now: at(i64::try_from(rows.len()).unwrap() + 7200),
        rows_budget: None,
        checkpoint_floor: CHECKPOINT_FLOOR_BYTES,
        hooks: ReplayHooks::default(),
    };
    let a = measure(|| run_pass(input).expect("the pass"));
    let out = a.value;
    assert_eq!(out.stats.unapplied.get("skip_underflow"), Some(&1));
    assert_eq!(out.refused_revs, vec![refused_rev]);
    assert_eq!(out.stats.gaps, 2, "the late gap and the union");
    assert!(out.stats.snapshots_unchecked.is_empty());
    let gap_states: Vec<(i64, Vec<u8>)> = out
        .checkpoints
        .iter()
        .filter(|c| c.kind == CheckpointKind::Gap)
        .map(|c| (c.rev, c.state_bytes.clone()))
        .collect();
    assert_eq!(
        gap_states.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
        vec![built_rev, union_rev]
    );
    let factor_a = (pass_input_bytes + a.peak) as f64 / pass_input_bytes as f64;

    // (b) A word count over the same input: the session of author B, which
    // holds a third of the new rows.
    let session = out
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::Session && e.author == Some(author(1)))
        .expect("author B's session")
        .clone();
    let end_rev = session.end_rev.expect("an end rev");
    let window_rows = rows_in(&rows, checkpoint_rev, end_rev);
    let words_input_bytes = checkpoint_state.len()
        + bytes_of(&window_rows)
        + gap_states.iter().map(|(_, s)| s.len()).sum::<usize>();
    let checkpoint_copy = checkpoint_state.clone();
    let w = WindowInput {
        mode: ReplayMode::KeepDeleted,
        checkpoint_state,
        reapply: Vec::new(),
        rows: window_rows,
        gap_states,
        skip_revs: BTreeSet::from([refused_rev]),
        end_rev,
        merge_gap_at_end: false,
        progress: Progress::default(),
        hooks: ReplayHooks::default(),
    };
    // `entry_word_counts`, step by step, to see where the heap goes.
    let start = reset_peak();
    let t = Instant::now();
    let r = replay_window(w).expect("the window");
    // `replay_window` dropped its input.
    let held = || CURRENT.load(Ordering::Relaxed) + words_input_bytes - start;
    let window_heap = held();
    let d = entry_diff(r.doc(), &session.inserted, &session.deleted).expect("the diff");
    let diff_heap = held();
    let chunks = text_chunks(&d);
    let counts = count_words(&chunks);
    let words_secs = t.elapsed().as_secs_f64();
    let words_peak = PEAK.load(Ordering::Relaxed).saturating_sub(start);
    drop((chunks, d, r));
    let factor_b = (words_input_bytes + words_peak) as f64 / words_input_bytes as f64;
    // One decoded checkpoint, for scale.
    let decoded = measure(|| Replayer::from_state(ReplayMode::Gc, &checkpoint_copy).unwrap());
    drop(decoded.value);

    println!("== 4. memory: {}", growth.name());
    println!(
        "  checkpoint {:.2} MiB at rev {checkpoint_rev}, prefix {:.2} MiB, new rows {:.2} MiB ({} rows), snapshots {:.2} + {:.2} MiB",
        mib(pass_input_bytes - bytes_of(&prefix) - bytes_of(&new) - late.len() - union_state.len()),
        mib(bytes_of(&prefix)),
        mib(bytes_of(&new)),
        new.len(),
        mib(late.len()),
        mib(union_state.len()),
    );
    println!(
        "  (a) pass {:.3} s: input {:.2} MiB, peak heap above start {:.2} MiB, peak / input {:.2}, F_a = (input + peak) / input {:.2}; {} gap checkpoints",
        a.secs,
        mib(pass_input_bytes),
        mib(a.peak),
        a.peak as f64 / pass_input_bytes as f64,
        factor_a,
        out.checkpoints.len(),
    );
    println!(
        "  (b) word count {words_secs:.3} s ({} added, {} removed): input {:.2} MiB, peak heap above start {:.2} MiB, peak / input {:.2}, F_b = (input + peak) / input {factor_b:.2}",
        counts.added,
        counts.removed,
        mib(words_input_bytes),
        mib(words_peak),
        words_peak as f64 / words_input_bytes as f64,
    );
    println!(
        "      heap held: skip-GC replay {:.2} MiB, + union doc and snapshots {:.2} MiB; peak above start {:.2} MiB",
        mib(window_heap),
        mib(diff_heap.saturating_sub(window_heap)),
        mib(words_peak),
    );
    println!(
        "  one GC decode of the {:.2} MiB checkpoint: {:.2} MiB of heap ({:.1} x its bytes), {:.3} s",
        mib(checkpoint_copy.len()),
        mib(decoded.peak),
        decoded.peak as f64 / checkpoint_copy.len() as f64,
        decoded.secs,
    );
    [factor_a, factor_b]
}

// ---------------------------------------------------------------------------
// Workload 6: inputs that are mostly a checkpoint
// ---------------------------------------------------------------------------

/// Peak heap factors of passes and a word count whose input is mostly the
/// starting checkpoint, the usual shape of an incremental pass on a large
/// page (final review round 1): (a) the checkpoint and 20 new rows; (b) the
/// same with a late snapshot whose probe finds a gap (a probe copy, the
/// merge and the gap checkpoint's decode check); (c) a word count over (b)'s
/// output (the checkpoint, the rows and the gap state).
/// Workload 6's page: grown to a size (4 MiB in workload 6) of state, its
/// editors, its log and the next row's index.
struct GrownPage {
    ed: Editors,
    log: Log,
    i: usize,
    checkpoint_rev: i64,
    /// The page's state as a GC checkpoint.
    checkpoint_state: Vec<u8>,
}

impl GrownPage {
    fn new(growth: Growth, size: usize) -> GrownPage {
        let mut ed = Editors::new(6);
        let mut log = Log::default();
        let mut i = 0;
        let check_every = match growth {
            // Every 20,000 rows on the way to 4 MiB.
            Growth::Typing => (20_000 * size / (4 * MIB)).max(1_000),
            Growth::Pastes => 1,
        };
        loop {
            let (c, bytes) = growth.row(&mut ed, i);
            log.push(c, bytes);
            i += 1;
            if i % check_every == 0 && ed.state().len() >= size {
                break;
            }
        }
        let checkpoint_rev = log.rows.len() as i64;
        let checkpoint_state = Replayer::from_state(ReplayMode::Gc, &ed.state())
            .expect("the page's state")
            .encode_state();
        GrownPage {
            ed,
            log,
            i,
            checkpoint_rev,
            checkpoint_state,
        }
    }

    /// The next of 20 small rows after the checkpoint: one character of
    /// interleaved typing, or a 1 KiB paste.
    fn small_row(&mut self, growth: Growth) {
        let c = self.i % 3;
        let bytes = match growth {
            Growth::Typing => self.ed.type_shared(c),
            Growth::Pastes => self.ed.paste(c, KIB),
        };
        self.log.push(c, bytes);
        self.i += 1;
    }
}

fn checkpoint_heavy(growth: Growth) -> [f64; 3] {
    let mut page = GrownPage::new(growth, 4 * MIB);
    let checkpoint_rev = page.checkpoint_rev;
    let checkpoint_state = std::mem::take(&mut page.checkpoint_state);
    let late = {
        let x = Doc::with_client_id(0x78);
        apply(&x, &page.ed.state());
        let frag = x.get_or_insert_xml_fragment("default");
        let mut txn = x.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(page.ed.rng.text(4 * KIB)));
        drop(txn);
        own_state(&x)
    };
    for _ in 0..20 {
        page.small_row(growth);
    }
    let rows = page.log.rows;
    let new = rows_in(&rows, checkpoint_rev, i64::MAX);
    let pass = |snapshots: Vec<SnapshotProbe>| PassInput {
        checkpoint_rev,
        checkpoint_state: checkpoint_state.clone(),
        reapply: Vec::new(),
        replay: Vec::new(),
        genesis_rev: 0,
        built_rev: checkpoint_rev,
        rows: new.clone(),
        rows_exhausted: true,
        open: Vec::new(),
        claims: Claims::default(),
        kept_gaps: Vec::new(),
        snapshots,
        snapshot_checked_rev: checkpoint_rev,
        skip_revs: BTreeSet::new(),
        progress: Progress::default(),
        now: at(i64::try_from(rows.len()).unwrap() + 7200),
        rows_budget: None,
        checkpoint_floor: CHECKPOINT_FLOOR_BYTES,
        hooks: ReplayHooks::default(),
    };

    let a_input = checkpoint_state.len() + bytes_of(&new);
    let input = pass(Vec::new());
    let a = measure(|| run_pass(input).expect("the pass"));
    drop(a.value);
    let factor_a = (a_input + a.peak) as f64 / a_input as f64;

    let b_input = a_input + late.len();
    let input = pass(vec![SnapshotProbe {
        rev: checkpoint_rev,
        created_at: at(checkpoint_rev) + Duration::seconds(30),
        state_bytes: Some(late.clone()),
        legacy: false,
    }]);
    let b = measure(|| run_pass(input).expect("the pass"));
    let out = b.value;
    assert_eq!(out.stats.gaps, 1, "the late snapshot's gap");
    let factor_b = (b_input + b.peak) as f64 / b_input as f64;

    let gap_states: Vec<(i64, Vec<u8>)> = out
        .checkpoints
        .iter()
        .filter(|c| c.kind == CheckpointKind::Gap)
        .map(|c| (c.rev, c.state_bytes.clone()))
        .collect();
    let session = out
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::Session)
        .expect("a session")
        .clone();
    let end_rev = session.end_rev.expect("an end rev");
    let window_rows = rows_in(&rows, checkpoint_rev, end_rev);
    let c_input = checkpoint_state.len()
        + bytes_of(&window_rows)
        + gap_states.iter().map(|(_, s)| s.len()).sum::<usize>();
    let w = WindowInput {
        mode: ReplayMode::KeepDeleted,
        checkpoint_state: checkpoint_state.clone(),
        reapply: Vec::new(),
        rows: window_rows,
        gap_states,
        skip_revs: BTreeSet::new(),
        end_rev,
        merge_gap_at_end: false,
        progress: Progress::default(),
        hooks: ReplayHooks::default(),
    };
    let c = measure(|| {
        let r = replay_window(w).expect("the window");
        let d = entry_diff(r.doc(), &session.inserted, &session.deleted).expect("the diff");
        count_words(&text_chunks(&d))
    });
    let factor_c = (c_input + c.peak) as f64 / c_input as f64;

    println!("== 6. a checkpoint-heavy input: {}", growth.name());
    println!(
        "  checkpoint {:.2} MiB at rev {checkpoint_rev}, new rows {:.3} MiB ({} rows), snapshot {:.2} MiB",
        mib(checkpoint_state.len()),
        mib(bytes_of(&new)),
        new.len(),
        mib(late.len()),
    );
    for (name, input, m, f) in [
        ("(a) pass, checkpoint + rows", a_input, a.peak, factor_a),
        (
            "(b) pass, + a late snapshot's gap",
            b_input,
            b.peak,
            factor_b,
        ),
        ("(c) word count over (b)", c_input, c.peak, factor_c),
    ] {
        println!(
            "  {name}: input {:.2} MiB, peak heap above start {:.2} MiB, F = (input + peak) / input {f:.2}",
            mib(input),
            mib(m),
        );
    }
    [factor_a, factor_b, factor_c]
}

// ---------------------------------------------------------------------------
// Workload 7: a version view and a restore (M1.4, Task 3)
// ---------------------------------------------------------------------------

/// One measured view (or restore): input bytes, peak heap above the start
/// (the serialized result included) and wall time.
struct ViewRun {
    name: &'static str,
    input: usize,
    peak: usize,
    secs: f64,
}

impl ViewRun {
    fn factor(&self) -> f64 {
        (self.input + self.peak) as f64 / self.input as f64
    }
}

/// Workload 6's page grown to `size`, then 20 small rows: (a) a plain view
/// of the window of a session holding a third of them, (b) its Show
/// changes view, (c) a view of the page's state as a baseline, (d) a
/// restore of (a): the replay, `encode_state` and the Markdown round trip
/// the restore runs; then the Show changes views of a session of one row
/// deleting (e) everything client B wrote (an undo of B's typing) or (f)
/// the whole page. Each view's span includes `serde_json::to_vec`, as the
/// view caches the serialized JSON.
fn views(growth: Growth, size: usize) -> Vec<ViewRun> {
    let mut page = GrownPage::new(growth, size);
    for _ in 0..20 {
        page.small_row(growth);
    }
    let checkpoint_rev = page.checkpoint_rev;
    let checkpoint_state = std::mem::take(&mut page.checkpoint_state);
    // (e)'s and (f)'s rows after the 20, each replayed without the other:
    // author B deletes in one row everything client B ever wrote, as an
    // undo of B's typing does (on the typing page every third character,
    // none next to another of B's), or everything on the page.
    let sv = page.ed.docs[0].transact().state_vector();
    let deletion = |clients: &[u64]| {
        let mut deleted = IdSet::new();
        for c in clients {
            let client = ClientID::new(*c);
            deleted.insert(ID::new(client, 0), sv.get(&client));
        }
        let mut encoder = EncoderV1::new();
        encoder.write_var(0u32);
        deleted.encode(&mut encoder);
        (deleted, encoder.to_vec())
    };
    let undo = deletion(&CLIENTS[1..2]);
    let clear = deletion(&CLIENTS);
    let undo_rev = page.log.push(1, undo.1);
    let clear_rev = page.log.push(1, clear.1);
    let rows = page.log.rows;

    let new = rows_in(&rows, checkpoint_rev, undo_rev - 1);
    let mut pass = one_pass(checkpoint_state.clone(), new);
    pass.checkpoint_rev = checkpoint_rev;
    pass.built_rev = checkpoint_rev;
    pass.snapshot_checked_rev = checkpoint_rev;
    let out = run_pass(pass).expect("the pass");
    let session = out
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::Session && e.author == Some(author(1)))
        .expect("author B's session")
        .clone();
    drop(out);
    let end_rev = session.end_rev.expect("an end rev");
    let window_rows = rows_in(&rows, checkpoint_rev, end_rev);
    let window_input = checkpoint_state.len() + bytes_of(&window_rows);
    let window = |rows: Vec<DocUpdate>, skip_revs: BTreeSet<i64>, end_rev: i64| WindowInput {
        mode: ReplayMode::KeepDeleted,
        checkpoint_state: checkpoint_state.clone(),
        reapply: Vec::new(),
        rows,
        gap_states: Vec::new(),
        skip_revs,
        end_rev,
        merge_gap_at_end: false,
        progress: Progress::default(),
        hooks: ReplayHooks::default(),
    };
    let session_window = || window(window_rows.clone(), BTreeSet::new(), end_rev);
    let mut lines = Vec::new();
    // A view: the render and its serialized JSON, measured together.
    let mut view = |name: &'static str, input: ViewInput, input_bytes: usize, changes| {
        let m = measure(|| {
            let rendered = render(input, changes, RenderHooks::default()).expect("the render");
            let json = serde_json::to_vec(&rendered.doc).expect("the JSON");
            (rendered, json)
        });
        let (rendered, json) = m.value;
        let struck = json
            .windows(15)
            .filter(|w| w == b"\"historyDelete\"")
            .count();
        lines.push(format!(
            "{name}: changes {}, JSON {:.2} MiB, {struck} struck text nodes",
            rendered.changes,
            mib(json.len())
        ));
        drop((rendered, json));
        ViewRun {
            name,
            input: input_bytes,
            peak: m.peak,
            secs: m.secs,
        }
    };

    let a = view(
        "(a) plain view of the window",
        ViewInput::Window(session_window()),
        window_input,
        None,
    );
    let b = view(
        "(b) Show changes view of the window",
        ViewInput::Window(session_window()),
        window_input,
        Some((session.inserted.clone(), session.deleted.clone())),
    );
    let c = view(
        "(c) view of the state",
        ViewInput::State(checkpoint_state.clone()),
        checkpoint_state.len(),
        None,
    );
    // The window through `clear_rev` without the row `skip`.
    let deleting = |skip: i64| {
        let rows = rows_in(&rows, checkpoint_rev, clear_rev);
        let skipped = rows
            .iter()
            .find(|r| rev_of(r) == skip)
            .map_or(0, |r| r.update_bytes.len());
        let bytes = checkpoint_state.len() + bytes_of(&rows) - skipped;
        (window(rows, BTreeSet::from([skip]), clear_rev), bytes)
    };
    let (input, bytes) = deleting(clear_rev);
    let e = view(
        "(e) Show changes view of an undo of B's typing",
        ViewInput::Window(input),
        bytes,
        Some((IdSet::new(), undo.0)),
    );
    let (input, bytes) = deleting(undo_rev);
    let f = view(
        "(f) Show changes view of a deletion of the whole page",
        ViewInput::Window(input),
        bytes,
        Some((IdSet::new(), clear.0)),
    );

    let input = session_window();
    let d = measure(|| {
        let state = replay_window(input).expect("the window").encode_state();
        let engine = YrsEngine;
        let doc = engine.new_doc();
        engine
            .apply_update(&doc, &state)
            .expect("the state applies");
        let markdown = to_markdown::serialise(&engine, &doc).expect("the Markdown");
        let (_, update) = from_markdown::parse(&markdown).expect("the update");
        (state, doc, markdown, update)
    });
    lines.push(format!(
        "(d) restore: Markdown {:.2} MiB, update {:.2} MiB",
        mib(d.value.2.len()),
        mib(d.value.3.len())
    ));
    drop(d.value);
    let d = ViewRun {
        name: "(d) restore of the window",
        input: window_input,
        peak: d.peak,
        secs: d.secs,
    };

    println!(
        "== 7. a version view and a restore: {}, {:.2} MiB",
        growth.name(),
        mib(size)
    );
    println!(
        "  checkpoint {:.2} MiB at rev {checkpoint_rev}, B's session: {} window rows ({:.3} MiB) through rev {end_rev}",
        mib(checkpoint_state.len()),
        window_rows.len(),
        mib(bytes_of(&window_rows)),
    );
    for l in &lines {
        println!("  {l}");
    }
    let runs = vec![a, b, c, d, e, f];
    for r in &runs {
        println!(
            "  {}: input {:.2} MiB, peak heap above start {:.2} MiB, F = (input + peak) / input {:.2}, {:.3} s",
            r.name,
            mib(r.input),
            mib(r.peak),
            r.factor(),
            r.secs,
        );
    }
    runs
}

/// (g) A plain view of a 4 MiB page whose every other word is bold, one
/// typed paragraph of about 400 characters per row: one text node per word
/// in the JSON, the plain render's densest shape short of formatting each
/// character.
fn formatted_view() -> ViewRun {
    let doc = Doc::with_client_id(CLIENTS[0]);
    let frag = doc.get_or_insert_xml_fragment("default");
    let mut rng = Rng::new(7);
    let bold = Attrs::from([(Arc::from("bold"), Any::Map(Default::default()))]);
    let mut n = 0;
    loop {
        {
            let mut txn = doc.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            let t = p.push_back(&mut txn, XmlTextPrelim::new(""));
            let mut len = 0;
            for w in 0..60 {
                let n = 3 + rng.below(6);
                let mut word: String = (0..n)
                    .map(|_| char::from(b'a' + rng.below(26) as u8))
                    .collect();
                word.push(' ');
                if w % 2 == 0 {
                    t.insert_with_attributes(&mut txn, len, &word, bold.clone());
                } else {
                    t.insert(&mut txn, len, &word);
                }
                len += word.len() as u32;
            }
        }
        n += 1;
        if n % 100 == 0 && own_state(&doc).len() >= 4 * MIB {
            break;
        }
    }
    let state = own_state(&doc);
    drop(doc);
    let bytes = state.len();
    let m = measure(|| {
        let rendered =
            render(ViewInput::State(state), None, RenderHooks::default()).expect("the render");
        let json = serde_json::to_vec(&rendered.doc).expect("the JSON");
        (rendered, json)
    });
    let run = ViewRun {
        name: "(g) view of a page whose every other word is bold",
        input: bytes,
        peak: m.peak,
        secs: m.secs,
    };
    println!("== 7. a version view of a formatted page: {n} paragraphs");
    println!(
        "  {}: input {:.2} MiB, JSON {:.2} MiB, peak heap above start {:.2} MiB, F = (input + peak) / input {:.2}, {:.3} s",
        run.name,
        mib(run.input),
        mib(m.value.1.len()),
        mib(run.peak),
        run.factor(),
        run.secs,
    );
    run
}

/// Workload 7: every view and restore on the 4 MiB pages, and the largest
/// F of the Show changes views ((b), (e), (f)) and of the plain views and
/// the restore ((a), (c), (d), (g)), the factors' floor; then the views
/// once more on a typing page of the diff cap that `VIEW_MEMORY_FACTOR`
/// gives, where F is higher (fixed costs weigh more on a smaller page):
/// there, the largest Show changes view must hold no more than
/// `BUILD_MEMORY_FACTOR` × 4 MiB, the builder's own bound, and its render
/// time is measured.
fn view_factors() {
    let mut runs = Vec::new();
    for growth in [Growth::Typing, Growth::Pastes] {
        runs.extend(views(growth, 4 * MIB));
    }
    runs.push(formatted_view());
    let largest = |show_changes: bool| {
        runs.iter()
            .filter(|r| r.name.contains("Show changes") == show_changes)
            .map(ViewRun::factor)
            .fold(0.0f64, f64::max)
    };
    let (f_view, f_plain) = (largest(true), largest(false));
    let cap = |f: f64| 4.0 * f64::from(BUILD_MEMORY_FACTOR as u32) / f.ceil().max(40.0);
    println!("== 7. summary");
    println!(
        "  largest Show changes F at 4 MiB {f_view:.2}, rounded up {} (a diff cap of {:.2} MiB); VIEW_MEMORY_FACTOR {VIEW_MEMORY_FACTOR}: diff cap {:.2} MiB",
        f_view.ceil(),
        cap(f_view),
        mib(view_max_input_bytes(4 * MIB as u64, true) as usize),
    );
    println!(
        "  largest plain F at 4 MiB {f_plain:.2}, rounded up {} (a plain cap of {:.2} MiB); PLAIN_MEMORY_FACTOR {PLAIN_MEMORY_FACTOR}: plain cap {:.2} MiB",
        f_plain.ceil(),
        cap(f_plain),
        mib(view_max_input_bytes(4 * MIB as u64, false) as usize),
    );
    let at_cap = views(
        Growth::Typing,
        view_max_input_bytes(4 * MIB as u64, true) as usize,
    );
    // F grows on smaller pages (fixed costs), so the rule is checked where
    // it binds: a page of the diff cap's size takes at most what the
    // builder may, `BUILD_MEMORY_FACTOR` × the replay cap.
    let show_changes = || at_cap.iter().filter(|r| r.name.contains("Show changes"));
    let slowest = show_changes().map(|r| r.secs).fold(0.0f64, f64::max);
    let held = show_changes()
        .map(|r| r.input + r.peak)
        .max()
        .expect("Show changes views");
    let budget = BUILD_MEMORY_FACTOR as usize * 4 * MIB;
    println!(
        "  at the diff cap: the largest Show changes view held {:.2} MiB (input + peak) against {:.0} MiB ({}); the slowest took {slowest:.3} s",
        mib(held),
        mib(budget),
        if held <= budget {
            "holds"
        } else {
            "does not hold"
        },
    );
    shifted_baseline();
}

/// [`shifted_baseline`]'s and workload 8 (f)'s shifted baseline: workload
/// 6's typing page grown to 4 MiB with `fixtures::overlapping_skips_as`'s
/// `t0`, `t2` and `t3` merged in as yrs encodes a full state.
fn shifted_state() -> Vec<u8> {
    let page = GrownPage::new(Growth::Typing, 4 * MIB);
    let o = fixtures::overlapping_skips_as(0x7e57_0001);
    let doc = Doc::new();
    for u in [&page.checkpoint_state, &o.t0, &o.t2, &o.t3] {
        apply(&doc, u);
    }
    drop(page);
    let state = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    drop(doc);
    assert!(
        knot_history::replay::may_be_shifted(&state),
        "the baseline may be shifted"
    );
    let bytes = state.len();
    assert!(
        bytes.abs_diff(4 * MIB) * 20 <= 4 * MIB,
        "{bytes} bytes is not within 5 % of 4 MiB"
    );
    state
}

/// (h) A view and (i) a restore of a baseline that may be shifted (M1.4,
/// Task 8), kept out of [`view_factors`]' maxima: workload 6's typing page
/// grown to 4 MiB, plus `fixtures::overlapping_skips_as`'s `t0`, `t2` and
/// `t3` by a client of its own (`t1` and `t2b` withheld), so its pending
/// update holds overlapping Skips, encoded as yrs encodes a full state
/// (`encode_state_as_update_v1`, which merges the pending update in at a
/// shifted clock): what a room wrote before the knot-crdt fix. Both cut it
/// to its trusted prefix through `view::state_input` before they replay.
fn shifted_baseline() {
    let state = shifted_state();
    let bytes = state.len();

    let input = state.clone();
    let h = measure(|| {
        let rendered =
            render(ViewInput::State(input), None, RenderHooks::default()).expect("the render");
        let json = serde_json::to_vec(&rendered.doc).expect("the JSON");
        (rendered, json)
    });
    let h_json = h.value.1.len();
    drop(h.value);
    let h = ViewRun {
        name: "(h) shifted baseline view",
        input: bytes,
        peak: h.peak,
        secs: h.secs,
    };
    let input = state.clone();
    let i = measure(|| {
        // As `Views::restore_input` does: the prefix replaces the bytes.
        let prefix = state_input(&input).expect("the prefix");
        drop(input);
        let state = Replayer::from_state(ReplayMode::KeepDeleted, &prefix)
            .expect("the prefix applies")
            .encode_state();
        drop(prefix);
        let engine = YrsEngine;
        let doc = engine.new_doc();
        engine
            .apply_update(&doc, &state)
            .expect("the state applies");
        let markdown = to_markdown::serialise(&engine, &doc).expect("the Markdown");
        let (_, update) = from_markdown::parse(&markdown).expect("the update");
        (state, doc, markdown, update)
    });
    let (i_markdown, i_update) = (i.value.2.len(), i.value.3.len());
    drop(i.value);
    let i = ViewRun {
        name: "(i) shifted baseline restore",
        input: bytes,
        peak: i.peak,
        secs: i.secs,
    };
    drop(state);

    println!(
        "== 7. a shifted baseline's view and restore: {}, {:.2} MiB",
        Growth::Typing.name(),
        mib(bytes)
    );
    println!(
        "  (h): JSON {:.2} MiB; (i) restore: Markdown {:.2} MiB, update {:.2} MiB",
        mib(h_json),
        mib(i_markdown),
        mib(i_update)
    );
    for r in [&h, &i] {
        println!(
            "  {}: input {:.2} MiB, peak heap above start {:.2} MiB, F = (input + peak) / input {:.2}, {:.3} s",
            r.name,
            mib(r.input),
            mib(r.peak),
            r.factor(),
            r.secs,
        );
    }
    let f = h.factor().max(i.factor());
    println!(
        "  largest shifted-baseline F at 4 MiB {f:.2}, rounded up {}; PLAIN_MEMORY_FACTOR {PLAIN_MEMORY_FACTOR}",
        f.ceil()
    );
}

// ---------------------------------------------------------------------------
// Workload 8: reverts (M2, Task 2)
// ---------------------------------------------------------------------------

/// `REVERT_TIMEOUT / 2`: what a whole plan at the time cap may take.
const REVERT_PLAN_SECS: f64 = 5.0;

/// The skip-GC state of `rows` through `through`, replayed from an empty
/// doc (the genesis checkpoint): what a restore's target replay hands to
/// `revert::target`.
fn version_state(rows: &[DocUpdate], through: i64) -> Vec<u8> {
    let mut r = Replayer::new(ReplayMode::KeepDeleted);
    for row in rows.iter().take_while(|row| rev_of(row) <= through) {
        let outcome = r.apply(&row.update_bytes).outcome;
        assert_eq!(outcome, RowOutcome::Applied, "rev {}", rev_of(row));
    }
    r.encode_state()
}

/// Workload 6's page grown to `size`, then 20 small rows (one session), as
/// a restore sees it: the live room's state and three versions.
struct RevertPage {
    /// The room's state after every row (GC, pending-free: the export).
    live: Vec<u8>,
    /// The version a quarter of the way into the rows, replayed from the
    /// genesis checkpoint: reverting to it undoes most of the page.
    quarter: RevertTarget,
    /// The version before the last session (the checkpoint, before the 20
    /// small rows): reverting to it undoes those rows only.
    session: RevertTarget,
    /// The version after every row: the live page already renders as it.
    head: RevertTarget,
}

impl RevertPage {
    fn new(growth: Growth, size: usize) -> RevertPage {
        let mut page = GrownPage::new(growth, size);
        for _ in 0..20 {
            page.small_row(growth);
        }
        let rows = std::mem::take(&mut page.log.rows);
        let live = page.ed.state();
        let checkpoint_rev = page.checkpoint_rev;
        let checkpoint_state = std::mem::take(&mut page.checkpoint_state);
        drop(page);
        let target = |state: Vec<u8>| revert::target(&state).expect("the version's target");
        let quarter = target(version_state(&rows, (rows.len() as i64 / 4).max(1)));
        let mut from_checkpoint = Replayer::from_state(ReplayMode::KeepDeleted, &checkpoint_state)
            .expect("the checkpoint");
        let session = target(from_checkpoint.encode_state());
        for row in rows_in(&rows, checkpoint_rev, i64::MAX) {
            let outcome = from_checkpoint.apply(&row.update_bytes).outcome;
            assert_eq!(outcome, RowOutcome::Applied, "rev {}", rev_of(&row));
        }
        let head = target(from_checkpoint.encode_state());
        RevertPage {
            live,
            quarter,
            session,
            head,
        }
    }
}

/// One measured plan: input (`target.state.len() + live.len()`), peak heap
/// above the start (the returned update included) and wall time.
struct RevertRun {
    name: &'static str,
    input: usize,
    peak: usize,
    secs: f64,
}

impl RevertRun {
    fn factor(&self) -> f64 {
        (self.input + self.peak) as f64 / self.input as f64
    }

    fn print(&self) {
        println!(
            "  {}: input {:.2} MiB, peak heap above start {:.2} MiB, F = (input + peak) / input {:.2}, {:.3} s",
            self.name,
            mib(self.input),
            mib(self.peak),
            self.factor(),
            self.secs,
        );
    }
}

/// What a plan is expected to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Undo,
    Fallback,
    /// A fallback after an undo that ran and failed its verification.
    FallbackAfterUnverifiedUndo,
    Unchanged,
}

/// `revert::plan` of `target` against `live`, measured; panics unless it
/// answers `expect`.
fn plan_run(
    name: &'static str,
    target: &RevertTarget,
    live: &[u8],
    hooks: RevertHooks,
    expect: Expect,
) -> RevertRun {
    let m = measure(|| revert::plan_traced(target, live, hooks));
    let (result, trace) = m.value;
    let got = match &result {
        Ok(RevertOutcome::Unchanged) => Expect::Unchanged,
        Ok(RevertOutcome::Revert {
            kind: RevertKind::Undo,
            ..
        }) => Expect::Undo,
        Ok(RevertOutcome::Revert {
            kind: RevertKind::Fallback,
            ..
        }) if trace.undo_unverified.is_some() => Expect::FallbackAfterUnverifiedUndo,
        Ok(RevertOutcome::Revert {
            kind: RevertKind::Fallback,
            ..
        }) => Expect::Fallback,
        Err(e) => panic!("{name}: {e} ({trace:?})"),
    };
    assert_eq!(got, expect, "{name}: {trace:?}");
    drop(result);
    RevertRun {
        name,
        input: target.state.len() + live.len(),
        peak: m.peak,
        secs: m.secs,
    }
}

/// (a) the large undo, (b) the small undo, (c) the fallback alone, (d) the
/// unchanged check and (c′) an undo that fails verification, then the
/// fallback, on `page`.
fn revert_cases(page: &RevertPage) -> Vec<RevertRun> {
    let skip_undo = RevertHooks {
        skip_undo: true,
        ..RevertHooks::default()
    };
    let corrupt_undo = RevertHooks {
        corrupt_undo: true,
        ..RevertHooks::default()
    };
    let live = &page.live;
    vec![
        plan_run(
            "(a) a quarter in: the large undo",
            &page.quarter,
            live,
            RevertHooks::default(),
            Expect::Undo,
        ),
        plan_run(
            "(b) one session back: the small undo",
            &page.session,
            live,
            RevertHooks::default(),
            Expect::Undo,
        ),
        plan_run(
            "(c) (a) by the whole-page replace",
            &page.quarter,
            live,
            skip_undo,
            Expect::Fallback,
        ),
        plan_run(
            "(d) the unchanged check",
            &page.head,
            live,
            RevertHooks::default(),
            Expect::Unchanged,
        ),
        plan_run(
            "(c′) (a), its undo unverified, then the replace",
            &page.quarter,
            live,
            corrupt_undo,
            Expect::FallbackAfterUnverifiedUndo,
        ),
    ]
}

/// (a) and (c′) of [`revert_cases`] only: the undo path and the slowest
/// path a plan can take.
fn timed_cases(page: &RevertPage) -> [RevertRun; 2] {
    let corrupt_undo = RevertHooks {
        corrupt_undo: true,
        ..RevertHooks::default()
    };
    [
        plan_run(
            "(a) the large undo",
            &page.quarter,
            &page.live,
            RevertHooks::default(),
            Expect::Undo,
        ),
        plan_run(
            "(c′) unverified undo, then the replace",
            &page.quarter,
            &page.live,
            corrupt_undo,
            Expect::FallbackAfterUnverifiedUndo,
        ),
    ]
}

/// (f) `Views::restore_input`'s work with M2's finishes: the replay,
/// `encode_state`, drop, then `revert::target` (Restore here) or
/// `revert::copy_state` (Restore as new page), measured as
/// `PLAIN_MEMORY_FACTOR` and `SHIFTED_MEMORY_FACTOR` were (input = the
/// version's input bytes, the finish's result held at the peak).
fn restore_finish(name: &'static str, input: ViewInput, input_bytes: usize, copy: bool) -> ViewRun {
    let m = measure(|| {
        let replayer = match input {
            ViewInput::State(bytes) => {
                // As `Views::restore_input` does: a prefix replaces bytes
                // that may be shifted, others are replayed as they are.
                let bytes = if knot_history::replay::may_be_shifted(&bytes) {
                    let prefix = state_input(&bytes).expect("the prefix");
                    drop(bytes);
                    prefix
                } else {
                    bytes
                };
                Replayer::from_state(ReplayMode::KeepDeleted, &bytes).expect("the state applies")
            }
            ViewInput::Window(w) => replay_window(w).expect("the window"),
        };
        let state = replayer.encode_state();
        drop(replayer);
        if copy {
            (None, Some(revert::copy_state(&state).expect("the copy")))
        } else {
            (Some(revert::target(&state).expect("the target")), None)
        }
    });
    drop(m.value);
    ViewRun {
        name,
        input: input_bytes,
        peak: m.peak,
        secs: m.secs,
    }
}

/// (f) on workload 6's page: a session's window (author B's session among
/// the 20 small rows, as workload 7's (a)) and the page's state as a
/// baseline, each with both finishes.
fn restore_finishes(growth: Growth) -> Vec<ViewRun> {
    let mut page = GrownPage::new(growth, 4 * MIB);
    for _ in 0..20 {
        page.small_row(growth);
    }
    let checkpoint_rev = page.checkpoint_rev;
    let checkpoint_state = std::mem::take(&mut page.checkpoint_state);
    let rows = std::mem::take(&mut page.log.rows);
    drop(page);
    let mut pass = one_pass(
        checkpoint_state.clone(),
        rows_in(&rows, checkpoint_rev, i64::MAX),
    );
    pass.checkpoint_rev = checkpoint_rev;
    pass.built_rev = checkpoint_rev;
    pass.snapshot_checked_rev = checkpoint_rev;
    let out = run_pass(pass).expect("the pass");
    let end_rev = out
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::Session && e.author == Some(author(1)))
        .and_then(|e| e.end_rev)
        .expect("author B's session");
    drop(out);
    let window_rows = rows_in(&rows, checkpoint_rev, end_rev);
    let window_input = checkpoint_state.len() + bytes_of(&window_rows);
    let window = || {
        ViewInput::Window(WindowInput {
            mode: ReplayMode::KeepDeleted,
            checkpoint_state: checkpoint_state.clone(),
            reapply: Vec::new(),
            rows: window_rows.clone(),
            gap_states: Vec::new(),
            skip_revs: BTreeSet::new(),
            end_rev,
            merge_gap_at_end: false,
            progress: Progress::default(),
            hooks: ReplayHooks::default(),
        })
    };
    let state = || ViewInput::State(checkpoint_state.clone());
    let runs = vec![
        restore_finish("(f) window, revert::target", window(), window_input, false),
        restore_finish(
            "(f) window, revert::copy_state",
            window(),
            window_input,
            true,
        ),
        restore_finish(
            "(f) state, revert::target",
            state(),
            checkpoint_state.len(),
            false,
        ),
        restore_finish(
            "(f) state, revert::copy_state",
            state(),
            checkpoint_state.len(),
            true,
        ),
    ];
    println!(
        "== 8 (f). a restore's replay with M2's finishes: {}, 4 MiB",
        growth.name()
    );
    for r in &runs {
        println!(
            "  {}: input {:.2} MiB, peak heap above start {:.2} MiB, F = (input + peak) / input {:.2}, {:.3} s",
            r.name,
            mib(r.input),
            mib(r.peak),
            r.factor(),
            r.secs,
        );
    }
    runs
}

/// (f) on workload 7's shifted baseline, through `view::state_input`.
fn shifted_restore_finishes() -> Vec<ViewRun> {
    let state = shifted_state();
    let bytes = state.len();
    let runs = vec![
        restore_finish(
            "(f) shifted baseline, revert::target",
            ViewInput::State(state.clone()),
            bytes,
            false,
        ),
        restore_finish(
            "(f) shifted baseline, revert::copy_state",
            ViewInput::State(state),
            bytes,
            true,
        ),
    ];
    println!(
        "== 8 (f). a shifted baseline's restore with M2's finishes: {}, {:.2} MiB",
        Growth::Typing.name(),
        mib(bytes)
    );
    for r in &runs {
        println!(
            "  {}: input {:.2} MiB, peak heap above start {:.2} MiB, F = (input + peak) / input {:.2}, {:.3} s",
            r.name,
            mib(r.input),
            mib(r.peak),
            r.factor(),
            r.secs,
        );
    }
    runs
}

fn reverts() {
    // (a)-(d) and (c′) on workload 6's two 4 MiB pages.
    let mut factor = 0.0f64;
    let mut ratio = Vec::new();
    for growth in [Growth::Typing, Growth::Pastes] {
        let page = RevertPage::new(growth, 4 * MIB);
        println!(
            "== 8. reverts: {}, 4 MiB; live {:.2} MiB, versions: a quarter in {:.2} MiB, one session back {:.2} MiB, the head {:.2} MiB",
            growth.name(),
            mib(page.live.len()),
            mib(page.quarter.state.len()),
            mib(page.session.state.len()),
            mib(page.head.state.len()),
        );
        let runs = revert_cases(&page);
        for r in &runs {
            r.print();
            factor = factor.max(r.factor());
        }
        let peak_of = |prefix: &str| {
            runs.iter()
                .find(|r| r.name.starts_with(prefix))
                .map_or(0, |r| r.peak)
        };
        let (a, c, c2) = (peak_of("(a)"), peak_of("(c)"), peak_of("(c′)"));
        println!(
            "  (c′)'s peak {:.2} MiB against the larger of (a) and (c) {:.2} MiB and their sum {:.2} MiB",
            mib(c2),
            mib(a.max(c)),
            mib(a + c),
        );
        ratio.push((growth, runs[0].input as f64 / page.live.len() as f64));
    }
    let f = factor.ceil() as u64;
    let memory_cap = 4 * MIB as u64 * BUILD_MEMORY_FACTOR / f.max(BUILD_MEMORY_FACTOR);
    println!(
        "== 8. summary: largest F {factor:.2}, rounded up {f}: a memory cap of {:.2} MiB at the default 4 MiB replay cap (REVERT_MEMORY_FACTOR {REVERT_MEMORY_FACTOR}, REVERT_TIME_CAP_BYTES {:.2} MiB: revert_max_input_bytes(4 MiB) {:.2} MiB)",
        mib(memory_cap as usize),
        mib(REVERT_TIME_CAP_BYTES as usize),
        mib(revert_max_input_bytes(4 * MIB as u64) as usize),
    );

    // (e) (a) and (c′) at 1×, 2× and 4× the memory cap.
    let mut timed: BTreeMap<usize, Vec<(usize, f64)>> = BTreeMap::new();
    for (growth, ratio) in ratio {
        for k in [1usize, 2, 4] {
            let live_size = (k as f64 * memory_cap as f64 / ratio) as usize;
            let page = RevertPage::new(growth, live_size);
            let runs = timed_cases(&page);
            let input = runs[0].input;
            println!(
                "== 8 (e). reverts: {}, {k}× the memory cap: input {:.2} MiB ({:.2}× the cap)",
                growth.name(),
                mib(input),
                input as f64 / memory_cap as f64,
            );
            for r in &runs {
                r.print();
            }
            let slower = runs[0].secs.max(runs[1].secs);
            timed.entry(k).or_default().push((input, slower));
        }
    }
    // The largest input whose every page (and every smaller size) planned
    // in under `REVERT_TIMEOUT / 2`, rounded down to a MiB.
    let mut time_cap = None;
    for (k, runs) in &timed {
        if runs.iter().any(|(_, s)| *s >= REVERT_PLAN_SECS) {
            println!("== 8 (e). at {k}× the memory cap a plan took {REVERT_PLAN_SECS} s or more");
            break;
        }
        let input = runs.iter().map(|(i, _)| *i).min().expect("a run");
        time_cap = Some(input / MIB * MIB);
    }
    match time_cap {
        Some(cap) => println!(
            "== 8 (e). summary: the time cap is {:.0} MiB (memory cap {:.2} MiB)",
            mib(cap),
            mib(memory_cap as usize)
        ),
        None => println!("== 8 (e). summary: no size planned within {REVERT_PLAN_SECS} s"),
    }

    // (f) the restore's replay with M2's finishes.
    let mut plain = Vec::new();
    for growth in [Growth::Typing, Growth::Pastes] {
        plain.extend(restore_finishes(growth));
    }
    let shifted = shifted_restore_finishes();
    let largest = |runs: &[ViewRun]| runs.iter().map(ViewRun::factor).fold(0.0f64, f64::max);
    let (f_plain, f_shifted) = (largest(&plain), largest(&shifted));
    println!(
        "== 8 (f). summary: largest plain F {f_plain:.4} (PLAIN_MEMORY_FACTOR {PLAIN_MEMORY_FACTOR}: {}), largest shifted-baseline F {f_shifted:.4} (SHIFTED_MEMORY_FACTOR {SHIFTED_MEMORY_FACTOR}: {})",
        if f_plain <= PLAIN_MEMORY_FACTOR as f64 {
            "holds"
        } else {
            "exceeded"
        },
        if f_shifted <= SHIFTED_MEMORY_FACTOR as f64 {
            "holds"
        } else {
            "exceeded"
        },
    );
}

// ---------------------------------------------------------------------------
// Workload 5: a row parked for good under typing
// ---------------------------------------------------------------------------

fn parked_for_good() {
    let mut ed = Editors::new(5);
    let mut typing = Log::default();
    for _ in 0..22_000 {
        let c = ed.rng.below(3);
        let bytes = ed.type_own(c);
        typing.push(c, bytes);
    }
    // After 2,000 rows client D pastes 1 MiB into a paragraph client E
    // created and never delivered: D's row parks for good.
    let parked_row = {
        let room = Doc::new();
        for r in &typing.rows[..2_000] {
            apply(&room, &r.update_bytes);
        }
        let e = Doc::with_client_id(0x0e0e_0e0e);
        apply(&e, &own_state(&room));
        let e_update = {
            let frag = e.get_or_insert_xml_fragment("default");
            let mut txn = e.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(""));
            txn.encode_update_v1()
        };
        let d = Doc::with_client_id(0x0d0d_0d0d);
        apply(&d, &own_state(&room));
        apply(&d, &e_update);
        let frag = d.get_or_insert_xml_fragment("default");
        let mut txn = d.transact_mut();
        let t = last_text(&frag, &txn).expect("E's paragraph");
        t.insert(&mut txn, 0, &ed.rng.text(MIB));
        txn.encode_update_v1()
    };
    let without = renumbered(typing.rows.clone());
    let mut with = typing.rows;
    let mut parked = without[0].clone();
    parked.update_bytes = parked_row;
    parked.by_user_id = Some(author(3));
    with.insert(2_000, parked);
    let with = renumbered(with);

    let base = Page::build(&without, PASS_MAX_ROWS);
    let page = Page::build(&with, PASS_MAX_ROWS);
    println!("== 5. a 1 MiB row parked for good under typing (passes of {PASS_MAX_ROWS} rows)");
    println!(
        "  without it: {} rows in {} passes, {:.2} s, {:.0} rows/s, retried {}",
        without.len(),
        base.passes,
        base.compute_secs,
        without.len() as f64 / base.compute_secs,
        base.retried,
    );
    println!(
        "  with it:    {} rows in {} passes, {:.2} s, {:.0} rows/s, retried {}, open claims {}, peak heap {:.2} MiB",
        with.len(),
        page.passes,
        page.compute_secs,
        with.len() as f64 / page.compute_secs,
        page.retried,
        page.claims.len(),
        mib(page.peak),
    );
}

// ---------------------------------------------------------------------------

/// Runs every workload and derives the cap; with arguments (`1` to `8`),
/// runs only those workloads.
fn main() {
    let only: Vec<String> = std::env::args().skip(1).collect();
    if !only.is_empty() {
        for w in &only {
            match w.as_str() {
                "1" => drop(typing()),
                "2" => drop(pastes()),
                "3" => drop(policy()),
                "4" => {
                    memory(Growth::Typing);
                    memory(Growth::Pastes);
                }
                "5" => parked_for_good(),
                "6" => {
                    checkpoint_heavy(Growth::Typing);
                    checkpoint_heavy(Growth::Pastes);
                }
                "7" => view_factors(),
                "8" => reverts(),
                other => panic!("no workload {other}"),
            }
        }
        return;
    }
    let t1 = typing();
    let t2 = pastes();
    let p = policy();
    let mut factors = Vec::new();
    for growth in [Growth::Typing, Growth::Pastes] {
        factors.extend(memory(growth));
    }
    for growth in [Growth::Typing, Growth::Pastes] {
        factors.extend(checkpoint_heavy(growth));
    }
    parked_for_good();
    view_factors();
    reverts();

    let slower = t1.mib_per_sec.min(t2.mib_per_sec);
    let f = factors.iter().copied().fold(0.0f64, f64::max).ceil();
    let by_time = slower * CAP_SECONDS * MIB as f64;
    let by_memory = (MEMORY_LIMIT - WEB_ALLOWANCE) as f64 / (CAP_WORKERS as f64 * f);
    let bound = by_time.min(by_memory);
    let mut cap = MIB;
    while ((cap * 2) as f64) <= bound && cap * 2 <= 32 * MIB {
        cap *= 2;
    }
    println!("== summary");
    println!(
        "  slower of 1-2: {slower:.2} MiB/s, x {CAP_SECONDS} s = {:.1} MiB",
        by_time / MIB as f64
    );
    println!(
        "  F = {f} (factors {:?}); (512 MiB - 256 MiB) / ({CAP_WORKERS} x F) = {:.1} MiB",
        factors
            .iter()
            .map(|x| (x * 100.0).round() / 100.0)
            .collect::<Vec<_>>(),
        by_memory / MIB as f64
    );
    println!(
        "  replay cap: largest power of two <= {:.1} MiB, at most 32 MiB = {} MiB ({CAP_WORKERS} worker x F x cap = {:.0} MiB)",
        bound / MIB as f64,
        cap / MIB,
        (CAP_WORKERS * cap) as f64 * f / MIB as f64,
    );
    println!(
        "  policy: worst replay {:.3} s against {POLICY_BOUND_SECS} s: {}",
        p.worst_secs,
        if p.worst_secs < POLICY_BOUND_SECS {
            "holds"
        } else {
            "does not hold"
        }
    );
}
