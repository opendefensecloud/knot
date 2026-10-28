//! The items of a yrs update, read without applying it
//! (`knot_crdt::update_items`), as nesting's prefilter and Duplicate's
//! remap read them.

use std::fmt::Write as _;

use knot_crdt::update_items::{ItemParent, UpdateItem, decode};
use yrs::block::{
    BLOCK_GC_REF_NUMBER, BLOCK_ITEM_STRING_REF_NUMBER, BLOCK_SKIP_REF_NUMBER, ClientID, HAS_ORIGIN,
    ItemContent,
};
use yrs::encoding::write::Write;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{Any, Array, ArrayPrelim, Doc, ID, Map, ReadTxn, StateVector, Text, Transact, Update};

fn id(client: u64, clock: u32) -> ID {
    ID::new(ClientID::new(client), clock)
}

/// An item on one line: `client:clock (len)`, then `after` its origin,
/// `before` its right origin, `in` the parent it names (a root's name or
/// an item's id) with `[key]` when it has one, and its content. A shared
/// type's content is just "type".
fn describe(item: &UpdateItem) -> String {
    let id = |id: &ID| format!("{}:{}", id.client.get(), id.clock);
    let mut out = format!("{} ({})", id(&item.id), item.len);
    if let Some(origin) = &item.origin {
        write!(out, " after {}", id(origin)).unwrap();
    }
    if let Some(right) = &item.right_origin {
        write!(out, " before {}", id(right)).unwrap();
    }
    match &item.parent {
        ItemParent::Root(name) => write!(out, " in {name}").unwrap(),
        ItemParent::Item(parent) => write!(out, " in {}", id(parent)).unwrap(),
        ItemParent::OfOrigin => {}
    }
    if let Some(key) = &item.parent_sub {
        write!(out, "[{key}]").unwrap();
    }
    let content = match &item.content {
        ItemContent::String(s) => format!("string {:?}", s.as_str()),
        ItemContent::Embed(value) => format!("embed {value}"),
        ItemContent::Any(values) => format!("any {values:?}"),
        ItemContent::Type(_) => "type".to_string(),
        ItemContent::Deleted(n) => format!("deleted {n}"),
        other => format!("{other:?}"),
    };
    write!(out, ": {content}").unwrap();
    out
}

/// The items of `update`, described, by id.
fn seen(update: &[u8]) -> Vec<String> {
    let mut items: Vec<UpdateItem> = decode(update)
        .map(|item| item.expect("the update decodes"))
        .collect();
    items.sort_by_key(|item| (item.id.client, item.id.clock));
    items.iter().map(describe).collect()
}

