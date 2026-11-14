//! The collab read loops (`room::serve`, `board_room_shim::serve`) under
//! their rate limits, over real sockets but without Postgres: the rooms run
//! on in-memory logs, and a bare router upgrades straight into the loops
//! (the upgrade's own checks, the socket cap and the upgrade bucket, are
//! covered by `collab_rate_limit_integration.rs`, which needs Postgres).
//!
//! What these pin (issue #88, spec "Collab WebSocket"): a burst past the
//! budget is read more slowly and never loses an edit; any end of the
//! session ends a throttled wait at once; presence past its budget is
//! coalesced to the newest state; a second client `SyncStep1` is left
//! unanswered with limits on, and answered with them off.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use knot_config::Config;
use knot_server::protocol::{YSyncMessage, decode};
use knot_server::rate_limit::RateLimits;
use knot_storage::{
    Board, BoardStore, BoardStoreError, DocSnapshot, DocUpdate, HistoryHead, NewUpdate, Persisted,
    RowsAfter, SnapshotMeta, SnapshotStamp, SnapshotStore, SnapshotStoreError, UpdatesStore,
    UpdatesStoreError,
};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, GetString, Map, MapRef, ReadTxn, StateVector, Transact, TransactionMut, Update,
    XmlElementPrelim, XmlFragment, XmlFragmentRef, XmlTextPrelim,
};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const DEADLINE: Duration = Duration::from_secs(30);

/// Every budget at its floor: bytes 4 MiB then 64 KiB/s, messages one then
/// 20/s, presence one then one a minute.
const FLOOR_SCALE: f64 = 0.0001;

fn limits(scale: f64) -> Arc<RateLimits> {
    Arc::new(RateLimits::from_config(&Config {
        rate_limit_enabled: true,
        rate_limit_scale: scale,
        ..Default::default()
    }))
}

// ---------------------------------------------------------------------------
// In-memory logs
// ---------------------------------------------------------------------------

/// A page log and a board log in memory: enough for rooms to hydrate,
/// persist and catch up.
#[derive(Default)]
struct MemoryLogs {
    doc_rows: Mutex<HashMap<Uuid, Vec<DocUpdate>>>,
    board_rows: Mutex<HashMap<Uuid, Vec<Vec<u8>>>>,
}

impl MemoryLogs {
    fn doc_updates(&self, doc_id: Uuid) -> Vec<Vec<u8>> {
        self.doc_rows
            .lock()
            .unwrap()
            .get(&doc_id)
            .map(|rows| rows.iter().map(|row| row.update_bytes.clone()).collect())
            .unwrap_or_default()
    }

