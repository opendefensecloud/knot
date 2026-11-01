//! Updates that nest a page's types deeper than a room accepts
//! (`knot_crdt::nesting`, F1 review). yrs deletes and garbage-collects a
//! nested type by recursing once per level, so a crafted update holding a
//! subtree 100,000 types deep, and its deletion, overflows a tokio worker's
//! stack while it is applied, and a stack overflow aborts the whole server.
//! A room runs a best-effort prefilter before applying an inbound update
//! (nothing of a refused one is applied), and the authoritative check after
//! applying it; an update refused after applying it is not persisted or
//! fanned out, and the room evicts itself. The rules for an update that
//! doesn't decode, or that yrs rejects part-way, are at the end (F6).
//! These tests are the evidence for the shapes they build; an update whose
//! own apply overflows in a shape the prefilter misses is not guarded (see
//! `knot_crdt::nesting`).
//!
//! The checks run on a thread with the stack of a tokio worker, 2 MiB, and
//! the room on a multi-threaded runtime, whose workers have that stack. This
//! file is a test binary of its own, so that an overflow can abort only
//! these tests.

mod common;

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
use knot_test_support::crafted::crafted_attack;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use yrs::block::{ClientID, ItemContent};
use yrs::branch::Branch;
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
    assert!(!room.shutdown.is_cancelled(), "the room evicted itself");

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
// refuses the update after the fact. `crafted_attack` builds the probe's base
// page and update (`knot_test_support::crafted`, shared with knot-server's
// socket tests).

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
        let ids = inserted_type_ids(&attack).expect("the crafted update decodes");
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

/// Board updates appended in order and served back, as Postgres does: a
/// test reads what a board room stored, and a board room can be reloaded
/// from what an earlier one stored.
#[derive(Clone, Default)]
struct StoredBoard(Arc<Mutex<Vec<Vec<u8>>>>);

impl StoredBoard {
    fn rows(&self) -> Vec<Vec<u8>> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl knot_storage::BoardStore for StoredBoard {
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
        Ok(self.rows())
    }
    async fn append_update(&self, _: Uuid, bytes: &[u8]) -> knot_storage::boards::Result<i64> {
        let mut rows = self.0.lock().unwrap();
        rows.push(bytes.to_vec());
        Ok(rows.len() as i64)
    }
    async fn since(
        &self,
        _: Uuid,
        after: i64,
    ) -> knot_storage::boards::Result<Vec<(i64, Vec<u8>)>> {
        Ok(self
            .rows()
            .into_iter()
            .zip(1..)
            .filter(|&(_, seq)| seq > after)
            .map(|(row, seq)| (seq, row))
            .collect())
    }
    async fn max_update_seq(&self, _: Uuid) -> knot_storage::boards::Result<i64> {
        Ok(self.rows().len() as i64)
    }
    async fn set_svg(&self, _: Uuid, _: &[u8]) -> knot_storage::boards::Result<()> {
        Ok(())
    }
    async fn get_svg(&self, _: Uuid) -> knot_storage::boards::Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

/// A board room for `board_id` loaded from `store`, which it appends to.
async fn board_on(board_id: Uuid, store: &StoredBoard) -> knot_crdt::BoardRoomHandle {
    let bus = Arc::new(MemBus::new());
    let sub = knot_crdt::Bus::subscribe(bus.as_ref(), board_id)
        .await
        .unwrap();
    knot_crdt::BoardRoom::spawn(
        board_id,
        Uuid::new_v4(),
        Arc::new(YrsEngine),
        Arc::new(store.clone()),
        bus,
        sub,
    )
    .await
    .unwrap()
}

/// The board room runs the same prefilter and authoritative post-apply check.
/// The crafted update is refused after apply: nothing of it is appended or
/// fanned out, its sender is told to close, and the board room evicts
/// itself. RED on HEAD: the board would apply, append and fan it out.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_room_refuses_and_evicts_a_crafted_reparenting_update() {
    let (base, attack) = crafted_attack();
    board_refused_after_apply_and_evicted(base, attack).await;
}

