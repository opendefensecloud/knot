//! Doc contributors: who gets credited for changing a page's content, and
//! `GET /api/docs/{id}/contributors`.
//!
//! A contributor is anyone whose action changed the page CONTENT — live
//! typing over the collab socket, markdown import, history restore,
//! workspace import, … Every one of those is persisted by the room's writer,
//! which records the author per `doc_updates` row and upserts
//! `doc_contributors` in the same statement.
//!
//! The live-editing tests drive the real collab socket with a raw y-sync
//! client that behaves like the web provider (see `offline_sync.rs` for the
//! full version and the rationale behind each step).

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::protocol::{YSyncMessage, decode};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole, sort_key_between,
};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, ReadTxn, StateVector, Text, Transact, TransactionMut, Update, XmlElementPrelim,
    XmlFragment, XmlFragmentRef, XmlTextPrelim, XmlTextRef,
};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Generous: a loaded CI runner can take a while to hydrate a room and run
/// the writer's 250 ms batch. Every wait polls toward this deadline and
/// returns as soon as its condition holds — no fixed sleeps.
const DEADLINE: Duration = Duration::from_secs(10);
const PASSWORD: &str = "hunter22";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Session {
    /// `sid=…; csrf=…`
    cookie: String,
    csrf: String,
}

struct Member {
    id: Uuid,
    session: Session,
}

struct Fixture {
    addr: std::net::SocketAddr,
    app: axum::Router,
    state: AppState,
    pool: knot_storage::Pool,
    ws_id: Uuid,
    doc_id: Uuid,
    doc_created_at: DateTime<Utc>,
    /// Owner; created the doc.
    alice: Member,
    /// Editor.
    bob: Member,
    /// Editor.
    carol: Member,
    /// Viewer.
    vic: Member,
}

/// Postgres-backed state with doc rooms on a MemBus, one doc created by
/// Alice, and four logged-in members, served on an ephemeral port.
async fn fixture() -> Fixture {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s.cookie_secure = false;

    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    s.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        Arc::new(knot_crdt::YrsEngine),
        Arc::new(knot_crdt::MemBus::new()),
        updates,
        snapshots,
        knot_crdt::SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(600),
        },
        Duration::from_secs(300),
    )));

    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (email, name, role) in [
        ("alice@contrib.test", "Alice", WorkspaceRole::Owner),
        ("bob@contrib.test", "Bob", WorkspaceRole::Editor),
        ("carol@contrib.test", "Carol", WorkspaceRole::Editor),
        ("vic@contrib.test", "Vic", WorkspaceRole::Viewer),
    ] {
        let hash = s.hasher.hash(PASSWORD).unwrap();
        let u = s
            .users
            .as_ref()
            .unwrap()
            .create_local(email, name, &hash)
            .await
            .unwrap();
        s.workspaces
            .as_ref()
            .unwrap()
            .add_member(ws.id, u.id, role)
            .await
            .unwrap();
        ids.push((u.id, email));
    }
    let doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(
            ws.id,
            None,
            "Credits",
            &sort_key_between(None, None),
            ids[0].0,
        )
        .await
        .unwrap();

    let app = router_with_state(s.clone());
    let mut members = Vec::new();
    for (id, email) in ids {
        members.push(Member {
            id,
            session: login(&app, email).await,
        });
    }
    let [alice, bob, carol, vic]: [Member; 4] = members.try_into().ok().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });

    Fixture {
        addr,
        app,
        state: s,
        pool,
        ws_id: ws.id,
        doc_id: doc.id,
        doc_created_at: doc.created_at,
        alice,
        bob,
        carol,
        vic,
    }
}

