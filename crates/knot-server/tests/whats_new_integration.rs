//! What's new: the upgrade card's data (`GET /api/whats-new`), marking the
//! running release seen (`POST /api/whats-new/seen`) and the release notes
//! the About page shows (`GET /api/release-notes`).
//!
//! Test binaries are built with KNOT_BUILD_VERSION="dev" unless KNOT_VERSION
//! is set, so every request goes through an app posing as the release the
//! test needs (`AppState::build_version`). The notes are the real
//! CHANGELOG.md, so the tests stick to releases that are already in it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::{UserStore, WorkspaceRole};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "hunter22";

struct Fixture {
    state: AppState,
    user_id: Uuid,
    /// `sid=…; csrf=…`
    cookie: String,
    csrf: String,
}

/// Postgres-backed state with one logged-in member. Their last seen version
/// is NULL, as for every user created after the migration.
async fn fixture() -> Fixture {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool);
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let hash = s.hasher.hash(PASSWORD).unwrap();
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
        .add_member(ws.id, user.id, WorkspaceRole::Viewer)
        .await
        .unwrap();

    let r = router_with_state(s.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": "alice@example.com", "password": PASSWORD}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login failed");
    let kvs: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .filter(|kv| kv.starts_with("sid=") || kv.starts_with("csrf="))
        .collect();
    let csrf = kvs
        .iter()
        .find_map(|kv| kv.strip_prefix("csrf="))
        .expect("login set no csrf cookie")
        .to_string();
    Fixture {
        state: s,
        user_id: user.id,
        cookie: kvs.join("; "),
        csrf,
    }
}

impl Fixture {
    /// The app as a server running `version`.
    fn app(&self, version: &'static str) -> axum::Router {
        let mut s = self.state.clone();
        s.build_version = version;
        router_with_state(s)
    }