/// As [`refused_after_apply_and_evicted`], for a board room: `base` is
/// appended and fanned out; `bad` is refused after it was applied, so
/// nothing of it is appended or fanned out, its sender is told to close,
/// and the board room evicts itself.
async fn board_refused_after_apply_and_evicted(base: Vec<u8>, bad: Vec<u8>) {
    let store = StoredBoard::default();
    let h = board_on(Uuid::new_v4(), &store).await;
    let (writer, _writer_rx, _) = join_board(&h).await;
    let (attacker, _attacker_rx, refused) = join_board(&h).await;
    let (_peer, mut peer_rx, _) = join_board(&h).await;

    send_board(&h, writer, base.clone()).await;
    let base_fanout = tokio::time::timeout(Duration::from_secs(10), peer_rx.recv())
        .await
        .expect("the board never fanned out the base")
        .expect("peer channel closed");
    assert_eq!(base_fanout, wrap_sync_update(&base));

    send_board(&h, attacker, bad).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !h.shutdown.is_cancelled() {
        assert!(
            std::time::Instant::now() < deadline,
            "the board room did not evict itself after refusing the update"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused.is_cancelled(), "the sender was not told to close");
    assert_eq!(
        store.rows(),
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
// What a room refuses, and from whom (F6)
// ---------------------------------------------------------------------------
//
// - An update that doesn't decode: nothing of it is applied. The room
//   refuses its sender (4422) and keeps running; no one else notices.
// - An update yrs rejects part-way: yrs integrates an update's blocks one at
//   a time and doesn't roll back when a later one fails. Within the same
//   apply it also retries the blocks it parked earlier for a missing
//   dependency, and passes on their error, so the failure can come from
//   another client's parked block rather than from the sender. What the
//   apply integrated is measured as for any update. Past the limit, the room
//   refuses the update and evicts itself, as for one it applies in full.
//   Within the limit, it does what main does: it neither persists nor fans
//   out the update, and keeps the sender and its other connections.

/// A page holding one paragraph of text, from `client`, and the id of the
/// text's first character: an item that holds a string, not a type.
fn text_page(client: u64) -> (Vec<u8>, ID) {
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
    (state(&doc), first)
}

/// An ordinary inconsistent update, which yrs integrates part of and then
/// rejects (`UpdateError::InvalidParent`): from client 77, an element
/// `partial` in the page's root fragment, then a string whose parent is
/// `not_a_type`, an item that holds text. yrs's own encoder never writes
/// such an update; a buggy or crafted client can. Encoded by hand, as
/// `knot_test_support::crafted` encodes the crafted attack. While the doc
/// lacks `not_a_type`, yrs integrates `partial` and parks the string until
/// that item arrives.
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
    let (base, not_a_type) = text_page(1);
    let bad = rejected_part_way(&not_a_type);
    let doc = page(&[&base]);
    assert_eq!(
        check_update(&doc, &bad),
        Ok(()),
        "the update should pass the prefilter"
    );
    let measured = YrsEngine
        .apply_update_measuring(&doc, &bad, MAX_NESTING)
        .expect("the update decodes");
    assert!(measured.rejected.is_some(), "yrs should reject the update");
    assert!(
        holds_partial(&doc),
        "yrs should keep the block it integrated before the error"
    );
    assert!(measured.changed, "what yrs integrated counts as a change");
    assert_eq!(measured.max_depth, 1, "`partial` is measured");
}

/// Three bytes that are no update at all, and one whose first item's
/// clocks run past `u32::MAX`, which the room's decoder used to add up with
/// an overflow (`knot_crdt::update_items`): updates that don't decode.
fn undecodable() -> [Vec<u8>; 2] {
    let mut enc = EncoderV1::new();
    enc.write_var(1u32);
    enc.write_var(1u32);
    enc.write_client(ClientID::new(7));
    enc.write_var(u32::MAX - 1);
    enc.write_info(4);
    enc.write_parent_info(true);
    enc.write_string("default");
    ItemContent::String("abc".into()).encode(&mut enc);
    enc.write_var(0u32);
    [vec![0xff, 0xff, 0xff], enc.to_vec()]
}

/// RED on HEAD: the room refused an update that doesn't decode like one it
/// had applied, and evicted itself, closing every other connection too
/// (the clock past `u32::MAX` panicked its actor in a debug build instead).
/// Nothing of such an update is applied, so only its sender is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_refuses_only_the_sender_of_an_update_that_does_not_decode() {
    for bad in undecodable() {
        refused_and_still_serving(bad).await;
    }
}

/// Persists updates in order and serves them back, as Postgres does, so a
/// room can be reloaded from what an earlier one stored.
#[derive(Clone, Default)]
struct Stored(Arc<Mutex<Vec<Vec<u8>>>>);

impl Stored {
    fn rows(&self) -> Vec<Vec<u8>> {
        self.0.lock().unwrap().clone()
    }

    /// Waits until `update` is stored.
    async fn holds(&self, update: &[u8]) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !self.rows().iter().any(|row| row == update) {
            assert!(
                std::time::Instant::now() < deadline,
                "the update was never stored"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[async_trait::async_trait]
impl knot_storage::UpdatesStore for Stored {
    async fn insert_batch(
        &self,
        _: Uuid,
        updates: &[(Option<Uuid>, Vec<u8>)],
    ) -> Result<Vec<i64>, knot_storage::UpdatesStoreError> {
        let mut rows = self.0.lock().unwrap();
        let first = rows.len() as i64 + 1;
        rows.extend(updates.iter().map(|(_, b)| b.clone()));
        Ok((first..first + updates.len() as i64).collect())
    }
    async fn since(
        &self,
        doc_id: Uuid,
        after: i64,
    ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
        Ok(self
            .rows()
            .into_iter()
            .zip(1..)
            .filter(|&(_, seq)| seq > after)
            .map(|(update_bytes, seq)| knot_storage::DocUpdate {
                seq,
                doc_id,
                update_bytes,
                by_user_id: None,
                created_at: sqlx::types::chrono::Utc::now(),
            })
            .collect())
    }
    async fn max_seq(&self, _: Uuid) -> Result<i64, knot_storage::UpdatesStoreError> {
        Ok(self.rows().len() as i64)
    }
    async fn delete_up_to(&self, _: Uuid, _: i64) -> Result<u64, knot_storage::UpdatesStoreError> {
        Ok(0)
    }
}

/// A room for `doc_id` loaded from `store`, which it persists to.
async fn room_on(doc_id: Uuid, store: &Stored) -> RoomHandle {
    let bus = Arc::new(MemBus::new());
    let sub = knot_crdt::Bus::subscribe(bus.as_ref(), doc_id)
        .await
        .unwrap();
    Room::spawn(
        doc_id,
        Arc::new(YrsEngine),
        bus,
        sub,
        Arc::new(store.clone()),
        Arc::new(RecordingSnapshots::default()),
        SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(3600),
        },
        None,
    )
    .await
    .unwrap()
}

/// Waits until `rx` gets `update` fanned out, and returns the frames it got
/// before (the room's catch-up may fan an earlier update out again).
async fn fanned_out(rx: &mut mpsc::Receiver<Vec<u8>>, update: &[u8]) -> Vec<Vec<u8>> {
    let want = wrap_sync_update(update);
    let mut before = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the update was never fanned out")
            .expect("the channel closed");
        if frame == want {
            return before;
        }
        before.push(frame);
    }
}

/// A crafted update whose string yrs parks, as it waits for client 5's
/// text, and the innocent edit that brings that text: the edit's apply
/// integrates the edit, then retries the parked string, which fails, since
/// text is no type.
fn parked_block_and_the_edit_it_fails_on() -> (Vec<u8>, Vec<u8>) {
    let (edit, text) = text_page(5);
    (rejected_part_way(&text), edit)
}

/// The fixture: the crafted update applies, its string parked; the edit's
/// apply integrates the edit, then fails on the parked string. GREEN-only:
/// it names new API.
#[test]
fn a_parked_block_fails_the_apply_of_the_edit_it_waits_for() {
    let (parked, edit) = parked_block_and_the_edit_it_fails_on();
    let doc = YrsEngine.new_doc();
    let measured = YrsEngine
        .apply_update_measuring(&doc, &parked, MAX_NESTING)
        .expect("the crafted update decodes");
    assert_eq!(
        measured.rejected, None,
        "the string is parked, not rejected"
    );
    assert!(measured.changed);
    let measured = YrsEngine
        .apply_update_measuring(&doc, &edit, MAX_NESTING)
        .expect("the edit decodes");
    assert!(
        measured.rejected.is_some(),
        "the parked string should fail on the edit"
    );
    // The edit's paragraph (depth 1) and text (depth 2) were integrated.
    assert!(measured.changed);
    assert_eq!(measured.max_depth, 2);
}

/// RED on HEAD: the room refused the innocent sender (4422) and evicted
/// itself, and every reload parked the block again from storage, so the
/// sender's re-sent edit was refused again: locked out for good, while the
/// page restarted for everyone. The failure is the parked block's, within
/// the limit, so the room now does what main does.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_keeps_the_sender_whose_update_a_parked_block_fails_on() {
    let doc_id = Uuid::new_v4();
    let store = Stored::default();
    let (parked, edit) = parked_block_and_the_edit_it_fails_on();

    let room = room_on(doc_id, &store).await;
    let (crafted, _crafted_rx, _) = join(&room).await;
    let (innocent, _innocent_rx, refused) = join(&room).await;
    let (_peer, mut peer_rx, peer_refused) = join(&room).await;
    // yrs integrates `partial` and parks the string: the update applies, so
    // it is persisted and fanned out.
    send(&room, crafted, parked.clone()).await;
    fanned_out(&mut peer_rx, &parked).await;
    send(&room, innocent, edit.clone()).await;
    let after = sentinel();
    send(&room, innocent, after.clone()).await;

    let before = fanned_out(&mut peer_rx, &after).await;
    assert!(
        !before.contains(&wrap_sync_update(&edit)),
        "the update yrs rejected was fanned out"
    );
    store.holds(&after).await;
    assert_eq!(
        store.rows(),
        vec![parked, after],
        "the update yrs rejected was persisted"
    );
    assert!(!refused.is_cancelled(), "the innocent sender was refused");
    assert!(!peer_refused.is_cancelled(), "a peer was refused");
    assert!(!room.shutdown.is_cancelled(), "the room evicted itself");

    // A reload parks the block again from storage, and the innocent client
    // re-sends its edit in its answer to the server's SyncStep1.
    room.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), room.tx.closed())
        .await
        .expect("the room did not stop");
    let room = room_on(doc_id, &store).await;
    let (innocent, _innocent_rx, refused) = join(&room).await;
    let (_peer, mut peer_rx, _) = join(&room).await;
    send(&room, innocent, edit).await;
    let after = sentinel();
    send(&room, innocent, after.clone()).await;
    fanned_out(&mut peer_rx, &after).await;
    assert!(
        !refused.is_cancelled(),
        "the innocent sender was locked out after a reload"
    );
    assert!(!room.shutdown.is_cancelled(), "the room evicted itself");
    room.shutdown.cancel();
}

