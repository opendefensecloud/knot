//! The entry diff and word counts (page history M1.3, Task 3), and the
//! architecture's spike "encode_state_from_snapshot on a mixed skip_gc doc":
//! a skip-GC replay seeded from a GC'd checkpoint (so it holds GC blocks and
//! `ItemContent::Deleted` items) diffs one author's entry exactly, with a
//! second author's edits interleaved.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use knot_history::diff::{
    ChunkKind, DiffError, TextChunk, entry_diff, strip_zero_clocks, text_chunks,
};
use knot_history::words::{WordCounts, count_words, entry_word_counts};
use knot_history::{ReplayMode, Replayer, RowOutcome};
use yrs::block::ClientID;
use yrs::types::Attrs;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::EncoderV1;
use yrs::{
    Any, Doc, GetString, IdSet, ReadTxn, Snapshot, StateVector, Text, Transact, TransactionMut,
    Update, XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
};

const SEED: u64 = 1;
const A: usize = 0;
const B: usize = 1;

/// The update of one transaction `f` runs on `doc` (`observe_update_v1`).
fn txn_update(doc: &Doc, f: impl FnOnce(&mut TransactionMut)) -> Vec<u8> {
    let out = Arc::new(Mutex::new(Vec::new()));
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

/// The text of the `i`th paragraph of `frag`.
fn text_of(frag: &XmlFragmentRef, txn: &impl ReadTxn, i: u32) -> XmlTextRef {
    match frag.get(txn, i) {
        Some(XmlOut::Element(p)) => match p.get(txn, 0) {
            Some(XmlOut::Text(t)) => t,
            other => panic!("paragraph {i} holds no text: {other:?}"),
        },
        other => panic!("no paragraph at {i}: {other:?}"),
    }
}

fn push_paragraph(frag: &XmlFragmentRef, txn: &mut TransactionMut, text: &str) {
    let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
    p.push_back(txn, XmlTextPrelim::new(text));
}

/// The checkpoint every test starts from, as a `ReplayMode::Gc` replay
/// encodes it: paragraph P1 "alpha beta" and paragraph P2 "old words here",
/// after a word typed into P1 and deleted again (an `ItemContent::Deleted`
/// item) and a paragraph added and removed again (GC blocks).
fn gc_checkpoint() -> Vec<u8> {
    let seed = Doc::with_client_id(SEED);
    let frag = seed.get_or_insert_xml_fragment("default");
    let mut gc = Replayer::new(ReplayMode::Gc);
    let rows = [
        txn_update(&seed, |txn| {
            push_paragraph(&frag, txn, "alpha beta");
            push_paragraph(&frag, txn, "old words here");
        }),
        txn_update(&seed, |txn| text_of(&frag, txn, 0).insert(txn, 10, " junk")),
        txn_update(&seed, |txn| text_of(&frag, txn, 0).remove_range(txn, 10, 5)),
        txn_update(&seed, |txn| push_paragraph(&frag, txn, "gone soon")),
        txn_update(&seed, |txn| frag.remove_range(txn, 2, 1)),
    ];
    for row in &rows {
        assert_eq!(gc.apply(row).outcome, RowOutcome::Applied, "fixture");
    }
    let state = gc.encode_state();
    let update = Update::decode_v1(&state).expect("a state");
    assert!(
        !update
            .insertions(true)
            .diff(&update.insertions(false))
            .is_empty(),
        "precondition: the checkpoint holds GC blocks"
    );
    state
}

/// A page history: H, the skip-GC replay seeded from [`gc_checkpoint`],
/// and one doc per author that edits it; each edit is one row, applied to
/// H and to every other author's doc.
struct Page {
    h: Replayer,
    authors: Vec<Doc>,
    /// Per author: the union of their rows' `inserted` and `deleted`.
    sets: Vec<(IdSet, IdSet)>,
}

impl Page {
    fn new(authors: usize) -> Page {
        let checkpoint = gc_checkpoint();
        let h = Replayer::from_state(ReplayMode::KeepDeleted, &checkpoint).expect("H");
        let authors = (0..authors)
            .map(|i| {
                let doc = Doc::with_client_id(10 * (i as u64 + 1));
                doc.get_or_insert_xml_fragment("default");
                doc.transact_mut()
                    .apply_update(Update::decode_v1(&checkpoint).expect("a state"))
                    .expect("the checkpoint applies");
                doc
            })
            .collect::<Vec<_>>();
        let sets = authors
            .iter()
            .map(|_| (IdSet::new(), IdSet::new()))
            .collect();
        Page { h, authors, sets }
    }

    fn edit(&mut self, who: usize, f: impl FnOnce(&XmlFragmentRef, &mut TransactionMut)) {
        let doc = &self.authors[who];
        let frag = doc.get_or_insert_xml_fragment("default");
        let row = txn_update(doc, |txn| f(&frag, txn));
        for (i, other) in self.authors.iter().enumerate() {
            if i != who {
                other
                    .transact_mut()
                    .apply_update(Update::decode_v1(&row).expect("a row"))
                    .expect("the row applies");
            }
        }
        let e = self.h.apply(&row);
        assert_eq!(e.outcome, RowOutcome::Applied, "fixture: the row applies");
        let (ins, del) = &mut self.sets[who];
        ins.merge_with(e.inserted);
        del.merge_with(e.deleted);
    }

    fn sets(&self, who: usize) -> (IdSet, IdSet) {
        self.sets[who].clone()
    }

    fn h(&self) -> &Doc {
        self.h.doc()
    }
}

fn insert_at_end(frag: &XmlFragmentRef, txn: &mut TransactionMut, paragraph: u32, text: &str) {
    let t = text_of(frag, txn, paragraph);
    let len = t.len(txn);
    t.insert(txn, len, text);
}

/// Removes the first occurrence of `word` from paragraph `paragraph`.
fn delete_word(frag: &XmlFragmentRef, txn: &mut TransactionMut, paragraph: u32, word: &str) {
    let t = text_of(frag, txn, paragraph);
    let at = t.get_string(txn).find(word).expect("the word is there");
    t.remove_range(txn, at as u32, word.len() as u32);
}

/// The design's overlap case: A and B edit the same paragraphs inside one
/// another's sessions.
fn overlap_history() -> Page {
    let mut page = Page::new(2);
    page.edit(A, |f, txn| insert_at_end(f, txn, 0, " gamma"));
    page.edit(B, |f, txn| insert_at_end(f, txn, 0, " delta"));
    page.edit(A, |f, txn| delete_word(f, txn, 0, "beta"));
    page.edit(B, |f, txn| delete_word(f, txn, 0, "alpha"));
    page.edit(A, |f, txn| f.remove_range(txn, 1, 1));
    page.edit(A, |f, txn| insert_at_end(f, txn, 0, "zeta"));
    page.edit(A, |f, txn| delete_word(f, txn, 0, "zeta"));
    page
}

fn texts_of(chunks: &[TextChunk], kind: ChunkKind) -> String {
    chunks
        .iter()
        .filter(|c| c.kind == kind)
        .map(|c| c.text.as_str())
        .collect()
}

#[test]
fn union_diff_on_a_doc_seeded_from_a_gc_checkpoint_shows_only_the_authors_changes() {
    let page = overlap_history();
    let (i_a, d_a) = page.sets(A);
    let d = entry_diff(page.h(), &i_a, &d_a).expect("encode_state_from_snapshot on a mixed doc");
    let chunks = text_chunks(&d);
    assert_eq!(texts_of(&chunks, ChunkKind::Added), " gamma");
    // P2's text is reachable through the union doc.
    assert_eq!(texts_of(&chunks, ChunkKind::Removed), "betaold words here");
    assert!(
        !chunks
            .iter()
            .any(|c| c.text.contains("delta") && c.kind != ChunkKind::Plain),
        "B's insert is plain: {chunks:?}"
    );
    assert!(
        !chunks.iter().any(|c| c.text.contains("alpha")),
        "B's delete is visible in neither state: {chunks:?}"
    );
    assert!(
        !chunks.iter().any(|c| c.text.contains("zeta")),
        "typed and deleted inside the entry shows nowhere: {chunks:?}"
    );
    assert!(
        !chunks
            .iter()
            .any(|c| c.text.contains("junk") || c.text.contains("gone")),
        "content deleted before the checkpoint shows nowhere: {chunks:?}"
    );
    let removed_segments: Vec<usize> = chunks
        .iter()
        .filter(|c| c.kind == ChunkKind::Removed)
        .map(|c| c.segment)
        .collect();
    assert!(
        removed_segments.windows(2).all(|w| w[0] < w[1]),
        "\"beta\" (P1) and P2's text are in different segments: {chunks:?}"
    );
    assert_eq!(
        entry_word_counts(page.h(), &i_a, &d_a).unwrap(),
        WordCounts {
            added: 1,
            removed: 4
        }
    );
}

#[test]
fn b_sees_only_bs_changes() {
    let page = overlap_history();
    let (i_b, d_b) = page.sets(B);
    let chunks = text_chunks(&entry_diff(page.h(), &i_b, &d_b).expect("the diff"));
    assert_eq!(texts_of(&chunks, ChunkKind::Added), " delta");
    assert_eq!(texts_of(&chunks, ChunkKind::Removed), "alpha");
    assert!(
        !chunks
            .iter()
            .any(|c| c.text.contains("gamma") && c.kind != ChunkKind::Plain),
        "A's insert is plain: {chunks:?}"
    );
    assert!(
        !chunks
            .iter()
            .any(|c| c.text.contains("beta") || c.text.contains("old words")),
        "A's deletes are visible in neither state: {chunks:?}"
    );
    assert_eq!(
        entry_word_counts(page.h(), &i_b, &d_b).unwrap(),
        WordCounts {
            added: 1,
            removed: 1
        }
    );
}

#[test]
fn the_entry_diff_carries_the_architectures_sets() {
    let page = overlap_history();
    let (i_a, d_a) = page.sets(A);
    let d = entry_diff(page.h(), &i_a, &d_a).expect("the diff");
    let s = page.h().transact().snapshot();
    assert_eq!(d.s, s);
    assert_eq!(d.removed, d_a.diff(&i_a));
    assert_eq!(d.added, i_a.diff(&s.delete_set));
    assert_eq!(
        d.s_lo,
        Snapshot::new(
            s.state_map.clone(),
            s.delete_set.diff(&d.removed).merge(&i_a)
        )
    );
    assert!(d.union.skip_gc(), "the union doc keeps deleted content");
    assert!(!d.removed.is_empty() && !d.added.is_empty());
}

#[test]
fn strip_zero_clocks_prevents_the_clock_minus_one_underflow() {
    let page = overlap_history();
    let s = page.h().transact().snapshot();
    // A client the doc holds blocks for, at clock 0.
    let client_of_p1 = ClientID::new(SEED);
    let mut sv = s.state_map.clone();
    sv.set_min(client_of_p1, 0);
    assert!(sv.contains_client(&client_of_p1) && sv.get(&client_of_p1) == 0);

    let snap = Snapshot::new(strip_zero_clocks(sv.clone()), s.delete_set.clone());
    assert!(
        page.h()
            .transact()
            .encode_state_from_snapshot(&snap, &mut EncoderV1::new())
            .is_ok()
    );

    // Without the strip, yrs 0.27.3 computes `find_index(clock - 1)` for
    // that client (`store.rs:179`) and panics.
    let unstripped = Snapshot::new(sv, s.delete_set.clone());
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let _ = page
            .h()
            .transact()
            .encode_state_from_snapshot(&unstripped, &mut EncoderV1::new());
    }));
    std::panic::set_hook(hook);
    assert!(outcome.is_err(), "precondition: a zero clock panics");
}

