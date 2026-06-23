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
use crate::engine::{DocHandle, Engine, EngineError};

use crate::protocol::wrap_sync_update;

pub type ConnId = Uuid;

pub struct InMsg {
    pub from: ConnId,
    pub bytes: Vec<u8>,
}

pub struct ConnHandle {
    pub tx: mpsc::Sender<Vec<u8>>,
}

pub enum Event {
    Inbound(InMsg),
    Join {
        conn_id: ConnId,
        handle: ConnHandle,
        reply: oneshot::Sender<Result<Vec<u8>, EngineError>>,
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
                        if payload.len() > 64 * 1024 { continue; }
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
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
                Some(_seq) = self.bus_updates_rx.recv() => {
                    // Remote edit on another pod: fetch new updates from the shared
                    // log and apply locally. Already persisted — do NOT re-append.
                    if let Ok(updates) = self.store.since(self.board_id, self.last_applied_seq).await {
                        for (seq, bytes) in updates {
                            if seq <= self.last_applied_seq { continue; }
                            if self.engine.apply_update(&self.doc, &bytes).is_ok() {
                                let framed = wrap_sync_update(&bytes);
                                let mut to_close: Vec<ConnId> = Vec::new();
                                for (cid, conn) in &self.conns {
                                    if conn.tx.try_send(framed.clone()).is_err() { to_close.push(*cid); }
                                }
                                for cid in to_close { self.conns.remove(&cid); }
                                // Advance the watermark only on success, mirroring Room:
                                // a transient apply failure is retried on the next notify
                                // rather than silently skipped.
                                self.last_applied_seq = seq;
                            }
                        }
                    }
                }
                Some(payload) = self.bus_presence_rx.recv() => {
                    let mut to_close: Vec<ConnId> = Vec::new();
                    for (cid, conn) in &self.conns {
                        if conn.tx.try_send(payload.clone()).is_err() { to_close.push(*cid); }
                    }
                    for cid in to_close { self.conns.remove(&cid); }
                }
            }
        }
    }

    async fn on_join(
        &mut self,
        conn_id: ConnId,
        handle: ConnHandle,
        reply: oneshot::Sender<Result<Vec<u8>, EngineError>>,
    ) {
        self.conns.insert(conn_id, handle);
        let r = self.engine.encode_state_as_update(&self.doc, None);
        let _ = reply.send(r);
    }

    #[tracing::instrument(skip(self, m), fields(board_id = %self.board_id, bytes = m.bytes.len()))]
    async fn on_inbound(&mut self, m: InMsg) {
        if let Err(e) = self.engine.apply_update(&self.doc, &m.bytes) {
            tracing::debug!(error=?e, "apply_update failed");
            return;
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
        for (cid, conn) in &self.conns {
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
}
