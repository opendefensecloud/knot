//! Rooms registry. One `RoomHandle` per active doc.
//!
//! Acquire is in-flight-dedup safe: concurrent acquire calls for the same
//! doc cooperate so only one room boots.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::bus::Bus;
use crate::engine::Engine;
use crate::room::{Room, RoomHandle};
use crate::snapshot::SnapshotPolicy;
use knot_storage::{SnapshotStore, UpdatesStore};

pub struct Rooms {
    map: DashMap<Uuid, Arc<RoomHandle>>,
    inflight: DashMap<Uuid, Arc<Mutex<()>>>,
    engine: Arc<dyn Engine>,
    bus: Arc<dyn Bus>,
    updates: Arc<dyn UpdatesStore>,
    snapshots: Arc<dyn SnapshotStore>,
    policy: SnapshotPolicy,
    idle_evict: Duration,
    /// Optional notifier fired with the room's `doc_id` after each
    /// successful persist. Consumers (the reindex worker in
    /// knot-server) debounce + reindex tasks. `try_send` is used so
    /// a full channel never stalls the room actor; a dropped
    /// notification is fine because the next applied update will
    /// re-fire it.
    dirty_tx: Option<mpsc::Sender<Uuid>>,
}

impl Rooms {
    pub fn new(
        engine: Arc<dyn Engine>,
        bus: Arc<dyn Bus>,
        updates: Arc<dyn UpdatesStore>,
        snapshots: Arc<dyn SnapshotStore>,
        policy: SnapshotPolicy,
        idle_evict: Duration,
    ) -> Self {
        Self {
            map: DashMap::new(),
            inflight: DashMap::new(),
            engine,
            bus,
            updates,
            snapshots,
            policy,
            idle_evict,
            dirty_tx: None,
        }
    }

    /// Builder: attach a notifier that receives this room's `doc_id`
    /// every time the writer reports a successful persist. The server
    /// owns the receiver and runs the reindex worker.
    pub fn with_dirty_tx(mut self, tx: mpsc::Sender<Uuid>) -> Self {
        self.dirty_tx = Some(tx);
        self
    }

    /// Acquire (or boot) the room for `doc_id`. Concurrent calls cooperate
    /// via a per-doc Mutex so only one room is booted.
    pub async fn acquire(&self, doc_id: Uuid) -> Arc<RoomHandle> {
        if let Some(h) = self.map.get(&doc_id) {
            return h.clone();
        }
        let guard = self
            .inflight
            .entry(doc_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _lock = guard.lock().await;
        if let Some(h) = self.map.get(&doc_id) {
            return h.clone();
        }
        let sub = self.bus.subscribe(doc_id).await.expect("bus subscribe");
        let h = Room::spawn(
            doc_id,
            self.engine.clone(),
            self.bus.clone(),
            sub,
            self.updates.clone(),
            self.snapshots.clone(),
            self.policy,
            self.dirty_tx.clone(),
        )
        .await
        .expect("hydrate");
        let arc = Arc::new(h);
        self.map.insert(doc_id, arc.clone());
        metrics::gauge!("knot_room_active").increment(1.0);
        arc
    }

    /// Send a revoke event to the room (if active). The room drops all
    /// conns; the WS shim's writer detects the closed channel and emits
    /// a 4403 close frame to each client.
    pub async fn revoke_all_for_doc(&self, doc_id: Uuid) {
        if let Some(h) = self.map.get(&doc_id) {
            let _ = h.tx.send(crate::room::Event::Revoke).await;
        }
    }

    /// Cancel the room's actor and unsubscribe from the bus. The caller
    /// is responsible for ordering this with any in-flight WS connections.
    pub async fn evict(&self, doc_id: Uuid) {
        if let Some((_, h)) = self.map.remove(&doc_id) {
            h.shutdown.cancel();
            metrics::gauge!("knot_room_active").decrement(1.0);
        }
        let _ = self.bus.unsubscribe(doc_id).await;
        let _ = self.idle_evict;
    }
}
