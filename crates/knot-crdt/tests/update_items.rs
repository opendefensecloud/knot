//! The items of a yrs update, read without applying it
//! (`knot_crdt::update_items`), as nesting's prefilter and Duplicate's
//! remap read them, and as a room reads every inbound update before yrs
//! does.

mod common;

use std::fmt::Write as _;

use knot_crdt::update_items::{ItemParent, UpdateItem, decode};
use knot_crdt::{Engine, YrsEngine};
use knot_test_support::crafted::crafted_attack;
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

/// A text "Grüße 🙂", by client 1.
fn non_ascii_text() -> Vec<u8> {
    let doc = Doc::with_client_id(1);
    let text = doc.get_or_insert_text("t");
    text.insert(&mut doc.transact_mut(), 0, "Grüße 🙂");
    state(&doc)
}

#[test]
fn a_string_spans_its_utf16_length() {
    let n = "Grüße 🙂".encode_utf16().count();
    assert_eq!(
        seen(&non_ascii_text()),
        [format!(r#"1:0 ({n}) in t: string "Grüße 🙂""#)]
    );
}

/// A text "abc" by client 1, whose "b" was deleted.
fn with_a_deleted_item() -> Vec<u8> {
    let doc = Doc::with_client_id(1);
    let text = doc.get_or_insert_text("t");
    text.insert(&mut doc.transact_mut(), 0, "abc");
    text.remove_range(&mut doc.transact_mut(), 1, 1);
    state(&doc)
}

#[test]
fn a_deleted_item_is_read_with_its_deleted_content() {
    assert_eq!(
        seen(&with_a_deleted_item()),
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

/// Client 7 from clock 10: 3 collected clocks, "ab", 5 skipped clocks, then
/// "c" after "b".
fn collected_and_skipped() -> Vec<u8> {
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
    enc.to_vec()
}

#[test]
fn collected_and_skipped_ranges_count_their_clocks() {
    let update = collected_and_skipped();
    Update::decode_v1(&update).expect("yrs reads it too");
    assert_eq!(
        seen(&update),
        [
            r#"7:13 (2) in t: string "ab""#,
            r#"7:20 (1) after 7:14: string "c""#,
        ]
    );
}

/// From client 7: an empty string, then "a".
fn with_an_empty_item() -> Vec<u8> {
    let mut enc = EncoderV1::new();
    enc.write_var(1u32);
    enc.write_var(2u32);
    enc.write_client(ClientID::new(7));
    enc.write_var(0u32);
    write_string(&mut enc, None, "");
    write_string(&mut enc, None, "a");
    enc.write_var(0u32);
    enc.to_vec()
}

#[test]
fn an_empty_item_is_left_out() {
    // yrs drops an item without content, and counts no clock for it.
    assert_eq!(seen(&with_an_empty_item()), [r#"7:0 (1) in t: string "a""#]);
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

/// The delete set after the blocks is read too, as yrs reads it: an update
/// that ends inside it doesn't decode, after all its items.
#[test]
fn an_update_whose_delete_set_ends_early_does_not_decode() {
    let update = with_a_deleted_item();
    let cut = &update[..update.len() - 1];
    Update::decode_v1(cut).expect_err("yrs doesn't read it");
    let read: Vec<_> = decode(cut).collect();
    let (last, items) = read.split_last().expect("something is read");
    assert!(last.is_err(), "the last is the error: {last:?}");
    assert_eq!(items.len(), 3, "every item comes first: {items:?}");
    assert!(items.iter().all(Result::is_ok), "{items:?}");
}

/// One client's blocks from `clock`: `blocks` writes them.
fn client_from(clock: u32, blocks: u32, write: impl FnOnce(&mut EncoderV1)) -> Vec<u8> {
    let mut enc = EncoderV1::new();
    enc.write_var(1u32);
    enc.write_var(blocks);
    enc.write_client(ClientID::new(7));
    enc.write_var(clock);
    write(&mut enc);
    enc.write_var(0u32);
    enc.to_vec()
}

/// A block's clocks must fit in a u32: an item or a skipped range that runs
/// past `u32::MAX` doesn't decode. Adding its length to the clock
/// overflowed, which panicked in a debug build and wrapped in a release
/// build. The last clock may be `u32::MAX` itself.
#[test]
fn a_block_whose_clocks_run_past_u32_max_does_not_decode() {
    let item = client_from(u32::MAX - 1, 1, |enc| write_string(enc, None, "abc"));
    let skipped = client_from(u32::MAX - 1, 1, |enc| {
        enc.write_info(BLOCK_SKIP_REF_NUMBER);
        enc.write_var(5u32);
    });
    for (what, update) in [("an item", item), ("a skipped range", skipped)] {
        let read: Vec<_> = decode(&update).collect();
        assert!(
            matches!(read.as_slice(), [Err(_)]),
            "{what} past u32::MAX: {read:?}"
        );
    }

    assert_eq!(
        seen(&to_the_end()),
        [format!(r#"7:{} (3) in t: string "abc""#, u32::MAX - 3)]
    );
}

/// An item whose last clock is `u32::MAX`.
fn to_the_end() -> Vec<u8> {
    client_from(u32::MAX - 3, 1, |enc| write_string(enc, None, "abc"))
}

/// The updates of the engine's own tests (`src/engine.rs`), built as they
/// build them: a state with a delete set, a diff that only deletes, an
/// update that waits for a missing dependency, the state of a doc that
/// holds it pending (which re-sends it), and three bytes that are no
/// update.
fn the_engines_updates() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut add = |what: &str, update: Vec<u8>| out.push((format!("engine: {what}"), update));

    let src = Doc::new();
    let t = src.get_or_insert_text("t");
    t.push(&mut src.transact_mut(), "hellox");
    t.remove_range(&mut src.transact_mut(), 5, 1);
    add("a state with a delete set", state(&src));
    let before = src.transact().state_vector();
    t.remove_range(&mut src.transact_mut(), 0, 1);
    add(
        "a diff that only deletes",
        src.transact().encode_state_as_update_v1(&before),
    );

    let src = Doc::new();
    let t = src.get_or_insert_text("t");
    t.push(&mut src.transact_mut(), "a");
    let after_a = src.transact().state_vector();
    t.push(&mut src.transact_mut(), "b");
    let only_b = src.transact().encode_state_as_update_v1(&after_a);
    let d = YrsEngine.new_doc();
    YrsEngine.apply_update(&d, &only_b).expect("apply");
    add("an update missing its dependency", only_b);
    add("a state re-sending what is pending", state(d.inner()));

    add("three bytes that are no update", vec![0xff, 0xff, 0xff]);
    out
}

/// The updates of the conformance tests (`tests/conformance.rs`): an empty
/// doc's state, and the diff a doc sends for another's state vector.
fn the_conformance_updates() -> Vec<(String, Vec<u8>)> {
    let e = YrsEngine;
    let (a, b) = (e.new_doc(), e.new_doc());
    let sv_b = e.encode_state_vector(&b).expect("state vector");
    vec![
        (
            "conformance: an empty doc's state".to_string(),
            e.encode_state_as_update(&a, None).expect("state"),
        ),
        (
            "conformance: a diff for a state vector".to_string(),
            e.encode_state_as_update(&a, Some(&sv_b)).expect("diff"),
        ),
    ]
}

/// The reader reads every update knot-crdt's tests build as yrs reads it,
/// and every update that ends early in one (`common::disagreements`):
/// this file's, the engine's and the conformance tests', and the crafted
/// base page and attack (`knot_test_support::crafted`); `tests/nesting.rs`
/// checks its own. Left out: the two updates whose clocks run past
/// `u32::MAX` (above), which the reader refuses on purpose where yrs's
/// decoder panics in a debug build and wraps in a release build.
#[test]
fn the_reader_reads_the_updates_of_these_tests_as_yrs_does() {
    let mut updates: Vec<(String, Vec<u8>)> = [
        ("a page from two clients", page()),
        ("a non-ASCII text", non_ascii_text()),
        ("a deleted item", with_a_deleted_item()),
        ("an empty doc", state(&Doc::new())),
        ("collected and skipped ranges", collected_and_skipped()),
        ("an empty item", with_an_empty_item()),
        ("an item up to u32::MAX", to_the_end()),
    ]
    .into_iter()
    .map(|(what, update)| (what.to_string(), update))
    .collect();
    updates.extend(the_engines_updates());
    updates.extend(the_conformance_updates());
    let (base, attack) = crafted_attack();
    updates.push(("the crafted base page".to_string(), base));
    updates.push(("the crafted attack".to_string(), attack));
    common::assert_read_as_yrs_reads(updates);
}
