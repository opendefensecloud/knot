//! Integration tests for doc history (snapshots): list, preview, restore, ACL.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Engine, Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build full-state update bytes from a markdown string via a transient yrs doc.
fn snapshot_bytes_from_md(md: &str) -> (Vec<u8>, Vec<u8>) {
    #[allow(unused_imports)]
    use yrs::ReadTxn;
    // Use knot_markdown for proper schema, but avoid the circular dep issue.
    // We use raw yrs to create a simple doc with a "default" fragment.
    let (_handle, update_bytes) = knot_markdown::from_markdown::parse(md).unwrap();
    // Build a transient doc and apply the update to get state_bytes.
    let engine = YrsEngine;
    let doc = engine.new_doc();
    engine.apply_update(&doc, &update_bytes).unwrap();
    let state_bytes = engine.encode_state_as_update(&doc, None).unwrap();
    let state_vector = engine.encode_state_vector(&doc).unwrap();
    (state_bytes, state_vector)
}

/// Seed a workspace + alice user + doc, wire snapshots into AppState.
/// Returns `(state, ws_id, doc_id, user_id)`.
async fn seeded_state(role: WorkspaceRole) -> (AppState, Uuid, Uuid, Uuid) {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

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
        .create(ws.id, None, "History Doc", "m", user.id)
        .await
        .unwrap();

    (s, ws.id, doc.id, user.id)
}

/// Like `seeded_state` but also adds a Viewer user ("bob@example.com").
async fn seeded_with_viewer() -> (AppState, Uuid, Uuid, Uuid, Uuid) {
    let (s, ws_id, doc_id, owner_id) = seeded_state(WorkspaceRole::Owner).await;
    let hash = s.hasher.hash("hunter22").unwrap();
    let viewer = s
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws_id, viewer.id, WorkspaceRole::Viewer)
        .await
        .unwrap();
    (s, ws_id, doc_id, owner_id, viewer.id)
}

/// Spin up a real Rooms registry wired to the test DB (needed for restore).
async fn wire_rooms(state: &mut AppState, db_url: &str) {
    let pool = state.pool.as_ref().unwrap().clone();
    let bus = Arc::new(knot_crdt::PgBus::connect(db_url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    let policy = SnapshotPolicy {
        every_n: 1000,
        idle: Duration::from_secs(3600),
    };
    let rooms = Arc::new(Rooms::new(
        Arc::new(YrsEngine),
        bus.clone(),
        updates,
        snaps,
        policy,
        Duration::from_secs(3600),
    ));
    state.bus = Some(bus);
    state.rooms_v2 = Some(rooms);
}

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
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login failed");
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

async fn get_history(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/docs/{doc_id}/history"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Array(vec![]));
    (status, json)
}

async fn get_history_markdown(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
    seq: i64,
) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/docs/{doc_id}/history/{seq}/markdown"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn post_restore(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
    seq: i64,
) -> StatusCode {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc_id}/history/{seq}/restore"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    r.status()
}

async fn get_markdown(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/docs/{doc_id}/markdown"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 1. List returns empty array when no snapshots exist.
#[tokio::test(flavor = "multi_thread")]
async fn history_list_empty_when_no_snapshots() {
    let (state, _ws_id, doc_id, _user_id) = seeded_state(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let (status, json) = get_history(&app, &sid, &csrf, doc_id).await;
    assert_eq!(status, StatusCode::OK, "expected 200, got {status}");
    assert_eq!(
        json,
        serde_json::json!([]),
        "expected empty list, got {json}"
    );
}

/// 2. List returns snapshots sorted newest-first by snapshot_seq.
#[tokio::test(flavor = "multi_thread")]
async fn history_list_sorted_newest_first() {
    let (state, _ws_id, doc_id, _user_id) = seeded_state(WorkspaceRole::Owner).await;
    let pool = state.pool.as_ref().unwrap().clone();
    let snaps = PgSnapshotStore::new(pool.clone());

    let (bytes1, sv1) = snapshot_bytes_from_md("# First");
    let (bytes2, sv2) = snapshot_bytes_from_md("# Second");
    let (bytes3, sv3) = snapshot_bytes_from_md("# Third");
    snaps.insert(doc_id, 1, &bytes1, &sv1).await.unwrap();
    snaps.insert(doc_id, 2, &bytes2, &sv2).await.unwrap();
    snaps.insert(doc_id, 3, &bytes3, &sv3).await.unwrap();

    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let (status, json) = get_history(&app, &sid, &csrf, doc_id).await;
    assert_eq!(status, StatusCode::OK);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 3, "expected 3 snapshots");
    // Newest first: seq 3, 2, 1
    assert_eq!(arr[0]["snapshot_seq"], 3);
    assert_eq!(arr[1]["snapshot_seq"], 2);
    assert_eq!(arr[2]["snapshot_seq"], 1);
}

/// 3. Preview returns markdown content from the seeded snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn history_preview_returns_markdown() {
    let (state, _ws_id, doc_id, _user_id) = seeded_state(WorkspaceRole::Owner).await;
    let pool = state.pool.as_ref().unwrap().clone();
    let snaps = PgSnapshotStore::new(pool.clone());

    let (bytes, sv) = snapshot_bytes_from_md("# Preview Test\n\nHello preview.");
    snaps.insert(doc_id, 1, &bytes, &sv).await.unwrap();

    let app = router_with_state(state);
    let (sid, csrf) = login(&app, "alice@example.com").await;

    let (status, md) = get_history_markdown(&app, &sid, &csrf, doc_id, 1).await;
    assert_eq!(status, StatusCode::OK, "expected 200, got {status}: {md}");
    assert!(
        md.contains("Preview Test"),
        "expected 'Preview Test' in markdown: {md:?}"
    );
    assert!(
        md.contains("Hello preview"),
        "expected 'Hello preview' in markdown: {md:?}"
    );
}

