//! Integration tests for change suggestions: create / list / reject /
//! withdraw, reply threads, ACL, cross-doc ids and the realtime NOTIFY.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::block::ClientID;
use yrs::updates::encoder::Encode;
use yrs::{Assoc, ID, StickyIndex};

// ---------------------------------------------------------------------------
// Scaffolding (file-local copies of the comments_integration helpers)
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
    add_member(state, ws_id, "bob@example.com", "Bob", role).await
}

/// Add a member with `role` whose display name is `name`; returns their id.
async fn add_member(
    state: &AppState,
    ws_id: Uuid,
    email: &str,
    name: &str,
    role: WorkspaceRole,
) -> Uuid {
    let hash = state.hasher.hash("hunter22").unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local(email, name, &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws_id, user.id, role)
        .await
        .unwrap();
    user.id
}

/// Give `grantee` `role` on `doc_id`, inheriting to its subpages.
async fn grant(
    state: &AppState,
    ws_id: Uuid,
    doc_id: Uuid,
    grantee: Uuid,
    role: WorkspaceRole,
    actor: Uuid,
) {
    state
        .grants
        .as_ref()
        .unwrap()
        .put(ws_id, doc_id, &format!("user:{grantee}"), role, true, actor)
        .await
        .unwrap();
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
                    json!({"email": email, "password": "hunter22"}).to_string(),
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

async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    sid: &str,
    csrf: &str,
    body: Option<Body>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("{sid}; csrf={csrf}"))
        .header("x-csrf-token", csrf);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let r = app
        .clone()
        .oneshot(req.body(body.unwrap_or_else(Body::empty)).unwrap())
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn post_json(
    app: &axum::Router,
    uri: &str,
    sid: &str,
    csrf: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        app,
        "POST",
        uri,
        sid,
        csrf,
        Some(Body::from(body.to_string())),
    )
    .await
}

async fn get_json(app: &axum::Router, uri: &str, sid: &str, csrf: &str) -> (StatusCode, Value) {
    send(app, "GET", uri, sid, csrf, None).await
}

async fn patch_json(
    app: &axum::Router,
    uri: &str,
    sid: &str,
    csrf: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        app,
        "PATCH",
        uri,
        sid,
        csrf,
        Some(Body::from(body.to_string())),
    )
    .await
}

async fn delete_req(app: &axum::Router, uri: &str, sid: &str, csrf: &str) -> StatusCode {
    send(app, "DELETE", uri, sid, csrf, None).await.0
}

// ---------------------------------------------------------------------------
// Suggestion helpers
// ---------------------------------------------------------------------------

/// A base64 RelativePosition tied to the item at `clock` of a fixed client.
fn anchor(clock: u32, assoc: Assoc) -> String {
    STANDARD.encode(StickyIndex::from_id(ID::new(ClientID::new(7), clock), assoc).encode_v1())
}

/// A valid replace-suggestion body.
fn replace_body() -> Value {
    json!({
        "kind": "replace",
        "position_y": anchor(0, Assoc::After),
        "position_y_end": anchor(4, Assoc::Before),
        "original_text": "Hello",
        "context_before": "",
        "proposed_text": "Howdy",
        "reason": "friendlier",
    })
}

