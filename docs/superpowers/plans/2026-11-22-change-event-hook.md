# Change Event Hook Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When `KNOT_CHANGE_EVENT_HOOK_URL` is set, every knot pod keeps a WebSocket connection to that receiver open and streams one JSON event per changed page per coalescing window, reconnecting whenever the connection drops.

**Architecture:** A new `knot_server::change_events` module owns two tasks: a coalescer (dedupes `(doc_id, kind)` over a leading-edge window) and a connection task (connect, hello, send, ping/pong, backoff, reconnect). Lifecycle changes enter from HTTP handlers through `AppState::emit_change`; content changes enter from a new local-only hook on the CRDT room (`Rooms::with_local_change_tx`), fired after the pod's own writer persists, so pods never report each other's edits.

**Tech Stack:** Rust, tokio, axum, tokio-tungstenite 0.30 (client with rustls/ring + webpki roots), serde_json, chrono, metrics 0.24, Postgres via `knot-test-support`.

**Spec:** `docs/superpowers/specs/2026-11-22-change-event-hook-design.md`

## Global Constraints

- Env var names: `KNOT_CHANGE_EVENT_HOOK_URL`, `KNOT_CHANGE_EVENT_HOOK_TOKEN`; config fields `change_event_hook_url`, `change_event_hook_token` (both `String`, default empty). No other new env vars.
- Feature off (empty URL) ⇒ no task spawned, `AppState.change_events == None`, every emit is a no-op.
- URL scheme must be `ws` or `wss`; otherwise `ConfigError::Invalid` (exit 2).
- Wire: hello `{"type":"hello","instance_id":"<uuid>","version":"<CARGO_PKG_VERSION>"}`; event `{"type":"page_changed","doc_id","workspace_id","kind","at"}`; `kind` ∈ `content|created|renamed|moved|archived|restored|purged`; `at` = window close time, identical for all events of one window. No titles, no content.
- Auth header when token set: `Authorization: Bearer <token>`.
- Production defaults: window 1 s, backoff 500 ms → 30 s (delay drawn from `[cur/2, cur]`), ping 30 s, 2 missed pongs = dead, connect timeout 10 s, send timeout 10 s, input queue capacity 4096.
- Delivery is at most once: drop on full queue / no connection / failed send; never buffer across reconnects, never retry.
- Metrics: `knot_change_event_hook_connected` (gauge), `knot_change_event_hook_events_sent_total`, `knot_change_event_hook_events_dropped_total{reason="queue_full"|"disconnected"|"send_failed"}`, `knot_change_event_hook_connects_total{result="ok"|"error"}`.
- Only Pong and Close frames from the receiver are acted on; others are logged at debug and ignored.
- Content events only from the local writer (`Room::on_applied`), never from the bus path.
- Restricted pages are included. Bulk operations emit one event per affected page.
- Tests use the dev-compose Postgres via `knot_test_support::fresh_db()` (never testcontainers). Run Rust tests with `cargo nextest run -p <crate> --all-features -E 'test(<name>)'` (the Makefile uses nextest).

## Review Focus