/// The crafted base page plus a paragraph of text from client 2, and an
/// update that nests the page past the limit (the crafted attack) and then
/// fails on that text (client 77's blocks integrate after client 99's).
fn nests_too_deep_then_rejected() -> (Vec<u8>, Vec<u8>) {
    let (base, attack) = crafted_attack();
    let (text, not_a_type) = text_page(2);
    let base = yrs::merge_updates_v1([base, text]).expect("merge the base");
    let bad =
        yrs::merge_updates_v1([attack, rejected_part_way(&not_a_type)]).expect("merge the update");
    (base, bad)
}

/// The fixture: the update passes the prefilter, nests the page past the
/// limit, and then yrs rejects the rest of it. GREEN-only: it names new
/// API.
#[test]
fn an_update_can_nest_too_deep_before_yrs_rejects_the_rest() {
    let (base, bad) = nests_too_deep_then_rejected();
    on_worker_stack(move || {
        let doc = page(&[&base]);
        assert_eq!(
            check_update(&doc, &bad),
            Ok(()),
            "the update should pass the prefilter"
        );
        let measured = YrsEngine
            .apply_update_measuring(&doc, &bad, MAX_NESTING)
            .expect("the update decodes");
        assert!(measured.rejected.is_some(), "yrs should reject the update");
        assert!(
            measured.max_depth > MAX_NESTING,
            "what yrs integrated should be measured past the limit; got {}",
            measured.max_depth
        );
    });
}

