//! Updates that nest a page's types deeper than a room accepts
//! (`knot_crdt::nesting`, F1 review). yrs deletes and garbage-collects a
//! nested type by recursing once per level, so a crafted update holding a
//! subtree 100,000 types deep, and its deletion, overflows a tokio worker's
//! stack while it is applied, and a stack overflow aborts the whole server.
//! A room checks every inbound update first, and refuses one that nests
//! past `MAX_NESTING`: nothing of it is applied, persisted or fanned out.
//!
//! The checks run on a thread with the stack of a tokio worker, 2 MiB, and
//! the room on a multi-threaded runtime, whose workers have that stack. This
//! file is a test binary of its own, so that an overflow can abort only
//! these tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use knot_crdt::nesting::{MAX_NESTING, TooDeep, check_update, nests_deeper_than};
use knot_crdt::protocol::wrap_sync_update;
use knot_crdt::{
    ConnHandle, DocHandle, Engine, Event, InMsg, MemBus, Room, RoomHandle, SnapshotPolicy,
    YrsEngine,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use yrs::{
    Doc, Options, ReadTxn, StateVector, Text, Transact, XmlElementPrelim, XmlElementRef,
    XmlFragment, XmlTextPrelim,
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

struct NoSnapshots;

#[async_trait::async_trait]
impl knot_storage::SnapshotStore for NoSnapshots {
    async fn insert(
        &self,
        _: Uuid,
        _: i64,
        _: &[u8],
        _: &[u8],
    ) -> Result<(), knot_storage::SnapshotStoreError> {
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
    let bus = Arc::new(MemBus::new());
    let doc_id = Uuid::new_v4();
    let sub = knot_crdt::Bus::subscribe(bus.as_ref(), doc_id)
        .await
        .unwrap();
    let persisted = Arc::new(Mutex::new(Vec::new()));
    let room = Room::spawn(
        doc_id,
        Arc::new(YrsEngine),
        bus,
        sub,
        Arc::new(CapturingUpdates {
            persisted: persisted.clone(),
        }),
        Arc::new(NoSnapshots),
        SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(3600),
        },
        None,
    )
    .await
    .unwrap();
    (room, persisted)
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