1. **Receiver accepts TCP but never answers the WebSocket handshake** → knot gives up after the connect timeout and retries; it does not hang forever. Pinned in Task 4 (`hangs_in_handshake_then_retries`).
2. **Receiver goes silent (half-open connection, no pongs)** → after 2 unanswered pings knot drops the connection and reconnects. Pinned in Task 4 (`silent_receiver_is_dropped_and_reconnected`).
3. **Receiver rejects the handshake (HTTP 401)** → knot backs off (no tight loop) and sends the bearer token on each attempt. Pinned in Task 4 (`rejected_handshake_backs_off_and_sends_token`).
4. **Server shutdown while connected** → knot sends a Close frame and does not reconnect. Pinned in Task 4 (`shutdown_closes_and_stays_closed`).
5. **Page purged inside the window of a content edit** → no `content` event with a missing workspace, no panic; the `purged` event still arrives. Pinned in Task 5 (`purge_inside_window_drops_content_event`).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/knot-crdt/src/registry.rs` (modify) | `Rooms::with_local_change_tx`, pass sender to each room |
| `crates/knot-crdt/src/room.rs` (modify) | `local_change_tx` field on `RoomDeps`/`Room`, fire in `on_applied` only |
| `crates/knot-crdt/tests/local_change.rs` (create) | local-only firing vs bus-received updates |
| `crates/knot-server/src/change_events/mod.rs` (create) | public API: `Hook`, `ChangeEvents`, `ChangeKind`, `WireMessage`; spawn |
| `crates/knot-server/src/change_events/coalesce.rs` (create) | `Pending` window/dedupe logic + coalescer task |
| `crates/knot-server/src/change_events/conn.rs` (create) | `Backoff`, TLS config, connection task |
| `crates/knot-server/src/lib.rs` (modify) | `pub mod change_events;`, `AppState.change_events`, `emit_change(s)` |
| `crates/knot-server/src/routes/api/docs.rs`, `archive.rs`, `routes/api/export_import.rs` (modify) | lifecycle emit calls |
| `crates/knot-server/src/main.rs` (modify) | wire hook from config |
| `crates/knot-server/Cargo.toml` (modify) | tokio-tungstenite runtime dep with rustls, rustls, webpki-roots |
| `crates/knot-config/src/lib.rs` (modify), `crates/knot-config/tests/load.rs` (modify) | two fields + validation + tests |
| `crates/knot-obs/src/metrics.rs` (modify) | metric describes |
| `crates/knot-server/tests/change_event_hook_integration.rs` (create) | all integration tests |
| `docs/change-event-hook.md` (create), `README.md`, `deploy/helm/knot/{values.yaml,values.schema.json,templates/configmap.yaml,templates/deployment.yaml,README.md}`, `CHANGELOG.md` (modify) | receiver doc + deployment wiring |

---

### Task 1: Reconnect integration test (red) with the API skeleton

The test from the original request. It must compile, so this task also adds the public API as a skeleton that spawns nothing; the test then fails by timing out waiting for knot to connect.

**Files:**
- Create: `crates/knot-server/src/change_events/mod.rs`
- Modify: `crates/knot-server/src/lib.rs` (module + `AppState` field + emit helpers)
- Modify: `crates/knot-crdt/src/registry.rs` (builder only, stores the sender; firing comes in Task 2)
- Test: `crates/knot-server/tests/change_event_hook_integration.rs`

**Interfaces:**
- Produces (used by every later task):
  - `knot_server::change_events::Hook` with `new(url: impl Into<String>) -> Hook`, `token(self, Option<String>) -> Hook`, `backoff(self, min: Duration, max: Duration) -> Hook`, `window(self, Duration) -> Hook`, `ping_interval(self, Duration) -> Hook`, `connect_timeout(self, Duration) -> Hook`, `send_timeout(self, Duration) -> Hook`, `queue_capacity(self, usize) -> Hook`, `spawn(self, docs: Arc<dyn knot_storage::DocStore>, shutdown: CancellationToken) -> ChangeEvents`.
  - `knot_server::change_events::ChangeEvents` (Clone) with `emit(&self, workspace_id: Uuid, doc_id: Uuid, kind: ChangeKind)`, `emit_many(&self, workspace_id: Uuid, doc_ids: &[Uuid], kind: ChangeKind)`, `content_sender(&self) -> mpsc::Sender<Uuid>`, `instance_id(&self) -> Uuid`.
  - `knot_server::change_events::ChangeKind` { `Content, Created, Renamed, Moved, Archived, Restored, Purged` }, serde snake_case.
  - `knot_server::change_events::WireMessage` { `Hello { instance_id: Uuid, version: &'static str }`, `PageChanged { doc_id: Uuid, workspace_id: Uuid, kind: ChangeKind, at: DateTime<Utc> }` }, serde `tag = "type"`, snake_case.
  - `AppState.change_events: Option<ChangeEvents>`; `AppState::emit_change(&self, workspace_id, doc_id, kind)`, `AppState::emit_changes(&self, workspace_id, &[Uuid], kind)`.
  - `knot_crdt::Rooms::with_local_change_tx(self, tx: mpsc::Sender<Uuid>) -> Self`.

- [ ] **Step 1: Add the `knot-crdt` builder (stores only)**

In `crates/knot-crdt/src/registry.rs`, add a field to `Rooms` right after `dirty_tx`:

```rust
    /// Optional notifier fired with the room's `doc_id` after each persist
    /// of an update applied **on this replica** (never for rows received
    /// over the bus). Feeds the change event hook in knot-server.
    local_change_tx: Option<mpsc::Sender<Uuid>>,
```

initialise it to `None` in `Rooms::new`, and add after `with_dirty_tx`:

```rust
    /// Builder: attach a notifier for updates applied and persisted on this
    /// replica only (see the field). `try_send`; a full channel drops.
    pub fn with_local_change_tx(mut self, tx: mpsc::Sender<Uuid>) -> Self {
        self.local_change_tx = Some(tx);
        self
    }
```

- [ ] **Step 2: Add the skeleton module**

Create `crates/knot-server/src/change_events/mod.rs`:

```rust
//! Change event hook: streams the UUIDs of changed pages to an external
//! WebSocket receiver (spec:
//! `docs/superpowers/specs/2026-11-22-change-event-hook-design.md`).
//!
//! Off unless `KNOT_CHANGE_EVENT_HOOK_URL` is set. Delivery is at most once.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// What happened to a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Content,
    Created,
    Renamed,
    Moved,
    Archived,
    Restored,
    Purged,
}

/// One text frame on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    Hello {
        instance_id: Uuid,
        version: &'static str,
    },
    PageChanged {
        doc_id: Uuid,
        workspace_id: Uuid,
        kind: ChangeKind,
        at: DateTime<Utc>,
    },
}

/// A lifecycle change as handlers report it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lifecycle {
    pub workspace_id: Uuid,
    pub doc_id: Uuid,
    pub kind: ChangeKind,
}

/// Configuration of the hook; `spawn` starts it.
#[derive(Debug, Clone)]
pub struct Hook {
    pub(crate) url: String,
    pub(crate) token: Option<String>,
    pub(crate) backoff_min: Duration,
    pub(crate) backoff_max: Duration,
    pub(crate) window: Duration,
    pub(crate) ping_interval: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) send_timeout: Duration,
    pub(crate) queue_capacity: usize,
}

impl Hook {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: None,
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            window: Duration::from_secs(1),
            ping_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            send_timeout: Duration::from_secs(10),
            queue_capacity: 4096,
        }
    }
    pub fn token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|t| !t.is_empty());
        self
    }
    pub fn backoff(mut self, min: Duration, max: Duration) -> Self {
        self.backoff_min = min;
        self.backoff_max = max.max(min);
        self
    }
    pub fn window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }
    pub fn ping_interval(mut self, every: Duration) -> Self {
        self.ping_interval = every;
        self
    }
    pub fn connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }
    pub fn send_timeout(mut self, d: Duration) -> Self {
        self.send_timeout = d;
        self
    }
    pub fn queue_capacity(mut self, n: usize) -> Self {
        self.queue_capacity = n.max(1);
        self
    }

    /// Start the hook's tasks. They end when `shutdown` is cancelled.
    pub fn spawn(
        self,
        _docs: Arc<dyn knot_storage::DocStore>,
        _shutdown: CancellationToken,
    ) -> ChangeEvents {
        // Skeleton (Task 1): channels exist, nothing consumes them yet.
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(self.queue_capacity);
        let (content_tx, _content_rx) = mpsc::channel(self.queue_capacity);
        ChangeEvents {
            lifecycle_tx,
            content_tx,
            instance_id: Uuid::new_v4(),
        }
    }
}

/// Handle to a running hook. Cheap to clone; emits never block.
#[derive(Debug, Clone)]
pub struct ChangeEvents {
    lifecycle_tx: mpsc::Sender<Lifecycle>,
    content_tx: mpsc::Sender<Uuid>,
    instance_id: Uuid,
}

impl ChangeEvents {
    pub fn emit(&self, workspace_id: Uuid, doc_id: Uuid, kind: ChangeKind) {
        match self.lifecycle_tx.try_send(Lifecycle {
            workspace_id,
            doc_id,
            kind,
        }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!("knot_change_event_hook_events_dropped_total", "reason" => "queue_full")
                    .increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
    pub fn emit_many(&self, workspace_id: Uuid, doc_ids: &[Uuid], kind: ChangeKind) {
        for &doc_id in doc_ids {
            self.emit(workspace_id, doc_id, kind);
        }
    }
    /// Sender for `knot_crdt::Rooms::with_local_change_tx`.
    pub fn content_sender(&self) -> mpsc::Sender<Uuid> {
        self.content_tx.clone()
    }
    pub fn instance_id(&self) -> Uuid {
        self.instance_id
    }
}
```

- [ ] **Step 3: Wire it into `AppState`**

In `crates/knot-server/src/lib.rs`: add `pub mod change_events;` in the module list (alphabetical, after `pub mod bots;`). Add to `pub struct AppState` (after `contributors`):

```rust
    /// The change event hook; `None` when `KNOT_CHANGE_EVENT_HOOK_URL` is unset.
    pub change_events: Option<change_events::ChangeEvents>,
```

Set `change_events: None,` in both `AppState::in_memory()` and `AppState::with_pool()` struct literals. Add to `impl AppState`:

```rust
    /// Report a page change to the change event hook (no-op when off).
    pub fn emit_change(&self, workspace_id: Uuid, doc_id: Uuid, kind: change_events::ChangeKind) {
        if let Some(events) = &self.change_events {
            events.emit(workspace_id, doc_id, kind);
        }
    }

    /// [`Self::emit_change`] for each page of a bulk operation.
    pub fn emit_changes(&self, workspace_id: Uuid, doc_ids: &[Uuid], kind: change_events::ChangeKind) {
        if let Some(events) = &self.change_events {
            events.emit_many(workspace_id, doc_ids, kind);
        }
    }
```

(If `Uuid` is not imported in `lib.rs`, use `uuid::Uuid`.)

Run: `cargo build -p knot-server`
Expected: builds.

- [ ] **Step 4: Write the failing test**

Create `crates/knot-server/tests/change_event_hook_integration.rs`. The helpers here are reused by Tasks 4–6; keep them in this file.

```rust
//! Integration: the change event hook (spec:
//! docs/superpowers/specs/2026-11-22-change-event-hook-design.md).
//! knot connects out to a WebSocket receiver the test runs on its own port.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
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
    let ws = state.workspaces.as_ref().unwrap().create("default", "WS").await.unwrap();
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
    Pod { state, app, session }
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
                .body(Body::from(json!({"email": EMAIL, "password": PASSWORD}).to_string()))
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
    let response = app.clone().oneshot(builder.body(request_body).unwrap()).await.unwrap();
    let status = response.status();
    (status, response.into_body().collect().await.unwrap().to_bytes())
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
    v[field].as_str().unwrap_or_else(|| panic!("{field} in {v}")).parse().unwrap()
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
            assert_eq!(*instance.get_or_insert(id), id, "cycle {cycle}: same process reconnected");

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
                    assert!(seen.insert(key.clone()), "cycle {cycle}: {key:?} twice in one window");
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
                let _ = timeout(Duration::from_secs(2), async { while ws.next().await.is_some() {} }).await;
            }
        }
        writer.abort();
    })
    .await
    .expect("whole test within 30 s");
}
```

- [ ] **Step 5: Run it and watch it fail**

Run: `cargo nextest run -p knot-server --all-features -E 'test(reconnects_after_each_drop_and_streams_page_ids)'`
Expected: FAIL with panic `knot connected in time` (the skeleton never connects). If it fails any other way (login, create, markdown import status), fix the test helper first — the only acceptable red is the missing connection.

- [ ] **Step 6: Commit**

```bash
git add crates/knot-crdt/src/registry.rs crates/knot-server/src/change_events/mod.rs crates/knot-server/src/lib.rs crates/knot-server/tests/change_event_hook_integration.rs
git commit -m "test: change event hook reconnect test (red) and API skeleton"
```

---

### Task 2: Local-only content notifications from the room

**Files:**
- Modify: `crates/knot-crdt/src/registry.rs` (pass the sender to each room)
- Modify: `crates/knot-crdt/src/room.rs` (`RoomDeps`, `Room`, `spawn_with_limits`, `spawn`, `spawn_detached`, `on_applied`)
- Test: `crates/knot-crdt/tests/local_change.rs`

**Interfaces:**
- Consumes: `Rooms::with_local_change_tx` (Task 1).
- Produces: each `Room` fires `local_change_tx.try_send(doc_id)` in `on_applied` only. `Room::spawn_with_limits` gains a trailing-before-limits parameter `local_change_tx: Option<mpsc::Sender<Uuid>>` (signature: `…, dirty_tx, local_change_tx, limits`).

- [ ] **Step 1: Write the failing test**

Create `crates/knot-crdt/tests/local_change.rs`:

```rust
//! The local change notifier fires on the replica that applied an update,
//! never on a replica that received it over the bus (change event hook,
//! spec D5).

use std::sync::Arc;
use std::time::Duration;

use knot_crdt::{Bus, Event, PgBus, Rooms, SnapshotPolicy, YrsEngine};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore};
use tokio::sync::mpsc;
use uuid::Uuid;

const POLICY: SnapshotPolicy = SnapshotPolicy {
    every_n: 1000,
    idle: Duration::from_secs(3600),
};

async fn page(pool: &sqlx::PgPool) -> Uuid {
    let ws: Uuid = sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap();
    let user: Uuid = sqlx::query_scalar("INSERT INTO users (email, display_name) VALUES ('u@x.test', 'U') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by) VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A replica: its own bus connection and registry, with both notifiers.
async fn replica(
    db: &knot_test_support::TestDb,
) -> (Arc<Rooms>, mpsc::Receiver<Uuid>, mpsc::Receiver<Uuid>) {
    let bus: Arc<dyn Bus> = Arc::new(PgBus::connect(&db.url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(db.pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(db.pool.clone()));
    let (local_tx, local_rx) = mpsc::channel(64);
    let (dirty_tx, dirty_rx) = mpsc::channel(64);
    let rooms = Rooms::new(Arc::new(YrsEngine), bus, updates, snaps, POLICY, Duration::from_secs(3600))
        .with_local_change_tx(local_tx)
        .with_dirty_tx(dirty_tx);
    (Arc::new(rooms), local_rx, dirty_rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn fires_only_on_the_replica_that_applied() {
    let db = knot_test_support::fresh_db().await;
    let doc = page(&db.pool).await;
    let (a, mut a_local, _a_dirty) = replica(&db).await;
    let (b, mut b_local, mut b_dirty) = replica(&db).await;
    let room_a = a.acquire(doc).await.unwrap();
    let _room_b = b.acquire(doc).await.unwrap();

    let (_doc, update_bytes) = knot_markdown::from_markdown::parse("hello\n").unwrap();
    let (reply, done) = tokio::sync::oneshot::channel();
    room_a
        .tx
        .send(Event::ApplyUpdate {
            update_bytes,
            by_user: None,
            source: knot_storage::UpdateSource::Edit,
            meta: None,
            reply,
        })
        .await
        .unwrap();
    done.await.unwrap().expect("apply");

    let got = tokio::time::timeout(Duration::from_secs(5), a_local.recv()).await.expect("A fired");
    assert_eq!(got, Some(doc));
    // B received the row: its dirty notifier fired …
    let got = tokio::time::timeout(Duration::from_secs(5), b_dirty.recv()).await.expect("B received the update");
    assert_eq!(got, Some(doc));
    // … but its local notifier did not.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), b_local.recv()).await.is_err(),
        "B must not report A's edit"
    );
}
```

If `knot-markdown` or `sqlx` are not dev-dependencies of `knot-crdt`, add them to `crates/knot-crdt/Cargo.toml` `[dev-dependencies]` (`knot-markdown = { path = "../knot-markdown" }`, `sqlx.workspace = true`). Check with `grep -n "knot-markdown\|sqlx\|knot-test-support" crates/knot-crdt/Cargo.toml` first.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo nextest run -p knot-crdt --all-features -E 'test(fires_only_on_the_replica_that_applied)'`
Expected: FAIL at `A fired` (timeout): the sender is stored but nothing fires.

- [ ] **Step 3: Implement**

In `crates/knot-crdt/src/room.rs`:

1. `struct RoomDeps`: add `local_change_tx: Option<mpsc::Sender<Uuid>>,` after `dirty_tx`.
2. `struct Room`: add after `dirty_tx`:
   ```rust
       /// Change event hook notifier (see [`crate::Rooms::with_local_change_tx`]).
       /// Fires only in [`Room::on_applied`]: updates this replica persisted.
       local_change_tx: Option<mpsc::Sender<Uuid>>,
   ```
3. `Room::spawn`: pass `None` for the new argument to `spawn_with_limits`.
4. `Room::spawn_with_limits`: add parameter `local_change_tx: Option<mpsc::Sender<Uuid>>` between `dirty_tx` and `limits`; put it in `RoomDeps`.
5. `Room::spawn_detached` (`#[cfg(test)]`): set `local_change_tx: None` in its `RoomDeps`.
6. `Room::start`: `local_change_tx: deps.local_change_tx,` next to `dirty_tx: deps.dirty_tx,`.
7. `on_applied`, right after the existing `dirty_tx` block:
   ```rust
           // The change event hook: only this replica's own persists, so a
           // change is reported by the pod it happened on (spec D5). Not in
           // the bus path below.
           if let Some(tx) = &self.local_change_tx {
               let _ = tx.try_send(self.doc_id);
           }
   ```

In `crates/knot-crdt/src/registry.rs` `acquire`: pass `self.local_change_tx.clone(),` after `self.dirty_tx.clone(),` in the `Room::spawn_with_limits` call. Fix any other caller the compiler reports (`grep -rn "spawn_with_limits(" crates`).

- [ ] **Step 4: Run it and see it pass; run the crate**

Run: `cargo nextest run -p knot-crdt --all-features -E 'test(fires_only_on_the_replica_that_applied)'` → PASS
Run: `cargo nextest run -p knot-crdt --all-features` → all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-crdt
git commit -m "feat(crdt): local-only change notifier on rooms"
```

---

### Task 3: Coalescer

**Files:**
- Create: `crates/knot-server/src/change_events/coalesce.rs` (with unit tests)
- Modify: `crates/knot-server/src/change_events/mod.rs` (`mod coalesce;`)

**Interfaces:**
- Consumes: `Lifecycle`, `ChangeKind`, `WireMessage` (Task 1).
- Produces:
  - `pub(super) struct Pending` with `new() -> Self`, `push(&mut self, doc_id: Uuid, kind: ChangeKind, workspace_id: Option<Uuid>, now: Instant)`, `deadline(&self, window: Duration) -> Option<Instant>`, `take(&mut self) -> Vec<(Uuid, ChangeKind, Option<Uuid>)>`, `is_empty(&self) -> bool`.
  - `pub(super) async fn run(lifecycle_rx: mpsc::Receiver<Lifecycle>, content_rx: mpsc::Receiver<Uuid>, out: mpsc::Sender<Vec<WireMessage>>, connected: Arc<AtomicBool>, docs: Arc<dyn DocStore>, window: Duration, shutdown: CancellationToken)`.

- [ ] **Step 1: Write the failing unit tests**

Create `crates/knot-server/src/change_events/coalesce.rs` with only the tests first:

```rust
//! Coalesces changes per `(doc_id, kind)` over a leading-edge window: the
//! window opens at the first pending change and closes `window` later.

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::Instant;
    use uuid::Uuid;

    use crate::change_events::ChangeKind;

    #[test]
    fn dedupes_per_doc_and_kind_and_keeps_order() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Instant::now();
        let mut p = Pending::new();
        p.push(a, ChangeKind::Content, None, now);
        p.push(b, ChangeKind::Created, Some(Uuid::nil()), now);
        p.push(a, ChangeKind::Content, None, now);
        p.push(a, ChangeKind::Renamed, Some(Uuid::nil()), now);
        let kinds: Vec<_> = p.take().into_iter().map(|(d, k, _)| (d, k)).collect();
        assert_eq!(
            kinds,
            vec![(a, ChangeKind::Content), (b, ChangeKind::Created), (a, ChangeKind::Renamed)]
        );
        assert!(p.is_empty());
    }

    #[test]
    fn window_opens_at_first_change() {
        let t0 = Instant::now();
        let w = Duration::from_millis(100);
        let mut p = Pending::new();
        assert_eq!(p.deadline(w), None);
        p.push(Uuid::new_v4(), ChangeKind::Content, None, t0);
        p.push(Uuid::new_v4(), ChangeKind::Content, None, t0 + Duration::from_millis(80));
        assert_eq!(p.deadline(w), Some(t0 + w));
        p.take();
        assert_eq!(p.deadline(w), None, "take closes the window");
    }

    #[test]
    fn a_known_workspace_wins_over_none() {
        let a = Uuid::new_v4();
        let ws = Uuid::new_v4();
        let mut p = Pending::new();
        p.push(a, ChangeKind::Content, None, Instant::now());
        p.push(a, ChangeKind::Content, Some(ws), Instant::now());
        assert_eq!(p.take(), vec![(a, ChangeKind::Content, Some(ws))]);
    }
}
```

Add `mod coalesce;` to `change_events/mod.rs`.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo nextest run -p knot-server --all-features -E 'test(coalesce::)'`
Expected: compile error, `Pending` not found.

