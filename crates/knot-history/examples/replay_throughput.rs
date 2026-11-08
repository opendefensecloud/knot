//! Page history M1.3, Task 5: replay throughput, the checkpoint policy and
//! the doc-memory factor F that set `DEFAULT_HISTORY_REPLAY_MAX_BYTES`.
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
//!    (b)'s output. F is the largest factor of workloads 4 and 6.
//!
//! The cap rule: the largest power of two at most min(the slower of
//! workloads 1-2 × 2 s, (512 MiB − 256 MiB) ÷ (1 worker × F)), at most
//! 32 MiB. One worker is the default; the [8, 32] MiB clamp of the M1.3
//! plan is gone, since at F > 32 its 8 MiB floor does not fit even one
//! worker in the chart's 512 Mi limit.

#[path = "../tests/common/underflow.rs"]
mod underflow;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use chrono::{DateTime, Duration, TimeZone, Utc};
use knot_history::{
    CHECKPOINT_FLOOR_BYTES, CheckpointKind, CheckpointWrite, Claims, EntryDraft, EntryKind,
    PassInput, PassOutput, Progress, ReplayHooks, ReplayMode, Replayer, RowOutcome, SnapshotProbe,
    WindowInput, checkpoint_threshold, count_words, entry_diff, replay_window, run_pass,
    text_chunks,
};
use knot_storage::{DocUpdate, UpdateSource};
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{
    Doc, ReadTxn, StateVector, Text, Transact, Update, XmlElementPrelim, XmlFragment,
    XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
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
    let (doc_state, refused_row, refused_room) = underflow::skip_underflow_case();
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
        },
        SnapshotProbe {
            rev: union_rev,
            created_at: at(union_rev) + Duration::seconds(30),
            state_bytes: Some(union_state.clone()),
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
fn checkpoint_heavy(growth: Growth) -> [f64; 3] {
    let mut ed = Editors::new(6);
    let mut log = Log::default();
    let mut i = 0;
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
    let checkpoint_state = Replayer::from_state(ReplayMode::Gc, &ed.state())
        .expect("the page's state")
        .encode_state();
    let late = {
        let x = Doc::with_client_id(0x78);
        apply(&x, &ed.state());
        let frag = x.get_or_insert_xml_fragment("default");
        let mut txn = x.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(ed.rng.text(4 * KIB)));
        drop(txn);
        own_state(&x)
    };
    for _ in 0..20 {
        let (c, bytes) = match growth {
            Growth::Typing => (i % 3, ed.type_shared(i % 3)),
            Growth::Pastes => (i % 3, ed.paste(i % 3, KIB)),
        };
        log.push(c, bytes);
        i += 1;
    }
    let rows = log.rows;
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

/// Runs every workload and derives the cap; with arguments (`1` to `5`),
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
