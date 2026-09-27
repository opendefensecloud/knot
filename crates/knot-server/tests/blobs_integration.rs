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

// ---------------------------------------------------------------------------
// Conditional GET: a blob id never changes content, so its sha256 is a strong
// ETag and a revalidation can be answered without reading the bytes.
// ---------------------------------------------------------------------------

/// sha256(TINY_PNG), computed independently (python hashlib).
const TINY_PNG_ETAG: &str = "\"90c86432df503e9bea47ad2d4989a0b5c5911eb60e01fb1baf78c17151cce11f\"";

/// Wraps the real store and counts byte reads, so a test can prove a 304
/// was answered from metadata alone.
struct CountingStore {
    inner: Arc<dyn knot_storage::BlobStore>,
    gets: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl knot_storage::BlobStore for CountingStore {
    async fn put(
        &self,
        id: Uuid,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), knot_storage::BlobStoreError> {
        self.inner.put(id, bytes, content_type).await
    }
    async fn get(&self, id: Uuid) -> Result<Vec<u8>, knot_storage::BlobStoreError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get(id).await
    }
    async fn delete(&self, id: Uuid) -> Result<(), knot_storage::BlobStoreError> {
        self.inner.delete(id).await
    }
}

async fn get_blob(
    app: &axum::Router,
    sid_kv: &str,
    blob_url: &str,
    if_none_match: Option<&str>,
) -> axum::response::Response<Body> {
    let mut request = Request::builder()
        .method("GET")
        .uri(blob_url)
        .header("cookie", sid_kv);
    if let Some(tag) = if_none_match {
        request = request.header("if-none-match", tag);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn upload_tiny_png(app: &axum::Router, sid: &str, csrf: &str, doc_id: Uuid) -> String {
    let r = upload(
        app,
        sid,
        csrf,
        doc_id,
        multipart_body("tiny.png", "image/png", TINY_PNG),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["url"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn download_carries_the_content_sha256_as_etag() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;
    let url = upload_tiny_png(&app, &sid, &csrf, doc_id).await;

    let r = get_blob(&app, &sid, &url, None).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers().get("etag").unwrap(), TINY_PNG_ETAG);
}

#[tokio::test(flavor = "multi_thread")]
async fn matching_if_none_match_is_304_without_reading_the_bytes() {
    let (mut state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let counting = Arc::new(CountingStore {
        inner: state.blob_store.clone().unwrap(),
        gets: Default::default(),
    });
    state.blob_store = Some(counting.clone());
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;
    let url = upload_tiny_png(&app, &sid, &csrf, doc_id).await;

    let r = get_blob(&app, &sid, &url, Some(TINY_PNG_ETAG)).await;
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(r.headers().get("etag").unwrap(), TINY_PNG_ETAG);
    assert!(r.headers().get("cache-control").is_some());
    assert!(r.into_body().collect().await.unwrap().to_bytes().is_empty());
    assert_eq!(counting.gets.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_if_none_match_gets_the_full_body() {
    let (state, _ws, doc_id, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    let app = router_with_state(state);
    let (sid, csrf) = login_alice(&app).await;
    let url = upload_tiny_png(&app, &sid, &csrf, doc_id).await;

    let r = get_blob(&app, &sid, &url, Some("\"not-this-one\", W/\"nor-this\"")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes().as_ref(),
        TINY_PNG
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn matching_if_none_match_does_not_bypass_the_acl() {
    let (state, _ws, _doc, _u) = state_with_seeded(WorkspaceRole::Owner).await;
    // A blob in another workspace: alice has no role there.
    let other_ws = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("other", "Other")
        .await
        .unwrap();
    let outsider = state
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", "$h$")
        .await
        .unwrap();
    let other_doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(other_ws.id, None, "Secret", "m", outsider.id)
        .await
        .unwrap();
    let blob_id = Uuid::new_v4();
    state
        .blob_meta
        .as_ref()
        .unwrap()
        .insert(&knot_storage::BlobMetadata {
            id: blob_id,
            workspace_id: other_ws.id,
            doc_id: other_doc.id,
            content_type: "image/png".into(),
            byte_size: TINY_PNG.len() as i64,
            sha256: vec![
                0x90, 0xc8, 0x64, 0x32, 0xdf, 0x50, 0x3e, 0x9b, 0xea, 0x47, 0xad, 0x2d, 0x49, 0x89,
                0xa0, 0xb5, 0xc5, 0x91, 0x1e, 0xb6, 0x0e, 0x01, 0xfb, 0x1b, 0xaf, 0x78, 0xc1, 0x71,
                0x51, 0xcc, 0xe1, 0x1f,
            ],
            original_name: None,
            created_by: outsider.id,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    state
        .blob_store
        .as_ref()
        .unwrap()
        .put(blob_id, TINY_PNG, "image/png")
        .await
        .unwrap();
    let app = router_with_state(state);
    let (sid, _csrf) = login_alice(&app).await;

    let r = get_blob(
        &app,
        &sid,
        &format!("/api/blobs/{blob_id}"),
        Some(TINY_PNG_ETAG),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}
