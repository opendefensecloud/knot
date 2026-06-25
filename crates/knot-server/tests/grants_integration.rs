//! HTTP-layer tests for the document grants API:
//! - GET  /api/docs/:id/grants
//! - PUT  /api/docs/:id/grants/:principal
//! - DELETE /api/docs/:id/grants/:principal
//!
//! Verifies owner-gating (403 for non-owners), group-principal rejection
//! (422 `grant.group_unsupported`), and cross-workspace/unknown doc handling.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Shared helpers — mirror docs_integration.rs `login_state` verbatim.
// ---------------------------------------------------------------------------

/// Build an AppState with a seeded workspace + owner, log the owner in via
/// HTTP, and return the `(state, "sid=…; csrf=…|<csrf_token>")` pair.
async fn login_state(email: &str, password: &str) -> (AppState, String) {
    let pool = knot_test_support::fresh_db().await.pool;

    let mut s = AppState::with_pool(pool);
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = s.hasher.hash(password).unwrap();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let u = s
        .users
        .as_ref()
        .unwrap()
        .create_local(email, "U", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    let app = router_with_state(s.clone());
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
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let sid_kv = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf_kv = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let cookie_header = format!("{sid_kv}; {csrf_kv}");
    let csrf_token = csrf_kv.trim_start_matches("csrf=").to_string();
    (s, format!("{cookie_header}|{csrf_token}"))
}

fn split_cookie_csrf(joined: &str) -> (&str, &str) {
    joined.split_once('|').unwrap()
}

/// Invite a second user to the workspace with a given role (using the
/// `POST /api/workspace/members` endpoint) and return the new user's UUID
/// (fetched from the user store after creation, since the endpoint returns 201
/// with no body).
async fn invite_user(
    app: axum::Router,
    owner_cookie: &str,
    owner_csrf: &str,
    email: &str,
    password: &str,
    role: &str,
    state: &AppState,
) -> String {
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", owner_cookie)
                .header("x-csrf-token", owner_csrf)
                .body(Body::from(
                    serde_json::json!({
                        "email": email,
                        "role": role,
                        "password": password
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "invite {email} as {role}");
    // The endpoint returns 201 with no body; look up the user in the store.
    let user = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email(email)
        .await
        .unwrap()
        .expect("invited user should exist");
    user.id.to_string()
}

/// Log in as `email`/`password` and return `"sid=…; csrf=…|<csrf_token>"`.
async fn login_as(app: axum::Router, email: &str, password: &str) -> String {
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
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login {email}");
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let sid_kv = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf_kv = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let cookie_header = format!("{sid_kv}; {csrf_kv}");
    let csrf_token = csrf_kv.trim_start_matches("csrf=").to_string();
    format!("{cookie_header}|{csrf_token}")
}

/// Create a doc via HTTP and return its UUID string.
async fn create_doc(app: axum::Router, cookie: &str, csrf: &str, title: &str) -> String {
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/docs")
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"title": title}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "create doc {title}");
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    v["id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// Test 1: owner can PUT/DELETE a grant + GET lists it
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn owner_can_put_and_delete_grant() {
    let (state, joined) = login_state("owner@grants.test", "pass1234").await;
    let (cookie, csrf) = split_cookie_csrf(&joined);
    let app = router_with_state(state.clone());

    // Create a doc as the owner.
    let doc_id = create_doc(app.clone(), cookie, csrf, "Grant Doc").await;

    // Invite a second user so we have a real UUID to grant to.
    let other_id = invite_user(
        app.clone(),
        cookie,
        csrf,
        "grantee@grants.test",
        "granteepass",
        "viewer",
        &state,
    )
    .await;
    let principal = format!("user:{other_id}");

    // PUT grant: owner grants grantee editor role on doc.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/docs/{doc_id}/grants/{principal}"))
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"role": "editor", "inherit": false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "owner PUT grant → 204");

    // GET grants: the newly created grant should appear.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/docs/{doc_id}/grants"))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "GET grants → 200");
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let arr: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let grants = arr.as_array().unwrap();
    assert!(
        grants
            .iter()
            .any(|g| g["principal"] == principal && g["role"] == "editor"),
        "grant should appear in list: {arr}",
    );

    // DELETE grant: owner removes the grant.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/docs/{doc_id}/grants/{principal}"))
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::NO_CONTENT,
        "owner DELETE grant → 204"
    );

    // GET grants: the grant should be gone.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/docs/{doc_id}/grants"))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let arr: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let grants = arr.as_array().unwrap();
    assert!(
        !grants.iter().any(|g| g["principal"] == principal),
        "deleted grant should not appear: {arr}",
    );
}

// ---------------------------------------------------------------------------
// Test 2: editor cannot PUT a grant (403 acl.owner_required)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn editor_cannot_put_grant() {
    let (state, owner_joined) = login_state("owner@grants2.test", "pass1234").await;
    let (owner_cookie, owner_csrf) = split_cookie_csrf(&owner_joined);
    let app = router_with_state(state.clone());

    // Create a doc as owner.
    let doc_id = create_doc(app.clone(), owner_cookie, owner_csrf, "Editor Gate Doc").await;

    // Invite an editor.
    let editor_id = invite_user(
        app.clone(),
        owner_cookie,
        owner_csrf,
        "editor@grants2.test",
        "editorpass",
        "editor",
        &state,
    )
    .await;

    // Log in as the editor.
    let editor_joined = login_as(app.clone(), "editor@grants2.test", "editorpass").await;
    let (editor_cookie, editor_csrf) = split_cookie_csrf(&editor_joined);

    // Try to PUT a grant as an editor — should get 403.
    let other_principal = format!("user:{editor_id}"); // self-grant attempt
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/docs/{doc_id}/grants/{other_principal}"))
                .header("cookie", editor_cookie)
                .header("x-csrf-token", editor_csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"role": "editor", "inherit": false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "editor PUT grant → 403");
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["error"]["code"], "acl.owner_required",
        "error code must be acl.owner_required, got: {v}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: viewer cannot PUT a grant (403 acl.owner_required)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_put_grant() {
    let (state, owner_joined) = login_state("owner@grants3.test", "pass1234").await;
    let (owner_cookie, owner_csrf) = split_cookie_csrf(&owner_joined);
    let app = router_with_state(state.clone());

    // Create a doc as owner.
    let doc_id = create_doc(app.clone(), owner_cookie, owner_csrf, "Viewer Gate Doc").await;

    // Invite a viewer.
    let viewer_id = invite_user(
        app.clone(),
        owner_cookie,
        owner_csrf,
        "viewer@grants3.test",
        "viewerpass",
        "viewer",
        &state,
    )
    .await;

    // Log in as the viewer.
    let viewer_joined = login_as(app.clone(), "viewer@grants3.test", "viewerpass").await;
    let (viewer_cookie, viewer_csrf) = split_cookie_csrf(&viewer_joined);

    // Try to PUT a grant as a viewer — should get 403.
    let some_principal = format!("user:{viewer_id}");
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/docs/{doc_id}/grants/{some_principal}"))
                .header("cookie", viewer_cookie)
                .header("x-csrf-token", viewer_csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"role": "viewer", "inherit": false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "viewer PUT grant → 403");
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["error"]["code"], "acl.owner_required",
        "error code must be acl.owner_required, got: {v}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: group: principal rejected with 422 grant.group_unsupported
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn group_principal_rejected_with_422() {
    let (state, joined) = login_state("owner@grants4.test", "pass1234").await;
    let (cookie, csrf) = split_cookie_csrf(&joined);
    let app = router_with_state(state.clone());

    // Create a doc.
    let doc_id = create_doc(app.clone(), cookie, csrf, "Group Doc").await;

    // PUT with a group: principal — handler should reject with 422.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/docs/{doc_id}/grants/group:eng"))
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"role": "editor", "inherit": false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "group: principal PUT → 422"
    );
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["error"]["code"], "grant.group_unsupported",
        "error code must be grant.group_unsupported, got: {v}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: unknown / cross-workspace doc returns 403 (no_grant via ACL middleware)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread")]
async fn unknown_doc_returns_403_or_404() {
    let (state, joined) = login_state("owner@grants5.test", "pass1234").await;
    let (cookie, csrf) = split_cookie_csrf(&joined);
    let app = router_with_state(state.clone());

    // Invite a real user to use as a grantee UUID.
    let other_id = invite_user(
        app.clone(),
        cookie,
        csrf,
        "nobody@grants5.test",
        "nopass123",
        "viewer",
        &state,
    )
    .await;
    let principal = format!("user:{other_id}");

    // Use a random UUID that doesn't correspond to any doc.
    let random_doc = uuid::Uuid::new_v4();

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/docs/{random_doc}/grants/{principal}"))
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"role": "editor", "inherit": false}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    // The require_doc_role_mw returns 403 (acl.no_grant) when the doc is unknown.
    assert!(
        r.status() == StatusCode::FORBIDDEN || r.status() == StatusCode::NOT_FOUND,
        "unknown doc PUT grant → 403 or 404, got {}",
        r.status()
    );
}
