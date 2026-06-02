use knot_storage::{
    DocStore, PgDocStore, PgSnapshotStore, PgUserStore, PgWorkspaceStore, SnapshotStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

async fn setup() -> (PgSnapshotStore, uuid::Uuid) {
    let c = Postgres::default().start().await.unwrap();
    let port = c.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    std::mem::forget(c);

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
    let d = PgDocStore::new(pool.clone())
        .create(ws.id, None, "D", "m", u.id)
        .await
        .unwrap();
    (PgSnapshotStore::new(pool), d.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_and_load_latest_round_trip() {
    let (s, doc) = setup().await;
    s.insert(doc, 100, b"state-bytes", b"sv-bytes")
        .await
        .unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(got.snapshot_seq, 100);
    assert_eq!(got.state_bytes, b"state-bytes");
    assert_eq!(got.state_vector, b"sv-bytes");
}

#[tokio::test(flavor = "multi_thread")]
async fn latest_returns_highest_snapshot_seq() {
    let (s, doc) = setup().await;
    s.insert(doc, 100, b"a", b"a").await.unwrap();
    s.insert(doc, 200, b"b", b"b").await.unwrap();
    s.insert(doc, 150, b"c", b"c").await.unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(got.snapshot_seq, 200);
    assert_eq!(got.state_bytes, b"b");
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_overwrites_same_seq() {
    let (s, doc) = setup().await;
    s.insert(doc, 100, b"v1", b"sv1").await.unwrap();
    s.insert(doc, 100, b"v2", b"sv2").await.unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(got.state_bytes, b"v2");
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_keeps_recent_and_per_day() {
    let (s, doc) = setup().await;
    for i in 1..=7i64 {
        s.insert(doc, i * 100, &format!("v{i}").into_bytes(), b"sv")
            .await
            .unwrap();
    }
    let n = s.gc(doc, 5, 30).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(s.latest(doc).await.unwrap().unwrap().snapshot_seq, 700);
}
