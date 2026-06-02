//! End-to-end test for /api/docs CRUD via the HTTP layer.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};
use tower::ServiceExt;

async fn login_state(email: &str, password: &str) -> (AppState, String) {
    let container = Postgres::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    std::mem::forget(container);

    let mut s = AppState::with_pool(pool);
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = s.hasher.hash(password).unwrap();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let u = s
        .users
        .as_ref()
        .unwrap()
        .create_local(email, "U", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    // Log in via the HTTP layer to capture the sid + csrf cookies.
    let app = router_with_state(s.clone());
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
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
    let csrf_kv = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let cookie_header = format!("{sid_kv}; {csrf_kv}");
    let csrf_token = csrf_kv.trim_start_matches("csrf=").to_string();
    (s, format!("{cookie_header}|{csrf_token}"))
}

fn split_cookie_csrf(joined: &str) -> (&str, &str) {
    joined.split_once('|').unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn docs_crud_happy_path() {
    let (state, joined) = login_state("a@x.test", "hunter22").await;
    let (cookie, csrf) = split_cookie_csrf(&joined);
    let app = router_with_state(state);

    // Create
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/docs")
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"title": "Hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    // List
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/docs")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let arr: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);

    // Get one
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/docs/{id}"))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["title"], "Hello");
    assert_eq!(v["effective_role"], "owner");
}

#[tokio::test(flavor = "multi_thread")]
async fn move_with_unknown_anchor_falls_to_end() {
    let (state, joined) = login_state("a@x.test", "hunter22").await;
    let (cookie, csrf) = split_cookie_csrf(&joined);
    let app = router_with_state(state);

    // Create three top-level docs A, B, C in order.
    async fn create_doc(
        app: &axum::Router,
        cookie: &str,
        csrf: &str,
        title: &str,
    ) -> serde_json::Value {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/docs")
                    .header("cookie", cookie)
                    .header("x-csrf-token", csrf)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "title": title }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED, "create {title}");
        let body = r.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }
    let a = create_doc(&app, cookie, csrf, "A").await;
    let _b = create_doc(&app, cookie, csrf, "B").await;
    let c = create_doc(&app, cookie, csrf, "C").await;

    // Move A with an after_id that does NOT refer to one of its siblings
    // (use a freshly minted UUID). Expect it to land at the END of the
    // sibling list (sort_key > C's sort_key), NOT at the first slot.
    let nonsense_anchor = uuid::Uuid::new_v4();
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{}/move", a["id"].as_str().unwrap()))
                .header("cookie", cookie)
                .header("x-csrf-token", csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "after_id": nonsense_anchor.to_string() }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let moved_body = r.into_body().collect().await.unwrap().to_bytes();
    let moved: serde_json::Value = serde_json::from_slice(&moved_body).unwrap();
    let moved_key = moved["sort_key"].as_str().unwrap();
    let c_key = c["sort_key"].as_str().unwrap();

    assert!(
        moved_key > c_key,
        "with unknown after_id, doc A should land at the end (key {moved_key} > C's key {c_key})",
    );
}