- [ ] **Step 3: Implement**

Put above the tests module in `coalesce.rs`:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use knot_storage::DocStore;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{ChangeKind, Lifecycle, WireMessage};

/// The changes of the open window, in arrival order, one per (doc, kind).
pub(super) struct Pending {
    opened_at: Option<Instant>,
    order: Vec<(Uuid, ChangeKind)>,
    workspace: HashMap<(Uuid, ChangeKind), Option<Uuid>>,
}

impl Pending {
    pub(super) fn new() -> Self {
        Self { opened_at: None, order: Vec::new(), workspace: HashMap::new() }
    }

    pub(super) fn push(&mut self, doc_id: Uuid, kind: ChangeKind, workspace_id: Option<Uuid>, now: Instant) {
        self.opened_at.get_or_insert(now);
        match self.workspace.get_mut(&(doc_id, kind)) {
            Some(known) => {
                if known.is_none() {
                    *known = workspace_id;
                }
            }
            None => {
                self.order.push((doc_id, kind));
                self.workspace.insert((doc_id, kind), workspace_id);
            }
        }
    }

    pub(super) fn deadline(&self, window: Duration) -> Option<Instant> {
        self.opened_at.map(|t| t + window)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub(super) fn take(&mut self) -> Vec<(Uuid, ChangeKind, Option<Uuid>)> {
        self.opened_at = None;
        let workspace = std::mem::take(&mut self.workspace);
        std::mem::take(&mut self.order)
            .into_iter()
            .map(|key| (key.0, key.1, workspace.get(&key).copied().flatten()))
            .collect()
    }
}

fn dropped(reason: &'static str, n: usize) {
    if n > 0 {
        metrics::counter!("knot_change_event_hook_events_dropped_total", "reason" => reason)
            .increment(n as u64);
    }
}

/// Close the window: resolve workspaces of content changes (a page gone
/// by now, e.g. purged, is skipped) and hand the batch to the connection.
async fn flush(
    batch: Vec<(Uuid, ChangeKind, Option<Uuid>)>,
    out: &mpsc::Sender<Vec<WireMessage>>,
    connected: &AtomicBool,
    docs: &dyn DocStore,
) {
    if !connected.load(Ordering::Acquire) {
        dropped("disconnected", batch.len());
        return;
    }
    let at = chrono::Utc::now();
    let mut messages = Vec::with_capacity(batch.len());
    for (doc_id, kind, workspace_id) in batch {
        let workspace_id = match workspace_id {
            Some(ws) => ws,
            None => match docs.get(doc_id).await {
                Ok(Some(doc)) => doc.workspace_id,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = ?e, %doc_id, "change event hook: workspace lookup failed");
                    dropped("send_failed", 1);
                    continue;
                }
            },
        };
        messages.push(WireMessage::PageChanged { doc_id, workspace_id, kind, at });
    }
    if messages.is_empty() {
        return;
    }
    let n = messages.len();
    match out.try_send(messages) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => dropped("queue_full", n),
        Err(mpsc::error::TrySendError::Closed(_)) => dropped("disconnected", n),
    }
}

