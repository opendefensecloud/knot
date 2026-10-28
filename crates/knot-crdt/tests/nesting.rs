//! Updates that nest a page's types deeper than a room accepts
//! (`knot_crdt::nesting`, F1 review). yrs deletes and garbage-collects a
//! nested type by recursing once per level, so a crafted update holding a
//! subtree 100,000 types deep, and its deletion, overflows a tokio worker's
//! stack while it is applied, and a stack overflow aborts the whole server.
//! A room runs a best-effort prefilter before applying an inbound update
//! (nothing of a refused one is applied), and the authoritative check after
//! applying it; an update refused after applying it, or one yrs rejects
//! part-way, is not persisted or fanned out, and the room evicts itself.
//! These tests are the evidence for the shapes they build; an update whose
//! own apply overflows in a shape the prefilter misses is not guarded (see
//! `knot_crdt::nesting`).
//!
//! The checks run on a thread with the stack of a tokio worker, 2 MiB, and
//! the room on a multi-threaded runtime, whose workers have that stack. This
//! file is a test binary of its own, so that an overflow can abort only
//! these tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use knot_crdt::nesting::{
    MAX_NESTING, TooDeep, check_update, deepest_inserted_type_depth, inserted_type_ids,
    nests_deeper_than,
};
use knot_crdt::protocol::wrap_sync_update;
use knot_crdt::{
    ConnHandle, DocHandle, Engine, Event, InMsg, MemBus, Room, RoomHandle, Rooms, SnapshotPolicy,
    YrsEngine,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use yrs::block::{ClientID, HAS_ORIGIN, HAS_RIGHT_ORIGIN, ItemContent};
use yrs::branch::{Branch, BranchID};
use yrs::encoding::write::Write;
use yrs::types::TypeRef;
use yrs::updates::encoder::{Encoder, EncoderV1};
use yrs::{
    Assoc, Doc, ID, IndexedSequence, Options, ReadTxn, StateVector, Text, Transact,
    XmlElementPrelim, XmlElementRef, XmlFragment, XmlOut, XmlTextPrelim,
};

/// How deep the crafted updates nest: far past what a walk that recursed
/// once per level could survive on [`STACK`].
const DEEP: usize = 100_000;

/// A tokio worker's stack.
const STACK: usize = 2 * 1024 * 1024;

/// What `f` returns, run on a thread with a [`STACK`]-sized stack.
fn on_worker_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("the check panicked")
}

/// What `f` returns, run on a thread whose stack holds any recursion of
/// yrs, to build the crafted updates.
fn on_big_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("building the update panicked")
}

/// `depth` blockquotes, one in the other, in `parent`; returns the
/// innermost.
fn chain<P: XmlFragment>(parent: &P, txn: &mut yrs::TransactionMut, depth: usize) -> XmlElementRef {
    let mut quote = parent.push_back(txn, XmlElementPrelim::empty("blockquote"));
    for _ in 1..depth {
        quote = quote.push_back(txn, XmlElementPrelim::empty("blockquote"));
    }
    quote
}

/// Everything `doc` holds, as one update.
fn state(doc: &Doc) -> Vec<u8> {
    doc.transact()
        .encode_state_as_update_v1(&StateVector::default())
}

/// What `doc` gained since `sv`, as one update.
fn since(doc: &Doc, sv: &StateVector) -> Vec<u8> {
    doc.transact().encode_state_as_update_v1(sv)
}

/// A page of `depth` nested blockquotes, as one update.
fn chain_update(depth: usize) -> Vec<u8> {
    let doc = Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    chain(&frag, &mut doc.transact_mut(), depth);
    state(&doc)
}

/// [`DEEP`] nested blockquotes and the deletion of the outermost, as one
/// update: what a client that keeps deleted content (`skip_gc`) sends.
/// Applying it deletes the chain, which recursed once per level.
fn insert_and_delete_update() -> Vec<u8> {
    on_big_stack(|| {
        let doc = Doc::with_options(Options {
            skip_gc: true,
            ..Options::default()
        });
        let frag = doc.get_or_insert_xml_fragment("default");
        chain(&frag, &mut doc.transact_mut(), DEEP);
        frag.remove_range(&mut doc.transact_mut(), 0, 1);
        state(&doc)
    })
}

/// A page holding what `updates` hold, applied in order.
fn page(updates: &[&[u8]]) -> DocHandle {
    let doc = YrsEngine.new_doc();
    for update in updates {
        YrsEngine.apply_update(&doc, update).expect("apply");
    }
    doc
}

