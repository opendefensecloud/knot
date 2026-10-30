//! Membership changes and ACL listener gaps reach cached access and open
//! sockets:
//! - a workspace role change evicts the cached access at once, so a demoted
//!   owner can't restrict a page (and so take it over) on a stale owner
//!   role;
//! - a role change, a removal (even of a member without grants) and a lost
//!   listener connection end every open doc and board socket with 1012, so
//!   each client reconnects through the upgrade's access check, which only
//!   refuses those who lost their access.
//!
//! See docs/superpowers/specs/2026-10-30-restricted-documents-design.md.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use serde_json::{Value, json};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers, copied from restricted_integration.rs.
// ---------------------------------------------------------------------------

/// Build an AppState with a seeded workspace + owner, log the owner in via
/// HTTP, and return the `(state, "sid=…; csrf=…|<csrf_token>")` pair.
async fn login_state(email: &str, password: &str) -> (AppState, String) {
    let pool = knot_test_support::fresh_db().await.pool;

    let mut state = AppState::with_pool(pool);
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = state.hasher.hash(password).unwrap();
    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local(email, "U", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    let app = router_with_state(state.clone());
    let joined = login_as(app, email, password).await;
    (state, joined)
}

fn split_cookie_csrf(joined: &str) -> (&str, &str) {
    joined.split_once('|').unwrap()
}

/// Invite a user to the workspace with `role` through
/// `POST /api/workspace/members` and return their id.
async fn invite_user(
    app: axum::Router,
    owner_joined: &str,
    email: &str,
    password: &str,
    role: &str,
    state: &AppState,
) -> String {
    let (status, _) = send(
        app,
        "POST",
        "/api/workspace/members",
        owner_joined,
        Some(json!({"email": email, "role": role, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "invite {email} as {role}");
    let user = state
        .users
        .as_ref()
        .unwrap()
        .find_by_email(email)
        .await
        .unwrap()
        .expect("invited user should exist");
    user.id.to_string()
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

/// Create a doc via HTTP, under `parent` when given, and return its id.
async fn create_doc(app: axum::Router, joined: &str, title: &str, parent: Option<&str>) -> String {
    let (status, body) = send(
        app,
        "POST",
        "/api/docs",
        joined,
        Some(json!({"title": title, "parent_id": parent})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create doc {title}");
    let created: Value = serde_json::from_slice(&body).unwrap();
    created["id"].as_str().unwrap().to_string()
}

/// One request as the session in `joined` (`None` sends no cookie).
async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    joined: &str,
    body: Option<Value>,
) -> (StatusCode, Bytes) {
    let (cookie, csrf) = split_cookie_csrf(joined);
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

fn json_of(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_| {
        panic!(
            "expected JSON, got {:?}",
            String::from_utf8_lossy(bytes.as_ref())
        )
    })
}

async fn set_restricted(
    app: axum::Router,
    joined: &str,
    doc_id: &str,
    restricted: bool,
) -> (StatusCode, Bytes) {
    send(
        app,
        "PUT",
        &format!("/api/docs/{doc_id}/restricted"),
        joined,
        Some(json!({"restricted": restricted})),
    )
    .await
}

/// Alice is the workspace owner; Bob is invited with `bob_role`.
struct Fixture {
    state: AppState,
    app: axum::Router,
    alice: String,
    bob: String,
    bob_id: String,
}

async fn fixture(bob_role: &str) -> Fixture {
    let (state, alice) = login_state("alice@restricted.test", "alicepass").await;
    let app = router_with_state(state.clone());
    let bob_id = invite_user(
        app.clone(),
        &alice,
        "bob@restricted.test",
        "bobpass1",
        bob_role,
        &state,
    )
    .await;
    let bob = login_as(app.clone(), "bob@restricted.test", "bobpass1").await;
    Fixture {
        state,
        app,
        alice,
        bob,
        bob_id,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
// Realtime: open doc and board sockets are revoked on restrict
// ---------------------------------------------------------------------------

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The fixture, with doc and board rooms on a MemBus, the ACL listener
/// running as in `main` (it evicts the cache and closes open sockets), and
/// the app served on an ephemeral port, whose address is returned.
async fn realtime_fixture(bob_role: &str) -> (Fixture, std::net::SocketAddr) {
    let mut fx = fixture(bob_role).await;
    let pool = fx.state.pool.clone().unwrap();
    let engine: Arc<dyn knot_crdt::Engine> = Arc::new(knot_crdt::YrsEngine);
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::MemBus::new());
    fx.state.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        engine.clone(),
        bus.clone(),
        Arc::new(knot_storage::PgUpdatesStore::new(pool.clone())),
        Arc::new(knot_storage::PgSnapshotStore::new(pool.clone())),
        knot_crdt::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(600),
        },
        std::time::Duration::from_secs(300),
    )));
    fx.state.board_rooms = Some(Arc::new(knot_crdt::BoardRooms::new(
        engine,
        fx.state.boards.clone().unwrap(),
        bus,
    )));
    fx.app = router_with_state(fx.state.clone());

    let _acl_listener = knot_docs::spawn_listener(
        pool.clone(),
        fx.state.acl.clone().unwrap(),
        fx.state.docs.clone().unwrap(),
        knot_server::revoke_doc_sockets(&fx.state),
    );
    wait_for_acl_listener(&pool).await;

    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp_listener.local_addr().unwrap();
    let served = fx.app.clone();
    tokio::spawn(async move {
        axum::serve(tcp_listener, served).await.unwrap();
    });
    (fx, addr)
}

/// Wait until the ACL listener has subscribed: a NOTIFY sent before that
/// is lost.
async fn wait_for_acl_listener(pool: &knot_storage::Pool) {
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

/// Dial `path` with `joined`'s session cookie.
async fn dial(
    addr: std::net::SocketAddr,
    path: &str,
    joined: &str,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("cookie", split_cookie_csrf(joined).0.parse().unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(socket, _)| socket)
}

/// Dial `path` and wait for the room's first frame (its state), which it
/// sends once the connection has joined.
async fn join_socket(addr: std::net::SocketAddr, path: &str, joined: &str) -> Socket {
    use futures_util::StreamExt;
    let mut socket = dial(addr, path, joined).await.expect("ws connect");
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
        .await
        .expect("the room never sent its state");
    assert!(
        matches!(
            first,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(_)))
        ),
        "expected the room's state, got {first:?}"
    );
    socket
}

/// The close code the server closes `socket` with, within `within`.
async fn close_code_within(socket: &mut Socket, within: std::time::Duration) -> u16 {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, socket.next()).await {
            Ok(Some(Ok(Message::Close(Some(frame))))) => return frame.code.into(),
            Ok(Some(Ok(Message::Close(None)))) | Ok(None) => {
                panic!("socket closed without a close code")
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) => panic!("ws error: {error}"),
            Err(_) => panic!("the server did not close the socket within {within:?}"),
        }
    }
}

