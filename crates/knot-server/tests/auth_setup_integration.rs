use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::Hasher;
use knot_server::{AppState, router_with_state};
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};
use tower::ServiceExt;

async fn fresh_state() -> AppState {
    let container = Postgres::default().start().await.expect("pg start");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
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
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_first_user_then_closes() {
    let state = fresh_state().await;
    let app = router_with_state(state);

    let body = serde_json::json!({
        "email": "admin@example.com",
        "password": "hunter2!hunter2",
        "display_name": "Admin",
    })
    .to_string();

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/setup")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    // SET_COOKIE may appear twice: sid + csrf. Both must be present.
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(
        cookies.iter().any(|c| c.starts_with("sid=")),
        "no sid cookie: {cookies:?}"
    );
    assert!(
        cookies.iter().any(|c| c.starts_with("csrf=")),
        "no csrf cookie: {cookies:?}"
    );

    // Second call: 410.
    let r2 = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/setup")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::GONE);
    let body = r2.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.setup_closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_rejects_short_password() {
    let state = fresh_state().await;
    let app = router_with_state(state);
    let body = serde_json::json!({
        "email": "a@x.test", "password": "short", "display_name": "A",
    })
    .to_string();
    let r = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/setup")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.weak_password");
}