/// `check_update` of `update` against the page `base` builds, on a worker's
/// stack.
fn check(base: Vec<Vec<u8>>, update: Vec<u8>) -> Result<(), TooDeep> {
    on_worker_stack(move || {
        let refs: Vec<&[u8]> = base.iter().map(Vec::as_slice).collect();
        check_update(&page(&refs), &update)
    })
}

#[test]
fn a_deeply_nested_update_is_refused() {
    assert!(check(vec![], chain_update(DEEP)).is_err());
}

#[test]
fn an_update_that_inserts_and_deletes_a_deep_subtree_is_refused() {
    assert!(check(vec![], insert_and_delete_update()).is_err());
}

/// The limit is the depth of the deepest type: a root's child is at 1.
#[test]
fn types_nest_up_to_the_limit() {
    assert_eq!(check(vec![], chain_update(MAX_NESTING)), Ok(()));
    assert_eq!(
        check(vec![], chain_update(MAX_NESTING + 1)),
        Err(TooDeep {
            depth: MAX_NESTING + 1
        })
    );
}

/// Updates that nest a little each: the check measures where the new types
/// hang in the page, below its parent or next to a sibling.
#[test]
fn updates_that_nest_a_page_deeper_step_by_step_are_refused_past_the_limit() {
    let half = MAX_NESTING / 2;
    let doc = Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    let innermost = chain(&frag, &mut doc.transact_mut(), half);
    let first = state(&doc);

    // Below the innermost quote: up to the limit, then one past it.
    let sv = doc.transact().state_vector();
    let below = chain(&innermost, &mut doc.transact_mut(), half);
    let fits = since(&doc, &sv);
    assert_eq!(check(vec![first.clone()], fits.clone()), Ok(()));
    let sv = doc.transact().state_vector();
    chain(&below, &mut doc.transact_mut(), 1);
    let past = since(&doc, &sv);
    assert_eq!(
        check(vec![first.clone(), fits.clone()], past),
        Err(TooDeep {
            depth: MAX_NESTING + 1
        })
    );

    // Next to the innermost quote's child, a sibling found by its origin.
    let sv = doc.transact().state_vector();
    {
        let mut txn = doc.transact_mut();
        let sibling = innermost.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        chain(&sibling, &mut txn, half);
    }
    let beside = since(&doc, &sv);
    assert_eq!(
        check(vec![first, fits], beside),
        Err(TooDeep {
            depth: MAX_NESTING + 1
        })
    );
}

/// An update whose parent hasn't arrived yet is held back by yrs. Its types
/// count from depth 0 until then, and once the update it waits for comes,
/// that one is checked with it.
#[test]
fn an_update_waiting_for_its_parent_is_checked_with_it() {
    let doc = Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    let outer = frag.push_back(
        &mut doc.transact_mut(),
        XmlElementPrelim::empty("blockquote"),
    );
    let parent = state(&doc);
    let sv = doc.transact().state_vector();
    chain(&outer, &mut doc.transact_mut(), MAX_NESTING);
    let waiting = since(&doc, &sv);

    // Alone, the waiting update is within the limit: it counts from 0.
    assert_eq!(check(vec![], waiting.clone()), Ok(()));
    // Once it waits in the page, the parent it waits for puts it past.
    assert_eq!(
        check(vec![waiting.clone()], parent),
        Err(TooDeep {
            depth: MAX_NESTING + 1
        })
    );
    // Deeper than the limit on its own, it is refused even while it waits.
    let sv = doc.transact().state_vector();
    let innermost = chain(&outer, &mut doc.transact_mut(), 1);
    chain(&innermost, &mut doc.transact_mut(), MAX_NESTING);
    assert!(check(vec![], since(&doc, &sv)).is_err());
}

/// The edits the editor makes: text, paragraphs, lists and marks.
#[test]
fn everyday_edits_pass() {
    let doc = Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    let mut base = Vec::new();
    let mut sv = StateVector::default();
    let mut edit = |f: &dyn Fn(&mut yrs::TransactionMut)| {
        f(&mut doc.transact_mut());
        let update = since(&doc, &sv);
        assert_eq!(check(base.clone(), update.clone()), Ok(()));
        base.push(update);
        sv = doc.transact().state_vector();
    };
    edit(&|txn| {
        let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("hello"));
    });
    edit(&|txn| {
        let list = frag.push_back(txn, XmlElementPrelim::empty("bullet_list"));
        let item = list.push_back(txn, XmlElementPrelim::empty("list_item"));
        let p = item.push_back(txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(txn, XmlTextPrelim::new("item"));
    });
    edit(&|txn| {
        let Some(yrs::XmlOut::Element(p)) = frag.get(txn, 0) else {
            panic!("no paragraph");
        };
        let Some(yrs::XmlOut::Text(t)) = p.get(txn, 0) else {
            panic!("no text");
        };
        t.insert(txn, 5, " world");
        t.format(
            txn,
            0,
            5,
            yrs::types::Attrs::from([("bold".into(), true.into())]),
        );
    });
    edit(&|txn| {
        frag.remove_range(txn, 0, 1);
    });
}