/// An update yrs rejects part-way after it nested the page past the limit
/// is refused, and the room evicts itself, as for one it applies in full.
/// Passes on HEAD too, where every apply error evicted.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_evicts_itself_over_an_update_that_nests_too_deep_before_it_fails() {
    let (base, bad) = nests_too_deep_then_rejected();
    refused_after_apply_and_evicted(base, bad).await;
}

// The board room follows the same rules.

/// A connection joined to the board room `h`: its id, what the room sends
/// it, and the signal the room raises when it refuses its update.
async fn join_board(
    h: &knot_crdt::BoardRoomHandle,
) -> (Uuid, mpsc::Receiver<Vec<u8>>, CancellationToken) {
    let conn_id = Uuid::new_v4();
    let (tx, rx) = mpsc::channel(64);
    let handle = knot_crdt::board_room::ConnHandle::new(tx);
    let refused = handle.refused.clone();
    let (reply, joined) = oneshot::channel();
    h.tx.send(knot_crdt::board_room::Event::Join {
        conn_id,
        handle,
        reply,
    })
    .await
    .unwrap();
    joined.await.unwrap().unwrap();
    (conn_id, rx, refused)
}

async fn send_board(h: &knot_crdt::BoardRoomHandle, from: Uuid, bytes: Vec<u8>) {
    h.tx.send(knot_crdt::board_room::Event::Inbound(
        knot_crdt::board_room::InMsg { from, bytes },
    ))
    .await
    .unwrap();
}