pub(super) async fn run(
    mut lifecycle_rx: mpsc::Receiver<Lifecycle>,
    mut content_rx: mpsc::Receiver<Uuid>,
    out: mpsc::Sender<Vec<WireMessage>>,
    connected: Arc<AtomicBool>,
    docs: Arc<dyn DocStore>,
    window: Duration,
    shutdown: CancellationToken,
) {
    let mut pending = Pending::new();
    loop {
        let deadline = pending.deadline(window);
        tokio::select! {
            _ = shutdown.cancelled() => return,
            Some(c) = lifecycle_rx.recv() => {
                pending.push(c.doc_id, c.kind, Some(c.workspace_id), Instant::now());
            }
            Some(doc_id) = content_rx.recv() => {
                pending.push(doc_id, ChangeKind::Content, None, Instant::now());
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                flush(pending.take(), &out, &connected, docs.as_ref()).await;
            }
        }
    }
}
```

- [ ] **Step 4: Run the unit tests**

Run: `cargo nextest run -p knot-server --all-features -E 'test(coalesce::)'`
Expected: 3 PASS. (`run` is unused until Task 4; add `#[allow(dead_code)]` on `run` and `flush` for now if clippy/`-D warnings` complains, and remove it in Task 4.)

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server/src/change_events
git commit -m "feat: change event coalescer"
```

---

### Task 4: Connection task, spawn wiring → reconnect test green

**Files:**
- Create: `crates/knot-server/src/change_events/conn.rs` (with `Backoff` unit tests)
- Modify: `crates/knot-server/src/change_events/mod.rs` (`mod conn;`, real `spawn`)
- Modify: `crates/knot-server/Cargo.toml`
- Test: `crates/knot-server/tests/change_event_hook_integration.rs` (add once-per-change + Review Focus 1–4 tests)

**Interfaces:**
- Consumes: `coalesce::run` (Task 3), `Hook` fields (Task 1), `Rooms::with_local_change_tx` firing (Task 2).
- Produces: `pub(super) struct Backoff { new(min, max), next(&mut self) -> Duration, reset(&mut self) }`; `pub(super) struct ConnConfig`; `pub(super) async fn run(cfg: ConnConfig, batches: mpsc::Receiver<Vec<WireMessage>>, connected: Arc<AtomicBool>, shutdown: CancellationToken)`.

- [ ] **Step 1: Dependencies**

In `crates/knot-server/Cargo.toml` `[dependencies]` add:

```toml
# Change event hook: knot is a WebSocket client of the receiver.
tokio-tungstenite = { version = "0.30", features = ["rustls-tls-webpki-roots"] }
futures-util = "0.3"
rustls.workspace = true
webpki-roots = "1"
```

Keep the `[dev-dependencies]` `tokio-tungstenite = "0.30"` line (features unify). Run `cargo deny check` (repo has `deny.toml`) and `cargo build -p knot-server`.

- [ ] **Step 2: Write the failing `Backoff` unit tests**

Create `crates/knot-server/src/change_events/conn.rs` with:

```rust
//! The connection to the receiver: connect, hello, send batches, keepalive,
//! reconnect with backoff.

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn backoff_doubles_within_jitter_bounds_and_resets() {
        let (min, max) = (Duration::from_millis(100), Duration::from_millis(800));
        let mut b = Backoff::new(min, max);
        for cur in [100u64, 200, 400, 800, 800, 800] {
            let d = b.next();
            let cur = Duration::from_millis(cur);
            assert!(d >= cur / 2 && d <= cur, "{d:?} not in [{:?}, {cur:?}]", cur / 2);
        }
        b.reset();
        assert!(b.next() <= min);
    }
}
```

Add `mod conn;` to `mod.rs`. Run: `cargo nextest run -p knot-server --all-features -E 'test(conn::)'` → compile error (`Backoff` missing).

- [ ] **Step 3: Implement `conn.rs`**

Above the tests module:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::RngExt as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::WireMessage;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Exponential backoff; each delay is drawn from `[cur/2, cur]`.
pub(super) struct Backoff {
    min: Duration,
    max: Duration,
    cur: Duration,
}

impl Backoff {
    pub(super) fn new(min: Duration, max: Duration) -> Self {
        Self { min, max, cur: min }
    }
    pub(super) fn next(&mut self) -> Duration {
        let cur = self.cur;
        self.cur = (self.cur * 2).min(self.max);
        let half = cur / 2;
        let span = (cur - half).as_millis() as u64;
        half + Duration::from_millis(rand::rng().random_range(0..=span))
    }
    pub(super) fn reset(&mut self) {
        self.cur = self.min;
    }
}

pub(super) struct ConnConfig {
    pub url: String,
    pub auth: Option<HeaderValue>,
    pub instance_id: Uuid,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub ping_interval: Duration,
    pub connect_timeout: Duration,
    pub send_timeout: Duration,
    pub tls: Arc<rustls::ClientConfig>,
}

/// rustls with the ring provider and the webpki roots, built explicitly so
/// it does not depend on a process-wide default provider.
pub(super) fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(cfg)
}

enum End {
    Dropped,
    Shutdown,
}

fn set_connected(flag: &AtomicBool, up: bool) {
    flag.store(up, Ordering::Release);
    metrics::gauge!("knot_change_event_hook_connected").set(if up { 1.0 } else { 0.0 });
}

async fn connect(cfg: &ConnConfig) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut request = cfg.url.as_str().into_client_request()?;
    if let Some(auth) = &cfg.auth {
        request.headers_mut().insert("authorization", auth.clone());
    }
    let (socket, _response) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        None,
        false,
        Some(Connector::Rustls(cfg.tls.clone())),
    )
    .await?;
    Ok(socket)
}

pub(super) async fn run(
    cfg: ConnConfig,
    mut batches: mpsc::Receiver<Vec<WireMessage>>,
    connected: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new(cfg.backoff_min, cfg.backoff_max);
    set_connected(&connected, false);
    loop {
        let attempt = tokio::select! {
            _ = shutdown.cancelled() => return,
            r = timeout(cfg.connect_timeout, connect(&cfg)) => r,
        };
        match attempt {
            Ok(Ok(socket)) => {
                metrics::counter!("knot_change_event_hook_connects_total", "result" => "ok").increment(1);
                tracing::info!("change event hook: connected");
                backoff.reset();
                set_connected(&connected, true);
                let end = session(socket, &cfg, &mut batches, &shutdown).await;
                set_connected(&connected, false);
                // Nothing is carried over to the next connection.
                let mut lost = 0usize;
                while let Ok(batch) = batches.try_recv() {
                    lost += batch.len();
                }
                if lost > 0 {
                    metrics::counter!("knot_change_event_hook_events_dropped_total", "reason" => "disconnected")
                        .increment(lost as u64);
                }
                if matches!(end, End::Shutdown) {
                    return;
                }
                tracing::warn!("change event hook: connection lost, reconnecting");
            }
            Ok(Err(e)) => {
                metrics::counter!("knot_change_event_hook_connects_total", "result" => "error").increment(1);
                tracing::warn!(error = %e, "change event hook: connect failed");
            }
            Err(_) => {
                metrics::counter!("knot_change_event_hook_connects_total", "result" => "error").increment(1);
                tracing::warn!("change event hook: connect timed out");
            }
        }
        let delay = backoff.next();
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

async fn send(socket: &mut Socket, msg: Message, cfg: &ConnConfig) -> bool {
    matches!(timeout(cfg.send_timeout, socket.send(msg)).await, Ok(Ok(())))
}

async fn session(
    mut socket: Socket,
    cfg: &ConnConfig,
    batches: &mut mpsc::Receiver<Vec<WireMessage>>,
    shutdown: &CancellationToken,
) -> End {
    let hello = WireMessage::Hello {
        instance_id: cfg.instance_id,
        version: env!("CARGO_PKG_VERSION"),
    };
    let text = serde_json::to_string(&hello).expect("hello serializes");
    if !send(&mut socket, Message::text(text), cfg).await {
        return End::Dropped;
    }
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + cfg.ping_interval, cfg.ping_interval);
    let mut unanswered = 0u32;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                return End::Shutdown;
            }
            batch = batches.recv() => {
                let Some(batch) = batch else { return End::Shutdown };
                let total = batch.len();
                for (i, msg) in batch.into_iter().enumerate() {
                    let text = serde_json::to_string(&msg).expect("event serializes");
                    if !send(&mut socket, Message::text(text), cfg).await {
                        metrics::counter!("knot_change_event_hook_events_dropped_total", "reason" => "send_failed")
                            .increment((total - i) as u64);
                        return End::Dropped;
                    }
                    metrics::counter!("knot_change_event_hook_events_sent_total").increment(1);
                }
            }
            _ = ping.tick() => {
                if unanswered >= 2 {
                    tracing::warn!("change event hook: receiver stopped answering pings");
                    return End::Dropped;
                }
                unanswered += 1;
                if !send(&mut socket, Message::Ping(Vec::new().into()), cfg).await {
                    return End::Dropped;
                }
            }
            frame = socket.next() => match frame {
                Some(Ok(Message::Pong(_))) => unanswered = 0,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return End::Dropped,
                Some(Ok(other)) => tracing::debug!(?other, "change event hook: ignoring frame from receiver"),
            },
        }
    }
}
```

