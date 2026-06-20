//! Integration tests for comment threads:
//! CRUD + ACL + resolve/unresolve + reactions + @mention + body limit.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// Seed: workspace + alice (given role) + a doc.
/// Returns `(state, ws_id, doc_id, alice_user_id)`.
async fn state_with_seeded(role: WorkspaceRole) -> (AppState, Uuid, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = s.hasher.hash("hunter22").unwrap();
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
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, user.id, role)
        .await
        .unwrap();
    let doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Test Doc", "m", user.id)
        .await
        .unwrap();
    (s, ws.id, doc.id, user.id)
}

/// Add a second user (bob) to the workspace with the specified role.
/// Returns bob's user_id.
async fn add_bob(state: &AppState, ws_id: Uuid, role: WorkspaceRole) -> Uuid {
    let hash = state.hasher.hash("hunter22").unwrap();
    let bob = state
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws_id, bob.id, role)
        .await
        .unwrap();
    bob.id
}

/// Log in and return `(sid_kv, csrf_val)`.
async fn login(app: &axum::Router, email: &str) -> (String, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": "hunter22"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::NO_CONTENT,
        "login failed for {email}"
    );
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
    let csrf_val = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
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
// HTTP helpers
// ---------------------------------------------------------------------------

async fn post_json(
    app: &axum::Router,
    uri: &str,
    sid: &str,
    csrf: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn get_json(
    app: &axum::Router,
    uri: &str,
    sid: &str,
    csrf: &str,
) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn patch_json(
    app: &axum::Router,
    uri: &str,
    sid: &str,
    csrf: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(uri)
                .header("content-type", "application/json")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn delete_req(app: &axum::Router, uri: &str, sid: &str, csrf: &str) -> StatusCode {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    r.status()
}

// ---------------------------------------------------------------------------
// Test cases
// ---------------------------------------------------------------------------

/// 1. Owner creates a thread → 201 with body + position_y echoed.
#[tokio::test(flavor = "multi_thread")]
async fn owner_creates_thread_201() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let pos_y = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"position_bytes",
    );
    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({
            "body": "Hello thread",
            "position_y": pos_y,
            "anchor_text": "some anchor"
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "expected 201, got {status}: {json}"
    );
    assert_eq!(json["body"], "Hello thread");
    assert!(json["id"].as_str().is_some(), "expected id");
    assert_eq!(json["thread_id"], json["id"], "root: thread_id == id");
    assert!(json["parent_id"].is_null(), "root has no parent_id");
    assert_eq!(json["anchor_text"], "some anchor");
}

/// 2. Editor replies → 201 with parent_id set.
#[tokio::test(flavor = "multi_thread")]
async fn editor_replies_201_with_parent_id() {
    let (state, ws_id, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    let app = router_with_state(state);

    // Alice creates root
    let (sid_alice, csrf_alice) = login(&app, "alice@example.com").await;
    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid_alice,
        &csrf_alice,
        serde_json::json!({"body": "Root comment"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let thread_id = json["id"].as_str().unwrap().to_string();

    // Bob replies
    let (sid_bob, csrf_bob) = login(&app, "bob@example.com").await;
    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{thread_id}/replies"),
        &sid_bob,
        &csrf_bob,
        serde_json::json!({"body": "Bob's reply"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "reply: expected 201, got {status}: {json}"
    );
    assert_eq!(json["body"], "Bob's reply");
    assert_eq!(json["thread_id"].as_str().unwrap(), thread_id);
    assert_eq!(json["parent_id"].as_str().unwrap(), thread_id);
}

/// 3. Viewer cannot create or reply → 403.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_create_or_reply_403() {
    let (state, ws_id, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;

    // Create root as owner first
    let (sid_alice, csrf_alice) =
        login(&router_with_state(state.clone()), "alice@example.com").await;
    let (_, root_json) = post_json(
        &router_with_state(state.clone()),
        &format!("/api/docs/{doc_id}/comments"),
        &sid_alice,
        &csrf_alice,
        serde_json::json!({"body": "Thread root"}),
    )
    .await;
    let thread_id = root_json["id"].as_str().unwrap().to_string();

    add_bob(&state, ws_id, WorkspaceRole::Viewer).await;
    let app = router_with_state(state);
    let (sid_bob, csrf_bob) = login(&app, "bob@example.com").await;

    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid_bob,
        &csrf_bob,
        serde_json::json!({"body": "Viewer tries to create"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "viewer create: expected 403");

    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{thread_id}/replies"),
        &sid_bob,
        &csrf_bob,
        serde_json::json!({"body": "Viewer reply"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "viewer reply: expected 403");
}

/// 4. Anon → 401.
#[tokio::test(flavor = "multi_thread")]
async fn anon_gets_401() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/docs/{doc_id}/comments"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::UNAUTHORIZED,
        "anon GET: expected 401"
    );

    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc_id}/comments"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"body": "x"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::UNAUTHORIZED,
        "anon POST: expected 401"
    );
}

/// 5. GET list returns threads + replies flat. Default excludes resolved.
#[tokio::test(flavor = "multi_thread")]
async fn get_list_flat_default_excludes_resolved() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    // Create two threads
    let (_, t1) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": "Thread 1"}),
    )
    .await;
    let t1_id = t1["id"].as_str().unwrap().to_string();

    let (_, t2) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": "Thread 2"}),
    )
    .await;
    let t2_id = t2["id"].as_str().unwrap().to_string();

    // Add reply to t1
    post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{t1_id}/replies"),
        &sid,
        &csrf,
        serde_json::json!({"body": "Reply to thread 1"}),
    )
    .await;

    // Resolve t2
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{t2_id}/resolve"),
        &sid,
        &csrf,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Default list: should have t1 root + t1 reply = 2 items; t2 excluded
    let (status, list) = get_json(&app, &format!("/api/docs/{doc_id}/comments"), &sid, &csrf).await;
    assert_eq!(status, StatusCode::OK);
    let arr = list.as_array().unwrap();
    assert_eq!(
        arr.len(),
        2,
        "default should return 2 items (t1 root + reply), got {list}"
    );
    let ids: Vec<&str> = arr
        .iter()
        .map(|c| c["thread_id"].as_str().unwrap())
        .collect();
    assert!(
        ids.iter().all(|id| *id == t1_id),
        "all returned items should be in thread 1"
    );
}

