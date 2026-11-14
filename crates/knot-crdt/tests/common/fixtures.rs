//! Updates and frames that knot-crdt's room tests build and compare, and
//! the reproducers of yrs 0.27.3's pending-encoding and skip-underflow
//! defects: the one copy of each. The integration tests include this file
//! by path (`#[path = "common/fixtures.rs"] mod fixtures;`), knot-history's
//! tests and its `replay_throughput` example include it as `fixtures` too,
//! and the crate's own unit tests include it as `crate::test_fixtures` (see
//! `lib.rs`), so it needs nothing but `yrs`: no `knot_crdt` item, which the
//! unit tests and knot-history could not name by that path.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use yrs::block::{ClientID, ItemContent};
use yrs::branch::Branch;
use yrs::encoding::write::Write;
use yrs::types::TypeRef;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{
    Assoc, Doc, ID, IndexedSequence, ReadTxn, StateVector, Text, Transact, TransactionMut, Update,
    XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlOut, XmlTextPrelim, XmlTextRef,
};

/// Whether two updates carry the same insertions and deletions: a room
/// persists and fans out what an apply changed, which yrs re-encodes, so its
/// bytes may differ from what a client sent. False when either does not
/// decode, so assert it holds; `!same_update(..)` would also hold for bytes
/// that are no update at all.
pub fn same_update(a: &[u8], b: &[u8]) -> bool {
    match (Update::decode_v1(a), Update::decode_v1(b)) {
        (Ok(a), Ok(b)) => {
            a.insertions(true) == b.insertions(true) && a.delete_set() == b.delete_set()
        }
        _ => false,
    }
}

/// The update inside a sync update frame (`protocol::wrap_sync_update`:
/// `[0, 2]`, the update's length as a varint, the update), or `None` when
/// `frame` is not one or its length does not match.
pub fn frame_update(frame: &[u8]) -> Option<&[u8]> {
    let [0, 2, rest @ ..] = frame else {
        return None;
    };
    let (mut len, mut shift, mut at) = (0usize, 0, 0);
    loop {
        let &b = rest.get(at)?;
        at += 1;
        len |= ((b & 0x7f) as usize) << shift;
        if b < 0x80 {
            break;
        }
        shift += 7;
    }
    (rest.len() - at == len).then(|| &rest[at..])
}

/// The update inside a sync update frame; panics when `frame` is not one.
pub fn unwrap_frame(frame: &[u8]) -> Vec<u8> {
    frame_update(frame)
        .expect("a sync update frame of the right length")
        .to_vec()
}

/// Whether `frame` is a sync update frame carrying `update` (as
/// [`same_update`]).
pub fn is_frame_of(frame: &[u8], update: &[u8]) -> bool {
    frame_update(frame).is_some_and(|inside| same_update(inside, update))
}

/// A paragraph holding `text`, from a fresh doc of `client`.
pub fn para(client: u64, text: &str) -> Vec<u8> {
    let doc = Doc::with_client_id(client);
    let frag = doc.get_or_insert_xml_fragment("default");
    {
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new(text));
    }
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

/// A page holding one paragraph "hello" from `client`: its state, its doc,
/// and the id of the text's first character (an item that holds a string,
/// not a type).
pub fn text_page(client: u64) -> (Vec<u8>, Doc, ID) {
    let doc = Doc::with_client_id(client);
    let frag = doc.get_or_insert_xml_fragment("default");
    let first = {
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        let text = p.push_back(&mut txn, XmlTextPrelim::new("hello"));
        let index = text
            .sticky_index(&txn, 0, Assoc::After)
            .expect("the text has a first character");
        *index.id().expect("a character has an id")
    };
    let state = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    (state, doc, first)
}