/// RED on HEAD: the board room evicted itself over an update that doesn't
/// decode, closing every connection. Now only its sender is refused (4422),
/// nothing of the update is stored or fanned out, and the board room keeps
/// serving.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_room_refuses_only_the_sender_of_an_update_that_does_not_decode() {
    for bad in undecodable() {
        let board_id = Uuid::new_v4();
        let store = StoredBoard::default();
        let h = board_on(board_id, &store).await;
        let (sender, _sender_rx, refused) = join_board(&h).await;
        let (writer, _writer_rx, writer_refused) = join_board(&h).await;
        let (_peer, mut peer_rx, peer_refused) = join_board(&h).await;
        send_board(&h, sender, bad).await;
        let after = sentinel();
        send_board(&h, writer, after.clone()).await;

        let before = fanned_out(&mut peer_rx, &after).await;
        assert!(before.is_empty(), "the refused update was fanned out");
        assert_eq!(store.rows(), vec![after], "the refused update was stored");
        assert!(refused.is_cancelled(), "the sender was not told to close");
        assert!(!writer_refused.is_cancelled(), "a writer was refused");
        assert!(!peer_refused.is_cancelled(), "a peer was refused");
        assert!(!h.shutdown.is_cancelled(), "the board room evicted itself");
        h.shutdown.cancel();
    }
}