#[test]
fn strip_zero_clocks_keeps_every_other_clock() {
    let mut sv = StateVector::default();
    sv.set_max(ClientID::new(1), 0);
    sv.set_max(ClientID::new(2), 7);
    let stripped = strip_zero_clocks(sv);
    assert!(!stripped.contains_client(&ClientID::new(1)));
    assert_eq!(stripped.get(&ClientID::new(2)), 7);
    assert_eq!(stripped.len(), 1);
}

#[test]
fn entry_diff_refuses_a_gc_doc() {
    let checkpoint = gc_checkpoint();
    let gc = Replayer::from_state(ReplayMode::Gc, &checkpoint).expect("a GC replay");
    let (i, d) = (IdSet::new(), IdSet::new());
    assert!(matches!(
        entry_diff(gc.doc(), &i, &d),
        Err(DiffError::NotKeepDeleted)
    ));
    assert!(matches!(
        entry_word_counts(gc.doc(), &i, &d),
        Err(DiffError::NotKeepDeleted)
    ));
}

fn chunk(segment: usize, kind: ChunkKind, text: &str) -> TextChunk {
    TextChunk {
        segment,
        kind,
        text: text.to_string(),
    }
}

#[test]
fn words_count_whitespace_tokens_per_run() {
    use ChunkKind::{Added, Plain, Removed};
    // "gamma two" is 2, "y" is 1.
    assert_eq!(
        count_words(&[
            chunk(0, Added, "gam"),
            chunk(0, Added, "ma two "),
            chunk(0, Removed, "x"),
            chunk(0, Plain, "a b c"),
            chunk(1, Added, "y"),
        ]),
        WordCounts {
            added: 3,
            removed: 1
        }
    );
    // Runs never cross a segment.
    assert_eq!(
        count_words(&[chunk(0, Removed, "beta"), chunk(1, Removed, "old")]),
        WordCounts {
            added: 0,
            removed: 2
        }
    );
    // A Plain chunk ends the run.
    assert_eq!(
        count_words(&[
            chunk(0, Added, "ab"),
            chunk(0, Plain, " "),
            chunk(0, Added, "cd")
        ]),
        WordCounts {
            added: 2,
            removed: 0
        }
    );
    // A Removed chunk ends an Added run, and whitespace alone is no word.
    assert_eq!(
        count_words(&[
            chunk(0, Added, "re"),
            chunk(0, Removed, "  "),
            chunk(0, Added, "do")
        ]),
        WordCounts {
            added: 2,
            removed: 0
        }
    );
    assert_eq!(
        count_words(&[]),
        WordCounts {
            added: 0,
            removed: 0
        }
    );
}