    fn board_updates(&self, board_id: Uuid) -> Vec<Vec<u8>> {
        self.board_rows
            .lock()
            .unwrap()
            .get(&board_id)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl UpdatesStore for MemoryLogs {
    async fn append(
        &self,
        doc_id: Uuid,
        epoch: i32,
        rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError> {
        let mut by_doc = self.doc_rows.lock().unwrap();
        let log = by_doc.entry(doc_id).or_default();
        let mut persisted = Vec::new();
        for row in rows {
            let next = log.len() as i64 + 1;
            log.push(DocUpdate {
                seq: next,
                doc_id,
                update_bytes: row.bytes.clone(),
                by_user_id: row.by_user_id,
                created_at: chrono::Utc::now(),
                rev: Some(next),
                source: Some(row.source),
                meta: row.meta.clone(),
                epoch: Some(epoch),
            });
            persisted.push(Persisted {
                seq: next,
                rev: next,
            });
        }
        Ok(persisted)
    }

    async fn rows_after(
        &self,
        doc_id: Uuid,
        from: RowsAfter,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        let after_rev = match from {
            RowsAfter::Rev(rev) => rev,
            RowsAfter::All => 0,
        };
        Ok(self
            .doc_rows
            .lock()
            .unwrap()
            .get(&doc_id)
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.rev.unwrap_or(0) > after_rev)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn head(&self, doc_id: Uuid) -> Result<HistoryHead, UpdatesStoreError> {
        let head_rev = self
            .doc_rows
            .lock()
            .unwrap()
            .get(&doc_id)
            .map_or(0, |rows| rows.len() as i64);
        Ok(HistoryHead {
            head_rev,
            legacy_rows: 0,
            content_epoch: 0,
        })
    }

    async fn ensure_legacy_numbered(&self, _doc_id: Uuid) -> Result<u64, UpdatesStoreError> {
        Ok(0)
    }

    async fn since(
        &self,
        doc_id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        self.rows_after(doc_id, RowsAfter::Rev(after_seq)).await
    }

    async fn max_seq(&self, doc_id: Uuid) -> Result<i64, UpdatesStoreError> {
        Ok(self.head(doc_id).await?.head_rev)
    }
}

#[async_trait]
impl SnapshotStore for MemoryLogs {
    async fn insert_stamped(
        &self,
        _doc_id: Uuid,
        _stamp: SnapshotStamp,
        _state_bytes: &[u8],
        _state_vector: &[u8],
    ) -> Result<(), SnapshotStoreError> {
        Ok(())
    }

    async fn latest(&self, _doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        Ok(None)
    }

    async fn gc(
        &self,
        _doc_id: Uuid,
        _keep_recent: i64,
        _retain_days: i32,
    ) -> Result<u64, SnapshotStoreError> {
        Ok(0)
    }

    async fn list(
        &self,
        _doc_id: Uuid,
        _limit: i64,
    ) -> Result<Vec<SnapshotMeta>, SnapshotStoreError> {
        Ok(Vec::new())
    }

    async fn by_seq(
        &self,
        _doc_id: Uuid,
        _snapshot_seq: i64,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        Ok(None)
    }
}

#[async_trait]
impl BoardStore for MemoryLogs {
    async fn create(
        &self,
        _doc_id: Uuid,
        _created_by: Uuid,
        _label: Option<String>,
    ) -> Result<Board, BoardStoreError> {
        unimplemented!("boards are not created here")
    }

    async fn get(&self, _id: Uuid) -> Result<Board, BoardStoreError> {
        unimplemented!("boards are not looked up here")
    }

    async fn list_for_doc(&self, _doc_id: Uuid) -> Result<Vec<Board>, BoardStoreError> {
        Ok(Vec::new())
    }

    async fn delete(&self, _id: Uuid) -> Result<(), BoardStoreError> {
        Ok(())
    }

    async fn append_update(&self, id: Uuid, bytes: &[u8]) -> Result<i64, BoardStoreError> {
        let mut by_board = self.board_rows.lock().unwrap();
        let log = by_board.entry(id).or_default();
        log.push(bytes.to_vec());
        Ok(log.len() as i64)
    }

    async fn load_updates(&self, id: Uuid) -> Result<Vec<Vec<u8>>, BoardStoreError> {
        Ok(self.board_updates(id))
    }

    async fn since(
        &self,
        id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<(i64, Vec<u8>)>, BoardStoreError> {
        Ok(self
            .board_updates(id)
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| (index as i64 + 1, bytes))
            .filter(|(seq, _)| *seq > after_seq)
            .collect())
    }

    async fn max_update_seq(&self, id: Uuid) -> Result<i64, BoardStoreError> {
        Ok(self.board_updates(id).len() as i64)
    }

    async fn put_snapshot(
        &self,
        _id: Uuid,
        _seq: i64,
        _state: &[u8],
    ) -> Result<(), BoardStoreError> {
        Ok(())
    }

    async fn latest_snapshot(&self, _id: Uuid) -> Result<Option<(i64, Vec<u8>)>, BoardStoreError> {
        Ok(None)
    }

    async fn set_svg(&self, _id: Uuid, _bytes: &[u8]) -> Result<(), BoardStoreError> {
        Ok(())
    }

    async fn get_svg(&self, _id: Uuid) -> Result<Option<Vec<u8>>, BoardStoreError> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Server: a bare router into the read loops
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct LoopServer {
    rooms: Arc<knot_crdt::Rooms>,
    board_rooms: Arc<knot_crdt::BoardRooms>,
    rate_limits: Arc<RateLimits>,
    shutdown: CancellationToken,
    /// Sessions whose read loop has returned.
    sessions_ended: Arc<AtomicUsize>,
}

struct Fixture {
    addr: std::net::SocketAddr,
    server: LoopServer,
    logs: Arc<MemoryLogs>,
    doc_id: Uuid,
    board_id: Uuid,
}

impl Fixture {
    fn doc_path(&self, user_id: Uuid) -> String {
        format!("/doc/{}/{user_id}", self.doc_id)
    }

    fn board_path(&self, user_id: Uuid) -> String {
        format!("/board/{}/{}/{user_id}", self.board_id, self.doc_id)
    }

    fn sessions_ended(&self) -> usize {
        self.server.sessions_ended.load(Ordering::SeqCst)
    }
}

async fn doc_socket(
    Path((doc_id, user_id)): Path<(Uuid, Uuid)>,
    State(server): State<LoopServer>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    let acl_epoch_at_check = server.rooms.acl_epoch(doc_id);
    ws.max_message_size(4 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            knot_server::room::serve(
                server.rooms.clone(),
                doc_id,
                user_id,
                socket,
                true,
                acl_epoch_at_check,
                server.shutdown.clone(),
                server.rate_limits.clone(),
            )
            .await;
            server.sessions_ended.fetch_add(1, Ordering::SeqCst);
        })
        .into_response()
}

async fn board_socket(
    Path((board_id, doc_id, user_id)): Path<(Uuid, Uuid, Uuid)>,
    State(server): State<LoopServer>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    let acl_epoch_at_check = server.board_rooms.acl_epoch(doc_id);
    ws.max_message_size(4 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            knot_server::board_room_shim::serve(
                server.board_rooms.clone(),
                board_id,
                doc_id,
                user_id,
                acl_epoch_at_check,
                socket,
                server.shutdown.clone(),
                server.rate_limits.clone(),
            )
            .await;
            server.sessions_ended.fetch_add(1, Ordering::SeqCst);
        })
        .into_response()
}

async fn fixture(rate_limits: Arc<RateLimits>) -> Fixture {
    let logs = Arc::new(MemoryLogs::default());
    let engine: Arc<dyn knot_crdt::Engine> = Arc::new(knot_crdt::YrsEngine);
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::MemBus::new());
    let rooms = Arc::new(knot_crdt::Rooms::new(
        engine.clone(),
        bus.clone(),
        logs.clone(),
        logs.clone(),
        knot_crdt::SnapshotPolicy {
            every_n: 100_000,
            idle: Duration::from_secs(600),
        },
        Duration::from_secs(300),
    ));
    let board_rooms = Arc::new(knot_crdt::BoardRooms::new(engine, logs.clone(), bus));
    let server = LoopServer {
        rooms,
        board_rooms,
        rate_limits,
        shutdown: CancellationToken::new(),
        sessions_ended: Arc::default(),
    };
    let app = axum::Router::new()
        .route("/doc/{doc_id}/{user_id}", get(doc_socket))
        .route("/board/{board_id}/{doc_id}/{user_id}", get(board_socket))
        .with_state(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Fixture {
        addr,
        server,
        logs,
        doc_id: Uuid::new_v4(),
        board_id: Uuid::new_v4(),
    }
}

// ---------------------------------------------------------------------------
// Raw y-sync client, as in offline_sync.rs
// ---------------------------------------------------------------------------

fn sync_frame(subtype: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0u8, subtype];
    knot_server::protocol::append_var_uint(&mut frame, payload.len() as u64);
    frame.extend_from_slice(payload);
    frame
}

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

struct Peer {
    doc: Doc,
    ws: Ws,
    step2_received: usize,
    awareness_received: Vec<Vec<u8>>,
}

impl Peer {
    /// Connect, send SyncStep1 and wait for the room's state.
    async fn join(fx: &Fixture, path: &str) -> Peer {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{path}", fx.addr))
            .await
            .expect("ws connect");
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

fn replay(updates: &[Vec<u8>]) -> Doc {
    let doc = Doc::new();
    for update in updates {
        let update = Update::decode_v1(update).unwrap();
        doc.transact_mut().apply_update(update).unwrap();
    }
    doc
}

/// Poll the in-memory log until `done` holds for its replay.
async fn wait_for_log(what: &str, load: impl Fn() -> Vec<Vec<u8>>, done: impl Fn(&Doc) -> bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let doc = replay(&load());
        if done(&doc) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never reached the log; it holds text of {} chars, board keys {:?}",
            doc_text(&doc).len(),
            board_keys(&doc)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 4.4 MB against a 4 MiB burst refilling at 64 KiB/s: about 3 s of debt.
/// Every paragraph reaches the other client and the log; the edit after
/// the burst only once the debt is paid off.
#[tokio::test(flavor = "multi_thread")]
async fn doc_burst_past_the_byte_budget_is_applied_in_full_and_delivered_later() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let mut reader = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;

    let burst_started = Instant::now();
    let markers: Vec<String> = (0..4).map(|index| format!("burst-{index}-")).collect();
    for marker in &markers {
        let filler = "x".repeat(1_100_000 - marker.len());
        writer.type_paragraph(&format!("{marker}{filler}")).await;
    }
    writer.type_paragraph("after-the-burst").await;

    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer.doc_text().contains("after-the-burst"))
            .await,
        "the edit after the burst never reached the reader"
    );
    let delivered_after = burst_started.elapsed();
    assert!(
        delivered_after >= Duration::from_secs(2),
        "the edit after the burst arrived after {delivered_after:?}: the byte debt \
         (about 3 s) was not waited off"
    );
    let reader_text = reader.doc_text();
    for marker in &markers {
        assert!(reader_text.contains(marker), "the reader lacks {marker}");
    }
    let logs = fx.logs.clone();
    let doc_id = fx.doc_id;
    wait_for_log(
        "the burst",
        || logs.doc_updates(doc_id),
        |doc| {
            let text = doc_text(doc);
            markers.iter().all(|marker| text.contains(marker)) && text.contains("after-the-burst")
        },
    )
    .await;
}

/// The board loop: 60 element inserts against one message and 20/s all
/// arrive, after about 3 s, and all are persisted.
#[tokio::test(flavor = "multi_thread")]
async fn board_burst_past_the_message_budget_is_applied_in_full_and_delivered_later() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let mut reader = Peer::join(&fx, &fx.board_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.board_path(Uuid::new_v4())).await;

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
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer.board_keys().len() == element_count)
            .await,
        "the reader has {} of {element_count} elements",
        reader.board_keys().len()
    );
    let delivered_after = burst_started.elapsed();
    assert!(
        delivered_after >= Duration::from_secs(2),
        "the last element arrived after {delivered_after:?}: the message debt \
         (about 3 s) was not waited off"
    );
    let logs = fx.logs.clone();
    let board_id = fx.board_id;
    wait_for_log(
        "the board burst",
        || logs.board_updates(board_id),
        |doc| board_keys(doc).len() == element_count,
    )
    .await;
}

