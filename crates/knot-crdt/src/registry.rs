//! Rooms registry. One `RoomHandle` per active doc.
//!
//! Acquire is in-flight-dedup safe: concurrent acquire calls for the same
//! doc cooperate so only one room boots.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// How many counters [`AclEpochs`] spreads doc ids over.
const ACL_EPOCH_STRIPES: usize = 64;

/// Per-doc ACL epochs, striped: one counter per stripe of doc ids, bumped
/// by every revoke of a doc in the stripe, whether or not a room is live.
///
/// They close the join race. A collab upgrade checks access before its
/// connection joins the room, so a revoke that lands in between finds the
/// connection not yet in the room, or the room not yet booted, and misses
/// it. The upgrade therefore snapshots the doc's epoch before its access
/// check and compares it once joined: a change means a revoke may have
/// missed it, and the connection is closed with 4403 (the client re-checks
/// its access and reconnects if it still has it). Two docs share a stripe,
/// so an unrelated revoke can close a connection that is joining at that
/// moment; that costs a reconnect, never access.
pub(crate) struct AclEpochs([AtomicU64; ACL_EPOCH_STRIPES]);

impl AclEpochs {
    pub(crate) fn new() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }

    /// The stripe of `doc_id`: its low bits, which are random in v4 and v7
    /// uuids alike.
    fn stripe(&self, doc_id: Uuid) -> &AtomicU64 {
        &self.0[(doc_id.as_u128() % ACL_EPOCH_STRIPES as u128) as usize]
    }

    /// The current epoch of `doc_id`'s stripe.
    pub(crate) fn get(&self, doc_id: Uuid) -> u64 {
        self.stripe(doc_id).load(Ordering::SeqCst)
    }

    /// Bump `doc_id`'s stripe. A revoke calls it before it revokes anything,
    /// so a connection the revoke misses (it joins after the revoke reached
    /// the room) reads the new epoch once it has joined.
    pub(crate) fn bump(&self, doc_id: Uuid) {
        self.stripe(doc_id).fetch_add(1, Ordering::SeqCst);
    }

    /// Bump every stripe: the access to any doc may have changed.
    pub(crate) fn bump_all(&self) {
        for stripe in &self.0 {
            stripe.fetch_add(1, Ordering::SeqCst);
        }
    }
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
    acl_epochs: AclEpochs,
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
            acl_epochs: AclEpochs::new(),
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
        // A room that evicted itself (it refused an update it had applied,
        // which nested the page too deep: [`crate::nesting`]) leaves a
        // cancelled handle in the map. Drop it before booting a fresh one,
        // so this join rehydrates from storage.
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
    /// A room that evicted itself over an update it refused after applying
    /// it (too deep, [`crate::nesting`]) is treated as absent — its live doc
    /// holds refused content, so a reader must fall back to storage.
    /// `acquire` prunes the dead entry on the next join.
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

    /// The ACL epoch of `doc_id` ([`AclEpochs`]): a collab upgrade reads it
    /// before its access check and again once its connection has joined,
    /// and closes the connection with 4403 when it changed.
    pub fn acl_epoch(&self, doc_id: Uuid) -> u64 {
        self.acl_epochs.get(doc_id)
    }

    /// Send a revoke event to the room (if active). The room cancels each
    /// connection's `revoked` token and drops the connection; each WS
    /// shim's writer then closes its socket with 4403. Returns once the
    /// event is queued, before the room has handled it: to close the
    /// sockets of a room that goes too, use [`Rooms::revoke_and_evict`].
    /// Bumps the doc's ACL epoch first, room or not, for the connections
    /// that join after the event.
    pub async fn revoke_all_for_doc(&self, doc_id: Uuid) {
        self.acl_epochs.bump(doc_id);
        let room = self.map.get(&doc_id).map(|h| h.clone());
        if let Some(h) = room {
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

    /// Close every connection of the doc's room with 4403, then evict it:
    /// for a page whose access ends for everyone (a purge). The revocation
    /// takes effect first and at once: each connection's `revoked` token is
    /// a child of the room's ([`RoomHandle::revoked`]), and each WS shim
    /// checks it before the room's end, which would close with 1012 and
    /// make the client reconnect to a page that is gone.
    pub async fn revoke_and_evict(&self, doc_id: Uuid) {
        self.acl_epochs.bump(doc_id);
        if let Some(h) = self.map.get(&doc_id).map(|h| h.clone()) {
            h.revoked.cancel();
        }
        self.evict(doc_id).await;
    }

    /// The access to any doc may have changed without this replica hearing
    /// which (the ACL listener lost its connection or could not walk a
    /// subtree), or it changed for one user in every doc (a workspace role
    /// change or a removal): end every room here, so each WS shim closes its
    /// socket with 1012 and its client reconnects through the upgrade's
    /// access check.
    /// Only a client that lost its access is refused then; nobody else is
    /// told it lost access, as a revoke (4403) would. Bumps every ACL epoch
    /// first, for the connections joining meanwhile.
    pub async fn restart_all(&self) {
        self.acl_epochs.bump_all();
        let live_doc_ids: Vec<Uuid> = self.map.iter().map(|entry| *entry.key()).collect();
        for doc_id in live_doc_ids {
            self.evict(doc_id).await;
        }
    }

    /// Cancel the room's actor and unsubscribe from the bus. Each WS shim
    /// of the room watches its token and closes its socket: 1012, or 4403
    /// when a revoke reached the connection first.
    pub async fn evict(&self, doc_id: Uuid) {
        if let Some((_, h)) = self.map.remove(&doc_id) {
            h.shutdown.cancel();
            metrics::gauge!("knot_room_active").decrement(1.0);
        }
        let _ = self.bus.unsubscribe(doc_id).await;
        let _ = self.idle_evict;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::tests::{NoopSnapshots, NoopUpdates};
    use crate::{MemBus, YrsEngine};

    fn rooms() -> Rooms {
        Rooms::new(
            Arc::new(YrsEngine),
            Arc::new(MemBus::new()),
            Arc::new(NoopUpdates),
            Arc::new(NoopSnapshots),
            SnapshotPolicy {
                every_n: 1000,
                idle: Duration::from_secs(60),
            },
            Duration::from_secs(300),
        )
    }

    /// A collab upgrade checks access before its connection joins the room,
    /// and a revoke in between may find no room to revoke yet. Every revoke
    /// therefore bumps the doc's ACL epoch, room or not, so the upgrade can
    /// tell after its join that its access check is stale.
    #[tokio::test]
    async fn revoke_bumps_epoch_without_live_room() {
        let rooms = rooms();
        let doc_id = Uuid::new_v4();

        let before_revoke = rooms.acl_epoch(doc_id);
        rooms.revoke_all_for_doc(doc_id).await;
        let after_revoke = rooms.acl_epoch(doc_id);
        assert_ne!(after_revoke, before_revoke, "revoke_all_for_doc");

        rooms.revoke_and_evict(doc_id).await;
        assert_ne!(rooms.acl_epoch(doc_id), after_revoke, "revoke_and_evict");
    }

    /// When the ACL listener may have missed changes, every room here ends
    /// (its sockets close with 1012, so clients reconnect through the
    /// upgrade's access check) rather than being revoked (4403), and every
    /// doc's epoch moves, opened or not, for the connections joining
    /// meanwhile. The next join boots a fresh room.
    #[tokio::test]
    async fn restart_all_ends_every_room_and_bumps_every_epoch() {
        let rooms = rooms();
        let (open_doc_id, other_open_doc_id, unopened_doc_id) =
            (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let open_room = rooms.acquire(open_doc_id).await.unwrap();
        let other_open_room = rooms.acquire(other_open_doc_id).await.unwrap();
        let epochs_before =
            [open_doc_id, other_open_doc_id, unopened_doc_id].map(|doc_id| rooms.acl_epoch(doc_id));

        rooms.restart_all().await;

        for room in [&open_room, &other_open_room] {
            assert!(room.shutdown.is_cancelled(), "a room was left running");
            assert!(
                !room.revoked.is_cancelled(),
                "a restart must not revoke (4403) its connections"
            );
        }
        let epochs_after =
            [open_doc_id, other_open_doc_id, unopened_doc_id].map(|doc_id| rooms.acl_epoch(doc_id));
        for (before, after) in epochs_before.iter().zip(epochs_after.iter()) {
            assert_ne!(before, after, "an epoch was not bumped");
        }
        let fresh_room = rooms.acquire(open_doc_id).await.unwrap();
        assert!(
            !Arc::ptr_eq(&fresh_room, &open_room),
            "the ended room was reused"
        );
    }
}
