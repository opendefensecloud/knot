//! Offline edits must reach the server on reconnect.
//!
//! The web providers (`web/src/features/editor/KnotProvider.ts`,
//! `web/src/features/boards/BoardProvider.ts`) apply edits made while the
//! socket is not OPEN to their local Y.Doc but do not send them. On
//! (re)connect they send SyncStep1, which only asks the server for what the
//! CLIENT lacks; they answer a server SyncStep1 with
//! `SyncStep2 = encodeStateAsUpdate(doc, serverSV)`. So offline edits reach
//! the server only if the server asks — standard y-protocol, where BOTH
//! sides send SyncStep1. These tests drive the real collab sockets with a raw
//! client that mirrors those providers exactly, so the fix has to work for
//! already-deployed SPA bundles without any client change.
//!
//! Once the server asks, every (re)connecting writer replies with a
//! SyncStep2 that usually carries nothing new (Yjs always includes its whole
//! delete set). The `*_noop_*` tests pin that such replies are neither
//! persisted nor fanned out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use knot_auth::{Hasher, Throttle};
use knot_server::protocol::{YSyncMessage, decode};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    BoardStore, PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole,
    sort_key_between,
};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use tower::ServiceExt;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, Map, MapRef, ReadTxn, StateVector, Text, Transact, TransactionMut, Update,
    XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlTextPrelim, XmlTextRef,
};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Generous: a loaded CI runner can take a while to hydrate a room and run
/// the writer's 250 ms batch. Every wait polls toward this deadline and
/// returns as soon as its condition holds — no fixed sleeps.
const DEADLINE: Duration = Duration::from_secs(10);
/// How long the reconnecting client waits to be asked for its state. Pre-fix
/// the server never asks, so this is pure dead time — keep it short.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    addr: std::net::SocketAddr,
    /// `sid=…; csrf=…` for the workspace owner.
    cookie: String,
    pool: knot_storage::Pool,
    user_id: Uuid,
    doc_id: Uuid,
    boards: Arc<dyn BoardStore>,
}

