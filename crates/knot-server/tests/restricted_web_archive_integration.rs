//! `GET /api/workspace/archive` rows for restricted pages: what the web
//! client needs to say where a page was and whether it may purge it.
//! - `parent_hidden` when the parent is a page the caller can't read
//! - `effective_role` the caller's role on the page, not the workspace role
//! - `subpage_ids` the archived subpages the caller can open
//!
//! See docs/superpowers/specs/2026-10-30-restricted-documents-design.md.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers, copied from restricted_integration.rs.
// ---------------------------------------------------------------------------

/// Build an AppState with a seeded workspace + owner, log the owner in via
/// HTTP, and return the `(state, "sid=…; csrf=…|<csrf_token>")` pair.
async fn login_state(email: &str, password: &str) -> (AppState, String) {
    let pool = knot_test_support::fresh_db().await.pool;

    let mut state = AppState::with_pool(pool);
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = state.hasher.hash(password).unwrap();
    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local(email, "U", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    let app = router_with_state(state.clone());
    let joined = login_as(app, email, password).await;
    (state, joined)
}

fn split_cookie_csrf(joined: &str) -> (&str, &str) {
    joined.split_once('|').unwrap()
}

/// Invite a user to the workspace with `role` through
/// `POST /api/workspace/members` and return their id.
async fn invite_user(
    app: axum::Router,
    owner_joined: &str,
    email: &str,
    password: &str,
    role: &str,
    state: &AppState,
) -> String {
    let (status, _) = send(
        app,
        "POST",
        "/api/workspace/members",
        owner_joined,
        Some(json!({"email": email, "role": role, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "invite {email} as {role}");
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
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "login {email}");
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect();
    let cookie_pair = |name: &str| {
        cookies
            .iter()
            .find(|cookie| cookie.starts_with(&format!("{name}=")))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    };
    let sid_kv = cookie_pair("sid");
    let csrf_kv = cookie_pair("csrf");
    let csrf_token = csrf_kv.trim_start_matches("csrf=").to_string();
    format!("{sid_kv}; {csrf_kv}|{csrf_token}")
}

/// Create a doc via HTTP, under `parent` when given, and return its id.
async fn create_doc(app: axum::Router, joined: &str, title: &str, parent: Option<&str>) -> String {
    let (status, body) = send(
        app,
        "POST",
        "/api/docs",
        joined,
        Some(json!({"title": title, "parent_id": parent})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create doc {title}");
    let created: Value = serde_json::from_slice(&body).unwrap();
    created["id"].as_str().unwrap().to_string()
}

/// One request as the session in `joined` (`None` sends no cookie).
async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    joined: &str,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let (cookie, csrf) = split_cookie_csrf(joined);
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie)
        .header("x-csrf-token", csrf);
    let request_body = match body {
        Some(json_body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(json_body.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .oneshot(builder.body(request_body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes)
}

fn json_of(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_| {
        panic!(
            "expected JSON, got {:?}",
            String::from_utf8_lossy(bytes.as_ref())
        )
    })
}

async fn set_restricted(
    app: axum::Router,
    joined: &str,
    doc_id: &str,
    restricted: bool,
) -> (StatusCode, Bytes) {
    send(
        app,
        "PUT",
        &format!("/api/docs/{doc_id}/restricted"),
        joined,
        Some(json!({"restricted": restricted})),
    )
    .await
}

async fn put_grant(
    app: axum::Router,
    joined: &str,
    doc_id: &str,
    user_id: &str,
    role: &str,
    inherit: bool,
) -> (StatusCode, Bytes) {
    send(
        app,
        "PUT",
        &format!("/api/docs/{doc_id}/grants/user:{user_id}"),
        joined,
        Some(json!({"role": role, "inherit": inherit})),
    )
    .await
}

/// Alice is the workspace owner; Bob is invited with `bob_role`.
struct Fixture {
    app: axum::Router,
    alice: String,
    bob_id: String,
    bob: String,
}

async fn fixture(bob_role: &str) -> Fixture {
    let (state, alice) = login_state("alice@restricted.test", "alicepass").await;
    let app = router_with_state(state.clone());
    let bob_id = invite_user(
        app.clone(),
        &alice,
        "bob@restricted.test",
        "bobpass1",
        bob_role,
        &state,
    )
    .await;
    let bob = login_as(app.clone(), "bob@restricted.test", "bobpass1").await;
    Fixture {
        app,
        alice,
        bob_id,
        bob,
    }
}

/// Archive `doc_id` as the session in `joined`.
async fn archive_doc(app: axum::Router, joined: &str, doc_id: &str) {
    let (status, body) = send(app, "DELETE", &format!("/api/docs/{doc_id}"), joined, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "archive {doc_id}: {body:?}");
}

/// `GET /api/workspace/archive` as `joined`: the rows and the raw body.
async fn list_archive(app: axum::Router, joined: &str) -> (Vec<Value>, String) {
    let (status, body) = send(app, "GET", "/api/workspace/archive", joined, None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body_text = String::from_utf8_lossy(body.as_ref()).to_string();
    let rows = json_of(&body).as_array().unwrap().clone();
    (rows, body_text)
}

fn archive_row<'a>(rows: &'a [Value], doc_id: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["id"] == doc_id)
        .unwrap_or_else(|| panic!("{doc_id} not in archive: {rows:?}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn archive_row_under_hidden_parent_says_so_without_naming_it() {
    let fx = fixture("editor").await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Shared page", Some(&boundary)).await;
    let grandchild = create_doc(fx.app.clone(), &fx.alice, "Shared subpage", Some(&child)).await;
    let top_level = create_doc(fx.app.clone(), &fx.alice, "Top page", None).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &child,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    archive_doc(fx.app.clone(), &fx.alice, &child).await;
    archive_doc(fx.app.clone(), &fx.alice, &top_level).await;

    let (rows, body_text) = list_archive(fx.app.clone(), &fx.bob).await;

    let child_row = archive_row(&rows, &child);
    assert!(child_row["parent_id"].is_null(), "{child_row}");
    assert!(child_row["parent_title"].is_null(), "{child_row}");
    assert_eq!(child_row["parent_hidden"], true, "{child_row}");
    assert_eq!(child_row["subpage_ids"], json!([grandchild]));
    let top_row = archive_row(&rows, &top_level);
    assert_eq!(top_row["parent_hidden"], false, "{top_row}");
    assert!(
        !body_text.contains(&boundary),
        "boundary id leaked: {body_text}"
    );
    assert!(
        !body_text.contains("Secret folder"),
        "boundary title leaked: {body_text}"
    );
    // Alice can read the parent: it is named, not hidden.
    let (rows, _) = list_archive(fx.app.clone(), &fx.alice).await;
    let child_row = archive_row(&rows, &child);
    assert_eq!(child_row["parent_id"], boundary);
    assert_eq!(child_row["parent_title"], "Secret folder");
    assert_eq!(child_row["parent_hidden"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_row_role_is_the_grant_for_a_workspace_owner() {
    let fx = fixture("owner").await;
    let restricted = create_doc(fx.app.clone(), &fx.alice, "Restricted page", None).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &restricted, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &restricted,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let open_page = create_doc(fx.app.clone(), &fx.alice, "Open page", None).await;
    archive_doc(fx.app.clone(), &fx.alice, &restricted).await;
    archive_doc(fx.app.clone(), &fx.alice, &open_page).await;

    let (rows, _) = list_archive(fx.app.clone(), &fx.bob).await;

    assert_eq!(archive_row(&rows, &restricted)["effective_role"], "editor");
    assert_eq!(archive_row(&rows, &open_page)["effective_role"], "owner");
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_row_role_is_the_grant_for_a_workspace_editor() {
    let fx = fixture("editor").await;
    let restricted = create_doc(fx.app.clone(), &fx.alice, "Restricted page", None).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &restricted, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &restricted,
        &fx.bob_id,
        "owner",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    archive_doc(fx.app.clone(), &fx.alice, &restricted).await;

    let (rows, _) = list_archive(fx.app.clone(), &fx.bob).await;

    assert_eq!(archive_row(&rows, &restricted)["effective_role"], "owner");
}
