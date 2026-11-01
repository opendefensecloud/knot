/// Cross-pod convergence tests for BoardRoom.
///
/// Two BoardRoom instances share one in-process MemBus and one PgBoardStore
/// (backed by a fresh_db), simulating two pods serving the same board.
/// Edits sent to room A must fan out to connections joined to room B via the
/// bus → since → apply → broadcast pipeline.  Presence published from room A
/// must also reach room B's connections.
use knot_crdt::{
    BoardRoom, Bus, Engine, MemBus, Subscription, YrsEngine,
    board_room::{ConnHandle, Event, InMsg},
};
use knot_storage::{
    BoardStore, DocStore, PgBoardStore, PgDocStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore, sort_key_between,
};
use std::sync::Arc;
use uuid::Uuid;

/// Spin up the board fixture: workspace → user → member → doc → board.
/// Returns `(store, board_id)`.
async fn setup_board() -> (Arc<PgBoardStore>, Uuid) {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool;

    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let u = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let sk = sort_key_between(None, None);
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "Doc", &sk, u.id)
        .await
        .unwrap();
    let store = Arc::new(PgBoardStore::new(pool.clone()));
    let board = store
        .create(doc.id, u.id, Some("TestBoard".into()))
        .await
        .unwrap();
    (store, board.id)
}

/// Build a minimal yrs update that inserts an entry in the "elements" map.
/// This mirrors the Excalidraw Yjs schema used by the board engine.
fn make_elements_update() -> Vec<u8> {
    use yrs::{Map, ReadTxn, Transact};
    let doc = yrs::Doc::new();
    {
        let elements = doc.get_or_insert_map("elements");
        let mut txn = doc.transact_mut();
        elements.insert(&mut txn, "shape-1", "rect");
    }
    let txn = doc.transact();
    txn.encode_state_as_update_v1(&yrs::StateVector::default())
}

/// Helper: join a connection to a room and drain the initial-state reply.
/// Returns the per-connection receiver for subsequent updates.
async fn join_conn(
    room: &knot_crdt::board_room::BoardRoomHandle,
) -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(Event::Join {
            conn_id: Uuid::new_v4(),
            handle: ConnHandle::new(tx),
            reply: reply_tx,
        })
        .await
        .unwrap();
    // Drain the initial-state frame — we don't need its content here.
    let _ = reply_rx.await.unwrap().unwrap();
    rx
}

#[tokio::test(flavor = "multi_thread")]
async fn edit_on_one_room_reaches_a_conn_on_another_room() {
    let (pg_store, board_id) = setup_board().await;
    let store: Arc<dyn BoardStore> = pg_store;

    let bus = Arc::new(MemBus::new());
    let engine: Arc<dyn Engine> = Arc::new(YrsEngine);

    // Subscribe before spawning so no notifications are missed.
    let sub_a: Subscription = bus.subscribe(board_id).await.unwrap();
    let sub_b: Subscription = bus.subscribe(board_id).await.unwrap();

    let room_a = BoardRoom::spawn(
        board_id,
        Uuid::new_v4(),
        engine.clone(),
        store.clone(),
        bus.clone(),
        sub_a,
    )
    .await
    .unwrap();
    let room_b = BoardRoom::spawn(
        board_id,
        Uuid::new_v4(),
        engine.clone(),
        store.clone(),
        bus.clone(),
        sub_b,
    )
    .await
    .unwrap();

    // Connect a client to room B so it can receive the cross-pod fan-out.
    let mut rx_b = join_conn(&room_b).await;

    // Send a real yrs update to room A.
    let update = make_elements_update();
    room_a
        .tx
        .send(Event::Inbound(InMsg {
            from: Uuid::new_v4(),
            bytes: update,
        }))
        .await
        .unwrap();

    // Room A persists → publishes seq on the bus → room B's bus_updates_rx
    // fires → room B fetches via `store.since` → applies → fans out to rx_b.
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx_b.recv())
        .await
        .expect("timed out waiting for cross-pod update on room B");
    assert!(got.is_some(), "room B did not receive cross-pod update");

    room_a.shutdown.cancel();
    room_b.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_on_one_room_reaches_a_conn_on_another_room() {
    let (pg_store, board_id) = setup_board().await;
    let store: Arc<dyn BoardStore> = pg_store;

    let bus = Arc::new(MemBus::new());
    let engine: Arc<dyn Engine> = Arc::new(YrsEngine);

    let sub_a: Subscription = bus.subscribe(board_id).await.unwrap();
    let sub_b: Subscription = bus.subscribe(board_id).await.unwrap();

    let room_a = BoardRoom::spawn(
        board_id,
        Uuid::new_v4(),
        engine.clone(),
        store.clone(),
        bus.clone(),
        sub_a,
    )
    .await
    .unwrap();
    let room_b = BoardRoom::spawn(
        board_id,
        Uuid::new_v4(),
        engine.clone(),
        store.clone(),
        bus.clone(),
        sub_b,
    )
    .await
    .unwrap();

    // Connect a client to room B.
    let mut rx_b = join_conn(&room_b).await;

    // Publish a presence payload from room A.
    room_a
        .tx
        .send(Event::AwarenessIn {
            from: Uuid::new_v4(),
            payload: b"presence-bytes".to_vec(),
        })
        .await
        .unwrap();

    // Room A publishes presence on the bus → room B's bus_presence_rx fires
    // → room B fans out the payload to rx_b.
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx_b.recv())
        .await
        .expect("timed out waiting for cross-pod presence on room B");
    assert!(got.is_some(), "room B did not receive cross-pod presence");

    room_a.shutdown.cancel();
    room_b.shutdown.cancel();
}

