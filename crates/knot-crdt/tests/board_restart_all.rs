//! `BoardRooms::restart_all`: when the ACL listener may have missed access
//! changes, every board room on this replica ends, so each socket closes
//! with 1012 and its client reconnects through the upgrade's access check.
use knot_crdt::{MemBus, YrsEngine};
use knot_storage::{
    BoardStore, DocStore, PgBoardStore, PgDocStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore, sort_key_between,
};
use std::sync::Arc;
use uuid::Uuid;

/// A workspace with a member, a page and two boards on it. Returns the
/// store, the page and both boards.
async fn two_boards() -> (Arc<PgBoardStore>, Uuid, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let workspace = workspaces.create("default", "W").await.unwrap();
    let user = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    workspaces
        .add_member(workspace.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let sort_key = sort_key_between(None, None);
    let doc = PgDocStore::new(pool.clone())
        .create(workspace.id, None, "Doc", &sort_key, user.id)
        .await
        .unwrap();
    let store = Arc::new(PgBoardStore::new(pool.clone()));
    let first_board = store
        .create(doc.id, user.id, Some("First".into()))
        .await
        .unwrap();
    let second_board = store
        .create(doc.id, user.id, Some("Second".into()))
        .await
        .unwrap();
    (store, doc.id, first_board.id, second_board.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_all_ends_every_board_room_and_bumps_every_epoch() {
    let (pg_store, doc_id, first_board_id, second_board_id) = two_boards().await;
    let store: Arc<dyn BoardStore> = pg_store;
    let rooms = knot_crdt::BoardRooms::new(Arc::new(YrsEngine), store, Arc::new(MemBus::new()));
    let first_room = rooms.acquire(first_board_id, doc_id).await.unwrap();
    let second_room = rooms.acquire(second_board_id, doc_id).await.unwrap();
    let unopened_doc_id = Uuid::new_v4();
    let epochs_before = [doc_id, unopened_doc_id].map(|page_id| rooms.acl_epoch(page_id));

    rooms.restart_all().await;

    for room in [&first_room, &second_room] {
        assert!(
            room.shutdown.is_cancelled(),
            "a board room was left running"
        );
        assert!(
            !room.revoked.is_cancelled(),
            "a restart must not revoke (4403) its connections"
        );
    }
    let epochs_after = [doc_id, unopened_doc_id].map(|page_id| rooms.acl_epoch(page_id));
    for (before, after) in epochs_before.iter().zip(epochs_after.iter()) {
        assert_ne!(before, after, "an epoch was not bumped");
    }
    let fresh_room = rooms.acquire(first_board_id, doc_id).await.unwrap();
    assert!(
        !Arc::ptr_eq(&fresh_room, &first_room),
        "the ended board room was reused"
    );
}
