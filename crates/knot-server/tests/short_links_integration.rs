//! Short page links: create/read under the doc ACL, resolve with one 404
//! for every case that must not reveal a page.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;
use uuid::Uuid;

struct Fx {
    app: axum::Router,
    base_url: String,
    doc: Uuid,
    owner: (String, String),
    viewer: (String, String),
    editor: (String, String),
}

/// A workspace with an owner (who created `doc`), a viewer and an editor,
/// each signed in. Returns their `(sid=…, csrf)` pairs.
async fn fixture() -> Fx {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let hash = s.hasher.hash("hunter22").unwrap();
    let ws = s.workspaces.as_ref().unwrap();
    let w = ws.create("default", "Workspace").await.unwrap();
    let mut ids = Vec::new();
    for (email, role) in [
        ("owner@example.com", WorkspaceRole::Owner),
        ("viewer@example.com", WorkspaceRole::Viewer),
        ("editor@example.com", WorkspaceRole::Editor),
    ] {
        let u = s
            .users
            .as_ref()
            .unwrap()
            .create_local(email, email, &hash)
            .await
            .unwrap();
        ws.add_member(w.id, u.id, role).await.unwrap();
        ids.push(u.id);
    }
    let doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(w.id, None, "Linked", "m", ids[0])
        .await
        .unwrap();
    let base_url = s.base_url.clone();
    let app = router_with_state(s);
    Fx {
        owner: login(&app, "owner@example.com").await,
        viewer: login(&app, "viewer@example.com").await,
        editor: login(&app, "editor@example.com").await,
        app,
        base_url,
        doc: doc.id,
    }
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
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let cookie = |name: &str| {
        r.headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap())
            .find(|c| c.starts_with(&format!("{name}=")))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    };
    let csrf = cookie("csrf").split_once('=').unwrap().1.to_string();
    (cookie("sid"), csrf)
}

