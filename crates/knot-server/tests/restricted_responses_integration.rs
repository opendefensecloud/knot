//! HTTP-layer tests for restricted documents: what single-doc responses
//! and the hidden-descendants guard reveal.
//! - every route answering with one doc nulls a parent the caller can't
//!   read, so a deep grantee never learns the hidden boundary's id
//! - archive and purge treat a page the caller can't read as hidden, an
//!   archived page they only view included
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

async fn user_id_of(state: &AppState, email: &str) -> String {
    state
        .users
        .as_ref()
        .unwrap()
        .find_by_email(email)
        .await
        .unwrap()
        .unwrap()
        .id
        .to_string()
}

/// Alice is the workspace owner; Bob is invited with `bob_role`.
struct Fixture {
    state: AppState,
    app: axum::Router,
    alice: String,
    alice_id: String,
    bob: String,
    bob_id: String,
}

async fn fixture(bob_role: &str) -> Fixture {
    let (state, alice) = login_state("alice@restricted.test", "alicepass").await;
    let app = router_with_state(state.clone());
    let alice_id = user_id_of(&state, "alice@restricted.test").await;
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
        state,
        app,
        alice,
        alice_id,
        bob,
        bob_id,
    }
}

fn error_code(body: &Bytes) -> String {
    json_of(body)["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Alice's restricted folder "Secret folder" with one page in it. Bob, a
/// workspace editor, holds a non-inheriting `bob_grant` on the page only,
/// so he can read the page but not its parent.
struct DeepGrantee {
    fx: Fixture,
    boundary: String,
    child: String,
}

async fn deep_grantee(bob_grant: &str) -> DeepGrantee {
    let fx = fixture("editor").await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Shared page", Some(&boundary)).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &child,
        &fx.bob_id,
        bob_grant,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    DeepGrantee {
        fx,
        boundary,
        child,
    }
}

/// `body` is a 200 single-doc response for `deep.child` that names
/// neither the hidden boundary's id nor its title.
fn assert_boundary_not_named(deep: &DeepGrantee, status: StatusCode, body: &Bytes) {
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(body));
    let doc_view = json_of(body);
    assert_eq!(doc_view["id"], deep.child.as_str());
    assert!(doc_view["parent_id"].is_null(), "{doc_view}");
    let text = String::from_utf8_lossy(body.as_ref());
    assert!(!text.contains(&deep.boundary), "boundary id leaked: {text}");
    assert!(
        !text.contains("Secret folder"),
        "boundary title leaked: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deep_grantee_rename_does_not_learn_boundary() {
    let deep = deep_grantee("editor").await;

    let (status, body) = send(
        deep.fx.app.clone(),
        "PATCH",
        &format!("/api/docs/{}", deep.child),
        &deep.fx.bob,
        Some(json!({"title": "Renamed", "icon": "📄"})),
    )
    .await;

    assert_boundary_not_named(&deep, status, &body);
    assert_eq!(json_of(&body)["title"], "Renamed");
}

#[tokio::test(flavor = "multi_thread")]
async fn deep_grantee_set_template_does_not_learn_boundary() {
    let deep = deep_grantee("owner").await;

    let (status, body) = send(
        deep.fx.app.clone(),
        "POST",
        &format!("/api/docs/{}/template", deep.child),
        &deep.fx.bob,
        Some(json!({"is_template": true})),
    )
    .await;

    assert_boundary_not_named(&deep, status, &body);
    assert_eq!(json_of(&body)["is_template"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn deep_grantee_set_restricted_does_not_learn_boundary() {
    let deep = deep_grantee("owner").await;

    let (status, body) = set_restricted(deep.fx.app.clone(), &deep.fx.bob, &deep.child, true).await;

    assert_boundary_not_named(&deep, status, &body);
    assert_eq!(json_of(&body)["restricted"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn write_responses_keep_a_readable_parent() {
    let deep = deep_grantee("owner").await;
    let alice = &deep.fx.alice;
    let child_uri = format!("/api/docs/{}", deep.child);

    let (status, body) = send(
        deep.fx.app.clone(),
        "PATCH",
        &child_uri,
        alice,
        Some(json!({"title": "Renamed"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["parent_id"], deep.boundary.as_str());

    let (status, body) = send(
        deep.fx.app.clone(),
        "POST",
        &format!("{child_uri}/template"),
        alice,
        Some(json!({"is_template": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["parent_id"], deep.boundary.as_str());

    let (status, body) = set_restricted(deep.fx.app.clone(), alice, &deep.child, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["parent_id"], deep.boundary.as_str());

    let (status, body) = send(
        deep.fx.app.clone(),
        "POST",
        &format!("{child_uri}/duplicate"),
        alice,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json_of(&body)["parent_id"], deep.boundary.as_str());

    let sibling = create_doc(deep.fx.app.clone(), alice, "Sibling", Some(&deep.boundary)).await;
    let (status, body) = send(
        deep.fx.app.clone(),
        "POST",
        &format!("/api/docs/{sibling}/move"),
        alice,
        Some(json!({"parent_id": deep.child})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["parent_id"], deep.child.as_str());
}

/// Bob, a workspace owner, keeps a restricted page under a top-level page
/// and lets Alice (also a workspace owner) view it: Alice owns the
/// top-level page but can only view the restricted one.
struct ViewerBelow {
    fx: Fixture,
    top: String,
    restricted: String,
    inner: String,
}

async fn viewer_below() -> ViewerBelow {
    let fx = fixture("owner").await;
    let top = create_doc(fx.app.clone(), &fx.bob, "Top", None).await;
    let restricted = create_doc(fx.app.clone(), &fx.bob, "Restricted", Some(&top)).await;
    let inner = create_doc(fx.app.clone(), &fx.bob, "Inner", Some(&restricted)).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.bob, &restricted, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.bob,
        &restricted,
        &fx.alice_id,
        "viewer",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    ViewerBelow {
        fx,
        top,
        restricted,
        inner,
    }
}

async fn doc_exists(state: &AppState, doc_id: &str) -> bool {
    state
        .docs
        .as_ref()
        .unwrap()
        .get(doc_id.parse().unwrap())
        .await
        .unwrap()
        .is_some()
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_with_archived_viewer_only_descendant_is_409() {
    let world = viewer_below().await;
    let fx = &world.fx;
    // Alice can read the live restricted page, so she may archive it with
    // its parent; archived, it is out of a viewer's sight.
    let (status, body) = send(
        fx.app.clone(),
        "DELETE",
        &format!("/api/docs/{}", world.top),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
    let purge_uri = format!("/api/docs/{}/purge", world.top);

    let (status, body) = send(fx.app.clone(), "POST", &purge_uri, &fx.alice, None).await;

    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.hidden_descendants".to_string()),
    );
    assert!(doc_exists(&fx.state, &world.restricted).await);
    assert!(doc_exists(&fx.state, &world.inner).await);
    let (status, body) = send(fx.app.clone(), "POST", &purge_uri, &fx.bob, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
    assert!(!doc_exists(&fx.state, &world.restricted).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_over_archived_viewer_only_descendant_is_409() {
    let world = viewer_below().await;
    let fx = &world.fx;
    let (status, _) = send(
        fx.app.clone(),
        "DELETE",
        &format!("/api/docs/{}", world.inner),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(
        fx.app.clone(),
        "DELETE",
        &format!("/api/docs/{}", world.top),
        &fx.alice,
        None,
    )
    .await;

    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.hidden_descendants".to_string()),
    );
}
