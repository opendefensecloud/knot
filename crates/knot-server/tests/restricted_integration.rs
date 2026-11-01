//! HTTP-layer tests for restricted documents (private subtrees):
//! - PUT /api/docs/{id}/restricted   the toggle (owner only)
//! - GET /api/docs/{id}               restriction metadata, hidden parents
//! - GET /api/docs/{id}/access        "People with access"
//! - grant guards: last owner of a restricted doc, non-member principals
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
// Helpers, copied from grants_integration.rs.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn owner_restricts_and_member_loses_access() {
    let fx = fixture("editor").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Private", None).await;

    let (status, _) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bob reads before the restriction");

    let (status, body) = set_restricted(fx.app.clone(), &fx.alice, &doc_id, true).await;
    assert_eq!(status, StatusCode::OK);
    let toggled = json_of(&body);
    assert_eq!(toggled["restricted"], true);
    assert_eq!(toggled["id"], doc_id);

    let (bob_status, bob_body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}"),
        &fx.bob,
        None,
    )
    .await;
    let random_doc = uuid::Uuid::new_v4();
    let (random_status, random_body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{random_doc}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(bob_status, StatusCode::FORBIDDEN);
    assert_eq!(random_status, StatusCode::FORBIDDEN);
    assert_eq!(
        bob_body, random_body,
        "a restricted doc answers exactly like an unknown id"
    );

    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}"),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let alice_view = json_of(&body);
    assert_eq!(alice_view["restricted"], true);
    assert_eq!(alice_view["within_restricted"], true);
    assert_eq!(alice_view["restriction_root_id"], doc_id);
    assert_eq!(alice_view["effective_role"], "owner");
}

#[tokio::test(flavor = "multi_thread")]
async fn restricting_grants_actor_owner() {
    let fx = fixture("editor").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Private", None).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &doc_id, true).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}/grants"),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let grants = json_of(&body);
    let alice_principal = format!("user:{}", fx.alice_id);
    assert!(
        grants.as_array().unwrap().iter().any(|grant| {
            grant["principal"] == alice_principal.as_str()
                && grant["role"] == "owner"
                && grant["inherit"] == true
        }),
        "restricting grants the actor owner, inheriting: {grants}"
    );

    // Idempotent: a second restrict keeps one owner grant and answers 200.
    let (status, body) = set_restricted(fx.app.clone(), &fx.alice, &doc_id, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["restricted"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_cannot_restrict() {
    let fx = fixture("viewer").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Shared", None).await;
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &doc_id,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = set_restricted(fx.app.clone(), &fx.bob, &doc_id, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json_of(&body)["error"]["code"], "acl.owner_required");

    // A workspace editor without a grant can't either.
    let fx = fixture("editor").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Shared", None).await;
    let (status, body) = set_restricted(fx.app.clone(), &fx.bob, &doc_id, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json_of(&body)["error"]["code"], "acl.owner_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn last_owner_grant_cannot_be_removed() {
    let fx = fixture("editor").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Private", None).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &doc_id, true).await;
    assert_eq!(status, StatusCode::OK);
    let alice_grant_uri = format!("/api/docs/{doc_id}/grants/user:{}", fx.alice_id);

    let (status, body) = send(fx.app.clone(), "DELETE", &alice_grant_uri, &fx.alice, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json_of(&body)["error"]["code"], "grant.last_owner");

    // Downgrading the last owner is refused the same way.
    let (status, body) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &doc_id,
        &fx.alice_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json_of(&body)["error"]["code"], "grant.last_owner");

    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &doc_id,
        &fx.bob_id,
        "owner",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(fx.app.clone(), "DELETE", &alice_grant_uri, &fx.alice, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "another owner remains");
}

#[tokio::test(flavor = "multi_thread")]
async fn grant_to_non_member_is_422() {
    let fx = fixture("editor").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Doc", None).await;
    let stranger = uuid::Uuid::new_v4().to_string();
    let (status, body) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &doc_id,
        &stranger,
        "viewer",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(&body)["error"]["code"], "grant.not_member");

    // A member removed from the workspace is not one either.
    let workspaces = fx.state.workspaces.as_ref().unwrap();
    let workspace = workspaces.get_singleton().await.unwrap().unwrap();
    workspaces
        .remove_member(workspace.id, fx.bob_id.parse().unwrap())
        .await
        .unwrap();
    let (status, body) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &doc_id,
        &fx.bob_id,
        "viewer",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_of(&body)["error"]["code"], "grant.not_member");
}