/// Deleting a subtree twice as deep as the limit recurses within a worker's
/// stack, so a page within the limit can always be deleted.
#[test]
fn a_page_twice_as_deep_as_the_limit_deletes_on_a_worker_stack() {
    let update = chain_update(2 * MAX_NESTING);
    on_worker_stack(move || {
        let doc = page(&[&update]);
        let frag = doc.inner().get_or_insert_xml_fragment("default");
        frag.remove_range(&mut doc.inner().transact_mut(), 0, 1);
    });
}

#[test]
fn deeply_nested_pages_are_found() {
    let deep = chain_update(DEEP);
    let at_limit = chain_update(MAX_NESTING);
    let (deep, at_limit) = on_worker_stack(move || {
        (
            nests_deeper_than(&page(&[&deep]), MAX_NESTING),
            nests_deeper_than(&page(&[&at_limit]), MAX_NESTING),
        )
    });
    assert!(deep);
    assert!(!at_limit);
}

/// Text, attributes and roots other than "default" nest too.
#[test]
fn nesting_is_found_in_every_root_and_attribute() {
    let doc = Doc::new();
    let map = doc.get_or_insert_map("elsewhere");
    {
        let mut txn = doc.transact_mut();
        let mut inner = yrs::Map::insert(&map, &mut txn, "k", yrs::MapPrelim::default());
        for _ in 0..MAX_NESTING {
            inner = yrs::Map::insert(&inner, &mut txn, "k", yrs::MapPrelim::default());
        }
    }
    let update = state(&doc);
    assert!(nests_deeper_than(&page(&[&update]), MAX_NESTING));
    assert!(check(vec![], update).is_err());
}

// ---------------------------------------------------------------------------
// The room
// ---------------------------------------------------------------------------