Note: check the `rand` 0.10 API in the workspace (`grep -rn "rand::" crates/knot-auth/src | head`) and use the same calls (`rand::rng().random_range(..)` with whatever trait import the repo uses). Same for `Message::Ping` payload type (`Bytes` in tungstenite 0.27+; `Vec::new().into()` converts).

- [ ] **Step 4: Real `Hook::spawn`**

Replace the body of `Hook::spawn` in `mod.rs` (rename `_docs`/`_shutdown` to `docs`/`shutdown`):

```rust
        let instance_id = Uuid::new_v4();
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(self.queue_capacity);
        let (content_tx, content_rx) = mpsc::channel(self.queue_capacity);
        let (batch_tx, batch_rx) = mpsc::channel::<Vec<WireMessage>>(16);
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let auth = self.token.as_deref().and_then(|t| {
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {t}"))
                .map_err(|_| tracing::error!("change event hook: token is not a valid header value; sending none"))
                .ok()
        });
        let cfg = conn::ConnConfig {
            url: self.url,
            auth,
            instance_id,
            backoff_min: self.backoff_min,
            backoff_max: self.backoff_max,
            ping_interval: self.ping_interval,
            connect_timeout: self.connect_timeout,
            send_timeout: self.send_timeout,
            tls: conn::tls_config(),
        };
        tokio::spawn(coalesce::run(
            lifecycle_rx,
            content_rx,
            batch_tx,
            connected.clone(),
            docs,
            self.window,
            shutdown.clone(),
        ));
        tokio::spawn(conn::run(cfg, batch_rx, connected, shutdown));
        ChangeEvents { lifecycle_tx, content_tx, instance_id }
```

Remove any `#[allow(dead_code)]` added in Task 3.

- [ ] **Step 5: Run unit tests and the reconnect test**

Run: `cargo nextest run -p knot-server --all-features -E 'test(conn::) | test(coalesce::) | test(reconnects_after_each_drop_and_streams_page_ids)'`
Expected: all PASS. If the reconnect test still fails, debug with `RUST_LOG=knot_server=debug` and superpowers:systematic-debugging; do not loosen its assertions.

- [ ] **Step 6: Add the once-per-change test and Review Focus 1–4 tests**

Append to `change_event_hook_integration.rs`:

```rust
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
    let created = timeout(CYCLE, next_json(&mut ws)).await.expect("created event");
    assert_eq!((created["kind"].as_str(), uuid_of(&created, "doc_id")), (Some("created"), doc));

    let edits = (0..20).map(|i| {
        let (app, session) = (pod.app.clone(), pod.session.clone());
        async move { edit(&app, &session, doc, &format!("burst {i}")).await }
    });
    futures_util::future::join_all(edits).await;

    let ev = timeout(window * 2 + Duration::from_secs(1), next_json(&mut ws)).await.expect("content event");
    assert_eq!((ev["kind"].as_str(), uuid_of(&ev, "doc_id")), (Some("content"), doc), "{ev}");
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
    let _pod = pod(&db, Some(test_hook(&url).connect_timeout(Duration::from_millis(300)))).await;
    let (_silent, _) = timeout(CYCLE, listener.accept()).await.expect("first attempt").unwrap();
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
    let _pod = pod(&db, Some(test_hook(&url).ping_interval(Duration::from_millis(100)))).await;
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
async fn rejected_handshake_backs_off_and_sends_token() {
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request as HsReq, Response as HsResp};

    let db = knot_test_support::fresh_db().await;
    seed(&db.pool).await;
    let (listener, url) = receiver().await;
    let _pod = pod(&db, Some(test_hook(&url).token(Some("s3cret".into())))).await;

    let mut last: Option<tokio::time::Instant> = None;
    for attempt in 0..3 {
        let (tcp, _) = timeout(CYCLE, listener.accept()).await.expect("attempt").unwrap();
        let now = tokio::time::Instant::now();
        if let Some(prev) = last {
            assert!(now - prev >= Duration::from_millis(20), "attempt {attempt} came after {:?}", now - prev);
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
    let frame = timeout(Duration::from_secs(5), ws.next()).await.expect("close in time");
    assert!(matches!(frame, Some(Ok(Message::Close(_)))), "got {frame:?}");
    assert!(
        timeout(Duration::from_secs(1), listener.accept()).await.is_err(),
        "no reconnect after shutdown"
    );
}
```

