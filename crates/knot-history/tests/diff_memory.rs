//! The heap `diff::text_chunks` needs (page history M1.3, Task 5's spike).
//!
//! yrs 0.27.3's `diff_range` splits the whole doc by both snapshots on every
//! call and records each delete-set slice in the transaction's
//! `merge_blocks` (`transaction.rs:1336-1362`), which only a commit drains.
//! Diffing every text of a page in one transaction therefore holds
//! (texts × blocks in the snapshots' delete sets) ids at once: about
//! 500 MiB for a word count on a 4 MiB page of interleaved typing, where
//! one author's insert set, which `S_lo` deletes, is a block per character.
//!
//! This test binary has its own counting allocator, so it holds only tests
//! that measure the heap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use knot_history::diff::{ChunkKind, entry_diff, text_chunks};
use knot_history::{ReplayMode, Replayer, RowOutcome};
use yrs::updates::decoder::Decode;
use yrs::{
    Doc, IdSet, Text, Transact, Update, XmlElementPrelim, XmlFragment, XmlOut, XmlTextPrelim,
};

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every call forwards to `System` and only adds bookkeeping.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Three clients take turns typing one character at the end of the last
/// paragraph (a new one every `per_paragraph` characters), so no two items
/// of one client are adjacent and none squash. Returns the skip-GC replay
/// and client 0's insert set: one block per character.
fn interleaved(paragraphs: usize, per_paragraph: usize) -> (Replayer, IdSet) {
    let docs: Vec<Doc> = (1..=3).map(Doc::with_client_id).collect();
    let mut h = Replayer::new(ReplayMode::KeepDeleted);
    let mut inserted = IdSet::new();
    for i in 0..paragraphs * per_paragraph {
        let c = i % 3;
        let frag = docs[c].get_or_insert_xml_fragment("default");
        let row = {
            let mut txn = docs[c].transact_mut();
            if i % per_paragraph == 0 {
                let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(&mut txn, XmlTextPrelim::new(""));
            }
            let n = frag.len(&txn);
            let Some(XmlOut::Element(p)) = frag.get(&txn, n - 1) else {
                panic!("a paragraph");
            };
            let Some(XmlOut::Text(t)) = p.get(&txn, 0) else {
                panic!("a text");
            };
            let len = t.len(&txn);
            t.insert(&mut txn, len, "x");
            txn.encode_update_v1()
        };
        for (j, doc) in docs.iter().enumerate() {
            if j != c {
                doc.transact_mut()
                    .apply_update(Update::decode_v1(&row).unwrap())
                    .unwrap();
            }
        }
        let e = h.apply(&row);
        assert_eq!(e.outcome, RowOutcome::Applied);
        if c == 0 {
            inserted.merge_with(e.inserted);
        }
    }
    (h, inserted)
}

#[test]
fn diffing_the_texts_holds_no_more_than_the_union_doc() {
    let (h, inserted) = interleaved(200, 60);

    let before = CURRENT.load(Ordering::Relaxed);
    let d = entry_diff(h.doc(), &inserted, &IdSet::new()).expect("the diff");
    let union = CURRENT.load(Ordering::Relaxed) - before;

    let start = CURRENT.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    let chunks = text_chunks(&d);
    let peak = PEAK.load(Ordering::Relaxed) - start;

    let added = chunks.iter().filter(|c| c.kind == ChunkKind::Added).count();
    assert_eq!(
        added, 4000,
        "one chunk per unsquashed character of client 0"
    );
    assert!(
        peak <= union,
        "text_chunks peaked {peak} bytes above its start; the union doc and snapshots hold {union}"
    );
}
