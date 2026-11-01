//! Drives the same code path the CLI does via direct function calls.
//! Avoids spawning the binary in tests (would need stdin mocking + IPC).

use std::sync::Arc;

use axum::http::StatusCode;
use knot_auth::Hasher;
use knot_storage::{PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole, WorkspaceStore};

#[tokio::test(flavor = "multi_thread")]
async fn admin_create_seeds_first_user_and_workspace() {
    let pool = knot_test_support::fresh_db().await.pool;

    let users = Arc::new(PgUserStore::new(pool.clone()));
    let ws = Arc::new(PgWorkspaceStore::new(pool));
    let hasher = Hasher::fast_for_tests();

    let workspace = ws.create("default", "Workspace").await.unwrap();
    let hash = hasher.hash("hunter22").unwrap();
    let user = users
        .create_local("admin@example.com", "Admin", &hash)
        .await
        .unwrap();
    ws.add_member(workspace.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    assert_eq!(users.count().await.unwrap(), 1);
    let role = ws.get_member_role(workspace.id, user.id).await.unwrap();
    assert_eq!(role, Some(WorkspaceRole::Owner));
}

/// `admin create` (via the extracted `create_owner` core) must work as a
/// break-glass tool AFTER the system is already bootstrapped: it bootstraps the
/// workspace on the first call, then still creates additional local owners even
/// though users already exist — refusing only a duplicate email.
#[tokio::test(flavor = "multi_thread")]
async fn admin_create_owner_is_break_glass_after_bootstrap() {
    let pool = knot_test_support::fresh_db().await.pool;
    let users = PgUserStore::new(pool.clone());
    let ws = PgWorkspaceStore::new(pool.clone());
    let hasher = Hasher::fast_for_tests();

    // First call bootstraps the singleton workspace + first owner.
    knot_server::admin::create_owner(
        &users,
        &ws,
        &hasher,
        "first@example.com",
        "First",
        "hunter22",
        "default",
        "Workspace",
    )
    .await
    .expect("bootstrap owner");

    // Break-glass: a second local owner is allowed even though a user exists.
    let bg = knot_server::admin::create_owner(
        &users,
        &ws,
        &hasher,
        "break-glass@example.com",
        "BreakGlass",
        "hunter22",
        "default",
        "Workspace",
    )
    .await
    .expect("break-glass owner after bootstrap");

    assert_eq!(users.count().await.unwrap(), 2);
    let wsrow = ws.get_singleton().await.unwrap().expect("workspace exists");
    assert_eq!(
        ws.get_member_role(wsrow.id, bg.id).await.unwrap(),
        Some(WorkspaceRole::Owner),
        "break-glass user is an owner"
    );

    // A duplicate email is refused (would violate the unique constraint).
    let dup = knot_server::admin::create_owner(
        &users,
        &ws,
        &hasher,
        "first@example.com",
        "Dup",
        "hunter22",
        "default",
        "Workspace",
    )
    .await;
    assert!(dup.is_err(), "duplicate email must be rejected");
}

// ---------------------------------------------------------------------------
// Restricted documents: `admin restricted-list` and `admin grant`
// ---------------------------------------------------------------------------

/// `restricted-list` prints one line per restriction boundary (nested ones
/// included) with its `user:` owner grants as emails, and nothing for
/// unrestricted docs.
#[tokio::test(flavor = "multi_thread")]
async fn admin_restricted_list_shows_boundaries_and_owners() {
    use knot_storage::{DocStore, GrantStore, PgDocStore, PgGrantStore};

    let pool = knot_test_support::fresh_db().await.pool;
    let users = PgUserStore::new(pool.clone());
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let docs = PgDocStore::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());

    let workspace = workspaces.create("default", "Workspace").await.unwrap();
    let mut member_ids = Vec::new();
    for email in ["alice@example.com", "bob@example.com", "carol@example.com"] {
        let user = users.create_local(email, "U", "x").await.unwrap();
        workspaces
            .add_member(workspace.id, user.id, WorkspaceRole::Editor)
            .await
            .unwrap();
        member_ids.push(user.id);
    }
    let (alice_id, bob_id, carol_id) = (member_ids[0], member_ids[1], member_ids[2]);

    let open_doc = docs
        .create(workspace.id, None, "Open", "a0", alice_id)
        .await
        .unwrap();
    let private_doc = docs
        .create(workspace.id, None, "Private", "a1", alice_id)
        .await
        .unwrap();
    let inner_doc = docs
        .create(workspace.id, Some(private_doc.id), "Inner", "a0", alice_id)
        .await
        .unwrap();
    // Restricting makes the actor an owner; Bob is added as a second owner
    // and Carol as an editor, who is not listed.
    docs.set_restricted(workspace.id, private_doc.id, alice_id, true)
        .await
        .unwrap();
    grants
        .put(
            workspace.id,
            private_doc.id,
            &format!("user:{bob_id}"),
            WorkspaceRole::Owner,
            true,
            alice_id,
        )
        .await
        .unwrap();
    grants
        .put(
            workspace.id,
            private_doc.id,
            &format!("user:{carol_id}"),
            WorkspaceRole::Editor,
            true,
            alice_id,
        )
        .await
        .unwrap();
    docs.set_restricted(workspace.id, inner_doc.id, carol_id, true)
        .await
        .unwrap();

    let mut lines = knot_server::admin::restricted_list(&pool, "default")
        .await
        .expect("restricted-list");
    lines.sort();
    let mut expected = vec![
        format!(
            "{}\tPrivate\towners=alice@example.com,bob@example.com",
            private_doc.id
        ),
        format!("{}\tInner\towners=carol@example.com", inner_doc.id),
    ];
    expected.sort();
    assert_eq!(lines, expected);
    assert!(
        lines
            .iter()
            .all(|line| !line.contains(&open_doc.id.to_string())),
        "an unrestricted doc is not listed"
    );

    assert!(
        knot_server::admin::restricted_list(&pool, "no-such-workspace")
            .await
            .is_err(),
        "an unknown workspace slug is an error"
    );
}