`tokio_tungstenite::tungstenite::http::StatusCode` and `axum::http::StatusCode` are the same `http` 1.x type; if the compiler disagrees, use `tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED`.

- [ ] **Step 7: Run the file**

Run: `cargo nextest run -p knot-server --all-features -E 'binary(change_event_hook_integration)'`
Expected: 6 PASS. Then `cargo clippy -p knot-server --all-features --tests -- -D warnings`.

- [ ] **Step 8: Commit**

```bash
git add Cargo.lock crates/knot-server
git commit -m "feat: change event hook connection with keepalive and reconnect"
```

---

### Task 5: Lifecycle events from the HTTP handlers

**Files:**
- Modify: `crates/knot-server/src/routes/api/docs.rs` (`create`, `rename`, `move_doc`, `duplicate`, `move_to_subpage`, `archive`, `restore`, `create_from_template_inline`)
- Modify: `crates/knot-server/src/archive.rs` (`purge_doc`, covers manual and retention purge)
- Modify: `crates/knot-server/src/routes/api/export_import.rs` (`import`)
- Test: `crates/knot-server/tests/change_event_hook_integration.rs`

**Interfaces:**
- Consumes: `AppState::emit_change`, `AppState::emit_changes`, `ChangeKind` (Task 1).
- Produces: nothing new.

- [ ] **Step 1: Write the failing tests**

Append:

```rust
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

    let (st, body) = send(app, s, "POST", &format!("/api/docs/{parent}/duplicate"), Some(json!({"include_subpages": true})), None).await;
    assert_eq!(st, StatusCode::CREATED, "{body:?}");
    let top: Uuid = serde_json::from_slice::<Value>(&body).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let copies = pod.state.docs.as_ref().unwrap().subtree_ids(top).await.unwrap();
    assert_eq!(copies.len(), 2, "copy of parent and child");
    expect_kinds(&mut ws, "created", &copies).await;

    let (st, _) = send(app, s, "DELETE", &format!("/api/docs/{parent}"), None, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    expect_kinds(&mut ws, "archived", &[parent, child]).await;

    let (st, _) = send(app, s, "POST", &format!("/api/docs/{parent}/restore"), None, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    expect_kinds(&mut ws, "restored", &[parent, child]).await;

    let (st, _) = send(app, s, "PATCH", &format!("/api/docs/{child}"), Some(json!({"title": "renamed"})), None).await;
    assert_eq!(st, StatusCode::OK);
    expect_kinds(&mut ws, "renamed", &[child]).await;

    let (st, _) = send(app, s, "POST", &format!("/api/docs/{child}/move"), Some(json!({"parent_id": null})), None).await;
    assert!(st.is_success(), "move {st}");
    expect_kinds(&mut ws, "moved", &[child]).await;

    let (st, _) = send(app, s, "DELETE", &format!("/api/docs/{parent}"), None, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    expect_kinds(&mut ws, "archived", &[parent]).await;
    let (st, _) = send(app, s, "POST", &format!("/api/docs/{parent}/purge"), None, None).await;
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
    let (st, _) = send(&pod.app, &pod.session, "DELETE", &format!("/api/docs/{doc}"), None, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = send(&pod.app, &pod.session, "POST", &format!("/api/docs/{doc}/purge"), None, None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let mut kinds = Vec::new();
    while let Ok(ev) = timeout(window * 2, next_json(&mut ws)).await {
        assert!(ev["workspace_id"].as_str().is_some(), "{ev}");
        kinds.push(ev["kind"].as_str().unwrap().to_string());
    }
    assert!(kinds.contains(&"purged".to_string()), "{kinds:?}");
    assert!(!kinds.contains(&"content".to_string()), "content of a purged page: {kinds:?}");
}
```

Note on the second test: the edit's persist may land before the purge removes the row (then `docs.get` still finds it at flush only if the purge has not run yet). With a 2 s window the purge always runs first. If `purge` turns out to keep the `documents` row (soft delete), adjust `coalesce::flush` to skip pages whose `archived_at` is set *and* that have a pending `Purged` in the same batch — do not weaken the assertion.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo nextest run -p knot-server --all-features -E 'test(lifecycle_changes_report_their_kind) | test(purge_inside_window_drops_content_event)'`
Expected: the first FAILS with `no created for {…}`. (`created` is the first expectation; nothing emits yet.) The second fails the same way.

- [ ] **Step 3: Emit from the handlers**

Use `crate::change_events::ChangeKind` (import at top of each file). Each emit goes **only on the success path**, after the store call succeeded:

- `docs.rs::create`, in `Ok(d) =>` after `ensure_creator_can_edit` succeeded, before `doc_answer`:
  `state.emit_change(ctx.workspace_id, d.id, ChangeKind::Created);`
- `docs.rs::rename`, `Ok(d) =>`: `state.emit_change(ctx.workspace_id, d.id, ChangeKind::Renamed);`
- `docs.rs::move_doc`, in its `Ok(d) =>` arm: `state.emit_change(ctx.workspace_id, d.id, ChangeKind::Moved);`
- `docs.rs::duplicate`, in `Ok(Ok(top)) =>`: list the copy's subtree and emit each:
  ```rust
  Ok(Ok(top)) => {
      if let Some(docs) = state.docs.as_ref() {
          match docs.subtree_ids(top.id).await {
              Ok(ids) => state.emit_changes(ctx.workspace_id, &ids, ChangeKind::Created),
              Err(e) => tracing::warn!(error = ?e, "change event: duplicate subtree"),
          }
      }
      doc_answer(&state, &ctx, StatusCode::CREATED, &top).await
  }
  ```
  (Restore-as-new in `routes/api/history.rs` calls `duplicate::run` too: find its success arm with `grep -n "duplicate::run" crates/knot-server/src/routes/api/history.rs` and emit `Created` for the returned `Copied.top.id` the same way.)
