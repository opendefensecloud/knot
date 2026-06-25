//! Integration tests for HTTP hardening: blob nosniff/attachment headers + security headers.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state, security_headers::CSP};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared scaffolding (mirrored from blobs_integration.rs)
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

/// An SVG upload (image/svg+xml) must be coerced to application/octet-stream
/// and served as attachment, never inline — stored XSS mitigation.
#[tokio::test(flavor = "multi_thread")]
async fn svg_blob_is_served_as_attachment_with_nosniff() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;

    // Upload an SVG blob.
    let body = multipart_body("evil.svg", "image/svg+xml", b"<svg></svg>");
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let url = v["url"].as_str().unwrap().to_string();

    // GET the blob and assert the hardened headers.
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
        r.headers()[header::X_CONTENT_TYPE_OPTIONS]
            .to_str()
            .unwrap(),
        "nosniff"
    );
    assert_eq!(
        r.headers()[header::CONTENT_TYPE].to_str().unwrap(),
        "application/octet-stream"
    );
    assert_eq!(
        r.headers()[header::CONTENT_DISPOSITION].to_str().unwrap(),
        "attachment"
    );
}

/// GET /api/healthz (no auth) must carry the full suite of security headers.
#[tokio::test(flavor = "multi_thread")]
async fn security_headers_are_present_on_healthz() {
    let (state, _ws, _doc, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers()["content-security-policy"].to_str().unwrap(),
        CSP,
    );
    assert_eq!(
        r.headers()[header::X_CONTENT_TYPE_OPTIONS]
            .to_str()
            .unwrap(),
        "nosniff",
    );
    assert_eq!(
        r.headers()[header::X_FRAME_OPTIONS].to_str().unwrap(),
        "DENY",
    );
}

/// A PNG blob must still be served inline with its real content-type,
/// but always with nosniff.
#[tokio::test(flavor = "multi_thread")]
async fn png_blob_is_served_inline_with_nosniff() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;

    let body = multipart_body("tiny.png", "image/png", TINY_PNG);
    let r = upload(&app, &sid, &csrf, doc_id, body).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let url = v["url"].as_str().unwrap().to_string();

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
        r.headers()[header::CONTENT_TYPE].to_str().unwrap(),
        "image/png"
    );
    assert_eq!(
        r.headers()[header::CONTENT_DISPOSITION].to_str().unwrap(),
        "inline"
    );
    assert_eq!(
        r.headers()[header::X_CONTENT_TYPE_OPTIONS]
            .to_str()
            .unwrap(),
        "nosniff"
    );
}
