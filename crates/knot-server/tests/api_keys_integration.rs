//! Scoped API keys and bot accounts.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fresh workspace whose only member is a local owner.
async fn state_with_owner(email: &str, password: &str) -> AppState {
    let pool = knot_test_support::fresh_db().await.pool;

    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = s.hasher.hash(password).unwrap();
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
        .create_local(email, "Owner", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    s
}

async fn post_login(app: &axum::Router, email: &str, password: &str) -> axum::response::Response {
    app.clone()
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
        .unwrap()
}

/// Session cookie header value and CSRF token for a successful login.
struct Session {
    cookie: String,
    csrf: String,
}

async fn login(app: &axum::Router, email: &str, password: &str) -> Session {
    let r = post_login(app, email, password).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login should succeed");
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let first = |name: &str| {
        cookies
            .iter()
            .find(|c| c.starts_with(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} cookie"))
            .split(';')
            .next()
            .unwrap()
            .to_string()
    };
    let sid = first("sid");
    let csrf_kv = first("csrf");
    let csrf = csrf_kv.split_once('=').unwrap().1.to_string();
    Session {
        cookie: format!("{sid}; {csrf_kv}"),
        csrf,
    }
}

async fn send(
    app: &axum::Router,
    s: &Session,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", &s.cookie)
        .header("x-csrf-token", &s.csrf);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let r = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ---------------------------------------------------------------------------
// Bot accounts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn bot_invite_creates_a_credentialless_member() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;

    // A bot can never be owner: clean 422 before the KB001 trigger.
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({"email": "owner-bot@example.com", "role": "owner", "kind": "bot"})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "bot.owner_forbidden", "{body}");

    // Unknown kind.
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({"email": "x@example.com", "role": "editor", "kind": "robot"})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "member.kind_invalid", "{body}");

    // A supplied password is ignored, not rejected.
    let (st, _) = send(
        &app,
        &s,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({
            "email": "ci-bot@example.com",
            "role": "editor",
            "kind": "bot",
            "password": "botpass123",
            "display_name": "CI Bot",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let bot = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("ci-bot@example.com")
        .await
        .unwrap()
        .expect("bot user exists");
    assert_eq!(bot.kind, knot_storage::user_store::UserKind::Bot);
    assert!(bot.password_hash.is_none() && bot.oidc_subject.is_none());

    let (st, members) = send(&app, &s, "GET", "/api/workspace/members", None).await;
    assert_eq!(st, StatusCode::OK);
    let row = members
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == "ci-bot@example.com")
        .expect("bot listed");
    assert_eq!(row["kind"], "bot");
    assert_eq!(row["role"], "editor");
    assert_eq!(row["display_name"], "CI Bot");
    let owner_row = members
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["email"] == "owner@example.com")
        .unwrap();
    assert_eq!(owner_row["kind"], "human");

    // No interactive login, whatever password is tried.
    for pw in ["botpass123", "anything-else"] {
        let r = post_login(&app, "ci-bot@example.com", pw).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "auth.invalid_credentials", "{body}");
    }

    // The audit record names the kind.
    let kind: String = sqlx::query_scalar(
        "SELECT data->>'kind' FROM audit_events
         WHERE action = 'workspace.member.invite' AND target_id = $1",
    )
    .bind(bot.id)
    .fetch_one(state.pool.as_ref().unwrap())
    .await
    .unwrap();
    assert_eq!(kind, "bot");
}

// ---------------------------------------------------------------------------
// Bot ownership guards
// ---------------------------------------------------------------------------