/// The HTTP status a refused upgrade of `path` is answered with.
async fn refused_upgrade_status(addr: std::net::SocketAddr, path: &str, joined: &str) -> u16 {
    match dial(addr, path, joined).await {
        Ok(_) => panic!("the upgrade of {path} was accepted"),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(error) => panic!("the upgrade of {path} failed without a status: {error}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

async fn change_role(fx: &Fixture, user_id: &str, role: &str) -> StatusCode {
    let (status, _) = send(
        fx.app.clone(),
        "PATCH",
        &format!("/api/workspace/members/{user_id}"),
        &fx.alice,
        Some(json!({"role": role})),
    )
    .await;
    status
}

/// A page with one board, created by Alice; returns the doc and board
/// socket paths.
async fn page_with_board(fx: &Fixture) -> (String, String) {
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Plans", None).await;
    let (status, body) = send(
        fx.app.clone(),
        "POST",
        &format!("/api/docs/{doc_id}/boards"),
        &fx.alice,
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create board: {body:?}");
    let board_id = json_of(&body)["id"].as_str().unwrap().to_string();
    (
        format!("/collab/doc/{doc_id}"),
        format!("/collab/board/{board_id}"),
    )
}

/// Review finding: a demoted owner's cached owner role outlived the
/// demotion, and restricting a page with it made them its only owner,
/// hiding it from every other owner. The role change evicts the cached
/// access at once, on this replica too, without waiting for its listener.
#[tokio::test(flavor = "multi_thread")]
async fn demoted_owner_cannot_restrict_on_a_cached_owner_role() {
    let fx = fixture("owner").await;
    let doc_id = create_doc(fx.app.clone(), &fx.alice, "Board minutes", None).await;
    // Bob reads the page as an owner, which caches his access.
    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}"),
        &fx.bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["effective_role"], "owner");

    assert_eq!(
        change_role(&fx, &fx.bob_id, "viewer").await,
        StatusCode::NO_CONTENT
    );

    let (status, _) = set_restricted(fx.app.clone(), &fx.bob, &doc_id, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "restricted on a stale role");
    let (status, body) = send(
        fx.app.clone(),
        "GET",
        &format!("/api/docs/{doc_id}"),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["restricted"], false);
}

/// A role change reaches sockets that are already open, on every replica:
/// Bob's doc and board sockets close with 1012, a code the clients
/// reconnect on. Demoted to viewer, he gets the page back read-only, but
/// the board, which needs write access, is refused.
#[tokio::test(flavor = "multi_thread")]
async fn role_change_closes_open_sockets_for_a_recheck() {
    let (fx, addr) = realtime_fixture("editor").await;
    let (doc_path, board_path) = page_with_board(&fx).await;
    let mut doc_socket = join_socket(addr, &doc_path, &fx.bob).await;
    let mut board_socket = join_socket(addr, &board_path, &fx.bob).await;

    assert_eq!(
        change_role(&fx, &fx.bob_id, "viewer").await,
        StatusCode::NO_CONTENT
    );

    let within = std::time::Duration::from_secs(5);
    assert_eq!(close_code_within(&mut doc_socket, within).await, 1012);
    assert_eq!(close_code_within(&mut board_socket, within).await, 1012);
    join_socket(addr, &doc_path, &fx.bob).await;
    assert_eq!(
        refused_upgrade_status(addr, &board_path, &fx.bob).await,
        403
    );
}

/// Removing a member who holds no grant names no doc, yet their open
/// socket closes, and reopening it is refused.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_member_without_grants_closes_their_sockets() {
    let (fx, addr) = realtime_fixture("editor").await;
    let (doc_path, _board_path) = page_with_board(&fx).await;
    let mut doc_socket = join_socket(addr, &doc_path, &fx.bob).await;

    let (status, _) = send(
        fx.app.clone(),
        "DELETE",
        &format!("/api/workspace/members/{}", fx.bob_id),
        &fx.alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let within = std::time::Duration::from_secs(5);
    assert_eq!(close_code_within(&mut doc_socket, within).await, 1012);
    let refused_status = refused_upgrade_status(addr, &doc_path, &fx.bob).await;
    assert!(
        refused_status == 401 || refused_status == 403,
        "a removed member reopened the page: {refused_status}"
    );
}

/// NOTIFYs sent while the ACL listener has no connection are lost, and sqlx
/// reconnects without a word. The listener notices, and every open socket
/// is closed for a re-check, so a change it never heard of can't leave a
/// socket streaming: here Bob's page is restricted behind the listener's
/// back, and his reconnect is refused.
#[tokio::test(flavor = "multi_thread")]
async fn lost_listener_connection_closes_open_sockets_for_a_recheck() {
    let (fx, addr) = realtime_fixture("editor").await;
    let (doc_path, board_path) = page_with_board(&fx).await;
    let doc_id = doc_path.trim_start_matches("/collab/doc/").to_string();
    let mut doc_socket = join_socket(addr, &doc_path, &fx.bob).await;
    let mut board_socket = join_socket(addr, &board_path, &fx.bob).await;
    let pool = fx.state.pool.clone().unwrap();

    // A restrict whose NOTIFY never reaches the listener.
    sqlx::query("UPDATE documents SET restricted = true WHERE id = $1::uuid")
        .bind(&doc_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() AND query ILIKE 'LISTEN%acl_invalidate%'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let within = std::time::Duration::from_secs(10);
    assert_eq!(close_code_within(&mut doc_socket, within).await, 1012);
    assert_eq!(close_code_within(&mut board_socket, within).await, 1012);
    assert_eq!(refused_upgrade_status(addr, &doc_path, &fx.bob).await, 403);
    assert_eq!(
        refused_upgrade_status(addr, &board_path, &fx.bob).await,
        403
    );
}
