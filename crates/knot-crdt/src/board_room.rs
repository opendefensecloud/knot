//! Per-board actor. Mirrors `Room` for Excalidraw-style sub-documents but
//! without markdown-cache concerns or snapshot scheduler.
//!
//! v0.1 persistence: the actor calls `BoardStore::append_update` inline
//! when a y-update is applied. Hydration replays the latest snapshot then
//! the update tail. No automatic snapshotting yet — boards are typically
//! small enough that replay is cheap.
//!
//! Cross-pod convergence: the actor holds a `Bus` subscription and replays
//! updates from the shared log whenever a peer signals a new seq via the bus.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::bus::Bus;
use crate::engine::{ApplyMeasured, DocHandle, Engine, EngineError};

use crate::protocol::wrap_sync_update;
pub use crate::room::JoinState;

pub type ConnId = Uuid;

pub struct InMsg {
    pub from: ConnId,
    pub bytes: Vec<u8>,
}

pub struct ConnHandle {
    pub tx: mpsc::Sender<Vec<u8>>,
    /// Cancelled by the board room when it refuses an update the
    /// connection sent while the room holds it, as `room::ConnHandle::refused`
    /// is; the WS shim then closes the socket with 4422. The board room
    /// ignores everything the connection sends after that (a re-join, an
    /// update, presence) and sends it nothing more
    /// (`BoardRoom::refuse_sender`). A connection the room no longer holds
    /// (dropped as a slow consumer) has no handle there to cancel: each bad
    /// update it sends is still refused, but its socket stays open.
    pub refused: CancellationToken,
}

impl ConnHandle {
    pub fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx,
            refused: CancellationToken::new(),
        }
    }
}

pub enum Event {
    Inbound(InMsg),
    Join {
        conn_id: ConnId,
        handle: ConnHandle,
        reply: oneshot::Sender<Result<JoinState, EngineError>>,
    },
    Leave(ConnId),
    AwarenessIn {
        from: ConnId,
        payload: Vec<u8>,
    },
    Shutdown,
}

pub struct BoardRoom {
    board_id: Uuid,
    engine: Arc<dyn Engine>,
    doc: DocHandle,
    conns: HashMap<ConnId, ConnHandle>,
    shutdown: CancellationToken,
    rx: mpsc::Receiver<Event>,
    store: Arc<dyn knot_storage::BoardStore>,
    bus: Arc<dyn Bus>,
    bus_updates_rx: mpsc::Receiver<i64>,
    bus_presence_rx: mpsc::Receiver<Vec<u8>>,
    last_applied_seq: i64,
}

pub struct BoardRoomHandle {
    pub tx: mpsc::Sender<Event>,
    /// The board room's lifetime, as `RoomHandle::shutdown`: the room
    /// cancels it when it evicts itself, and its actor whenever it ends;
    /// the registry then boots a fresh room on the next join, and every WS
    /// shim of the room closes its socket so the client reconnects to it.
    pub shutdown: CancellationToken,
}

impl BoardRoom {
    pub async fn spawn(
        board_id: Uuid,
        engine: Arc<dyn Engine>,
        store: Arc<dyn knot_storage::BoardStore>,
        bus: Arc<dyn Bus>,
        subscription: crate::bus::Subscription,
    ) -> Result<BoardRoomHandle, EngineError> {
        let doc = engine.new_doc();

        // Hydrate from latest snapshot then replay updates after it.
        let mut hydrated_from_snapshot = false;
        if let Ok(Some((_seq, state))) = store.latest_snapshot(board_id).await {
            engine.apply_update(&doc, &state)?;
            hydrated_from_snapshot = true;
        }
        let (mut applied, mut failed, mut loaded) = (0usize, 0usize, 0usize);
        if let Ok(updates) = store.load_updates(board_id).await {
            loaded = updates.len();
            for u in updates {
                match engine.apply_update(&doc, &u) {
                    Ok(_) => applied += 1,
                    Err(e) => {
                        failed += 1;
                        tracing::warn!(error=?e, "board hydrate apply_update failed");
                    }
                }
            }
        }
        let element_count = {
            use yrs::{Map, ReadTxn, Transact};
            let txn = doc.inner().transact();
            txn.get_map("elements").map(|m| m.len(&txn)).unwrap_or(0)
        };
        tracing::info!(
            %board_id, hydrated_from_snapshot, loaded, applied, failed, element_count,
            "board room hydrated"
        );

        let last_applied_seq = store.max_update_seq(board_id).await.unwrap_or(0);

        let (tx, rx) = mpsc::channel::<Event>(256);
        let shutdown = CancellationToken::new();
        let room = Self {
            board_id,
            engine,
            doc,
            conns: HashMap::new(),
            shutdown: shutdown.clone(),
            rx,
            store,
            bus,
            bus_updates_rx: subscription.updates,
            bus_presence_rx: subscription.presence,
            last_applied_seq,
        };
        tokio::spawn(room.run());
        Ok(BoardRoomHandle { tx, shutdown })
    }