/// An ordinary inconsistent update, which yrs integrates part of and then
/// rejects (`UpdateError::InvalidParent`): from client 77, an element
/// `partial` in the page's root fragment, then a string whose parent is
/// `not_a_type`, an item that holds text. yrs's own encoder never writes
/// such an update; a buggy or crafted client can. Encoded by hand, as
/// `knot_test_support::crafted` encodes the crafted attack. While the doc
/// lacks `not_a_type`, yrs integrates `partial` and parks the string until
/// that item arrives.
pub fn rejected_part_way(not_a_type: &ID) -> Vec<u8> {
    let mut enc = EncoderV1::new();
    enc.write_var(1u32);
    enc.write_var(2u32);
    enc.write_client(ClientID::new(77));
    enc.write_var(0u32);
    enc.write_info(7);
    enc.write_parent_info(true);
    enc.write_string("default");
    ItemContent::Type(Branch::new(TypeRef::XmlElement("partial".into()))).encode(&mut enc);
    enc.write_info(4);
    enc.write_parent_info(false);
    enc.write_left_id(not_a_type);
    ItemContent::String("y".into()).encode(&mut enc);
    enc.write_var(0u32);
    enc.to_vec()
}

/// A crafted update whose string yrs parks, as it waits for client 5's
/// text; the innocent edit that brings that text (its apply integrates the
/// edit, then retries the parked string, which fails, since text is no
/// type); and the edit's client doc.
pub fn parked_block_and_the_edit_it_fails_on() -> (Vec<u8>, Vec<u8>, Doc) {
    let (edit, doc, text) = text_page(5);
    (rejected_part_way(&text), edit, doc)
}

/// A seeded xorshift64: the histories are reproducible from the seed alone.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }

    pub fn word(&mut self) -> String {
        let len = 1 + self.below(6);
        (0..len)
            .map(|_| char::from(b'a' + self.below(26) as u8))
            .collect()
    }
}

/// The text of the `i`th paragraph of `frag`.
pub fn text_at(frag: &XmlFragmentRef, txn: &impl ReadTxn, i: u32) -> Option<XmlTextRef> {
    match frag.get(txn, i)? {
        XmlOut::Element(p) => match p.get(txn, 0)? {
            XmlOut::Text(t) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

/// One random transaction on `doc`: a new paragraph, a word typed into
/// one, or a few characters deleted.
pub fn random_edit(doc: &Doc, frag: &XmlFragmentRef, rng: &mut Rng) {
    let mut txn = doc.transact_mut();
    let n = frag.len(&txn);
    let pick = if n > 0 { rng.below(3) } else { 2 };
    let target = if n > 0 {
        text_at(frag, &txn, rng.below(n as usize) as u32)
    } else {
        None
    };
    match (pick, target) {
        (0, Some(t)) => {
            let at = rng.below(t.len(&txn) as usize + 1) as u32;
            t.insert(&mut txn, at, &rng.word());
        }
        (1, Some(t)) if t.len(&txn) > 2 => {
            let at = rng.below(t.len(&txn) as usize - 2) as u32;
            t.remove_range(&mut txn, at, 1 + rng.below(2) as u32);
        }
        _ => {
            let at = rng.below(n as usize + 1) as u32;
            let p = frag.insert(&mut txn, at, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(rng.word()));
        }
    }
}

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

/// The text of the `i`th paragraph of `frag`; panics when there is none.
fn text_of(frag: &XmlFragmentRef, txn: &impl ReadTxn, i: u32) -> XmlTextRef {
    text_at(frag, txn, i).unwrap_or_else(|| panic!("paragraph {i} holds a text"))
}

/// One client's five transactions on one paragraph, which leave a doc with
/// a pending update that holds overlapping Skips (see
/// [`overlapping_skips_as`]).
pub struct OverlappingSkips {
    /// "base", a new paragraph.
    pub t0: Vec<u8>,
    /// "WWW" after "base" (withheld from the doc).
    pub t1: Vec<u8>,
    /// "pp" after the "WWW": parks on `t1`.
    pub t2: Vec<u8>,
    /// "zz" before "base" (withheld from the doc).
    pub t2b: Vec<u8>,
    /// A new paragraph "new", which integrates past the gap `t2b` leaves,
    /// and "qq" inside the "WWW", which parks on `t1`.
    pub t3: Vec<u8>,
}

/// [`overlapping_skips_as`] by client 1.
pub fn overlapping_skips() -> OverlappingSkips {
    overlapping_skips_as(1)
}

/// The five transactions of [`OverlappingSkips`] by `client`. A doc that
/// applied `t0`, `t2` and `t3` holds "pp" and "qq" pending: yrs 0.27.3
/// merges `t3`'s parked remainder into the pending update without splitting
/// its Skips, so the pending update holds overlapping Skips, and its v1
/// encoding (which `encode_state_as_update_v1` merges into a full state)
/// writes "qq" at a shifted clock. `t1` and `t2b` drain it. Pass a client
/// other than 1 and 2 to combine it with [`skip_underflow_case`].
pub fn overlapping_skips_as(client: u64) -> OverlappingSkips {
    let c = Doc::with_client_id(client);
    let frag = c.get_or_insert_xml_fragment("default");
    let t0 = txn_update(&c, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("base"));
    });
    let t1 = txn_update(&c, |txn| text_of(&frag, txn, 0).insert(txn, 4, "WWW"));
    let t2 = txn_update(&c, |txn| text_of(&frag, txn, 0).insert(txn, 7, "pp"));
    let t2b = txn_update(&c, |txn| text_of(&frag, txn, 0).insert(txn, 0, "zz"));
    let t3 = txn_update(&c, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("new"));
        text_of(&frag, txn, 0).insert(txn, 7, "qq");
    });
    OverlappingSkips {
        t0,
        t1,
        t2,
        t2b,
        t3,
    }
}

