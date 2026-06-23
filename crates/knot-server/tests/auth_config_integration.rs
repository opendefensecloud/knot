use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::Hasher;
use knot_server::{AppState, router_with_state};
use tower::ServiceExt;

async fn fresh_state() -> AppState {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool);
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s
}

async fn get_config(app: &axum::Router) -> serde_json::Value {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reports_setup_available_until_first_user() {
    let state = fresh_state().await;
    let app = router_with_state(state);

    let v = get_config(&app).await;
    assert_eq!(v["setup_available"], true);
    assert_eq!(v["oidc_enabled"], false);
    assert_eq!(v["password_login_enabled"], true);

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
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let v = get_config(&app).await;
    assert_eq!(v["setup_available"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reports_oidc_enabled_from_state() {
    let mut state = fresh_state().await;
    state.oidc_enabled = true;
    let app = router_with_state(state);

    let v = get_config(&app).await;
    assert_eq!(v["oidc_enabled"], true);
}