/// Records every update the writer persists, in order.
struct CapturingUpdates {
    persisted: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl knot_storage::UpdatesStore for CapturingUpdates {
    async fn insert_batch(
        &self,
        _: Uuid,
        updates: &[(Option<Uuid>, Vec<u8>)],
    ) -> Result<Vec<i64>, knot_storage::UpdatesStoreError> {
        let mut p = self.persisted.lock().unwrap();
        let first = p.len() as i64 + 1;
        p.extend(updates.iter().map(|(_, b)| b.clone()));
        Ok((first..first + updates.len() as i64).collect())
    }
    async fn since(
        &self,
        _: Uuid,
        _: i64,
    ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
        Ok(vec![])
    }
    async fn max_seq(&self, _: Uuid) -> Result<i64, knot_storage::UpdatesStoreError> {
        Ok(0)
    }
    async fn delete_up_to(&self, _: Uuid, _: i64) -> Result<u64, knot_storage::UpdatesStoreError> {
        Ok(0)
    }
}

/// Records the state of every snapshot a room writes, and serves none.
#[derive(Default)]
struct RecordingSnapshots {
    written: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl knot_storage::SnapshotStore for RecordingSnapshots {
    async fn insert(
        &self,
        _: Uuid,
        _: i64,
        state: &[u8],
        _: &[u8],
    ) -> Result<(), knot_storage::SnapshotStoreError> {
        self.written.lock().unwrap().push(state.to_vec());
        Ok(())
    }
    async fn latest(
        &self,
        _: Uuid,
    ) -> Result<Option<knot_storage::DocSnapshot>, knot_storage::SnapshotStoreError> {
        Ok(None)
    }
    async fn gc(&self, _: Uuid, _: i64, _: i32) -> Result<u64, knot_storage::SnapshotStoreError> {
        Ok(0)
    }
    async fn list(
        &self,
        _: Uuid,
        _: i64,
    ) -> Result<Vec<knot_storage::SnapshotMeta>, knot_storage::SnapshotStoreError> {
        Ok(vec![])
    }
    async fn by_seq(
        &self,
        _: Uuid,
        _: i64,
    ) -> Result<Option<knot_storage::DocSnapshot>, knot_storage::SnapshotStoreError> {
        Ok(None)
    }
}

/// A room whose writer records what it persists.
async fn room() -> (RoomHandle, Arc<Mutex<Vec<Vec<u8>>>>) {
    let (room, persisted, _) = room_and_snapshots().await;
    (room, persisted)
}

/// A room whose writer records what it persists, and the snapshots it
/// writes. Its policy writes none while it runs, so any snapshot is its
/// final flush.
async fn room_and_snapshots() -> (
    RoomHandle,
    Arc<Mutex<Vec<Vec<u8>>>>,
    Arc<Mutex<Vec<Vec<u8>>>>,
) {
    let bus = Arc::new(MemBus::new());
    let doc_id = Uuid::new_v4();
    let sub = knot_crdt::Bus::subscribe(bus.as_ref(), doc_id)
        .await
        .unwrap();
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let snapshots = RecordingSnapshots::default();
    let written = snapshots.written.clone();
    let room = Room::spawn(
        doc_id,
        Arc::new(YrsEngine),
        bus,
        sub,
        Arc::new(CapturingUpdates {
            persisted: persisted.clone(),
        }),
        Arc::new(snapshots),
        SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(3600),
        },
        None,
    )
    .await
    .unwrap();
    (room, persisted, written)
}

/// A connection joined to `room`: its id, what the room sends it, and the
/// signal the room raises when it refuses its update.
async fn join(room: &RoomHandle) -> (Uuid, mpsc::Receiver<Vec<u8>>, CancellationToken) {
    let conn_id = Uuid::new_v4();
    let (tx, rx) = mpsc::channel(64);
    let handle = ConnHandle::new(tx);
    let refused = handle.refused.clone();
    let (reply, joined) = oneshot::channel();
    room.tx
        .send(Event::Join {
            conn_id,
            handle,
            reply,
        })
        .await
        .unwrap();
    joined.await.unwrap().unwrap();
    (conn_id, rx, refused)
}

async fn send(room: &RoomHandle, from: Uuid, bytes: Vec<u8>) {
    room.tx
        .send(Event::Inbound(InMsg {
            from,
            by_user: None,
            bytes,
        }))
        .await
        .unwrap();
}

/// A one-paragraph edit, from a client of its own.
fn sentinel() -> Vec<u8> {
    let doc = Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    {
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("after"));
    }
    state(&doc)
}

