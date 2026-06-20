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

/// The supervisor must transparently reconnect after the LISTEN connection
/// drops and re-issue LISTEN for active subscriptions, so cross-pod fan-out
/// survives a transient DB blip instead of dying permanently.
#[tokio::test(flavor = "multi_thread")]
async fn reconnects_and_redelivers_after_connection_drop() {
    let db = knot_test_support::fresh_db().await;

    let bus = PgBus::connect(&db.url).await.unwrap();
    let doc = Uuid::new_v4();
    let mut sub = bus.subscribe(doc).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Kill every backend on this database except our own admin connection —
    // that includes the bus's LISTEN connection.
    let cfg = db.url.parse::<tokio_postgres::Config>().unwrap();
    let (admin, admin_conn) = cfg.connect(tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = admin_conn.await;
    });
    admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid()",
            &[],
        )
        .await
        .unwrap();

    // Give the supervisor time to notice the drop, reconnect, and re-LISTEN.
    // publish() uses the swapped-in client, so retry through the reconnect
    // window until a NOTIFY lands.
    let mut delivered = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if bus.publish(doc, 99).await.is_err() {
            continue; // still on the dead client; keep waiting
        }
        if let Ok(Some(seq)) = timeout(Duration::from_millis(300), sub.updates.recv()).await {
            delivered = Some(seq);
            break;
        }
    }
    assert_eq!(
        delivered,
        Some(99),
        "bus did not redeliver after reconnect within the window"
    );
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