/// A bot can work in the normal tree but never ends up an owner: no create
/// inside a restricted subtree (that mints an owner grant), no restricting,
/// no owner grant, no member management, no purge.
#[tokio::test(flavor = "multi_thread")]
async fn bot_ownership_guards() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;

    let (st, _) = send(
        &app,
        &s,
        "POST",
        "/api/workspace/members",
        Some(
            serde_json::json!({"email": "guard-bot@example.com", "role": "editor", "kind": "bot"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let bot_id = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("guard-bot@example.com")
        .await
        .unwrap()
        .expect("bot user")
        .id;
    let key = guards_mint_all_scopes_key(&state, bot_id).await;

    // Normal tree: fine.
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "bot page"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let bot_doc = body["id"].as_str().unwrap().to_string();

    // A restricted page the bot may edit under.
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "secret"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let secret = body["id"].as_str().unwrap().to_string();
    let (st, body) = send(
        &app,
        &s,
        "PUT",
        &format!("/api/docs/{secret}/restricted"),
        Some(serde_json::json!({"restricted": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = send(
        &app,
        &s,
        "PUT",
        &format!("/api/docs/{secret}/grants/user:{bot_id}"),
        Some(serde_json::json!({"role": "editor", "inherit": true})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");

    // Create inside the restricted subtree.
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "child", "parent_id": secret})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.restricted_forbidden", "{body}");

    // Duplicating a page inside the restricted subtree.
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "inner", "parent_id": secret})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let inner = body["id"].as_str().unwrap().to_string();
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{inner}/duplicate"),
        Some(serde_json::json!({"include_subpages": false})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.restricted_forbidden", "{body}");

    // Restricting a page: a bot never holds the page-owner role it needs
    // (the bot guard behind it is pinned in the handler's own tests).
    let (st, body) = guards_bearer(
        &app,
        &key,
        "PUT",
        &format!("/api/docs/{bot_doc}/restricted"),
        Some(serde_json::json!({"restricted": true})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "acl.owner_required", "{body}");

    // An owner grant for the bot, even from the page's owner.
    let (st, body) = send(
        &app,
        &s,
        "PUT",
        &format!("/api/docs/{secret}/grants/user:{bot_id}"),
        Some(serde_json::json!({"role": "owner", "inherit": true})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "bot.owner_forbidden", "{body}");

    // Promoting the bot to workspace owner: clean 422 before KB001.
    let (st, body) = send(
        &app,
        &s,
        "PATCH",
        &format!("/api/workspace/members/{bot_id}"),
        Some(serde_json::json!({"role": "owner"})),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "bot.owner_forbidden", "{body}");

    // Member management.
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({"email": "x@example.com", "role": "viewer", "kind": "bot"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");

    // Purge of its own archived page.
    let (st, body) =
        guards_bearer(&app, &key, "DELETE", &format!("/api/docs/{bot_doc}"), None).await;
    assert!(st.is_success(), "archive: {st} {body}");
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{bot_doc}/purge"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
}

/// A live key with every scope for `user_id`, written through the store.
async fn guards_mint_all_scopes_key(state: &AppState, user_id: uuid::Uuid) -> String {
    let workspace_id = state
        .workspaces
        .as_ref()
        .unwrap()
        .get_singleton()
        .await
        .unwrap()
        .expect("workspace")
        .id;
    let secret = knot_auth::ApiKeySecret::generate();
    let names: Vec<String> = ["read", "create", "update", "delete"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let scopes = knot_storage::api_key_store::Scopes::from_names(&names).unwrap();
    state
        .api_keys
        .as_ref()
        .unwrap()
        .create(knot_storage::api_key_store::NewApiKey {
            workspace_id,
            user_id,
            name: "guards",
            key_hash: &knot_auth::api_key::hash(secret.as_str(), &state.session_key),
            scopes: &scopes,
            created_by: user_id,
            expires_at: None,
            session_hash: None,
        })
        .await
        .unwrap();
    secret.as_str().to_string()
}

async fn guards_bearer(
    app: &axum::Router,
    key: &str,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {key}"));
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let r = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

// ---------------------------------------------------------------------------
// Key management endpoints
// ---------------------------------------------------------------------------

/// Invite a bot editor through the HTTP API and return its user id.
async fn invite_bot(app: &axum::Router, s: &Session, state: &AppState, email: &str) -> uuid::Uuid {
    let (st, body) = send(
        app,
        s,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({"email": email, "role": "editor", "kind": "bot"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    state
        .users
        .as_ref()
        .unwrap()
        .find_by_email(email)
        .await
        .unwrap()
        .expect("bot user")
        .id
}

/// Add a local human member and log them in.
async fn add_human(app: &axum::Router, owner: &Session, email: &str, password: &str) -> Session {
    let (st, body) = send(
        app,
        owner,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({"email": email, "role": "editor", "password": password})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    login(app, email, password).await
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_lifecycle() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;

    let (st, created) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "  ci  ", "scopes": ["read"], "expires_in_days": 30})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    let key = created["key"].as_str().unwrap().to_string();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(knot_auth::api_key::looks_like_api_key(&key));
    assert_eq!(created["name"], "ci");
    assert_eq!(created["scopes"], serde_json::json!(["read"]));
    assert!(created["expires_at"].is_string());

    // Listed without the secret or its hash.
    let (st, list) = send(&app, &s, "GET", "/api/auth/api-keys", None).await;
    assert_eq!(st, StatusCode::OK);
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["id"], id.as_str());
    assert!(row.get("key").is_none() && row.get("key_hash").is_none());
    assert!(!list.to_string().contains(&key), "secret never re-served");
    for f in ["created_at", "expires_at", "last_used_at", "revoked_at"] {
        assert!(row.get(f).is_some(), "{f}");
    }
    assert!(row["revoked_at"].is_null());

    // Audit row carries no secret material.
    let data: serde_json::Value =
        sqlx::query_scalar("SELECT data FROM audit_events WHERE action = 'api-key.create'")
            .fetch_one(state.pool.as_ref().unwrap())
            .await
            .unwrap();
    assert_eq!(data["key_id"], id.as_str());
    assert!(!data.to_string().contains(&key));
    assert!(data.get("key_hash").is_none());

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (st, _) = send(
        &app,
        &s,
        "DELETE",
        &format!("/api/auth/api-keys/{id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "api_key.invalid", "{body}");

    let (_, list) = send(&app, &s, "GET", "/api/auth/api-keys", None).await;
    assert!(list[0]["revoked_at"].is_string());

    // Revoking twice is indistinguishable from an unknown key.
    let (st, body) = send(
        &app,
        &s,
        "DELETE",
        &format!("/api/auth/api-keys/{id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "api_key.not_found", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn api_key_create_validates_name_and_scopes() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    for (body, code) in [
        (
            serde_json::json!({"name": "x", "scopes": []}),
            "api_key.scopes_invalid",
        ),
        (
            serde_json::json!({"name": "x", "scopes": ["read", "admin"]}),
            "api_key.scopes_invalid",
        ),
        (
            serde_json::json!({"name": "   ", "scopes": ["read"]}),
            "api_key.name_invalid",
        ),
        (
            serde_json::json!({"name": "n".repeat(101), "scopes": ["read"]}),
            "api_key.name_invalid",
        ),
    ] {
        let (st, resp) = send(&app, &s, "POST", "/api/auth/api-keys", Some(body)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{resp}");
        assert_eq!(resp["error"]["code"], code, "{resp}");
    }
}

/// S2: a key can never mint, list or revoke keys, whatever its scopes.
#[tokio::test(flavor = "multi_thread")]
async fn api_key_management_requires_a_session() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let owner_id = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("owner@example.com")
        .await
        .unwrap()
        .unwrap()
        .id;
    let key = guards_mint_all_scopes_key(&state, owner_id).await;
    let key_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM api_keys")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap();

    for (method, uri, body) in [
        (
            "POST",
            "/api/auth/api-keys".to_string(),
            Some(serde_json::json!({"name": "escalate", "scopes": ["read"]})),
        ),
        ("GET", "/api/auth/api-keys".to_string(), None),
        ("DELETE", format!("/api/auth/api-keys/{key_id}"), None),
    ] {
        let (st, resp) = guards_bearer(&app, &key, method, &uri, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{method} {resp}");
        assert_eq!(
            resp["error"]["code"], "api_key.session_required",
            "{method}"
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE revoked_at IS NULL")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(n, 1, "nothing minted, nothing revoked");
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_can_manage_a_bot_key_but_not_a_human_key() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &s, &state, "ci-bot@example.com").await;
    let human = add_human(&app, &s, "eve@example.com", "evepass123").await;
    let human_id = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("eve@example.com")
        .await
        .unwrap()
        .unwrap()
        .id;

    // Owner mints, lists and revokes a bot key.
    let (st, created) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "bot", "scopes": ["read", "create"], "user_id": bot})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    let bot_key = created["key"].as_str().unwrap().to_string();
    let bot_key_id = created["id"].as_str().unwrap().to_string();
    let (st, body) = guards_bearer(
        &app,
        &bot_key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "by bot"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    let (st, list) = send(
        &app,
        &s,
        "GET",
        &format!("/api/auth/api-keys?user_id={bot}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    // The owner's own list does not include the bot's key.
    let (_, own) = send(&app, &s, "GET", "/api/auth/api-keys", None).await;
    assert!(own.as_array().unwrap().is_empty());

    // Without ?user_id= the bot's key is not the owner's: no oracle.
    let (st, _) = send(
        &app,
        &s,
        "DELETE",
        &format!("/api/auth/api-keys/{bot_key_id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = send(
        &app,
        &s,
        "DELETE",
        &format!("/api/auth/api-keys/{bot_key_id}?user_id={bot}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = guards_bearer(&app, &bot_key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // A human target is foreign, for every verb.
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "eve", "scopes": ["read"], "user_id": human_id})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "api_key.foreign_forbidden", "{body}");
    let (st, body) = send(
        &app,
        &s,
        "GET",
        &format!("/api/auth/api-keys?user_id={human_id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "api_key.foreign_forbidden", "{body}");
    let (st, body) = send(
        &app,
        &s,
        "DELETE",
        &format!(
            "/api/auth/api-keys/{}?user_id={human_id}",
            uuid::Uuid::new_v4()
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "api_key.foreign_forbidden", "{body}");

    // An unknown user id is equally foreign.
    let (st, body) = send(
        &app,
        &s,
        "GET",
        &format!("/api/auth/api-keys?user_id={}", uuid::Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "api_key.foreign_forbidden", "{body}");

    // A non-owner cannot manage a bot's keys.
    let (st, body) = send(
        &app,
        &human,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "bot", "scopes": ["read"], "user_id": bot})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "api_key.foreign_forbidden", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ttl_rules() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;

    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "short", "scopes": ["read"], "expires_in_days": 10})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "api_key.ttl_too_short", "{body}");

    // A local user has no upper bound, but an expiry past what a timestamp
    // holds is refused, without blaming single sign-on.
    for days in [i64::MAX, 100_000_000] {
        let (st, body) = send(
            &app,
            &s,
            "POST",
            "/api/auth/api-keys",
            Some(serde_json::json!({"name": "far", "scopes": ["read"], "expires_in_days": days})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{days}: {body}");
        assert_eq!(body["error"]["code"], "api_key.ttl_too_long", "{body}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(!message.contains("sign-on"), "{message}");
    }

    // A bot may have a key that never expires.
    let bot = invite_bot(&app, &s, &state, "ci-bot@example.com").await;
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({
            "name": "forever", "scopes": ["read"], "expires_in_days": null, "user_id": bot,
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert!(body["expires_at"].is_null());

    // Turn a logged-in human into a pure-OIDC user (the session survives).
    let oidc = add_human(&app, &s, "sso@example.com", "ssopass123").await;
    sqlx::query(
        "UPDATE users SET password_hash = NULL, oidc_issuer = 'https://idp.test',
                          oidc_subject = 'sub-1'
         WHERE email = 'sso@example.com'",
    )
    .execute(state.pool.as_ref().unwrap())
    .await
    .unwrap();
    for days in [serde_json::json!(120), serde_json::Value::Null] {
        let (st, body) = send(
            &app,
            &oidc,
            "POST",
            "/api/auth/api-keys",
            Some(serde_json::json!({"name": "sso", "scopes": ["read"], "expires_in_days": days})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{days}: {body}");
        assert_eq!(body["error"]["code"], "api_key.ttl_too_long", "{body}");
    }
    let (st, body) = send(
        &app,
        &oidc,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "sso", "scopes": ["read"], "expires_in_days": 90})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_key_delete_is_not_found() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let eve = add_human(&app, &s, "eve@example.com", "evepass123").await;

    let (st, created) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "mine", "scopes": ["read"]})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap();

    let (st, body) = send(
        &app,
        &eve,
        "DELETE",
        &format!("/api/auth/api-keys/{id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "api_key.not_found", "{body}");
    let (st, unknown) = send(
        &app,
        &eve,
        "DELETE",
        &format!("/api/auth/api-keys/{}", uuid::Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(unknown, body, "same answer as an unknown id");

    // The owner's key still works.
    let (st, _) = guards_bearer(
        &app,
        created["key"].as_str().unwrap(),
        "GET",
        "/api/docs",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Session fields
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn session_reports_auth_kind_and_max_api_key_ttl() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let (st, body) = send(&app, &s, "GET", "/auth/session", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["auth_kind"], "local", "{body}");
    assert!(body["max_api_key_ttl_days"].is_null(), "{body}");

    // A bot's key sees the bot's kind (and no cap).
    let bot_id = invite_bot(&app, &s, &state, "session-bot@example.com").await;
    let key = guards_mint_all_scopes_key(&state, bot_id).await;
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auth/session")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // `/auth/session` is outside `/api/`, so a key does not authenticate it (S1).
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_caps_api_key_ttl_for_pure_oidc_users() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let pool = state.pool.clone().unwrap();
    // Turn the owner into a pure-OIDC human; the old session stays valid.
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    sqlx::query(
        "UPDATE users SET password_hash = NULL, oidc_subject = 'sub-1', oidc_issuer = 'https://idp' \
         WHERE email = 'owner@example.com'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (st, body) = send(&app, &s, "GET", "/auth/session", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["auth_kind"], "oidc", "{body}");
    assert_eq!(
        body["max_api_key_ttl_days"],
        knot_auth::api_key::SSO_MAX_TTL_DAYS
    );
}

// ---------------------------------------------------------------------------
// Scope matrix (S3)
// ---------------------------------------------------------------------------

/// Mint a key for the logged-in user through the HTTP API.
async fn matrix_key(app: &axum::Router, s: &Session, scopes: &[&str]) -> String {
    let (st, body) = send(
        app,
        s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "matrix", "scopes": scopes})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    body["key"].as_str().unwrap().to_string()
}

/// A page created through the session.
async fn matrix_doc(app: &axum::Router, s: &Session, title: &str) -> String {
    let (st, body) = send(
        app,
        s,
        "POST",
        "/api/docs",
        Some(serde_json::json!({ "title": title })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

/// A page created and archived through the session, ready to purge.
async fn matrix_archived_doc(app: &axum::Router, s: &Session, title: &str) -> String {
    let id = matrix_doc(app, s, title).await;
    let (st, body) = send(app, s, "DELETE", &format!("/api/docs/{id}"), None).await;
    assert!(st.is_success(), "archive: {st} {body}");
    id
}

/// Export snapshots the live room, so it needs a real rooms registry
/// (as in `export_integration.rs`).
async fn matrix_wire_rooms(state: &mut AppState) {
    use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore};
    let pool = state.pool.as_ref().unwrap().clone();
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let admin = knot_test_support::admin_url();
    let url = format!("{}/{db}", &admin[..admin.rfind('/').unwrap()]);
    let bus = Arc::new(knot_crdt::PgBus::connect(&url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    let policy = knot_crdt::SnapshotPolicy {
        every_n: 1000,
        idle: std::time::Duration::from_secs(3600),
    };
    let rooms = Arc::new(knot_crdt::Rooms::new(
        Arc::new(knot_crdt::YrsEngine),
        bus.clone(),
        updates,
        snaps,
        policy,
        std::time::Duration::from_secs(3600),
    ));
    state.bus = Some(bus);
    state.rooms_v2 = Some(rooms);
}

#[track_caller]
fn assert_scope_denied(st: StatusCode, body: &serde_json::Value, scope: &str) {
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "api_key.scope_denied", "{body}");
    assert_eq!(body["error"]["details"]["scope"], scope, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_key_scope_matrix() {
    let mut state = state_with_owner("owner@example.com", "ownerpass1").await;
    matrix_wire_rooms(&mut state).await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["read"]).await;
    let doc = matrix_doc(&app, &s, "page").await;
    let archived = matrix_archived_doc(&app, &s, "old").await;

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "nope"})),
    )
    .await;
    assert_scope_denied(st, &body, "create");

    // The POST that only renders.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc}/export/docx"))
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "export with a read key");

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{archived}/purge"),
        None,
    )
    .await;
    assert_scope_denied(st, &body, "delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn create_key_scope_matrix() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["create"]).await;
    let doc = matrix_doc(&app, &s, "page").await;

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "made by key"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    let (st, body) = guards_bearer(
        &app,
        &key,
        "PATCH",
        &format!("/api/docs/{doc}"),
        Some(serde_json::json!({"title": "renamed"})),
    )
    .await;
    assert_scope_denied(st, &body, "update");

    let (st, body) = guards_bearer(&app, &key, "DELETE", &format!("/api/docs/{doc}"), None).await;
    assert_scope_denied(st, &body, "delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn update_key_scope_matrix() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["update"]).await;
    let doc = matrix_doc(&app, &s, "page").await;
    let parent = matrix_doc(&app, &s, "parent").await;

    let (st, body) = guards_bearer(
        &app,
        &key,
        "PATCH",
        &format!("/api/docs/{doc}"),
        Some(serde_json::json!({"title": "renamed"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "renamed", "{body}");

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{doc}/move"),
        Some(serde_json::json!({"parent_id": parent})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "move is an update: {body}");

    let (st, body) = guards_bearer(&app, &key, "DELETE", &format!("/api/docs/{doc}"), None).await;
    assert_scope_denied(st, &body, "delete");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_key_scope_matrix() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["delete"]).await;
    let doc = matrix_doc(&app, &s, "page").await;
    let other = matrix_doc(&app, &s, "other").await;
    let parent = matrix_doc(&app, &s, "parent").await;
    let archived = matrix_archived_doc(&app, &s, "old").await;

    let (st, body) = guards_bearer(&app, &key, "DELETE", &format!("/api/docs/{doc}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{other}/move"),
        Some(serde_json::json!({"parent_id": parent})),
    )
    .await;
    assert_scope_denied(st, &body, "update");

    // Purge maps to `delete`, not the POST default `create`: a delete key may
    // purge an archived page.
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{archived}/purge"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");

    // And a create key may not, although purge is a POST.
    let create_key = matrix_key(&app, &s, &["create"]).await;
    let archived2 = matrix_archived_doc(&app, &s, "older").await;
    let (st, body) = guards_bearer(
        &app,
        &create_key,
        "POST",
        &format!("/api/docs/{archived2}/purge"),
        None,
    )
    .await;
    assert_scope_denied(st, &body, "delete");
}

// ---------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------

/// S1, S9: a key authenticates nothing outside `/api/`.
#[tokio::test(flavor = "multi_thread")]
async fn bearer_is_rejected_outside_api() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["read", "create", "update", "delete"]).await;
    let doc = matrix_doc(&app, &s, "page").await;

    // A real socket: `oneshot` cannot upgrade, so it would answer 426
    // before the auth check runs.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://{addr}/collab/doc/{doc}"
        ))
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "collab with a key");
        }
        Err(e) => panic!("collab with a key: unexpected error {e}"),
        Ok(_) => panic!("collab with a key: upgraded"),
    }

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"current": "ownerpass1", "new": "hijacked99"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "password with a key");

    // The password is unchanged.
    assert_eq!(
        post_login(&app, "owner@example.com", "ownerpass1")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

/// Session cookie and CSRF token from a response's `set-cookie` headers.
fn matrix_session_from(r: &axum::response::Response) -> Session {
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    let sid = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .expect("sid cookie")
        .clone();
    let csrf_kv = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .expect("csrf cookie")
        .clone();
    let csrf = csrf_kv.split_once('=').unwrap().1.to_string();
    Session {
        cookie: format!("{sid}; {csrf_kv}"),
        csrf,
    }
}

/// S5 via the HTTP handler.
#[tokio::test(flavor = "multi_thread")]
async fn password_change_revokes_api_keys() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["read"]).await;
    let (st, _) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK);

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", &s.cookie)
                .header("x-csrf-token", &s.csrf)
                .body(Body::from(
                    serde_json::json!({"current": "ownerpass1", "new": "newpass999"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let fresh = matrix_session_from(&r);

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "api_key.invalid", "{body}");

    let (st, body) = send(&app, &fresh, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK, "fresh session still works: {body}");
}

/// S5 via a direct database UPDATE: the trigger covers every writer.
#[tokio::test(flavor = "multi_thread")]
async fn direct_password_update_revokes_api_keys() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["read"]).await;
    let (st, _) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK);

    let new_hash = state.hasher.hash("adminreset1").unwrap();
    sqlx::query("UPDATE users SET password_hash = $1 WHERE email = 'owner@example.com'")
        .bind(new_hash)
        .execute(state.pool.as_ref().unwrap())
        .await
        .unwrap();

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "api_key.invalid", "{body}");
    let revoked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE revoked_at IS NOT NULL")
            .fetch_one(state.pool.as_ref().unwrap())
            .await
            .unwrap();
    assert_eq!(revoked, 1, "revoked in the DB, not just refused");
}

/// S8: membership is checked at auth time.
#[tokio::test(flavor = "multi_thread")]
async fn removed_member_key_is_inert() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@example.com", "ownerpass1").await;
    let eve = add_human(&app, &owner, "eve@example.com", "evepass123").await;
    let key = matrix_key(&app, &eve, &["read"]).await;
    let (st, _) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK);

    let eve_id = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email("eve@example.com")
        .await
        .unwrap()
        .unwrap()
        .id;
    let (st, body) = send(
        &app,
        &owner,
        "DELETE",
        &format!("/api/workspace/members/{eve_id}"),
        None,
    )
    .await;
    assert!(st.is_success(), "remove member: {st} {body}");

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "api_key.invalid", "{body}");
}

/// S5: a bot key the session minted before the password change must die with
/// it, or a stolen owner session outlives the reset it provoked.
#[tokio::test(flavor = "multi_thread")]
async fn password_change_revokes_bot_keys_the_user_minted() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &s, &state, "ci-bot@example.com").await;
    let (st, created) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "bot", "scopes": ["read"], "user_id": bot})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    let bot_key = created["key"].as_str().unwrap();
    let (st, _) = guards_bearer(&app, bot_key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK);

    assert_eq!(
        change_password(&app, &s, "ownerpass1", "newpass999").await,
        StatusCode::NO_CONTENT
    );

    let (st, body) = guards_bearer(&app, bot_key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "api_key.invalid", "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn csrf_does_not_apply_to_api_keys() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["create"]).await;

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "no cookie"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/docs")
                .header("cookie", &s.cookie)
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"title": "no csrf"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "auth.csrf", "{body}");
}

// ---------------------------------------------------------------------------
// Review fixes
// ---------------------------------------------------------------------------

async fn user_id(state: &AppState, email: &str) -> uuid::Uuid {
    state
        .users
        .as_ref()
        .unwrap()
        .find_by_email(email)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("{email} exists"))
        .id
}

async fn member_role(state: &AppState, user: uuid::Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT role FROM workspace_members WHERE user_id = $1")
        .bind(user)
        .fetch_optional(state.pool.as_ref().unwrap())
        .await
        .unwrap()
}

/// S2, S9: member management changes who holds which role, so a key that
/// reached it could mint itself an owner with a known password. Refused for
/// every key, whatever its scopes; listing members stays readable.
#[tokio::test(flavor = "multi_thread")]
async fn api_key_cannot_manage_members_whatever_its_scopes() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let key = matrix_key(&app, &s, &["read", "create", "update", "delete"]).await;
    add_human(&app, &s, "viewer@example.com", "viewerpass1").await;
    let viewer = user_id(&state, "viewer@example.com").await;
    let bot = invite_bot(&app, &s, &state, "member-bot@example.com").await;
    let users_before: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap();

    for (method, uri, body) in [
        (
            "POST",
            "/api/workspace/members".to_string(),
            Some(serde_json::json!({
                "email": "evil@example.com", "password": "evilpass123", "role": "owner",
            })),
        ),
        (
            "PATCH",
            format!("/api/workspace/members/{viewer}"),
            Some(serde_json::json!({"role": "owner"})),
        ),
        ("DELETE", format!("/api/workspace/members/{bot}"), None),
    ] {
        let (st, resp) = guards_bearer(&app, &key, method, &uri, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{method} {uri}: {resp}");
        assert_eq!(
            resp["error"]["code"], "api_key.session_required",
            "{method} {uri}"
        );
    }

    let users_after: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(users_after, users_before, "no user was written");
    assert_eq!(member_role(&state, viewer).await.as_deref(), Some("editor"));
    assert_eq!(member_role(&state, bot).await.as_deref(), Some("editor"));
    assert_eq!(
        post_login(&app, "evil@example.com", "evilpass123")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/workspace/members", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

/// Defence in depth behind the OIDC refusal: should a session for a bot
/// ever exist, it still cannot mint keys for itself.
#[tokio::test(flavor = "multi_thread")]
async fn bot_session_cannot_manage_keys() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &s, &state, "session-bot@example.com").await;
    let ws = state
        .workspaces
        .as_ref()
        .unwrap()
        .get_singleton()
        .await
        .unwrap()
        .unwrap()
        .id;
    let token = knot_auth::SessionToken::generate();
    let sid = knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes());
    state
        .sessions
        .as_ref()
        .unwrap()
        .create(
            &sid,
            bot,
            ws,
            chrono::Utc::now() + chrono::Duration::hours(1),
            None,
            None,
        )
        .await
        .unwrap();
    let (sid_cookie, csrf_cookie) =
        knot_server::auth::cookies::build_session_cookies(&state, &token);
    let sid_pair = sid_cookie.split(';').next().unwrap().to_string();
    let csrf_pair = csrf_cookie.split(';').next().unwrap().to_string();
    let bot_session = Session {
        csrf: csrf_pair.trim_start_matches("csrf=").to_string(),
        cookie: format!("{sid_pair}; {csrf_pair}"),
    };

    let (st, body) = send(
        &app,
        &bot_session,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "self", "scopes": ["read"]})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.forbidden", "{body}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// S8: removal revokes in the DB, so a re-invite does not revive old keys;
/// a removed bot cannot be re-added through the human invite path.
#[tokio::test(flavor = "multi_thread")]
async fn removed_member_keys_stay_dead_after_reinvite() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@example.com", "ownerpass1").await;
    let eve = add_human(&app, &owner, "eve@example.com", "evepass123").await;
    let eve_key = matrix_key(&app, &eve, &["read"]).await;
    let eve_id = user_id(&state, "eve@example.com").await;
    let bot = invite_bot(&app, &owner, &state, "gone-bot@example.com").await;
    let (st, created) = send(
        &app,
        &owner,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "bot", "scopes": ["read"], "user_id": bot})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    let bot_key = created["key"].as_str().unwrap().to_string();

    for id in [eve_id, bot] {
        let (st, body) = send(
            &app,
            &owner,
            "DELETE",
            &format!("/api/workspace/members/{id}"),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    }

    // Eve comes back as an existing user, without her old key.
    let (st, body) = send(
        &app,
        &owner,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({"email": "eve@example.com", "role": "editor"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (st, body) = guards_bearer(&app, &eve_key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "api_key.invalid", "{body}");

    // The bot cannot be re-added, through either path.
    for body in [
        serde_json::json!({"email": "gone-bot@example.com", "role": "editor"}),
        serde_json::json!({"email": "gone-bot@example.com", "role": "editor", "kind": "bot"}),
    ] {
        let (st, resp) = send(&app, &owner, "POST", "/api/workspace/members", Some(body)).await;
        assert_eq!(st, StatusCode::CONFLICT, "{resp}");
        assert_eq!(resp["error"]["code"], "workspace.email_taken", "{resp}");
    }
    assert_eq!(member_role(&state, bot).await, None);
    let (st, _) = guards_bearer(&app, &bot_key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

/// A departing or demoted owner keeps no bot access through keys it minted;
/// the key list names who minted each key.
#[tokio::test(flavor = "multi_thread")]
async fn bot_keys_minted_by_an_owner_die_with_its_ownership() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &owner, &state, "shared-bot@example.com").await;
    let mut minted = Vec::new();
    for email in ["leaver@example.com", "demoted@example.com"] {
        let (st, body) = send(
            &app,
            &owner,
            "POST",
            "/api/workspace/members",
            Some(serde_json::json!({"email": email, "role": "owner", "password": "ownerpass2"})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{body}");
        let s = login(&app, email, "ownerpass2").await;
        let (st, created) = send(
            &app,
            &s,
            "POST",
            "/api/auth/api-keys",
            Some(serde_json::json!({"name": email, "scopes": ["read"], "user_id": bot})),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{created}");
        let key = created["key"].as_str().unwrap().to_string();
        let (st, _) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
        assert_eq!(st, StatusCode::OK);
        minted.push((user_id(&state, email).await, key));
    }

    let (st, list) = send(
        &app,
        &owner,
        "GET",
        &format!("/api/auth/api-keys?user_id={bot}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{list}");
    let creators: std::collections::BTreeSet<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["created_by"].as_str().unwrap().to_string())
        .collect();
    let want: std::collections::BTreeSet<String> =
        minted.iter().map(|(id, _)| id.to_string()).collect();
    assert_eq!(creators, want, "{list}");

    let (leaver, leaver_key) = &minted[0];
    let (st, body) = send(
        &app,
        &owner,
        "DELETE",
        &format!("/api/workspace/members/{leaver}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    let (demoted, demoted_key) = &minted[1];
    let (st, body) = send(
        &app,
        &owner,
        "PATCH",
        &format!("/api/workspace/members/{demoted}"),
        Some(serde_json::json!({"role": "editor"})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");

    for key in [leaver_key, demoted_key] {
        let (st, body) = guards_bearer(&app, key, "GET", "/api/docs", None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    }
}

/// S4: a bot import never creates a page it would own; refused up front
/// instead of skipping restricted pages one by one.
#[tokio::test(flavor = "multi_thread")]
async fn bot_import_of_restricted_pages_is_refused() {
    use std::io::Write;
    let mut state = state_with_owner("owner@example.com", "ownerpass1").await;
    matrix_wire_rooms(&mut state).await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &s, &state, "import-bot@example.com").await;
    let key = guards_mint_all_scopes_key(&state, bot).await;
    let target = matrix_doc(&app, &s, "target").await;
    let secret = matrix_doc(&app, &s, "secret").await;
    let (st, body) = send(
        &app,
        &s,
        "PUT",
        &format!("/api/docs/{secret}/restricted"),
        Some(serde_json::json!({"restricted": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = send(
        &app,
        &s,
        "PUT",
        &format!("/api/docs/{secret}/grants/user:{bot}"),
        Some(serde_json::json!({"role": "editor", "inherit": true})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");

    let zip = |restricted: bool| {
        let manifest = serde_json::json!({
            "knot_export_version": "2",
            "docs": [{
                "id": uuid::Uuid::new_v4().to_string(),
                "parent_id": null,
                "title": "imported",
                "sort_key": "a0",
                "path": "docs/page-0.md",
                "restricted": restricted,
            }],
            "attachments": [],
            "boards": [],
        });
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("index.json", options).unwrap();
        zip.write_all(manifest.to_string().as_bytes()).unwrap();
        zip.start_file("docs/page-0.md", options).unwrap();
        zip.write_all(b"# imported\n").unwrap();
        zip.finish().unwrap().into_inner()
    };
    let import = |parent: String, body: Vec<u8>| {
        let app = app.clone();
        let key = key.clone();
        async move {
            let r = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/workspace/import?parent_id={parent}"))
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "application/zip")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = r.status();
            let bytes = r.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value =
                serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }
    };
    let imported = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM documents WHERE title = 'imported'")
            .fetch_one(state.pool.as_ref().unwrap())
            .await
            .unwrap()
    };

    // A restricted page in the manifest, under a normal page.
    let (st, body) = import(target.clone(), zip(true)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.restricted_forbidden", "{body}");
    // A normal page into a restricted subtree.
    let (st, body) = import(secret.clone(), zip(false)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.restricted_forbidden", "{body}");
    assert_eq!(imported().await, 0, "nothing was created");

    // A normal page under a normal page is fine.
    let (st, body) = import(target, zip(false)).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["imported_docs"], 1, "{body}");
}

/// A bot whose only write access to the parent is a non-inheriting grant
/// can still create below it: the creator grant is editor, never owner.
#[tokio::test(flavor = "multi_thread")]
async fn bot_create_under_a_non_inheriting_grant_gets_an_editor_grant() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/workspace/members",
        Some(
            serde_json::json!({"email": "viewer-bot@example.com", "role": "viewer", "kind": "bot"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let bot = user_id(&state, "viewer-bot@example.com").await;
    let key = guards_mint_all_scopes_key(&state, bot).await;
    let parent = matrix_doc(&app, &s, "parent").await;
    let (st, body) = send(
        &app,
        &s,
        "PUT",
        &format!("/api/docs/{parent}/grants/user:{bot}"),
        Some(serde_json::json!({"role": "editor", "inherit": false})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "by bot", "parent_id": parent})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let child = body["id"].as_str().unwrap().to_string();
    let (st, body) = guards_bearer(
        &app,
        &key,
        "PATCH",
        &format!("/api/docs/{child}"),
        Some(serde_json::json!({"title": "edited by bot"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the bot can edit what it created: {body}"
    );
    let role: String = sqlx::query_scalar(
        "SELECT role FROM document_grants WHERE doc_id = $1::uuid AND principal = $2",
    )
    .bind(&child)
    .bind(format!("user:{bot}"))
    .fetch_one(state.pool.as_ref().unwrap())
    .await
    .unwrap();
    assert_eq!(role, "editor");
}

/// The agent flow a read+update key is meant for: read tasks and pages,
/// write progress into a page (markdown import is an update), tick tasks.
/// New pages and comments are creates, so they need `create`.
#[tokio::test(flavor = "multi_thread")]
async fn agent_with_read_update_key_can_work_tasks() {
    let mut state = state_with_owner("owner@example.com", "ownerpass1").await;
    matrix_wire_rooms(&mut state).await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &s, &state, "agent@example.com").await;
    let (st, created) = send(
        &app,
        &s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "agent", "scopes": ["read", "update"], "user_id": bot})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    let key = created["key"].as_str().unwrap().to_string();
    let doc = matrix_doc(&app, &s, "work log").await;

    let (st, body) = guards_bearer(&app, &key, "GET", "/api/workspace/tasks", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/{doc}/markdown"))
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "text/markdown")
                .body(Body::from("- [ ] hand off to the review agent\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(r.status().is_success(), "markdown import: {}", r.status());

    let (st, body) = guards_bearer(
        &app,
        &key,
        "PATCH",
        &format!("/api/docs/{doc}/tasks/0"),
        Some(serde_json::json!({"checked": true})),
    )
    .await;
    assert!(st.is_success(), "tick the task: {st} {body}");

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/docs/{doc}/markdown"))
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let md = String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    assert!(md.contains("[x] hand off to the review agent"), "{md}");

    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{doc}/comments"),
        Some(serde_json::json!({"body": "done"})),
    )
    .await;
    assert_scope_denied(st, &body, "create");
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "handoff"})),
    )
    .await;
    assert_scope_denied(st, &body, "create");
}

/// S3: every override row names a real route. A key holding only the
/// mapped scope passes the scope check on each; were a route renamed, its
/// row would stop matching and the POST default (`create`) would deny it.
#[tokio::test(flavor = "multi_thread")]
async fn every_scope_override_matches_a_real_route() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let doc = matrix_doc(&app, &s, "page").await;
    let id = uuid::Uuid::new_v4().to_string();

    // The override matched and the handler ran: neither a router miss (405)
    // nor the method default's denial.
    let reached = |st: StatusCode, body: &serde_json::Value| {
        st != StatusCode::METHOD_NOT_ALLOWED && body["error"]["code"] != "api_key.scope_denied"
    };
    // A path no route has fails that test: proves the loop can tell a
    // matched override from a renamed route.
    let update_key = matrix_key(&app, &s, &["update"]).await;
    let (st, body) = guards_bearer(
        &app,
        &update_key,
        "POST",
        &format!("/api/docs/{doc}/no-such-route"),
        None,
    )
    .await;
    assert!(
        !reached(st, &body),
        "a miss looked like a match: {st} {body}"
    );

    for (method, template, scope) in knot_server::auth::scope::OVERRIDES {
        assert_ne!(*scope, knot_storage::Scope::Create, "{template}");
        let key = matrix_key(&app, &s, &[scope.as_str()]).await;
        let uri = template
            .replace("{id}", &doc)
            .replace("{thread_id}", &id)
            .replace("{suggestion_id}", &id)
            .replace("{seq}", "1")
            .replace("{format}", "docx");
        assert!(!uri.contains('{'), "unsubstituted parameter in {template}");
        let (st, body) = guards_bearer(&app, &key, method.as_str(), &uri, None).await;
        assert!(
            reached(st, &body),
            "{method} {template} with only `{}`: {st} {body}",
            scope.as_str()
        );
    }
}

// ---------------------------------------------------------------------------
// A mint racing what revokes keys (S5, S8)
// ---------------------------------------------------------------------------

/// Start `POST /api/auth/api-keys` and deliver its body only when the
/// returned sender fires. The session is checked when the request arrives;
/// the handler then waits for the body, so whatever runs meanwhile happens
/// between the request's authentication and its insert.
fn held_create_key(
    app: &axum::Router,
    s: &Session,
) -> (
    tokio::sync::oneshot::Sender<serde_json::Value>,
    tokio::task::JoinHandle<(StatusCode, serde_json::Value)>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    let stream = futures_util::stream::once(async move {
        let body = rx.await.unwrap().to_string();
        Ok::<_, std::io::Error>(axum::body::Bytes::from(body))
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/auth/api-keys")
        .header("cookie", &s.cookie)
        .header("x-csrf-token", &s.csrf)
        .header("content-type", "application/json")
        .body(Body::from_stream(stream))
        .unwrap();
    let app = app.clone();
    let task = tokio::spawn(async move {
        let r = app.oneshot(req).await.unwrap();
        let st = r.status();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        (
            st,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    });
    (tx, task)
}

/// Let a held request get past authentication before racing it.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

async fn live_keys_of(state: &AppState, user: uuid::Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM api_keys
          WHERE user_id = $1 AND revoked_at IS NULL
            AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(user)
    .fetch_one(state.pool.as_ref().unwrap())
    .await
    .unwrap()
}

async fn change_password(app: &axum::Router, s: &Session, current: &str, new: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/password")
                .header("content-type", "application/json")
                .header("cookie", &s.cookie)
                .header("x-csrf-token", &s.csrf)
                .body(Body::from(
                    serde_json::json!({"current": current, "new": new}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

const ALL_SCOPES: [&str; 4] = ["read", "create", "update", "delete"];

/// S5: a stolen cookie that starts a mint before the victim changes the
/// password gets no key that outlives the change.
#[tokio::test(flavor = "multi_thread")]
async fn mint_authenticated_before_a_password_change_is_refused() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let stolen = login(&app, "owner@example.com", "ownerpass1").await;
    let victim = login(&app, "owner@example.com", "ownerpass1").await;
    let owner = user_id(&state, "owner@example.com").await;

    let (tx, held) = held_create_key(&app, &stolen);
    settle().await;
    assert_eq!(
        change_password(&app, &victim, "ownerpass1", "newpass999").await,
        StatusCode::NO_CONTENT
    );
    tx.send(serde_json::json!({"name": "persist", "scopes": ALL_SCOPES}))
        .unwrap();
    let (st, body) = held.await.unwrap();
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "auth.session_required", "{body}");
    assert_eq!(live_keys_of(&state, owner).await, 0);
}

/// S5: the new hash (whose trigger revokes the keys) and the end of every
/// session commit together. Were they two statements, a mint could run in
/// between: keys already revoked, its session still there.
#[tokio::test(flavor = "multi_thread")]
async fn password_change_commits_the_hash_and_the_session_purge_together() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let owner = user_id(&state, "owner@example.com").await;
    let pool = state.pool.as_ref().unwrap().clone();
    let hash_now = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT password_hash FROM users WHERE id = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let old = hash_now().await;

    // Hold the session rows: the purge has to wait for them.
    let mut lock = knot_storage::begin(&pool).await.unwrap();
    sqlx::query("SELECT 1 FROM sessions WHERE user_id = $1 FOR UPDATE")
        .bind(owner)
        .fetch_all(&mut *lock)
        .await
        .unwrap();
    let change = {
        let app = app.clone();
        let s = Session {
            cookie: s.cookie.clone(),
            csrf: s.csrf.clone(),
        };
        tokio::spawn(async move { change_password(&app, &s, "ownerpass1", "newpass999").await })
    };
    settle().await;
    assert!(!change.is_finished(), "the purge did not wait for the lock");
    assert_eq!(
        hash_now().await,
        old,
        "the new hash committed while the old sessions were still there"
    );
    lock.commit().await.unwrap();
    assert_eq!(change.await.unwrap(), StatusCode::NO_CONTENT);
    assert_ne!(hash_now().await, old);
}

/// S8: an owner removed while minting a bot key keeps no bot access.
#[tokio::test(flavor = "multi_thread")]
async fn bot_key_mint_by_an_owner_removed_meanwhile_is_refused() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &owner, &state, "bot@example.com").await;
    let (st, body) = send(
        &app,
        &owner,
        "POST",
        "/api/workspace/members",
        Some(
            serde_json::json!({"email": "leaver@example.com", "role": "owner", "password": "ownerpass2"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let leaver_id = user_id(&state, "leaver@example.com").await;
    let leaver = login(&app, "leaver@example.com", "ownerpass2").await;

    let (tx, held) = held_create_key(&app, &leaver);
    settle().await;
    let (st, body) = send(
        &app,
        &owner,
        "DELETE",
        &format!("/api/workspace/members/{leaver_id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    tx.send(serde_json::json!({"name": "after", "scopes": ALL_SCOPES, "user_id": bot}))
        .unwrap();
    let (st, body) = held.await.unwrap();
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "auth.session_required", "{body}");
    assert_eq!(live_keys_of(&state, bot).await, 0);
}

/// S8: an owner demoted while minting a bot key gets none.
#[tokio::test(flavor = "multi_thread")]
async fn bot_key_mint_by_an_owner_demoted_meanwhile_is_refused() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &owner, &state, "bot@example.com").await;
    let (st, body) = send(
        &app,
        &owner,
        "POST",
        "/api/workspace/members",
        Some(
            serde_json::json!({"email": "second@example.com", "role": "owner", "password": "ownerpass2"}),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let second_id = user_id(&state, "second@example.com").await;
    let second = login(&app, "second@example.com", "ownerpass2").await;

    let (tx, held) = held_create_key(&app, &second);
    settle().await;
    let (st, body) = send(
        &app,
        &owner,
        "PATCH",
        &format!("/api/workspace/members/{second_id}"),
        Some(serde_json::json!({"role": "editor"})),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    tx.send(serde_json::json!({"name": "after", "scopes": ALL_SCOPES, "user_id": bot}))
        .unwrap();
    let (st, body) = held.await.unwrap();
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "api_key.foreign_forbidden", "{body}");
    assert_eq!(live_keys_of(&state, bot).await, 0);
}

/// S8: a member removed while minting their own key gets none, so a
/// re-invite brings nothing back.
#[tokio::test(flavor = "multi_thread")]
async fn own_key_mint_by_a_member_removed_meanwhile_stays_dead_after_reinvite() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@example.com", "ownerpass1").await;
    let ed = add_human(&app, &owner, "ed@example.com", "edpass123").await;
    let ed_id = user_id(&state, "ed@example.com").await;

    let (tx, held) = held_create_key(&app, &ed);
    settle().await;
    let (st, _) = send(
        &app,
        &owner,
        "DELETE",
        &format!("/api/workspace/members/{ed_id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    tx.send(serde_json::json!({"name": "x", "scopes": ["read"]}))
        .unwrap();
    let (st, body) = held.await.unwrap();
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "auth.session_required", "{body}");

    let (st, body) = send(
        &app,
        &owner,
        "POST",
        "/api/workspace/members",
        Some(serde_json::json!({"email": "ed@example.com", "role": "editor"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(live_keys_of(&state, ed_id).await, 0);
}

// ---------------------------------------------------------------------------
// Public share links are session-only (S2)
// ---------------------------------------------------------------------------

/// A public link outlives the key, its user's password and membership, so
/// no key may mint one or read the existing tokens. Revoking one is fine.
#[tokio::test(flavor = "multi_thread")]
async fn keys_cannot_mint_or_read_share_links() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let doc = matrix_doc(&app, &s, "secret plans").await;
    let key = matrix_key(&app, &s, &ALL_SCOPES).await;

    let (st, body) =
        guards_bearer(&app, &key, "POST", &format!("/api/docs/{doc}/shares"), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "api_key.session_required", "{body}");
    let shares: i64 = sqlx::query_scalar("SELECT count(*) FROM share_tokens")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(shares, 0);

    let (st, share) = send(&app, &s, "POST", &format!("/api/docs/{doc}/shares"), None).await;
    assert_eq!(st, StatusCode::CREATED, "{share}");
    let token = share["token"].as_str().unwrap().to_string();
    let (st, body) =
        guards_bearer(&app, &key, "GET", &format!("/api/docs/{doc}/shares"), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "api_key.session_required", "{body}");
    assert!(!body.to_string().contains(&token));

    let share_id = share["id"].as_str().unwrap();
    let (st, body) = guards_bearer(
        &app,
        &key,
        "DELETE",
        &format!("/api/docs/{doc}/shares/{share_id}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    assert!(
        state
            .shares
            .as_ref()
            .unwrap()
            .find_alive(&token)
            .await
            .unwrap()
            .is_none()
    );
}

/// S2: every session-only row names a real route. A key with every scope
/// is refused on each; were a route renamed, its row would stop matching
/// and the key would reach the handler.
#[tokio::test(flavor = "multi_thread")]
async fn every_session_only_row_matches_a_real_route() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let doc = matrix_doc(&app, &s, "page").await;
    let key = matrix_key(&app, &s, &ALL_SCOPES).await;
    let id = uuid::Uuid::new_v4().to_string();
    for (method, template) in knot_server::auth::scope::SESSION_ONLY {
        let uri = template.replace("{doc_id}", &doc).replace("{id}", &id);
        assert!(!uri.contains('{'), "unsubstituted parameter in {template}");
        let body = (*method == axum::http::Method::POST).then(|| serde_json::json!({}));
        let (st, body) = guards_bearer(&app, &key, method.as_str(), &uri, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{method} {template}: {body}");
        assert_eq!(
            body["error"]["code"], "api_key.session_required",
            "{method} {template}: {body}"
        );
    }
}

// ---------------------------------------------------------------------------
// Resource bounds and workspace isolation of key management
// ---------------------------------------------------------------------------

/// At most 50 live keys per user; the 51st answers 409 until one goes.
#[tokio::test(flavor = "multi_thread")]
async fn the_51st_live_key_is_refused() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let mint = || {
        send(
            &app,
            &s,
            "POST",
            "/api/auth/api-keys",
            Some(serde_json::json!({"name": "k", "scopes": ["read"]})),
        )
    };
    let mut first = None;
    for i in 0..50 {
        let (st, body) = mint().await;
        assert_eq!(st, StatusCode::CREATED, "key {i}: {body}");
        first.get_or_insert(body["id"].as_str().unwrap().to_string());
    }
    let (st, body) = mint().await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "api_key.limit_reached", "{body}");

    let (st, _) = send(
        &app,
        &s,
        "DELETE",
        &format!("/api/auth/api-keys/{}", first.unwrap()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, body) = mint().await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
}

/// An owner manages the bots of their own workspace only.
#[tokio::test(flavor = "multi_thread")]
async fn owner_cannot_manage_the_keys_of_another_workspaces_bot() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;

    let wss = state.workspaces.as_ref().unwrap();
    let users = state.users.as_ref().unwrap();
    let other = wss.create("other", "Other").await.unwrap().id;
    let other_owner = users
        .create_local("o2@example.com", "O2", "$h$")
        .await
        .unwrap()
        .id;
    wss.add_member(other, other_owner, WorkspaceRole::Owner)
        .await
        .unwrap();
    let bot = users.create_bot("b2@example.com", "B2").await.unwrap().id;
    wss.add_member(other, bot, WorkspaceRole::Editor)
        .await
        .unwrap();
    let scopes = knot_storage::api_key_store::Scopes::from_names(&["read".to_string()]).unwrap();
    let key = state
        .api_keys
        .as_ref()
        .unwrap()
        .create(knot_storage::api_key_store::NewApiKey {
            workspace_id: other,
            user_id: bot,
            name: "theirs",
            key_hash: &[42u8; 32],
            scopes: &scopes,
            created_by: other_owner,
            expires_at: None,
            session_hash: None,
        })
        .await
        .unwrap();

    for (method, uri, body) in [
        ("GET", format!("/api/auth/api-keys?user_id={bot}"), None),
        (
            "POST",
            "/api/auth/api-keys".to_string(),
            Some(serde_json::json!({"name": "mine", "scopes": ["read"], "user_id": bot})),
        ),
        (
            "DELETE",
            format!("/api/auth/api-keys/{}?user_id={bot}", key.id),
            None,
        ),
    ] {
        let (st, resp) = send(&app, &s, method, &uri, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{method} {uri}: {resp}");
        assert_eq!(
            resp["error"]["code"], "api_key.foreign_forbidden",
            "{method} {uri}: {resp}"
        );
    }
    assert_eq!(live_keys_of(&state, bot).await, 1, "their bot key stays");
}

// ---------------------------------------------------------------------------
// Grant principals (S4)
// ---------------------------------------------------------------------------

/// Every spelling `Uuid::parse_str` takes for a user (canonical, upper
/// case, simple, braced, urn) names the same principal: a bot owner grant
/// is refused under each, and a grant is stored, and removed, as one
/// canonical row.
#[tokio::test(flavor = "multi_thread")]
async fn grant_principals_are_canonical_whatever_the_uuid_spelling() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &s, &state, "bot@example.com").await;
    let doc = matrix_doc(&app, &s, "page").await;
    let simple = bot.simple().to_string();
    let spellings = [
        bot.to_string(),
        bot.to_string().to_uppercase(),
        simple.clone(),
        simple.to_uppercase(),
        format!("%7B{bot}%7D"),
        format!("urn:uuid:{bot}"),
    ];
    let pool = state.pool.as_ref().unwrap().clone();
    let bot_rows = || {
        let pool = pool.clone();
        let doc = doc.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT principal FROM document_grants
                  WHERE doc_id = $1::uuid AND lower(replace(principal, '-', '')) LIKE '%' || $2 || '%'
                  ORDER BY principal",
            )
            .bind(doc)
            .bind(bot.simple().to_string())
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    let put = |spelling: String, role: &'static str| {
        let uri = format!("/api/docs/{doc}/grants/user:{spelling}");
        let (app, s) = (&app, &s);
        async move {
            send(
                app,
                s,
                "PUT",
                &uri,
                Some(serde_json::json!({"role": role, "inherit": true})),
            )
            .await
        }
    };

    for spelling in &spellings {
        let (st, body) = put(spelling.clone(), "owner").await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{spelling}: {body}");
        assert_eq!(
            body["error"]["code"], "bot.owner_forbidden",
            "{spelling}: {body}"
        );
    }
    assert!(bot_rows().await.is_empty());

    for spelling in &spellings {
        let (st, body) = put(spelling.clone(), "editor").await;
        assert_eq!(st, StatusCode::NO_CONTENT, "{spelling}: {body}");
    }
    assert_eq!(bot_rows().await, vec![format!("user:{bot}")]);

    // Removal takes any spelling too.
    let (st, body) = send(
        &app,
        &s,
        "DELETE",
        &format!(
            "/api/docs/{doc}/grants/user:%7B{}%7D",
            bot.to_string().to_uppercase()
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    assert!(bot_rows().await.is_empty());

    // A row stored under another spelling before canonicalisation is
    // still removed by that spelling.
    let owner = user_id(&state, "owner@example.com").await;
    sqlx::query(
        "INSERT INTO document_grants (doc_id, principal, role, granted_by)
         VALUES ($1::uuid, $2, 'editor', $3)",
    )
    .bind(&doc)
    .bind(format!("user:{simple}"))
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    let (st, body) = send(
        &app,
        &s,
        "DELETE",
        &format!("/api/docs/{doc}/grants/user:{simple}"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    assert!(bot_rows().await.is_empty());
}

// ---------------------------------------------------------------------------
// Bots and restricted subtrees: template, duplicate (S4)
// ---------------------------------------------------------------------------

/// A page from a template, a copy of a restricted page, and a copy of a
/// tree holding one: each would give the bot an owner grant inside a
/// restricted subtree, so each is a clean 403 before the KB002 trigger.
#[tokio::test(flavor = "multi_thread")]
async fn bot_cannot_reach_owner_grants_via_template_duplicate_or_restrict() {
    let mut state = state_with_owner("owner@example.com", "ownerpass1").await;
    matrix_wire_rooms(&mut state).await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let bot = invite_bot(&app, &s, &state, "bot@example.com").await;
    let key = guards_mint_all_scopes_key(&state, bot).await;

    let restrict_and_grant = |doc: String| {
        let app = app.clone();
        let s = Session {
            cookie: s.cookie.clone(),
            csrf: s.csrf.clone(),
        };
        async move {
            let (st, body) = send(
                &app,
                &s,
                "PUT",
                &format!("/api/docs/{doc}/restricted"),
                Some(serde_json::json!({"restricted": true})),
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{body}");
            let (st, body) = send(
                &app,
                &s,
                "PUT",
                &format!("/api/docs/{doc}/grants/user:{bot}"),
                Some(serde_json::json!({"role": "editor", "inherit": true})),
            )
            .await;
            assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
        }
    };
    let secret = matrix_doc(&app, &s, "secret").await;
    restrict_and_grant(secret.clone()).await;

    // 1. From a template into the restricted subtree.
    let tpl = matrix_doc(&app, &s, "tpl").await;
    let (st, body) = send(
        &app,
        &s,
        "POST",
        &format!("/api/docs/{tpl}/template"),
        Some(serde_json::json!({"is_template": true})),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/from-template/{tpl}"),
        Some(serde_json::json!({"parent_id": secret})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.restricted_forbidden", "{body}");

    // 2. A copy of the restricted page itself.
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{secret}/duplicate"),
        Some(serde_json::json!({"include_subpages": false})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.restricted_forbidden", "{body}");

    // 3. A copy of an open tree with a restricted subpage.
    let top = matrix_doc(&app, &s, "top").await;
    let (st, body) = send(
        &app,
        &s,
        "POST",
        "/api/docs",
        Some(serde_json::json!({"title": "inner", "parent_id": top})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let inner = body["id"].as_str().unwrap().to_string();
    restrict_and_grant(inner).await;
    let (st, body) = guards_bearer(
        &app,
        &key,
        "POST",
        &format!("/api/docs/{top}/duplicate"),
        Some(serde_json::json!({"include_subpages": true})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "bot.restricted_forbidden", "{body}");

    // 4. Nothing was created.
    let docs: i64 = sqlx::query_scalar("SELECT count(*) FROM documents")
        .fetch_one(state.pool.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(docs, 4, "secret, tpl, top, inner");
}
