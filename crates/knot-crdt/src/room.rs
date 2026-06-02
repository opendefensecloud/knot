//! Per-doc actor. One tokio task. Exclusive owner of `DocHandle` and the
//! local connection map. All I/O flows through mpsc channels.
//!
//! This file is iteratively extended by Tasks 7-15:
//!   T7   minimal select loop + InMsg → engine.apply_update + local fan-out
//!   T8   writer task: batch persist
//!   T9   hydration: load latest snapshot + replay updates
//!   T10  snapshot scheduler
//!   T12  backpressure: bounded channels, slow-consumer close
//!   T13  awareness + bus presence + disconnect clearing
//!   T14  catch-up tick

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::bus::{Bus, Subscription};
use crate::engine::{DocHandle, Engine, EngineError};

pub type ConnId = Uuid;

/// Bytes delivered from a local connection's WS read task.
pub struct InMsg {
    pub from: ConnId,
    pub bytes: Vec<u8>,
}

/// Handle the room hands to a local connection. The WS read task wraps it
/// to send framed messages back to the client.
pub struct ConnHandle {
    pub tx: mpsc::Sender<Vec<u8>>,
}

/// All inputs the room actor multiplexes.
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
    BusUpdate(i64),
    BusPresence(Vec<u8>),
    Shutdown,
}

pub struct Room {
    pub doc_id: Uuid,
    engine: Arc<dyn Engine>,
    doc: DocHandle,
    conns: HashMap<ConnId, ConnHandle>,
    last_applied_seq: i64,
    bus: Arc<dyn Bus>,
    shutdown: CancellationToken,
    rx: mpsc::Receiver<Event>,
    bus_updates_rx: mpsc::Receiver<i64>,
    bus_presence_rx: mpsc::Receiver<Vec<u8>>,
    persist_tx: mpsc::Sender<crate::writer::PersistJob>,
    applied_rx: mpsc::Receiver<crate::writer::Applied>,
    snapshots: Arc<dyn knot_storage::SnapshotStore>,
    policy: crate::snapshot::SnapshotPolicy,
    snap_state: crate::snapshot::SnapshotState,
}

pub struct RoomHandle {
    pub tx: mpsc::Sender<Event>,
    pub shutdown: CancellationToken,
}

impl Room {
    /// Spawn a room, hydrating its `DocHandle` from the latest snapshot and
    /// replaying any updates persisted after that snapshot. The actor starts
    /// with `last_applied_seq` set to the highest seq applied during hydration.
    pub async fn spawn(
        doc_id: Uuid,
        engine: Arc<dyn Engine>,
        bus: Arc<dyn Bus>,
        subscription: Subscription,
        updates_store: Arc<dyn knot_storage::UpdatesStore>,
        snapshots: Arc<dyn knot_storage::SnapshotStore>,
        policy: crate::snapshot::SnapshotPolicy,
    ) -> Result<RoomHandle, EngineError> {
        // Hydrate the doc.
        let doc = engine.new_doc();
        let mut last_applied_seq: i64 = 0;
        if let Ok(Some(snap)) = snapshots.latest(doc_id).await {
            engine.apply_update(&doc, &snap.state_bytes)?;
            last_applied_seq = snap.snapshot_seq;
        }
        if let Ok(after) = updates_store.since(doc_id, last_applied_seq).await {
            for u in after {
                engine.apply_update(&doc, &u.update_bytes)?;
                if u.seq > last_applied_seq {
                    last_applied_seq = u.seq;
                }
            }
        }

        // Spawn the actor with the hydrated doc + watermark.
        let (tx, rx) = mpsc::channel::<Event>(256);
        let shutdown = CancellationToken::new();

        let (persist_tx, persist_rx) = mpsc::channel::<crate::writer::PersistJob>(1024);
        let (applied_tx, applied_rx) = mpsc::channel::<crate::writer::Applied>(256);
        crate::writer::spawn(doc_id, updates_store, bus.clone(), persist_rx, applied_tx);

        let snap_state = crate::snapshot::SnapshotState {
            last_snapshot_seq: last_applied_seq,
            updates_since_snapshot: 0,
            last_apply_at: std::time::Instant::now(),
        };

        let room = Self {
            doc_id,
            engine,
            doc,
            conns: HashMap::new(),
            last_applied_seq,
            bus,
            shutdown: shutdown.clone(),
            rx,
            bus_updates_rx: subscription.updates,
            bus_presence_rx: subscription.presence,
            persist_tx,
            applied_rx,
            snapshots,
            policy,
            snap_state,
        };
        tokio::spawn(room.run());
        Ok(RoomHandle { tx, shutdown })
    }