/// 6. `?include_resolved=true` returns resolved threads too.
#[tokio::test(flavor = "multi_thread")]
async fn include_resolved_returns_all() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let (_, t1) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": "Active thread"}),
    )
    .await;
    let (_, t2) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": "Resolved thread"}),
    )
    .await;
    let t2_id = t2["id"].as_str().unwrap();

    post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{t2_id}/resolve"),
        &sid,
        &csrf,
        serde_json::json!({}),
    )
    .await;
    let _ = t1;

    // include_resolved=true → both threads
    let (status, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/comments?include_resolved=true"),
        &sid,
        &csrf,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list.as_array().unwrap().len(),
        2,
        "expected 2 comments with include_resolved=true, got {list}"
    );

    // include_resolved=false (default) → only active
    let (status, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/comments?include_resolved=false"),
        &sid,
        &csrf,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        list.as_array().unwrap().len(),
        1,
        "expected 1 comment without resolved, got {list}"
    );
}

/// 7. Resolve sets resolved_at; unresolve clears it.
#[tokio::test(flavor = "multi_thread")]
async fn resolve_and_unresolve_roundtrip() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let (_, t) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": "Resolve me"}),
    )
    .await;
    let tid = t["id"].as_str().unwrap();

    // Resolve
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{tid}/resolve"),
        &sid,
        &csrf,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "resolve expected 204");

    // Resolved: not in default list
    let (_, list) = get_json(&app, &format!("/api/docs/{doc_id}/comments"), &sid, &csrf).await;
    assert_eq!(
        list.as_array().unwrap().len(),
        0,
        "resolved thread should be hidden by default"
    );

    // include_resolved=true: it's there, resolved_at set
    let (_, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/comments?include_resolved=true"),
        &sid,
        &csrf,
    )
    .await;
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert!(
        !arr[0]["resolved_at"].is_null(),
        "resolved_at should be set after resolve"
    );

    // Unresolve
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{tid}/unresolve"),
        &sid,
        &csrf,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "unresolve expected 204");

    // Now visible again in default list with resolved_at = null
    let (_, list) = get_json(&app, &format!("/api/docs/{doc_id}/comments"), &sid, &csrf).await;
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert!(
        arr[0]["resolved_at"].is_null(),
        "resolved_at should be null after unresolve"
    );
}

/// 8. Add + remove reaction round-trip.
#[tokio::test(flavor = "multi_thread")]
async fn reaction_add_remove_roundtrip() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let (_, c) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": "React to me"}),
    )
    .await;
    let comment_id = c["id"].as_str().unwrap();

    // Add 👍
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{comment_id}/reactions"),
        &sid,
        &csrf,
        serde_json::json!({"emoji": "👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "add reaction: expected 204");

    // Verify via list
    let (_, list) = get_json(&app, &format!("/api/docs/{doc_id}/comments"), &sid, &csrf).await;
    let comment = &list.as_array().unwrap()[0];
    let thumbs_up = &comment["reactions"]["👍"];
    assert!(
        thumbs_up.is_array() && !thumbs_up.as_array().unwrap().is_empty(),
        "expected 👍 in reactions, got {comment}"
    );

    // Remove 👍
    let status = delete_req(
        &app,
        &format!("/api/docs/{doc_id}/comments/{comment_id}/reactions/👍"),
        &sid,
        &csrf,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "remove reaction: expected 204"
    );

    // Verify removed
    let (_, list) = get_json(&app, &format!("/api/docs/{doc_id}/comments"), &sid, &csrf).await;
    let comment = &list.as_array().unwrap()[0];
    let thumbs_up = &comment["reactions"]["👍"];
    assert!(
        thumbs_up.is_null() || thumbs_up.as_array().map(|a| a.is_empty()).unwrap_or(true),
        "expected 👍 removed from reactions, got {comment}"
    );
}

/// 9. Invalid emoji → 415 with code comment.invalid_emoji.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_emoji_returns_415() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let (_, c) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": "Test comment"}),
    )
    .await;
    let comment_id = c["id"].as_str().unwrap();

    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{comment_id}/reactions"),
        &sid,
        &csrf,
        serde_json::json!({"emoji": "💩"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "invalid emoji: expected 415, got {status}: {json}"
    );
    assert_eq!(
        json["error"]["code"], "comment.invalid_emoji",
        "expected code comment.invalid_emoji, got {json}"
    );
}