fn state(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

/// A text "ab" plus an embed, and a map with a value and a list, written
/// by client 1; then client 2 writes "y" between "a" and "b".
fn page() -> Vec<u8> {
    let one = Doc::with_client_id(1);
    let text = one.get_or_insert_text("t");
    let map = one.get_or_insert_map("m");
    {
        let mut txn = one.transact_mut();
        text.insert(&mut txn, 0, "ab");
        text.insert_embed(&mut txn, 2, Any::Bool(true));
        map.insert(&mut txn, "k", "v");
        let list = map.insert(&mut txn, "list", ArrayPrelim::default());
        list.push_back(&mut txn, "x");
    }
    let two = Doc::with_client_id(2);
    let text = two.get_or_insert_text("t");
    {
        let mut txn = two.transact_mut();
        txn.apply_update(Update::decode_v1(&state(&one)).expect("decode"))
            .expect("apply");
        text.insert(&mut txn, 1, "y");
    }
    state(&two)
}

#[test]
fn items_carry_their_ids_origins_parents_and_content() {
    assert_eq!(
        seen(&page()),
        [
            // "ab", split around client 2's "y".
            r#"1:0 (1) in t: string "a""#,
            r#"1:1 (1) after 1:0: string "b""#,
            r#"1:2 (1) after 1:1: embed true"#,
            r#"1:3 (1) in m[k]: any [String("v")]"#,
            r#"1:4 (1) in m[list]: type"#,
            r#"1:5 (1) in 1:4: any [String("x")]"#,
            r#"2:0 (1) after 1:0 before 1:1: string "y""#,
        ]
    );
}

#[test]
fn a_string_spans_its_utf16_length() {
    let doc = Doc::with_client_id(1);
    let text = doc.get_or_insert_text("t");
    text.insert(&mut doc.transact_mut(), 0, "Grüße 🙂");
    let n = "Grüße 🙂".encode_utf16().count();
    assert_eq!(
        seen(&state(&doc)),
        [format!(r#"1:0 ({n}) in t: string "Grüße 🙂""#)]
    );
}

#[test]
fn a_deleted_item_is_read_with_its_deleted_content() {
    let doc = Doc::with_client_id(1);
    let text = doc.get_or_insert_text("t");
    text.insert(&mut doc.transact_mut(), 0, "abc");
    text.remove_range(&mut doc.transact_mut(), 1, 1);
    assert_eq!(
        seen(&state(&doc)),
        [
            r#"1:0 (1) in t: string "a""#,
            r#"1:1 (1) after 1:0: deleted 1"#,
            r#"1:2 (1) after 1:1: string "c""#,
        ]
    );
}

#[test]
fn an_empty_doc_has_no_items() {
    assert_eq!(seen(&state(&Doc::new())), Vec::<String>::new());
}

/// Writes a string item `s` at the start of the root text `t`, or after
/// `origin`.
fn write_string(enc: &mut EncoderV1, origin: Option<&ID>, s: &str) {
    match origin {
        Some(origin) => {
            enc.write_info(BLOCK_ITEM_STRING_REF_NUMBER | HAS_ORIGIN);
            enc.write_left_id(origin);
        }
        None => {
            enc.write_info(BLOCK_ITEM_STRING_REF_NUMBER);
            enc.write_parent_info(true);
            enc.write_string("t");
        }
    }
    ItemContent::String(s.into()).encode(enc);
}

#[test]
fn collected_and_skipped_ranges_count_their_clocks() {
    // Client 7 from clock 10: 3 collected clocks, "ab", 5 skipped clocks,
    // then "c" after "b".
    let mut enc = EncoderV1::new();
    enc.write_var(1u32); // one client
    enc.write_var(4u32); // four blocks
    enc.write_client(ClientID::new(7));
    enc.write_var(10u32);
    enc.write_info(BLOCK_GC_REF_NUMBER);
    enc.write_len(3);
    write_string(&mut enc, None, "ab");
    enc.write_info(BLOCK_SKIP_REF_NUMBER);
    enc.write_var(5u32);
    write_string(&mut enc, Some(&id(7, 14)), "c");
    enc.write_var(0u32); // no deletions
    let update = enc.to_vec();
    Update::decode_v1(&update).expect("yrs reads it too");
    assert_eq!(
        seen(&update),
        [
            r#"7:13 (2) in t: string "ab""#,
            r#"7:20 (1) after 7:14: string "c""#,
        ]
    );
}

#[test]
fn an_empty_item_is_left_out() {
    // yrs drops an item without content, and counts no clock for it.
    let mut enc = EncoderV1::new();
    enc.write_var(1u32);
    enc.write_var(2u32);
    enc.write_client(ClientID::new(7));
    enc.write_var(0u32);
    write_string(&mut enc, None, "");
    write_string(&mut enc, None, "a");
    enc.write_var(0u32);
    assert_eq!(seen(&enc.to_vec()), [r#"7:0 (1) in t: string "a""#]);
}

#[test]
fn an_update_that_does_not_decode_ends_in_one_error() {
    let update = page();
    // Cut inside the blocks: the items before the cut, one error, then
    // nothing more.
    let cut = &update[..update.len() / 2];
    let mut items = decode(cut);
    let read: Vec<_> = items.by_ref().collect();
    assert!(read.len() > 1, "some items come before the cut: {read:?}");
    let (last, before) = read.split_last().expect("something is read");
    assert!(last.is_err(), "the last is the error: {last:?}");
    assert!(before.iter().all(Result::is_ok), "{before:?}");
    assert!(items.next().is_none(), "nothing after the error");

    let mut empty = decode(&[]);
    assert!(matches!(empty.next(), Some(Err(_))));
    assert!(empty.next().is_none());
}