#[test]
fn a_word_typed_in_two_items_counts_once() {
    let mut page = Page::new(1);
    page.edit(A, |f, txn| insert_at_end(f, txn, 0, "gam"));
    // The cursor left: "x" at the start of P2.
    page.edit(A, |f, txn| text_of(f, txn, 1).insert(txn, 0, "x"));
    // "ma" right after "gam", at a later clock, so not squashed with it.
    page.edit(A, |f, txn| insert_at_end(f, txn, 0, "ma"));
    let (i_a, d_a) = page.sets(A);
    let chunks = text_chunks(&entry_diff(page.h(), &i_a, &d_a).unwrap());
    assert!(
        chunks.iter().filter(|c| c.kind == ChunkKind::Added).count() >= 3,
        "precondition: gamma comes back split: {chunks:?}"
    );
    assert_eq!(texts_of(&chunks, ChunkKind::Added), "gammax");
    // "gamma", "x".
    assert_eq!(
        entry_word_counts(page.h(), &i_a, &d_a).unwrap(),
        WordCounts {
            added: 2,
            removed: 0
        }
    );
}

#[test]
fn bold_applied_mid_word_counts_once() {
    let mut page = Page::new(1);
    page.edit(A, |f, txn| insert_at_end(f, txn, 0, " gamma"));
    page.edit(A, |f, txn| {
        let t = text_of(f, txn, 0);
        let at = t.get_string(txn).find("mm").expect("mm") as u32;
        let bold: Attrs = [(Arc::from("bold"), Any::Bool(true))].into_iter().collect();
        t.format(txn, at, 2, bold);
    });
    let (i_a, d_a) = page.sets(A);
    let chunks = text_chunks(&entry_diff(page.h(), &i_a, &d_a).unwrap());
    assert!(
        chunks.iter().filter(|c| c.kind == ChunkKind::Added).count() >= 2,
        "precondition: the bold splits the word: {chunks:?}"
    );
    assert_eq!(
        entry_word_counts(page.h(), &i_a, &d_a).unwrap(),
        WordCounts {
            added: 1,
            removed: 0
        }
    );
}