/// A peer's update whose bus NOTIFY was dropped must still converge: the
/// periodic catch-up tick re-sweeps the shared log from the watermark. We
/// simulate the dropped notify by writing directly to the store and never
/// publishing on the bus, then assert the connection still receives the update.
#[tokio::test(flavor = "multi_thread")]
async fn catch_up_tick_recovers_a_missed_update() {
    let (pg_store, board_id) = setup_board().await;
    let store: Arc<dyn BoardStore> = pg_store.clone();

    let bus = Arc::new(MemBus::new());
    let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
    let sub: Subscription = bus.subscribe(board_id).await.unwrap();

    let room = BoardRoom::spawn(board_id, Uuid::new_v4(), engine, store, bus, sub)
        .await
        .unwrap();
    let mut rx = join_conn(&room).await;

    // Append directly to the shared log WITHOUT publishing on the bus — exactly
    // what a dropped NOTIFY (or a write during a bus reconnect) looks like to
    // this pod. Only the catch-up tick can recover it.
    let update = make_elements_update();
    pg_store.append_update(board_id, &update).await.unwrap();

    // The tick runs every 5s; allow generous slack.
    let got = tokio::time::timeout(std::time::Duration::from_secs(9), rx.recv())
        .await
        .expect("timed out: catch-up tick did not recover the missed update");
    assert!(got.is_some(), "conn did not receive the recovered update");

    room.shutdown.cancel();
}

/// Join a connection to `room` and return its `revoked` token.
async fn join_revocable(
    room: &knot_crdt::board_room::BoardRoomHandle,
) -> (
    tokio_util::sync::CancellationToken,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    // The WS shim makes each connection's token a child of the room's.
    let revoked = room.revoked.child_token();
    let handle = ConnHandle {
        tx,
        revoked: revoked.clone(),
        refused: tokio_util::sync::CancellationToken::new(),
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(Event::Join {
            conn_id: Uuid::new_v4(),
            handle,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let _ = reply_rx.await.unwrap().unwrap();
    (revoked, rx)
}

/// An ACL change on a page closes the sockets of its boards (4403) and only
/// of its boards, and bumps the page's ACL epoch even when none of its
/// boards is open. The next join boots a fresh board room, which a grantee
/// can join again.
#[tokio::test(flavor = "multi_thread")]
async fn revoke_for_doc_revokes_only_that_docs_board_rooms() {
    let (pg_store, board_id) = setup_board().await;
    let store: Arc<dyn BoardStore> = pg_store;
    let other_board_id = Uuid::new_v4();
    let (doc_id, other_doc_id) = (Uuid::new_v4(), Uuid::new_v4());
    let rooms = knot_crdt::BoardRooms::new(Arc::new(YrsEngine), store, Arc::new(MemBus::new()));

    let room = rooms.acquire(board_id, doc_id).await.unwrap();
    let other_room = rooms.acquire(other_board_id, other_doc_id).await.unwrap();
    let (revoked, _rx) = join_revocable(&room).await;
    let (other_revoked, _other_rx) = join_revocable(&other_room).await;

    let epoch_before = rooms.acl_epoch(doc_id);
    rooms.revoke_for_doc(doc_id).await;
    assert!(revoked.is_cancelled(), "the page's board socket stays open");
    assert!(
        room.shutdown.is_cancelled(),
        "the revoked board room was not evicted"
    );
    assert!(
        !other_revoked.is_cancelled(),
        "another page's board socket was revoked"
    );
    assert_ne!(rooms.acl_epoch(doc_id), epoch_before, "epoch not bumped");

    let unopened_doc_id = Uuid::new_v4();
    let unopened_epoch = rooms.acl_epoch(unopened_doc_id);
    rooms.revoke_for_doc(unopened_doc_id).await;
    assert_ne!(
        rooms.acl_epoch(unopened_doc_id),
        unopened_epoch,
        "a revoke with no board room open must bump the epoch too"
    );

    let fresh = rooms.acquire(board_id, doc_id).await.unwrap();
    assert!(!Arc::ptr_eq(&fresh, &room), "the revoked room was reused");
    let (fresh_revoked, _fresh_rx) = join_revocable(&fresh).await;
    assert!(!fresh_revoked.is_cancelled());
    fresh.shutdown.cancel();
    other_room.shutdown.cancel();
}
