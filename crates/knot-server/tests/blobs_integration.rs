//! Integration tests for blob upload / download / delete + ACL checks.

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
// Shared scaffolding
// ---------------------------------------------------------------------------

const BOUNDARY: &str = "----PlaywrightFormBoundary";

fn multipart_body(filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn ct_multipart() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

/// Seed a workspace + alice user with the given role, create a doc in the workspace.
/// Returns `(state, workspace_id, doc_id, user_id)`.
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

    // DocStore::create(workspace_id, parent_id, title, sort_key, created_by)
    let doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Test Doc", "m", user.id)
        .await
        .unwrap();

    (s, ws.id, doc.id, user.id)
}

/// Log in as alice and return `(sid_kv, csrf_val)`.
async fn login_alice(app: &axum::Router) -> (String, String) {
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

async fn upload(
    app: &axum::Router,
    sid_kv: &str,
    csrf_val: &str,
    doc_id: Uuid,
    body: Vec<u8>,
) -> axum::response::Response<Body> {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc_id}/blobs"))
                .header("content-type", ct_multipart())
                .header("cookie", format!("{sid_kv}; csrf={csrf_val}"))
                .header("x-csrf-token", csrf_val)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// Minimal valid 1×1 PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x60, 0x60, 0x00, 0x00,
    0x00, 0x00, 0x04, 0x00, 0x01, 0x5C, 0x5B, 0x66, 0xE3, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn owner_uploads_png_and_downloads_it() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;

    let body = multipart_body("tiny.png", "image/png", TINY_PNG);
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let url = v["url"].as_str().unwrap().to_string();
    assert_eq!(v["content_type"], "image/png");
    assert_eq!(v["byte_size"].as_i64().unwrap(), TINY_PNG.len() as i64);

    // GET it back.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&url)
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers().get("content-type").unwrap().to_str().unwrap(),
        "image/png"
    );
    let body = r.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.as_ref(), TINY_PNG);
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_over_10mb_is_413() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;

    let big = vec![0u8; 11 * 1024 * 1024];
    let body = multipart_body("big.bin", "application/octet-stream", &big);
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "blob.too_large");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_blocked_content_type_is_415() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;

    let body = multipart_body("evil.exe", "application/x-msdos-program", b"MZ\x90\x00");
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "blob.blocked_type");
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_upload() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Viewer).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;
    let body = multipart_body("a.png", "image/png", TINY_PNG);
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn anon_get_returns_401() {
    let (state, _ws, _doc, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let r = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/blobs/{}", Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_delete_then_get_is_404() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;

    // Upload.
    let body = multipart_body("a.png", "image/png", TINY_PNG);
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let resp_bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&resp_bytes).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    // Delete.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/blobs/{id}"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // Subsequent GET → 404.
    let r = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/blobs/{id}"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_file_is_400() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;

    let body = multipart_body("empty.bin", "application/octet-stream", b"");
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "blob.empty");
}
