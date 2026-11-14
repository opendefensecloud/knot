//! Collab WebSocket limits through real sockets (issue #88, spec test 5).
//!
//! The anti-goal these pin: a limit must never drop or refuse an edit. Over
//! budget, the server reads more slowly (backpressure); presence is
//! coalesced to the newest state; a second `SyncStep1` is left unanswered;
//! the open-socket cap and the per-target upgrade bucket refuse the upgrade
//! with 429. A revoke, refusal or shutdown still ends a throttled session at
//! once.
//!
//! The raw client mirrors `KnotProvider` / `BoardProvider` as in
//! `tests/offline_sync.rs`. Sessions are signed in through a router without
//! limits, so the sign-in bucket never gets in the way, and the sockets are
//! served by a router built from the limited config. Needs Postgres
//! (`knot_test_support::fresh_db`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use knot_auth::{Hasher, Throttle};
use knot_config::Config;
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
    Doc, GetString, Map, MapRef, ReadTxn, StateVector, Transact, TransactionMut, Update,
    XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlTextPrelim,
};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Generous: every wait polls toward it and returns as soon as its
/// condition holds.
const DEADLINE: Duration = Duration::from_secs(30);

/// Every budget at its floor: doc bytes 4 MiB then 64 KiB/s, messages one
/// then 20/s, presence one then one a minute, one upgrade per target a
/// minute.
const FLOOR_SCALE: f64 = 0.0001;

fn limited_config(scale: f64, max_sockets: u32) -> Config {
    Config {
        rate_limit_enabled: true,
        rate_limit_scale: scale,
        rate_limit_collab_max_sockets: max_sockets,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Session {
    user_id: Uuid,
    /// `sid=…; csrf=…`.
    cookie: String,
}

struct Fixture {
    addr: std::net::SocketAddr,
    state: AppState,
    pool: knot_storage::Pool,
    workspace_id: Uuid,
    /// Sends the edits.
    writer: Session,
    /// Watches them arrive.
    reader: Session,
    doc_id: Uuid,
    boards: Arc<dyn BoardStore>,
    /// Sort key of the last top-level page created, so each new page sorts
    /// after it (top-level siblings need distinct keys).
    last_sort_key: std::sync::Mutex<String>,
}

impl Fixture {
    /// Another page in the workspace, created by the writer.
    async fn another_doc(&self, title: &str) -> Uuid {
        let next_sort_key = {
            let mut last_sort_key = self.last_sort_key.lock().unwrap();
            *last_sort_key = sort_key_between(Some(&last_sort_key), None);
            last_sort_key.clone()
        };
        self.state
            .docs
            .as_ref()
            .unwrap()
            .create(
                self.workspace_id,
                None,
                title,
                &next_sort_key,
                self.writer.user_id,
            )
            .await
            .unwrap()
            .id
    }

    fn doc_path(&self) -> String {
        format!("/collab/doc/{}", self.doc_id)
    }
}

async fn fixture(limits: Config) -> Fixture {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut state = AppState::with_pool(pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    state.cookie_secure = false;

    let engine: Arc<dyn knot_crdt::Engine> = Arc::new(knot_crdt::YrsEngine);
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::MemBus::new());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snapshots: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    state.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
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
    let boards = state.boards.clone().expect("with_pool wires boards");
    state.board_rooms = Some(Arc::new(knot_crdt::BoardRooms::new(
        engine,
        boards.clone(),
        bus,
    )));

    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    // Sign in through a router without limits (the constructors' config).
    let unlimited_app = router_with_state(state.clone());
    let writer = sign_up(&state, &unlimited_app, workspace.id, "writer@limits.test").await;
    let reader = sign_up(&state, &unlimited_app, workspace.id, "reader@limits.test").await;
    let first_sort_key = sort_key_between(None, None);
    let doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(
            workspace.id,
            None,
            "Limited",
            &first_sort_key,
            writer.user_id,
        )
        .await
        .unwrap();

    state.config = Arc::new(limits);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let limited_app = router_with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, limited_app).await.unwrap();
    });

    Fixture {
        addr,
        state,
        pool,
        workspace_id: workspace.id,
        writer,
        reader,
        doc_id: doc.id,
        boards,
        last_sort_key: std::sync::Mutex::new(first_sort_key),
    }
}

