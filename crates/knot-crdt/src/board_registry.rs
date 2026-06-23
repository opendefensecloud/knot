//! BoardRoom registry. One `BoardRoomHandle` per active board.
//!
//! Concurrent acquire calls for the same board cooperate via a per-board
//! Mutex so only one room boots.

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::board_room::{BoardRoom, BoardRoomHandle};
use crate::bus::Bus;
use crate::engine::Engine;
use knot_storage::BoardStore;

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

    pub async fn acquire(&self, board_id: Uuid) -> Arc<BoardRoomHandle> {
        if let Some(h) = self.map.get(&board_id) {
            return h.clone();
        }
        let guard = self
            .inflight
            .entry(board_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _lock = guard.lock().await;
        if let Some(h) = self.map.get(&board_id) {
            return h.clone();
        }
        let sub = self.bus.subscribe(board_id).await.expect("board bus subscribe");
        let h = BoardRoom::spawn(board_id, self.engine.clone(), self.store.clone(), self.bus.clone(), sub)
            .await
            .expect("hydrate board room");
        let arc = Arc::new(h);
        self.map.insert(board_id, arc.clone());
        metrics::gauge!("knot_board_room_active").increment(1.0);
        arc
    }

    pub async fn evict(&self, board_id: Uuid) {
        if let Some((_, h)) = self.map.remove(&board_id) {
            h.shutdown.cancel();
            let _ = self.bus.unsubscribe(board_id).await;
            metrics::gauge!("knot_board_room_active").decrement(1.0);
        }
    }
}
