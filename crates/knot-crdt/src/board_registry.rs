//! BoardRoom registry. One `BoardRoomHandle` per active board.
//!
//! Concurrent acquire calls for the same board cooperate via a per-board
//! Mutex so only one room boots.

use std::sync::Arc;

use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::board_room::{BoardRoom, BoardRoomHandle};
use crate::bus::{Bus, BusError};
use crate::engine::{Engine, EngineError};
use crate::registry::AclEpochs;
use knot_storage::BoardStore;

/// Error returned by [`BoardRooms::acquire`] when a board room cannot be booted.
#[derive(Debug, Error)]
pub enum BoardAcquireError {
    #[error("bus subscribe: {0}")]
    Bus(#[from] BusError),
    #[error("hydrate: {0}")]
    Hydrate(#[from] EngineError),
}

pub struct BoardRooms {
    map: DashMap<Uuid, Arc<BoardRoomHandle>>,
    inflight: DashMap<Uuid, Arc<Mutex<()>>>,
    engine: Arc<dyn Engine>,
    store: Arc<dyn BoardStore>,
    bus: Arc<dyn Bus>,
    /// Keyed by the boards' page: a board's access is its page's.
    acl_epochs: AclEpochs,
}

impl BoardRooms {
    pub fn new(engine: Arc<dyn Engine>, store: Arc<dyn BoardStore>, bus: Arc<dyn Bus>) -> Self {
        Self {
            map: DashMap::new(),
            inflight: DashMap::new(),
            engine,
            store,
            bus,
            acl_epochs: AclEpochs::new(),
        }
    }

    /// Acquire (or boot) the board room for `board_id`, a board of the page
    /// `doc_id`. Concurrent calls cooperate via a per-board Mutex so only
    /// one room boots.
    ///
    /// Returns `Err` when the bus subscription or initial hydration fails
    /// (transient DB/bus blip). The inflight entry is removed after a
    /// successful insert so it does not accumulate indefinitely.
    pub async fn acquire(
        &self,
        board_id: Uuid,
        doc_id: Uuid,
    ) -> Result<Arc<BoardRoomHandle>, BoardAcquireError> {
        if let Some(h) = self.live(board_id) {
            return Ok(h);
        }
        let guard = self
            .inflight
            .entry(board_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _lock = guard.lock().await;
        if let Some(h) = self.live(board_id) {
            return Ok(h);
        }
        // A board room that evicted itself (it refused an update it had
        // applied, which nested the board too deep: [`crate::nesting`])
        // leaves a cancelled handle; drop it so this join rehydrates from
        // storage.
        self.drop_if_dead(board_id).await;
        let sub = self.bus.subscribe(board_id).await?;
        let h = BoardRoom::spawn(
            board_id,
            doc_id,
            self.engine.clone(),
            self.store.clone(),
            self.bus.clone(),
            sub,
        )
        .await?;
        let arc = Arc::new(h);
        self.map.insert(board_id, arc.clone());
        // Prune the inflight entry now that the room is live; leaving it in
        // would cause the DashMap to grow without bound over time.
        self.inflight.remove(&board_id);
        metrics::gauge!("knot_board_room_active").increment(1.0);
        Ok(arc)
    }

    /// The ACL epoch of the page `doc_id`, as [`crate::Rooms::acl_epoch`]:
    /// a board upgrade reads it before its access check on the page and
    /// again once its connection has joined, and closes the connection with
    /// 4403 when it changed.
    pub fn acl_epoch(&self, doc_id: Uuid) -> u64 {
        self.acl_epochs.get(doc_id)
    }

    /// The access to the page `doc_id` changed: close every connection of
    /// its board rooms with 4403 (each WS shim's token is a child of the
    /// room's `revoked`) and evict the rooms, so that the next join boots a
    /// fresh one with an uncancelled token. The client re-checks its access
    /// before it reconnects. Bumps the page's ACL epoch first, for the
    /// connections joining meanwhile, whether or not a board room is live.
    pub async fn revoke_for_doc(&self, doc_id: Uuid) {
        self.acl_epochs.bump(doc_id);
        let revoked_rooms: Vec<(Uuid, Arc<BoardRoomHandle>)> = self
            .map
            .iter()
            .filter(|entry| entry.value().doc_id == doc_id)
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        for (board_id, handle) in revoked_rooms {
            handle.revoked.cancel();
            self.evict(board_id).await;
        }
    }

    /// As [`crate::Rooms::restart_all`]: the access to any page may have
    /// changed unheard, so end every board room here. Each WS shim closes
    /// its socket with 1012 and its client reconnects through the upgrade's
    /// access check. Bumps every page's ACL epoch first.
    pub async fn restart_all(&self) {
        self.acl_epochs.bump_all();
        let live_board_ids: Vec<Uuid> = self.map.iter().map(|entry| *entry.key()).collect();
        for board_id in live_board_ids {
            self.evict(board_id).await;
        }
    }

    pub async fn evict(&self, board_id: Uuid) {
        if let Some((_, h)) = self.map.remove(&board_id) {
            h.shutdown.cancel();
            let _ = self.bus.unsubscribe(board_id).await;
            metrics::gauge!("knot_board_room_active").decrement(1.0);
        }
    }

    /// The running board room for `board_id`, or `None` if there is none or
    /// it has evicted itself (its `shutdown` is cancelled).
    fn live(&self, board_id: Uuid) -> Option<Arc<BoardRoomHandle>> {
        let h = self.map.get(&board_id)?;
        (!h.shutdown.is_cancelled()).then(|| h.clone())
    }

    /// Remove a self-evicted board room from the map (best-effort
    /// unsubscribe, gauge down). A no-op if the room is live or absent.
    async fn drop_if_dead(&self, board_id: Uuid) {
        let dead = self
            .map
            .get(&board_id)
            .map(|h| h.shutdown.is_cancelled())
            .unwrap_or(false);
        if dead {
            if self.map.remove(&board_id).is_some() {
                metrics::gauge!("knot_board_room_active").decrement(1.0);
            }
            let _ = self.bus.unsubscribe(board_id).await;
        }
    }
}