/// A workspace owner `email`, signed in.
async fn sign_up(
    state: &AppState,
    unlimited_app: &axum::Router,
    workspace_id: Uuid,
    email: &str,
) -> Session {
    let password = "limitspass";
    let hash = state.hasher.hash(password).unwrap();
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
        .add_member(workspace_id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let response = unlimited_app
        .clone()
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
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| {
            value
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_string()
        })
        .filter(|pair| pair.starts_with("sid=") || pair.starts_with("csrf="))
        .collect::<Vec<_>>()
        .join("; ");
    assert!(cookie.contains("sid="), "login set no session cookie");
    Session {
        user_id: user.id,
        cookie,
    }
}

// ---------------------------------------------------------------------------
// Raw y-sync client
// ---------------------------------------------------------------------------

fn sync_frame(subtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8, subtype];
    knot_server::protocol::append_var_uint(&mut frame, payload.len() as u64);
    frame.extend_from_slice(payload);
    frame
}

/// A presence frame carrying `marker` (the room relays it verbatim).
fn awareness_frame(marker: &str) -> Vec<u8> {
    let mut frame = vec![knot_server::protocol::MSG_AWARENESS];
    knot_server::protocol::append_var_uint(&mut frame, marker.len() as u64);
    frame.extend_from_slice(marker.as_bytes());
    frame
}

struct Roots {
    fragment: XmlFragmentRef,
    elements: MapRef,
}

/// A client that mirrors the web providers: SyncStep1 on open, answers a
/// server SyncStep1, applies SyncStep2 and updates.
struct Peer {
    doc: Doc,
    ws: Ws,
    step2_received: usize,
    /// Presence frames received, in order.
    awareness_received: Vec<Vec<u8>>,
}

/// Dial `path` with `session`'s cookie.
async fn dial(fx: &Fixture, path: &str, session: &Session) -> Result<Ws, Box<tungstenite::Error>> {
    let mut request = format!("ws://{}{path}", fx.addr)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "cookie",
        tungstenite::http::HeaderValue::from_str(&session.cookie).unwrap(),
    );
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(ws, _)| ws)
        .map_err(Box::new)
}

/// The response a refused upgrade of `path` is answered with.
async fn refused_upgrade(
    fx: &Fixture,
    path: &str,
    session: &Session,
) -> tungstenite::http::Response<Option<Vec<u8>>> {
    match dial(fx, path, session).await {
        Ok(_) => panic!("the upgrade of {path} was accepted"),
        Err(error) => match *error {
            tungstenite::Error::Http(response) => *response,
            other => panic!("the upgrade of {path} failed without a status: {other}"),
        },
    }
}

impl Peer {
    /// Connect, send SyncStep1 and wait for the room's state.
    async fn join(fx: &Fixture, path: &str, session: &Session) -> Peer {
        let mut ws = dial(fx, path, session).await.expect("ws connect");
        let doc = Doc::new();
        let state_vector = doc.transact().state_vector().encode_v1();
        ws.send(tungstenite::Message::Binary(
            sync_frame(0, &state_vector).into(),
        ))
        .await
        .unwrap();
        let mut peer = Peer {
            doc,
            ws,
            step2_received: 0,
            awareness_received: Vec::new(),
        };
        assert!(
            peer.pump_until(DEADLINE, |peer| peer.step2_received >= 1)
                .await,
            "{path}: the room never sent its state"
        );
        peer
    }

    async fn send_frame(&mut self, frame: Vec<u8>) {
        self.ws
            .send(tungstenite::Message::Binary(frame.into()))
            .await
            .unwrap();
    }