/// Sends `deep` from one connection, then [`sentinel`] from another, and
/// checks that only the sentinel was persisted and fanned out, that the
/// sender was closed, and that the room still serves.
async fn refused_and_still_serving(deep: Vec<u8>) {
    let (room, persisted) = room().await;
    let (attacker, _attacker_rx, refused) = join(&room).await;
    let (writer, _writer_rx) = {
        let (id, rx, _) = join(&room).await;
        (id, rx)
    };
    let (_peer, mut peer_rx, _) = join(&room).await;
    // The join's state, and the server's SyncStep1, are sent by the shim,
    // not the room: the peer's channel holds only fan-out.
    send(&room, attacker, deep).await;
    let after = sentinel();
    send(&room, writer, after.clone()).await;

    let fanned = tokio::time::timeout(Duration::from_secs(10), peer_rx.recv())
        .await
        .expect("the room stopped fanning out")
        .expect("peer channel closed");
    assert_eq!(
        fanned,
        wrap_sync_update(&after),
        "the refused update was fanned out"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !persisted.lock().unwrap().contains(&after) {
        assert!(
            std::time::Instant::now() < deadline,
            "the room stopped persisting"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        *persisted.lock().unwrap(),
        vec![after],
        "the refused update was persisted"
    );
    assert!(refused.is_cancelled(), "the sender was not told to close");

    let (reply, state) = oneshot::channel();
    room.tx.send(Event::ExportState(reply)).await.unwrap();
    let (bytes, _) = state.await.unwrap().unwrap();
    assert!(
        !on_worker_stack(move || nests_deeper_than(&page(&[&bytes]), MAX_NESTING)),
        "the room applied the refused update"
    );
    room.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_room_refuses_a_deeply_nested_update() {
    let deep = on_big_stack(|| chain_update(DEEP));
    refused_and_still_serving(deep).await;
}

/// Applying this update overflowed the worker's stack, which aborted the
/// process.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_refuses_an_update_that_inserts_and_deletes_a_deep_subtree() {
    refused_and_still_serving(insert_and_delete_update()).await;
}

// ---------------------------------------------------------------------------
// The reviewer's crafted reparenting sequence (F1 round 2)
// ---------------------------------------------------------------------------
//
// `check_update` (the prefilter) predicts where yrs will place an update's
// items, so it can be bypassed: an update can hang a short chain of new types
// off existing, legal, deep-but-within-limit content so the chain's depth
// comes from the page, not from the update's own structure. The reviewer's
// probe does exactly that, and reached depth 10,501 over 20 rounds while every
// round passed `check_update`; a later delete then overflowed the stack. The
// authoritative post-apply check measures where the types actually landed and
// refuses the update after the fact.

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

/// The crafted base page (the full state update) and the crafted attack
/// update that reparents onto it. The base is [`CRAFTED_BASE_DEPTH`]
/// blockquotes deep plus a bottom paragraph (within the limit), and holds one
/// deleted paragraph that yrs garbage-collects — the item the attack's string
/// hangs under.
fn crafted_attack() -> (Vec<u8>, Vec<u8>) {
    on_big_stack(|| {
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
        let base = state(&doc);
        let attack = attack_bytes(ClientID::new(99), 0, &dead, &bottom);
        (base, attack)
    })
}

/// The authoritative post-apply check catches the crafted update that
/// `check_update` lets through. GREEN-only: it names new API.
#[test]
fn the_post_apply_check_catches_a_reparenting_update_the_prefilter_misses() {
    let (base, attack) = crafted_attack();
    on_worker_stack(move || {
        let doc = YrsEngine.new_doc();
        YrsEngine.apply_update(&doc, &base).expect("apply base");
        // The base page is within the limit.
        assert!(
            !nests_deeper_than(&doc, MAX_NESTING),
            "the base page should be within the limit"
        );
        // The prefilter is bypassed: it returns Ok for the crafted update.
        assert_eq!(
            check_update(&doc, &attack),
            Ok(()),
            "the crafted update should pass the prefilter"
        );
        // Applying it and measuring where the types landed catches it.
        let measured = YrsEngine
            .apply_update_measuring(&doc, &attack, MAX_NESTING)
            .expect("apply and measure");
        assert!(measured.changed);
        assert!(
            measured.max_depth > MAX_NESTING,
            "the post-apply check must catch the crafted update; got {}",
            measured.max_depth
        );
        // And the doc really did get nested past the limit.
        assert!(nests_deeper_than(&doc, MAX_NESTING));
        // Directly: the update's inserted types are measured past the limit.
        let ids = inserted_type_ids(&attack);
        let txn = doc.inner().transact();
        assert!(deepest_inserted_type_depth(&txn, &ids, MAX_NESTING) > MAX_NESTING);
    });
}

/// RED on HEAD: without the post-apply check the room applies, persists and
/// fans out the crafted update and never evicts, so this times out waiting
/// for the eviction. With the fix the room refuses it after the fact —
/// nothing of it is persisted or fanned out, the sender is closed, and the
/// room evicts itself — and the process keeps running throughout.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_refuses_and_evicts_a_crafted_reparenting_update() {
    let (base, attack) = crafted_attack();
    refused_after_apply_and_evicted(base, attack).await;
}

/// Sends `base` from one connection, then `bad` from another, and checks
/// that the room accepted `base` (fanned out and persisted) and refused
/// `bad` after applying it: nothing of `bad` is persisted or fanned out,
/// its sender is told to close, and the room evicts itself without a final
/// snapshot of its live doc, which holds what `bad` brought.
async fn refused_after_apply_and_evicted(base: Vec<u8>, bad: Vec<u8>) {
    let (room, persisted, snapshots) = room_and_snapshots().await;
    let (writer, _writer_rx, _) = join(&room).await;
    let (attacker, _attacker_rx, refused) = join(&room).await;
    let (_peer, mut peer_rx, _) = join(&room).await;

    // The base page (within the limit) is accepted, fanned out and persisted.
    send(&room, writer, base.clone()).await;
    let base_fanout = tokio::time::timeout(Duration::from_secs(10), peer_rx.recv())
        .await
        .expect("the room never fanned out the base")
        .expect("peer channel closed");
    assert_eq!(base_fanout, wrap_sync_update(&base));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while persisted.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the base was never persisted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // `bad` passes the prefilter; the room refuses it after applying it and
    // evicts itself.
    send(&room, attacker, bad).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !room.shutdown.is_cancelled() {
        assert!(
            std::time::Instant::now() < deadline,
            "the room did not evict itself after refusing the update"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The actor drops its channel once it has ended, final flush included.
    tokio::time::timeout(Duration::from_secs(10), room.tx.closed())
        .await
        .expect("the evicted room did not stop");
    assert!(refused.is_cancelled(), "the sender was not told to close");
    assert_eq!(
        *persisted.lock().unwrap(),
        vec![base],
        "the refused update must not be persisted"
    );
    assert!(
        snapshots.lock().unwrap().is_empty(),
        "the evicted room snapshotted a doc holding the refused update"
    );
    assert!(
        matches!(
            peer_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty) | Err(mpsc::error::TryRecvError::Disconnected)
        ),
        "the refused update must not be fanned out"
    );
}

/// After a room evicts itself over a too-deep update, the next acquire boots
/// a fresh room from storage (which never got the refused update), and that
/// room serves edits — the server keeps serving.
#[tokio::test(flavor = "multi_thread")]
async fn an_evicted_room_is_rebooted_on_the_next_acquire_and_keeps_serving() {
    let (base, attack) = crafted_attack();
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let rooms = Rooms::new(
        Arc::new(YrsEngine),
        Arc::new(MemBus::new()),
        Arc::new(CapturingUpdates {
            persisted: persisted.clone(),
        }),
        Arc::new(RecordingSnapshots::default()),
        SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(3600),
        },
        Duration::from_secs(300),
    );
    let doc_id = Uuid::new_v4();
    let first = rooms.acquire(doc_id).await.unwrap();

    let (writer, _wr, _) = join(&first).await;
    let (attacker, _ar, _refused) = join(&first).await;
    send(&first, writer, base).await;
    send(&first, attacker, attack).await;

    // The room evicts itself.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !first.shutdown.is_cancelled() {
        assert!(
            std::time::Instant::now() < deadline,
            "the room did not evict itself"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        rooms.get_active(doc_id).is_none(),
        "the evicted room must not be handed out as active"
    );

    // The next acquire boots a fresh, live room.
    let second = rooms.acquire(doc_id).await.unwrap();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "acquire must boot a new room after eviction"
    );
    assert!(!second.shutdown.is_cancelled(), "the rebooted room is live");

    // It reloaded from storage, which never got the refused update, and it
    // serves a new edit.
    let (editor, _editor_rx, _) = join(&second).await;
    let (_peer, mut peer_rx, _) = join(&second).await;
    let edit = sentinel();
    send(&second, editor, edit.clone()).await;
    let fanned = tokio::time::timeout(Duration::from_secs(10), peer_rx.recv())
        .await
        .expect("the rebooted room stopped serving")
        .expect("peer channel closed");
    assert_eq!(fanned, wrap_sync_update(&edit));

    let (reply, st) = oneshot::channel();
    second.tx.send(Event::ExportState(reply)).await.unwrap();
    let (bytes, _) = st.await.unwrap().unwrap();
    assert!(
        !on_worker_stack(move || nests_deeper_than(&page(&[&bytes]), MAX_NESTING)),
        "the rebooted room must hold nothing past the limit"
    );
    second.shutdown.cancel();
}

