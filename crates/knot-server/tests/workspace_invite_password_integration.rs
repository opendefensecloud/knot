use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn state_with_seeded_user(email: &str, password: &str) -> AppState {
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

    s
}

/// Log in and return (sid_kv, csrf_val).
async fn login(app: axum::Router, email: &str, password: &str) -> (String, String) {
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

    (sid_kv, csrf_val)
}

// ---------------------------------------------------------------------------
// 1. invite_with_password_creates_user_and_member
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn invite_with_password_creates_user_and_member() {
    let state = state_with_seeded_user("alice@example.com", "alicepass1").await;
    let app = router_with_state(state.clone());

    let (sid_kv, csrf_val) = login(app.clone(), "alice@example.com", "alicepass1").await;

    // POST invite with brand-new email + password.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({
                        "email": "newuser@example.com",
                        "role": "editor",
                        "password": "newpass99"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "invite should return 201");

    // Verify the new user has a password_hash in the DB via the user store.
    let user = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("newuser@example.com")
        .await
        .unwrap()
        .expect("new user should exist in DB");
    assert!(
        user.password_hash.is_some(),
        "invited user should have a password_hash"
    );

    // Verify the new user can log in with the supplied password.
    let login_status = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "email": "newuser@example.com",
                        "password": "newpass99"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        login_status,
        StatusCode::NO_CONTENT,
        "new user should be able to log in with the invited password"
    );
}

// ---------------------------------------------------------------------------
// 2. invite_existing_user_without_password_still_works
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn invite_existing_user_without_password_still_works() {
    let state = state_with_seeded_user("alice@example.com", "alicepass1").await;

    // Pre-create a user with a dummy hash (simulating an existing local user).
    let dummy_hash = state.hasher.hash("bobpass99").unwrap();
    state
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", &dummy_hash)
        .await
        .unwrap();

    let app = router_with_state(state.clone());
    let (sid_kv, csrf_val) = login(app.clone(), "alice@example.com", "alicepass1").await;

    // POST invite for existing user, no password field.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({
                        "email": "bob@example.com",
                        "role": "editor"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::CREATED,
        "inviting an existing user without password should return 201"
    );
}

// ---------------------------------------------------------------------------
// 3. invite_unknown_email_without_password_returns_404
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn invite_unknown_email_without_password_returns_404() {
    let state = state_with_seeded_user("alice@example.com", "alicepass1").await;
    let app = router_with_state(state);

    let (sid_kv, csrf_val) = login(app.clone(), "alice@example.com", "alicepass1").await;

    // POST invite for a missing email, no password.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({
                        "email": "nobody@example.com",
                        "role": "editor"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::NOT_FOUND,
        "unknown email without password should return 404"
    );

    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["error"]["code"], "workspace.user_not_found",
        "error code should be workspace.user_not_found"
    );
}

// ---------------------------------------------------------------------------
// 4. invite_sets_display_name_when_provided
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn invite_sets_display_name_when_provided() {
    let state = state_with_seeded_user("owner@x.test", "ownerpass1").await;
    let app = router_with_state(state.clone());

    let (sid_kv, csrf_val) = login(app.clone(), "owner@x.test", "ownerpass1").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({
                        "email": "newbie@x.test",
                        "role": "editor",
                        "password": "newbie-pass-1",
                        "display_name": "Ada Lovelace"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "invite should return 201");

    let user = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("newbie@x.test")
        .await
        .unwrap()
        .expect("new user should exist in DB");
    assert_eq!(
        user.display_name, "Ada Lovelace",
        "display_name should be set from the invite request"
    );
}

// ---------------------------------------------------------------------------
// 5. invite_falls_back_to_email_prefix_without_display_name
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn invite_falls_back_to_email_prefix_without_display_name() {
    let state = state_with_seeded_user("owner2@x.test", "ownerpass2").await;
    let app = router_with_state(state.clone());

    let (sid_kv, csrf_val) = login(app.clone(), "owner2@x.test", "ownerpass2").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", &csrf_val)
                .body(Body::from(
                    serde_json::json!({
                        "email": "plain@x.test",
                        "role": "viewer",
                        "password": "plain-pass-1"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "invite should return 201");

    let user = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("plain@x.test")
        .await
        .unwrap()
        .expect("new user should exist in DB");
    assert_eq!(
        user.display_name, "plain",
        "display_name should fall back to email prefix when not provided"
    );
}
