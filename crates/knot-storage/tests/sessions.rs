//! Integration tests for SessionStore against an ephemeral Postgres.

use chrono::{Duration, Utc};
use knot_storage::{
    PgSessionStore, PgUserStore, PgWorkspaceStore, SessionStore, UserStore, WorkspaceRole,
    WorkspaceStore,
};

async fn setup() -> (PgSessionStore, uuid::Uuid, uuid::Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;

    let ws = PgWorkspaceStore::new(pool.clone())
        .create("acme", "Acme")
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
    (PgSessionStore::new(pool), u.id, ws.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn create_find_delete() {
    let (s, user_id, ws_id) = setup().await;
    let id = [1u8; 32];
    let exp = Utc::now() + Duration::days(30);

    s.create(&id, user_id, ws_id, exp, Some("ua"), None)
        .await
        .unwrap();
    let found = s.find_active(&id).await.unwrap().expect("some");
    assert_eq!(found.user_id, user_id);
    assert_eq!(found.user_agent.as_deref(), Some("ua"));
    assert!(found.ip.is_none());

    s.delete(&id).await.unwrap();
    assert!(s.find_active(&id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_sessions_invisible() {
    let (s, user_id, ws_id) = setup().await;
    let id = [2u8; 32];
    let exp = Utc::now() - Duration::seconds(1);
    s.create(&id, user_id, ws_id, exp, None, None)
        .await
        .unwrap();
    assert!(s.find_active(&id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn touch_updates_last_seen() {
    let (s, user_id, ws_id) = setup().await;
    let id = [3u8; 32];
    let exp = Utc::now() + Duration::days(30);
    let created = s
        .create(&id, user_id, ws_id, exp, None, None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    s.touch(&id).await.unwrap();
    let after = s.find_active(&id).await.unwrap().unwrap();
    assert!(after.last_seen_at > created.last_seen_at);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_round_trips_user_agent_and_ip() {
    let (s, user_id, ws_id) = setup().await;
    let id = [4u8; 32];
    let exp = Utc::now() + Duration::days(30);
    let ip: std::net::IpAddr = "203.0.113.7".parse().unwrap();
    s.create(&id, user_id, ws_id, exp, Some("Mozilla/test"), Some(ip))
        .await
        .unwrap();

    let found = s.find_active(&id).await.unwrap().expect("found");
    assert_eq!(found.user_agent.as_deref(), Some("Mozilla/test"));
    assert_eq!(found.ip, Some(ip));
}

#[tokio::test(flavor = "multi_thread")]
async fn ipv6_round_trips() {
    let (s, user_id, ws_id) = setup().await;
    let id = [5u8; 32];
    let exp = Utc::now() + Duration::days(30);
    let ip: std::net::IpAddr = "2001:db8::1".parse().unwrap();
    s.create(&id, user_id, ws_id, exp, None, Some(ip))
        .await
        .unwrap();

    let found = s.find_active(&id).await.unwrap().expect("found");
    assert_eq!(found.ip, Some(ip));
}
