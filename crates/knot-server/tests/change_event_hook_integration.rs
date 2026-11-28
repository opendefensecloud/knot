//! Integration: the change event hook (spec:
//! docs/superpowers/specs/2026-11-22-change-event-hook-design.md).
//! knot connects out to a WebSocket receiver the test runs on its own port.

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
use yrs::{Doc, ReadTxn, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

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
            // Keep reading until a full window plus the start of the next
            // were seen (two distinct `at`) and a known page id arrived.
            let mut ats: HashSet<String> = HashSet::new();
            let mut saw_known = false;
            timeout(CYCLE, async {
                loop {
                    let ev = next_json(&mut ws).await;
                    assert_eq!(ev["type"], "page_changed", "{ev}");
                    assert_eq!(uuid_of(&ev, "workspace_id"), workspace_id);
                    let doc = uuid_of(&ev, "doc_id");
                    let at = ev["at"].as_str().unwrap().to_string();
                    ats.insert(at.clone());
                    let key = (at, doc, ev["kind"].as_str().unwrap().to_string());
                    assert!(
                        seen.insert(key.clone()),
                        "cycle {cycle}: {key:?} twice in one window"
                    );
                    saw_known |= known.contains(&doc);
                    if saw_known && ats.len() >= 2 {
                        break;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!("cycle {cycle}: no page id / second window in time (ats {ats:?})")
            });

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

/// Spec D4: 20 edits to one page inside one window → exactly one content
/// event, and nothing after it.
#[tokio::test(flavor = "multi_thread")]
async fn one_event_per_page_per_window() {
    let window = Duration::from_secs(2);
    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let pod = pod(&db, Some(test_hook(&url).window(window))).await;
    let mut ws = accept(&listener).await;
    expect_hello(&mut ws).await;

    let doc = create_doc(&pod, "once", None).await;
    let created = timeout(CYCLE, next_json(&mut ws))
        .await
        .expect("created event");
    assert_eq!(
        (created["kind"].as_str(), uuid_of(&created, "doc_id")),
        (Some("created"), doc)
    );

    // The burst goes straight into the room as a collab client's typing
    // does (`Event::Inbound` from a joined connection): 20 HTTP markdown
    // imports each wait for their persist, which the writer batches every
    // 250 ms, so they take ~5 s, longer than any window a test can afford.
    let room = pod
        .state
        .rooms_v2
        .as_ref()
        .unwrap()
        .acquire(doc)
        .await
        .unwrap();
    let conn_id = Uuid::new_v4();
    // Keep the receiving end: fan-out to this connection must not fail.
    let (conn_tx, _conn_rx) = tokio::sync::mpsc::channel(256);
    let (reply, joined) = tokio::sync::oneshot::channel();
    room.tx
        .send(knot_crdt::Event::Join {
            conn_id,
            handle: knot_crdt::ConnHandle::new(conn_tx),
            reply,
        })
        .await
        .unwrap();
    joined.await.unwrap().expect("join the room");
    let typist = Doc::with_client_id(7);
    let frag = typist.get_or_insert_xml_fragment("default");
    for i in 0..20 {
        let before = typist.transact().state_vector();
        {
            let mut txn = typist.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new(format!("burst {i}")));
        }
        let bytes = typist.transact().encode_state_as_update_v1(&before);
        room.tx
            .send(knot_crdt::Event::Inbound(knot_crdt::InMsg {
                from: conn_id,
                by_user: None,
                bytes,
            }))
            .await
            .unwrap();
    }

    let ev = timeout(window * 2 + Duration::from_secs(1), next_json(&mut ws))
        .await
        .expect("content event");
    assert_eq!(
        (ev["kind"].as_str(), uuid_of(&ev, "doc_id")),
        (Some("content"), doc),
        "{ev}"
    );
    assert!(
        timeout(window * 2, next_json(&mut ws)).await.is_err(),
        "no second event for the same burst"
    );
}

/// Review Focus 1: a receiver that accepts TCP but never answers the
/// handshake does not hang the hook; it times out and retries.
#[tokio::test(flavor = "multi_thread")]
async fn hangs_in_handshake_then_retries() {
    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let _pod = pod(
        &db,
        Some(test_hook(&url).connect_timeout(Duration::from_millis(300))),
    )
    .await;
    let (_silent, _) = timeout(CYCLE, listener.accept())
        .await
        .expect("first attempt")
        .unwrap();
    // Hold `_silent` open without a handshake; the next attempt must come.
    let mut ws = accept(&listener).await;
    expect_hello(&mut ws).await;
}

/// Review Focus 2: a receiver that stops reading (no pongs) is dropped
/// after two unanswered pings, and the hook reconnects.
#[tokio::test(flavor = "multi_thread")]
async fn silent_receiver_is_dropped_and_reconnected() {
    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let _pod = pod(
        &db,
        Some(test_hook(&url).ping_interval(Duration::from_millis(100))),
    )
    .await;
    let mut first = accept(&listener).await;
    let a = expect_hello(&mut first).await;
    // Stop reading `first`: tungstenite answers pings only while reading.
    let mut second = accept(&listener).await;
    assert_eq!(expect_hello(&mut second).await, a);
    drop(first);
}

/// Review Focus 3: a refused handshake is retried after a backoff, not in a
/// tight loop, and every attempt carries the bearer token.
#[tokio::test(flavor = "multi_thread")]
// tungstenite's handshake `Callback` fixes the `ErrorResponse` error type.
#[allow(clippy::result_large_err)]
async fn rejected_handshake_backs_off_and_sends_token() {
    use tokio_tungstenite::tungstenite::handshake::server::{
        ErrorResponse, Request as HsReq, Response as HsResp,
    };

    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let _pod = pod(&db, Some(test_hook(&url).token(Some("s3cret".into())))).await;

    let mut last: Option<tokio::time::Instant> = None;
    for attempt in 0..3 {
        let (tcp, _) = timeout(CYCLE, listener.accept())
            .await
            .expect("attempt")
            .unwrap();
        let now = tokio::time::Instant::now();
        if let Some(prev) = last {
            assert!(
                now - prev >= Duration::from_millis(20),
                "attempt {attempt} came after {:?}",
                now - prev
            );
        }
        last = Some(now);
        let refuse = attempt < 2;
        let result = tokio_tungstenite::accept_hdr_async(tcp, move |req: &HsReq, resp: HsResp| {
            assert_eq!(req.headers().get("authorization").unwrap(), "Bearer s3cret");
            if refuse {
                let mut err = ErrorResponse::new(None);
                *err.status_mut() = StatusCode::UNAUTHORIZED;
                Err(err)
            } else {
                Ok(resp)
            }
        })
        .await;
        if !refuse {
            let mut ws = result.unwrap();
            expect_hello(&mut ws).await;
        }
    }
}

/// Review Focus 4: on shutdown the hook says goodbye and stays away.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_closes_and_stays_closed() {
    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let pod = pod(&db, Some(test_hook(&url))).await;
    let mut ws = accept(&listener).await;
    expect_hello(&mut ws).await;
    pod.state.shutdown.cancel();
    let frame = timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("close in time");
    assert!(
        matches!(frame, Some(Ok(Message::Close(_)))),
        "got {frame:?}"
    );
    assert!(
        timeout(Duration::from_secs(1), listener.accept())
            .await
            .is_err(),
        "no reconnect after shutdown"
    );
}

/// Read until `kind` arrives for every page in `docs` (other events skipped).
async fn expect_kinds(ws: &mut Ws, kind: &str, docs: &[Uuid]) {
    let mut missing: HashSet<Uuid> = docs.iter().copied().collect();
    timeout(CYCLE, async {
        while !missing.is_empty() {
            let ev = next_json(ws).await;
            if ev["kind"] == kind {
                missing.remove(&uuid_of(&ev, "doc_id"));
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} for {missing:?}"));
}

/// Spec D3 + D12: every lifecycle change reports its kind, bulk operations
/// one event per page.
#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_changes_report_their_kind() {
    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let pod = pod(&db, Some(test_hook(&url))).await;
    let mut ws = accept(&listener).await;
    expect_hello(&mut ws).await;
    let (app, s) = (&pod.app, pod.session.as_str());

    let parent = create_doc(&pod, "parent", None).await;
    expect_kinds(&mut ws, "created", &[parent]).await;
    let child = create_doc(&pod, "child", Some(parent)).await;
    expect_kinds(&mut ws, "created", &[child]).await;

    let (st, body) = send(
        app,
        s,
        "POST",
        &format!("/api/docs/{parent}/duplicate"),
        Some(json!({"include_subpages": true})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body:?}");
    let top: Uuid = serde_json::from_slice::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let copies = pod
        .state
        .docs
        .as_ref()
        .unwrap()
        .subtree_ids(top)
        .await
        .unwrap();
    assert_eq!(copies.len(), 2, "copy of parent and child");
    expect_kinds(&mut ws, "created", &copies).await;

    let (st, _) = send(app, s, "DELETE", &format!("/api/docs/{parent}"), None, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    expect_kinds(&mut ws, "archived", &[parent, child]).await;

    let (st, _) = send(
        app,
        s,
        "POST",
        &format!("/api/docs/{parent}/restore"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    expect_kinds(&mut ws, "restored", &[parent, child]).await;

    let (st, _) = send(
        app,
        s,
        "PATCH",
        &format!("/api/docs/{child}"),
        Some(json!({"title": "renamed"})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    expect_kinds(&mut ws, "renamed", &[child]).await;

    let (st, _) = send(
        app,
        s,
        "POST",
        &format!("/api/docs/{child}/move"),
        Some(json!({"parent_id": null})),
        None,
    )
    .await;
    assert!(st.is_success(), "move {st}");
    expect_kinds(&mut ws, "moved", &[child]).await;

    let (st, _) = send(app, s, "DELETE", &format!("/api/docs/{parent}"), None, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    expect_kinds(&mut ws, "archived", &[parent]).await;
    let (st, _) = send(
        app,
        s,
        "POST",
        &format!("/api/docs/{parent}/purge"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    expect_kinds(&mut ws, "purged", &[parent]).await;
}

/// Review Focus 5: a page purged inside the window of a content edit yields
/// `purged`, and no `content` event with a missing workspace.
#[tokio::test(flavor = "multi_thread")]
async fn purge_inside_window_drops_content_event() {
    let window = Duration::from_secs(2);
    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let pod = pod(&db, Some(test_hook(&url).window(window))).await;
    let mut ws = accept(&listener).await;
    expect_hello(&mut ws).await;
    let doc = create_doc(&pod, "doomed", None).await;
    expect_kinds(&mut ws, "created", &[doc]).await;

    edit(&pod.app, &pod.session, doc, "last words").await;
    let (st, _) = send(
        &pod.app,
        &pod.session,
        "DELETE",
        &format!("/api/docs/{doc}"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = send(
        &pod.app,
        &pod.session,
        "POST",
        &format!("/api/docs/{doc}/purge"),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let mut kinds = Vec::new();
    while let Ok(ev) = timeout(window * 2, next_json(&mut ws)).await {
        assert!(ev["workspace_id"].as_str().is_some(), "{ev}");
        kinds.push(ev["kind"].as_str().unwrap().to_string());
    }
    assert!(kinds.contains(&"purged".to_string()), "{kinds:?}");
    assert!(
        !kinds.contains(&"content".to_string()),
        "content of a purged page: {kinds:?}"
    );
}

/// Spec D5: two replicas on one database, each with its own hook
/// connection. An edit is reported by the replica it happened on only,
/// although the other replica's room receives it over the bus.
#[tokio::test(flavor = "multi_thread")]
async fn each_pod_reports_only_its_own_edits() {
    let window = Duration::from_millis(200);
    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener_a, url_a) = receiver().await;
    let (listener_b, url_b) = receiver().await;
    let (dirty_b_tx, mut dirty_b) = tokio::sync::mpsc::channel(64);
    let a = pod(&db, Some(test_hook(&url_a).window(window))).await;
    let b = pod_with(
        &db,
        Some(test_hook(&url_b).window(window)),
        Some(dirty_b_tx),
    )
    .await;
    let mut ws_a = accept(&listener_a).await;
    let mut ws_b = accept(&listener_b).await;
    assert_ne!(
        expect_hello(&mut ws_a).await,
        expect_hello(&mut ws_b).await,
        "two processes"
    );

    let doc = create_doc(&a, "shared", None).await;
    expect_kinds(&mut ws_a, "created", &[doc]).await;
    // B has the page open, so it receives A's rows over the bus.
    let _room_b = b
        .state
        .rooms_v2
        .as_ref()
        .unwrap()
        .acquire(doc)
        .await
        .unwrap();

    edit(&a.app, &a.session, doc, "from A").await;
    expect_kinds(&mut ws_a, "content", &[doc]).await;
    let got = timeout(CYCLE, dirty_b.recv())
        .await
        .expect("B's room received A's edit");
    assert_eq!(got, Some(doc));
    assert!(
        timeout(window * 3, next_json(&mut ws_b)).await.is_err(),
        "B must not report A's edit"
    );

    edit(&b.app, &b.session, doc, "from B").await;
    expect_kinds(&mut ws_b, "content", &[doc]).await;
    assert!(
        timeout(window * 3, next_json(&mut ws_a)).await.is_err(),
        "A must not report B's edit"
    );
}
