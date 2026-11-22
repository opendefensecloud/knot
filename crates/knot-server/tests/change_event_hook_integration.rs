//! Integration: the change event hook (spec:
//! docs/superpowers/specs/2026-11-22-change-event-hook-design.md).
//! knot connects out to a WebSocket receiver the test runs on its own port.

// Helpers are used by tests added in Tasks 4-6.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::change_events::Hook;
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;
use uuid::Uuid;

const EMAIL: &str = "alice@hook.test";
const PASSWORD: &str = "password1";
/// Per read-and-disconnect cycle.
const CYCLE: Duration = Duration::from_secs(10);

type Ws = WebSocketStream<TcpStream>;

// ---------------------------------------------------------------------------
// knot side
// ---------------------------------------------------------------------------

/// One workspace with one owner. Returns the workspace id.
async fn seed(pool: &knot_storage::Pool) -> Uuid {
    let state = AppState::with_pool(pool.clone());
    let hash = Hasher::fast_for_tests().hash(PASSWORD).unwrap();
    let ws = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "WS")
        .await
        .unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local(EMAIL, "Alice", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws.id
}

/// One knot replica on the shared test database.
struct Pod {
    state: AppState,
    app: axum::Router,
    /// `"sid=…; csrf=…|<csrf token>"`.
    session: String,
}

async fn pod(db: &knot_test_support::TestDb, hook: Option<Hook>) -> Pod {
    pod_with(db, hook, None).await
}