    async fn send(&self, version: &'static str, req: Request<Body>) -> (StatusCode, Value) {
        let r = self.app(version).oneshot(req).await.unwrap();
        let status = r.status();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn get(&self, version: &'static str, uri: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", &self.cookie)
            .body(Body::empty())
            .unwrap();
        self.send(version, req).await
    }

    async fn whats_new(&self, version: &'static str) -> Value {
        let (status, body) = self.get(version, "/api/whats-new").await;
        assert_eq!(status, StatusCode::OK, "whats-new on {version}: {body}");
        body
    }

    async fn mark_seen(&self, version: &'static str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/api/whats-new/seen")
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .body(Body::empty())
            .unwrap();
        self.send(version, req).await.0
    }

    fn users(&self) -> &dyn UserStore {
        self.state.users.as_deref().unwrap()
    }

    async fn stored(&self) -> Option<String> {
        self.users().last_seen_version(self.user_id).await.unwrap()
    }

    /// Pose as a user the migration backfilled to `version`.
    async fn store(&self, version: &str) {
        let current = self.stored().await;
        let written = self
            .users()
            .set_last_seen_version(self.user_id, current.as_deref(), version)
            .await
            .unwrap();
        assert!(written);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_route_requires_a_session() {
    let f = fixture().await;
    for (method, uri) in [
        ("GET", "/api/whats-new"),
        ("POST", "/api/whats-new/seen"),
        ("GET", "/api/release-notes"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let (status, body) = f.send("0.8.1", req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_seen_requires_csrf() {
    let f = fixture().await;
    f.store("0.8.0").await;
    let req = Request::builder()
        .method("POST")
        .uri("/api/whats-new/seen")
        .header("cookie", &f.cookie)
        .body(Body::empty())
        .unwrap();
    let (status, _) = f.send("0.8.1", req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.stored().await.as_deref(), Some("0.8.0"));
}

#[tokio::test(flavor = "multi_thread")]
async fn new_user_starts_caught_up() {
    let f = fixture().await;
    assert_eq!(f.stored().await, None);

    let body = f.whats_new("0.8.0").await;
    assert_eq!(body["version"], "0.8.0");
    assert!(body["commit"].as_str().is_some_and(|c| !c.is_empty()));
    assert_eq!(body["released_on"], "2026-10-13");
    assert_eq!(body["dev"], false);
    assert_eq!(body["last_seen_version"], "0.8.0");
    assert_eq!(body["unseen"], json!([]));
    assert_eq!(f.stored().await.as_deref(), Some("0.8.0"));

    // The next upgrade is news to them.
    assert_eq!(f.whats_new("0.8.1").await["unseen"], json!(["0.8.1"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn returning_user_sees_the_releases_they_missed() {
    let f = fixture().await;
    f.store("0.7.0").await;

    let body = f.whats_new("0.8.1").await;
    assert_eq!(body["released_on"], "2026-10-21");
    assert_eq!(body["last_seen_version"], "0.7.0");
    assert_eq!(body["unseen"], json!(["0.8.1", "0.8.0"]));
    // Reading does not mark anything seen.
    assert_eq!(f.stored().await.as_deref(), Some("0.7.0"));

    assert_eq!(f.mark_seen("0.8.1").await, StatusCode::NO_CONTENT);
    let body = f.whats_new("0.8.1").await;
    assert_eq!(body["last_seen_version"], "0.8.1");
    assert_eq!(body["unseen"], json!([]));

    // Idempotent.
    assert_eq!(f.mark_seen("0.8.1").await, StatusCode::NO_CONTENT);
    assert_eq!(f.stored().await.as_deref(), Some("0.8.1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn versions_compare_as_semver() {
    let f = fixture().await;
    // As strings "0.10.0" < "0.8.1"; as versions it is newer, so a server
    // running 0.8.1 must not report or record anything for this user.
    f.store("0.10.0").await;
    assert_eq!(f.whats_new("0.8.1").await["unseen"], json!([]));
    assert_eq!(f.mark_seen("0.8.1").await, StatusCode::NO_CONTENT);
    assert_eq!(f.stored().await.as_deref(), Some("0.10.0"));
}

#[tokio::test(flavor = "multi_thread")]
async fn downgrade_shows_nothing_and_mark_seen_never_lowers() {
    let f = fixture().await;
    f.store("0.8.1").await;

    let body = f.whats_new("0.8.0").await;
    assert_eq!(body["version"], "0.8.0");
    assert_eq!(body["released_on"], "2026-10-13");
    assert_eq!(body["last_seen_version"], "0.8.1");
    assert_eq!(body["unseen"], json!([]));

    assert_eq!(f.mark_seen("0.8.0").await, StatusCode::NO_CONTENT);
    assert_eq!(f.stored().await.as_deref(), Some("0.8.1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_build_reports_nothing_and_writes_nothing() {
    let f = fixture().await;

    let body = f.whats_new("dev").await;
    assert_eq!(body["version"], "dev");
    assert_eq!(body["dev"], true);
    assert_eq!(body["released_on"], Value::Null);
    assert_eq!(body["last_seen_version"], Value::Null);
    assert_eq!(body["unseen"], json!([]));
    assert_eq!(f.mark_seen("dev").await, StatusCode::NO_CONTENT);
    assert_eq!(f.stored().await, None, "a dev build must not initialise");

    f.store("0.7.0").await;
    let body = f.whats_new("dev").await;
    assert_eq!(body["last_seen_version"], "0.7.0");
    assert_eq!(body["unseen"], json!([]));
    assert_eq!(f.mark_seen("dev").await, StatusCode::NO_CONTENT);
    assert_eq!(f.stored().await.as_deref(), Some("0.7.0"));
}

#[tokio::test(flavor = "multi_thread")]
async fn release_notes_list_every_release_newest_first() {
    let f = fixture().await;
    let (status, body) = f.get("dev", "/api/release-notes").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let releases = body["releases"].as_array().unwrap();
    assert!(releases.len() >= 10, "got {} releases", releases.len());

    let versions: Vec<&str> = releases
        .iter()
        .map(|r| r["version"].as_str().unwrap())
        .collect();
    assert!(!versions.contains(&"Unreleased"), "{versions:?}");
    let at = |v: &str| versions.iter().position(|x| *x == v).unwrap();
    assert!(at("0.8.1") < at("0.8.0") && at("0.2.0") < at("0.1.0"));

    let r081 = &releases[at("0.8.1")];
    assert_eq!(r081["date"], "2026-10-21");
    assert_eq!(r081["sections"][0]["title"], "Upgrading");
    let html = r081["sections"][0]["html"].as_str().unwrap();
    assert!(html.starts_with("<ul>"), "{html}");

    // Text above a release's first `###` is a section with an empty title.
    assert_eq!(releases[at("0.2.1")]["sections"][0]["title"], "");

    for section in releases
        .iter()
        .flat_map(|r| r["sections"].as_array().unwrap())
    {
        let html = section["html"].as_str().unwrap();
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("javascript:"), "{html}");
    }
}