#[test]
fn an_inline_node_starts_a_new_segment() {
    // "ab" typed, an inline node (a mention) embedded in the text, "cd"
    // typed after it: the view renders two highlighted words.
    let mut page = Page::new(1);
    page.edit(A, |f, txn| {
        let t = text_of(f, txn, 1);
        t.insert(txn, 0, "ab");
        t.insert_embed(txn, 2, XmlElementPrelim::empty("mention"));
        t.insert(txn, 3, "cd");
    });
    let (i_a, d_a) = page.sets(A);
    let chunks = text_chunks(&entry_diff(page.h(), &i_a, &d_a).unwrap());
    let added: Vec<&TextChunk> = chunks
        .iter()
        .filter(|c| c.kind == ChunkKind::Added)
        .collect();
    assert_eq!(added.len(), 2, "{chunks:?}");
    assert_ne!(added[0].segment, added[1].segment, "{chunks:?}");
    assert_eq!(
        entry_word_counts(page.h(), &i_a, &d_a).unwrap(),
        WordCounts {
            added: 2,
            removed: 0
        }
    );
}

#[test]
fn an_inline_node_between_texts_starts_a_new_segment() {
    // y-tiptap's shape: a mention is an element between two texts of the
    // paragraph. "ab" typed before it and "cd" after it are two words.
    let mut page = Page::new(1);
    page.edit(A, |f, txn| {
        let Some(XmlOut::Element(p)) = f.get(txn, 1) else {
            panic!("P2")
        };
        p.insert(txn, 0, XmlTextPrelim::new("ab"));
        p.insert(txn, 1, XmlElementPrelim::empty("mention"));
    });
    page.edit(A, |f, txn| {
        let Some(XmlOut::Element(p)) = f.get(txn, 1) else {
            panic!("P2")
        };
        let Some(XmlOut::Text(t)) = p.get(txn, 2) else {
            panic!("P2's old text")
        };
        t.insert(txn, 0, "cd");
    });
    let (i_a, d_a) = page.sets(A);
    let chunks = text_chunks(&entry_diff(page.h(), &i_a, &d_a).unwrap());
    let added: Vec<&TextChunk> = chunks
        .iter()
        .filter(|c| c.kind == ChunkKind::Added)
        .collect();
    assert_eq!(added.len(), 2, "{chunks:?}");
    assert_ne!(added[0].segment, added[1].segment, "{chunks:?}");
    assert_eq!(
        entry_word_counts(page.h(), &i_a, &d_a).unwrap(),
        WordCounts {
            added: 2,
            removed: 0
        }
    );
}

