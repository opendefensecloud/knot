//! Tower-level oneshot tests for the auth middleware layer.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    response::Response,
    routing::get,
};
use knot_server::auth::{AuthContext, require_session_mw};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;
use uuid::Uuid;

async fn ok() -> &'static str {
    "ok"
}

#[tokio::test]
async fn require_session_rejects_when_no_auth_context() {
    let app = Router::new()
        .route("/x", get(ok))
        .layer(axum::middleware::from_fn(require_session_mw));
    let resp = app
        .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn require_session_allows_when_auth_context_present() {
    // Inject AuthContext into request extensions via a from_fn layer that
    // wraps the request before require_session_mw runs.
    let app = Router::new()
        .route("/x", get(ok))
        // Layer order: from bottom up. The injecting layer must run BEFORE
        // require_session_mw so the extension is visible at the check.
        .layer(axum::middleware::from_fn(require_session_mw))
        .layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(AuthContext {
                    user_id: Uuid::new_v4(),
                    workspace_id: Uuid::new_v4(),
                    role: WorkspaceRole::Owner,
                    root_doc_id: None,
                });
                next.run(req).await
            },
        ));

    let resp = app
        .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

use knot_server::auth::{CSRF_COOKIE, CSRF_HEADER, csrf_mw};

#[tokio::test]
async fn csrf_get_passes_without_token() {
    let app = Router::new()
        .route("/x", get(ok))
        .layer(axum::middleware::from_fn(csrf_mw));
    let resp = app
        .oneshot(Request::builder().uri("/x").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn csrf_unauth_post_passes_without_token() {
    let app = Router::new()
        .route("/x", axum::routing::post(ok))
        .layer(axum::middleware::from_fn(csrf_mw));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn csrf_authed_post_requires_matching_token() {
    // Layer order: csrf (innermost) → injecting (outermost).
    fn inject(
        mut req: Request<Body>,
        next: axum::middleware::Next,
    ) -> impl std::future::Future<Output = Response> + Send {
        req.extensions_mut().insert(AuthContext {
            user_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            role: WorkspaceRole::Owner,
            root_doc_id: None,
        });
        async move { next.run(req).await }
    }

    let make_app = || {
        Router::new()
            .route("/x", axum::routing::post(ok))
            .layer(axum::middleware::from_fn(csrf_mw))
            .layer(axum::middleware::from_fn(inject))
    };

    // No token: 403.
    let bad = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::FORBIDDEN);

    // Matching token: 200.
    let good = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/x")
                .header("cookie", format!("{CSRF_COOKIE}=abc"))
                .header(CSRF_HEADER, "abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(good.status(), StatusCode::OK);
}

use knot_server::AppState;
use knot_server::auth::cookies::{OIDC_FLOW_COOKIE, build_flow_clear_cookie, build_flow_cookie};

#[test]
fn flow_cookie_round_trip_format() {
    let s = AppState::in_memory();
    let cookie = build_flow_cookie(&s, "PAYLOAD");
    assert!(cookie.starts_with(&format!("{OIDC_FLOW_COOKIE}=PAYLOAD;")));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
    assert!(cookie.contains("Max-Age=300"));
}

#[test]
fn flow_clear_cookie_zero_max_age() {
    let c = build_flow_clear_cookie();
    assert!(c.contains("Max-Age=0"));
}