/// Content parked on another client's edit, with no Skip: u1 by client 11
/// writes a paragraph "front"; u2 by client 12, from clock 0, types
/// "back held" inside it. A doc holding only u2 parks it, and its yrs full
/// state names no block past a gap. Clients 11 and 12 keep it disjoint from
/// [`overlapping_skips`] and [`skip_underflow_case`].
pub fn cross_client_parked() -> (Vec<u8>, Vec<u8>) {
    let (u1, u2, _) = cross_client_parked_tail(0);
    (u1, u2)
}

/// [`cross_client_parked`]'s u1 and u2, then `n` one-character updates that
/// client 12 types after u2's last character, each its own transaction.
/// Each one's origin is client 12's previous character, so each parks while
/// u1 is missing.
pub fn cross_client_parked_tail(n: usize) -> (Vec<u8>, Vec<u8>, Vec<Vec<u8>>) {
    let a = Doc::with_client_id(11);
    let frag = a.get_or_insert_xml_fragment("default");
    let u1 = txn_update(&a, |txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("front"));
    });
    let b = Doc::with_client_id(12);
    let frag = b.get_or_insert_xml_fragment("default");
    b.transact_mut()
        .apply_update(Update::decode_v1(&u1).expect("u1 decodes"))
        .expect("u1 applies");
    let at = "front".len() as u32;
    let u2 = txn_update(&b, |txn| {
        text_of(&frag, txn, 0).insert(txn, at, "back held")
    });
    let mut end = at + "back held".len() as u32;
    let tail = (0..n)
        .map(|i| {
            let ch = char::from(b'a' + (i % 26) as u8).to_string();
            let u = txn_update(&b, |txn| text_of(&frag, txn, 0).insert(txn, end, &ch));
            end += 1;
            u
        })
        .collect();
    (u1, u2, tail)
}

