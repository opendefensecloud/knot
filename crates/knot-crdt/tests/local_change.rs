//! The local change notifier fires on the replica that applied an update,
//! never on a replica that received it over the bus (change event hook,
//! spec D5).

use std::sync::Arc;
use std::time::Duration;

use knot_crdt::{Bus, Event, PgBus, Rooms, SnapshotPolicy, YrsEngine};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore};
use tokio::sync::mpsc;
use uuid::Uuid;

const POLICY: SnapshotPolicy = SnapshotPolicy {
    every_n: 1000,
    idle: Duration::from_secs(3600),
};

async fn page(pool: &sqlx::PgPool) -> Uuid {
    let ws: Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
            .fetch_one(pool)
            .await
            .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('u@x.test', 'U') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by) VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A replica: its own bus connection and registry, with both notifiers.
async fn replica(
    db: &knot_test_support::TestDb,
) -> (Arc<Rooms>, mpsc::Receiver<Uuid>, mpsc::Receiver<Uuid>) {
    let bus: Arc<dyn Bus> = Arc::new(PgBus::connect(&db.url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(db.pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(db.pool.clone()));
    let (local_tx, local_rx) = mpsc::channel(64);
    let (dirty_tx, dirty_rx) = mpsc::channel(64);
    let rooms = Rooms::new(
        Arc::new(YrsEngine),
        bus,
        updates,
        snaps,
        POLICY,
        Duration::from_secs(3600),
    )
    .with_local_change_tx(local_tx)
    .with_dirty_tx(dirty_tx);
    (Arc::new(rooms), local_rx, dirty_rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn fires_only_on_the_replica_that_applied() {
    let db = knot_test_support::fresh_db().await;
    let doc = page(&db.pool).await;
    let (a, mut a_local, _a_dirty) = replica(&db).await;
    let (b, mut b_local, mut b_dirty) = replica(&db).await;
    let room_a = a.acquire(doc).await.unwrap();
    let _room_b = b.acquire(doc).await.unwrap();

    let (_doc, update_bytes) = knot_markdown::from_markdown::parse("hello\n").unwrap();
    let (reply, done) = tokio::sync::oneshot::channel();
    room_a
        .tx
        .send(Event::ApplyUpdate {
            update_bytes,
            by_user: None,
            source: knot_storage::UpdateSource::Edit,
            meta: None,
            reply,
        })
        .await
        .unwrap();
    done.await.unwrap().expect("apply");

    let got = tokio::time::timeout(Duration::from_secs(5), a_local.recv())
        .await
        .expect("A fired");
    assert_eq!(got, Some(doc));
    // B received the row: its dirty notifier fired …
    let got = tokio::time::timeout(Duration::from_secs(5), b_dirty.recv())
        .await
        .expect("B received the update");
    assert_eq!(got, Some(doc));
    // … but its local notifier did not.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), b_local.recv())
            .await
            .is_err(),
        "B must not report A's edit"
    );
}
