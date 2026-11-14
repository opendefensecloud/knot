//! Integration: `/api/me/favorites` — star, list and unstar pages, with the
//! list filtered to what the caller can read and the star refused the way
//! `require_doc_role_mw` refuses doc routes.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::{FAVORITES_LIMIT, FavoriteStore, PgFavoriteStore, Pool, WorkspaceRole};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "hunter22";

/// A workspace with alice (owner), bob (editor), vera (viewer) and two
/// pages owned by alice.
struct World {
    state: AppState,
    pool: Pool,
    ws: Uuid,
    alice: Uuid,
    bob: Uuid,
    a: Uuid,
    b: Uuid,
}

async fn world() -> World {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = s.hasher.hash(PASSWORD).unwrap();
    let workspaces = s.workspaces.clone().unwrap();
    let users = s.users.clone().unwrap();
    let ws = workspaces.create("default", "W").await.unwrap();
    let mut ids = Vec::new();
    for (email, name, role) in [
        ("alice@example.com", "Alice", WorkspaceRole::Owner),
        ("bob@example.com", "Bob", WorkspaceRole::Editor),
        ("vera@example.com", "Vera", WorkspaceRole::Viewer),
    ] {
        let user = users.create_local(email, name, &hash).await.unwrap();
        workspaces.add_member(ws.id, user.id, role).await.unwrap();
        ids.push(user.id);
    }
    let docs = s.docs.clone().unwrap();
    let a = docs
        .create(ws.id, None, "Alpha", "m", ids[0])
        .await
        .unwrap();
    let b = docs.create(ws.id, None, "Beta", "n", ids[0]).await.unwrap();
    World {
        state: s,
        pool,
        ws: ws.id,
        alice: ids[0],
        bob: ids[1],
        a: a.id,
        b: b.id,
    }
}

/// Log in as `email` and return the Cookie header value to replay.
async fn login(state: &AppState, email: &str) -> String {
    let res = router_with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": email, "password": PASSWORD }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT, "login {email}");
    res.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

fn csrf_from(cookie: &str) -> String {
    cookie
        .split("; ")
        .find_map(|c| c.strip_prefix("csrf="))
        .unwrap_or_default()
        .to_string()
}

/// `method uri` as the session in `cookie`, with an optional JSON body.
async fn send(
    state: &AppState,
    cookie: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie)
        .header("x-csrf-token", csrf_from(cookie));
    dispatch(state, builder, body).await
}

/// `method uri` with `Authorization: Bearer {key}`.
async fn send_bearer(state: &AppState, key: &str, method: &str, uri: &str) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    dispatch(state, builder, None).await
}