/// 4. Restore replaces doc content — GET /markdown after restore returns snapshot content.
#[tokio::test(flavor = "multi_thread")]
async fn history_restore_replaces_content() {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut state = AppState::with_pool(pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = state.hasher.hash("hunter22").unwrap();
    let ws = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "Workspace")
        .await
        .unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Restore Doc", "m", user.id)
        .await
        .unwrap();
    let doc_id = doc.id;

    // Seed a snapshot with known content.
    let snaps = PgSnapshotStore::new(pool.clone());
    let (snap_bytes, snap_sv) = snapshot_bytes_from_md("# Snapshot Content\n\nOriginal text.");
    snaps
        .insert(doc_id, 1, &snap_bytes, &snap_sv)
        .await
        .unwrap();

    // Wire rooms so restore can acquire the room and apply the replace.
    wire_rooms(&mut state, &db.url).await;

    // Also set a markdown cache (required by export_inline path through rooms).
    // AppState::with_pool already wires markdown_cache.

    let app = router_with_state(state.clone());
    let (sid, csrf) = login(&app, "alice@example.com").await;

    // First: apply some content via the markdown import endpoint to simulate
    // a "current" state that differs from the snapshot.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc_id}/markdown"))
                .header("content-type", "text/markdown")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", &csrf)
                .body(Body::from("# Current Content\n\nThis should be replaced."))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "markdown import failed");

    // Now restore to snapshot seq=1.
    let status = post_restore(&app, &sid, &csrf, doc_id, 1).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "restore failed with {status}"
    );

    // Export current markdown; should match the snapshot content.
    // Give the room a brief moment to process (it's async).
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (status, md) = get_markdown(&app, &sid, &csrf, doc_id).await;
    assert_eq!(status, StatusCode::OK, "get markdown failed: {md}");
    assert!(
        md.contains("Snapshot Content"),
        "expected 'Snapshot Content' after restore, got: {md:?}"
    );
    assert!(
        !md.contains("Current Content"),
        "expected 'Current Content' to be gone after restore, got: {md:?}"
    );
}

/// 5. Viewer cannot list, preview, or restore (403).
#[tokio::test(flavor = "multi_thread")]
async fn history_viewer_gets_403() {
    let (state, _ws_id, doc_id, _owner_id, _viewer_id) = seeded_with_viewer().await;

    // Seed one snapshot so the list/preview endpoints have something to return.
    {
        let pool = state.pool.as_ref().unwrap().clone();
        let snaps = PgSnapshotStore::new(pool);
        let (bytes, sv) = snapshot_bytes_from_md("# Test");
        snaps.insert(doc_id, 1, &bytes, &sv).await.unwrap();
    }

    let app = router_with_state(state);

    // Log in as the viewer (bob).
    let (sid, csrf) = login(&app, "bob@example.com").await;

    let (status, _) = get_history(&app, &sid, &csrf, doc_id).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "list: expected 403 for viewer"
    );

    let (status, _) = get_history_markdown(&app, &sid, &csrf, doc_id, 1).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "preview: expected 403 for viewer"
    );

    let status = post_restore(&app, &sid, &csrf, doc_id, 1).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "restore: expected 403 for viewer"
    );
}

/// 6. Anonymous request (no session) returns 401.
#[tokio::test(flavor = "multi_thread")]
async fn history_anon_gets_401() {
    let (state, _ws_id, doc_id, _user_id) = seeded_state(WorkspaceRole::Owner).await;
    let app = router_with_state(state);

    // No cookie at all.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/docs/{doc_id}/history"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::UNAUTHORIZED,
        "expected 401 for anon"
    );
}