/// With limits off the same message burst is read at once.
#[tokio::test(flavor = "multi_thread")]
async fn without_limits_a_burst_is_read_at_once() {
    let fx = fixture(Arc::new(RateLimits::disabled())).await;
    let mut reader = Peer::join(&fx, &fx.board_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.board_path(Uuid::new_v4())).await;
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
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer.board_keys().len() == element_count)
            .await
    );
    let delivered_after = burst_started.elapsed();
    assert!(
        delivered_after < Duration::from_secs(2),
        "without limits the burst took {delivered_after:?}"
    );
}

/// Overdraw `writer`'s byte budget (4 MiB at the floor) by about 1.2 MB, ~19 s
/// at 64 KiB/s, then send `sentinel`, which the server reads only once the
/// debt is paid off. `insert` puts one value into the document.
async fn overdraw_bytes_then_send(
    writer: &mut Peer,
    insert: fn(&Roots, &mut TransactionMut, String),
    sentinel: &str,
) {
    for (index, value_bytes) in [3_900_000, 1_500_000].into_iter().enumerate() {
        let value = format!("big-{index}-{}", "x".repeat(value_bytes));
        writer.edit(|roots, txn| insert(roots, txn, value)).await;
    }
    let sentinel = sentinel.to_owned();
    writer.edit(|roots, txn| insert(roots, txn, sentinel)).await;
}