/// Postgres-backed state with doc rooms AND board rooms wired to a MemBus,
/// one owner, one doc, served on an ephemeral port.
async fn fixture() -> Fixture {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s.cookie_secure = false;

    let engine: Arc<dyn knot_crdt::Engine> = Arc::new(knot_crdt::YrsEngine);
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::MemBus::new());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    s.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        engine.clone(),
        bus.clone(),
        updates,
        snapshots,
        knot_crdt::SnapshotPolicy {
            every_n: 1000,
            idle: Duration::from_secs(600),
        },
        Duration::from_secs(300),
    )));
    let boards = s.boards.clone().expect("with_pool wires boards");
    s.board_rooms = Some(Arc::new(knot_crdt::BoardRooms::new(
        engine,
        boards.clone(),
        bus,
    )));

    let email = "offline@sync.test";
    let password = "offlinepass";
    let hash = s.hasher.hash(password).unwrap();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let u = s
        .users
        .as_ref()
        .unwrap()
        .create_local(email, "U", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Offline", &sort_key_between(None, None), u.id)
        .await
        .unwrap();

    let r = router_with_state(s.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .filter(|kv| kv.starts_with("sid=") || kv.starts_with("csrf="))
        .collect::<Vec<_>>()
        .join("; ");
    assert!(cookie.contains("sid="), "login set no session cookie");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router_with_state(s);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Fixture {
        addr,
        cookie,
        pool,
        user_id: u.id,
        doc_id: doc.id,
        boards,
    }
}

// ---------------------------------------------------------------------------
// Raw y-sync client mirroring KnotProvider / BoardProvider
// ---------------------------------------------------------------------------

fn sync_frame(subtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8, subtype];
    knot_server::protocol::append_var_uint(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out
}

/// Root types both providers use: Tiptap's `default` fragment and
/// Excalidraw's `elements` map.
struct Roots {
    frag: XmlFragmentRef,
    elements: MapRef,
}

struct Peer {
    doc: Doc,
    ws: Option<Ws>,
    /// Server SyncStep1 frames this peer has answered with a SyncStep2.
    step1_answered: usize,
    /// Server SyncStep2 frames applied (the initial state on every join).
    step2_received: usize,
    /// SYNC_UPDATE frames applied — i.e. fan-out from other connections.
    updates_received: usize,
}

impl Peer {
    fn new() -> Self {
        Self {
            doc: Doc::new(),
            ws: None,
            step1_answered: 0,
            step2_received: 0,
            updates_received: 0,
        }
    }

    /// `connect()` + `onopen`: dial, then send SyncStep1 with our state
    /// vector. (The provider also sends its awareness state; awareness is
    /// irrelevant to document sync and is left out.)
    async fn connect(&mut self, fx: &Fixture, path: &str) {
        let mut req = format!("ws://{}{path}", fx.addr)
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "cookie",
            tungstenite::http::HeaderValue::from_str(&fx.cookie).unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("ws connect");
        let sv = self.doc.transact().state_vector().encode_v1();
        ws.send(tungstenite::Message::Binary(sync_frame(0, &sv).into()))
            .await
            .unwrap();
        self.ws = Some(ws);
    }

    /// The network goes away: drop the TCP connection without a close
    /// handshake, as a laptop lid or a flaky train connection would. (A
    /// graceful `close()` would wait for the server's Close reply, which the
    /// server's read loop never sends once it has broken out.)
    ///
    /// Settles first: closing a socket with unread inbound data makes the OS
    /// send RST, and the server may then discard frames it has not read yet.
    /// That is a real-world loss mode too, but these tests pin a different
    /// one, so make sure everything sent so far has been applied.
    async fn disconnect(&mut self) {
        if self.ws.is_some() {
            self.settle().await;
        }
        self.ws.take();
    }

    /// Round-trip barrier: the server answers SyncStep1 from the same read
    /// loop that forwards our updates to the room actor, and the actor handles
    /// both in order, so once that SyncStep2 is back every update we sent
    /// before it has been applied.
    async fn settle(&mut self) {
        let sv = self.doc.transact().state_vector().encode_v1();
        let ws = self.ws.as_mut().unwrap();
        ws.send(tungstenite::Message::Binary(sync_frame(0, &sv).into()))
            .await
            .unwrap();
        let seen = self.step2_received;
        assert!(
            self.pump_until(DEADLINE, |p| p.step2_received > seen).await,
            "server never answered the settle SyncStep1"
        );
    }

    /// A local edit, as Tiptap/Excalidraw would make it. Mirrors
    /// `handleDocUpdate`: the resulting update is sent only while the socket
    /// is open; offline it exists solely in the local doc.
    async fn edit(&mut self, f: impl FnOnce(&Roots, &mut TransactionMut)) {
        // Root refs must be taken BEFORE opening the transaction:
        // `get_or_insert_*` acquires its own write lock and would deadlock
        // against an open `TransactionMut`.
        let roots = Roots {
            frag: self.doc.get_or_insert_xml_fragment("default"),
            elements: self.doc.get_or_insert_map("elements"),
        };
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = captured.clone();
        let sub = self
            .doc
            .observe_update_v1(move |_, e| sink.lock().unwrap().push(e.update.clone()));
        {
            let mut txn = self.doc.transact_mut();
            f(&roots, &mut txn);
        }
        drop(sub);
        let updates = std::mem::take(&mut *captured.lock().unwrap());
        if let Some(ws) = self.ws.as_mut() {
            for u in updates {
                ws.send(tungstenite::Message::Binary(sync_frame(2, &u).into()))
                    .await
                    .unwrap();
            }
        }
    }

    /// `handleMessage` for MSG_SYNC: answer SyncStep1 with
    /// `encodeStateAsUpdate(doc, serverSV)`; apply SyncStep2 / Update.
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
            Ok(YSyncMessage::Update(u)) => {
                self.apply(&u);
                self.updates_received += 1;
            }
            _ => {}
        }
    }

    fn apply(&self, update: &[u8]) {
        let u = Update::decode_v1(update).expect("server sent a valid update");
        self.doc.transact_mut().apply_update(u).unwrap();
    }

    /// Process inbound frames until `done` holds or `within` elapses.
    /// Returns whether `done` held.
    async fn pump_until(&mut self, within: Duration, done: impl Fn(&Peer) -> bool) -> bool {
        let deadline = Instant::now() + within;
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

    fn doc_text(&self) -> String {
        doc_text(&self.doc)
    }

    fn board_keys(&self) -> Vec<String> {
        board_keys(&self.doc)
    }
}

