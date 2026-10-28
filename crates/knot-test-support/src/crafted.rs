//! A crafted update that nests a page past the rooms' limit
//! (`knot_crdt::nesting::MAX_NESTING`) where only the authoritative check
//! after apply can see it, for the rooms' refusal tests: knot-crdt's room
//! tests and knot-server's socket tests share it.
//!
//! `check_update` (the rooms' prefilter) predicts where yrs will place an
//! update's items, so it can be bypassed: an update can hang a short chain
//! of new types off existing, legal, deep-but-within-limit content so the
//! chain's depth comes from the page, not from the update's own structure.
//! The F1 round 2 reviewer's probe does exactly that; [`crafted_attack`]
//! builds its update.

use yrs::block::{ClientID, HAS_ORIGIN, HAS_RIGHT_ORIGIN, ItemContent};
use yrs::branch::{Branch, BranchID};
use yrs::encoding::write::Write;
use yrs::types::TypeRef;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{Doc, ID, ReadTxn, StateVector, Transact, XmlElementPrelim, XmlElementRef, XmlFragment};

/// How many blockquotes the crafted update nests under its reparented head.
const CRAFTED_CHAIN: u32 = 500;
/// How many blockquotes the crafted base page is deep (within the limit).
const CRAFTED_BASE_DEPTH: usize = 500;

fn nested_id(el: &XmlElementRef) -> ID {
    match <XmlElementRef as AsRef<Branch>>::as_ref(el).id() {
        BranchID::Nested(id) => id,
        _ => panic!("root type has no nested id"),
    }
}

/// The reviewer's crafted update, as raw lib0 v1 bytes (yrs keeps `Update`'s
/// blocks private, so it is encoded by hand, exactly as the probe does):
/// `x` — a string whose parent is the deleted, garbage-collected paragraph
/// `dead`; `Y` — a blockquote with origin `x` and right origin `right` (an
/// item deep in the base page), which yrs reparents under `right`'s parent;
/// and [`CRAFTED_CHAIN`] blockquotes nested under `Y`.
fn attack_bytes(attacker: ClientID, start: u32, dead: &ID, right: &ID) -> Vec<u8> {
    let mut enc = EncoderV1::new();
    enc.write_var(1u32);
    enc.write_var(2 + CRAFTED_CHAIN);
    enc.write_client(attacker);
    enc.write_var(start);
    enc.write_info(4);
    enc.write_parent_info(false);
    enc.write_left_id(dead);
    ItemContent::String("x".into()).encode(&mut enc);
    enc.write_info(7 | HAS_ORIGIN | HAS_RIGHT_ORIGIN);
    enc.write_left_id(&ID::new(attacker, start));
    enc.write_right_id(right);
    ItemContent::Type(Branch::new(TypeRef::XmlElement("blockquote".into()))).encode(&mut enc);
    for i in 0..CRAFTED_CHAIN {
        enc.write_info(7);
        enc.write_parent_info(false);
        enc.write_left_id(&ID::new(attacker, start + 1 + i));
        ItemContent::Type(Branch::new(TypeRef::XmlElement("blockquote".into()))).encode(&mut enc);
    }
    enc.write_var(0u32);
    enc.to_vec()
}

/// The crafted base page (the full state update, from client 1) and the
/// crafted attack update (from client 99) that reparents onto it. The base
/// is [`CRAFTED_BASE_DEPTH`] blockquotes deep plus a bottom paragraph
/// (within the limit), and holds one deleted paragraph that yrs
/// garbage-collects — the item the attack's string hangs under. Applied
/// after the base, the attack nests its chain about 1,000 types deep: past
/// the limit, and well within what a worker's stack deletes.
pub fn crafted_attack() -> (Vec<u8>, Vec<u8>) {
    // Built on a thread whose stack holds any recursion of yrs.
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(|| {
            let doc = Doc::with_client_id(1);
            let frag = doc.get_or_insert_xml_fragment("default");
            let dead;
            let bottom;
            {
                let mut txn = doc.transact_mut();
                let d = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                dead = nested_id(&d);
                let mut q = frag.push_back(&mut txn, XmlElementPrelim::empty("blockquote"));
                for _ in 1..CRAFTED_BASE_DEPTH {
                    q = q.push_back(&mut txn, XmlElementPrelim::empty("blockquote"));
                }
                let p = q.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                bottom = nested_id(&p);
            }
            {
                let mut txn = doc.transact_mut();
                frag.remove_range(&mut txn, 0, 1);
            }
            let base = doc
                .transact()
                .encode_state_as_update_v1(&StateVector::default());
            let attack = attack_bytes(ClientID::new(99), 0, &dead, &bottom);
            (base, attack)
        })
        .expect("spawn")
        .join()
        .expect("building the update panicked")
}