fn insert_paragraph(roots: &Roots, txn: &mut TransactionMut, text: String) {
    let paragraph = roots
        .fragment
        .push_back(txn, XmlElementPrelim::empty("paragraph"));
    paragraph.push_back(txn, XmlTextPrelim::new(text));
}

fn insert_element(roots: &Roots, txn: &mut TransactionMut, value: String) {
    let key = if value.starts_with("big-") {
        value[..5].to_owned()
    } else {
        value.clone()
    };
    roots.elements.insert(txn, key, value);
}

/// Wait until both sessions' read loops (the reader's and the throttled
/// writer's: a revoke, a shutdown or a room end reaches every connection)
/// have returned, at most 2 s after `since`.
async fn both_sessions_end_within_two_seconds(fx: &Fixture, since: Instant) {
    while fx.sessions_ended() < 2 {
        assert!(
            since.elapsed() < Duration::from_secs(2),
            "the throttled read loop outlived the end of its session"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Review focus 2: a revoke while the read loop waits off ~19 s of byte
/// debt closes the socket with 4403 and ends the session at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoke_during_a_wait_closes_with_4403_and_ends_the_session_at_once() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let mut reader = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    overdraw_bytes_then_send(&mut writer, insert_paragraph, "<sentinel>").await;
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
    assert_eq!(fx.sessions_ended(), 0);

    let revoked_at = Instant::now();
    fx.server.rooms.revoke_all_for_doc(fx.doc_id).await;
    assert_eq!(writer.close_code_within(Duration::from_secs(2)).await, 4403);
    both_sessions_end_within_two_seconds(&fx, revoked_at).await;
}

/// The same for a board socket.
#[tokio::test(flavor = "multi_thread")]
async fn a_board_revoke_during_a_wait_closes_with_4403_and_ends_the_session_at_once() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let mut reader = Peer::join(&fx, &fx.board_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.board_path(Uuid::new_v4())).await;
    overdraw_bytes_then_send(&mut writer, insert_element, "sentinel").await;
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer
                .board_keys()
                .contains(&"big-1".to_string()))
            .await,
        "the overdrawing edit never arrived"
    );
    let keys = reader.board_keys();
    assert!(
        !keys.contains(&"sentinel".to_string()),
        "the read loop is not waiting off a debt, so this tests no wait: {keys:?}"
    );

    let revoked_at = Instant::now();
    fx.server.board_rooms.revoke_for_doc(fx.doc_id).await;
    assert_eq!(writer.close_code_within(Duration::from_secs(2)).await, 4403);
    both_sessions_end_within_two_seconds(&fx, revoked_at).await;
}