    /// A local edit, sent as one SYNC_UPDATE frame per update.
    async fn edit(&mut self, change: impl FnOnce(&Roots, &mut TransactionMut)) {
        let roots = Roots {
            fragment: self.doc.get_or_insert_xml_fragment("default"),
            elements: self.doc.get_or_insert_map("elements"),
        };
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let sink = captured.clone();
        let subscription = self
            .doc
            .observe_update_v1(move |_, event| sink.lock().unwrap().push(event.update.clone()));
        {
            let mut txn = self.doc.transact_mut();
            change(&roots, &mut txn);
        }
        drop(subscription);
        let updates = std::mem::take(&mut *captured.lock().unwrap());
        for update in updates {
            self.send_frame(sync_frame(2, &update)).await;
        }
    }

    async fn type_paragraph(&mut self, text: &str) {
        let text = text.to_owned();
        self.edit(move |roots, txn| {
            let paragraph = roots
                .fragment
                .push_back(txn, XmlElementPrelim::empty("paragraph"));
            paragraph.push_back(txn, XmlTextPrelim::new(text));
        })
        .await;
    }

    async fn handle(&mut self, bytes: &[u8]) {
        match decode(bytes) {
            Ok(YSyncMessage::SyncStep1(state_vector)) => {
                let state_vector = StateVector::decode_v1(&state_vector).unwrap();
                let reply = self.doc.transact().encode_state_as_update_v1(&state_vector);
                self.send_frame(sync_frame(1, &reply)).await;
            }
            Ok(YSyncMessage::SyncStep2(update)) => {
                self.apply(&update);
                self.step2_received += 1;
            }
            Ok(YSyncMessage::Update(update)) => self.apply(&update),
            Ok(YSyncMessage::Awareness) => self.awareness_received.push(bytes.to_vec()),
            Err(_) => {}
        }
    }

    fn apply(&self, update: &[u8]) {
        let update = Update::decode_v1(update).expect("server sent a valid update");
        self.doc.transact_mut().apply_update(update).unwrap();
    }