/// RED on HEAD: the board room evicted itself over the innocent sender's
/// update, and every reload parked the block again. Now it keeps running
/// with every connection, as on main, and stores and fans out nothing of
/// that update.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_room_keeps_the_sender_whose_update_a_parked_block_fails_on() {
    let board_id = Uuid::new_v4();
    let store = StoredBoard::default();
    let (parked, edit) = parked_block_and_the_edit_it_fails_on();

    let h = board_on(board_id, &store).await;
    let (crafted, _crafted_rx, _) = join_board(&h).await;
    let (innocent, _innocent_rx, refused) = join_board(&h).await;
    let (_peer, mut peer_rx, peer_refused) = join_board(&h).await;
    send_board(&h, crafted, parked.clone()).await;
    fanned_out(&mut peer_rx, &parked).await;
    send_board(&h, innocent, edit.clone()).await;
    let after = sentinel();
    send_board(&h, innocent, after.clone()).await;

    let before = fanned_out(&mut peer_rx, &after).await;
    assert!(
        !before.contains(&wrap_sync_update(&edit)),
        "the update yrs rejected was fanned out"
    );
    assert_eq!(
        store.rows(),
        vec![parked, after],
        "the update yrs rejected was stored"
    );
    assert!(!refused.is_cancelled(), "the innocent sender was refused");
    assert!(!peer_refused.is_cancelled(), "a peer was refused");
    assert!(!h.shutdown.is_cancelled(), "the board room evicted itself");

    h.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), h.tx.closed())
        .await
        .expect("the board room did not stop");
    let h = board_on(board_id, &store).await;
    let (innocent, _innocent_rx, refused) = join_board(&h).await;
    let (_peer, mut peer_rx, _) = join_board(&h).await;
    send_board(&h, innocent, edit).await;
    let after = sentinel();
    send_board(&h, innocent, after.clone()).await;
    fanned_out(&mut peer_rx, &after).await;
    assert!(
        !refused.is_cancelled(),
        "the innocent sender was locked out after a reload"
    );
    assert!(
        !h.shutdown.is_cancelled(),
        "the board room evicted itself after a reload"
    );
    h.shutdown.cancel();
}

/// As for a page room: past the limit, an update yrs rejects part-way
/// evicts the board room. Passes on HEAD too.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_room_evicts_itself_over_an_update_that_nests_too_deep_before_it_fails() {
    let (base, bad) = nests_too_deep_then_rejected();
    board_refused_after_apply_and_evicted(base, bad).await;
}

// ---------------------------------------------------------------------------
// A refused connection stays refused (F7)
// ---------------------------------------------------------------------------
//
// A room refuses the sender of an update the prefilter refuses, or of one
// that doesn't decode, and the WS shim closes that connection with 4422.
// Until the socket is gone, the room still gets what the connection sent
// after the refused update: its client may have pipelined more, and the
// shim forwards a SyncStep1 as a re-join. The room ignores all of it: it
// doesn't join the connection again, and applies, stores and fans out
// nothing it sends, presence included.

/// The updates a room refuses its sender for and keeps running: one the
/// prefilter refuses, and one that doesn't decode.
fn refused_without_eviction() -> [Vec<u8>; 2] {
    let [no_update, _] = undecodable();
    [chain_update(MAX_NESTING + 1), no_update]
}

/// Whether the text of the page `state` builds holds `text`.
fn holds_text(state: &[u8], text: &str) -> bool {
    use yrs::GetString;
    let doc = page(&[state]);
    let frag = doc.inner().get_or_insert_xml_fragment("default");
    let txn = doc.inner().transact();
    frag.get_string(&txn).contains(text)
}