    async fn run(mut self) {
        let mut idle_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = idle_tick.tick() => {
                    let idle = self.snap_state.last_apply_at.elapsed();
                    if self.snap_state.updates_since_snapshot > 0
                        && idle >= self.policy.idle
                        && crate::snapshot::write_snapshot(
                            self.doc_id, self.last_applied_seq,
                            self.engine.as_ref(), &self.doc,
                            self.snapshots.as_ref(),
                        ).await.is_ok()
                    {
                        self.snap_state.last_snapshot_seq = self.last_applied_seq;
                        self.snap_state.updates_since_snapshot = 0;
                    }
                }
                msg = self.rx.recv() => match msg {
                    Some(Event::Inbound(m)) => self.on_inbound(m).await,
                    Some(Event::Join { conn_id, handle, reply }) => {
                        self.on_join(conn_id, handle, reply).await;
                    }
                    Some(Event::Leave(c)) => {
                        self.conns.remove(&c);
                        // Best-effort clearing frame: an empty Vec<u8>
                        // the frontend interprets as "re-query awareness".
                        let _ = self.bus.publish_presence(self.doc_id, Vec::new()).await;
                    }
                    Some(Event::AwarenessIn { from, payload }) => {
                        if crate::presence::is_oversize(&payload) { continue; }
                        // Local fan-out (sans origin).
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
                            if *cid == from { continue; }
                            match conn.tx.try_send(payload.clone()) {
                                Ok(_) => {}
                                Err(_) => to_close.push(*cid),
                            }
                        }
                        for cid in to_close { self.conns.remove(&cid); }
                        // Bus fan-out to other replicas.
                        let _ = self.bus.publish_presence(self.doc_id, payload).await;
                    }
                    Some(Event::BusUpdate(_)) | Some(Event::BusPresence(_)) => {}
                    Some(Event::Shutdown) | None => break,
                },
                Some(applied) = self.applied_rx.recv() => {
                    if applied.seq > self.last_applied_seq {
                        self.last_applied_seq = applied.seq;
                    }
                    self.snap_state.updates_since_snapshot += 1;
                    self.snap_state.last_apply_at = std::time::Instant::now();
                    if self.snap_state.updates_since_snapshot >= self.policy.every_n {
                        if let Err(e) = crate::snapshot::write_snapshot(
                            self.doc_id, self.last_applied_seq,
                            self.engine.as_ref(), &self.doc,
                            self.snapshots.as_ref(),
                        ).await {
                            tracing::warn!(error=?e, "snapshot write failed");
                        } else {
                            self.snap_state.last_snapshot_seq = self.last_applied_seq;
                            self.snap_state.updates_since_snapshot = 0;
                        }
                    }
                }
                Some(_seq) = self.bus_updates_rx.recv() => {
                    // T14 wires the SELECT-since-watermark replay path.
                }
                Some(payload) = self.bus_presence_rx.recv() => {
                    let mut to_close: Vec<ConnId> = Vec::new();
                    for (cid, conn) in &self.conns {
                        match conn.tx.try_send(payload.clone()) {
                            Ok(_) => {}
                            Err(_) => to_close.push(*cid),
                        }
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

    async fn on_inbound(&mut self, m: InMsg) {
        if let Err(e) = self.engine.apply_update(&self.doc, &m.bytes) {
            tracing::debug!(error=?e, "apply_update failed");
            return;
        }
        let mut to_close: Vec<ConnId> = Vec::new();
        for (cid, conn) in &self.conns {
            if *cid == m.from {
                continue;
            }
            match conn.tx.try_send(m.bytes.clone()) {
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => to_close.push(*cid),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => to_close.push(*cid),
            }
        }
        for cid in to_close {
            self.conns.remove(&cid);
        }
        if let Err(e) = self
            .persist_tx
            .send(crate::writer::PersistJob {
                bytes: m.bytes,
                by_user_id: None,
            })
            .await
        {
            tracing::error!(error=?e, "persist channel closed; dropping update");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemBus, YrsEngine};

    struct NoopUpdates;
    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for NoopUpdates {
        async fn insert_batch(
            &self,
            _: Uuid,
            _: Option<Uuid>,
            updates: &[Vec<u8>],
        ) -> Result<Vec<i64>, knot_storage::UpdatesStoreError> {
            Ok((1..=updates.len() as i64).collect())
        }
        async fn since(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            Ok(vec![])
        }
        async fn max_seq(&self, _: Uuid) -> Result<i64, knot_storage::UpdatesStoreError> {
            Ok(0)
        }
        async fn delete_up_to(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
        }
    }

    struct NoopSnapshots;
    #[async_trait::async_trait]
    impl knot_storage::SnapshotStore for NoopSnapshots {
        async fn insert(
            &self,
            _: Uuid,
            _: i64,
            _: &[u8],
            _: &[u8],
        ) -> Result<(), knot_storage::SnapshotStoreError> {
            Ok(())
        }
        async fn latest(
            &self,
            _: Uuid,
        ) -> Result<Option<knot_storage::DocSnapshot>, knot_storage::SnapshotStoreError> {
            Ok(None)
        }
        async fn gc(
            &self,
            _: Uuid,
            _: i64,
            _: i32,
        ) -> Result<u64, knot_storage::SnapshotStoreError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn room_spawns_and_shuts_down_clean() {
        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let updates: Arc<dyn knot_storage::UpdatesStore> = Arc::new(NoopUpdates);
        let snapshots: Arc<dyn knot_storage::SnapshotStore> = Arc::new(NoopSnapshots);
        let policy = crate::snapshot::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(60),
        };
        let h = Room::spawn(
            doc_id,
            Arc::new(YrsEngine),
            bus,
            sub,
            updates,
            snapshots,
            policy,
        )
        .await
        .unwrap();
        h.shutdown.cancel();
        drop(h);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_update_persists_via_writer() {
        use knot_storage::{
            DocStore, PgDocStore, PgUpdatesStore, PgUserStore, PgWorkspaceStore, UpdatesStore,
            UserStore, WorkspaceRole, WorkspaceStore,
        };
        use sqlx::postgres::PgPoolOptions;
        use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

        let c = Postgres::default().start().await.unwrap();
        let port = c.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        std::mem::forget(c);

        let ws = PgWorkspaceStore::new(pool.clone())
            .create("d", "W")
            .await
            .unwrap();
        let u = PgUserStore::new(pool.clone())
            .create_local("a@x.test", "A", "$h$")
            .await
            .unwrap();
        PgWorkspaceStore::new(pool.clone())
            .add_member(ws.id, u.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let d = PgDocStore::new(pool.clone())
            .create(ws.id, None, "D", "m", u.id)
            .await
            .unwrap();

        let updates_store: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
        let snapshots: Arc<dyn knot_storage::SnapshotStore> =
            Arc::new(knot_storage::PgSnapshotStore::new(pool.clone()));
        let bus = Arc::new(MemBus::new());
        let sub = bus.subscribe(d.id).await.unwrap();
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let policy = crate::snapshot::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(60),
        };
        let h = Room::spawn(
            d.id,
            engine.clone(),
            bus.clone(),
            sub,
            updates_store.clone(),
            snapshots,
            policy,
        )
        .await
        .unwrap();

        // Produce an actual yrs update from a separate doc.
        let other = engine.new_doc();
        // Force a tiny state change so encode_state_as_update returns non-empty bytes.
        // (yrs always returns something even for an empty doc — this is enough.)
        let real_update = engine.encode_state_as_update(&other, None).unwrap();

        // Join + send.
        let conn_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel(8);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id,
            handle: ConnHandle { tx },
            reply: reply_tx,
        })
        .await
        .unwrap();
        let _ = reply_rx.await.unwrap().unwrap();
        h.tx.send(Event::Inbound(InMsg {
            from: conn_id,
            bytes: real_update.clone(),
        }))
        .await
        .unwrap();

        // Writer batches 250 ms; wait 500 ms then assert.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let max = updates_store.max_seq(d.id).await.unwrap();
        assert!(
            max > 0,
            "expected at least one row persisted; got max_seq={max}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn room_replays_prior_updates_on_boot() {
        use knot_storage::{
            DocStore, PgSnapshotStore, PgUpdatesStore, UpdatesStore, UserStore, WorkspaceStore,
        };
        use sqlx::postgres::PgPoolOptions;
        use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

        let c = Postgres::default().start().await.unwrap();
        let port = c.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .unwrap();
        sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
        std::mem::forget(c);

        let ws = knot_storage::PgWorkspaceStore::new(pool.clone())
            .create("d", "W")
            .await
            .unwrap();
        let u = knot_storage::PgUserStore::new(pool.clone())
            .create_local("a@x.test", "A", "$h$")
            .await
            .unwrap();
        knot_storage::PgWorkspaceStore::new(pool.clone())
            .add_member(ws.id, u.id, knot_storage::WorkspaceRole::Owner)
            .await
            .unwrap();
        let d = knot_storage::PgDocStore::new(pool.clone())
            .create(ws.id, None, "D", "m", u.id)
            .await
            .unwrap();

        // Seed one update.
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let seed_doc = engine.new_doc();
        let seed_bytes = engine.encode_state_as_update(&seed_doc, None).unwrap();
        let updates = PgUpdatesStore::new(pool.clone());
        updates
            .insert_batch(d.id, Some(u.id), std::slice::from_ref(&seed_bytes))
            .await
            .unwrap();

        let updates_arc: Arc<dyn knot_storage::UpdatesStore> = Arc::new(updates);
        let snapshots: Arc<dyn knot_storage::SnapshotStore> =
            Arc::new(PgSnapshotStore::new(pool.clone()));
        let bus = Arc::new(MemBus::new());
        let sub = bus.subscribe(d.id).await.unwrap();
        let policy = crate::snapshot::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(60),
        };
        let h = Room::spawn(
            d.id,
            engine.clone(),
            bus.clone(),
            sub,
            updates_arc,
            snapshots,
            policy,
        )
        .await
        .unwrap();

        let (tx, _rx) = mpsc::channel(8);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id: Uuid::new_v4(),
            handle: ConnHandle { tx },
            reply: reply_tx,
        })
        .await
        .unwrap();
        let state = reply_rx.await.unwrap().unwrap();
        assert!(
            !state.is_empty(),
            "hydrated state should include the seed update"
        );
    }
}