#[tokio::test(flavor = "multi_thread")]
async fn restrict_revokes_share_links_in_subtree() {
    let fx = fixture("editor").await;
    let parent = create_doc(fx.app.clone(), &fx.alice, "Parent", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Child", Some(&parent)).await;
    let (status, body) = send(
        fx.app.clone(),
        "POST",
        &format!("/api/docs/{child}/shares"),
        &fx.alice,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = json_of(&body)["token"].as_str().unwrap().to_string();

    let public_get = |app: axum::Router, token: String| async move {
        app.oneshot(
            Request::builder()
                .uri(format!("/p/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    };
    // Nothing rendered yet: a 503 placeholder, but the link is alive.
    assert_ne!(
        public_get(fx.app.clone(), token.clone()).await,
        StatusCode::GONE
    );

    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &parent, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(public_get(fx.app.clone(), token).await, StatusCode::GONE);
}

#[tokio::test(flavor = "multi_thread")]
async fn deep_grantee_does_not_learn_boundary() {
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
        "viewer",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, doc_body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{child}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let doc_view = json_of(&doc_body);
    assert_eq!(doc_view["effective_role"], "viewer");
    assert_eq!(doc_view["within_restricted"], true);
    assert_eq!(doc_view["restricted"], false);
    assert!(doc_view["parent_id"].is_null(), "{doc_view}");
    assert!(doc_view["restriction_root_id"].is_null(), "{doc_view}");

    let (status, access_body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{child}/access"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let access = json_of(&access_body);
    assert!(access["restriction_root_id"].is_null(), "{access}");
    assert!(access["restriction_root_title"].is_null(), "{access}");
    // Alice's owner grant on the boundary applies; Bob learns who, not where.
    let alice_principal = format!("user:{}", fx.alice_id);
    let alice_row = access["people"]
        .as_array()
        .unwrap()
        .iter()
        .find(|person| person["principal"] == alice_principal.as_str())
        .unwrap_or_else(|| panic!("alice listed: {access}"));
    assert!(alice_row["source_doc_id"].is_null(), "{access}");
    assert!(alice_row["source_title"].is_null(), "{access}");

    for body in [&doc_body, &access_body] {
        let text = String::from_utf8_lossy(body.as_ref());
        assert!(!text.contains(&boundary), "boundary id leaked: {text}");
        assert!(
            !text.contains("Secret folder"),
            "boundary title leaked: {text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn access_lists_people_up_to_boundary() {
    let fx = fixture("viewer").await;
    let carol_id = invite_user(
        fx.app.clone(),
        &fx.alice,
        "carol@restricted.test",
        "carolpass",
        "viewer",
        &fx.state,
    )
    .await;
    let root = create_doc(fx.app.clone(), &fx.alice, "Root", None).await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Boundary", Some(&root)).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Child", Some(&boundary)).await;
    // Above the boundary: must not be listed below it.
    let (status, _) = put_grant(fx.app.clone(), &fx.alice, &root, &carol_id, "editor", true).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);
    // On the boundary, inheriting: listed on the child with its source.
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &boundary,
        &fx.bob_id,
        "viewer",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{child}/access"),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let access = json_of(&body);
    assert_eq!(access["restricted"], false);
    assert_eq!(access["restriction_root_id"], boundary);
    assert_eq!(access["restriction_root_title"], "Boundary");
    let people = access["people"].as_array().unwrap();
    let carol_principal = format!("user:{carol_id}");
    assert!(
        people
            .iter()
            .all(|person| person["principal"] != carol_principal.as_str()),
        "a grant above the boundary does not apply: {access}"
    );
    for (principal, role) in [
        (format!("user:{}", fx.alice_id), "owner"),
        (format!("user:{}", fx.bob_id), "viewer"),
    ] {
        let row = people
            .iter()
            .find(|person| person["principal"] == principal.as_str())
            .unwrap_or_else(|| panic!("{principal} listed: {access}"));
        assert_eq!(row["role"], role);
        assert_eq!(row["inherit"], true);
        assert_eq!(row["source_doc_id"], boundary);
        assert_eq!(row["source_title"], "Boundary");
    }

    // Outside any restriction, ancestors' inheriting grants are listed.
    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{root}/access"),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let access = json_of(&body);
    assert_eq!(access["restricted"], false);
    assert!(access["restriction_root_id"].is_null());
    assert!(access["people"].as_array().unwrap().iter().any(|person| {
        person["principal"] == carol_principal.as_str() && person["source_doc_id"] == root
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn unrestrict_restores_workspace_access() {
    let fx = fixture("editor").await;
    let parent = create_doc(fx.app.clone(), &fx.alice, "Parent", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Child", Some(&parent)).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &parent, true).await;
    assert_eq!(status, StatusCode::OK);
    for doc_id in [&parent, &child] {
        let (status, _) = send(
            fx.app.clone(),
            "GET",
            &format!("/api/docs/{doc_id}"),
            &fx.bob,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    let (status, body) = set_restricted(fx.app.clone(), &fx.alice, &parent, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["restricted"], false);
    for doc_id in [&parent, &child] {
        let (status, body) = send(
            fx.app.clone(),
            "GET",
            &format!("/api/docs/{doc_id}"),
            &fx.bob,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let view = json_of(&body);
        assert_eq!(view["within_restricted"], false);
        assert_eq!(view["effective_role"], "editor");
        assert!(view["restriction_root_id"].is_null());
    }
    // The child's parent is readable again, so it is named.
    let (_, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{child}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(json_of(&body)["parent_id"], parent);
}

// ---------------------------------------------------------------------------
// GET /api/docs: the sidebar list, filtered by visibility
// ---------------------------------------------------------------------------

/// `GET /api/docs` as the session in `joined`, plus the raw body for
/// leak checks.
async fn list_docs(app: axum::Router, joined: &str) -> (Vec<Value>, String) {
    let (status, body) = send(app, "GET", "/api/docs", joined, None).await;
    assert_eq!(status, StatusCode::OK, "list docs");
    let listed = json_of(&body).as_array().unwrap().clone();
    (listed, String::from_utf8_lossy(body.as_ref()).into_owned())
}

fn listed_entry<'a>(listed: &'a [Value], doc_id: &str) -> Option<&'a Value> {
    listed.iter().find(|entry| entry["id"] == doc_id)
}

#[tokio::test(flavor = "multi_thread")]
async fn list_hides_restricted_subtree_from_non_grantee() {
    let fx = fixture("editor").await;
    let open_doc = create_doc(fx.app.clone(), &fx.alice, "Open", None).await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Secret page", Some(&boundary)).await;

    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    assert!(
        listed_entry(&listed, &boundary).is_some(),
        "listed before the restriction"
    );

    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);

    let (listed, body_text) = list_docs(fx.app.clone(), &fx.bob).await;
    let open_entry = listed_entry(&listed, &open_doc).expect("open doc listed");
    assert_eq!(open_entry["effective_role"], "editor");
    assert_eq!(open_entry["within_restricted"], false);
    assert_eq!(open_entry["parent_hidden"], false);
    for hidden_id in [&boundary, &child] {
        assert!(
            !body_text.contains(hidden_id.as_str()),
            "{hidden_id} leaked: {body_text}"
        );
    }
    assert!(!body_text.contains("Secret"), "title leaked: {body_text}");

    // The owner still sees the whole subtree, flagged.
    let (listed, _) = list_docs(fx.app.clone(), &fx.alice).await;
    let boundary_entry = listed_entry(&listed, &boundary).expect("boundary listed");
    assert_eq!(boundary_entry["restricted"], true);
    assert_eq!(boundary_entry["within_restricted"], true);
    assert_eq!(boundary_entry["effective_role"], "owner");
    let child_entry = listed_entry(&listed, &child).expect("child listed");
    assert_eq!(child_entry["restricted"], false);
    assert_eq!(child_entry["within_restricted"], true);
    assert_eq!(child_entry["parent_id"], boundary);
    assert_eq!(child_entry["parent_hidden"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_hoists_deep_grantee_doc_with_parent_hidden() {
    let fx = fixture("editor").await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Shared page", Some(&boundary)).await;
    let grandchild = create_doc(fx.app.clone(), &fx.alice, "Shared subpage", Some(&child)).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &child,
        &fx.bob_id,
        "viewer",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (listed, body_text) = list_docs(fx.app.clone(), &fx.bob).await;
    let child_entry = listed_entry(&listed, &child).expect("granted child listed");
    assert!(child_entry["parent_id"].is_null(), "{child_entry}");
    assert_eq!(child_entry["parent_hidden"], true);
    assert_eq!(child_entry["within_restricted"], true);
    assert_eq!(child_entry["restricted"], false);
    assert_eq!(child_entry["effective_role"], "viewer");
    // Below the hoisted doc, the tree is intact.
    let grandchild_entry = listed_entry(&listed, &grandchild).expect("grandchild listed");
    assert_eq!(grandchild_entry["parent_id"], child);
    assert_eq!(grandchild_entry["parent_hidden"], false);
    assert_eq!(grandchild_entry["within_restricted"], true);

    assert!(
        !body_text.contains(&boundary),
        "boundary id leaked: {body_text}"
    );
    assert!(
        !body_text.contains("Secret folder"),
        "boundary title leaked: {body_text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deep_grantee_template_does_not_learn_boundary() {
    let fx = fixture("editor").await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let child = create_doc(
        fx.app.clone(),
        &fx.alice,
        "Shared template",
        Some(&boundary),
    )
    .await;
    let (status, _) = send(
        fx.app.clone(),
        "POST",
        &format!("/api/docs/{child}/template"),
        &fx.alice,
        Some(json!({"is_template": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &child,
        &fx.bob_id,
        "viewer",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(
        fx.app.clone(),
        "GET",
        "/api/workspace/templates",
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let templates = json_of(&body).as_array().unwrap().clone();
    let template_entry = listed_entry(&templates, &child).expect("granted template listed");
    assert!(template_entry["parent_id"].is_null(), "{template_entry}");

    let body_text = String::from_utf8_lossy(body.as_ref());
    assert!(
        !body_text.contains(&boundary),
        "boundary id leaked: {body_text}"
    );
    assert!(
        !body_text.contains("Secret folder"),
        "boundary title leaked: {body_text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_boundary_hides_inner_from_outer_grantee() {
    let fx = fixture("editor").await;
    let outer = create_doc(fx.app.clone(), &fx.alice, "Outer", None).await;
    let inner = create_doc(fx.app.clone(), &fx.alice, "Inner", Some(&outer)).await;
    let inner_child = create_doc(fx.app.clone(), &fx.alice, "Inner child", Some(&inner)).await;
    let outer_child = create_doc(fx.app.clone(), &fx.alice, "Outer child", Some(&outer)).await;
    for boundary in [&outer, &inner] {
        let (status, _) = set_restricted(fx.app.clone(), &fx.alice, boundary, true).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &outer,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (listed, body_text) = list_docs(fx.app.clone(), &fx.bob).await;
    let outer_entry = listed_entry(&listed, &outer).expect("outer listed");
    assert_eq!(outer_entry["effective_role"], "editor");
    assert_eq!(outer_entry["parent_hidden"], false);
    let outer_child_entry = listed_entry(&listed, &outer_child).expect("outer child listed");
    assert_eq!(outer_child_entry["parent_id"], outer);
    assert_eq!(outer_child_entry["within_restricted"], true);
    for hidden_id in [&inner, &inner_child] {
        assert!(
            !body_text.contains(hidden_id.as_str()),
            "{hidden_id} leaked: {body_text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn list_reports_per_doc_effective_role() {
    let fx = fixture("viewer").await;
    let granted = create_doc(fx.app.clone(), &fx.alice, "Granted", None).await;
    let plain = create_doc(fx.app.clone(), &fx.alice, "Plain", None).await;
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &granted,
        &fx.bob_id,
        "editor",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    let granted_entry = listed_entry(&listed, &granted).expect("granted listed");
    assert_eq!(granted_entry["effective_role"], "editor");
    assert_eq!(granted_entry["within_restricted"], false);
    assert_eq!(granted_entry["parent_hidden"], false);
    let plain_entry = listed_entry(&listed, &plain).expect("plain listed");
    assert_eq!(plain_entry["effective_role"], "viewer");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_shows_docs_created_after_visibility_was_cached() {
    let fx = fixture("editor").await;
    let first = create_doc(fx.app.clone(), &fx.alice, "First", None).await;
    for joined in [&fx.alice, &fx.bob] {
        let (listed, _) = list_docs(fx.app.clone(), joined).await;
        assert!(listed_entry(&listed, &first).is_some());
    }

    // Created after both callers' visibility was cached.
    let second = create_doc(fx.app.clone(), &fx.alice, "Second", None).await;
    let nested = create_doc(fx.app.clone(), &fx.bob, "Nested", Some(&second)).await;
    for joined in [&fx.alice, &fx.bob] {
        let (listed, _) = list_docs(fx.app.clone(), joined).await;
        assert!(listed_entry(&listed, &second).is_some(), "new doc listed");
        let nested_entry = listed_entry(&listed, &nested).expect("new child listed");
        assert_eq!(nested_entry["parent_id"], second);
        assert_eq!(nested_entry["parent_hidden"], false);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn templates_cover_parents_created_after_visibility_was_cached() {
    let fx = fixture("editor").await;
    let template = create_doc(fx.app.clone(), &fx.alice, "Template", None).await;
    let (status, _) = send(
        fx.app.clone(),
        "POST",
        &format!("/api/docs/{template}/template"),
        &fx.alice,
        Some(json!({"is_template": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list_templates = || {
        send(
            fx.app.clone(),
            "GET",
            "/api/workspace/templates",
            &fx.bob,
            None,
        )
    };
    let (status, body) = list_templates().await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed_entry(json_of(&body).as_array().unwrap(), &template).is_some());

    // A new parent, and a move under it that this replica never saw (as
    // when another replica made it): Bob's cached snapshot knows the
    // template but not its new parent, which the template list leaves out.
    let new_parent = create_doc(fx.app.clone(), &fx.alice, "New parent", None).await;
    sqlx::query("UPDATE documents SET parent_id = $1 WHERE id = $2")
        .bind(uuid::Uuid::parse_str(&new_parent).unwrap())
        .bind(uuid::Uuid::parse_str(&template).unwrap())
        .execute(fx.state.pool.as_ref().unwrap())
        .await
        .unwrap();

    let (status, body) = list_templates().await;
    assert_eq!(status, StatusCode::OK);
    let templates = json_of(&body).as_array().unwrap().clone();
    let template_entry = listed_entry(&templates, &template).expect("template listed");
    assert_eq!(
        template_entry["parent_id"], new_parent,
        "not taken for hidden"
    );
}

// ---------------------------------------------------------------------------
// Read-your-writes: grants and moves evict this replica's cached access
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn grant_and_revoke_take_effect_for_the_grantee_immediately() {
    let fx = fixture("editor").await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Secret page", Some(&boundary)).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);

    // Cache Bob's view: the subtree is hidden.
    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    assert!(listed_entry(&listed, &boundary).is_none());
    let child_uri = format!("/api/docs/{child}");
    let (status, _) = send(fx.app.clone(), "GET", &child_uri, &fx.bob, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &boundary,
        &fx.bob_id,
        "viewer",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    assert!(
        listed_entry(&listed, &boundary).is_some(),
        "granted boundary"
    );
    assert!(listed_entry(&listed, &child).is_some(), "inherited child");
    let (status, _) = send(fx.app.clone(), "GET", &child_uri, &fx.bob, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "child readable right after the grant"
    );

    let (status, _) = send(
        fx.app.clone(),
        "DELETE",
        &format!("/api/docs/{boundary}/grants/user:{}", fx.bob_id),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    assert!(
        listed_entry(&listed, &boundary).is_none(),
        "revoked boundary"
    );
    assert!(listed_entry(&listed, &child).is_none(), "revoked child");
    let (status, _) = send(fx.app.clone(), "GET", &child_uri, &fx.bob, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "child hidden right after");
}

#[tokio::test(flavor = "multi_thread")]
async fn move_into_restricted_subtree_hides_page_immediately() {
    let fx = fixture("editor").await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);
    let page = create_doc(fx.app.clone(), &fx.alice, "Page", None).await;
    let subpage = create_doc(fx.app.clone(), &fx.alice, "Subpage", Some(&page)).await;

    // Cache Bob's view: the page and its subpage are visible.
    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    assert!(listed_entry(&listed, &page).is_some());
    let subpage_uri = format!("/api/docs/{subpage}");
    let (status, _) = send(fx.app.clone(), "GET", &subpage_uri, &fx.bob, None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = move_doc(fx.app.clone(), &fx.alice, &page, Some(&boundary)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    assert!(listed_entry(&listed, &page).is_none(), "moved page hidden");
    assert!(listed_entry(&listed, &subpage).is_none(), "its subpage too");
    let (status, _) = send(fx.app.clone(), "GET", &subpage_uri, &fx.bob, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Create, from-template, import and move: destination and boundary rules
// ---------------------------------------------------------------------------

/// `POST /api/docs/{doc_id}/move` to the end of `parent` (top level when
/// `None`).
async fn move_doc(
    app: axum::Router,
    joined: &str,
    doc_id: &str,
    parent: Option<&str>,
) -> (StatusCode, Bytes) {
    send(
        app,
        "POST",
        &format!("/api/docs/{doc_id}/move"),
        joined,
        Some(json!({"parent_id": parent})),
    )
    .await
}

fn error_code(body: &Bytes) -> String {
    json_of(body)["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Alice's restricted folder with one page in it; Bob is invited with
/// `bob_role` and holds no grant yet.
struct RestrictedFolder {
    fx: Fixture,
    boundary: String,
    child: String,
}

async fn restricted_folder(bob_role: &str) -> RestrictedFolder {
    let fx = fixture(bob_role).await;
    let boundary = create_doc(fx.app.clone(), &fx.alice, "Secret folder", None).await;
    let child = create_doc(fx.app.clone(), &fx.alice, "Secret page", Some(&boundary)).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &boundary, true).await;
    assert_eq!(status, StatusCode::OK);
    RestrictedFolder {
        fx,
        boundary,
        child,
    }
}

async fn parent_of(state: &AppState, doc_id: &str) -> Option<uuid::Uuid> {
    sqlx::query_scalar("SELECT parent_id FROM documents WHERE id = $1")
        .bind(uuid::Uuid::parse_str(doc_id).unwrap())
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap()
}

async fn document_count(state: &AppState) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM documents")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn create_under_unreadable_restricted_parent_is_404() {
    let folder = restricted_folder("editor").await;
    let fx = &folder.fx;
    let before = document_count(&fx.state).await;

    let create_under = |parent: String| {
        send(
            fx.app.clone(),
            "POST",
            "/api/docs",
            &fx.bob,
            Some(json!({"title": "Sneaky", "parent_id": parent})),
        )
    };
    let (restricted_status, restricted_body) = create_under(folder.boundary.clone()).await;
    let (random_status, random_body) = create_under(uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(restricted_status, StatusCode::NOT_FOUND);
    assert_eq!(random_status, StatusCode::NOT_FOUND);
    assert_eq!(restricted_body, random_body, "no existence oracle");
    assert_eq!(error_code(&restricted_body), "doc.not_found");
    assert_eq!(document_count(&fx.state).await, before, "nothing created");
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_with_editor_grant_can_create_inside_restricted() {
    let folder = restricted_folder("viewer").await;
    let fx = &folder.fx;
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &folder.boundary,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let created = create_doc(
        fx.app.clone(),
        &fx.bob,
        "Bob's page",
        Some(&folder.boundary),
    )
    .await;
    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{created}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let created_view = json_of(&body);
    assert_eq!(created_view["effective_role"], "editor");
    assert_eq!(created_view["within_restricted"], true);

    // A workspace viewer still can't create at the top level.
    let (status, body) = send(
        fx.app.clone(),
        "POST",
        "/api/docs",
        &fx.bob,
        Some(json!({"title": "Top"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "acl.editor_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn creator_with_non_inheriting_grant_gets_owner_on_new_page() {
    let folder = restricted_folder("editor").await;
    let fx = &folder.fx;
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &folder.boundary,
        &fx.bob_id,
        "editor",
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let created = create_doc(
        fx.app.clone(),
        &fx.bob,
        "Bob's page",
        Some(&folder.boundary),
    )
    .await;
    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{created}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the creator can open the new page");
    assert_eq!(json_of(&body)["effective_role"], "owner");

    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{created}/grants"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let grants = json_of(&body);
    let bob_principal = format!("user:{}", fx.bob_id);
    let bob_grant = grants
        .as_array()
        .unwrap()
        .iter()
        .find(|grant| grant["principal"] == bob_principal.as_str())
        .unwrap_or_else(|| panic!("no grant for bob: {grants}"));
    assert_eq!(bob_grant["role"], "owner");
    assert_eq!(bob_grant["inherit"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_cannot_move_out_of_restricted_subtree() {
    let folder = restricted_folder("editor").await;
    let fx = &folder.fx;
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &folder.boundary,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let open_folder = create_doc(fx.app.clone(), &fx.alice, "Open folder", None).await;

    for target in [None, Some(open_folder.as_str())] {
        let (status, body) = move_doc(fx.app.clone(), &fx.bob, &folder.child, target).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "to {target:?}");
        assert_eq!(error_code(&body), "acl.restricted_move", "to {target:?}");
    }
    assert_eq!(
        parent_of(&fx.state, &folder.child).await,
        Some(uuid::Uuid::parse_str(&folder.boundary).unwrap()),
        "the page stayed where it was"
    );

    // Nor can an editor pull a page in from outside.
    let outside = create_doc(fx.app.clone(), &fx.bob, "Outside", None).await;
    let (status, body) = move_doc(fx.app.clone(), &fx.bob, &outside, Some(&folder.boundary)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "acl.restricted_move");
    assert_eq!(parent_of(&fx.state, &outside).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_can_move_out_of_restricted_subtree() {
    let folder = restricted_folder("editor").await;
    let fx = &folder.fx;

    let (status, body) = move_doc(fx.app.clone(), &fx.alice, &folder.child, None).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(parent_of(&fx.state, &folder.child).await, None);

    // Out of the subtree, the workspace role applies again.
    let (status, _) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{}", folder.child),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_into_unreadable_parent_is_404() {
    let folder = restricted_folder("editor").await;
    let fx = &folder.fx;
    let mine = create_doc(fx.app.clone(), &fx.bob, "Mine", None).await;

    let (restricted_status, restricted_body) =
        move_doc(fx.app.clone(), &fx.bob, &mine, Some(&folder.boundary)).await;
    let random_parent = uuid::Uuid::new_v4().to_string();
    let (random_status, random_body) =
        move_doc(fx.app.clone(), &fx.bob, &mine, Some(&random_parent)).await;
    assert_eq!(restricted_status, StatusCode::NOT_FOUND);
    assert_eq!(random_status, StatusCode::NOT_FOUND);
    assert_eq!(restricted_body, random_body, "no existence oracle");
    assert_eq!(parent_of(&fx.state, &mine).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_within_same_boundary_needs_only_editor() {
    let folder = restricted_folder("viewer").await;
    let fx = &folder.fx;
    let sibling = create_doc(fx.app.clone(), &fx.alice, "Sibling", Some(&folder.boundary)).await;
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &folder.boundary,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = move_doc(fx.app.clone(), &fx.bob, &sibling, Some(&folder.child)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        parent_of(&fx.state, &sibling).await,
        Some(uuid::Uuid::parse_str(&folder.child).unwrap())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn move_with_unreadable_anchor_is_bad_anchor() {
    let fx = fixture("editor").await;
    let folder = create_doc(fx.app.clone(), &fx.alice, "Folder", None).await;
    let hidden = create_doc(fx.app.clone(), &fx.alice, "Hidden", Some(&folder)).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &hidden, true).await;
    assert_eq!(status, StatusCode::OK);
    let mine = create_doc(fx.app.clone(), &fx.bob, "Mine", None).await;

    let move_uri = format!("/api/docs/{mine}/move");
    let place_next_to = |anchor: String| {
        send(
            fx.app.clone(),
            "POST",
            &move_uri,
            &fx.bob,
            Some(json!({"parent_id": folder, "after_id": anchor})),
        )
    };
    let (hidden_status, hidden_body) = place_next_to(hidden.clone()).await;
    let (random_status, random_body) = place_next_to(uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(hidden_status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(random_status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(hidden_body, random_body, "no existence oracle");
    assert_eq!(error_code(&hidden_body), "doc.bad_anchor");

    // The same for a create placed after it.
    let (status, body) = send(
        fx.app.clone(),
        "POST",
        "/api/docs",
        &fx.bob,
        Some(json!({"title": "New", "parent_id": folder, "after_id": hidden})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "doc.bad_anchor");
}

#[tokio::test(flavor = "multi_thread")]
async fn from_template_under_unreadable_parent_is_404() {
    let folder = restricted_folder("editor").await;
    let fx = &folder.fx;
    let template = create_doc(fx.app.clone(), &fx.alice, "Template", None).await;
    let (status, _) = send(
        fx.app.clone(),
        "POST",
        &format!("/api/docs/{template}/template"),
        &fx.alice,
        Some(json!({"is_template": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before = document_count(&fx.state).await;

    let from_template_uri = format!("/api/docs/from-template/{template}");
    let instantiate_under = |parent: String| {
        send(
            fx.app.clone(),
            "POST",
            &from_template_uri,
            &fx.bob,
            Some(json!({"title": "Sneaky", "parent_id": parent})),
        )
    };
    let (restricted_status, restricted_body) = instantiate_under(folder.boundary.clone()).await;
    let (random_status, random_body) = instantiate_under(uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(restricted_status, StatusCode::NOT_FOUND);
    assert_eq!(random_status, StatusCode::NOT_FOUND);
    assert_eq!(restricted_body, random_body, "no existence oracle");
    assert_eq!(document_count(&fx.state).await, before, "nothing created");
}

/// A workspace export with one page.
fn one_page_export() -> Vec<u8> {
    use std::io::Write;
    let manifest = json!({
        "knot_export_version": "2",
        "docs": [{
            "id": uuid::Uuid::new_v4().to_string(),
            "parent_id": null,
            "title": "Imported",
            "sort_key": "m",
            "path": "docs/imported.md",
        }],
        "attachments": [],
        "boards": [],
    });
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("index.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("docs/imported.md", opts).unwrap();
    zip.write_all(b"# Imported\n\nBody.\n").unwrap();
    zip.finish().unwrap().into_inner()
}

/// `POST /api/workspace/import?parent_id={parent}` with a one-page export.
async fn import_under(fx: &Fixture, joined: &str, parent: &str) -> (StatusCode, Bytes) {
    let (cookie, csrf) = split_cookie_csrf(joined);
    let response = fx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/workspace/import?parent_id={parent}"))
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/zip")
                .body(Body::from(one_page_export()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes)
}

#[tokio::test(flavor = "multi_thread")]
async fn import_under_unreadable_parent_is_404() {
    let folder = restricted_folder("editor").await;
    let fx = &folder.fx;
    let before = document_count(&fx.state).await;

    let (restricted_status, restricted_body) = import_under(fx, &fx.bob, &folder.boundary).await;
    let random_parent = uuid::Uuid::new_v4().to_string();
    let (random_status, random_body) = import_under(fx, &fx.bob, &random_parent).await;
    assert_eq!(restricted_status, StatusCode::NOT_FOUND);
    assert_eq!(random_status, StatusCode::NOT_FOUND);
    assert_eq!(restricted_body, random_body, "no existence oracle");
    assert_eq!(document_count(&fx.state).await, before, "nothing created");
}

/// Removing a member drops their page grants, so inviting them back gives
/// only their new workspace role: no way back into a restricted page, even
/// with their pre-removal access warm in the cache.
#[tokio::test(flavor = "multi_thread")]
async fn reinvited_member_does_not_regain_restricted_access() {
    let fx = fixture("editor").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Private", None).await;
    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &doc_id, true).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_grant(
        fx.app.clone(),
        &fx.alice,
        &doc_id,
        &fx.bob_id,
        "editor",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Warm Bob's cached access and listing.
    let (status, _) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bob reads through his grant");
    let (listed, _) = list_docs(fx.app.clone(), &fx.bob).await;
    assert!(listed_entry(&listed, &doc_id).is_some());

    let (status, _) = send(
        fx.app.clone(),
        "DELETE",
        &format!("/api/workspace/members/{}", fx.bob_id),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "alice removes bob");
    let (status, _) = send(
        fx.app.clone(),
        "POST",
        "/api/workspace/members",
        &fx.alice,
        Some(json!({"email": "bob@restricted.test", "role": "editor"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "alice invites bob back");
    let bob_again = login_as(fx.app.clone(), "bob@restricted.test", "bobpass1").await;

    let (status, _) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}"),
        &bob_again,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the old grant is gone");
    let (listed, _) = list_docs(fx.app.clone(), &bob_again).await;
    assert!(
        listed_entry(&listed, &doc_id).is_none(),
        "the restricted page is hidden from the re-invited member"
    );
    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}/access"),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let people = json_of(&body)["people"].as_array().unwrap().clone();
    assert!(
        people
            .iter()
            .all(|person| person["principal"] != format!("user:{}", fx.bob_id)),
        "bob no longer appears under people with access: {people:?}"
    );
}

// ---------------------------------------------------------------------------
// Realtime: open doc and board sockets are revoked on restrict
// ---------------------------------------------------------------------------

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The fixture, with doc and board rooms on a MemBus, the ACL listener
/// running as in `main` (it evicts the cache and revokes open sockets), and
/// the app served on an ephemeral port, whose address is returned.
async fn realtime_fixture(bob_role: &str) -> (Fixture, std::net::SocketAddr) {
    let mut fx = fixture(bob_role).await;
    let pool = fx.state.pool.clone().unwrap();
    let engine: Arc<dyn knot_crdt::Engine> = Arc::new(knot_crdt::YrsEngine);
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::MemBus::new());
    fx.state.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        engine.clone(),
        bus.clone(),
        Arc::new(knot_storage::PgUpdatesStore::new(pool.clone())),
        Arc::new(knot_storage::PgSnapshotStore::new(pool.clone())),
        knot_crdt::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(600),
        },
        std::time::Duration::from_secs(300),
    )));
    fx.state.board_rooms = Some(Arc::new(knot_crdt::BoardRooms::new(
        engine,
        fx.state.boards.clone().unwrap(),
        bus,
    )));
    fx.app = router_with_state(fx.state.clone());

    let _acl_listener = knot_docs::spawn_listener(
        pool.clone(),
        fx.state.acl.clone().unwrap(),
        fx.state.docs.clone().unwrap(),
        knot_server::revoke_doc_sockets(&fx.state),
    );
    wait_for_acl_listener(&pool).await;

    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp_listener.local_addr().unwrap();
    let served = fx.app.clone();
    tokio::spawn(async move {
        axum::serve(tcp_listener, served).await.unwrap();
    });
    (fx, addr)
}

/// Wait until the ACL listener has subscribed: a NOTIFY sent before that
/// is lost.
async fn wait_for_acl_listener(pool: &knot_storage::Pool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let listening: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND query ILIKE 'LISTEN%acl_invalidate%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if listening > 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ACL listener never subscribed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Dial `path` with `joined`'s session cookie.
async fn dial(
    addr: std::net::SocketAddr,
    path: &str,
    joined: &str,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("cookie", split_cookie_csrf(joined).0.parse().unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
}

/// Dial `path` and wait for the room's first frame (its state), which it
/// sends once the connection has joined.
async fn join_socket(addr: std::net::SocketAddr, path: &str, joined: &str) -> Socket {
    use futures_util::StreamExt;
    let mut socket = dial(addr, path, joined).await.expect("ws connect");
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
        .await
        .expect("the room never sent its state");
    assert!(
        matches!(
            first,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(_)))
        ),
        "expected the room's state, got {first:?}"
    );
    socket
}

/// The close code the server closes `socket` with, within `within`.
async fn close_code_within(socket: &mut Socket, within: std::time::Duration) -> u16 {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, socket.next()).await {
            Ok(Some(Ok(Message::Close(Some(frame))))) => return frame.code.into(),
            Ok(Some(Ok(Message::Close(None)))) | Ok(None) => {
                panic!("socket closed without a close code")
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => panic!("ws error: {error}"),
            Err(_) => panic!("the server did not close the socket within {within:?}"),
        }
    }
}

/// The HTTP status a refused upgrade of `path` is answered with.
async fn refused_upgrade_status(addr: std::net::SocketAddr, path: &str, joined: &str) -> u16 {
    match dial(addr, path, joined).await {
        Ok(_) => panic!("the upgrade of {path} was accepted"),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(error) => panic!("the upgrade of {path} failed without a status: {error}"),
    }
}

/// Restricting a page reaches sockets that are already open: Bob, a
/// workspace editor without a grant, has the page and one of its boards
/// open, and both close with 4403 at once, with no cache window. Opening
/// them again is refused.
#[tokio::test(flavor = "multi_thread")]
async fn restrict_closes_doc_and_board_sockets() {
    let (fx, addr) = realtime_fixture("editor").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Soon private", None).await;
    let (status, body) = send(
        fx.app.clone(),
        "POST",
        &format!("/api/docs/{doc_id}/boards"),
        &fx.alice,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create board: {body:?}");
    let board_id = json_of(&body)["id"].as_str().unwrap().to_string();
    let doc_path = format!("/collab/doc/{doc_id}");
    let board_path = format!("/collab/board/{board_id}");

    let mut doc_socket = join_socket(addr, &doc_path, &fx.bob).await;
    let mut board_socket = join_socket(addr, &board_path, &fx.bob).await;

    let (status, _) = set_restricted(fx.app.clone(), &fx.alice, &doc_id, true).await;
    assert_eq!(status, StatusCode::OK, "alice restricts");

    let within = std::time::Duration::from_secs(5);
    assert_eq!(close_code_within(&mut doc_socket, within).await, 4403);
    assert_eq!(close_code_within(&mut board_socket, within).await, 4403);
    assert_eq!(refused_upgrade_status(addr, &doc_path, &fx.bob).await, 403);
    assert_eq!(
        refused_upgrade_status(addr, &board_path, &fx.bob).await,
        403
    );
}