    /// Process inbound frames until `done` holds or `within` elapses.
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
            match tokio::time::timeout(remaining, self.ws.next()).await {
                Ok(Some(Ok(tungstenite::Message::Binary(bytes)))) => self.handle(&bytes).await,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("ws error: {error}"),
                Ok(None) => panic!("server closed the socket"),
                Err(_) => return false,
            }
        }
    }

    /// The close code the server closes this socket with, within `within`.
    async fn close_code_within(&mut self, within: Duration) -> u16 {
        let deadline = Instant::now() + within;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, self.ws.next()).await {
                Ok(Some(Ok(tungstenite::Message::Close(Some(frame))))) => return frame.code.into(),
                Ok(Some(Ok(tungstenite::Message::Close(None)))) | Ok(None) => {
                    panic!("socket closed without a close code")
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => panic!("ws error: {error}"),
                Err(_) => panic!("the server did not close the socket within {within:?}"),
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
    let fragment = doc.get_or_insert_xml_fragment("default");
    fragment.get_string(&doc.transact())
}

fn board_keys(doc: &Doc) -> Vec<String> {
    let elements = doc.get_or_insert_map("elements");
    let txn = doc.transact();
    let mut keys: Vec<String> = elements.keys(&txn).map(str::to_string).collect();
    keys.sort();
    keys
}

/// The doc as a cold room would load it: latest snapshot, then updates.
async fn hydrate_doc(pool: &knot_storage::Pool, doc_id: Uuid) -> Doc {
    let doc = Doc::new();
    let mut after = 0;
    if let Some(snapshot) = PgSnapshotStore::new(pool.clone())
        .latest(doc_id)
        .await
        .unwrap()
    {
        let update = Update::decode_v1(&snapshot.state_bytes).unwrap();
        doc.transact_mut().apply_update(update).unwrap();
        after = snapshot.snapshot_seq;
    }
    for row in PgUpdatesStore::new(pool.clone())
        .since(doc_id, after)
        .await
        .unwrap()
    {
        let update = Update::decode_v1(&row.update_bytes).unwrap();
        doc.transact_mut().apply_update(update).unwrap();
    }
    doc
}

async fn hydrate_board(boards: &Arc<dyn BoardStore>, board_id: Uuid) -> Doc {
    let doc = Doc::new();
    for row in boards.load_updates(board_id).await.unwrap() {
        let update = Update::decode_v1(&row).unwrap();
        doc.transact_mut().apply_update(update).unwrap();
    }
    doc
}

/// Poll `load` until `done` holds, or panic after [`DEADLINE`].
async fn wait_for_storage<Load, Loading>(what: &str, load: Load, done: impl Fn(&Doc) -> bool)
where
    Load: Fn() -> Loading,
    Loading: std::future::Future<Output = Doc>,
{
    let deadline = Instant::now() + DEADLINE;
    loop {
        let doc = load().await;
        if done(&doc) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never became durable; storage holds text {:?}, board keys {:?}",
            doc_text(&doc),
            board_keys(&doc)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Backpressure: nothing is lost
// ---------------------------------------------------------------------------

/// Review focus 1: a burst past the byte budget is read more slowly, never
/// dropped. 4.4 MB against a 4 MiB burst refilling at 64 KiB/s leaves about
/// 3 s of debt: the edit after the burst reaches the other client only once
/// it is paid off, and everything is persisted.
#[tokio::test(flavor = "multi_thread")]
async fn ws_burst_past_byte_budget_is_persisted_and_delivered_later() {
    let fx = fixture(limited_config(FLOOR_SCALE, 64)).await;
    let path = fx.doc_path();
    let mut reader = Peer::join(&fx, &path, &fx.reader).await;
    let mut writer = Peer::join(&fx, &path, &fx.writer).await;

    let burst_started = Instant::now();
    let paragraph_bytes = 1_100_000;
    let markers: Vec<String> = (0..4).map(|index| format!("burst-{index}-")).collect();
    for marker in &markers {
        let filler = "x".repeat(paragraph_bytes - marker.len());
        writer.type_paragraph(&format!("{marker}{filler}")).await;
    }
    writer.type_paragraph("after-the-burst").await;

    let delivered = reader
        .pump_until(DEADLINE, |peer| peer.doc_text().contains("after-the-burst"))
        .await;
    assert!(
        delivered,
        "the edit after the burst never reached the reader"
    );
    let delivered_after = burst_started.elapsed();
    assert!(
        delivered_after >= Duration::from_secs(2),
        "the edit after the burst arrived after {delivered_after:?}: the byte debt \
         (about 3 s at 64 KiB/s) was not waited off"
    );
    let reader_text = reader.doc_text();
    for marker in &markers {
        assert!(reader_text.contains(marker), "the reader lacks {marker}");
    }

    let pool = fx.pool.clone();
    let doc_id = fx.doc_id;
    wait_for_storage(
        "the burst",
        || hydrate_doc(&pool, doc_id),
        |doc| {
            let text = doc_text(doc);
            markers.iter().all(|marker| text.contains(marker)) && text.contains("after-the-burst")
        },
    )
    .await;
}

/// The board read loop waits off message debt the same way: 60 element
/// inserts against one message and 20/s arrive, all of them, after ~3 s.
#[tokio::test(flavor = "multi_thread")]
async fn board_burst_past_message_budget_is_persisted_and_delivered_later() {
    let fx = fixture(limited_config(FLOOR_SCALE, 64)).await;
    let board = fx
        .boards
        .create(fx.doc_id, fx.writer.user_id, None)
        .await
        .unwrap();
    let path = format!("/collab/board/{}", board.id);
    let mut reader = Peer::join(&fx, &path, &fx.reader).await;
    let mut writer = Peer::join(&fx, &path, &fx.writer).await;

    let burst_started = Instant::now();
    let element_count = 60;
    for index in 0..element_count {
        writer
            .edit(|roots, txn| {
                roots
                    .elements
                    .insert(txn, format!("element-{index:02}"), "rect");
            })
            .await;
    }
    let delivered = reader
        .pump_until(DEADLINE, |peer| peer.board_keys().len() == element_count)
        .await;
    assert!(
        delivered,
        "the reader has {} of {element_count} elements",
        reader.board_keys().len()
    );
    let delivered_after = burst_started.elapsed();
    assert!(
        delivered_after >= Duration::from_secs(2),
        "the last element arrived after {delivered_after:?}: the message debt \
         (about 3 s at 20/s) was not waited off"
    );

    let boards = fx.boards.clone();
    wait_for_storage(
        "the board burst",
        || hydrate_board(&boards, board.id),
        |doc| board_keys(doc).len() == element_count,
    )
    .await;
}

/// Review focus 2: a revoke while the read loop waits off a debt closes the
/// socket with 4403 at once and ends the session: its place under the
/// socket cap (one, here) is free again right away.
#[tokio::test(flavor = "multi_thread")]
async fn revoke_during_throttle_wait_closes_promptly() {
    let fx = fixture(limited_config(FLOOR_SCALE, 1)).await;
    let path = fx.doc_path();
    let mut reader = Peer::join(&fx, &path, &fx.reader).await;
    let mut writer = Peer::join(&fx, &path, &fx.writer).await;
    // 5.4 MB against 4 MiB: about 19 s of byte debt at 64 KiB/s, waited off
    // before the sentinel is read.
    writer
        .type_paragraph(&format!("big-0-{}", "x".repeat(3_900_000)))
        .await;
    writer
        .type_paragraph(&format!("big-1-{}", "x".repeat(1_500_000)))
        .await;
    writer.type_paragraph("<sentinel>").await;
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer.doc_text().contains("big-1-"))
            .await,
        "the overdrawing edit never arrived"
    );
    assert!(
        !reader.doc_text().contains("<sentinel>"),
        "the read loop is not waiting off a debt, so this tests no wait"
    );

    let revoked_at = Instant::now();
    fx.state
        .rooms_v2
        .as_ref()
        .unwrap()
        .revoke_all_for_doc(fx.doc_id)
        .await;
    assert_eq!(writer.close_code_within(Duration::from_secs(2)).await, 4403);
    drop(writer);

    // The session ended, so its socket's place is free: another page opens
    // (a different target, so the upgrade bucket isn't in the way).
    let other_doc = fx.another_doc("Elsewhere").await;
    let other_path = format!("/collab/doc/{other_doc}");
    loop {
        match dial(&fx, &other_path, &fx.writer).await {
            Ok(_) => break,
            Err(_) if revoked_at.elapsed() < Duration::from_secs(2) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!(
                "the revoked session still held its socket place after {:?}: {error}",
                revoked_at.elapsed()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Presence and SyncStep1
// ---------------------------------------------------------------------------

/// Presence past its budget is coalesced: fewer frames reach the other
/// client than were sent, and the last one is the newest state.
#[tokio::test(flavor = "multi_thread")]
async fn awareness_past_budget_is_coalesced_and_the_last_state_arrives() {
    // Presence: 2.4 frames, then 1.2 per second.
    let fx = fixture(limited_config(0.01, 64)).await;
    let path = fx.doc_path();
    let mut reader = Peer::join(&fx, &path, &fx.reader).await;
    let mut writer = Peer::join(&fx, &path, &fx.writer).await;

    let sent_count = 30;
    let marker = |index: usize| format!("cursor-{index:02}");
    for index in 0..sent_count {
        writer.send_frame(awareness_frame(&marker(index))).await;
    }
    let newest = awareness_frame(&marker(sent_count - 1));
    let arrived = reader
        .pump_until(DEADLINE, |peer| peer.awareness_received.contains(&newest))
        .await;
    assert!(arrived, "the newest presence state never arrived");
    let from_writer: Vec<&Vec<u8>> = reader
        .awareness_received
        .iter()
        .filter(|frame| frame.windows(7).any(|window| window == b"cursor-"))
        .collect();
    assert!(
        from_writer.len() < sent_count,
        "all {sent_count} presence frames were relayed: nothing was coalesced"
    );
    assert_eq!(
        from_writer.last().copied(),
        Some(&newest),
        "the last presence frame relayed is the newest state"
    );
}

/// The server sends the whole state on join; a client asks with one
/// `SyncStep1` per connection. With limits on, a second one goes
/// unanswered.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_sync_step1_gets_no_reply() {
    let fx = fixture(limited_config(1.0, 64)).await;
    assert_eq!(second_sync_step1_replies(&fx).await, 0);
}

/// With limits off, every `SyncStep1` is answered, as before (the
/// `offline_sync` tests use that as a barrier).
#[tokio::test(flavor = "multi_thread")]
async fn a_second_sync_step1_is_answered_with_limits_off() {
    let fx = fixture(Config {
        rate_limit_enabled: false,
        ..Default::default()
    })
    .await;
    assert_eq!(second_sync_step1_replies(&fx).await, 1);
}

/// How many SyncStep2 replies a second client `SyncStep1` gets.
async fn second_sync_step1_replies(fx: &Fixture) -> usize {
    let path = fx.doc_path();
    let mut reader = Peer::join(fx, &path, &fx.reader).await;
    let mut writer = Peer::join(fx, &path, &fx.writer).await;
    // The state sent on join, and the answer to the first SyncStep1.
    assert!(
        writer
            .pump_until(DEADLINE, |peer| peer.step2_received >= 2)
            .await,
        "the first SyncStep1 was not answered"
    );
    let answered_before = writer.step2_received;

    let state_vector = writer.doc.transact().state_vector().encode_v1();
    writer.send_frame(sync_frame(0, &state_vector)).await;
    // The read loop handles frames in order and answers a SyncStep1 before
    // it reads on, so once this edit reaches the reader, a reply would be
    // queued for the writer already.
    writer.type_paragraph("barrier").await;
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer.doc_text().contains("barrier"))
            .await,
        "the edit after the second SyncStep1 never arrived"
    );
    writer
        .pump_until(Duration::from_secs(1), |peer| {
            peer.step2_received > answered_before
        })
        .await;
    writer.step2_received - answered_before
}

// ---------------------------------------------------------------------------
// Upgrades
// ---------------------------------------------------------------------------

/// The 65th open socket of one user is refused with 429, whatever page it
/// is for; another user is unaffected, and a closed socket frees its place.
#[tokio::test(flavor = "multi_thread")]
async fn the_65th_socket_is_refused_with_429() {
    let fx = fixture(limited_config(1.0, 64)).await;
    let mut open_sockets = Vec::new();
    for index in 0..64 {
        let doc_id = fx.another_doc(&format!("Page {index}")).await;
        let path = format!("/collab/doc/{doc_id}");
        open_sockets.push(Peer::join(&fx, &path, &fx.writer).await);
    }
    let one_more_doc = fx.another_doc("One more").await;
    let one_more_path = format!("/collab/doc/{one_more_doc}");
    let refused = refused_upgrade(&fx, &one_more_path, &fx.writer).await;
    assert_eq!(refused.status(), 429);
    assert!(
        refused.headers().get("retry-after").is_some(),
        "a refused upgrade says when to retry"
    );
    let board = fx
        .boards
        .create(fx.doc_id, fx.writer.user_id, None)
        .await
        .unwrap();
    assert_eq!(
        refused_upgrade(&fx, &format!("/collab/board/{}", board.id), &fx.writer)
            .await
            .status(),
        429,
        "board sockets count against the same cap"
    );
    Peer::join(&fx, &one_more_path, &fx.reader).await;

    drop(open_sockets.pop());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match dial(&fx, &one_more_path, &fx.writer).await {
            Ok(_) => break,
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => panic!("a closed socket never freed its place: {error}"),
        }
    }
}

/// A socket reconnecting to one page in a loop is refused with 429 after
/// 20 upgrades; opening another page is not limited.
#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_loop_to_one_doc_is_refused_with_429() {
    let fx = fixture(limited_config(1.0, 64)).await;
    let path = fx.doc_path();
    for _ in 0..20 {
        drop(dial(&fx, &path, &fx.writer).await.expect("ws connect"));
    }
    let refused = refused_upgrade(&fx, &path, &fx.writer).await;
    assert_eq!(refused.status(), 429);
    let retry_after: u64 = refused.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (1..=3).contains(&retry_after),
        "one upgrade is back within 3 s"
    );

    let other_doc = fx.another_doc("Skimmed").await;
    Peer::join(&fx, &format!("/collab/doc/{other_doc}"), &fx.writer).await;
    Peer::join(&fx, &path, &fx.reader).await;
}
