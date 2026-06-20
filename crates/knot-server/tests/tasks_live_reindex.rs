//! Integration: a doc edit fires the room dirty-notify, the reindex
//! worker picks it up on the next tick, and `TaskStore::list_for_assignee`
//! returns the new row without any HTTP roundtrip.

use std::sync::Arc;
use std::time::Duration;

use knot_auth::{Hasher, Throttle};
use knot_crdt::{Event, Rooms, SnapshotPolicy, YrsEngine};
use knot_server::AppState;
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};

#[tokio::test]
async fn doc_edit_with_mention_lands_in_task_index_without_http() {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    // Standard AppState wiring.
    let mut state = AppState::with_pool(pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = state.hasher.hash("hunter22").unwrap();
    let ws = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "WS")
        .await
        .unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "T", "m", user.id)
        .await
        .unwrap();

    // Rooms registry with dirty-notify wired to the reindex worker.
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::PgBus::connect(&db.url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    let policy = SnapshotPolicy {
        every_n: 1000,
        idle: Duration::from_secs(3600),
    };
    let (dirty_tx, dirty_rx) = tokio::sync::mpsc::channel::<uuid::Uuid>(64);
    let rooms = Arc::new(
        Rooms::new(
            Arc::new(YrsEngine),
            bus.clone(),
            updates,
            snaps,
            policy,
            Duration::from_secs(3600),
        )
        .with_dirty_tx(dirty_tx),
    );
    state.bus = Some(bus);
    state.rooms_v2 = Some(rooms.clone());

    // Spawn the worker against the same state.
    knot_server::reindex::spawn(state.clone(), dirty_rx);

    // Build a y-update that contains a single task item assigning the
    // user to "ship it" via @-mention. The mention extension serializes
    // as `[Alice](knot://user/<uuid>)` in markdown — round-tripping
    // through from_markdown produces the same nodes the extractor
    // expects.
    let md = format!("- [ ] [Alice](knot://user/{}) ship it\n", user.id);
    let (_handle, update_bytes) = knot_markdown::from_markdown::parse(&md).unwrap();

    // Acquire the room and apply the update.
    let room = rooms.acquire(doc.id).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(Event::ApplyUpdate {
            update_bytes,
            by_user: Some(user.id),
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().expect("apply failed");

    // Worker flushes every 2s; wait at most ~5s for the row to appear.
    let tasks_store = state.tasks.as_ref().unwrap().clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    loop {
        let rows = tasks_store
            .list_for_assignee(ws.id, user.id, false)
            .await
            .unwrap();
        if let Some(row) = rows.first() {
            assert!(row.text.contains("ship it"), "got: {:?}", row.text);
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("task row did not appear within deadline");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
