//! Rooms registry. One `RoomHandle` per active doc.
//!
//! Acquire is in-flight-dedup safe: concurrent acquire calls for the same
//! doc cooperate so only one room boots.
//!
//! A page's writer outlives its room until it has written what it holds
//! (after an eviction, a self-eviction or on shutdown), and a new room for
//! the page waits for it, up to [`DRAIN_JOIN_WAIT`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::bus::{Bus, BusError};
use crate::engine::Engine;
use crate::hydrate::BootError;
use crate::room::{Room, RoomHandle};
use crate::snapshot::SnapshotPolicy;
use crate::writer::{WriterDone, WriterLimits};
use knot_storage::{SnapshotStore, UpdatesStore};

/// Error returned by [`Rooms::acquire`] when a room cannot be booted.
#[derive(Debug, Error)]
pub enum AcquireError {
    #[error("bus subscribe: {0}")]
    Bus(#[from] BusError),
    #[error("hydrate: {0}")]
    Hydrate(#[from] BootError),
    /// The page's previous room ended, and its writer is still writing what
    /// it held after [`DRAIN_JOIN_WAIT`]. The client retries.
    #[error("the page's previous writer is still saving")]
    Draining,
}

/// How long [`Rooms::acquire`] waits for the previous writer of the same
/// page to finish draining before it gives up with
/// [`AcquireError::Draining`].
pub const DRAIN_JOIN_WAIT: Duration = Duration::from_secs(15);

/// How long the server waits for every page writer after the HTTP drain on
/// shutdown ([`Rooms::drain_all`]). The chart's
/// `terminationGracePeriodSeconds` budgets for it.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(10);

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

/// The rooms of this replica, one per open page.
///
/// A page's writer outlives its room until its buffer is written: when a
/// room ends (evicted, or by itself on a writer Fatal), its writer keeps
/// retrying what it holds, and a new room for the page waits for it
/// ([`Rooms::acquire`], up to [`DRAIN_JOIN_WAIT`]), so the new room boots
/// with those edits instead of crediting them to whichever client resends
/// them first. The barrier is per replica.
pub struct Rooms {
    map: DashMap<Uuid, Arc<RoomHandle>>,
    inflight: DashMap<Uuid, Arc<Mutex<()>>>,
    /// The writers of rooms removed from `map` that were still writing
    /// what they held. Entries that are done are pruned on the next
    /// removal, or by the page's next `acquire`.
    draining: DashMap<Uuid, WriterDone>,
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
    writer_limits: WriterLimits,
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
            draining: DashMap::new(),
            engine,
            bus,
            updates,
            snapshots,
            policy,
            idle_evict,
            dirty_tx: None,
            acl_epochs: AclEpochs::new(),
            writer_limits: WriterLimits::default(),
        }
    }

    /// Builder: attach a notifier that receives this room's `doc_id`
    /// every time the writer reports a successful persist. The server
    /// owns the receiver and runs the reindex worker.
    pub fn with_dirty_tx(mut self, tx: mpsc::Sender<Uuid>) -> Self {
        self.dirty_tx = Some(tx);
        self
    }

    /// Builder: the limits every room's writer gets (default
    /// [`WriterLimits::default`]; tests pass smaller ones).
    pub fn with_writer_limits(mut self, limits: WriterLimits) -> Self {
        self.writer_limits = limits;
        self
    }