/// RED on HEAD: the room took the refused connection back on its queued
/// SyncStep1, then applied, persisted and fanned out the edit it sent next;
/// it also fanned out its presence.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connection_stays_refused() {
    for bad in refused_without_eviction() {
        let store = Stored::default();
        let room = room_on(Uuid::new_v4(), &store).await;
        // The connection's handle as the WS shim builds it for every
        // (re-)join: the same sender and the same signals each time.
        let (tx, rx) = mpsc::channel(64);
        let (revoked, refused) = (CancellationToken::new(), CancellationToken::new());
        let handle = || ConnHandle {
            tx: tx.clone(),
            revoked: revoked.clone(),
            refused: refused.clone(),
        };
        let conn = Uuid::new_v4();
        let (reply, joined) = oneshot::channel();
        room.tx
            .send(Event::Join {
                conn_id: conn,
                handle: handle(),
                reply,
            })
            .await
            .unwrap();
        joined.await.unwrap().unwrap();
        let (writer, _writer_rx, _) = join(&room).await;
        let (_peer, mut peer_rx, _) = join(&room).await;

        send(&room, conn, bad).await;
        // Its WS shim closes the socket and stops reading.
        drop(rx);
        // Queued behind the refused update: a SyncStep1, then an edit and
        // presence, twice: a peer's edit fanned out in between must not
        // let the connection back in.
        let (reply, rejoined) = oneshot::channel();
        room.tx
            .send(Event::Join {
                conn_id: conn,
                handle: handle(),
                reply,
            })
            .await
            .unwrap();
        let (edit, _) = text_page(3);
        let mut afters: Vec<Vec<u8>> = Vec::new();
        for _ in 0..2 {
            send(&room, conn, edit.clone()).await;
            room.tx
                .send(Event::AwarenessIn {
                    from: conn,
                    payload: b"presence".to_vec(),
                })
                .await
                .unwrap();
            let after = sentinel();
            send(&room, writer, after.clone()).await;
            let before = fanned_out(&mut peer_rx, &after).await;
            // The room's catch-up may fan an earlier sentinel out again
            // (see `fanned_out`); nothing else may come first.
            assert!(
                before
                    .iter()
                    .all(|f| afters.iter().any(|a| *f == wrap_sync_update(a))),
                "the refused connection's edit or presence was fanned out"
            );
            afters.push(after);
        }
        store.holds(&afters[1]).await;
        assert_eq!(
            store.rows(),
            afters,
            "the refused connection's edit was persisted"
        );
        assert!(
            rejoined.await.is_err(),
            "the refused connection was joined again"
        );
        let (reply, state) = oneshot::channel();
        room.tx.send(Event::ExportState(reply)).await.unwrap();
        let (state, _) = state.await.unwrap().unwrap();
        assert!(
            !holds_text(&state, "hello"),
            "the refused connection's edit was applied"
        );
        assert!(refused.is_cancelled(), "the sender was not told to close");
        assert!(!room.shutdown.is_cancelled(), "the room evicted itself");
        room.shutdown.cancel();
    }
}

