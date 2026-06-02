use knot_crdt::{Bus, PgBus};
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn publish_reaches_subscriber_via_pg() {
    let c = Postgres::default().start().await.unwrap();
    let port = c.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    // Touch via sqlx to stabilize the wait.
    let _pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    std::mem::forget(c);

    let bus = PgBus::connect(&url).await.unwrap();
    let doc = Uuid::new_v4();
    let mut sub = bus.subscribe(doc).await.unwrap();
    // LISTEN takes a tick to settle; give it head start.
    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish(doc, 7).await.unwrap();
    let got = timeout(Duration::from_secs(2), sub.updates.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_round_trip_via_pg() {
    let c = Postgres::default().start().await.unwrap();
    let port = c.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let _pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    std::mem::forget(c);

    let bus = PgBus::connect(&url).await.unwrap();
    let doc = Uuid::new_v4();
    let mut sub = bus.subscribe(doc).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.publish_presence(doc, vec![9, 8, 7]).await.unwrap();
    let got = timeout(Duration::from_secs(2), sub.presence.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, vec![9, 8, 7]);
}
