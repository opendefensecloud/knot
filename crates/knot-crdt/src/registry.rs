//! Rooms registry. One `RoomHandle` per active doc.
//!
//! Acquire is in-flight-dedup safe: concurrent acquire calls for the same
//! doc cooperate so only one room boots.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::bus::{Bus, BusError};
use crate::engine::{Engine, EngineError};
use crate::room::{Room, RoomHandle};
use crate::snapshot::SnapshotPolicy;
use knot_storage::{SnapshotStore, UpdatesStore};

/// Error returned by [`Rooms::acquire`] when a room cannot be booted.
#[derive(Debug, Error)]
pub enum AcquireError {
    #[error("bus subscribe: {0}")]
    Bus(#[from] BusError),
    #[error("hydrate: {0}")]
    Hydrate(#[from] EngineError),
}

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
    /// via a per-doc Mutex so only one room boots.
    ///
    /// Returns `Err` when the bus subscription or initial hydration fails
    /// (transient DB/bus blip). The inflight entry is removed after a
    /// successful insert so it does not accumulate indefinitely.
    pub async fn acquire(&self, doc_id: Uuid) -> Result<Arc<RoomHandle>, AcquireError> {
        if let Some(h) = self.live(doc_id) {
            return Ok(h);
        }
        let guard = self
            .inflight
            .entry(doc_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _lock = guard.lock().await;
        if let Some(h) = self.live(doc_id) {
            return Ok(h);
        }
        // A room that evicted itself (an update it refused after applying
        // it: too deep, [`crate::nesting`], or rejected part-way by yrs)
        // leaves a cancelled handle in the map. Drop it before booting a
        // fresh one, so this join rehydrates from storage.
        self.drop_if_dead(doc_id).await;
        let sub = self.bus.subscribe(doc_id).await?;
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
        .await?;
        let arc = Arc::new(h);
        self.map.insert(doc_id, arc.clone());
        // Prune the inflight entry now that the room is live; leaving it in
        // would cause the DashMap to grow without bound over time.
        self.inflight.remove(&doc_id);
        metrics::gauge!("knot_room_active").increment(1.0);
        Ok(arc)
    }

    /// The room for `doc_id` if one is already running here. Never boots
    /// one: rooms are not evicted when idle, so a background reader that
    /// acquired them would keep every page it read in memory.
    ///
    /// A room that evicted itself over a refused update (too deep,
    /// [`crate::nesting`], or rejected part-way by yrs) is treated as absent
    /// — its live doc holds refused content, so a reader must fall back to
    /// storage. `acquire` prunes the dead entry on the next join.
    pub fn get_active(&self, doc_id: Uuid) -> Option<Arc<RoomHandle>> {
        self.live(doc_id)
    }

    /// The running room for `doc_id`, or `None` if there is none or it has
    /// evicted itself (its `shutdown` is cancelled).
    fn live(&self, doc_id: Uuid) -> Option<Arc<RoomHandle>> {
        let h = self.map.get(&doc_id)?;
        (!h.shutdown.is_cancelled()).then(|| h.clone())
    }

    /// Remove a self-evicted room from the map (best-effort unsubscribe,
    /// gauge down). A no-op if the room is live or absent. Idempotent with
    /// [`Rooms::evict`]: only the call whose `remove` wins adjusts the gauge.
    async fn drop_if_dead(&self, doc_id: Uuid) {
        let dead = self
            .map
            .get(&doc_id)
            .map(|h| h.shutdown.is_cancelled())
            .unwrap_or(false);
        if dead {
            if self.map.remove(&doc_id).is_some() {
                metrics::gauge!("knot_room_active").decrement(1.0);
            }
            let _ = self.bus.unsubscribe(doc_id).await;
        }
    }

    /// Send a revoke event to the room (if active). The room drops all
    /// conns; the WS shim's writer detects the closed channel and emits
    /// a 4403 close frame to each client.
    pub async fn revoke_all_for_doc(&self, doc_id: Uuid) {
        if let Some(h) = self.map.get(&doc_id) {
            let _ = h.tx.send(crate::room::Event::Revoke).await;
        }
    }

    /// Push a pre-framed server message to all clients in the doc's room, IF a
    /// room is currently active. Never boots a room — a comment on an unviewed
    /// document is simply dropped.
    pub async fn notify_doc_comments(&self, doc_id: Uuid, frame: Vec<u8>) {
        if let Some(h) = self.map.get(&doc_id) {
            let _ = h.tx.send(crate::room::Event::ServerFrame(frame)).await;
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