/// `(status, raw body)`; `who = None` sends no cookies.
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    who: Option<&(String, String)>,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some((sid, csrf)) = who {
        b = b
            .header("cookie", format!("{sid}; csrf={csrf}"))
            .header("x-csrf-token", csrf);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let r = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = r.status();
    (
        status,
        r.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or(serde_json::json!({}))
}

fn error_code(bytes: &[u8]) -> String {
    json(bytes)["error"]["code"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

async fn create(fx: &Fx, doc: Uuid, who: &(String, String)) -> (StatusCode, serde_json::Value) {
    let (s, b) = call(
        &fx.app,
        "POST",
        &format!("/api/docs/{doc}/short-link"),
        Some(who),
        None,
    )
    .await;
    (s, json(&b))
}

async fn archive(fx: &Fx, doc: Uuid) {
    let (s, _) = call(
        &fx.app,
        "DELETE",
        &format!("/api/docs/{doc}"),
        Some(&fx.owner),
        None,
    )
    .await;
    assert!(s.is_success(), "archive: {s}");
}

async fn second_doc(fx: &Fx) -> Uuid {
    let (s, b) = call(
        &fx.app,
        "POST",
        "/api/docs",
        Some(&fx.owner),
        Some(serde_json::json!({"title": "Second"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    json(&b)["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_can_create_and_gets_same_link_twice() {
    let fx = fixture().await;
    let (s1, a) = create(&fx, fx.doc, &fx.viewer).await;
    let (s2, b) = create(&fx, fx.doc, &fx.viewer).await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK), "{a} {b}");
    let code = a["code"].as_str().unwrap();
    assert!(knot_storage::is_short_code(code), "{code}");
    assert_eq!(b["code"], a["code"]);
    assert_eq!(
        a["url"].as_str().unwrap(),
        format!("{}/{}", fx.base_url.trim_end_matches('/'), code)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_does_not_create() {
    let fx = fixture().await;
    let uri = format!("/api/docs/{}/short-link", fx.doc);
    let (s, b) = call(&fx.app, "GET", &uri, Some(&fx.viewer), None).await;
    assert_eq!(
        (s, error_code(&b).as_str()),
        (StatusCode::NOT_FOUND, "short_link.none")
    );
    let (_, created) = create(&fx, fx.doc, &fx.viewer).await;
    let (s, b) = call(&fx.app, "GET", &uri, Some(&fx.viewer), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json(&b)["code"], created["code"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_page_keeps_existing_link_and_gets_no_new_one() {
    let fx = fixture().await;
    let (_, created) = create(&fx, fx.doc, &fx.editor).await;
    let other = second_doc(&fx).await;
    archive(&fx, fx.doc).await;
    archive(&fx, other).await;

    let (s, b) = call(
        &fx.app,
        "GET",
        &format!("/api/docs/{}/short-link", fx.doc),
        Some(&fx.editor),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json(&b)["code"], created["code"]);

    let (s, b) = call(
        &fx.app,
        "POST",
        &format!("/api/docs/{other}/short-link"),
        Some(&fx.editor),
        None,
    )
    .await;
    assert_eq!(
        (s, error_code(&b).as_str()),
        (StatusCode::CONFLICT, "doc.archived")
    );
    let (s, b) = call(
        &fx.app,
        "GET",
        &format!("/api/docs/{other}/short-link"),
        Some(&fx.editor),
        None,
    )
    .await;
    assert_eq!(
        (s, error_code(&b).as_str()),
        (StatusCode::NOT_FOUND, "short_link.none")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_returns_doc_for_a_reader() {
    let fx = fixture().await;
    let (_, created) = create(&fx, fx.doc, &fx.owner).await;
    let code = created["code"].as_str().unwrap();
    let (s, b) = call(
        &fx.app,
        "GET",
        &format!("/api/short-links/{code}"),
        Some(&fx.viewer),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json(&b)["doc_id"], fx.doc.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_hides_restricted_page_like_unknown_code() {
    let fx = fixture().await;
    let (_, created) = create(&fx, fx.doc, &fx.owner).await;
    let code = created["code"].as_str().unwrap().to_string();
    let (s, _) = call(
        &fx.app,
        "PUT",
        &format!("/api/docs/{}/restricted", fx.doc),
        Some(&fx.owner),
        Some(serde_json::json!({"restricted": true})),
    )
    .await;
    assert!(s.is_success(), "restrict: {s}");

    let restricted = call(
        &fx.app,
        "GET",
        &format!("/api/short-links/{code}"),
        Some(&fx.viewer),
        None,
    )
    .await;
    let unknown = call(
        &fx.app,
        "GET",
        "/api/short-links/Zz9Zz9Z",
        Some(&fx.viewer),
        None,
    )
    .await;
    let malformed = call(
        &fx.app,
        "GET",
        "/api/short-links/nope",
        Some(&fx.viewer),
        None,
    )
    .await;
    assert_eq!(restricted.0, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&restricted.1), "short_link.not_found");
    assert_eq!(restricted, unknown);
    assert_eq!(restricted, malformed);
}

/// The link went out first; then the page moves under a restricted parent.
#[tokio::test(flavor = "multi_thread")]
async fn resolve_hides_page_moved_under_a_restricted_parent() {
    let fx = fixture().await;
    let (_, created) = create(&fx, fx.doc, &fx.owner).await;
    let code = created["code"].as_str().unwrap().to_string();
    let uri = format!("/api/short-links/{code}");
    assert_eq!(
        call(&fx.app, "GET", &uri, Some(&fx.viewer), None).await.0,
        StatusCode::OK
    );

    let parent = second_doc(&fx).await;
    let (s, _) = call(
        &fx.app,
        "PUT",
        &format!("/api/docs/{parent}/restricted"),
        Some(&fx.owner),
        Some(serde_json::json!({"restricted": true})),
    )
    .await;
    assert!(s.is_success(), "restrict: {s}");
    let (s, b) = call(
        &fx.app,
        "POST",
        &format!("/api/docs/{}/move", fx.doc),
        Some(&fx.owner),
        Some(serde_json::json!({"parent_id": parent})),
    )
    .await;
    assert!(s.is_success(), "move: {s} {}", String::from_utf8_lossy(&b));

    let moved = call(&fx.app, "GET", &uri, Some(&fx.viewer), None).await;
    let unknown = call(
        &fx.app,
        "GET",
        "/api/short-links/Zz9Zz9Z",
        Some(&fx.viewer),
        None,
    )
    .await;
    assert_eq!(moved.0, StatusCode::NOT_FOUND);
    assert_eq!(moved, unknown);
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_hides_archived_page_from_viewer() {
    let fx = fixture().await;
    let (_, created) = create(&fx, fx.doc, &fx.owner).await;
    let code = created["code"].as_str().unwrap().to_string();
    archive(&fx, fx.doc).await;
    let uri = format!("/api/short-links/{code}");
    let viewer = call(&fx.app, "GET", &uri, Some(&fx.viewer), None).await;
    let unknown = call(
        &fx.app,
        "GET",
        "/api/short-links/Zz9Zz9Z",
        Some(&fx.viewer),
        None,
    )
    .await;
    assert_eq!(viewer.0, StatusCode::NOT_FOUND);
    assert_eq!(viewer, unknown);
    let (s, b) = call(&fx.app, "GET", &uri, Some(&fx.editor), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json(&b)["doc_id"], fx.doc.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_requires_session() {
    let fx = fixture().await;
    let (s, b) = call(&fx.app, "GET", "/api/short-links/Zz9Zz9Z", None, None).await;
    assert_eq!(
        (s, error_code(&b).as_str()),
        (StatusCode::UNAUTHORIZED, "auth.session_required")
    );
}