/// Dropping a doc 100,000 types deep does not recurse: yrs holds items flat
/// in the block store, and neither `Branch` nor `Item` has a custom `Drop`.
/// The room relies on this when it evicts itself (drops the deep doc) over a
/// refused update. The drop runs on a tokio worker's 2 MiB stack.
#[test]
fn dropping_a_hundred_thousand_deep_doc_does_not_recurse() {
    let deep = on_big_stack(|| chain_update(DEEP));
    let doc = page(&[&deep]);
    on_worker_stack(move || drop(doc));
}

/// A full-state SyncStep2 re-send of a page already nested past the limit (a
/// legacy page, the pre-existing limitation) must not be measured as a fresh
/// violation: nothing is newly integrated, so a client reconnecting to such a
/// page does not evict the room on every join. GREEN-only: names new API.
#[test]
fn a_resend_of_an_already_deep_page_is_not_measured_as_a_violation() {
    let deep = on_big_stack(|| chain_update(MAX_NESTING + 100));
    on_worker_stack(move || {
        let doc = YrsEngine.new_doc();
        // As if hydrated from storage (a legacy deep page): applied without
        // the measuring path, the way `Room::spawn` replays stored updates.
        YrsEngine.apply_update(&doc, &deep).expect("hydrate");
        assert!(
            nests_deeper_than(&doc, MAX_NESTING),
            "fixture: the hydrated page is over the limit"
        );
        // A client's full-state re-send integrates nothing new.
        let measured = YrsEngine
            .apply_update_measuring(&doc, &deep, MAX_NESTING)
            .expect("measure a re-send");
        assert!(!measured.changed, "a re-send integrates nothing new");
        assert_eq!(
            measured.max_depth, 0,
            "a re-send of a legacy deep page must not read as a fresh violation"
        );
    });
}