/// 10. Edit (author) → 200; non-author → 403; delete (author) → 204;
///     delete by workspace owner → 204.
#[tokio::test(flavor = "multi_thread")]
async fn edit_and_delete_acl() {
    let (state, ws_id, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    let app = router_with_state(state);

    let (sid_alice, csrf_alice) = login(&app, "alice@example.com").await;
    let (sid_bob, csrf_bob) = login(&app, "bob@example.com").await;

    // Alice creates a comment
    let (_, c) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid_alice,
        &csrf_alice,
        serde_json::json!({"body": "Alice's comment"}),
    )
    .await;
    let comment_id = c["id"].as_str().unwrap();

    // Bob (editor, not author) tries to edit → 403
    let (status, json) = patch_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{comment_id}"),
        &sid_bob,
        &csrf_bob,
        serde_json::json!({"body": "Bob edits Alice"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-author edit: expected 403, got {status}: {json}"
    );

    // Alice edits her own comment → 200
    let (status, json) = patch_json(
        &app,
        &format!("/api/docs/{doc_id}/comments/{comment_id}"),
        &sid_alice,
        &csrf_alice,
        serde_json::json!({"body": "Updated body"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "author edit: expected 200, got {status}: {json}"
    );
    assert_eq!(json["body"], "Updated body");

    // Bob creates his own comment
    let (_, bc) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid_bob,
        &csrf_bob,
        serde_json::json!({"body": "Bob's comment"}),
    )
    .await;
    let bob_comment_id = bc["id"].as_str().unwrap();

    // Alice (workspace owner) deletes Bob's comment → 204
    let status = delete_req(
        &app,
        &format!("/api/docs/{doc_id}/comments/{bob_comment_id}"),
        &sid_alice,
        &csrf_alice,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "workspace owner delete other's comment: expected 204"
    );

    // Bob deletes his own second comment after alice-deletes-first
    // (Make a new one so bob can delete his own)
    let (_, bc2) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid_bob,
        &csrf_bob,
        serde_json::json!({"body": "Bob's second comment"}),
    )
    .await;
    let bc2_id = bc2["id"].as_str().unwrap();

    let status = delete_req(
        &app,
        &format!("/api/docs/{doc_id}/comments/{bc2_id}"),
        &sid_bob,
        &csrf_bob,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "author self-delete: expected 204"
    );
}

/// 12. Cross-document IDOR: a comment in doc B cannot be resolved/edited/
///     reacted-to by routing the request through doc A's path, even though the
///     caller is authorized on doc A. The handler now re-asserts that the
///     comment belongs to the path's doc.
#[tokio::test(flavor = "multi_thread")]
async fn cross_doc_comment_mutation_is_rejected() {
    let (state, ws_id, doc_a, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    // A second doc in the same workspace.
    let doc_b = state
        .docs
        .as_ref()
        .unwrap()
        .create(ws_id, None, "Doc B", "m", uid)
        .await
        .unwrap();
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    // Create a comment in doc B via its correct path.
    let (status, c) = post_json(
        &app,
        &format!("/api/docs/{}/comments", doc_b.id),
        &sid,
        &csrf,
        serde_json::json!({"body": "lives in doc B"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let comment_b = c["id"].as_str().unwrap().to_string();

    // Resolve via doc A's path → must 404.
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_a}/comments/{comment_b}/resolve"),
        &sid,
        &csrf,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-doc resolve must 404");

    // Edit via doc A's path → must 404.
    let (status, _) = patch_json(
        &app,
        &format!("/api/docs/{doc_a}/comments/{comment_b}"),
        &sid,
        &csrf,
        serde_json::json!({"body": "hijack"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-doc edit must 404");

    // React via doc A's path → must 404.
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_a}/comments/{comment_b}/reactions"),
        &sid,
        &csrf,
        serde_json::json!({"emoji": "👍"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cross-doc reaction must 404");

    // Sanity: the same operation via doc B's correct path still works.
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{}/comments/{comment_b}/resolve", doc_b.id),
        &sid,
        &csrf,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "same-doc resolve should succeed"
    );
}

/// 11. Body length > 4096 → 413.
#[tokio::test(flavor = "multi_thread")]
async fn body_over_4096_returns_413() {
    let (state, _ws, doc_id, _uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let long_body = "x".repeat(4097);
    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/comments"),
        &sid,
        &csrf,
        serde_json::json!({"body": long_body}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "body >4096: expected 413, got {status}: {json}"
    );
}
