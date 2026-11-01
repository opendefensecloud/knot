//! Personal API tokens end to end: minting over the session-authed
//! management routes, bearer use across `/api/*`, scope enforcement,
//! and revocation.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "hunter22";

struct Fixture {
    state: AppState,
    /// `sid=…`
    cookie: String,
    csrf: String,
}

/// Postgres-backed state with one logged-in editor.
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
        .add_member(ws.id, user.id, WorkspaceRole::Editor)
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
        .collect();
    let sid = kvs.iter().find(|c| c.starts_with("sid=")).unwrap().clone();
    let csrf = kvs
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
        .split('=')
        .nth(1)
        .unwrap()
        .to_string();
    Fixture {
        state: s,
        cookie: format!("{}; csrf={csrf}", sid),
        csrf,
    }
}

/// Authenticated session request (cookie + CSRF on unsafe methods).
fn sess(f: &Fixture, method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", &f.cookie)
        .header("x-csrf-token", &f.csrf);
    if let Some(body) = body {
        b = b.header("content-type", "application/json");
        b.body(Body::from(body.to_string())).unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    }
}

/// Bearer-authenticated request: no cookies, no CSRF header — tokens must
/// work without either.
fn token_req(secret: &str, method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {secret}"));
    if let Some(body) = body {
        b = b.header("content-type", "application/json");
        b.body(Body::from(body.to_string())).unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    }
}

async fn mint(f: &Fixture, name: &str, scopes: &[&str]) -> (String, String) {
    let r = router_with_state(f.state.clone())
        .oneshot(sess(
            f,
            "POST",
            "/api/tokens",
            Some(json!({"name": name, "scopes": scopes})),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "token mint failed");
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    (
        v["id"].as_str().unwrap().to_string(),
        v["token_secret"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn full_lifecycle_and_scope_enforcement() {
    let f = fixture().await;
    let app = || router_with_state(f.state.clone());

    // --- mint a broad token over the session-authed route ---
    let (id, secret) = mint(&f, "docs bot", &["read", "create", "write"]).await;
    assert!(secret.starts_with("knot_") && secret.len() > 40);

    // list shows the token, never the secret or hash
    let r = app()
        .oneshot(sess(&f, "GET", "/api/tokens", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let list: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], json!(id));
    assert_eq!(list[0]["name"], json!("docs bot"));
    assert!(list[0].get("token_secret").is_none());

    // --- bearer requests work without cookies or CSRF ---
    let r = app()
        .oneshot(token_req(&secret, "GET", "/api/docs", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "token GET docs");

    // create scope: POST /api/docs
    let r = app()
        .oneshot(token_req(
            &secret,
            "POST",
            "/api/docs",
            Some(json!({"title": "Bot page"})),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "token create doc");
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let doc: Value = serde_json::from_slice(&body).unwrap();
    let doc_id = doc["id"].as_str().unwrap().to_string();

    // write scope: PATCH rename
    let r = app()
        .oneshot(token_req(
            &secret,
            "PATCH",
            &format!("/api/docs/{doc_id}"),
            Some(json!({"title": "Renamed by bot"})),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "token rename doc");

    // delete scope missing: DELETE must be refused with the scope name
    let r = app()
        .oneshot(token_req(
            &secret,
            "DELETE",
            &format!("/api/docs/{doc_id}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], json!("auth.token.insufficient_scope"));

    // --- tokens cannot manage tokens ---
    for (method, uri) in [("GET", "/api/tokens"), ("POST", "/api/tokens")] {
        let r = app()
            .oneshot(token_req(&secret, method, uri, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }

    // --- a read-only token cannot create ---
    let (_, secret2) = mint(&f, "reader", &["read"]).await;
    let r = app()
        .oneshot(token_req(&secret2, "POST", "/api/docs", Some(json!({}))))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // --- session requests pass the scope middleware untouched ---
    let r = app()
        .oneshot(sess(
            &f,
            "POST",
            "/api/docs",
            Some(json!({"title": "Human page"})),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "session create doc");

    // --- a forged or revoked token is a hard 401, no cookie fallback ---
    let r = app()
        .oneshot(token_req("knot_forged", "GET", "/api/docs", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let r = app()
        .oneshot(sess(&f, "DELETE", &format!("/api/tokens/{id}"), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "revoke own token");

    let r = app()
        .oneshot(token_req(&secret, "GET", "/api/docs", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "revoked token");

    // revoking a token twice is still 204 (idempotent on our own token)
    let r = app()
        .oneshot(sess(&f, "DELETE", &format!("/api/tokens/{id}"), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // revoking an unknown id is a 404, not an error
    let r = app()
        .oneshot(sess(
            &f,
            "DELETE",
            &format!("/api/tokens/{}", uuid::Uuid::new_v4()),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

/// Validation at the trust boundary: unknown scopes, empty name rejected.
#[tokio::test]
async fn creation_validates_name_and_scopes() {
    let f = fixture().await;
    let app = || router_with_state(f.state.clone());

    let r = app()
        .oneshot(sess(
            &f,
            "POST",
            "/api/tokens",
            Some(json!({"name": "x", "scopes": ["read", "admin"]})),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let r = app()
        .oneshot(sess(
            &f,
            "POST",
            "/api/tokens",
            Some(json!({"name": "  ", "scopes": ["read"]})),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let r = app()
        .oneshot(sess(
            &f,
            "POST",
            "/api/tokens",
            Some(json!({"name": "empty", "scopes": []})),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Token auth never applies outside `/api/*`: `/auth/*` routes ignore it.
/// (The collab socket can't be exercised through `oneshot` — no upgrade
/// state — and the loader's `/api/` path check excludes it the same way.)
#[tokio::test]
async fn tokens_only_apply_to_api_paths() {
    let f = fixture().await;
    let app = || router_with_state(f.state.clone());
    let (_, secret) = mint(&f, "scoped", &["read"]).await;

    let r = app()
        .oneshot(token_req(&secret, "GET", "/auth/session", None))
        .await
        .unwrap();
    // No cookie rode along, and the token does not count as a session here.
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

/// Removing the token holder from the workspace kills their tokens: the
/// member join + role lookup yields nothing, so no AuthContext is minted.
#[tokio::test]
async fn removal_from_workspace_kills_tokens() {
    let f = fixture().await;
    let app = || router_with_state(f.state.clone());
    let (_, secret) = mint(&f, "doomed", &["read"]).await;

    let ws = f
        .state
        .workspaces
        .as_ref()
        .unwrap()
        .get_singleton()
        .await
        .unwrap()
        .unwrap();
    let me = f
        .state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("alice@example.com")
        .await
        .unwrap()
        .unwrap();
    f.state
        .workspaces
        .as_ref()
        .unwrap()
        .remove_member(ws.id, me.id)
        .await
        .unwrap();

    let r = app()
        .oneshot(token_req(&secret, "GET", "/api/docs", None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}
