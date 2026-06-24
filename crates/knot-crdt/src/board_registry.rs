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
}

impl BoardRooms {
    pub fn new(engine: Arc<dyn Engine>, store: Arc<dyn BoardStore>, bus: Arc<dyn Bus>) -> Self {
        Self {
            map: DashMap::new(),
            inflight: DashMap::new(),
            engine,
            store,
            bus,
        }
    }

    /// Acquire (or boot) the board room for `board_id`. Concurrent calls
    /// cooperate via a per-board Mutex so only one room boots.
    ///
    /// Returns `Err` when the bus subscription or initial hydration fails
    /// (transient DB/bus blip). The inflight entry is removed after a
    /// successful insert so it does not accumulate indefinitely.
    pub async fn acquire(&self, board_id: Uuid) -> Result<Arc<BoardRoomHandle>, BoardAcquireError> {
        if let Some(h) = self.map.get(&board_id) {
            return Ok(h.clone());
        }
        let guard = self
            .inflight
            .entry(board_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _lock = guard.lock().await;
        if let Some(h) = self.map.get(&board_id) {
            return Ok(h.clone());
        }
        let sub = self.bus.subscribe(board_id).await?;
        let h = BoardRoom::spawn(
            board_id,
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

    pub async fn evict(&self, board_id: Uuid) {
        if let Some((_, h)) = self.map.remove(&board_id) {
            h.shutdown.cancel();
            let _ = self.bus.unsubscribe(board_id).await;
            metrics::gauge!("knot_board_room_active").decrement(1.0);
        }
    }
}