/// A replica with its own PgBus and Rooms, like a separate process.
async fn pod_with(
    db: &knot_test_support::TestDb,
    hook: Option<Hook>,
    dirty_tx: Option<tokio::sync::mpsc::Sender<Uuid>>,
) -> Pod {
    let mut state = AppState::with_pool(db.pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::PgBus::connect(&db.url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(db.pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(db.pool.clone()));
    let policy = SnapshotPolicy {
        every_n: 1000,
        idle: Duration::from_secs(3600),
    };
    let mut rooms = Rooms::new(
        Arc::new(YrsEngine),
        bus.clone(),
        updates,
        snaps,
        policy,
        Duration::from_secs(3600),
    );
    if let Some(tx) = dirty_tx {
        rooms = rooms.with_dirty_tx(tx);
    }
    if let Some(hook) = hook {
        let events = hook.spawn(state.docs.clone().unwrap(), state.shutdown.clone());
        rooms = rooms.with_local_change_tx(events.content_sender());
        state.change_events = Some(events);
    }
    state.bus = Some(bus);
    state.rooms_v2 = Some(Arc::new(rooms));
    let app = router_with_state(state.clone());
    let session = login(&app).await;
    Pod {
        state,
        app,
        session,
    }
}

/// Hook settings that keep a test fast.
fn test_hook(url: &str) -> Hook {
    Hook::new(url)
        .backoff(Duration::from_millis(50), Duration::from_millis(200))
        .window(Duration::from_millis(100))
        .ping_interval(Duration::from_secs(30))
}

async fn login(app: &axum::Router) -> String {
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
    let pick = |name: &str| {
        cookies
            .iter()
            .find(|c| c.starts_with(&format!("{name}=")))
            .unwrap()
            .clone()
    };
    let (sid, csrf) = (pick("sid"), pick("csrf"));
    let token = csrf.trim_start_matches("csrf=").to_string();
    format!("{sid}; {csrf}|{token}")
}

/// One request as `session`; `body` is JSON, or raw markdown with `markdown`.
async fn send(
    app: &axum::Router,
    session: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
    markdown: Option<String>,
) -> (StatusCode, Bytes) {
    let (cookie, csrf) = session.split_once('|').unwrap();
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie)
        .header("x-csrf-token", csrf);
    let request_body = match (body, markdown) {
        (Some(json_body), _) => {
            builder = builder.header("content-type", "application/json");
            Body::from(json_body.to_string())
        }
        (None, Some(md)) => {
            builder = builder.header("content-type", "text/markdown");
            Body::from(md)
        }
        (None, None) => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(request_body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}

/// Create a page (under `parent` when given) and return its id.
async fn create_doc(pod: &Pod, title: &str, parent: Option<Uuid>) -> Uuid {
    let (status, body) = send(
        &pod.app,
        &pod.session,
        "POST",
        "/api/docs",
        Some(json!({"title": title, "parent_id": parent})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create {title}");
    let v: Value = serde_json::from_slice(&body).unwrap();
    v["id"].as_str().unwrap().parse().unwrap()
}

/// Append a paragraph to the page through the room (a content change).
async fn edit(app: &axum::Router, session: &str, doc: Uuid, text: &str) {
    let (status, body) = send(
        app,
        session,
        "POST",
        &format!("/api/docs/{doc}/markdown"),
        None,
        Some(format!("{text}\n")),
    )
    .await;
    assert!(status.is_success(), "markdown import {status}: {body:?}");
}

/// Edits the pages round-robin every ~50 ms until aborted.
fn spawn_writer(pod: &Pod, ids: Arc<Vec<Uuid>>) -> tokio::task::JoinHandle<()> {
    let (app, session) = (pod.app.clone(), pod.session.clone());
    tokio::spawn(async move {
        for i in 0usize.. {
            edit(&app, &session, ids[i % ids.len()], &format!("edit {i}")).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// receiver side
// ---------------------------------------------------------------------------

async fn receiver() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    (listener, url)
}

/// Wait for knot to connect and complete the WebSocket handshake.
async fn accept(listener: &TcpListener) -> Ws {
    let (tcp, _) = timeout(CYCLE, listener.accept())
        .await
        .expect("knot connected in time")
        .unwrap();
    tokio_tungstenite::accept_async(tcp).await.unwrap()
}

/// The next text frame as JSON (pings are answered by tungstenite on read).
async fn next_json(ws: &mut Ws) -> Value {
    loop {
        match ws.next().await.expect("connection open").expect("frame") {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

/// Read the hello frame and return its instance id.
async fn expect_hello(ws: &mut Ws) -> Uuid {
    let hello = timeout(CYCLE, next_json(ws)).await.expect("hello in time");
    assert_eq!(hello["type"], "hello", "first frame: {hello}");
    assert!(hello["version"].as_str().is_some_and(|v| !v.is_empty()));
    hello["instance_id"].as_str().unwrap().parse().unwrap()
}

fn uuid_of(v: &Value, field: &str) -> Uuid {
    v[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} in {v}"))
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// The request's test: pages are created and keep changing; three times the
/// receiver reads a valid page id and then drops the connection (abrupt TCP
/// drop, close frame, abrupt drop); knot reconnects each time, as the same
/// process, and sends each (page, kind) at most once per window.
#[tokio::test(flavor = "multi_thread")]
async fn reconnects_after_each_drop_and_streams_page_ids() {
    timeout(Duration::from_secs(30), async {
        let db = knot_test_support::fresh_db().await;
        let workspace_id = seed(&db.pool).await;
        let (listener, url) = receiver().await;
        let pod = pod(&db, Some(test_hook(&url))).await;

        let mut ids = Vec::new();
        for n in 0..5 {
            ids.push(create_doc(&pod, &format!("page {n}"), None).await);
        }
        let known: HashSet<Uuid> = ids.iter().copied().collect();
        let writer = spawn_writer(&pod, Arc::new(ids));

        let mut instance: Option<Uuid> = None;
        for cycle in 0..3 {
            let mut ws = accept(&listener).await;
            let id = expect_hello(&mut ws).await;
            assert_eq!(
                *instance.get_or_insert(id),
                id,
                "cycle {cycle}: same process reconnected"
            );

            // (at, doc_id, kind) — one window shares one `at`.
            let mut seen: HashSet<(String, Uuid, String)> = HashSet::new();
            timeout(CYCLE, async {
                loop {
                    let ev = next_json(&mut ws).await;
                    assert_eq!(ev["type"], "page_changed", "{ev}");
                    assert_eq!(uuid_of(&ev, "workspace_id"), workspace_id);
                    let doc = uuid_of(&ev, "doc_id");
                    let key = (
                        ev["at"].as_str().unwrap().to_string(),
                        doc,
                        ev["kind"].as_str().unwrap().to_string(),
                    );
                    assert!(
                        seen.insert(key.clone()),
                        "cycle {cycle}: {key:?} twice in one window"
                    );
                    if known.contains(&doc) {
                        break;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle}: no page id in time"));

            if cycle % 2 == 0 {
                // Abrupt: the TCP connection goes away without a close frame.
                drop(ws);
            } else {
                ws.close(None).await.unwrap();
                let _ = timeout(Duration::from_secs(2), async {
                    while ws.next().await.is_some() {}
                })
                .await;
            }
        }
        writer.abort();
    })
    .await
    .expect("whole test within 30 s");
}
