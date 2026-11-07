//! Updates and frames that knot-crdt's room tests build and compare: the
//! one copy of each. The integration tests include this file by path
//! (`#[path = "common/fixtures.rs"] mod fixtures;`), and the crate's own unit
//! tests include it as `crate::test_fixtures` (see `lib.rs`), so it needs
//! nothing but `yrs`: no `knot_crdt` item, which the unit tests could not
//! name by that path.
#![allow(dead_code)]

use yrs::block::{ClientID, ItemContent};
use yrs::branch::Branch;
use yrs::encoding::write::Write;
use yrs::types::TypeRef;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{
    Assoc, Doc, ID, IndexedSequence, ReadTxn, StateVector, Text, Transact, Update,
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