/// yrs 0.27.3's skip underflow (`block_store.rs:318`), minimised from what
/// `find_the_skip_underflow` (knot-history's `tests/replay.rs`, M1.2's
/// property generator with its `has_gaps` guards removed) found: on a debug
/// build it panics applying a reconnect resend to the room doc at seed 237,
/// step 36 (and a client's pull of the server's state at seed 80 first).
/// Rebuilt by hand with plain yrs docs:
///
/// - client 1 makes paragraph A "keep" and paragraph B "x" (1#0..1#9);
/// - client 2 types "ab" at the end of B's text (T1, 2#0..2#2), then "c" at
///   its start (T2, 2#2), which depends only on client 1's text, not on T1;
/// - client 2 removes B, so its doc GCs B's text and everything in it, and
///   squashes 2#0..2#3 into one GC block.
///
/// The doc applied client 1's paragraphs and T2 only: it holds 2#2 past a
/// gap, behind a Skip over 2#0..2#2 (state vector `2: 0`). The update is
/// client 2's resend against that state vector: one GC block 2#0..2#3 and
/// the delete set. The GC block meets the doc's Skip 2#0..2#2 and runs past
/// its end into 2#2, which the doc holds. yrs trims what a doc holds before
/// integrating (`BlockSet::exclude`), but splitting a GC block
/// (`Block::splice`, `block.rs:355-360`) returns its right half without
/// shortening the left one, so the whole 2#0..2#3 is pushed into the Skip
/// and `skip.next_clock() - block.next_clock()` (2 − 3) underflows: a panic
/// in debug builds, a wrapped Skip that corrupts the block list in release
/// builds.
///
/// The room state is client 2's full state: it holds the update's content
/// squashed with the blocks after it (the same GC block 2#0..2#3, which
/// covers T2's 2#2), so a probe of it on the doc meets the same Skip.
///
/// Returns the doc's full state, the update, and the room's full state.
pub fn skip_underflow_case() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (
        unhex(
            "020202000a02440108016306010007010764656661756c740309706172616772617068070001000604\
             000101046b6565708701000309706172616772617068070001060604000107017800",
        ),
        unhex("010102000003020101060302010003"),
        unhex(
            "02010200000305010007010764656661756c740309706172616772617068070001000604000101046b\
             656570810100010002020101060302010003",
        ),
    )
}

/// yrs 0.27.3's full state (`encode_state_as_update_v1`) of a live doc
/// that the interleaving generator built (`tests/pending_state.rs`'s
/// property, seed 209, step 44, three clients), whose pending update holds
/// overlapping Skips. The encoding writes client 2's pending blocks at
/// shifted clocks and names 2#148, which the live doc neither holds nor has
/// pending, yet a fresh doc that applies these bytes parks nothing: every
/// shifted block's dependencies are in the bytes, so the phantom
/// integrates as text. It names blocks past a gap.
pub fn shifted_state_that_parks_nothing() -> Vec<u8> {
    unhex(
        "032e030007010764656661756c740309706172616772617068070003000604000301016d810302014403\
         02037869784703000309706172616772617068070003070604000308017481030903c703070300030970\
         61726167726170680700030d060400030e016781030f0184031002717a8403120176c10310031102c403\
         1503110172810300010003c70206023b03097061726167726170680700031b060400031c016781022501\
         000784030c026571c70307022c0309706172616772617068070003280601000329018403030476716d6c\
         c70220030d03097061726167726170680700032f060400033003766b74c7031b023b0309706172616772\
         61706807000334060100033502840337016784022b0361687ac102000206010006c70225031e03097061\
         72616772617068070003430604000344066d6f6e726268c10222022301c7031b03340309706172616772\
         6170680700034c060400034d04656b65784e020007010764656661756c74030970617261677261706807\
         0002000604000201017a8102020387020003097061726167726170680700020606040002070174810208\
         01840209016d87020603097061726167726170680700020b060100020c0284020a017781020f0146020d\
         04626f6c64047472756586020e04626f6c64046e756c6c8402100466777063c70206020b030970617261\
         6772617068070002170604000218016284020502687084021b01774402190170c4021d0219016ec4021e\
         0219016ac70307030d030970617261677261706807000220060400022101728102220287031703097061\
         72616772617068070002250604000226057861666d62c103070220010007840219066e6e70656b768402\
         160175c70206021703097061726167726170680700023b060400023c06666f6f686c71c70206023b0309\
         70617261677261706807000243060100024402c4021e021f027366c70100020003097061726167726170\
         6807000249060100024a0284024c01714402220a74637872727269747269c6021d021e04636f64650474\
         727565c6021e024704636f6465046e756c6c47010003097061726167726170680700025a060400025b05\
         6a61626d65840246027177840242046676786ec10200020601000547025a030970617261677261706807\
         00026d060400026e0576776c7876c60205021a04636f64650474727565c6021b021c04636f6465046e75\
         6c6cc4031203130161c402760313026764c7023b0217030970617261677261706807000279060400027a\
         0163c60261026204626f6c64047472756586026204626f6c64046e756c6cc7020b030703097061726167\
         726170680700027e060400027f027772440304026d6e0a0384030c066861777a6c730a02c40276027705\
         6b6166747984029301016c17010007010764656661756c74030970617261677261706807000100060100\
         0101020a01c7010002000309706172616772617068070001050604000106016d81010702840109026570\
         87030003097061726167726170680700010c060400010d016181010e038401110265668401030462757a\
         6b44010e016c0a01c70300010c03097061726167726170680700011a060400011b03736773c702000206\
         03097061726167726170680700011f0604000120026968030103020208020f030209030309010d021001\
         23022c0845024b026706030a03010a031001140217041e082a0136023c074b01",
    )
}