/// A server shutdown during a wait closes with 1001 and ends the session
/// at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_during_a_wait_closes_with_1001_and_ends_the_session_at_once() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let mut reader = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    overdraw_bytes_then_send(&mut writer, insert_paragraph, "<sentinel>").await;
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
    let shut_down_at = Instant::now();
    fx.server.shutdown.cancel();
    assert_eq!(writer.close_code_within(Duration::from_secs(2)).await, 1001);
    both_sessions_end_within_two_seconds(&fx, shut_down_at).await;
}

/// A room that ends during a wait (it is evicted) closes with 1012, so the
/// client reconnects and re-sends what the fresh room lacks, and ends the
/// session at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_restart_during_a_wait_closes_with_1012_and_ends_the_session_at_once() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let mut reader = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    overdraw_bytes_then_send(&mut writer, insert_paragraph, "<sentinel>").await;
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
    let restarted_at = Instant::now();
    fx.server.rooms.restart_all().await;
    assert_eq!(writer.close_code_within(Duration::from_secs(2)).await, 1012);
    both_sessions_end_within_two_seconds(&fx, restarted_at).await;
}

/// The throttled writer vanishes (its tab closed) while frames it already
/// sent wait unread in the server's receive buffer; another client keeps
/// editing, so the server's next writes to the dead socket fail and end
/// the session during the wait. The edit it had sent still reaches the
/// room and the log, long before the debt would have been paid off.
async fn edits_already_sent_survive_a_vanished_client(
    fx: &Fixture,
    path_of: impl Fn(Uuid) -> String,
    insert: fn(&Roots, &mut TransactionMut, String),
    sentinel_arrived: impl Fn(&Doc) -> bool,
    load_log: impl Fn() -> Vec<Vec<u8>>,
) {
    let mut reader = Peer::join(fx, &path_of(Uuid::new_v4())).await;
    let mut writer = Peer::join(fx, &path_of(Uuid::new_v4())).await;
    overdraw_bytes_then_send(&mut writer, insert, "sentinel").await;
    assert!(
        reader
            .pump_until(DEADLINE, |peer| replay_has_big_1(&peer.doc))
            .await,
        "the overdrawing edit never arrived"
    );
    assert!(
        !sentinel_arrived(&reader.doc),
        "the read loop is not waiting off a debt, so this tests no wait"
    );

    let vanished_at = Instant::now();
    drop(writer);
    let mut keep_writing_index = 0;
    while fx.sessions_ended() == 0 {
        assert!(
            vanished_at.elapsed() < Duration::from_secs(10),
            "the vanished client's session did not end"
        );
        reader
            .edit(|roots, txn| insert(roots, txn, format!("keep-{keep_writing_index}")))
            .await;
        keep_writing_index += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let deadline = vanished_at + Duration::from_secs(10);
    loop {
        if sentinel_arrived(&replay(&load_log())) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the edit the vanished client had already sent never reached the log"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn replay_has_big_1(doc: &Doc) -> bool {
    doc_text(doc).contains("big-1-") || board_keys(doc).iter().any(|key| key == "big-1")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_doc_edit_already_sent_survives_a_client_that_vanishes_during_a_wait() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let logs = fx.logs.clone();
    let doc_id = fx.doc_id;
    edits_already_sent_survive_a_vanished_client(
        &fx,
        |user_id| fx.doc_path(user_id),
        insert_paragraph,
        |doc| doc_text(doc).contains("sentinel"),
        || logs.doc_updates(doc_id),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_board_edit_already_sent_survives_a_client_that_vanishes_during_a_wait() {
    let fx = fixture(limits(FLOOR_SCALE)).await;
    let logs = fx.logs.clone();
    let board_id = fx.board_id;
    edits_already_sent_survive_a_vanished_client(
        &fx,
        |user_id| fx.board_path(user_id),
        insert_element,
        |doc| board_keys(doc).iter().any(|key| key == "sentinel"),
        || logs.board_updates(board_id),
    )
    .await;
}

/// Limits with fast pings: a client that reads (and so answers pings)
/// stays, one that reads nothing for the silence limit is let go, so a
/// half-open socket doesn't hold a place under the socket cap.
fn limits_with_fast_pings() -> Arc<RateLimits> {
    let mut rate_limits = RateLimits::from_config(&Config {
        rate_limit_enabled: true,
        ..Default::default()
    });
    rate_limits.liveness = Some(knot_server::rate_limit::Liveness {
        ping_interval: Duration::from_millis(200),
        silence_limit: Duration::from_millis(1_000),
    });
    Arc::new(rate_limits)
}

async fn a_silent_client_is_let_go_and_a_live_one_kept(path_of: impl Fn(&Fixture) -> String) {
    let fx = fixture(limits_with_fast_pings()).await;
    let _silent = Peer::join(&fx, &path_of(&fx)).await;
    let mut live = Peer::join(&fx, &path_of(&fx)).await;
    // The live client reads for 3 s (answering every ping) and is never
    // closed; `pump_until` panics if the server closes its socket.
    assert!(!live.pump_until(Duration::from_secs(3), |_| false).await);
    assert_eq!(
        fx.sessions_ended(),
        1,
        "only the client that read nothing was let go"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_doc_client_is_let_go_and_a_live_one_kept() {
    a_silent_client_is_let_go_and_a_live_one_kept(|fx| fx.doc_path(Uuid::new_v4())).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_silent_board_client_is_let_go_and_a_live_one_kept() {
    a_silent_client_is_let_go_and_a_live_one_kept(|fx| fx.board_path(Uuid::new_v4())).await;
}

/// Presence past its budget is coalesced: fewer frames reach the other
/// client than were sent, and the last one relayed is the newest state.
#[tokio::test(flavor = "multi_thread")]
async fn presence_past_its_budget_is_coalesced_and_the_newest_state_arrives() {
    // Presence: 2.4 frames, then 1.2 per second.
    let fx = fixture(limits(0.01)).await;
    let mut reader = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;

    let sent_count = 30;
    let marker = |index: usize| format!("cursor-{index:02}");
    for index in 0..sent_count {
        writer.send_frame(awareness_frame(&marker(index))).await;
    }
    let newest = awareness_frame(&marker(sent_count - 1));
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer.awareness_received.contains(&newest))
            .await,
        "the newest presence state never arrived"
    );
    let from_writer: Vec<&Vec<u8>> = reader
        .awareness_received
        .iter()
        .filter(|frame| frame.windows(7).any(|window| window == b"cursor-"))
        .collect();
    assert!(
        from_writer.len() < sent_count,
        "all {sent_count} presence frames were relayed: nothing was coalesced"
    );
    assert_eq!(from_writer.last().copied(), Some(&newest));
}

/// With limits off every presence frame is relayed.
#[tokio::test(flavor = "multi_thread")]
async fn without_limits_every_presence_frame_is_relayed() {
    let fx = fixture(Arc::new(RateLimits::disabled())).await;
    let mut reader = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    let mut writer = Peer::join(&fx, &fx.doc_path(Uuid::new_v4())).await;
    let sent_count = 30;
    for index in 0..sent_count {
        writer
            .send_frame(awareness_frame(&format!("cursor-{index:02}")))
            .await;
    }
    let newest = awareness_frame(&format!("cursor-{:02}", sent_count - 1));
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer.awareness_received.contains(&newest))
            .await
    );
    let mut distinct_from_writer: Vec<&Vec<u8>> = reader
        .awareness_received
        .iter()
        .filter(|frame| frame.windows(7).any(|window| window == b"cursor-"))
        .collect();
    distinct_from_writer.sort();
    distinct_from_writer.dedup();
    assert_eq!(distinct_from_writer.len(), sent_count);
}

/// How many SyncStep2 replies a second client `SyncStep1` gets, on `path_of`.
async fn second_sync_step1_replies(fx: &Fixture, path_of: impl Fn(Uuid) -> String) -> usize {
    let mut reader = Peer::join(fx, &path_of(Uuid::new_v4())).await;
    let mut writer = Peer::join(fx, &path_of(Uuid::new_v4())).await;
    assert!(
        writer
            .pump_until(DEADLINE, |peer| peer.step2_received >= 2)
            .await,
        "the first SyncStep1 was not answered"
    );
    let answered_before = writer.step2_received;
    let state_vector = writer.doc.transact().state_vector().encode_v1();
    writer.send_frame(sync_frame(0, &state_vector)).await;
    // Frames are handled in order and a SyncStep1 is answered before the
    // next read, so once this edit reaches the reader a reply is queued.
    writer
        .edit(|roots, txn| {
            roots.elements.insert(txn, "barrier", "rect");
        })
        .await;
    assert!(
        reader
            .pump_until(DEADLINE, |peer| peer
                .board_keys()
                .contains(&"barrier".to_string()))
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

#[tokio::test(flavor = "multi_thread")]
async fn a_second_sync_step1_gets_no_reply_with_limits_on() {
    let fx = fixture(limits(1.0)).await;
    assert_eq!(
        second_sync_step1_replies(&fx, |user_id| fx.doc_path(user_id)).await,
        0,
        "doc"
    );
    assert_eq!(
        second_sync_step1_replies(&fx, |user_id| fx.board_path(user_id)).await,
        0,
        "board"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_sync_step1_is_answered_with_limits_off() {
    let fx = fixture(Arc::new(RateLimits::disabled())).await;
    assert_eq!(
        second_sync_step1_replies(&fx, |user_id| fx.doc_path(user_id)).await,
        1,
        "doc"
    );
    assert_eq!(
        second_sync_step1_replies(&fx, |user_id| fx.board_path(user_id)).await,
        1,
        "board"
    );
}