- `docs.rs::move_to_subpage`, on success: `state.emit_change(ctx.workspace_id, moved.doc.id, ChangeKind::Created);` (the source page's content change arrives through the room by itself).
- `docs.rs::archive`, `Ok(archived_ids) =>`: `state.emit_changes(ctx.workspace_id, &archived_ids, ChangeKind::Archived);`
- `docs.rs::restore`, `Ok(restored_ids) =>`: `state.emit_changes(ctx.workspace_id, &restored_ids, ChangeKind::Restored);`
- `docs.rs::create_from_template_inline`: on its success arm (the created page), `state.emit_change(ctx.workspace_id, <created>.id, ChangeKind::Created);`, plus its subpages if it copies any (`grep -n "Ok(" ` within the function to find the arm).
- `archive.rs::purge_doc`: at the start, after the docs store is available, read the subtree (`docs.subtree_ids(doc_id).await`) into `ids` (on error, use `vec![doc_id]`); after the purge has succeeded and just before `Ok(())`, `state.emit_changes(workspace_id, &ids, ChangeKind::Purged);`. This covers both manual purge and the retention sweep (`run_purge_once`).
- `export_import.rs::import`: after the import transaction committed (where `"imported_docs": doc_remap.len()` is reported), `let ids: Vec<Uuid> = doc_remap.values().copied().collect(); state.emit_changes(ctx.workspace_id, &ids, ChangeKind::Created);` (use the workspace id variable the function already has).

- [ ] **Step 4: Run the file**

Run: `cargo nextest run -p knot-server --all-features -E 'binary(change_event_hook_integration)'`
Expected: 8 PASS. Also run the touched handlers' suites: `cargo nextest run -p knot-server --all-features -E 'binary(docs_integration) | binary(archive_integration) | binary(duplicate_integration) | binary(workspace_import_integration) | binary(templates_integration) | binary(move_to_subpage_integration)'` → PASS (emits are no-ops there).

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server
git commit -m "feat: report page lifecycle changes to the change event hook"
```

---

### Task 6: Two pods report only their own changes

**Files:**
- Test: `crates/knot-server/tests/change_event_hook_integration.rs`

**Interfaces:**
- Consumes: `pod_with` (Task 1), local-only firing (Task 2).
- Produces: nothing.

- [ ] **Step 1: Write the test**

```rust
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
    let b = pod_with(&db, Some(test_hook(&url_b).window(window)), Some(dirty_b_tx)).await;
    let mut ws_a = accept(&listener_a).await;
    let mut ws_b = accept(&listener_b).await;
    assert_ne!(expect_hello(&mut ws_a).await, expect_hello(&mut ws_b).await, "two processes");

    let doc = create_doc(&a, "shared", None).await;
    expect_kinds(&mut ws_a, "created", &[doc]).await;
    // B has the page open, so it receives A's rows over the bus.
    let _room_b = b.state.rooms_v2.as_ref().unwrap().acquire(doc).await.unwrap();

    edit(&a.app, &a.session, doc, "from A").await;
    expect_kinds(&mut ws_a, "content", &[doc]).await;
    let got = timeout(CYCLE, dirty_b.recv()).await.expect("B's room received A's edit");
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
```

- [ ] **Step 2: Run it**

Run: `cargo nextest run -p knot-server --all-features -E 'test(each_pod_reports_only_its_own_edits)'`
Expected: PASS (behavior delivered in Tasks 2 and 4). If it fails, the bug is in the implementation (likely a notifier fired from the bus path): debug, do not change the test. To prove the test can fail, temporarily add `if let Some(tx) = &self.local_change_tx { let _ = tx.try_send(self.doc_id); }` next to the bus-path `dirty_tx` send in `room.rs` (~line 1452), see it fail with `B must not report A's edit`, then revert.

- [ ] **Step 3: Commit**

```bash
git add crates/knot-server/tests/change_event_hook_integration.rs
git commit -m "test: change events are reported only by the originating pod"
```

---

### Task 7: Configuration and server wiring

**Files:**
- Modify: `crates/knot-config/src/lib.rs` (fields, defaults, validation)
- Modify: `crates/knot-config/tests/load.rs`
- Modify: `crates/knot-server/src/change_events/mod.rs` (`Hook::from_config` + unit test)
- Modify: `crates/knot-server/src/main.rs`
- Modify: `crates/knot-obs/src/metrics.rs` (describes)

**Interfaces:**
- Consumes: `Hook`, `ChangeEvents` (Task 1), `Rooms::with_local_change_tx` (Task 2).
- Produces: `Config.change_event_hook_url: String`, `Config.change_event_hook_token: String`; `Hook::from_config(cfg: &knot_config::Config) -> Option<Hook>`.

- [ ] **Step 1: Write the failing config tests**

Append to `crates/knot-config/tests/load.rs`:

```rust
#[test]
fn change_event_hook_is_off_by_default() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_SESSION_KEY", "0123456789abcdef0123456789abcdef");
        let cfg = Config::load::<&str>(None).expect("load");
        assert!(cfg.change_event_hook_url.is_empty());
        assert!(cfg.change_event_hook_token.is_empty());
        Ok(())
    });
}

#[test]
fn change_event_hook_from_env() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_SESSION_KEY", "0123456789abcdef0123456789abcdef");
        jail.set_env("KNOT_CHANGE_EVENT_HOOK_URL", "wss://hooks.example/knot");
        jail.set_env("KNOT_CHANGE_EVENT_HOOK_TOKEN", "s3cret");
        let cfg = Config::load::<&str>(None).expect("load");
        assert_eq!(cfg.change_event_hook_url, "wss://hooks.example/knot");
        assert_eq!(cfg.change_event_hook_token, "s3cret");
        Ok(())
    });
}

#[test]
fn change_event_hook_rejects_bad_urls() {
    for bad in ["http://hooks.example", "not a url", "ws//missing-colon"] {
        figment::Jail::expect_with(|jail| {
            jail.set_env("KNOT_SESSION_KEY", "0123456789abcdef0123456789abcdef");
            jail.set_env("KNOT_CHANGE_EVENT_HOOK_URL", bad);
            let err = Config::load::<&str>(None).expect_err(bad);
            assert!(err.to_string().contains("KNOT_CHANGE_EVENT_HOOK_URL"), "{bad}: {err}");
            Ok(())
        });
    }
}

#[test]
fn change_event_hook_rejects_a_token_that_is_no_header_value() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_SESSION_KEY", "0123456789abcdef0123456789abcdef");
        jail.set_env("KNOT_CHANGE_EVENT_HOOK_URL", "ws://localhost:9000");
        jail.set_env("KNOT_CHANGE_EVENT_HOOK_TOKEN", "line\nbreak");
        let err = Config::load::<&str>(None).expect_err("token");
        assert!(err.to_string().contains("KNOT_CHANGE_EVENT_HOOK_TOKEN"), "{err}");
        Ok(())
    });
}
```

Run: `cargo nextest run -p knot-config --all-features -E 'test(change_event_hook)'` → compile error (fields missing).

- [ ] **Step 2: Implement the config**

In `crates/knot-config/src/lib.rs`, add to `struct Config` (after the quota fields):

```rust
    /// Change event hook receiver (`ws://` or `wss://`). Empty = off.
    pub change_event_hook_url: String,
    /// Optional bearer token sent on the hook's WebSocket handshake.
    pub change_event_hook_token: String,
```

Defaults in `impl Default for Config`: `change_event_hook_url: String::new(), change_event_hook_token: String::new(),`. In `fn validate`, before the final `Ok(())`:

```rust
        if !self.change_event_hook_url.is_empty() {
            match url::Url::parse(&self.change_event_hook_url) {
                Ok(u) if matches!(u.scheme(), "ws" | "wss") && u.host().is_some() => {}
                _ => {
                    return Err(ConfigError::Invalid(
                        "KNOT_CHANGE_EVENT_HOOK_URL must be a ws:// or wss:// URL".into(),
                    ));
                }
            }
        }
        if self
            .change_event_hook_token
            .bytes()
            .any(|b| !(0x20..0x7f).contains(&b))
        {
            return Err(ConfigError::Invalid(
                "KNOT_CHANGE_EVENT_HOOK_TOKEN must be printable ASCII".into(),
            ));
        }
```

If `knot-config` has no `url` dependency, add `url.workspace = true` to `crates/knot-config/Cargo.toml`. Also extend the existing in-crate defaults test (around `lib.rs:640`, the one asserting `defaults.rate_limit_enabled`) with `assert!(defaults.change_event_hook_url.is_empty());`.

Run: `cargo nextest run -p knot-config --all-features` → all PASS.

- [ ] **Step 3: `Hook::from_config` with a unit test**

In `change_events/mod.rs`:

```rust
impl Hook {
    /// The hook as configured; `None` (feature off) without a URL.
    pub fn from_config(cfg: &knot_config::Config) -> Option<Self> {
        if cfg.change_event_hook_url.is_empty() {
            return None;
        }
        Some(Self::new(cfg.change_event_hook_url.clone()).token(Some(cfg.change_event_hook_token.clone())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_without_url() {
        assert!(Hook::from_config(&knot_config::Config::default()).is_none());
    }

    #[test]
    fn on_with_url_and_empty_token_is_no_token() {
        let cfg = knot_config::Config {
            change_event_hook_url: "ws://localhost:9000".into(),
            ..Default::default()
        };
        let hook = Hook::from_config(&cfg).expect("on");
        assert_eq!(hook.url, "ws://localhost:9000");
        assert_eq!(hook.token, None);
    }

    #[test]
    fn wire_format_matches_the_spec() {
        let id = Uuid::nil();
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-22T10:00:00Z").unwrap().with_timezone(&Utc);
        let v = serde_json::to_value(WireMessage::PageChanged { doc_id: id, workspace_id: id, kind: ChangeKind::Purged, at }).unwrap();
        assert_eq!(v["type"], "page_changed");
        assert_eq!(v["kind"], "purged");
        assert_eq!(v["at"], "2026-11-22T10:00:00Z");
        let v = serde_json::to_value(WireMessage::Hello { instance_id: id, version: "1.2.3" }).unwrap();
        assert_eq!(v, serde_json::json!({"type": "hello", "instance_id": id, "version": "1.2.3"}));
    }
}
```

Run: `cargo nextest run -p knot-server --all-features -E 'test(change_events::tests)'` → PASS.

- [ ] **Step 4: Wire `main.rs`**

In `run_server`, where `Rooms::new(...)…with_dirty_tx(dirty_tx)` is built: the hook needs the doc store and the shutdown token, which exist on `AppState`. Spawn the hook **before** building `Rooms` and keep the `ChangeEvents`:

```rust
                let change_events = knot_server::change_events::Hook::from_config(&cfg).map(|hook| {
                    tracing::info!("change event hook enabled");
                    hook.spawn(
                        std::sync::Arc::new(knot_storage::PgDocStore::new(pool.clone())),
                        shutdown.clone(),
                    )
                });
                let mut rooms = knot_crdt::Rooms::new(/* unchanged args */).with_dirty_tx(dirty_tx);
                if let Some(events) = &change_events {
                    rooms = rooms.with_local_change_tx(events.content_sender());
                }
                let rooms = std::sync::Arc::new(rooms);
```

Use the doc store type `AppState::with_pool` uses (`grep -n "DocStore> = Arc::new" crates/knot-server/src/lib.rs`) and the shutdown token the server later puts into `AppState.shutdown` (`grep -n "shutdown" crates/knot-server/src/main.rs`; if the token is created after this block, move its creation up — the comment near line 170 says the shutdown token exists later, so create it first and reuse it). Return `change_events` from that block alongside `rooms` and set `state.change_events = change_events;` where the other `state.*` fields are set. Never log the token or the URL's userinfo.

Run: `cargo build -p knot-server` and start locally with `KNOT_CHANGE_EVENT_HOOK_URL=ws://127.0.0.1:9 cargo run -p knot-server` → log shows `change event hook enabled` and repeated `connect failed` warnings with growing gaps; Ctrl-C exits promptly. With `KNOT_CHANGE_EVENT_HOOK_URL=http://x` it exits with code 2 and `config: … KNOT_CHANGE_EVENT_HOOK_URL …`.

- [ ] **Step 5: Metric describes**

In `crates/knot-obs/src/metrics.rs`, next to the existing `describe_counter!`/`describe_gauge!` calls:

```rust
    describe_gauge!("knot_change_event_hook_connected", "1 while the change event hook is connected to its receiver");
    describe_counter!("knot_change_event_hook_events_sent_total", "Change events sent to the receiver");
    describe_counter!("knot_change_event_hook_events_dropped_total", "Change events dropped (reason: queue_full, disconnected, send_failed)");
    describe_counter!("knot_change_event_hook_connects_total", "Change event hook connection attempts (result: ok, error)");
```

- [ ] **Step 6: Full Rust suite and commit**

Run: `make test.rust` and `cargo clippy --workspace --all-features --tests -- -D warnings` → PASS.

```bash
git add crates/knot-config crates/knot-server crates/knot-obs Cargo.lock
git commit -m "feat: configure the change event hook with KNOT_CHANGE_EVENT_HOOK_URL"
```

---

### Task 8: Receiver documentation, Helm, changelog

**Files:**
- Create: `docs/change-event-hook.md`
- Modify: `README.md`, `deploy/helm/knot/values.yaml`, `deploy/helm/knot/values.schema.json`, `deploy/helm/knot/templates/configmap.yaml`, `deploy/helm/knot/templates/deployment.yaml`, `deploy/helm/knot/README.md`, `CHANGELOG.md`

**Interfaces:** none.

- [ ] **Step 1: Write `docs/change-event-hook.md`**

Audience: engineers building a receiver, who don't know knot internals. Plain, concise prose (apply elements-of-style:writing-clearly-and-concisely). Sections and the facts each must state:

1. **What it is** — knot opens a WebSocket connection *to you* and streams which pages changed. Configure `KNOT_CHANGE_EVENT_HOOK_URL` (`ws://` or `wss://`) and optionally `KNOT_CHANGE_EVENT_HOOK_TOKEN`; unset URL = off.
2. **Handshake** — knot sends `Authorization: Bearer <token>` when a token is configured; reject with 401/403 to refuse; knot retries with backoff (0.5 s doubling to 30 s, jittered).
3. **Messages** — hello and page_changed JSON with a field table; `kind` values and what triggers each (content: any edit of the page body, including markdown import and history restore; created: new page, duplicate, template, import, move-to-subpage; renamed: title or icon; moved: reparent/reorder; archived/restored: one event per page of the subtree; purged: permanently deleted, manually or by retention). Unknown `type` or `kind` values must be ignored (forward compatibility).
4. **Delivery guarantees** — at most once; coalesced per page and kind over ~1 s windows; events of one window share `at`; nothing is replayed after a disconnect, a pod restart, or a full queue; no ordering across connections. Therefore: treat events as hints and fetch current state through the API (`GET /api/docs/{id}`, `GET /api/docs/{id}/markdown`) with a scoped API key; after a reconnect or a new `instance_id`, reconcile (e.g. full listing) if you need completeness. A `content` event may arrive for a page you cannot read with your key, and for restricted pages.
5. **Many connections** — one connection per knot pod; the number changes with scaling, rolling deploys and restarts; every change arrives on exactly one connection (the pod where it happened); expect N concurrent connections where N is the maximum replica count during a rolling update (replicas + surge); identify pods by `instance_id` from hello, which changes on every process restart, not on reconnect; a closed connection is normal (deploys) — do not alert on single disconnects; load-balance across receiver instances freely, since no state is tied to one connection. The receiver is responsible for this complexity; knot does no coordination.
6. **Keepalive** — knot pings every 30 s and drops the connection after 2 unanswered pings; WebSocket libraries answer pings automatically while you read; knot ignores anything you send except Pong and Close.
7. **Operating** — the four metrics with labels; the log lines (`change event hook: connected` / `connect failed` / `connection lost, reconnecting`).
8. **Minimal receiver example** — a ~20-line Python `websockets` server that prints events (asyncio, `websockets.serve`, checks the Authorization header).

- [ ] **Step 2: Link it and wire Helm**

- `README.md`: in the Deploy section add one line: "To stream page changes to another system, see [docs/change-event-hook.md](docs/change-event-hook.md)."
- `deploy/helm/knot/values.yaml`: add
  ```yaml
  # Change event hook: knot connects to this WebSocket receiver and streams
  # changed page ids (docs/change-event-hook.md). Empty = off.
  changeEventHook:
    url: ""
    # Optional bearer token, read from an existing Secret.
    tokenSecretName: ""
    tokenSecretKey: KNOT_CHANGE_EVENT_HOOK_TOKEN
  ```
- `values.schema.json`: add `changeEventHook` object with `url` (string, pattern `^(wss?://.+)?$`), `tokenSecretName` (string), `tokenSecretKey` (string), following the style of neighbouring entries.
- `templates/configmap.yaml`, next to `KNOT_TRUSTED_PROXIES`:
  ```yaml
  {{- with .Values.changeEventHook.url }}
  KNOT_CHANGE_EVENT_HOOK_URL: {{ . | quote }}
  {{- end }}
  ```
- `templates/deployment.yaml`, in `env:` after the OIDC secret block:
  ```yaml
  {{- if .Values.changeEventHook.tokenSecretName }}
  - name: KNOT_CHANGE_EVENT_HOOK_TOKEN
    valueFrom:
      secretKeyRef:
        name: {{ .Values.changeEventHook.tokenSecretName }}
        key: {{ .Values.changeEventHook.tokenSecretKey }}
  {{- end }}
  ```
- `deploy/helm/knot/README.md`: add `changeEventHook` to the values listing with a one-line pointer to `docs/change-event-hook.md`, noting that the receiver must accept one connection per replica (plus surge during rollouts).

Run: `helm lint deploy/helm/knot` and `helm template deploy/helm/knot --set changeEventHook.url=wss://h.example --set changeEventHook.tokenSecretName=hook --set session.key=x --set database.url=postgres://x | grep -A4 CHANGE_EVENT` → both variables rendered. (Use whatever required values `ct-values.yaml` sets if `helm template` demands more.)

- [ ] **Step 3: CHANGELOG**

Under `## [Unreleased]` → `### Added`:

```markdown
- Change event hook: set `KNOT_CHANGE_EVENT_HOOK_URL` to a `ws://` or
  `wss://` receiver and every pod keeps a WebSocket connection to it,
  sending one JSON event per changed page (content, created, renamed,
  moved, archived, restored, purged) about once a second, and reconnecting
  when the connection drops. Delivery is at most once; read
  docs/change-event-hook.md before building a receiver. (#TBD)
```

- [ ] **Step 4: Commit**

```bash
git add docs/change-event-hook.md README.md deploy/helm/knot CHANGELOG.md
git commit -m "docs: change event hook receiver guide, Helm values, changelog"
```

---

## Self-review notes (for the executor)

- Window dedupe is per `(doc_id, kind)`, so a page created and edited in the same window yields two events with the same `at` (`created` and `content`). The reconnect test's duplicate check keys on `(at, doc_id, kind)` for that reason.
- Task 1 leaves one red test on the branch until Task 4. Do not push the branch to CI before Task 4 is done, or mark the test `#[ignore]` only on a pushed intermediate commit and remove the attribute in Task 4.
