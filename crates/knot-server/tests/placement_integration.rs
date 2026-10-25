//! Where new and moved pages land among their siblings, over HTTP against a
//! fresh database per test.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use serde_json::{Value, json};
use tower::ServiceExt;

const EMAIL: &str = "owner@placement.test";
const PASSWORD: &str = "hunter22hunter22";

/// The router plus a logged-in owner's cookie header and CSRF token.
struct World {
    app: axum::Router,
    cookie: String,
    csrf: String,
}

async fn world() -> World {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut state = AppState::with_pool(pool);
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let hash = state.hasher.hash(PASSWORD).unwrap();
    let owner = state
        .users
        .as_ref()
        .unwrap()
        .create_local(EMAIL, "Olive Owner", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let app = router_with_state(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": EMAIL, "password": PASSWORD}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "login");
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    let sid = cookies.iter().find(|c| c.starts_with("sid=")).unwrap();
    let csrf = cookies.iter().find(|c| c.starts_with("csrf=")).unwrap();
    World {
        app,
        cookie: format!("{sid}; {csrf}"),
        csrf: csrf.trim_start_matches("csrf=").to_string(),
    }
}

impl World {
    /// One request as the owner; the JSON body, or `Null` when there is none.
    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .header("x-csrf-token", &self.csrf)
                    .header("content-type", "application/json")
                    .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// `POST /api/docs`, asserting 201; returns the new page's id.
    async fn create(&self, body: Value) -> String {
        let (status, doc) = self.send("POST", "/api/docs", Some(body.clone())).await;
        assert_eq!(status, StatusCode::CREATED, "create {body}: {doc}");
        doc["id"].as_str().unwrap().to_string()
    }

    /// The live children of `parent` (`None`: the top level) as
    /// `(title, sort_key)`, in the sidebar's order: by key, code unit by
    /// code unit.
    async fn level(&self, parent: Option<&str>) -> Vec<(String, String)> {
        let (status, docs) = self.send("GET", "/api/docs", None).await;
        assert_eq!(status, StatusCode::OK, "list: {docs}");
        let mut level: Vec<(String, String)> = docs
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["parent_id"].as_str() == parent)
            .map(|d| {
                let title = d["title"].as_str().unwrap().to_string();
                (title, d["sort_key"].as_str().unwrap().to_string())
            })
            .collect();
        level.sort_by(|x, y| x.1.cmp(&y.1));
        level
    }
}

/// Titles of `level`, after asserting its keys are pairwise distinct.
fn distinct_titles(level: &[(String, String)]) -> Vec<&str> {
    assert!(
        level.windows(2).all(|w| w[0].1 < w[1].1),
        "duplicate sort keys: {level:?}"
    );
    level.iter().map(|(title, _)| title.as_str()).collect()
}

/// "New page" under a parent puts each page first. The old generator's 8th
/// key duplicated the 7th, so the 8th create failed with 409.
#[tokio::test(flavor = "multi_thread")]
async fn ten_new_pages_under_a_parent_all_succeed_in_order() {
    let w = world().await;
    let parent = w.create(json!({"title": "Parent"})).await;
    for i in 1..=10 {
        w.create(json!({"title": format!("Page {i}"), "parent_id": parent}))
            .await;
    }
    let level = w.level(Some(&parent)).await;
    let expected: Vec<String> = (1..=10).rev().map(|i| format!("Page {i}")).collect();
    assert_eq!(distinct_titles(&level), expected);
}

/// The same at the top level, where the database accepted the duplicate key:
/// from the 7th page on, every new page got the same "0m" key.
#[tokio::test(flavor = "multi_thread")]
async fn ten_new_top_level_pages_get_distinct_ordered_keys() {
    let w = world().await;
    for i in 1..=10 {
        w.create(json!({"title": format!("Page {i}")})).await;
    }
    let level = w.level(None).await;
    let expected: Vec<String> = (1..=10).rev().map(|i| format!("Page {i}")).collect();
    assert_eq!(distinct_titles(&level), expected);
}

/// "Insert after" the first page, ten times: each new page goes right after
/// it. The old generator returned the neighbour's own key on the 7th try, so
/// that create failed with 409.
#[tokio::test(flavor = "multi_thread")]
async fn insert_after_first_ten_times() {
    let w = world().await;
    let parent = w.create(json!({"title": "Parent"})).await;
    let first = w
        .create(json!({"title": "First", "parent_id": parent}))
        .await;
    for i in 1..=10 {
        w.create(json!({
            "title": format!("After {i}"),
            "parent_id": parent,
            "after_id": first,
        }))
        .await;
    }
    let level = w.level(Some(&parent)).await;
    let mut expected = vec!["First".to_string()];
    expected.extend((1..=10).rev().map(|i| format!("After {i}")));
    assert_eq!(distinct_titles(&level), expected);
}