fn doc_text(doc: &Doc) -> String {
    let frag = doc.get_or_insert_xml_fragment("default");
    frag.get_string(&doc.transact())
}

fn board_keys(doc: &Doc) -> Vec<String> {
    let elements = doc.get_or_insert_map("elements");
    let txn = doc.transact();
    let mut keys: Vec<String> = elements.keys(&txn).map(str::to_string).collect();
    keys.sort();
    keys
}

/// Rebuild the doc exactly as a cold `Room::spawn` would: latest snapshot,
/// then every persisted update after it. Returns the doc and the number of
/// `doc_updates` rows.
async fn hydrate_doc(pool: &knot_storage::Pool, doc_id: Uuid) -> (Doc, usize) {
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
    let rows = PgUpdatesStore::new(pool.clone())
        .since(doc_id, after)
        .await
        .unwrap();
    for r in &rows {
        let u = Update::decode_v1(&r.update_bytes).unwrap();
        doc.transact_mut().apply_update(u).unwrap();
    }
    (doc, rows.len())
}

/// Board equivalent of [`hydrate_doc`] (`BoardRoom::spawn` replays the whole
/// `board_updates` log). Returns the doc and the row count.
async fn hydrate_board(boards: &Arc<dyn BoardStore>, board_id: Uuid) -> (Doc, usize) {
    let doc = Doc::new();
    let rows = boards.load_updates(board_id).await.unwrap();
    for r in &rows {
        let u = Update::decode_v1(r).unwrap();
        doc.transact_mut().apply_update(u).unwrap();
    }
    (doc, rows.len())
}