    #[tracing::instrument(skip_all, fields(board_id = %self.board_id))]
    async fn run(mut self) {
        // However the actor ends, cancel the room's token on the way out, as
        // `Room::run` does (see `BoardRoomHandle::shutdown`).
        let _ended = self.shutdown.clone().drop_guard();
        // Periodic catch-up: Postgres NOTIFY is best-effort and the PgBus
        // channel drops on overflow/reconnect, so a missed seq would otherwise
        // leave this pod diverged until the next local edit. Re-sweep the log
        // from the watermark on a timer, mirroring `Room`.
        let mut catchup_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                msg = self.rx.recv() => match msg {
                    Some(Event::Inbound(m)) => self.on_inbound(m).await,
                    Some(Event::Join { conn_id, handle, reply }) => {
                        self.on_join(conn_id, handle, reply).await;
                    }
                    Some(Event::Leave(c)) => {
                        self.conns.remove(&c);
                    }
                    Some(Event::AwarenessIn { from, payload }) => {
                        // Match the document Room's cap (4KB) so a frame that fans
                        // out locally also fits under PgBus's cross-pod size limit;
                        // otherwise large presence would silently fail across pods.
                        if crate::presence::is_oversize(&payload) { continue; }
                        if self.is_refused(&from) { continue; }
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in self.served() {
                            if *cid == from { continue; }
                            if conn.tx.try_send(payload.clone()).is_err() {
                                to_close.push(*cid);
                            }
                        }
                        for cid in to_close { self.conns.remove(&cid); }
                        // Bus fan-out to other pods — payload is still owned
                        // (the loop above clones per conn, not a move).
                        let _ = self.bus.publish_presence(self.board_id, payload).await;
                    }
                    Some(Event::Shutdown) | None => break,
                },
                // A peer signalled a new seq; pull whatever we're missing.
                Some(_seq) = self.bus_updates_rx.recv() => {
                    self.replay_since_watermark().await;
                }
                _ = catchup_tick.tick() => {
                    self.replay_since_watermark().await;
                }
                Some(payload) = self.bus_presence_rx.recv() => {
                    let mut to_close: Vec<ConnId> = Vec::new();
                    for (cid, conn) in self.served() {
                        if conn.tx.try_send(payload.clone()).is_err() { to_close.push(*cid); }
                    }
                    for cid in to_close { self.conns.remove(&cid); }
                }
            }
        }
    }

    /// Apply any board updates persisted after our watermark — from a peer's
    /// bus notification or the periodic catch-up tick — and fan them out to
    /// local connections. Remote updates are already persisted, so they are
    /// NOT re-appended. The watermark advances only on a successful apply, so a
    /// transient failure is retried rather than silently skipped.
    async fn replay_since_watermark(&mut self) {
        let updates = match self.store.since(self.board_id, self.last_applied_seq).await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(error=?e, "board catch-up replay failed");
                return;
            }
        };
        for (seq, bytes) in updates {
            if seq <= self.last_applied_seq {
                continue;
            }
            if self.engine.apply_update(&self.doc, &bytes).is_ok() {
                let framed = wrap_sync_update(&bytes);
                let mut to_close: Vec<ConnId> = Vec::new();
                for (cid, conn) in self.served() {
                    if conn.tx.try_send(framed.clone()).is_err() {
                        to_close.push(*cid);
                    }
                }
                for cid in to_close {
                    self.conns.remove(&cid);
                }
                self.last_applied_seq = seq;
            }
        }
    }

    async fn on_join(
        &mut self,
        conn_id: ConnId,
        handle: ConnHandle,
        reply: oneshot::Sender<Result<JoinState, EngineError>>,
    ) {
        // A re-join (client SyncStep1) queued behind an update the room
        // refused still carries the connection's cancelled handle. Refuse it
        // by dropping `reply`, as `Room::on_join` does.
        if handle.refused.is_cancelled() {
            return;
        }
        self.conns.insert(conn_id, handle);
        let _ = reply.send(JoinState::of(self.engine.as_ref(), &self.doc));
    }

    #[tracing::instrument(skip(self, m), fields(board_id = %self.board_id, bytes = m.bytes.len()))]
    async fn on_inbound(&mut self, m: InMsg) {
        // A board room takes updates from any connection, except one it
        // refused: that one can keep sending until its socket is torn down.
        if self.is_refused(&m.from) {
            tracing::debug!(conn = %m.from, "dropping a board update from a refused connection");
            return;
        }
        // The same rules as `Room::on_inbound`: nothing of an update that
        // nests too deep, that doesn't decode, or that yrs rejects part-way
        // is persisted or fanned out. The board editor never nests types,
        // so the nesting checks only ever fire for a crafted or broken
        // update.
        //
        // Prefilter: refuse, before applying, an update `check_update`
        // predicts nests too deep. Best-effort only: it does not guarantee
        // that applying an update stays within the worker's stack (see
        // `crate::nesting` for what is not guarded).
        if let Err(too_deep) = crate::nesting::check_update(&self.doc, &m.bytes) {
            tracing::warn!(
                board_id = %self.board_id,
                conn = %m.from,
                depth = too_deep.depth,
                limit = crate::nesting::MAX_NESTING,
                "refused a board update that nests too deep (prefilter); closing its connection"
            );
            self.refuse_sender(m.from);
            return;
        }
        // Authoritative post-apply check, as in `Room::on_inbound`.
        match self
            .engine
            .apply_update_measuring(&self.doc, &m.bytes, crate::nesting::MAX_NESTING)
        {
            Ok(applied) if applied.max_depth > crate::nesting::MAX_NESTING => {
                self.refuse_applied(
                    m.from,
                    format_args!(
                        "it nested a type {} deep, past the limit of {}",
                        applied.max_depth,
                        crate::nesting::MAX_NESTING
                    ),
                );
                return;
            }
            // Rejected part-way within the limit: as on main, nothing is
            // appended or fanned out, and the room keeps every connection
            // (see `Room::on_inbound` for why the sender stays).
            Ok(ApplyMeasured {
                rejected: Some(error),
                ..
            }) => {
                tracing::debug!(
                    board_id = %self.board_id,
                    conn = %m.from,
                    %error,
                    "yrs rejected a board update part-way; neither appended nor fanned out"
                );
                return;
            }
            // Nothing new — the usual answer to the server's SyncStep1 from a
            // (re)connecting client. Don't append a `board_updates` row per
            // open or echo the no-op to every peer (see `Room::on_inbound`).
            Ok(applied) if !applied.changed => return,
            Ok(_) => {}
            // It doesn't decode: nothing of it was applied. Refuse its
            // sender and no one else.
            Err(e) => {
                tracing::warn!(
                    board_id = %self.board_id,
                    conn = %m.from,
                    error = %e,
                    "refused a board update that doesn't decode; closing its connection"
                );
                self.refuse_sender(m.from);
                return;
            }
        }
        // Persist inline and capture the seq so we can advance the watermark
        // before publishing — ensures the self-NOTIFY from our own publish
        // is a no-op when the bus_updates_rx arm fires.
        match self.store.append_update(self.board_id, &m.bytes).await {
            Ok(seq) => {
                self.last_applied_seq = seq;
                if let Err(e) = self.bus.publish(self.board_id, seq).await {
                    tracing::warn!(error=?e, "board bus publish failed");
                }
            }
            Err(e) => tracing::warn!(error=?e, "board append_update failed"),
        }
        let framed = wrap_sync_update(&m.bytes);
        let mut to_close: Vec<ConnId> = Vec::new();
        for (cid, conn) in self.served() {
            if *cid == m.from {
                continue;
            }
            if conn.tx.try_send(framed.clone()).is_err() {
                to_close.push(*cid);
            }
        }
        for cid in to_close {
            self.conns.remove(&cid);
        }
    }

    /// Refuse an update `from` sent, as `Room::refuse_sender` does: count
    /// it and, if the room holds the connection, tell its WS shim to close
    /// it with 4422. Unlike a
    /// page room, a board room takes updates from any connection, joined
    /// or not, so dropping the connection would not stop what it still
    /// sends. The room keeps its handle, with `refused` cancelled, until
    /// the connection leaves: [`Self::is_refused`] then makes it ignore
    /// everything the connection sends (a re-join, an update, presence),
    /// and [`Self::served`] leaves it out of every fan-out, so that it is
    /// never dropped as a slow or closed consumer before it leaves.
    fn refuse_sender(&mut self, from: ConnId) {
        metrics::counter!("knot_room_updates_refused_total").increment(1);
        if let Some(conn) = self.conns.get(&from) {
            conn.refused.cancel();
        }
    }

    /// Whether `from` is a connection the board room refused
    /// ([`Self::refuse_sender`]).
    fn is_refused(&self, from: &ConnId) -> bool {
        self.conns
            .get(from)
            .is_some_and(|conn| conn.refused.is_cancelled())
    }

    /// The connections the board room sends to: every one it holds except
    /// those it refused.
    fn served(&self) -> impl Iterator<Item = (&ConnId, &ConnHandle)> {
        self.conns
            .iter()
            .filter(|(_, conn)| !conn.refused.is_cancelled())
    }

    /// Refuse an inbound update the live doc already holds, all of it or
    /// the part yrs integrated before it rejected the rest, because it
    /// nested the board past the limit, as `Room::refuse_applied` does.
    /// Boards persist inline, so the caller's return means `append_update`
    /// never runs for it. The sender is closed with 4422, and the room
    /// evicts itself (cancel `shutdown`), which closes every other
    /// connection: each WS shim watches the token and closes its socket
    /// with 1012, and the client reconnects to a fresh room, rehydrated
    /// from storage, which never got the update.
    fn refuse_applied(&mut self, from: ConnId, why: std::fmt::Arguments) {
        tracing::warn!(
            board_id = %self.board_id,
            conn = %from,
            "refused a board update after applying it ({why}); closing its connection and evicting the room"
        );
        metrics::counter!("knot_room_evicted_total").increment(1);
        self.refuse_sender(from);
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemBus, YrsEngine};
    use std::sync::Arc;
    use uuid::Uuid;

    struct NoopBoardStore;

    #[async_trait::async_trait]
    impl knot_storage::BoardStore for NoopBoardStore {
        async fn create(
            &self,
            _doc_id: Uuid,
            _created_by: Uuid,
            _label: Option<String>,
        ) -> knot_storage::boards::Result<knot_storage::Board> {
            unimplemented!()
        }
        async fn get(&self, _id: Uuid) -> knot_storage::boards::Result<knot_storage::Board> {
            Err(knot_storage::boards::BoardStoreError::NotFound)
        }
        async fn list_for_doc(
            &self,
            _doc_id: Uuid,
        ) -> knot_storage::boards::Result<Vec<knot_storage::Board>> {
            Ok(vec![])
        }
        async fn delete(&self, _id: Uuid) -> knot_storage::boards::Result<()> {
            Ok(())
        }
        async fn latest_snapshot(
            &self,
            _id: Uuid,
        ) -> knot_storage::boards::Result<Option<(i64, Vec<u8>)>> {
            Ok(None)
        }
        async fn put_snapshot(
            &self,
            _id: Uuid,
            _seq: i64,
            _state: &[u8],
        ) -> knot_storage::boards::Result<()> {
            Ok(())
        }
        async fn load_updates(&self, _id: Uuid) -> knot_storage::boards::Result<Vec<Vec<u8>>> {
            Ok(vec![])
        }
        async fn append_update(
            &self,
            _id: Uuid,
            _bytes: &[u8],
        ) -> knot_storage::boards::Result<i64> {
            Ok(1)
        }
        async fn since(
            &self,
            _id: Uuid,
            _after_seq: i64,
        ) -> knot_storage::boards::Result<Vec<(i64, Vec<u8>)>> {
            Ok(vec![])
        }
        async fn max_update_seq(&self, _id: Uuid) -> knot_storage::boards::Result<i64> {
            Ok(0)
        }
        async fn set_svg(&self, _id: Uuid, _bytes: &[u8]) -> knot_storage::boards::Result<()> {
            Ok(())
        }
        async fn get_svg(&self, _id: Uuid) -> knot_storage::boards::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn board_room_spawns_and_shuts_down() {
        let board_id = Uuid::new_v4();
        let bus = Arc::new(MemBus::new());
        let sub = bus.subscribe(board_id).await.unwrap();
        let store: Arc<dyn knot_storage::BoardStore> = Arc::new(NoopBoardStore);
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let h = BoardRoom::spawn(board_id, engine, store, bus, sub)
            .await
            .unwrap();
        h.shutdown.cancel();
    }

    /// An update nesting a board's types deeper than a room accepts is
    /// neither applied nor fanned out (`crate::nesting`), and its sender is
    /// told to close; the next one, from another connection, is applied.
    #[tokio::test(flavor = "multi_thread")]
    async fn board_room_refuses_a_deeply_nested_update() {
        use yrs::{Map, MapPrelim, ReadTxn, StateVector, Transact};

        let board_id = Uuid::new_v4();
        let bus = Arc::new(MemBus::new());
        let sub = bus.subscribe(board_id).await.unwrap();
        let store: Arc<dyn knot_storage::BoardStore> = Arc::new(NoopBoardStore);
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let h = BoardRoom::spawn(board_id, engine, store, bus, sub)
            .await
            .unwrap();
        let mut conns = Vec::new();
        for _ in 0..3 {
            let conn_id = Uuid::new_v4();
            let (tx, rx) = mpsc::channel(64);
            let handle = ConnHandle::new(tx);
            let refused = handle.refused.clone();
            let (reply, joined) = oneshot::channel();
            h.tx.send(Event::Join {
                conn_id,
                handle,
                reply,
            })
            .await
            .unwrap();
            joined.await.unwrap().unwrap();
            conns.push((conn_id, rx, refused));
        }
        let update = |depth: usize| {
            let doc = yrs::Doc::new();
            let map = doc.get_or_insert_map("elements");
            {
                let mut txn = doc.transact_mut();
                let mut inner = map.insert(&mut txn, "k", MapPrelim::default());
                for _ in 1..depth {
                    inner = inner.insert(&mut txn, "k", MapPrelim::default());
                }
            }
            doc.transact()
                .encode_state_as_update_v1(&StateVector::default())
        };
        let (sender, writer) = (conns[0].0, conns[2].0);
        let shallow = update(1);
        for (from, bytes) in [
            (sender, update(crate::nesting::MAX_NESTING + 1)),
            (writer, shallow.clone()),
        ] {
            h.tx.send(Event::Inbound(InMsg { from, bytes }))
                .await
                .unwrap();
        }
        let fanned = tokio::time::timeout(std::time::Duration::from_secs(10), conns[1].1.recv())
            .await
            .expect("the room stopped fanning out")
            .expect("peer channel closed");
        assert_eq!(
            fanned,
            wrap_sync_update(&shallow),
            "the refused update was fanned out"
        );
        assert!(
            conns[0].2.is_cancelled(),
            "the sender was not told to close"
        );
        assert!(!conns[1].2.is_cancelled(), "the peer was told to close");
        h.shutdown.cancel();
    }

    /// As for `Room`: however the actor ends, its token is cancelled, so the
    /// WS shims close their sockets and the registry boots a fresh room.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_board_room_whose_actor_ends_cancels_its_token() {
        let board_id = Uuid::new_v4();
        let bus = Arc::new(MemBus::new());
        let sub = bus.subscribe(board_id).await.unwrap();
        let store: Arc<dyn knot_storage::BoardStore> = Arc::new(NoopBoardStore);
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let h = BoardRoom::spawn(board_id, engine, store, bus, sub)
            .await
            .unwrap();
        h.tx.send(Event::Shutdown).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
            .await
            .expect("the board room's token was not cancelled when its actor ended");
    }
}