async fn dispatch(
    state: &AppState,
    builder: axum::http::request::Builder,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    };
    let res = router_with_state(state.clone())
        .oneshot(request.unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn star(state: &AppState, cookie: &str, doc: Uuid) -> (StatusCode, Value) {
    send(
        state,
        cookie,
        "PUT",
        &format!("/api/me/favorites/{doc}"),
        None,
    )
    .await
}

async fn unstar(state: &AppState, cookie: &str, doc: Uuid) -> (StatusCode, Value) {
    send(
        state,
        cookie,
        "DELETE",
        &format!("/api/me/favorites/{doc}"),
        None,
    )
    .await
}

/// The caller's favorites, as the list of their `doc_id`s.
async fn favorite_ids(state: &AppState, cookie: &str) -> Vec<Uuid> {
    let (status, body) = send(state, cookie, "GET", "/api/me/favorites", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .unwrap()
        .iter()
        .map(|f| f["doc_id"].as_str().unwrap().parse().unwrap())
        .collect()
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// A key for `user_id` with exactly `scopes`, written through the store.
async fn mint_key(w: &World, user_id: Uuid, scopes: &[&str]) -> String {
    let secret = knot_auth::ApiKeySecret::generate();
    let names: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    let scopes = knot_storage::api_key_store::Scopes::from_names(&names).unwrap();
    w.state
        .api_keys
        .as_ref()
        .unwrap()
        .create(knot_storage::api_key_store::NewApiKey {
            workspace_id: w.ws,
            user_id,
            name: "favorites",
            key_hash: &knot_auth::api_key::hash(secret.as_str(), &w.state.session_key),
            scopes: &scopes,
            created_by: user_id,
            expires_at: None,
            session_hash: None,
        })
        .await
        .unwrap();
    secret.as_str().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn star_list_unstar_round_trip() {
    let w = world().await;
    let bob = login(&w.state, "bob@example.com").await;

    assert_eq!(star(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(star(&w.state, &bob, w.b).await.0, StatusCode::NO_CONTENT);

    let (status, body) = send(&w.state, &bob, "GET", "/api/me/favorites", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["doc_id"], w.a.to_string());
    assert_eq!(items[0]["title"], "Alpha");
    assert_eq!(items[0]["icon"], Value::Null);
    let favorited_at = items[0]["favorited_at"].as_str().unwrap();
    chrono::DateTime::parse_from_rfc3339(favorited_at).expect("favorited_at is RFC 3339");
    assert_eq!(items[1]["doc_id"], w.b.to_string());
    assert_eq!(items[1]["title"], "Beta");

    assert_eq!(unstar(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(favorite_ids(&w.state, &bob).await, vec![w.b]);

    // Per user: alice has none of bob's.
    let alice = login(&w.state, "alice@example.com").await;
    assert!(favorite_ids(&w.state, &alice).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_is_idempotent() {
    let w = world().await;
    let bob = login(&w.state, "bob@example.com").await;
    assert_eq!(star(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(star(&w.state, &bob, w.b).await.0, StatusCode::NO_CONTENT);
    // Starring `a` again keeps it first.
    assert_eq!(star(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(favorite_ids(&w.state, &bob).await, vec![w.a, w.b]);
    // Unstarring twice is fine too.
    assert_eq!(unstar(&w.state, &bob, w.b).await.0, StatusCode::NO_CONTENT);
    assert_eq!(unstar(&w.state, &bob, w.b).await.0, StatusCode::NO_CONTENT);
    assert_eq!(favorite_ids(&w.state, &bob).await, vec![w.a]);
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_can_star() {
    let w = world().await;
    let vera = login(&w.state, "vera@example.com").await;
    let (status, body) = star(&w.state, &vera, w.a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(favorite_ids(&w.state, &vera).await, vec![w.a]);
    assert_eq!(unstar(&w.state, &vera, w.a).await.0, StatusCode::NO_CONTENT);
    assert!(favorite_ids(&w.state, &vera).await.is_empty());
}

/// A page restricted without a grant for vera leaves her list, can't be
/// starred again, and can still be unstarred.
#[tokio::test(flavor = "multi_thread")]
async fn restricted_page_is_hidden_and_refused() {
    let w = world().await;
    let alice = login(&w.state, "alice@example.com").await;
    let vera = login(&w.state, "vera@example.com").await;
    assert_eq!(star(&w.state, &alice, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(star(&w.state, &vera, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(star(&w.state, &vera, w.b).await.0, StatusCode::NO_CONTENT);

    let (status, body) = send(
        &w.state,
        &alice,
        "PUT",
        &format!("/api/docs/{}/restricted", w.a),
        Some(json!({ "restricted": true })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    assert_eq!(favorite_ids(&w.state, &vera).await, vec![w.b]);
    assert_eq!(favorite_ids(&w.state, &alice).await, vec![w.a]);

    let (status, body) = star(&w.state, &vera, w.a).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(error_code(&body), "acl.no_grant");

    assert_eq!(unstar(&w.state, &vera, w.a).await.0, StatusCode::NO_CONTENT);

    // The unstar removed the row: lifting the restriction doesn't bring it
    // back.
    let (status, body) = send(
        &w.state,
        &alice,
        "PUT",
        &format!("/api/docs/{}/restricted", w.a),
        Some(json!({ "restricted": false })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(favorite_ids(&w.state, &vera).await, vec![w.b]);
}

/// A page restricted after vera starred it comes back when access does.
#[tokio::test(flavor = "multi_thread")]
async fn restricted_then_unrestricted_comes_back() {
    let w = world().await;
    let alice = login(&w.state, "alice@example.com").await;
    let vera = login(&w.state, "vera@example.com").await;
    assert_eq!(star(&w.state, &vera, w.a).await.0, StatusCode::NO_CONTENT);
    let restricted = |on: bool| {
        let state = w.state.clone();
        let alice = alice.clone();
        let uri = format!("/api/docs/{}/restricted", w.a);
        async move {
            let (status, body) = send(
                &state,
                &alice,
                "PUT",
                &uri,
                Some(json!({ "restricted": on })),
            )
            .await;
            assert!(status.is_success(), "{status} {body}");
        }
    };
    restricted(true).await;
    assert!(favorite_ids(&w.state, &vera).await.is_empty());
    restricted(false).await;
    assert_eq!(favorite_ids(&w.state, &vera).await, vec![w.a]);
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_hidden_then_restored() {
    let w = world().await;
    let alice = login(&w.state, "alice@example.com").await;
    let bob = login(&w.state, "bob@example.com").await;
    let vera = login(&w.state, "vera@example.com").await;
    for cookie in [&alice, &bob] {
        assert_eq!(star(&w.state, cookie, w.a).await.0, StatusCode::NO_CONTENT);
    }

    let (status, body) = send(
        &w.state,
        &alice,
        "DELETE",
        &format!("/api/docs/{}", w.a),
        None,
    )
    .await;
    assert!(status.is_success(), "archive: {status} {body}");

    // Hidden for everyone while archived, editors included.
    assert!(favorite_ids(&w.state, &bob).await.is_empty());
    assert!(favorite_ids(&w.state, &alice).await.is_empty());

    // Owners and editors can read it but not star it; a viewer can't read
    // it at all.
    for cookie in [&alice, &bob] {
        let (status, body) = star(&w.state, cookie, w.a).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(error_code(&body), "doc.archived");
    }
    let (status, body) = star(&w.state, &vera, w.a).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(error_code(&body), "doc.not_found");

    // Unstarring an archived page works.
    assert_eq!(
        unstar(&w.state, &alice, w.a).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, body) = send(
        &w.state,
        &alice,
        "POST",
        &format!("/api/docs/{}/restore", w.a),
        None,
    )
    .await;
    assert!(status.is_success(), "restore: {status} {body}");

    // Bob's star survived the archive; alice's was removed.
    assert_eq!(favorite_ids(&w.state, &bob).await, vec![w.a]);
    assert!(favorite_ids(&w.state, &alice).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_doc_is_no_grant() {
    let w = world().await;
    let bob = login(&w.state, "bob@example.com").await;
    let (status, body) = star(&w.state, &bob, Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(error_code(&body), "acl.no_grant");
    // A page of another workspace is unknown here too.
    let other_ws = w
        .state
        .workspaces
        .as_ref()
        .unwrap()
        .create("other", "Other")
        .await
        .unwrap();
    let foreign = w
        .state
        .docs
        .as_ref()
        .unwrap()
        .create(other_ws.id, None, "Foreign", "m", w.alice)
        .await
        .unwrap();
    let (status, body) = star(&w.state, &bob, foreign.id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(error_code(&body), "acl.no_grant");
    assert!(favorite_ids(&w.state, &bob).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn template_refused() {
    let w = world().await;
    w.state
        .docs
        .as_ref()
        .unwrap()
        .set_template(w.ws, w.a, w.alice, true)
        .await
        .unwrap();
    let bob = login(&w.state, "bob@example.com").await;
    let (status, body) = star(&w.state, &bob, w.a).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(error_code(&body), "favorite.not_a_page");
    assert!(favorite_ids(&w.state, &bob).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn limit_reached() {
    let w = world().await;
    let store = PgFavoriteStore::new(w.pool.clone());
    let docs = w.state.docs.clone().unwrap();
    // `a` and `b` count too; fill up to the cap with fresh pages.
    store.add(w.bob, w.ws, w.a).await.unwrap();
    store.add(w.bob, w.ws, w.b).await.unwrap();
    for i in 2..FAVORITES_LIMIT {
        let doc = docs
            .create(
                w.ws,
                None,
                &format!("Page {i}"),
                &format!("k{i:04}x"),
                w.alice,
            )
            .await
            .unwrap();
        store.add(w.bob, w.ws, doc.id).await.unwrap();
    }
    let one_more = docs
        .create(w.ws, None, "One more", "z", w.alice)
        .await
        .unwrap();

    let bob = login(&w.state, "bob@example.com").await;
    let (status, body) = star(&w.state, &bob, one_more.id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(error_code(&body), "favorite.limit_reached");
    assert_eq!(body["error"]["details"]["limit"], 200);

    // Already starred is still fine at the cap.
    assert_eq!(star(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_scopes() {
    let w = world().await;
    let read = mint_key(&w, w.bob, &["read"]).await;
    let update = mint_key(&w, w.bob, &["read", "update"]).await;
    let delete = mint_key(&w, w.bob, &["delete"]).await;
    let star_uri = format!("/api/me/favorites/{}", w.a);

    let (status, body) = send_bearer(&w.state, &read, "GET", "/api/me/favorites").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send_bearer(&w.state, &read, "PUT", &star_uri).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(error_code(&body), "api_key.scope_denied");

    let (status, body) = send_bearer(&w.state, &update, "PUT", &star_uri).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = send_bearer(&w.state, &update, "GET", "/api/me/favorites").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);

    let (status, body) = send_bearer(&w.state, &update, "DELETE", &star_uri).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(error_code(&body), "api_key.scope_denied");
    let (status, body) = send_bearer(&w.state, &delete, "DELETE", &star_uri).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (_, body) = send_bearer(&w.state, &update, "GET", "/api/me/favorites").await;
    assert!(body.as_array().unwrap().is_empty());
}

/// A star is private: nothing lands in the audit log, so the page audit
/// log doesn't change either.
#[tokio::test(flavor = "multi_thread")]
async fn no_audit_rows() {
    let w = world().await;
    let alice = login(&w.state, "alice@example.com").await;
    let bob = login(&w.state, "bob@example.com").await;
    let audit_uri = format!("/api/docs/{}/audit", w.a);
    let (status, page_log_before) = send(&w.state, &alice, "GET", &audit_uri, None).await;
    assert_eq!(status, StatusCode::OK, "{page_log_before}");
    let rows_before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&w.pool)
        .await
        .unwrap();

    assert_eq!(star(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(unstar(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);
    assert_eq!(star(&w.state, &bob, w.a).await.0, StatusCode::NO_CONTENT);

    let favorite_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action LIKE 'favorite%'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!(favorite_rows, 0);
    let rows_after: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(rows_after, rows_before);
    let (_, page_log_after) = send(&w.state, &alice, "GET", &audit_uri, None).await;
    assert_eq!(page_log_after, page_log_before);
}
