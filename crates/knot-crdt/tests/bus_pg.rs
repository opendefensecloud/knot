use knot_crdt::{Bus, PgBus};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn publish_reaches_subscriber_via_pg() {
    // fresh_db hands back a pool already warmed against the unique DB,
    // which stabilizes the LISTEN handshake below.
    let db = knot_test_support::fresh_db().await;

    let bus = PgBus::connect(&db.url).await.unwrap();
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
    let db = knot_test_support::fresh_db().await;

    let bus = PgBus::connect(&db.url).await.unwrap();
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