// ---------------------------------------------------------------------------
// The board room shares the same prefilter + post-apply path
// ---------------------------------------------------------------------------

/// A board store that records every update appended, to check nothing of a
/// refused update is stored.
struct CapturingBoard {
    appended: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl knot_storage::BoardStore for CapturingBoard {
    async fn create(
        &self,
        _: Uuid,
        _: Uuid,
        _: Option<String>,
    ) -> knot_storage::boards::Result<knot_storage::Board> {
        unimplemented!()
    }
    async fn get(&self, _: Uuid) -> knot_storage::boards::Result<knot_storage::Board> {
        Err(knot_storage::boards::BoardStoreError::NotFound)
    }
    async fn list_for_doc(
        &self,
        _: Uuid,
    ) -> knot_storage::boards::Result<Vec<knot_storage::Board>> {
        Ok(vec![])
    }
    async fn delete(&self, _: Uuid) -> knot_storage::boards::Result<()> {
        Ok(())
    }
    async fn latest_snapshot(
        &self,
        _: Uuid,
    ) -> knot_storage::boards::Result<Option<(i64, Vec<u8>)>> {
        Ok(None)
    }
    async fn put_snapshot(&self, _: Uuid, _: i64, _: &[u8]) -> knot_storage::boards::Result<()> {
        Ok(())
    }
    async fn load_updates(&self, _: Uuid) -> knot_storage::boards::Result<Vec<Vec<u8>>> {
        Ok(vec![])
    }
    async fn append_update(&self, _: Uuid, bytes: &[u8]) -> knot_storage::boards::Result<i64> {
        let mut a = self.appended.lock().unwrap();
        a.push(bytes.to_vec());
        Ok(a.len() as i64)
    }
    async fn since(&self, _: Uuid, _: i64) -> knot_storage::boards::Result<Vec<(i64, Vec<u8>)>> {
        Ok(vec![])
    }
    async fn max_update_seq(&self, _: Uuid) -> knot_storage::boards::Result<i64> {
        Ok(0)
    }
    async fn set_svg(&self, _: Uuid, _: &[u8]) -> knot_storage::boards::Result<()> {
        Ok(())
    }
    async fn get_svg(&self, _: Uuid) -> knot_storage::boards::Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// The board room runs the same prefilter and authoritative post-apply check.
/// The crafted update is refused after apply: nothing of it is appended or
/// fanned out, and the board room evicts itself (it has no close signal of
/// its own). RED on HEAD: the board would apply, append and fan it out.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_room_refuses_and_evicts_a_crafted_reparenting_update() {
    let (base, attack) = crafted_attack();
    board_refused_after_apply_and_evicted(base, attack).await;
}

/// As [`refused_after_apply_and_evicted`], for a board room: `base` is
/// appended and fanned out; `bad` is refused after it was applied, so
/// nothing of it is appended or fanned out, and the board room evicts
/// itself.
async fn board_refused_after_apply_and_evicted(base: Vec<u8>, bad: Vec<u8>) {
    use knot_crdt::BoardRoom;
    use knot_crdt::board_room::{Event as BoardEvent, InMsg as BoardInMsg};

    let board_id = Uuid::new_v4();
    let bus = Arc::new(MemBus::new());
    let sub = knot_crdt::Bus::subscribe(bus.as_ref(), board_id)
        .await
        .unwrap();
    let appended = Arc::new(Mutex::new(Vec::new()));
    let store: Arc<dyn knot_storage::BoardStore> = Arc::new(CapturingBoard {
        appended: appended.clone(),
    });
    let h = BoardRoom::spawn(board_id, Arc::new(YrsEngine), store, bus, sub)
        .await
        .unwrap();

    async fn join_board(h: &knot_crdt::BoardRoomHandle, tx: mpsc::Sender<Vec<u8>>) -> Uuid {
        let conn_id = Uuid::new_v4();
        let (reply, joined) = oneshot::channel();
        h.tx.send(knot_crdt::board_room::Event::Join {
            conn_id,
            handle: knot_crdt::board_room::ConnHandle { tx },
            reply,
        })
        .await
        .unwrap();
        joined.await.unwrap().unwrap();
        conn_id
    }

    let (wtx, _wrx) = mpsc::channel(64);
    let writer = join_board(&h, wtx).await;
    let (atx, _arx) = mpsc::channel(64);
    let attacker = join_board(&h, atx).await;
    let (ptx, mut peer_rx) = mpsc::channel(64);
    let _peer = join_board(&h, ptx).await;

    h.tx.send(BoardEvent::Inbound(BoardInMsg {
        from: writer,
        bytes: base.clone(),
    }))
    .await
    .unwrap();
    let base_fanout = tokio::time::timeout(Duration::from_secs(10), peer_rx.recv())
        .await
        .expect("the board never fanned out the base")
        .expect("peer channel closed");
    assert_eq!(base_fanout, wrap_sync_update(&base));

    h.tx.send(BoardEvent::Inbound(BoardInMsg {
        from: attacker,
        bytes: bad,
    }))
    .await
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !h.shutdown.is_cancelled() {
        assert!(
            std::time::Instant::now() < deadline,
            "the board room did not evict itself after refusing the update"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        *appended.lock().unwrap(),
        vec![base],
        "the refused update must not be appended"
    );
    assert!(
        matches!(
            peer_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty) | Err(mpsc::error::TryRecvError::Disconnected)
        ),
        "the refused update must not be fanned out"
    );
}

// ---------------------------------------------------------------------------
// An update yrs rejects part-way (F1 round 3)
// ---------------------------------------------------------------------------
//
// yrs integrates an update's blocks one at a time and does not roll back
// when a later one fails, so an update it rejects leaves the blocks before
// the failing one in the live doc, unmeasured. A room treats an apply error
// like a too-deep update: nothing of it is persisted or fanned out, the
// sender is closed, and the room evicts itself, so the next join reloads the
// page from storage.

/// A page holding one paragraph of text, from client 1, and the id of the
/// text's first character: an item that holds a string, not a type.
fn text_page() -> (Vec<u8>, ID) {
    let doc = Doc::with_client_id(1);
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
    (state(&doc), first)
}

/// An ordinary inconsistent update, which yrs integrates part of and then
/// rejects (`UpdateError::InvalidParent`): from client 77, an element
/// `partial` in the page's root fragment, then a string whose parent is
/// `not_a_type`, an item that holds text. yrs's own encoder never writes
/// such an update; a buggy or crafted client can. Encoded by hand, as
/// [`attack_bytes`] is.
fn rejected_part_way(not_a_type: &ID) -> Vec<u8> {
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

/// Whether the root fragment of `doc` holds an element tagged `partial`.
fn holds_partial(doc: &DocHandle) -> bool {
    let frag = doc.inner().get_or_insert_xml_fragment("default");
    let txn = doc.inner().transact();
    frag.children(&txn)
        .any(|child| matches!(child, XmlOut::Element(e) if e.tag().as_ref() == "partial"))
}

/// The fixture: the update passes the prefilter, so it reaches apply, and
/// yrs rejects it there after integrating its first block, which stays in
/// the doc although apply returned an error.
#[test]
fn yrs_rejects_the_update_part_way_and_keeps_what_it_integrated() {
    let (base, not_a_type) = text_page();
    let bad = rejected_part_way(&not_a_type);
    let doc = page(&[&base]);
    assert_eq!(
        check_update(&doc, &bad),
        Ok(()),
        "the update should pass the prefilter"
    );
    assert!(
        YrsEngine
            .apply_update_measuring(&doc, &bad, MAX_NESTING)
            .is_err(),
        "yrs should reject the update"
    );
    assert!(
        holds_partial(&doc),
        "yrs should keep the block it integrated before the error"
    );
}

/// RED on HEAD: the room logged the apply error and kept running, with the
/// partly applied update in its live doc, where a later snapshot (its final
/// flush, at the latest) would store it. Now it is refused like a too-deep
/// update: not persisted, fanned out or snapshotted, the sender is closed,
/// and the room evicts itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_refuses_and_evicts_an_update_yrs_rejects_part_way() {
    let (base, not_a_type) = text_page();
    refused_after_apply_and_evicted(base, rejected_part_way(&not_a_type)).await;
}

/// The same for a board room. RED on HEAD: the board room logged the error
/// and kept running.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_room_refuses_and_evicts_an_update_yrs_rejects_part_way() {
    let (base, not_a_type) = text_page();
    board_refused_after_apply_and_evicted(base, rejected_part_way(&not_a_type)).await;
}