/// Create a suggestion through the API; returns its id.
async fn create(app: &axum::Router, doc_id: Uuid, sid: &str, csrf: &str) -> String {
    let (status, json) = post_json(
        app,
        &format!("/api/docs/{doc_id}/suggestions"),
        sid,
        csrf,
        replace_body(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create: {json}");
    json["id"].as_str().unwrap().to_string()
}

fn code(json: &Value) -> &str {
    json["error"]["code"].as_str().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Create + list
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_create() {
    let (state, ws_id, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Viewer).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "bob@example.com").await;

    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions"),
        &sid,
        &csrf,
        replace_body(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(code(&json), "acl.editor_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_creates_and_lists() {
    let (state, ws_id, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let bob = add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "bob@example.com").await;

    let body = replace_body();
    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions"),
        &sid,
        &csrf,
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json["kind"], "replace");
    assert_eq!(json["status"], "pending");
    assert_eq!(json["author_id"], bob.to_string());
    assert_eq!(json["doc_id"], doc_id.to_string());
    assert_eq!(
        json["position_y"], body["position_y"],
        "anchors echo as base64"
    );
    assert_eq!(json["position_y_end"], body["position_y_end"]);
    assert_eq!(json["original_text"], "Hello");
    assert_eq!(json["proposed_text"], "Howdy");
    assert_eq!(json["reason"], "friendlier");

    let (status, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions"),
        &sid,
        &csrf,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], json["id"]);
    assert_eq!(
        list[0]["proposed_text"], "Howdy",
        "suggestion fields flatten"
    );
    assert_eq!(list[0]["replies"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_can_list() {
    let (state, ws_id, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Viewer).await;
    let app = router_with_state(state);
    let (sid_a, csrf_a) = login(&app, "alice@example.com").await;
    create(&app, doc_id, &sid_a, &csrf_a).await;
    let (sid, csrf) = login(&app, "bob@example.com").await;

    let (status, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions"),
        &sid,
        &csrf,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_kind_combination_is_400() {
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;
    let uri = format!("/api/docs/{doc_id}/suggestions");

    for (kind, original, proposed) in [
        ("replace", "Hello", "Hello"), // a no-op
        ("replace", "", "Howdy"),
        ("replace", "Hello", ""),
        ("delete", "Hello", "Howdy"),
        ("delete", "", ""),
        ("insert", "Hello", "Howdy"),
        ("insert", "", ""),
    ] {
        let mut body = replace_body();
        body["kind"] = json!(kind);
        body["original_text"] = json!(original);
        body["proposed_text"] = json!(proposed);
        let (status, json) = post_json(&app, &uri, &sid, &csrf, body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{kind} {original:?} → {proposed:?}: {json}"
        );
        assert_eq!(code(&json), "suggestion.invalid", "{kind}");
    }

    // The valid shapes of delete and insert pass.
    for (kind, original, proposed) in [("delete", "Hello", ""), ("insert", "", "Hi ")] {
        let mut body = replace_body();
        body["kind"] = json!(kind);
        body["original_text"] = json!(original);
        body["proposed_text"] = json!(proposed);
        let (status, json) = post_json(&app, &uri, &sid, &csrf, body).await;
        assert_eq!(status, StatusCode::CREATED, "{kind}: {json}");
        assert_eq!(json["kind"], kind);
    }

    // An unknown kind does not parse.
    let mut body = replace_body();
    body["kind"] = json!("rewrite");
    let (status, json) = post_json(&app, &uri, &sid, &csrf, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn newline_in_proposed_text_is_400() {
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    for proposed in ["two\nlines", "carriage\rreturn"] {
        let mut body = replace_body();
        body["proposed_text"] = json!(proposed);
        let (status, json) = post_json(
            &app,
            &format!("/api/docs/{doc_id}/suggestions"),
            &sid,
            &csrf,
            body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{proposed:?}: {json}");
        assert_eq!(code(&json), "suggestion.invalid");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn too_long_is_422() {
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;
    let uri = format!("/api/docs/{doc_id}/suggestions");

    for (field, value) in [
        ("proposed_text", "ä".repeat(10_001)),
        ("original_text", "ä".repeat(10_001)),
        ("context_before", "ä".repeat(201)),
        ("reason", "ä".repeat(4_097)),
    ] {
        let mut body = replace_body();
        body[field] = json!(value);
        let (status, json) = post_json(&app, &uri, &sid, &csrf, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {json}");
        assert_eq!(code(&json), "suggestion.too_long", "{field}");
    }

    // At the limit, counted in characters (not bytes), it passes.
    let mut body = replace_body();
    body["proposed_text"] = json!("ä".repeat(10_000));
    body["context_before"] = json!("€".repeat(200));
    body["reason"] = json!("😀".repeat(4_096));
    let (status, json) = post_json(&app, &uri, &sid, &csrf, body).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");

    // Over the request cap is 413.
    let mut body = replace_body();
    body["reason"] = json!("x".repeat(300 * 1024));
    let (status, json) = post_json(&app, &uri, &sid, &csrf, body).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{json}");
    assert_eq!(code(&json), "suggestion.body_too_large");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_anchor_is_400() {
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;
    let uri = format!("/api/docs/{doc_id}/suggestions");

    let not_a_position = STANDARD.encode(b"position_bytes");
    for (field, value) in [
        ("position_y", "not base64!!".to_string()),
        ("position_y_end", "not base64!!".to_string()),
        ("position_y", not_a_position.clone()),
        ("position_y_end", not_a_position),
        ("position_y", String::new()),
    ] {
        let mut body = replace_body();
        body[field] = json!(value);
        let (status, json) = post_json(&app, &uri, &sid, &csrf, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}={value:?}: {json}");
        assert_eq!(
            code(&json),
            "suggestion.invalid_anchor",
            "{field}={value:?}"
        );
    }

    // Malformed JSON is a plain bad_request.
    let (status, json) = send(
        &app,
        "POST",
        &uri,
        &sid,
        &csrf,
        Some(Body::from("{not json")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(code(&json), "bad_request");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_hides_resolved_unless_asked() {
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;
    let open = create(&app, doc_id, &sid, &csrf).await;
    let rejected = create(&app, doc_id, &sid, &csrf).await;
    let withdrawn = create(&app, doc_id, &sid, &csrf).await;
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions/{rejected}/reject"),
        &sid,
        &csrf,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions/{withdrawn}/withdraw"),
        &sid,
        &csrf,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let ids = |list: &Value| -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect()
    };
    let (_, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions"),
        &sid,
        &csrf,
    )
    .await;
    assert_eq!(ids(&list), vec![open.clone()]);

    let (_, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions?include_resolved=true"),
        &sid,
        &csrf,
    )
    .await;
    assert_eq!(ids(&list), vec![open, rejected, withdrawn.clone()]);
    let w = &list.as_array().unwrap()[2];
    assert_eq!(w["status"], "withdrawn");
    assert!(w["decided_at"].as_str().is_some(), "{w}");
    let r = &list.as_array().unwrap()[1];
    assert_eq!(r["status"], "rejected");
    assert!(r["decided_by"].as_str().is_some(), "{r}");
}

// ---------------------------------------------------------------------------
// Reject + withdraw
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn editor_cannot_reject_owner_can() {
    let (state, ws_id, doc_id, alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    let app = router_with_state(state);
    let (sid_bob, csrf_bob) = login(&app, "bob@example.com").await;
    let sid_id = create(&app, doc_id, &sid_bob, &csrf_bob).await;
    let uri = format!("/api/docs/{doc_id}/suggestions/{sid_id}/reject");

    let (status, json) = post_json(&app, &uri, &sid_bob, &csrf_bob, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(code(&json), "acl.owner_required");

    let (sid_alice, csrf_alice) = login(&app, "alice@example.com").await;
    let (status, json) = post_json(&app, &uri, &sid_alice, &csrf_alice, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "rejected");
    assert_eq!(json["decided_by"], alice.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_by_grant_rejects_in_restricted_doc_but_workspace_owner_with_editor_grant_cannot() {
    let (state, ws_id, doc_id, alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let bob = add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    state
        .docs
        .as_ref()
        .unwrap()
        .set_restricted(ws_id, doc_id, bob, true)
        .await
        .unwrap();
    grant(&state, ws_id, doc_id, alice, WorkspaceRole::Editor, bob).await;
    let app = router_with_state(state);
    let (sid_alice, csrf_alice) = login(&app, "alice@example.com").await;
    let (sid_bob, csrf_bob) = login(&app, "bob@example.com").await;
    let sid_id = create(&app, doc_id, &sid_alice, &csrf_alice).await;
    let uri = format!("/api/docs/{doc_id}/suggestions/{sid_id}/reject");

    let (status, json) = post_json(&app, &uri, &sid_alice, &csrf_alice, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(code(&json), "acl.owner_required");
    let (status, json) = post_json(&app, &uri, &sid_bob, &csrf_bob, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_author_withdraws() {
    let (state, ws_id, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    let app = router_with_state(state);
    let (sid_bob, csrf_bob) = login(&app, "bob@example.com").await;
    let (sid_alice, csrf_alice) = login(&app, "alice@example.com").await;
    let sid_id = create(&app, doc_id, &sid_bob, &csrf_bob).await;
    let uri = format!("/api/docs/{doc_id}/suggestions/{sid_id}/withdraw");

    // Not even an owner withdraws someone else's suggestion.
    let (status, json) = post_json(&app, &uri, &sid_alice, &csrf_alice, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(code(&json), "suggestion.not_author");

    let (status, json) = post_json(&app, &uri, &sid_bob, &csrf_bob, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "withdrawn");

    let (status, json) = post_json(&app, &uri, &sid_bob, &csrf_bob, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(code(&json), "suggestion.not_pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn reject_twice_is_not_pending() {
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;
    let sid_id = create(&app, doc_id, &sid, &csrf).await;
    let uri = format!("/api/docs/{doc_id}/suggestions/{sid_id}/reject");

    let (status, _) = post_json(&app, &uri, &sid, &csrf, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, json) = post_json(&app, &uri, &sid, &csrf, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(code(&json), "suggestion.not_pending");

    // A rejected one can no longer be withdrawn either.
    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions/{sid_id}/withdraw"),
        &sid,
        &csrf,
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(code(&json), "suggestion.not_pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_doc_suggestion_id_is_404() {
    let (state, ws_id, doc_a, alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let doc_b = state
        .docs
        .as_ref()
        .unwrap()
        .create(ws_id, None, "Other Doc", "n", alice)
        .await
        .unwrap()
        .id;
    let app = router_with_state(state.clone());
    let (sid, csrf) = login(&app, "alice@example.com").await;
    let in_b = create(&app, doc_b, &sid, &csrf).await;
    let (status, reply) = post_json(
        &app,
        &format!("/api/docs/{doc_b}/suggestions/{in_b}/replies"),
        &sid,
        &csrf,
        json!({"body": "in b"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reply}");
    let reply_id = reply["id"].as_str().unwrap();

    for (method, uri, body) in [
        (
            "POST",
            format!("/api/docs/{doc_a}/suggestions/{in_b}/reject"),
            Some(json!({})),
        ),
        (
            "POST",
            format!("/api/docs/{doc_a}/suggestions/{in_b}/withdraw"),
            Some(json!({})),
        ),
        (
            "POST",
            format!("/api/docs/{doc_a}/suggestions/{in_b}/replies"),
            Some(json!({"body": "x"})),
        ),
        (
            "PATCH",
            format!("/api/docs/{doc_a}/suggestions/{in_b}/replies/{reply_id}"),
            Some(json!({"body": "x"})),
        ),
        (
            "DELETE",
            format!("/api/docs/{doc_a}/suggestions/{in_b}/replies/{reply_id}"),
            None,
        ),
        (
            "POST",
            format!("/api/docs/{doc_a}/suggestions/{}/reject", Uuid::new_v4()),
            Some(json!({})),
        ),
    ] {
        let (status, json) = send(
            &app,
            method,
            &uri,
            &sid,
            &csrf,
            body.map(|b| Body::from(b.to_string())),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {json}");
        assert_eq!(code(&json), "suggestion.not_found", "{method} {uri}");
    }

    // A reply id paired with a different suggestion of the right doc is 404 too.
    let other_in_b = create(&app, doc_b, &sid, &csrf).await;
    let (status, json) = patch_json(
        &app,
        &format!("/api/docs/{doc_b}/suggestions/{other_in_b}/replies/{reply_id}"),
        &sid,
        &csrf,
        json!({"body": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(code(&json), "suggestion.not_found");

    // Nothing in doc B moved.
    let (_, list) = get_json(&app, &format!("/api/docs/{doc_b}/suggestions"), &sid, &csrf).await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|s| s["status"] == "pending"), "{list:?}");
    assert_eq!(list[0]["replies"][0]["body"], "in b");
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn replies_are_listed_with_their_suggestion() {
    let (state, ws_id, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let bob = add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    add_member(
        &state,
        ws_id,
        "vic@example.com",
        "Vic",
        WorkspaceRole::Viewer,
    )
    .await;
    let app = router_with_state(state);
    let (sid_a, csrf_a) = login(&app, "alice@example.com").await;
    let (sid_b, csrf_b) = login(&app, "bob@example.com").await;
    let first = create(&app, doc_id, &sid_a, &csrf_a).await;
    let second = create(&app, doc_id, &sid_a, &csrf_a).await;

    let reply = |s: String, body: &'static str, sid: String, csrf: String| {
        let app = app.clone();
        async move {
            post_json(
                &app,
                &format!("/api/docs/{doc_id}/suggestions/{s}/replies"),
                &sid,
                &csrf,
                json!({"body": body, "mentions": []}),
            )
            .await
        }
    };
    let (status, r1) = reply(first.clone(), "one", sid_b.clone(), csrf_b.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{r1}");
    assert_eq!(r1["suggestion_id"], first);
    assert_eq!(r1["author_id"], bob.to_string());
    assert_eq!(r1["body"], "one");
    let (status, _) = reply(second.clone(), "two", sid_a.clone(), csrf_a.clone()).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = reply(first.clone(), "three", sid_a.clone(), csrf_a.clone()).await;
    assert_eq!(status, StatusCode::CREATED);

    // An empty reply is invalid, an over-long one too long.
    let (status, json) = reply(first.clone(), "", sid_a.clone(), csrf_a.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(code(&json), "suggestion.invalid");
    let (status, json) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions/{first}/replies"),
        &sid_a,
        &csrf_a,
        json!({"body": "ü".repeat(4_097)}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_eq!(code(&json), "suggestion.too_long");

    // A viewer reads replies but cannot write one.
    let (sid_v, csrf_v) = login(&app, "vic@example.com").await;
    let (status, json) = reply(first.clone(), "nope", sid_v.clone(), csrf_v.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(code(&json), "acl.editor_required");

    let (status, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions"),
        &sid_v,
        &csrf_v,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let list = list.as_array().unwrap();
    let bodies = |s: &Value| -> Vec<String> {
        s["replies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["body"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(list[0]["id"], first);
    assert_eq!(bodies(&list[0]), ["one", "three"]);
    assert_eq!(list[1]["id"], second);
    assert_eq!(bodies(&list[1]), ["two"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_edit_author_only() {
    let (state, ws_id, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    let app = router_with_state(state);
    let (sid_a, csrf_a) = login(&app, "alice@example.com").await;
    let (sid_b, csrf_b) = login(&app, "bob@example.com").await;
    let s = create(&app, doc_id, &sid_a, &csrf_a).await;
    let (_, reply) = post_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions/{s}/replies"),
        &sid_b,
        &csrf_b,
        json!({"body": "bob says"}),
    )
    .await;
    let uri = format!(
        "/api/docs/{doc_id}/suggestions/{s}/replies/{}",
        reply["id"].as_str().unwrap()
    );

    // The owner may not edit someone else's reply.
    let (status, json) = patch_json(&app, &uri, &sid_a, &csrf_a, json!({"body": "x"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(code(&json), "suggestion.not_author");

    let (status, json) =
        patch_json(&app, &uri, &sid_b, &csrf_b, json!({"body": "bob edited"})).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["body"], "bob edited");

    let (status, json) = patch_json(&app, &uri, &sid_b, &csrf_b, json!({"body": ""})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(code(&json), "suggestion.invalid");
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_delete_author_or_owner() {
    let (state, ws_id, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    add_member(
        &state,
        ws_id,
        "carol@example.com",
        "Carol",
        WorkspaceRole::Editor,
    )
    .await;
    let app = router_with_state(state);
    let (sid_a, csrf_a) = login(&app, "alice@example.com").await;
    let (sid_b, csrf_b) = login(&app, "bob@example.com").await;
    let (sid_c, csrf_c) = login(&app, "carol@example.com").await;
    let s = create(&app, doc_id, &sid_a, &csrf_a).await;
    let replies = format!("/api/docs/{doc_id}/suggestions/{s}/replies");
    let mut ids = Vec::new();
    for body in ["bob one", "bob two"] {
        let (_, r) = post_json(&app, &replies, &sid_b, &csrf_b, json!({"body": body})).await;
        ids.push(r["id"].as_str().unwrap().to_string());
    }

    // Another editor may not delete it.
    let status = delete_req(&app, &format!("{replies}/{}", ids[0]), &sid_c, &csrf_c).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The author may.
    let status = delete_req(&app, &format!("{replies}/{}", ids[0]), &sid_b, &csrf_b).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The doc's owner may.
    let status = delete_req(&app, &format!("{replies}/{}", ids[1]), &sid_a, &csrf_a).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, list) = get_json(
        &app,
        &format!("/api/docs/{doc_id}/suggestions"),
        &sid_a,
        &csrf_a,
    )
    .await;
    assert_eq!(list[0]["replies"], json!([]), "deleted replies are hidden");

    // A deleted reply is gone.
    let (status, json) = patch_json(
        &app,
        &format!("{replies}/{}", ids[0]),
        &sid_b,
        &csrf_b,
        json!({"body": "back"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
}

// ---------------------------------------------------------------------------
// Realtime
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn mutation_sends_notify_with_topic() {
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let pool = state.pool.clone().unwrap();
    let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .unwrap();
    listener.listen("doc_comments").await.unwrap();
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    create(&app, doc_id, &sid, &csrf).await;

    let note = tokio::time::timeout(std::time::Duration::from_secs(10), listener.recv())
        .await
        .expect("a doc_comments NOTIFY within 10 s")
        .unwrap();
    let payload: Value = serde_json::from_str(note.payload()).unwrap();
    assert_eq!(payload["doc_id"], doc_id.to_string(), "{payload}");
    assert_eq!(payload["topic"], "suggestions", "{payload}");
}

// ---------------------------------------------------------------------------
// Accept (server-side apply) and the `applying` sweep
// ---------------------------------------------------------------------------

/// The text every accept test seeds its page with, as one paragraph of a
/// fresh doc of client 7: the paragraph is clock 0, its XmlText clock 1 and
/// its characters start at clock 2, so "Hello" is clocks 2..=6.
const SEED_TEXT: &str = "Hello world";

/// Give `state` doc rooms on a MemBus over its Postgres pool, as a replica
/// serving the collab socket has.
fn with_rooms(state: &mut AppState) {
    use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore};
    let pool = state.pool.clone().unwrap();
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool));
    state.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        Arc::new(knot_crdt::YrsEngine),
        Arc::new(knot_crdt::MemBus::new()),
        updates,
        snapshots,
        knot_crdt::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(600),
        },
        std::time::Duration::from_secs(300),
    )));
}

/// A fresh doc of client 7 holding `paragraph > text(SEED_TEXT)`, kept by
/// the test so it can make later edits against the same items.
fn seed_doc() -> yrs::Doc {
    use yrs::{Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};
    let doc = yrs::Doc::with_client_id(7);
    let frag = doc.get_or_insert_xml_fragment("default");
    {
        let mut txn = doc.transact_mut();
        let para = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        para.push_back(&mut txn, XmlTextPrelim::new(SEED_TEXT));
    }
    doc
}

/// Apply `update_bytes` to the doc's room as `by_user`.
async fn apply_to_room(state: &AppState, doc_id: Uuid, update_bytes: Vec<u8>, by_user: Uuid) {
    let room = state
        .rooms_v2
        .as_ref()
        .unwrap()
        .acquire(doc_id)
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(knot_crdt::Event::ApplyUpdate {
            update_bytes,
            by_user: Some(by_user),
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().expect("apply update");
}

/// Seed the doc's room with [`seed_doc`]'s content, credited to `by_user`;
/// returns the seed doc for later edits.
async fn seed_room(state: &AppState, doc_id: Uuid, by_user: Uuid) -> yrs::Doc {
    use yrs::{ReadTxn, StateVector, Transact};
    let doc = seed_doc();
    let update = doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    apply_to_room(state, doc_id, update, by_user).await;
    doc
}

/// The plain text of every XmlText of the room's doc, concatenated.
async fn room_text(state: &AppState, doc_id: Uuid) -> String {
    use yrs::updates::decoder::Decode;
    use yrs::{GetString, Transact, Update, XmlFragment, XmlOut};
    let room = state
        .rooms_v2
        .as_ref()
        .unwrap()
        .acquire(doc_id)
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(knot_crdt::Event::ExportState(tx))
        .await
        .unwrap();
    let (bytes, _seq) = rx.await.unwrap().expect("export state");
    let doc = yrs::Doc::new();
    let frag = doc.get_or_insert_xml_fragment("default");
    doc.transact_mut()
        .apply_update(Update::decode_v1(&bytes).unwrap())
        .unwrap();
    let txn = doc.transact();
    frag.successors(&txn)
        .filter_map(|n| match n {
            XmlOut::Text(t) => Some(t.get_string(&txn)),
            _ => None,
        })
        .collect()
}

/// A replace suggestion of "Hello" (clocks 2..=6 of [`SEED_TEXT`]) with
/// "Howdy".
fn seeded_replace_body() -> Value {
    json!({
        "kind": "replace",
        "position_y": anchor(2, Assoc::After),
        "position_y_end": anchor(6, Assoc::Before),
        "original_text": "Hello",
        "context_before": "",
        "proposed_text": "Howdy",
    })
}

/// Create [`seeded_replace_body`] through the API; returns its id.
async fn create_seeded(app: &axum::Router, doc_id: Uuid, sid: &str, csrf: &str) -> String {
    let (status, json) = post_json(
        app,
        &format!("/api/docs/{doc_id}/suggestions"),
        sid,
        csrf,
        seeded_replace_body(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create: {json}");
    json["id"].as_str().unwrap().to_string()
}

async fn accept(
    app: &axum::Router,
    doc_id: Uuid,
    suggestion_id: &str,
    sid: &str,
    csrf: &str,
    seen_text: &str,
) -> (StatusCode, Value) {
    post_json(
        app,
        &format!("/api/docs/{doc_id}/suggestions/{suggestion_id}/accept"),
        sid,
        csrf,
        json!({ "seen_text": seen_text }),
    )
    .await
}

/// The row's status, read straight from the database.
async fn status_of(state: &AppState, suggestion_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM suggestions WHERE id = $1")
        .bind(Uuid::parse_str(suggestion_id).unwrap())
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap()
}

/// Alice (Owner) with a room seeded with [`SEED_TEXT`] (credited to her),
/// and Bob (Editor) as the suggester, both logged in.
struct AcceptFixture {
    state: AppState,
    app: axum::Router,
    ws_id: Uuid,
    doc_id: Uuid,
    alice: Uuid,
    bob: Uuid,
    seed: yrs::Doc,
    alice_session: (String, String),
    bob_session: (String, String),
}

async fn accept_fixture() -> AcceptFixture {
    let (mut state, ws_id, doc_id, alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let bob = add_bob(&state, ws_id, WorkspaceRole::Editor).await;
    with_rooms(&mut state);
    let seed = seed_room(&state, doc_id, alice).await;
    let app = router_with_state(state.clone());
    let alice_session = login(&app, "alice@example.com").await;
    let bob_session = login(&app, "bob@example.com").await;
    AcceptFixture {
        state,
        app,
        ws_id,
        doc_id,
        alice,
        bob,
        seed,
        alice_session,
        bob_session,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_accept_applies_text() {
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;

    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "Hello").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["id"], s);
    assert_eq!(json["status"], "accepted");
    assert_eq!(json["decided_by"], f.alice.to_string());
    assert!(json["decided_at"].as_str().is_some(), "{json}");
    assert_eq!(room_text(&f.state, f.doc_id).await, "Howdy world");
    assert_eq!(status_of(&f.state, &s).await, "accepted");
}

#[tokio::test(flavor = "multi_thread")]
async fn accept_credits_suggester_in_contributors() {
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;
    let pool = f.state.pool.clone().unwrap();
    let contributors = || {
        let pool = pool.clone();
        let doc_id = f.doc_id;
        async move {
            let mut ids: Vec<Uuid> =
                sqlx::query_scalar("SELECT user_id FROM doc_contributors WHERE doc_id = $1")
                    .bind(doc_id)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            ids.sort();
            ids
        }
    };

    let before = contributors().await;
    assert!(
        before.contains(&f.alice),
        "the seed credits alice: {before:?}"
    );
    assert!(!before.contains(&f.bob), "{before:?}");

    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "Hello").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let after = contributors().await;
    assert!(
        after.contains(&f.bob),
        "the suggester wrote the words: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_doc_accept_is_404_and_leaves_row_pending() {
    let f = accept_fixture().await;
    let (sid_a, csrf_a) = &f.alice_session;
    let doc_b = f
        .state
        .docs
        .as_ref()
        .unwrap()
        .create(f.ws_id, None, "Other Doc", "n", f.alice)
        .await
        .unwrap()
        .id;
    let in_b = create(&f.app, doc_b, sid_a, csrf_a).await;

    let (status, json) = accept(&f.app, f.doc_id, &in_b, sid_a, csrf_a, "Hello").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(code(&json), "suggestion.not_found");
    assert_eq!(status_of(&f.state, &in_b).await, "pending");

    // An unknown id is 404 too.
    let (status, json) = accept(
        &f.app,
        f.doc_id,
        &Uuid::new_v4().to_string(),
        sid_a,
        csrf_a,
        "Hello",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(code(&json), "suggestion.not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn room_unavailable_leaves_row_pending() {
    // No rooms on this state.
    let (state, _ws, doc_id, _alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state.clone());
    let (sid, csrf) = login(&app, "alice@example.com").await;
    let s = create_seeded(&app, doc_id, &sid, &csrf).await;

    let (status, json) = accept(&app, doc_id, &s, &sid, &csrf, "Hello").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json}");
    assert_eq!(status_of(&state, &s).await, "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_cannot_accept() {
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;

    let (status, json) = accept(&f.app, f.doc_id, &s, sid_b, csrf_b, "Hello").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(code(&json), "acl.owner_required");
    assert_eq!(status_of(&f.state, &s).await, "pending");
    assert_eq!(room_text(&f.state, f.doc_id).await, SEED_TEXT);
}

#[tokio::test(flavor = "multi_thread")]
async fn over_long_seen_text_is_422_and_row_untouched() {
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;

    let seen = "a".repeat(10_001);
    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, &seen).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{json}");
    assert_eq!(code(&json), "suggestion.too_long");
    assert_eq!(status_of(&f.state, &s).await, "pending");
    assert_eq!(room_text(&f.state, f.doc_id).await, SEED_TEXT);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_seen_text_is_409_changed_and_row_back_to_pending() {
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;

    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "Hallo").await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(code(&json), "suggestion.changed");
    assert_eq!(status_of(&f.state, &s).await, "pending");
    assert_eq!(room_text(&f.state, f.doc_id).await, SEED_TEXT);

    // With the text the card now shows, a second accept succeeds.
    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "Hello").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(room_text(&f.state, f.doc_id).await, "Howdy world");
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_target_is_409_anchor_lost() {
    use yrs::{ReadTxn, Text, Transact, XmlFragment, XmlOut};
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;

    // Someone deletes "Hello " in the page.
    let update = {
        let frag = f.seed.get_or_insert_xml_fragment("default");
        let sv = f.seed.transact().state_vector();
        {
            let mut txn = f.seed.transact_mut();
            let text = frag
                .successors(&txn)
                .find_map(|n| match n {
                    XmlOut::Text(t) => Some(t),
                    _ => None,
                })
                .unwrap();
            text.remove_range(&mut txn, 0, 6);
        }
        f.seed.transact().encode_state_as_update_v1(&sv)
    };
    apply_to_room(&f.state, f.doc_id, update, f.alice).await;
    assert_eq!(room_text(&f.state, f.doc_id).await, "world");

    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(code(&json), "suggestion.anchor_lost");
    assert_eq!(status_of(&f.state, &s).await, "pending");
    assert_eq!(room_text(&f.state, f.doc_id).await, "world");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_accepts_one_wins() {
    let f = accept_fixture().await;
    add_member(
        &f.state,
        f.ws_id,
        "carol@example.com",
        "Carol",
        WorkspaceRole::Owner,
    )
    .await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let (sid_c, csrf_c) = login(&f.app, "carol@example.com").await;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;

    let (a, c) = tokio::join!(
        accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "Hello"),
        accept(&f.app, f.doc_id, &s, &sid_c, &csrf_c, "Hello"),
    );
    let mut statuses = [a.0, c.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::OK, StatusCode::CONFLICT],
        "{} / {}",
        a.1,
        c.1
    );
    let loser = if a.0 == StatusCode::CONFLICT {
        &a.1
    } else {
        &c.1
    };
    assert_eq!(code(loser), "suggestion.not_pending");
    assert_eq!(room_text(&f.state, f.doc_id).await, "Howdy world");
    assert_eq!(status_of(&f.state, &s).await, "accepted");
}

#[tokio::test(flavor = "multi_thread")]
async fn accept_after_already_applied_finishes() {
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let s = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;
    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "Hello").await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // As if the row had been released after the text landed.
    sqlx::query("UPDATE suggestions SET status = 'pending', decided_by = NULL, decided_at = NULL WHERE id = $1")
        .bind(Uuid::parse_str(&s).unwrap())
        .execute(f.state.pool.as_ref().unwrap())
        .await
        .unwrap();

    let (status, json) = accept(&f.app, f.doc_id, &s, sid_a, csrf_a, "Hello").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["status"], "accepted");
    assert_eq!(json["decided_by"], f.alice.to_string());
    assert_eq!(
        room_text(&f.state, f.doc_id).await,
        "Howdy world",
        "applied once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sweep_finishes_applied_and_releases_unapplied() {
    let f = accept_fixture().await;
    let (sid_b, csrf_b) = &f.bob_session;
    let (sid_a, csrf_a) = &f.alice_session;
    let pool = f.state.pool.clone().unwrap();

    // Applied in the doc, but its row left `applying` (a crash before
    // `finish_accept`).
    let applied = create_seeded(&f.app, f.doc_id, sid_b, csrf_b).await;
    let (status, json) = accept(&f.app, f.doc_id, &applied, sid_a, csrf_a, "Hello").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    // Claimed but never applied (a crash before the room event).
    let unapplied = create(&f.app, f.doc_id, sid_b, csrf_b).await;
    // Claimed just now: too young to sweep.
    let young = create(&f.app, f.doc_id, sid_b, csrf_b).await;
    for (id, age) in [
        (&applied, "5 minutes"),
        (&unapplied, "5 minutes"),
        (&young, "0 seconds"),
    ] {
        sqlx::query(
            "UPDATE suggestions \
             SET status = 'applying', decided_by = $2, decided_at = NULL, \
                 updated_at = now() - $3::interval \
             WHERE id = $1",
        )
        .bind(Uuid::parse_str(id).unwrap())
        .bind(f.alice)
        .bind(age)
        .execute(&pool)
        .await
        .unwrap();
    }

    let out = knot_server::suggestions_sweep::run_once(&f.state, chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(
        out,
        knot_server::suggestions_sweep::SweepOutcome {
            finished: 1,
            released: 1
        }
    );
    assert_eq!(status_of(&f.state, &applied).await, "accepted");
    assert_eq!(status_of(&f.state, &unapplied).await, "pending");
    assert_eq!(status_of(&f.state, &young).await, "applying");
    assert_eq!(room_text(&f.state, f.doc_id).await, "Howdy world");

    let decided_by: Option<Uuid> =
        sqlx::query_scalar("SELECT decided_by FROM suggestions WHERE id = $1")
            .bind(Uuid::parse_str(&applied).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(decided_by, Some(f.alice));

    // A second run finds nothing left to do.
    let out = knot_server::suggestions_sweep::run_once(&f.state, chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(out, knot_server::suggestions_sweep::SweepOutcome::default());
}
