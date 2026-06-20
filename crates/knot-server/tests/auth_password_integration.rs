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

/// Log in and return (app clone, sid_kv, csrf_val) for use in subsequent requests.
async fn login(app: axum::Router, email: &str, password: &str) -> (Vec<String>, String, String) {
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login should succeed");

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

    let csrf_val = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .expect("csrf cookie")
        .split(';')
        .next()
        .unwrap()
        .split('=')
        .nth(1)
        .unwrap()
        .to_string();

    (cookies, sid_kv, csrf_val)
}

async fn assert_login_status(app: axum::Router, email: &str, password: &str) -> StatusCode {
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    r.status()
}

// ---------------------------------------------------------------------------
// 1. Happy path — change password, new works, old fails.
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn happy_path_changes_hash_and_lets_user_log_in_with_new() {
    let (state, _) = state_with_seeded_user("bob@example.com", "oldpass1").await;
    let app = router_with_state(state);

    let (_, sid_kv, csrf_val) = login(app.clone(), "bob@example.com", "oldpass1").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({"current": "oldpass1", "new": "newpass99"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // Login with new password succeeds.
    assert_eq!(
        assert_login_status(app.clone(), "bob@example.com", "newpass99").await,
        StatusCode::NO_CONTENT,
        "new password should work"
    );

    // Login with old password fails.
    assert_eq!(
        assert_login_status(app.clone(), "bob@example.com", "oldpass1").await,
        StatusCode::UNAUTHORIZED,
        "old password should be rejected"
    );
}

// ---------------------------------------------------------------------------
// 2. Wrong current password → 401 auth.invalid_credentials
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn wrong_current_password_returns_401_invalid_credentials() {
    let (state, _) = state_with_seeded_user("carol@example.com", "realpass1").await;
    let app = router_with_state(state);

    let (_, sid_kv, csrf_val) = login(app.clone(), "carol@example.com", "realpass1").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({"current": "WRONGPASS", "new": "newpass99"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.invalid_credentials");
}

// ---------------------------------------------------------------------------
// 3. Weak new password (< 8 chars) → 400 auth.weak_password
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn weak_new_password_returns_400_weak_password() {
    let (state, _) = state_with_seeded_user("dave@example.com", "strongpass1").await;
    let app = router_with_state(state);

    let (_, sid_kv, csrf_val) = login(app.clone(), "dave@example.com", "strongpass1").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({"current": "strongpass1", "new": "short"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.weak_password");
}

// ---------------------------------------------------------------------------
// 4. Reusing current password → 400 auth.password_reuse
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn reusing_current_password_returns_400_password_reuse() {
    let (state, _) = state_with_seeded_user("eve@example.com", "samepass1").await;
    let app = router_with_state(state);

    let (_, sid_kv, csrf_val) = login(app.clone(), "eve@example.com", "samepass1").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({"current": "samepass1", "new": "samepass1"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.password_reuse");
}

// ---------------------------------------------------------------------------
// 5. No session cookie → 401 auth.session_required
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_returns_401_session_required() {
    let (state, _) = state_with_seeded_user("frank@example.com", "frankpass1").await;
    let app = router_with_state(state);

    // Provide a fake csrf value so we pass CSRF middleware but have no valid sid.
    let fake_csrf = "fake-csrf-token";

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("csrf={fake_csrf}"))
                .header("x-csrf-token", fake_csrf)
                .body(Body::from(
                    serde_json::json!({"current": "frankpass1", "new": "newpass99"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.session_required");
}

// ---------------------------------------------------------------------------
// 6. Throttle — 5 wrong-current attempts fill the bucket, 6th returns 429
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn throttle_returns_429_after_repeated_wrong_currents() {
    let (state, _) = state_with_seeded_user("grace@example.com", "first-password1").await;
    let app = router_with_state(state);

    let (_, sid_kv, csrf_val) = login(app.clone(), "grace@example.com", "first-password1").await;

    // 5 wrong-current attempts (CAPACITY = 5).
    for _ in 0..5 {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/password")
                    .header("content-type", "application/json")
                    .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                    .header("x-csrf-token", &csrf_val)
                    .body(Body::from(
                        serde_json::json!({"current": "wrong-password", "new": "correct-horse-22"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    // 6th attempt should be throttled.
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({"current": "wrong-password", "new": "correct-horse-22"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.throttled");
}