/// yrs 0.27.3's full state of a live doc the interleaving generator built
/// (seed 79, step 62), whose pending update holds overlapping Skips. The
/// encoding writes client 3's pending blocks at clocks shifted by 5, so a
/// paragraph's text names a string item as its parent: a fresh doc's apply
/// is refused part way (`UpdateError::InvalidParent`), before the delete
/// set. No id it names is a phantom. A room booting from such a snapshot
/// fails. It names blocks past a gap.
pub fn shifted_state_yrs_refuses() -> Vec<u8> {
    unhex(
        "034a030001010764656661756c740100020004000100028103000100050002c10300030a010006410300\
         01000687030a03097061726167726170680700032006040003210161810322038403250265660003c703\
         0a032003097061726167726170680700032b060400032c016884032701630002c7022103120309706172\
         616772617068070003310604000332016f840333026f77c4033303340461646162c403390334016fc702\
         11020003097061726167726170680700033b060100033c03c60325032604636f64650474727565c60326\
         032704636f6465046e756c6cc10105010605c403460106016d4702280309706172616772617068070003\
         480604000349026367c602180219066974616c69630474727565c6020e020f066974616c6963046e756c\
         6c0002c70100020a03097061726167726170680700035006010003510184035201618403530171470348\
         030970617261677261706807000355060400035602796c840358026e6c0001c1035203530684034b0163\
         c401040109046e64716246034a04636f646504747275650a01c60358035904626f6c6404747275658603\
         5a04626f6c64046e756c6c47035503097061726167726170680700036b060400036c0262650a05840362\
         04786a736dc403340335026a660a01440322067a777a736266c7033b0200030970617261677261706807\
         00037c060400037d066e7964696a79c70211033b03097061726167726170680700038401060400038501\
         026b6584038c010576766b69613a020001010764656661756c7401000947020003097061726167726170\
         680700020a060400020b02756c81020d0181020e0184020f0178c7020a02000309706172616772617068\
         0700021106040002120378696d81021501c1020d020e02c10218020e03c1021b020e0181021604c10300\
         030a010005c4020f0210016d4101000100050001000300030001c7021102000309706172616772617068\
         0700023606040002370566696b7773c70211023603097061726167726170680700023d060400023e046e\
         7167758402420162c402420243046e6d6a6e000244023f03737167c40103010402676ec1024e01040400\
         034702280309706172616772617068070002560604000257026e6ec1021b021c03440258027a680001c4\
         021b025a037a6f7187030a0309706172616772617068070002630604000264036477618402670372796d\
         c70100020a03097061726167726170680700026b060400026c027566440213046c736f7ac4025e025801\
         70c402730258027a76c4026702680677746379716ec40275025805796d756962c4027302740361676a1a\
         010007010764656661756c740309706172616772617068070001000604000101016f8401020174840103\
         01648401040167810105018401060173c601020103066974616c69630474727565c60104010506697461\
         6c6963046e756c6c0004c401050106027966c1010f010602000247010003097061726167726170680700\
         0114060400011503716d79810118018401190261710006c7010002000309706172616772617068070001\
         220604000123027376c7010001220309706172616772617068070001260604000127066a7a6776717303\
         010506010a04100419011c060208000a0e021611280e48024f075a035f01030a0020230328032f023d03\
         42054e0252015b077201",
    )
}

fn unhex(s: &str) -> Vec<u8> {
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    s.chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex"))
        .collect()
}