/// The same for a board room. RED on HEAD: the board room took the refused
/// connection back on its queued SyncStep1, and applied, stored and fanned
/// out the edit it sent next even without that, as it takes updates from
/// any connection; it also fanned out its presence.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_board_connection_stays_refused() {
    for bad in refused_without_eviction() {
        let store = StoredBoard::default();
        let h = board_on(Uuid::new_v4(), &store).await;
        let (tx, rx) = mpsc::channel(64);
        let refused = CancellationToken::new();
        let handle = || knot_crdt::board_room::ConnHandle {
            tx: tx.clone(),
            revoked: CancellationToken::new(),
            refused: refused.clone(),
        };
        let conn = Uuid::new_v4();
        let (reply, joined) = oneshot::channel();
        h.tx.send(knot_crdt::board_room::Event::Join {
            conn_id: conn,
            handle: handle(),
            reply,
        })
        .await
        .unwrap();
        joined.await.unwrap().unwrap();
        let (writer, _writer_rx, _) = join_board(&h).await;
        let (_peer, mut peer_rx, _) = join_board(&h).await;

        send_board(&h, conn, bad).await;
        drop(rx);
        let (reply, rejoined) = oneshot::channel();
        h.tx.send(knot_crdt::board_room::Event::Join {
            conn_id: conn,
            handle: handle(),
            reply,
        })
        .await
        .unwrap();
        // Twice: fanning the peer's edit out must not drop the refused
        // connection, whose channel is closed, and with it the board room's
        // knowledge that it was refused.
        let (edit, _) = text_page(3);
        let mut afters = Vec::new();
        for _ in 0..2 {
            send_board(&h, conn, edit.clone()).await;
            h.tx.send(knot_crdt::board_room::Event::AwarenessIn {
                from: conn,
                payload: b"presence".to_vec(),
            })
            .await
            .unwrap();
            let after = sentinel();
            send_board(&h, writer, after.clone()).await;
            let before = fanned_out(&mut peer_rx, &after).await;
            assert!(
                before.is_empty(),
                "the refused connection's edit or presence was fanned out"
            );
            afters.push(after);
        }
        assert_eq!(
            store.rows(),
            afters,
            "the refused connection's edit was stored"
        );
        assert!(
            rejoined.await.is_err(),
            "the refused connection was joined again"
        );
        // What a client joining now gets: the board room's live doc.
        let (reply, state) = oneshot::channel();
        h.tx.send(knot_crdt::board_room::Event::Join {
            conn_id: Uuid::new_v4(),
            handle: knot_crdt::board_room::ConnHandle::new(mpsc::channel(64).0),
            reply,
        })
        .await
        .unwrap();
        let state = state.await.unwrap().unwrap();
        assert!(
            !holds_text(&state.update, "hello"),
            "the refused connection's edit was applied"
        );
        assert!(refused.is_cancelled(), "the sender was not told to close");
        assert!(!h.shutdown.is_cancelled(), "the board room evicted itself");
        h.shutdown.cancel();
    }
}

// ---------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------

/// A room reads every inbound update with `knot_crdt::update_items` before
/// yrs reads it (the prefilter, and `inserted_type_ids`), and refuses the
/// sender of one it can't read. It reads the updates this file builds, and
/// every update that ends early in one, as yrs reads them
/// (`common::disagreements`; `tests/update_items.rs` checks the rest of
/// knot-crdt's). The update [`DEEP`] types deep is compared whole: each of
/// its prefixes would be decoded in full. Left out: [`undecodable`]'s clock
/// past `u32::MAX`, which the reader refuses on purpose where yrs's decoder
/// panics in a debug build.
#[test]
fn the_reader_reads_the_updates_of_these_tests_as_yrs_does() {
    let (text, first_char) = text_page(1);
    let (parked, edit) = parked_block_and_the_edit_it_fails_on();
    let (deep_base, too_deep) = nests_too_deep_then_rejected();
    let [no_update, _past_u32_max] = undecodable();
    let updates = [
        ("a chain one past the limit", chain_update(MAX_NESTING + 1)),
        ("a one-paragraph edit", sentinel()),
        ("a page of text", text),
        (
            "an update yrs rejects part-way",
            rejected_part_way(&first_char),
        ),
        ("a block yrs parks", parked),
        ("the edit it fails on", edit),
        ("the crafted base page plus text", deep_base),
        ("an update too deep, then rejected", too_deep),
        ("three bytes that are no update", no_update),
    ];
    common::assert_read_as_yrs_reads(
        updates
            .into_iter()
            .map(|(what, update)| (what.to_string(), update))
            .collect(),
    );
    assert_eq!(
        common::disagreement(&insert_and_delete_update()),
        None,
        "a subtree DEEP types deep, and its deletion"
    );
}