/// The break-glass path for Review Focus 5: the only owner grant of a
/// restricted doc belonged to a member who has since been removed, so not
/// even the workspace owner can open it. `admin grant` gives them owner
/// again, through `GrantStore::put`, so the ACL listener evicts the stale
/// cached 403, and audits `admin.grant` with `break_glass: true`.
#[tokio::test(flavor = "multi_thread")]
async fn admin_grant_recovers_orphaned_restricted_doc() {
    let fx = http::fixture().await;
    let pool = fx.state.pool.clone().unwrap();
    let _acl_listener = knot_docs::spawn_listener(
        pool.clone(),
        fx.state.acl.clone().unwrap(),
        fx.state.docs.clone().unwrap(),
        Arc::new(|_doc_id| {}),
    );
    http::wait_for_acl_listener(&pool).await;

    // Bob (a workspace owner) restricts his page, becoming its only owner.
    let doc_id = http::create_doc(fx.app.clone(), &fx.bob, "Bob's private").await;
    let (status, _) = http::send(
        fx.app.clone(),
        "PUT",
        &format!("/api/docs/{doc_id}/restricted"),
        &fx.bob,
        Some(serde_json::json!({"restricted": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bob restricts his page");
    let (status, _) = http::send(
        fx.app.clone(),
        "DELETE",
        &format!("/api/workspace/members/{}", fx.bob_id),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "alice removes bob");

    let doc_path = format!("/api/docs/{doc_id}");
    let (status, _) = http::send(fx.app.clone(), "GET", &doc_path, &fx.alice, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the orphaned page is closed even to the workspace owner"
    );

    let doc_uuid: uuid::Uuid = doc_id.parse().unwrap();
    // Refused: unknown role, unknown user, and a user outside the workspace.
    assert!(
        knot_server::admin::grant(&pool, doc_uuid, "alice@admin.test", "admin", false)
            .await
            .is_err()
    );
    assert!(
        knot_server::admin::grant(&pool, doc_uuid, "nobody@admin.test", "owner", false)
            .await
            .is_err()
    );
    assert!(
        knot_server::admin::grant(&pool, doc_uuid, "bob@admin.test", "owner", false)
            .await
            .is_err(),
        "a removed member can't be granted"
    );
    assert!(
        knot_server::admin::grant(
            &pool,
            uuid::Uuid::new_v4(),
            "alice@admin.test",
            "owner",
            false
        )
        .await
        .is_err(),
        "an unknown doc is an error"
    );

    knot_server::admin::grant(&pool, doc_uuid, "alice@admin.test", "owner", false)
        .await
        .expect("break-glass grant");

    // The listener evicts the cached 403 asynchronously.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (status, _) = http::send(fx.app.clone(), "GET", &doc_path, &fx.alice, None).await;
        if status == StatusCode::OK {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "alice never regained access, last status {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let grant_row: (String, bool) = sqlx::query_as(
        "SELECT role, inherit FROM document_grants WHERE doc_id = $1 AND principal = $2",
    )
    .bind(doc_uuid)
    .bind(format!("user:{}", fx.alice_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grant_row, ("owner".to_string(), true), "inherit by default");

    let (actor, data): (Option<uuid::Uuid>, serde_json::Value) = sqlx::query_as(
        "SELECT actor_id, data FROM audit_events
          WHERE action = 'admin.grant' AND target_id = $1",
    )
    .bind(doc_uuid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actor, None, "a break-glass grant has no acting user");
    assert_eq!(data["break_glass"], true);
    assert_eq!(data["principal"], format!("user:{}", fx.alice_id));
    assert_eq!(data["role"], "owner");

    // --no-inherit is honoured, and the last-owner guard still applies.
    knot_server::admin::grant(&pool, doc_uuid, "alice@admin.test", "owner", true)
        .await
        .expect("re-grant without inherit");
    let inherits: bool = sqlx::query_scalar(
        "SELECT inherit FROM document_grants WHERE doc_id = $1 AND principal = $2",
    )
    .bind(doc_uuid)
    .bind(format!("user:{}", fx.alice_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!inherits, "--no-inherit stores inherit=false");
    knot_server::admin::grant(&pool, doc_uuid, "alice@admin.test", "editor", false)
        .await
        .expect_err("downgrading the last owner grant is refused");
}

/// HTTP helpers for the break-glass test, trimmed from
/// restricted_integration.rs.
mod http {
    use std::sync::Arc;

    use axum::body::{Body, Bytes};
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use knot_auth::{Hasher, Throttle};
    use knot_server::{AppState, router_with_state};
    use knot_storage::WorkspaceRole;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// Alice and Bob are both workspace owners, logged in over HTTP.
    pub struct Fixture {
        pub state: AppState,
        pub app: axum::Router,
        pub alice: String,
        pub alice_id: uuid::Uuid,
        pub bob: String,
        pub bob_id: uuid::Uuid,
    }

    pub async fn fixture() -> Fixture {
        let pool = knot_test_support::fresh_db().await.pool;
        let mut state = AppState::with_pool(pool);
        state.hasher = Arc::new(Hasher::fast_for_tests());
        state.throttle = Arc::new(Throttle::new());
        state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

        let workspaces = state.workspaces.clone().unwrap();
        let users = state.users.clone().unwrap();
        let workspace = workspaces.create("default", "W").await.unwrap();
        let mut owner_ids = Vec::new();
        for email in ["alice@admin.test", "bob@admin.test"] {
            let hash = state.hasher.hash("password1").unwrap();
            let user = users.create_local(email, "U", &hash).await.unwrap();
            workspaces
                .add_member(workspace.id, user.id, WorkspaceRole::Owner)
                .await
                .unwrap();
            owner_ids.push(user.id);
        }

        let app = router_with_state(state.clone());
        let alice = login_as(app.clone(), "alice@admin.test", "password1").await;
        let bob = login_as(app.clone(), "bob@admin.test", "password1").await;
        Fixture {
            state,
            app,
            alice,
            alice_id: owner_ids[0],
            bob,
            bob_id: owner_ids[1],
        }
    }

    /// Log in as `email`/`password` and return `"sid=…; csrf=…|<csrf_token>"`.
    async fn login_as(app: axum::Router, email: &str, password: &str) -> String {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"email": email, "password": password}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "login {email}");
        let cookies: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect();
        let cookie_pair = |name: &str| {
            cookies
                .iter()
                .find(|cookie| cookie.starts_with(&format!("{name}=")))
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_string()
        };
        let sid_kv = cookie_pair("sid");
        let csrf_kv = cookie_pair("csrf");
        let csrf_token = csrf_kv.trim_start_matches("csrf=").to_string();
        format!("{sid_kv}; {csrf_kv}|{csrf_token}")
    }

    /// Create a top-level doc via HTTP and return its id.
    pub async fn create_doc(app: axum::Router, joined: &str, title: &str) -> String {
        let (status, body) = send(
            app,
            "POST",
            "/api/docs",
            joined,
            Some(json!({"title": title})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create doc {title}");
        let created: Value = serde_json::from_slice(&body).unwrap();
        created["id"].as_str().unwrap().to_string()
    }

    /// One request as the session in `joined`.
    pub async fn send(
        app: axum::Router,
        method: &str,
        uri: &str,
        joined: &str,
        body: Option<Value>,
    ) -> (StatusCode, Bytes) {
        let (cookie, csrf) = joined.split_once('|').unwrap();
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie)
            .header("x-csrf-token", csrf);
        let request_body = match body {
            Some(json_body) => {
                builder = builder.header("content-type", "application/json");
                Body::from(json_body.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .oneshot(builder.body(request_body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes)
    }

    /// Wait until the ACL listener has subscribed: a NOTIFY sent before that
    /// is lost.
    pub async fn wait_for_acl_listener(pool: &knot_storage::Pool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let listening: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND query ILIKE 'LISTEN%acl_invalidate%'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if listening > 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the ACL listener never subscribed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