async fn login(app: &axum::Router, email: &str) -> Session {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": PASSWORD}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login {email} failed");
    let kvs: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .filter(|kv| kv.starts_with("sid=") || kv.starts_with("csrf="))
        .collect();
    let csrf = kvs
        .iter()
        .find_map(|kv| kv.strip_prefix("csrf="))
        .expect("login set no csrf cookie")
        .to_string();
    Session {
        cookie: kvs.join("; "),
        csrf,
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

async fn get_contributors(
    app: &axum::Router,
    session: Option<&Session>,
    doc_id: Uuid,
) -> (StatusCode, Option<String>, String) {
    let mut req = Request::builder()
        .method("GET")
        .uri(format!("/api/docs/{doc_id}/contributors"));
    if let Some(s) = session {
        req = req.header("cookie", &s.cookie);
    }
    let r = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = r.status();
    let content_type = r
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

async fn post(
    app: &axum::Router,
    session: &Session,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("cookie", &session.cookie)
                .header("x-csrf-token", &session.csrf)
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// The contributors' user ids, in the order the endpoint lists them.
fn listed(byline: &serde_json::Value) -> Vec<Uuid> {
    byline["contributors"]
        .as_array()
        .expect("contributors array")
        .iter()
        .map(|c| c["user_id"].as_str().unwrap().parse().unwrap())
        .collect()
}

/// Poll the endpoint (as Alice) until `done` holds for the byline, or panic.
async fn wait_for_byline(
    fx: &Fixture,
    doc_id: Uuid,
    what: &str,
    done: impl Fn(&[Uuid]) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let (status, _, body) = get_contributors(&fx.app, Some(&fx.alice.session), doc_id).await;
        assert_eq!(status, StatusCode::OK, "contributors: {body}");
        let byline: serde_json::Value = serde_json::from_str(&body).unwrap();
        if done(&listed(&byline)) {
            return byline;
        }
        if Instant::now() >= deadline {
            panic!("{what}: contributors never matched; last byline {byline}");
        }
        // Storage is polled, not pushed: back off briefly between requests.
        // Bounded by DEADLINE above.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------

/// Every `doc_updates` row for the doc, in seq order.
async fn update_rows(pool: &knot_storage::Pool, doc_id: Uuid) -> Vec<knot_storage::DocUpdate> {
    PgUpdatesStore::new(pool.clone())
        .since(doc_id, 0)
        .await
        .unwrap()
}

fn doc_text(doc: &Doc) -> String {
    let frag = doc.get_or_insert_xml_fragment("default");
    frag.get_string(&doc.transact())
}

/// Poll storage until a cold hydration of the doc contains `needle`. The
/// writer persists in arrival order, so once a sentinel edit is durable,
/// anything a connection sent before it has been persisted (or dropped).
async fn wait_for_durable_text(pool: &knot_storage::Pool, doc_id: Uuid, needle: &str) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let doc = Doc::new();
        let mut after = 0;
        if let Some(snap) = PgSnapshotStore::new(pool.clone())
            .latest(doc_id)
            .await
            .unwrap()
        {
            let u = Update::decode_v1(&snap.state_bytes).unwrap();
            doc.transact_mut().apply_update(u).unwrap();
            after = snap.snapshot_seq;
        }
        for r in PgUpdatesStore::new(pool.clone())
            .since(doc_id, after)
            .await
            .unwrap()
        {
            let u = Update::decode_v1(&r.update_bytes).unwrap();
            doc.transact_mut().apply_update(u).unwrap();
        }
        let text = doc_text(&doc);
        if text.contains(needle) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("{needle:?} never became durable; storage holds {text:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Raw y-sync client (slimmed-down `offline_sync::Peer`)
// ---------------------------------------------------------------------------

fn sync_frame(subtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8, subtype];
    knot_server::protocol::append_var_uint(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out
}

struct Peer {
    doc: Doc,
    ws: Option<Ws>,
    /// Server SyncStep1 frames answered with a SyncStep2.
    step1_answered: usize,
    /// Server SyncStep2 frames applied.
    step2_received: usize,
}

impl Peer {
    fn new() -> Self {
        Self {
            doc: Doc::new(),
            ws: None,
            step1_answered: 0,
            step2_received: 0,
        }
    }

    /// Dial as `who`, send SyncStep1, and complete the handshake: apply the
    /// server's state and answer its SyncStep1 exactly like the web provider
    /// (for a writer that answer reaches the room; it carries nothing new).
    async fn join(fx: &Fixture, who: &Member) -> Self {
        let mut p = Self::new();
        let mut req = format!("ws://{}/collab/doc/{}", fx.addr, fx.doc_id)
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "cookie",
            tungstenite::http::HeaderValue::from_str(&who.session.cookie).unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("ws connect");
        let sv = p.doc.transact().state_vector().encode_v1();
        ws.send(tungstenite::Message::Binary(sync_frame(0, &sv).into()))
            .await
            .unwrap();
        p.ws = Some(ws);
        assert!(
            p.pump_until(|p| p.step2_received >= 1 && p.step1_answered >= 1)
                .await,
            "handshake never completed"
        );
        p
    }

    /// Round-trip barrier: the server answers SyncStep1 from the same read
    /// loop that forwards our updates to the room actor, and the actor
    /// handles both in order, so once that SyncStep2 is back every update we
    /// sent before it has been applied (or dropped, for a viewer).
    async fn settle(&mut self) {
        let sv = self.doc.transact().state_vector().encode_v1();
        self.ws
            .as_mut()
            .unwrap()
            .send(tungstenite::Message::Binary(sync_frame(0, &sv).into()))
            .await
            .unwrap();
        let seen = self.step2_received;
        assert!(
            self.pump_until(|p| p.step2_received > seen).await,
            "server never answered the settle SyncStep1"
        );
    }

    /// Settle, then drop the connection.
    async fn leave(mut self) {
        self.settle().await;
        self.ws.take();
    }

    /// A local edit, sent as SYNC_UPDATE frames like `handleDocUpdate`.
    async fn edit(&mut self, f: impl FnOnce(&XmlFragmentRef, &mut TransactionMut)) {
        // Root ref BEFORE the transaction: `get_or_insert_*` takes its own
        // write lock and would deadlock against an open `TransactionMut`.
        let frag = self.doc.get_or_insert_xml_fragment("default");
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = captured.clone();
        let sub = self
            .doc
            .observe_update_v1(move |_, e| sink.lock().unwrap().push(e.update.clone()));
        {
            let mut txn = self.doc.transact_mut();
            f(&frag, &mut txn);
        }
        drop(sub);
        let updates = std::mem::take(&mut *captured.lock().unwrap());
        let ws = self.ws.as_mut().unwrap();
        for u in updates {
            ws.send(tungstenite::Message::Binary(sync_frame(2, &u).into()))
                .await
                .unwrap();
        }
    }

    /// Append a paragraph holding `s`.
    async fn type_paragraph(&mut self, s: &str) {
        let s = s.to_string();
        self.edit(move |frag, txn| {
            paragraph_text(frag, txn, &s);
        })
        .await;
    }

    async fn handle(&mut self, bytes: &[u8]) {
        match decode(bytes) {
            Ok(YSyncMessage::SyncStep1(sv)) => {
                let sv = StateVector::decode_v1(&sv).expect("server sent a valid state vector");
                let reply = self.doc.transact().encode_state_as_update_v1(&sv);
                self.ws
                    .as_mut()
                    .unwrap()
                    .send(tungstenite::Message::Binary(sync_frame(1, &reply).into()))
                    .await
                    .unwrap();
                self.step1_answered += 1;
            }
            Ok(YSyncMessage::SyncStep2(u)) => {
                self.apply(&u);
                self.step2_received += 1;
            }
            Ok(YSyncMessage::Update(u)) => self.apply(&u),
            _ => {}
        }
    }

    fn apply(&self, update: &[u8]) {
        let u = Update::decode_v1(update).expect("server sent a valid update");
        self.doc.transact_mut().apply_update(u).unwrap();
    }

    /// Process inbound frames until `done` holds or DEADLINE elapses.
    async fn pump_until(&mut self, done: impl Fn(&Peer) -> bool) -> bool {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if done(self) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let ws = self.ws.as_mut().expect("pump on a connected peer");
            match tokio::time::timeout(remaining, ws.next()).await {
                Ok(Some(Ok(tungstenite::Message::Binary(b)))) => self.handle(&b).await,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => panic!("ws error: {e}"),
                Ok(None) => panic!("server closed the socket"),
                Err(_) => return false,
            }
        }
    }
}

fn paragraph_text(frag: &XmlFragmentRef, txn: &mut TransactionMut, s: &str) -> XmlTextRef {
    let p = frag.push_back(txn, XmlElementPrelim::empty("paragraph"));
    p.push_back(txn, XmlTextPrelim::new(s))
}

// ---------------------------------------------------------------------------
// Who gets credited
// ---------------------------------------------------------------------------

/// The everyday case: an editor types over the collab socket.
#[tokio::test(flavor = "multi_thread")]
async fn live_edit_credits_the_editor() {
    let fx = fixture().await;
    let mut bob = Peer::join(&fx, &fx.bob).await;
    bob.type_paragraph("hello").await;

    let byline = wait_for_byline(&fx, fx.doc_id, "Bob's live edit", |l| {
        l.contains(&fx.bob.id)
    })
    .await;
    assert_eq!(
        listed(&byline),
        vec![fx.bob.id],
        "only the editor who typed is a contributor (not the creator, who did not edit)"
    );
    let rows = update_rows(&fx.pool, fx.doc_id).await;
    assert!(
        !rows.is_empty() && rows.iter().all(|r| r.by_user_id == Some(fx.bob.id)),
        "live edits must be stored under their author: {:?}",
        rows.iter().map(|r| r.by_user_id).collect::<Vec<_>>()
    );
    bob.leave().await;
}

/// A viewer's updates are dropped at the socket (`can_write`), so they can
/// neither change the page nor become a contributor by trying.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_update_over_the_socket_credits_nobody() {
    let fx = fixture().await;
    let mut vic = Peer::join(&fx, &fx.vic).await;
    vic.type_paragraph("vandal").await;
    vic.settle().await;

    // Sentinel: once Bob's later edit is durable, anything of Vic's that
    // reached the writer would be too.
    let mut bob = Peer::join(&fx, &fx.bob).await;
    bob.type_paragraph("sentinel").await;
    wait_for_durable_text(&fx.pool, fx.doc_id, "sentinel").await;

    let byline = wait_for_byline(&fx, fx.doc_id, "sentinel", |l| l.contains(&fx.bob.id)).await;
    assert_eq!(listed(&byline), vec![fx.bob.id], "the viewer was credited");
    let rows = update_rows(&fx.pool, fx.doc_id).await;
    assert!(
        rows.iter().all(|r| r.by_user_id != Some(fx.vic.id)),
        "a viewer's update was persisted"
    );
    vic.leave().await;
    bob.leave().await;
}

/// Two people typing at once share the writer's 250 ms batch. Both must be
/// credited, and every row must carry the author whose client produced it
/// — not whoever happened to be first in the batch.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_editors_are_each_credited_for_their_own_rows() {
    let fx = fixture().await;
    let mut bob = Peer::join(&fx, &fx.bob).await;
    let mut carol = Peer::join(&fx, &fx.carol).await;

    // Interleaved, back to back: these land in the same writer batch.
    for i in 0..3 {
        bob.type_paragraph(&format!("bob{i}")).await;
        carol.type_paragraph(&format!("carol{i}")).await;
    }
    bob.settle().await;
    carol.settle().await;
    wait_for_durable_text(&fx.pool, fx.doc_id, "bob2").await;
    wait_for_durable_text(&fx.pool, fx.doc_id, "carol2").await;

    let byline = wait_for_byline(&fx, fx.doc_id, "both editors", |l| {
        l.contains(&fx.bob.id) && l.contains(&fx.carol.id)
    })
    .await;
    assert_eq!(listed(&byline).len(), 2, "byline {byline}");

    // Each row's yrs client id says whose editor produced it.
    let author_of = |client: yrs::ClientID| {
        if client == bob.doc.client_id() {
            fx.bob.id
        } else if client == carol.doc.client_id() {
            fx.carol.id
        } else {
            panic!("row from unknown client {client}")
        }
    };
    let rows = update_rows(&fx.pool, fx.doc_id).await;
    assert_eq!(rows.len(), 6, "one row per edit");
    for r in &rows {
        let u = Update::decode_v1(&r.update_bytes).unwrap();
        // `state_vector_lower`: every client with blocks in the update
        // (`state_vector` skips those whose blocks do not start at clock 0).
        let clients: Vec<_> = u.state_vector_lower().iter().map(|(c, _)| *c).collect();
        assert_eq!(clients.len(), 1, "each live edit comes from one client");
        assert_eq!(
            r.by_user_id,
            Some(author_of(clients[0])),
            "row {} stored under the wrong author",
            r.seq
        );
    }
    bob.leave().await;
    carol.leave().await;
}

/// Opening a page as an editor is not editing it. On join the server asks
/// for the client's state (SyncStep1) and every writer answers; that answer
/// usually carries nothing new and must not count.
#[tokio::test(flavor = "multi_thread")]
async fn editor_who_only_opens_the_page_is_not_credited() {
    let fx = fixture().await;
    // Seed history with a deletion, so the no-op answer carries a non-empty
    // delete set — the realistic case.
    let mut bob = Peer::join(&fx, &fx.bob).await;
    let text: Arc<Mutex<Option<XmlTextRef>>> = Arc::default();
    let slot = text.clone();
    bob.edit(move |frag, txn| {
        *slot.lock().unwrap() = Some(paragraph_text(frag, txn, "seedx"));
    })
    .await;
    let t = text.lock().unwrap().take().unwrap();
    bob.edit(move |_, txn| t.remove_range(txn, 4, 1)).await;
    bob.settle().await;

    // Carol opens the page, syncs, answers the server's SyncStep1, leaves.
    let mut carol = Peer::join(&fx, &fx.carol).await;
    assert!(
        carol
            .pump_until(|p| doc_text(&p.doc).contains("seed<"))
            .await,
        "Carol never received the page"
    );
    carol.leave().await;

    // Sentinel after Carol's answer: once durable, her answer would be too.
    bob.type_paragraph("sentinel").await;
    wait_for_durable_text(&fx.pool, fx.doc_id, "sentinel").await;

    let byline = wait_for_byline(&fx, fx.doc_id, "Bob", |l| l.contains(&fx.bob.id)).await;
    assert_eq!(
        listed(&byline),
        vec![fx.bob.id],
        "an editor who only opened the page was credited"
    );
    let rows = update_rows(&fx.pool, fx.doc_id).await;
    assert!(
        rows.iter().all(|r| r.by_user_id == Some(fx.bob.id)),
        "the opener's no-op answer was persisted: {:?}",
        rows.iter().map(|r| r.by_user_id).collect::<Vec<_>>()
    );
    bob.leave().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn history_restore_credits_the_restorer() {
    let fx = fixture().await;
    let (_, bytes) = knot_markdown::from_markdown::parse("# Snapshot\n\nOld text.").unwrap();
    let engine = knot_crdt::YrsEngine;
    let doc = knot_crdt::Engine::new_doc(&engine);
    knot_crdt::Engine::apply_update(&engine, &doc, &bytes).unwrap();
    let state = knot_crdt::Engine::encode_state_as_update(&engine, &doc, None).unwrap();
    let sv = knot_crdt::Engine::encode_state_vector(&engine, &doc).unwrap();
    PgSnapshotStore::new(fx.pool.clone())
        .insert(fx.doc_id, 1, &state, &sv)
        .await
        .unwrap();

    let (status, body) = post(
        &fx.app,
        &fx.carol.session,
        &format!("/api/docs/{}/history/1/restore", fx.doc_id),
        "application/json",
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "restore: {body}");

    let byline = wait_for_byline(&fx, fx.doc_id, "restore", |l| l.contains(&fx.carol.id)).await;
    assert_eq!(listed(&byline), vec![fx.carol.id]);
    let rows = update_rows(&fx.pool, fx.doc_id).await;
    assert_eq!(
        rows.last().map(|r| r.by_user_id),
        Some(Some(fx.carol.id)),
        "the restore must be stored under the restorer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn markdown_import_credits_the_importer() {
    let fx = fixture().await;
    let (status, body) = post(
        &fx.app,
        &fx.bob.session,
        &format!("/api/docs/{}/markdown", fx.doc_id),
        "text/markdown",
        b"# Imported\n\nBody.".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "import: {body}");

    let byline = wait_for_byline(&fx, fx.doc_id, "import", |l| l.contains(&fx.bob.id)).await;
    assert_eq!(listed(&byline), vec![fx.bob.id]);
}

/// Importing a workspace export fills each new page with content; the
/// person who ran the import is the one who put it there.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_import_credits_the_importer() {
    let fx = fixture().await;
    let old_id = Uuid::new_v4();
    let manifest = serde_json::json!({
        "knot_export_version": "2",
        "docs": [{
            "id": old_id.to_string(),
            "parent_id": null,
            "title": "From the archive",
            "sort_key": "n",
            "path": "docs/From-the-archive.md",
        }],
        "attachments": [],
        "boards": [],
    });
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("index.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("docs/From-the-archive.md", opts).unwrap();
    zip.write_all(b"# From the archive\n\nImported body.")
        .unwrap();
    let bytes = zip.finish().unwrap().into_inner();

    let (status, body) = post(
        &fx.app,
        &fx.alice.session,
        "/api/workspace/import",
        "application/zip",
        bytes,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "workspace import: {body}");

    let imported = fx
        .state
        .docs
        .as_ref()
        .unwrap()
        .list_alive(fx.ws_id)
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.title == "From the archive")
        .expect("imported doc");
    let byline = wait_for_byline(&fx, imported.id, "workspace import", |l| {
        l.contains(&fx.alice.id)
    })
    .await;
    assert_eq!(listed(&byline), vec![fx.alice.id]);
}

// ---------------------------------------------------------------------------
// GET /api/docs/{id}/contributors
// ---------------------------------------------------------------------------

fn at(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 3, h, 0, 0).unwrap()
}

/// Credit `user` with one content change, then pin its timestamps.
async fn contribute(fx: &Fixture, user: Uuid, first: DateTime<Utc>, last: DateTime<Utc>) {
    PgUpdatesStore::new(fx.pool.clone())
        .insert_batch(fx.doc_id, &[(Some(user), vec![0u8])])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE doc_contributors SET first_edited_at = $3, last_edited_at = $4
         WHERE doc_id = $1 AND user_id = $2",
    )
    .bind(fx.doc_id)
    .bind(user)
    .bind(first)
    .bind(last)
    .execute(&fx.pool)
    .await
    .unwrap();
}

fn keys(v: &serde_json::Value) -> Vec<&str> {
    let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    k.sort();
    k
}

fn ts(v: &serde_json::Value) -> DateTime<Utc> {
    let s = v.as_str().expect("timestamp string");
    assert!(s.ends_with('Z'), "timestamps are RFC 3339 in UTC: {s}");
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// The full contract, read by the least-privileged role.
#[tokio::test(flavor = "multi_thread")]
async fn viewer_reads_the_byline() {
    let fx = fixture().await;
    contribute(&fx, fx.bob.id, at(9), at(11)).await;
    contribute(&fx, fx.carol.id, at(10), at(12)).await;

    let (status, content_type, body) =
        get_contributors(&fx.app, Some(&fx.vic.session), fx.doc_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type.as_deref(), Some("application/json"));
    assert!(
        !body.contains('@'),
        "the byline must never carry emails: {body}"
    );

    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        keys(&v),
        [
            "contributors",
            "contributors_since",
            "created_at",
            "created_by"
        ]
    );
    assert_eq!(
        v["created_by"],
        serde_json::json!({"id": fx.alice.id, "display_name": "Alice"})
    );
    assert_eq!(ts(&v["created_at"]), fx.doc_created_at);
    assert_eq!(
        ts(&v["contributors_since"]),
        fx.doc_created_at,
        "a doc created after tracking began is tracked from creation"
    );

    let contributors = v["contributors"].as_array().unwrap();
    assert_eq!(contributors.len(), 2, "{body}");
    for (c, (id, name, first, last)) in contributors.iter().zip([
        (fx.carol.id, "Carol", at(10), at(12)),
        (fx.bob.id, "Bob", at(9), at(11)),
    ]) {
        assert_eq!(
            keys(c),
            [
                "display_name",
                "first_edited_at",
                "last_edited_at",
                "user_id"
            ]
        );
        assert_eq!(c["user_id"], serde_json::json!(id));
        assert_eq!(c["display_name"], name);
        assert_eq!(ts(&c["first_edited_at"]), first);
        assert_eq!(ts(&c["last_edited_at"]), last);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn byline_requires_a_session() {
    let fx = fixture().await;
    let (status, _, body) = get_contributors(&fx.app, None, fx.doc_id).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.session_required");
}

/// A doc the caller cannot open (here: another workspace's) is refused
/// exactly like every other doc sub-resource.
#[tokio::test(flavor = "multi_thread")]
async fn byline_is_denied_without_access_to_the_doc() {
    let fx = fixture().await;
    let ws = fx.state.workspaces.as_ref().unwrap();
    let other = ws.create("other", "Other").await.unwrap();
    let stranger = fx
        .state
        .users
        .as_ref()
        .unwrap()
        .create_local("stranger@contrib.test", "Stranger", "$h$")
        .await
        .unwrap();
    ws.add_member(other.id, stranger.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let foreign = fx
        .state
        .docs
        .as_ref()
        .unwrap()
        .create(other.id, None, "Foreign", "m", stranger.id)
        .await
        .unwrap();

    let (status, _, body) = get_contributors(&fx.app, Some(&fx.alice.session), foreign.id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["code"], "acl.no_grant");
}

/// Leaving the workspace does not erase what someone wrote.
#[tokio::test(flavor = "multi_thread")]
async fn byline_still_names_a_former_member() {
    let fx = fixture().await;
    contribute(&fx, fx.bob.id, at(9), at(11)).await;
    fx.state
        .workspaces
        .as_ref()
        .unwrap()
        .remove_member(fx.ws_id, fx.bob.id)
        .await
        .unwrap();

    let (status, _, body) = get_contributors(&fx.app, Some(&fx.alice.session), fx.doc_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["contributors"],
        serde_json::json!([{
            "user_id": fx.bob.id,
            "display_name": "Bob",
            "first_edited_at": v["contributors"][0]["first_edited_at"],
            "last_edited_at": v["contributors"][0]["last_edited_at"],
        }])
    );
}