    /// Acquire (or boot) the room for `doc_id`. Concurrent calls cooperate
    /// via a per-doc Mutex so only one room boots.
    ///
    /// Before booting, it waits up to [`DRAIN_JOIN_WAIT`] for the page's
    /// previous writer to write what it holds, and gives up with
    /// [`AcquireError::Draining`] when it is still writing. That wait
    /// happens before the per-doc Mutex, on a clone of the writer's
    /// [`WriterDone`], so reconnects that arrive together wait side by side;
    /// outside the Mutex `acquire` only reads. Removing a dead handle and
    /// unsubscribing from the bus happen under the Mutex, so an
    /// unsubscribe never lands after the next boot's subscribe.
    ///
    /// Returns `Err` when the bus subscription or initial hydration fails
    /// (transient DB/bus blip). The inflight entry is removed after a
    /// successful insert so it does not accumulate indefinitely.
    pub async fn acquire(&self, doc_id: Uuid) -> Result<Arc<RoomHandle>, AcquireError> {
        if let Some(h) = self.live(doc_id) {
            return Ok(h);
        }
        if let Some(mut writer) = self.previous_writer(doc_id)
            && tokio::time::timeout(DRAIN_JOIN_WAIT, writer.wait())
                .await
                .is_err()
        {
            return Err(AcquireError::Draining);
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
        // A room that evicted itself (a writer Fatal, or an update it refused
        // after applying it: [`crate::nesting`]) leaves a cancelled handle in
        // the map. Drop it before booting a fresh one, so this join
        // rehydrates from storage.
        self.drop_if_dead(doc_id).await;
        // Without waiting: the wait above is over. A writer still running
        // here belongs to a room that ended while this call waited (for the
        // writer or the Mutex).
        if self
            .draining
            .get(&doc_id)
            .is_some_and(|writer| !writer.is_done())
        {
            return Err(AcquireError::Draining);
        }
        self.draining.remove(&doc_id);
        let sub = self.bus.subscribe(doc_id).await?;
        let h = Room::spawn_with_limits(
            doc_id,
            self.engine.clone(),
            self.bus.clone(),
            sub,
            self.updates.clone(),
            self.snapshots.clone(),
            self.policy,
            self.dirty_tx.clone(),
            self.writer_limits,
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

    /// The writer a new room for `doc_id` must wait for: that of a dead
    /// handle still in the map, else that of a removed room still draining.
    /// Only reads (see [`Rooms::acquire`]).
    fn previous_writer(&self, doc_id: Uuid) -> Option<WriterDone> {
        let dead = self
            .map
            .get(&doc_id)
            .filter(|h| h.shutdown.is_cancelled())
            .map(|h| h.writer_done.clone());
        dead.or_else(|| self.draining.get(&doc_id).map(|w| w.clone()))
    }

    /// Keep the writer of a room just removed from the map until it has
    /// written what it holds.
    fn keep_draining(&self, doc_id: Uuid, h: &RoomHandle) {
        if !h.writer_done.is_done() {
            self.draining.insert(doc_id, h.writer_done.clone());
        }
    }

    /// Forget the writers that have finished.
    fn prune_drained(&self) {
        self.draining.retain(|_, writer| !writer.is_done());
    }

    /// Remove a self-evicted room from the map (its writer goes to
    /// `draining` unless done; best-effort unsubscribe, gauge down). A no-op
    /// if the room is live or absent. Idempotent with [`Rooms::evict`]: only
    /// the call whose `remove` wins adjusts the gauge.
    async fn drop_if_dead(&self, doc_id: Uuid) {
        self.prune_drained();
        let dead = self
            .map
            .get(&doc_id)
            .map(|h| h.shutdown.is_cancelled())
            .unwrap_or(false);
        if dead {
            if let Some((_, h)) = self.map.remove(&doc_id) {
                metrics::gauge!("knot_room_active").decrement(1.0);
                self.keep_draining(doc_id, &h);
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
    /// when a revoke reached the connection first. The room's writer goes
    /// to `draining` unless it is done, so the page's next room waits for
    /// it.
    pub async fn evict(&self, doc_id: Uuid) {
        self.prune_drained();
        if let Some((_, h)) = self.map.remove(&doc_id) {
            h.shutdown.cancel();
            metrics::gauge!("knot_room_active").decrement(1.0);
            self.keep_draining(doc_id, &h);
        }
        let _ = self.bus.unsubscribe(doc_id).await;
        let _ = self.idle_evict;
    }

    /// On shutdown, after the HTTP drain: end every room (as
    /// [`Rooms::restart_all`], without bumping ACL epochs), then wait up to
    /// `within` for every writer, of those rooms and of rooms that ended
    /// earlier, to write what it holds. Returns how many were still writing
    /// and logs each one's page at warn level: their buffered edits are lost
    /// when the process exits.
    pub async fn drain_all(&self, within: Duration) -> usize {
        let doc_ids: Vec<Uuid> = self.map.iter().map(|entry| *entry.key()).collect();
        for doc_id in doc_ids {
            self.evict(doc_id).await;
        }
        let writers: Vec<(Uuid, WriterDone)> = self
            .draining
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let waits = writers.iter().map(|(_, writer)| {
            let mut writer = writer.clone();
            async move { writer.wait().await }
        });
        let _ = tokio::time::timeout(within, futures_util::future::join_all(waits)).await;
        let mut pending = 0;
        for (doc_id, writer) in &writers {
            if !writer.is_done() {
                pending += 1;
                tracing::warn!(
                    doc_id = %doc_id,
                    "page writer still saving at shutdown; its unsaved edits are lost"
                );
            }
        }
        pending
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

    // -----------------------------------------------------------------------
    // Draining writers (page history M1.2): a page's writer outlives its room
    // until its buffer is written, and a new room for the page waits for it.
    // -----------------------------------------------------------------------

    use crate::WriterLimits;
    use crate::room::tests::{exported_page, join, send_edit, until};
    use crate::test_fixtures::para;
    use knot_storage::{DocUpdate, NewUpdate, Persisted, RowsAfter, UpdatesStoreError};
    use std::sync::atomic::AtomicBool;
    use tokio::time::Instant;

    /// A log of several pages whose `append` fails, as a lost connection
    /// does (transient), while the database is down, and otherwise stores
    /// its rows at consecutive revs per page; `rows_after` serves what it
    /// stored.
    struct Outage {
        down: AtomicBool,
        state: std::sync::Mutex<OutageState>,
    }

    #[derive(Default)]
    struct OutageState {
        rows: Vec<DocUpdate>,
        /// The page of every failed append, in order.
        failed: Vec<Uuid>,
    }

    impl Outage {
        /// A store whose database is down.
        fn down() -> Arc<Self> {
            Arc::new(Self {
                down: AtomicBool::new(true),
                state: Default::default(),
            })
        }

        fn come_back(&self) {
            self.down.store(false, Ordering::SeqCst);
        }

        /// How many appends failed for `doc_id`.
        fn failed(&self, doc_id: Uuid) -> usize {
            let s = self.state.lock().unwrap();
            s.failed.iter().filter(|&&d| d == doc_id).count()
        }

        /// How many rows were stored for `doc_id`.
        fn stored(&self, doc_id: Uuid) -> usize {
            let s = self.state.lock().unwrap();
            s.rows.iter().filter(|r| r.doc_id == doc_id).count()
        }
    }

    #[async_trait::async_trait]
    impl UpdatesStore for Outage {
        async fn append(
            &self,
            doc_id: Uuid,
            epoch: i32,
            rows: &[NewUpdate],
        ) -> Result<Vec<Persisted>, UpdatesStoreError> {
            let mut s = self.state.lock().unwrap();
            if self.down.load(Ordering::SeqCst) {
                s.failed.push(doc_id);
                return Err(UpdatesStoreError::Sqlx(sqlx::Error::Io(
                    std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "database down"),
                )));
            }
            let mut out = Vec::new();
            for new in rows {
                let seq = s.rows.len() as i64 + 1;
                let rev = s.rows.iter().filter(|r| r.doc_id == doc_id).count() as i64 + 1;
                s.rows.push(DocUpdate {
                    seq,
                    doc_id,
                    update_bytes: new.bytes.clone(),
                    by_user_id: new.by_user_id,
                    created_at: sqlx::types::chrono::Utc::now(),
                    rev: Some(rev),
                    source: Some(new.source),
                    meta: new.meta.clone(),
                    epoch: Some(epoch),
                });
                out.push(Persisted { seq, rev });
            }
            Ok(out)
        }
        async fn rows_after(
            &self,
            doc_id: Uuid,
            from: RowsAfter,
        ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
            let s = self.state.lock().unwrap();
            Ok(s.rows
                .iter()
                .filter(|r| r.doc_id == doc_id)
                .filter(|r| match from {
                    RowsAfter::All => true,
                    RowsAfter::Rev(after) => r.rev.unwrap() > after,
                })
                .cloned()
                .collect())
        }
        async fn head(&self, doc_id: Uuid) -> Result<knot_storage::HistoryHead, UpdatesStoreError> {
            Ok(knot_storage::HistoryHead {
                head_rev: self.stored(doc_id) as i64,
                ..Default::default()
            })
        }
        async fn ensure_legacy_numbered(&self, _: Uuid) -> Result<u64, UpdatesStoreError> {
            Ok(0)
        }
        async fn since(
            &self,
            doc_id: Uuid,
            after: i64,
        ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
            let s = self.state.lock().unwrap();
            Ok(s.rows
                .iter()
                .filter(|r| r.doc_id == doc_id && r.seq > after)
                .cloned()
                .collect())
        }
        async fn max_seq(&self, _: Uuid) -> Result<i64, UpdatesStoreError> {
            Ok(self.state.lock().unwrap().rows.len() as i64)
        }
    }

    /// A bus that records every subscribe and unsubscribe, in order, and
    /// yields inside each, so a call made outside the per-page lock could
    /// interleave with another acquire's.
    #[derive(Default)]
    struct RecordingBus {
        inner: MemBus,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl RecordingBus {
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Bus for RecordingBus {
        async fn publish(&self, doc_id: Uuid, seq: i64) -> Result<(), BusError> {
            self.inner.publish(doc_id, seq).await
        }
        async fn publish_presence(&self, doc_id: Uuid, payload: Vec<u8>) -> Result<(), BusError> {
            self.inner.publish_presence(doc_id, payload).await
        }
        async fn subscribe(&self, doc_id: Uuid) -> Result<crate::Subscription, BusError> {
            tokio::task::yield_now().await;
            self.calls.lock().unwrap().push("subscribe");
            self.inner.subscribe(doc_id).await
        }
        async fn unsubscribe(&self, doc_id: Uuid) -> Result<(), BusError> {
            tokio::task::yield_now().await;
            self.calls.lock().unwrap().push("unsubscribe");
            self.inner.unsubscribe(doc_id).await
        }
    }

    fn rooms_on(updates: Arc<dyn UpdatesStore>, bus: Arc<dyn Bus>) -> Rooms {
        Rooms::new(
            Arc::new(YrsEngine),
            bus,
            updates,
            Arc::new(NoopSnapshots),
            SnapshotPolicy {
                every_n: 1000,
                idle: Duration::from_secs(60),
            },
            Duration::from_secs(300),
        )
    }

    /// A room for `doc_id` whose writer holds one WS edit with `text` that
    /// it could not write yet (the store is down).
    async fn room_holding_an_edit(
        rooms: &Rooms,
        store: &Outage,
        doc_id: Uuid,
        client: u64,
        text: &str,
    ) -> Arc<RoomHandle> {
        let h = rooms.acquire(doc_id).await.expect("the room boots");
        let (conn, _rx) = join(&h).await;
        send_edit(&h, conn, para(client, text)).await;
        until("the writer tried to write the edit", || {
            store.failed(doc_id) > 0
        })
        .await;
        h
    }

    fn spawn_acquire(
        rooms: &Arc<Rooms>,
        doc_id: Uuid,
    ) -> tokio::task::JoinHandle<Result<Arc<RoomHandle>, AcquireError>> {
        let rooms = rooms.clone();
        tokio::spawn(async move { rooms.acquire(doc_id).await })
    }

    /// An evicted room's writer still holds an edit; the page's next room
    /// waits for it to be written, so it boots with the edit (and a
    /// reconnecting client's resend is not credited with it).
    #[tokio::test(start_paused = true)]
    async fn a_new_room_waits_for_the_previous_writer_to_drain() {
        let store = Outage::down();
        let rooms = Arc::new(rooms_on(store.clone(), Arc::new(MemBus::new())));
        let doc_id = Uuid::new_v4();
        let old = room_holding_an_edit(&rooms, &store, doc_id, 11, "kept").await;

        rooms.evict(doc_id).await;
        let acquiring = spawn_acquire(&rooms, doc_id);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !acquiring.is_finished(),
            "a new room booted while the old writer still held an edit"
        );

        store.come_back();
        let new = tokio::time::timeout(DRAIN_JOIN_WAIT, acquiring)
            .await
            .expect("acquire never returned")
            .unwrap()
            .expect("the new room boots once the old writer drained");
        assert!(!Arc::ptr_eq(&new, &old));
        assert_eq!(store.stored(doc_id), 1, "the edit is written once");
        assert!(
            exported_page(&new).await.contains("kept"),
            "the new room lacks the drained edit"
        );
    }

    /// A writer that cannot drain within `DRAIN_JOIN_WAIT` makes `acquire`
    /// give up with `Draining` (the client retries); once it has drained,
    /// the page opens.
    #[tokio::test(start_paused = true)]
    async fn acquire_gives_up_on_a_writer_that_keeps_draining() {
        let store = Outage::down();
        let rooms = Arc::new(rooms_on(store.clone(), Arc::new(MemBus::new())));
        let doc_id = Uuid::new_v4();
        room_holding_an_edit(&rooms, &store, doc_id, 11, "kept").await;
        rooms.evict(doc_id).await;

        let started = Instant::now();
        let refused = rooms.acquire(doc_id).await;
        assert!(
            matches!(refused, Err(AcquireError::Draining)),
            "{:?}",
            refused.map(|_| "a room")
        );
        assert!(started.elapsed() >= DRAIN_JOIN_WAIT, "gave up early");

        store.come_back();
        let h = rooms.acquire(doc_id).await.expect("the page opens");
        assert!(exported_page(&h).await.contains("kept"));
    }

    /// Reconnects that arrive together wait for the drain side by side,
    /// not one after another under the per-page lock.
    #[tokio::test(start_paused = true)]
    async fn concurrent_acquires_wait_for_one_drain_side_by_side() {
        let store = Outage::down();
        let rooms = Arc::new(rooms_on(store.clone(), Arc::new(MemBus::new())));
        let doc_id = Uuid::new_v4();
        room_holding_an_edit(&rooms, &store, doc_id, 11, "kept").await;
        rooms.evict(doc_id).await;

        let started = Instant::now();
        let (a, b, c) = tokio::join!(
            rooms.acquire(doc_id),
            rooms.acquire(doc_id),
            rooms.acquire(doc_id)
        );
        for result in [a, b, c] {
            assert!(matches!(result, Err(AcquireError::Draining)));
        }
        assert!(
            started.elapsed() <= DRAIN_JOIN_WAIT + Duration::from_secs(1),
            "the acquires waited in turn: {:?}",
            started.elapsed()
        );
    }

    /// A room that evicts itself (a server-side edit missed its deadline
    /// during an outage) leaves its dead handle in the map; the page's next
    /// room waits until its writer has written the WS edit buffered ahead,
    /// and the failed server-side edit is never written.
    #[tokio::test(start_paused = true)]
    async fn a_self_evicted_room_is_drained_before_a_new_one_boots() {
        let store = Outage::down();
        let rooms = Arc::new(
            rooms_on(store.clone(), Arc::new(MemBus::new())).with_writer_limits(WriterLimits {
                server_job_deadline: Duration::from_millis(300),
                attempt_timeout: Duration::from_millis(300),
            }),
        );
        let doc_id = Uuid::new_v4();
        let old = room_holding_an_edit(&rooms, &store, doc_id, 11, "typed").await;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        old.tx
            .send(crate::room::Event::ApplyUpdate {
                update_bytes: para(12, "toggled"),
                by_user: None,
                source: knot_storage::UpdateSource::Edit,
                meta: None,
                reply: reply_tx,
            })
            .await
            .unwrap();
        let reply = reply_rx.await.expect("the room answers the server edit");
        assert!(reply.is_err(), "the server edit was not persisted");
        until("the room evicted itself", || old.shutdown.is_cancelled()).await;
        assert!(
            rooms.map.contains_key(&doc_id),
            "the dead handle is still mapped"
        );

        let acquiring = spawn_acquire(&rooms, doc_id);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !acquiring.is_finished(),
            "a new room booted while the old writer still held the WS edit"
        );

        store.come_back();
        let new = tokio::time::timeout(DRAIN_JOIN_WAIT, acquiring)
            .await
            .expect("acquire never returned")
            .unwrap()
            .expect("the new room boots once the old writer drained");
        assert_eq!(store.stored(doc_id), 1, "only the WS edit is written, once");
        let page = exported_page(&new).await;
        assert!(page.contains("typed"), "{page}");
        assert!(!page.contains("toggled"), "{page}");
    }

    /// A room that dies while an `acquire` waits for the per-page lock (its
    /// unlocked check found nothing to wait for) is drained too: under the
    /// lock the dead handle moves to `draining` before the draining check,
    /// so the `acquire` refuses instead of booting a room without the edit.
    #[tokio::test(start_paused = true)]
    async fn a_room_that_dies_while_acquire_waits_for_the_lock_is_drained_first() {
        let store = Outage::down();
        let bus = Arc::new(MemBus::new());
        let rooms = Arc::new(rooms_on(store.clone(), bus.clone()));
        let doc_id = Uuid::new_v4();

        // As a concurrent acquire booting the page would hold it.
        let page_lock = rooms
            .inflight
            .entry(doc_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let held = page_lock.lock().await;
        let acquiring = spawn_acquire(&rooms, doc_id);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!acquiring.is_finished());

        // What that concurrent acquire does: boots a room and maps it. The
        // room takes an edit, then dies (as on a writer Fatal).
        let sub = bus.subscribe(doc_id).await.unwrap();
        let room = Arc::new(
            Room::spawn(
                doc_id,
                Arc::new(YrsEngine),
                bus.clone(),
                sub,
                store.clone(),
                Arc::new(NoopSnapshots),
                rooms.policy,
                None,
            )
            .await
            .unwrap(),
        );
        rooms.map.insert(doc_id, room.clone());
        let (conn, _rx) = join(&room).await;
        send_edit(&room, conn, para(11, "kept")).await;
        until("the writer tried to write the edit", || {
            store.failed(doc_id) > 0
        })
        .await;
        room.shutdown.cancel();
        drop(held);

        let refused = acquiring.await.unwrap();
        assert!(
            matches!(refused, Err(AcquireError::Draining)),
            "{:?}",
            refused.map(|_| "a room booted without the edit")
        );

        store.come_back();
        let h = rooms.acquire(doc_id).await.expect("the page opens");
        assert_eq!(store.stored(doc_id), 1);
        assert!(exported_page(&h).await.contains("kept"));
    }

    /// The dead handle is removed and the bus unsubscribed only under the
    /// per-page lock: an `UNLISTEN` sent outside it could land after the
    /// next boot's `LISTEN` on the same connection.
    #[tokio::test(start_paused = true)]
    async fn a_dead_room_unsubscribes_only_under_the_lock() {
        let bus = Arc::new(RecordingBus::default());
        let rooms = rooms_on(Arc::new(NoopUpdates), bus.clone());
        let doc_id = Uuid::new_v4();
        let old = rooms.acquire(doc_id).await.unwrap();
        old.shutdown.cancel();
        old.writer_done.clone().wait().await;
        bus.calls.lock().unwrap().clear();

        let (a, b, c) = tokio::join!(
            rooms.acquire(doc_id),
            rooms.acquire(doc_id),
            rooms.acquire(doc_id)
        );
        let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
        assert_eq!(bus.calls(), ["unsubscribe", "subscribe"]);
        assert!(!Arc::ptr_eq(&a, &old));
        assert!(
            Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c),
            "more than one room booted"
        );
    }

    /// On shutdown every writer gets `within` to write what it holds.
    #[tokio::test(start_paused = true)]
    async fn drain_all_writes_buffered_edits_before_returning() {
        let within = Duration::from_secs(10);

        // The database comes back after 2 s: both edits are written.
        let store = Outage::down();
        let rooms = Arc::new(rooms_on(store.clone(), Arc::new(MemBus::new())));
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        room_holding_an_edit(&rooms, &store, one, 11, "one").await;
        room_holding_an_edit(&rooms, &store, two, 12, "two").await;
        tokio::spawn({
            let store = store.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                store.come_back();
            }
        });
        assert_eq!(
            rooms.drain_all(within).await,
            0,
            "a writer was left running"
        );
        assert_eq!((store.stored(one), store.stored(two)), (1, 1));

        // The database stays down: both writers are still running at `within`.
        let store = Outage::down();
        let rooms = Arc::new(rooms_on(store.clone(), Arc::new(MemBus::new())));
        room_holding_an_edit(&rooms, &store, one, 11, "one").await;
        room_holding_an_edit(&rooms, &store, two, 12, "two").await;
        let started = Instant::now();
        assert_eq!(rooms.drain_all(within).await, 2);
        let waited = started.elapsed();
        assert!(
            waited >= within && waited < within + Duration::from_secs(1),
            "{waited:?}"
        );
    }
}