/// A doc where client 1 typed "hello world" and then `f` ran on client 2's
/// view of it; returns the replay (skip-GC), client 1's insertions and
/// client 2's effect.
fn after_second_client(
    f: impl FnOnce(&mut TransactionMut, &XmlFragmentRef),
) -> (Replayer, IdSet, IdSet, IdSet) {
    let a = Doc::with_client_id(1);
    let fa = a.get_or_insert_xml_fragment("default");
    let ua = txn_update(&a, |txn| push_paragraph(&fa, txn, "hello world"));
    let b = Doc::with_client_id(2);
    let fb = b.get_or_insert_xml_fragment("default");
    b.transact_mut()
        .apply_update(Update::decode_v1(&ua).unwrap())
        .unwrap();
    let ub = txn_update(&b, |txn| f(txn, &fb));
    let mut r = Replayer::new(ReplayMode::KeepDeleted);
    let first = r.apply(&ua);
    assert_eq!(first.outcome, RowOutcome::Applied);
    let e = r.apply(&ub);
    assert_eq!(e.outcome, RowOutcome::Applied);
    (r, first.inserted, e.inserted, e.deleted)
}

/// yrs 0.27.3's `find_index` divides by zero looking up clock 1 of a client
/// with one block of length 1 (`diff_range` → `split_by_snapshot`); the
/// union doc pads such clients.
#[test]
fn a_client_that_typed_one_character_is_counted() {
    let (r, _, ins, del) = after_second_client(|txn, frag| {
        text_of(frag, txn, 0).insert(txn, 5, "!");
    });
    let counts = catch_unwind(AssertUnwindSafe(|| entry_word_counts(r.doc(), &ins, &del)))
        .expect("no panic")
        .expect("counts");
    assert_eq!(
        counts,
        WordCounts {
            added: 1,
            removed: 0
        }
    );
}

/// ... and indexes past the list at clock 2 over two blocks of length 1 (a
/// client that only made an empty paragraph); the other author's words
/// still count.
#[test]
fn an_empty_paragraph_by_another_client_does_not_break_the_diff() {
    let (r, ins, _, _) = after_second_client(|txn, frag| {
        let n = frag.len(txn);
        let p = frag.insert(txn, n, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new(""));
    });
    let counts = catch_unwind(AssertUnwindSafe(|| {
        entry_word_counts(r.doc(), &ins, &IdSet::new())
    }))
    .expect("no panic")
    .expect("counts");
    assert_eq!(
        counts,
        WordCounts {
            added: 2,
            removed: 0
        }
    );
    // The pad never shows in the diff.
    let d = entry_diff(r.doc(), &ins, &IdSet::new()).unwrap();
    assert_eq!(texts_of(&text_chunks(&d), ChunkKind::Added), "hello world");
}
