use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;

async fn state_with_seeded_user(email: &str, password: &str) -> (AppState, uuid::Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;

    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    // Seed workspace + user + membership using the same stores.
    let hash = s.hasher.hash(password).unwrap();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "Workspace")
        .await
        .unwrap();
    let user = s
        .users
        .as_ref()
        .unwrap()
        .create_local(email, "Test", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    (s, user.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn login_session_logout_happy_path() {
    let (state, user_id) = state_with_seeded_user("alice@example.com", "hunter22").await;
    let app = router_with_state(state);

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": "alice@example.com", "password": "hunter22"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let sid_cookie = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .expect("sid")
        .clone();
    assert!(cookies.iter().any(|c| c.starts_with("csrf=")), "csrf set");
    let sid_kv = sid_cookie.split(';').next().unwrap().to_string();

    // GET /auth/session with cookie.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auth/session")
                .header("cookie", &sid_kv)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["email"], "alice@example.com");
    assert_eq!(v["user_id"], user_id.to_string());
    assert_eq!(v["role"], "owner");

    // Logout.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/logout")
                .header("cookie", &sid_kv)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // /auth/session now 401.
    let r = app
        .oneshot(
            Request::builder()
                .uri("/auth/session")
                .header("cookie", &sid_kv)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn login_wrong_password_returns_401() {
    let (state, _) = state_with_seeded_user("alice@example.com", "hunter22").await;
    let app = router_with_state(state);
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": "alice@example.com", "password": "WRONG"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_hmac_round_trip_and_store_does_not_hold_raw_token() {
    // After login the session cookie holds the raw token, but the store
    // persists HMAC(session_key, token). A raw-token lookup must return None
    // while the cookie still authenticates (the loader hashes before lookup).
    let (state, _) = state_with_seeded_user("hmac@example.com", "hmacpass1").await;
    let app = router_with_state(state.clone());

    // Login → get sid cookie.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": "hmac@example.com", "password": "hmacpass1"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let sid_kv = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .expect("sid cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // Cookie still authenticates (create and find_active both hash the token).
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auth/session")
                .header("cookie", &sid_kv)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "cookie must authenticate");

    // The raw token (without HMAC) must NOT be in the store.
    let raw_val = sid_kv.trim_start_matches("sid=");
    let raw_decoded = knot_auth::SessionToken::decode(raw_val).expect("decode");
    let found_raw = state
        .sessions
        .as_ref()
        .unwrap()
        .find_active(raw_decoded.as_bytes())
        .await
        .unwrap();
    assert!(
        found_raw.is_none(),
        "raw token must not be stored; only the HMAC is"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unauth_session_returns_401() {
    let (state, _) = state_with_seeded_user("alice@example.com", "hunter22").await;
    let app = router_with_state(state);
    let r = app
        .oneshot(
            Request::builder()
                .uri("/auth/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}