/// Poll storage until `done` holds for the hydrated doc, or panic.
async fn wait_for_storage<F, Fut>(what: &str, load: F, done: impl Fn(&Doc) -> bool) -> usize
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = (Doc, usize)>,
{
    let deadline = Instant::now() + DEADLINE;
    loop {
        let (doc, rows) = load().await;
        if done(&doc) {
            return rows;
        }
        if Instant::now() >= deadline {
            panic!(
                "{what} never became durable; storage holds text {:?}, board keys {:?}",
                doc_text(&doc),
                board_keys(&doc)
            );
        }
        // Storage is polled, not pushed: back off briefly between queries so
        // the loop does not hammer Postgres. Bounded by DEADLINE above.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn paragraph_text(roots: &Roots, txn: &mut TransactionMut, s: &str) -> XmlTextRef {
    let p = roots
        .frag
        .push_back(txn, XmlElementPrelim::empty("paragraph"));
    p.push_back(txn, XmlTextPrelim::new(s))
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// The reader report: edits made while offline never reached the server, and
/// later edits built on them were parked as pending by yrs and stayed
/// invisible to everyone else.
#[tokio::test(flavor = "multi_thread")]
async fn doc_edits_made_offline_reach_the_server_on_reconnect() {
    let fx = fixture().await;
    let path = format!("/collab/doc/{}", fx.doc_id);

    // A connects, syncs, types "online" while connected.
    let mut a = Peer::new();
    a.connect(&fx, &path).await;
    assert!(
        a.pump_until(DEADLINE, |p| p.step2_received >= 1).await,
        "A never received the initial state"
    );
    let text: Arc<Mutex<Option<XmlTextRef>>> = Arc::default();
    let slot = text.clone();
    a.edit(move |r, txn| {
        *slot.lock().unwrap() = Some(paragraph_text(r, txn, "online"));
    })
    .await;
    let text = text.lock().unwrap().take().unwrap();

    // Connection drops. A keeps typing; the update is NOT sent.
    a.disconnect().await;
    let t = text.clone();
    a.edit(move |_, txn| t.push(txn, " offline")).await;

    // Reconnect: onopen sends SyncStep1, and A answers any server SyncStep1.
    let answered = a.step1_answered;
    a.connect(&fx, &path).await;
    let asked = a
        .pump_until(HANDSHAKE_WAIT, |p| p.step1_answered > answered)
        .await;
    // A keeps typing live. This update depends on the offline one.
    let t = text.clone();
    a.edit(move |_, txn| t.push(txn, "!")).await;

    // A fresh reader must see everything.
    let mut b = Peer::new();
    b.connect(&fx, &path).await;
    let converged = b
        .pump_until(DEADLINE, |p| p.doc_text().contains("online offline!"))
        .await;
    assert!(
        converged,
        "fresh client never saw the offline edit: got {:?}; the server asked the \
         reconnecting client for its state (SyncStep1): {asked}",
        b.doc_text()
    );

    // And it is durable: a cold hydration from doc_updates has it too.
    let pool = fx.pool.clone();
    let doc_id = fx.doc_id;
    wait_for_storage(
        "offline doc edit",
        || hydrate_doc(&pool, doc_id),
        |d| doc_text(d).contains("online offline!"),
    )
    .await;

    a.disconnect().await;
    b.disconnect().await;
}

/// Once the server asks every joining writer for its state, the typical
/// reply carries nothing new (just the full delete set). That must not be
/// persisted — otherwise every page open writes a `doc_updates` row and makes
/// the opener a "contributor" — nor fanned out to the other connections.
#[tokio::test(flavor = "multi_thread")]
async fn doc_noop_sync_reply_is_neither_persisted_nor_fanned_out() {
    let fx = fixture().await;
    let path = format!("/collab/doc/{}", fx.doc_id);

    // Seed real history including a deletion, so every later SyncStep2 reply
    // carries a non-empty delete set — the realistic no-op.
    let mut a = Peer::new();
    a.connect(&fx, &path).await;
    assert!(a.pump_until(DEADLINE, |p| p.step2_received >= 1).await);
    let text: Arc<Mutex<Option<XmlTextRef>>> = Arc::default();
    let slot = text.clone();
    a.edit(move |r, txn| {
        *slot.lock().unwrap() = Some(paragraph_text(r, txn, "seedx"));
    })
    .await;
    let text = text.lock().unwrap().take().unwrap();
    a.edit(move |_, txn| text.remove_range(txn, 4, 1)).await;
    a.disconnect().await;
    let real_updates = 2;

    // B observes. C joins and makes one real edit.
    let mut b = Peer::new();
    b.connect(&fx, &path).await;
    assert!(
        b.pump_until(DEADLINE, |p| p.step1_answered >= 1
            && p.doc_text().contains("seed<"))
            .await,
        "B never completed the handshake: text {:?}, SyncStep1 answered {}",
        b.doc_text(),
        b.step1_answered
    );
    let mut c = Peer::new();
    c.connect(&fx, &path).await;
    assert!(
        c.pump_until(DEADLINE, |p| p.step1_answered >= 1
            && p.doc_text().contains("seed<"))
            .await,
        "C never completed the handshake"
    );
    c.edit(|r, txn| {
        paragraph_text(r, txn, "sentinel");
    })
    .await;
    let real_updates = real_updates + 1;

    // B's next fan-out must be the sentinel itself — not C's no-op reply.
    assert!(
        b.pump_until(DEADLINE, |p| p.doc_text().contains("sentinel"))
            .await,
        "B never received C's edit"
    );
    assert_eq!(
        b.updates_received, 1,
        "B was sent {} SYNC_UPDATE frames for one real edit: C's no-op \
         SyncStep2 was fanned out",
        b.updates_received
    );

    // The writer persists in arrival order, so once the sentinel is durable
    // any no-op enqueued before it would be too.
    let pool = fx.pool.clone();
    let doc_id = fx.doc_id;
    let rows = wait_for_storage(
        "sentinel",
        || hydrate_doc(&pool, doc_id),
        |d| doc_text(d).contains("sentinel"),
    )
    .await;
    assert_eq!(
        rows, real_updates,
        "doc_updates holds {rows} rows for {real_updates} real edits: no-op \
         SyncStep2 replies from (re)connecting writers were persisted"
    );

    b.disconnect().await;
    c.disconnect().await;
}

// ---------------------------------------------------------------------------
// Boards — BoardProvider has the identical offline gap
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn board_edits_made_offline_reach_the_server_on_reconnect() {
    let fx = fixture().await;
    let board = fx.boards.create(fx.doc_id, fx.user_id, None).await.unwrap();
    let path = format!("/collab/board/{}", board.id);

    let mut a = Peer::new();
    a.connect(&fx, &path).await;
    assert!(a.pump_until(DEADLINE, |p| p.step2_received >= 1).await);
    a.edit(|r, txn| {
        r.elements.insert(txn, "online", "rect");
    })
    .await;

    a.disconnect().await;
    a.edit(|r, txn| {
        r.elements.insert(txn, "offline", "ellipse");
    })
    .await;

    let answered = a.step1_answered;
    a.connect(&fx, &path).await;
    let asked = a
        .pump_until(HANDSHAKE_WAIT, |p| p.step1_answered > answered)
        .await;

    let mut b = Peer::new();
    b.connect(&fx, &path).await;
    let converged = b
        .pump_until(DEADLINE, |p| p.board_keys() == ["offline", "online"])
        .await;
    assert!(
        converged,
        "fresh client never saw the offline board edit: got {:?}; the server \
         asked the reconnecting client for its state (SyncStep1): {asked}",
        b.board_keys()
    );

    let boards = fx.boards.clone();
    wait_for_storage(
        "offline board edit",
        || hydrate_board(&boards, board.id),
        |d| board_keys(d) == ["offline", "online"],
    )
    .await;

    a.disconnect().await;
    b.disconnect().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn board_noop_sync_reply_is_neither_persisted_nor_fanned_out() {
    let fx = fixture().await;
    let board = fx.boards.create(fx.doc_id, fx.user_id, None).await.unwrap();
    let path = format!("/collab/board/{}", board.id);

    // Seed history with a deletion (Excalidraw deletes map keys too).
    let mut a = Peer::new();
    a.connect(&fx, &path).await;
    assert!(a.pump_until(DEADLINE, |p| p.step2_received >= 1).await);
    a.edit(|r, txn| {
        r.elements.insert(txn, "keep", "rect");
        r.elements.insert(txn, "gone", "rect");
    })
    .await;
    a.edit(|r, txn| {
        r.elements.remove(txn, "gone");
    })
    .await;
    a.disconnect().await;
    let real_updates = 2;

    let mut b = Peer::new();
    b.connect(&fx, &path).await;
    assert!(
        b.pump_until(DEADLINE, |p| p.step1_answered >= 1
            && p.board_keys() == ["keep"])
            .await,
        "B never completed the handshake: keys {:?}, SyncStep1 answered {}",
        b.board_keys(),
        b.step1_answered
    );
    let mut c = Peer::new();
    c.connect(&fx, &path).await;
    assert!(
        c.pump_until(DEADLINE, |p| p.step1_answered >= 1
            && p.board_keys() == ["keep"])
            .await,
        "C never completed the handshake"
    );
    c.edit(|r, txn| {
        r.elements.insert(txn, "sentinel", "rect");
    })
    .await;
    let real_updates = real_updates + 1;

    assert!(
        b.pump_until(DEADLINE, |p| p.board_keys().contains(&"sentinel".into()))
            .await,
        "B never received C's edit"
    );
    assert_eq!(
        b.updates_received, 1,
        "B was sent {} SYNC_UPDATE frames for one real edit: C's no-op \
         SyncStep2 was fanned out",
        b.updates_received
    );

    // BoardRoom appends inline before fanning out, so by the time B has the
    // sentinel every earlier inbound update has already been written.
    let (_, rows) = hydrate_board(&fx.boards, board.id).await;
    assert_eq!(
        rows, real_updates,
        "board_updates holds {rows} rows for {real_updates} real edits: no-op \
         SyncStep2 replies from (re)connecting writers were persisted"
    );

    b.disconnect().await;
    c.disconnect().await;
}
