//! Per-doc actor. One tokio task. Exclusive owner of `DocHandle` and the
//! local connection map. All I/O flows through mpsc channels.
//!
//! - **Boot** ([`crate::hydrate`], room mode): the latest snapshot, then the
//!   page's log rows contiguous in rev after it. The room's watermark,
//!   `applied_through_rev`, is a rev: every row at or below it is applied.
//! - **Inbound updates** are applied, and what the apply changed (the
//!   effective update, or the raw bytes when content is pending:
//!   [`crate::engine::ApplyMeasured`]) is handed to the writer
//!   ([`crate::writer`]) and only then fanned out, those same bytes.
//! - **Server-side edits** (`ApplyUpdate`, `ReplaceWithMarkdown`,
//!   `PatchTaskChecked`, `ApplySuggestion`) are tagged with what made them
//!   (`source`, `meta`), and the room waits until the writer has stored one
//!   before it fans it out and replies. One that is not stored, or that
//!   fails after it began to change the doc, reaches no client and evicts
//!   the room.
//! - **The writer** reports where each row landed (`WriterEvent::Applied`,
//!   with its rev). An own rev right after the watermark advances it; one
//!   past it means rows of other replicas committed in between, and the room
//!   catches up at once. A `WriterEvent::Fatal` evicts the room.
//! - **Catch-up** (a bus message, a 5 s tick, a gap) reads the rows past the
//!   watermark and applies those contiguous in rev. It stops at a rev gap
//!   and retries on the next tick. A row that fails to apply is passed.
//! - **Snapshots** (every N own rows, when idle, when the room ends) are
//!   written only when the doc holds nothing beyond the page's pre-upgrade
//!   base and the log up to the watermark: no row in flight, no own rev past
//!   the watermark, not evicted, and not `unlogged`. A room is `unlogged`
//!   for the rest of its life once its doc holds content no row carries: an
//!   update yrs rejected part-way, or a row that failed to apply at boot or
//!   in catch-up. Each snapshot is stamped with the watermark, the content
//!   epoch and the seq of the row at the watermark.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::bus::{Bus, Subscription};
use crate::engine::{ApplyMeasured, DocHandle, Engine, EngineError};
use crate::hydrate::{BootError, BootMode};
use crate::protocol::wrap_sync_update;
use crate::tasks::patch_task_checked;
use crate::writer::{Applied, PersistJob, WriterDone, WriterEvent, WriterLimits};

pub type ConnId = Uuid;

/// Bytes delivered from a local connection's WS read task.
pub struct InMsg {
    pub from: ConnId,
    /// The authenticated user behind the connection, persisted as the
    /// update's author (`doc_updates.by_user_id`) and thereby credited as a
    /// contributor. `None` only where no user is meaningful (tests).
    pub by_user: Option<Uuid>,
    pub bytes: Vec<u8>,
}

/// Handle the room hands to a local connection. The WS read task wraps it
/// to send framed messages back to the client.
pub struct ConnHandle {
    pub tx: mpsc::Sender<Vec<u8>>,
    /// Cancelled by the room when the connection's access is revoked; the WS
    /// shim then closes the socket with 4403. The shim makes it a child of
    /// [`RoomHandle::revoked`], which revokes every connection at once. A
    /// dedicated signal because the channel cannot carry it by closing (the
    /// shim holds its own sender for the whole session) and no frame value
    /// is free to mean it (an empty frame is already the presence-clearing
    /// message).
    pub revoked: CancellationToken,
    /// Cancelled by the room when it refuses an update the connection sent:
    /// one that would nest the page too deep ([`crate::nesting`]), or one
    /// that doesn't decode; the WS shim then closes the socket with 4422.
    /// The room no longer holds the connection and doesn't join it again,
    /// so nothing more it sends is applied or shared, its presence
    /// included. When the refusal also evicts the room (an update it applied
    /// and then refused), the other connections are closed through
    /// [`RoomHandle::shutdown`].
    pub refused: CancellationToken,
}

impl ConnHandle {
    pub fn new(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx,
            revoked: CancellationToken::new(),
            refused: CancellationToken::new(),
        }
    }
}

/// What a joining connection needs for both halves of the y-sync handshake.
///
/// `update` is the full document state, sent as SyncStep2. `state_vector` is
/// sent as the server's own SyncStep1 so the client replies with whatever the
/// server lacks — the only path by which edits a client made while its socket
/// was down ever reach the server (the web providers apply offline edits
/// locally but do not queue them for sending). Both are read in the same
/// actor turn, so they describe the same document state.
#[derive(Debug)]
pub struct JoinState {
    pub update: Vec<u8>,
    pub state_vector: Vec<u8>,
}

impl JoinState {
    pub(crate) fn of(engine: &dyn Engine, doc: &DocHandle) -> Result<Self, EngineError> {
        Ok(Self {
            update: engine.encode_state_as_update(doc, None)?,
            state_vector: engine.encode_state_vector(doc)?,
        })
    }
}

/// All inputs the room actor multiplexes.
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
    BusUpdate(i64),
    BusPresence(Vec<u8>),
    Revoke,
    /// Broadcast a pre-framed server message to every connection in the room.
    /// Opaque bytes — the room does not interpret them.
    ServerFrame(Vec<u8>),
    ExportState(oneshot::Sender<Result<(Vec<u8>, i64), EngineError>>),
    /// Apply a server-side update (a Markdown import in append mode, a page
    /// created from a template, a test's seed) to the live doc. What the
    /// apply changed (`ApplyMeasured::persist`) is persisted as one row
    /// tagged `source` and `meta`, and fanned out only once the writer has
    /// stored it; `reply` then carries its seq. An update that changes
    /// nothing persists nothing and replies `Ok` with the room's seq. An
    /// update that does not decode replies `Err` and changes nothing. One
    /// that yrs rejects part-way, or that nests the page too deep, or whose
    /// persist fails, replies `Err` and evicts the room, which then holds
    /// content no row carries.
    ApplyUpdate {
        update_bytes: Vec<u8>,
        by_user: Option<Uuid>,
        /// What made the edit (`doc_updates.source`).
        source: knot_storage::UpdateSource,
        /// Ids, timestamps and page titles only; never page text or names.
        meta: Option<serde_json::Value>,
        reply: oneshot::Sender<Result<i64, EngineError>>,
    },
    /// Replace the entire document content with the given pre-parsed update bytes.
    /// The bytes are decoded first; then the live doc's "default"
    /// XmlFragment is cleared and the update applied, in a single Yrs
    /// transaction, whose own update is persisted as one row tagged `source`
    /// and `meta` and fanned out only once the writer has stored it. Bytes
    /// that do not decode reply `Err` and leave the doc untouched; an apply
    /// error after the clear, a transaction that produced no update, or a
    /// failed persist replies `Err` and evicts the room. The caller is responsible for converting markdown to update
    /// bytes (e.g. via `knot_markdown::from_markdown::parse`).
    ReplaceWithMarkdown {
        /// Full-state update bytes encoding the replacement content.
        update_bytes: Vec<u8>,
        /// Attribution for the persisted update, mirroring `ApplyUpdate`:
        /// the importer / restorer, credited as a contributor. `None` only
        /// where there is no meaningful actor (tests).
        by_user: Option<Uuid>,
        /// What made the edit (`doc_updates.source`): `Import` or `Restore`.
        source: knot_storage::UpdateSource,
        /// Ids, timestamps and page titles only; never page text or names.
        meta: Option<serde_json::Value>,
        reply: oneshot::Sender<Result<i64, String>>,
    },
    /// Flip the `checked` attribute on the Nth task `list_item` in the doc.
    /// `item_index` counts list_item nodes with a `checked` attr in
    /// document order ([`crate::tasks::task_items`]) — the index
    /// `knot_markdown::tasks::extract_tasks_from_doc` lists. The mutation
    /// happens directly on the live doc; the resulting update is persisted
    /// as a `task` row and fanned out only once the writer has stored it. A
    /// failed persist replies `Err` and evicts the room.
    PatchTaskChecked {
        item_index: i32,
        checked: bool,
        by_user: Option<Uuid>,
        reply: oneshot::Sender<Result<i64, String>>,
    },
    /// Apply an accepted change suggestion to the live doc
    /// ([`crate::suggestions::apply_suggestion`]): resolve its anchors, check
    /// the text the owner saw, replace the range and record the suggestion
    /// id in [`crate::suggestions::APPLIED_MAP`] under the claim's
    /// `apply_token`, all in one transaction.
    /// The room first catches up on updates other replicas persisted, so a
    /// lagging room doesn't answer `AnchorLost` for text it hasn't seen yet.
    /// The update is persisted first and fanned out only once storage has
    /// it, so no client ever holds an apply that storage lacks; a refusal
    /// writes nothing. When persisting fails the room evicts itself, so no
    /// live room keeps an applied marker that storage lacks either.
    ApplySuggestion {
        request: crate::suggestions::ApplySuggestionRequest,
        /// Attribution for the persisted update: the suggester, who wrote
        /// the words, not the accepting owner.
        by_user: Option<Uuid>,
        /// The owner who accepted it, recorded in the row's `meta` as
        /// `accepted_by`, beside the `suggestion_id`.
        accepted_by: Uuid,
        reply: oneshot::Sender<Result<i64, crate::suggestions::ApplySuggestionError>>,
    },
    /// Move the top-level blocks the anchors `start..end` lie in to a new
    /// page ([`crate::move_blocks`]). After catching up, the room builds the
    /// new page's content and awaits `commit` (which stores it) while holding
    /// the doc, so nothing changes between what was copied and what is cut.
    /// Then it replaces the blocks with a link, persists that as an `edit`
    /// row by `by_user` and fans it out once stored. A refused commit or an
    /// anchor that doesn't resolve changes nothing; a failed persist evicts
    /// the room (the new page then holds the blocks, and so does the source).
    MoveBlocksOut {
        start: Vec<u8>,
        end: Vec<u8>,
        by_user: Option<Uuid>,
        commit: crate::move_blocks::Commit,
        reply: oneshot::Sender<Result<i64, crate::move_blocks::MoveBlocksError>>,
    },
    /// Whether the doc records `suggestion_id` as applied under the claim's
    /// `apply_token` ([`crate::suggestions::is_applied`]), after catching up
    /// on persisted updates. The marker in the doc is the single source of
    /// truth for "was this applied", which recovering a suggestion stuck in
    /// `applying` relies on; a marker with any other value (an Editor can
    /// write the map over the socket) is not the room's and reads as "no".
    /// When the catch-up does not catch up (its read fails, or it stops at a
    /// rev gap) the room drops `reply` unanswered rather than answer from a
    /// doc that may be stale: the caller treats a dropped channel as
    /// "unknown, ask again later".
    SuggestionApplied {
        suggestion_id: Uuid,
        apply_token: Uuid,
        reply: oneshot::Sender<bool>,
    },
    Shutdown,
    /// The room's log bookkeeping, for tests.
    #[cfg(test)]
    Probe(oneshot::Sender<Probe>),
}

/// What [`Event::Probe`] reports.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub applied_through_rev: i64,
    pub through_seq: i64,
    pub last_applied_seq: i64,
    pub max_own_rev: i64,
    pub in_flight: usize,
    pub unlogged: bool,
    pub content_epoch: i32,
}

pub struct Room {
    pub doc_id: Uuid,
    engine: Arc<dyn Engine>,
    doc: DocHandle,
    conns: HashMap<ConnId, ConnHandle>,
    /// The highest seq applied (own rows, rows read back, the boot's). Kept
    /// for `Event::ExportState` and the Markdown cache, which stay keyed by
    /// seq; the log watermark is `applied_through_rev`.
    last_applied_seq: i64,
    /// The page's content epoch at boot. The writer appends at it and
    /// snapshots are stamped with it.
    content_epoch: i32,
    /// The highest rev such that every row at or below it is applied.
    applied_through_rev: i64,
    /// The seq of the row at `applied_through_rev` (or of the snapshot the
    /// room booted from): the `snapshot_seq` of the next snapshot.
    through_seq: i64,
    /// Jobs handed to the writer and not yet confirmed (`Applied`).
    in_flight: usize,
    /// The highest rev the writer confirmed for this room's own rows.
    max_own_rev: i64,
    /// The doc holds content no log row carries (an update yrs rejected
    /// part-way, or a row that failed to apply and was passed): no snapshot
    /// is written for the rest of the room's life.
    unlogged: bool,
    bus: Arc<dyn Bus>,
    shutdown: CancellationToken,
    rx: mpsc::Receiver<Event>,
    bus_updates_rx: mpsc::Receiver<i64>,
    bus_presence_rx: mpsc::Receiver<Vec<u8>>,
    persist_tx: mpsc::Sender<PersistJob>,
    writer_events: mpsc::UnboundedReceiver<WriterEvent>,
    snapshots: Arc<dyn knot_storage::SnapshotStore>,
    updates_store: Arc<dyn knot_storage::UpdatesStore>,
    policy: crate::snapshot::SnapshotPolicy,
    snap_state: crate::snapshot::SnapshotState,
    /// Server-side reindex notifier (see [`crate::Rooms::with_dirty_tx`]).
    /// Fires after the writer confirms a successful persist; consumers
    /// debounce + reindex tasks for the doc.
    dirty_tx: Option<mpsc::Sender<Uuid>>,
    /// Set when the room evicts itself after applying an update it then
    /// refused: one that nested the page past
    /// [`crate::nesting::MAX_NESTING`] (the authoritative post-apply check),
    /// whether yrs applied all of it or rejected the rest. The live doc then
    /// holds the refused content, so the final flush must NOT snapshot it:
    /// the next join reloads the page from storage, which never got the
    /// refused update. Also set (`Room::evict`) when a server-side edit the
    /// doc holds was not persisted, or failed after it began to change the
    /// doc (`Room::persist_server_edit`; for an applied suggestion, so no
    /// snapshot carries that unpersisted marker to storage), when its writer
    /// sends a Fatal or closes its channel, and when a snapshot write finds
    /// the page's epoch changed or the page gone.
    evicted: bool,
}

pub struct RoomHandle {
    pub tx: mpsc::Sender<Event>,
    /// The room's lifetime. Cancel it to stop the room. The room cancels it
    /// itself when it evicts itself (`Room::refuse_applied`, or
    /// `Room::evict` for a server-side edit it could not persist or on a
    /// writer Fatal), and its actor
    /// cancels it whenever it ends, panics included. Once it is cancelled the
    /// registry treats the room as gone and boots a fresh one on the next
    /// join, and every WS shim of the room, which watches it, closes its
    /// socket so the client reconnects to that fresh room.
    pub shutdown: CancellationToken,
    /// Cancelled when the page's access ends for everyone and the room goes
    /// with it ([`crate::Rooms::revoke_and_evict`], for a purge). Each WS
    /// shim makes its connection's [`ConnHandle::revoked`] a child of it,
    /// so every socket closes with 4403 and its client leaves the page,
    /// rather than with 1012 and a reconnect to a page that is gone.
    pub revoked: CancellationToken,
    /// Resolves once the room's writer has written what it holds and ended,
    /// which can be after the room itself ended.
    pub writer_done: WriterDone,
}

/// What a room is built from, besides its booted doc and its writer.
struct RoomDeps {
    doc_id: Uuid,
    engine: Arc<dyn Engine>,
    bus: Arc<dyn Bus>,
    subscription: Subscription,
    updates_store: Arc<dyn knot_storage::UpdatesStore>,
    snapshots: Arc<dyn knot_storage::SnapshotStore>,
    policy: crate::snapshot::SnapshotPolicy,
    dirty_tx: Option<mpsc::Sender<Uuid>>,
}

impl Room {
    /// Spawn a room, booting its `DocHandle` from storage
    /// ([`crate::hydrate::boot`], room mode), and its writer with the default
    /// [`WriterLimits`]. Fails when a store read fails or the snapshot does
    /// not apply.
    #[allow(clippy::too_many_arguments)] // cohesive set of room dependencies
    pub async fn spawn(
        doc_id: Uuid,
        engine: Arc<dyn Engine>,
        bus: Arc<dyn Bus>,
        subscription: Subscription,
        updates_store: Arc<dyn knot_storage::UpdatesStore>,
        snapshots: Arc<dyn knot_storage::SnapshotStore>,
        policy: crate::snapshot::SnapshotPolicy,
        dirty_tx: Option<mpsc::Sender<Uuid>>,
    ) -> Result<RoomHandle, BootError> {
        Self::spawn_with_limits(
            doc_id,
            engine,
            bus,
            subscription,
            updates_store,
            snapshots,
            policy,
            dirty_tx,
            WriterLimits::default(),
        )
        .await
    }

    /// [`Room::spawn`] with the writer's limits given.
    #[allow(clippy::too_many_arguments)] // cohesive set of room dependencies
    pub async fn spawn_with_limits(
        doc_id: Uuid,
        engine: Arc<dyn Engine>,
        bus: Arc<dyn Bus>,
        subscription: Subscription,
        updates_store: Arc<dyn knot_storage::UpdatesStore>,
        snapshots: Arc<dyn knot_storage::SnapshotStore>,
        policy: crate::snapshot::SnapshotPolicy,
        dirty_tx: Option<mpsc::Sender<Uuid>>,
        limits: WriterLimits,
    ) -> Result<RoomHandle, BootError> {
        let deps = RoomDeps {
            doc_id,
            engine,
            bus,
            subscription,
            updates_store,
            snapshots,
            policy,
            dirty_tx,
        };
        let booted = Self::boot(&deps).await?;
        let (persist_tx, persist_rx) = mpsc::channel::<PersistJob>(1024);
        let (events_tx, writer_events) = mpsc::unbounded_channel();
        let writer_done = crate::writer::spawn(
            doc_id,
            booted.content_epoch,
            deps.updates_store.clone(),
            deps.bus.clone(),
            persist_rx,
            events_tx,
            limits,
        );
        Ok(Self::start(
            deps,
            booted,
            persist_tx,
            writer_events,
            writer_done,
        ))
    }

    /// [`Room::spawn`] without a writer: the test gets the room's persist
    /// receiver, the sender of its writer events and the sender that
    /// resolves its handle's `WriterDone` (by sending `true` or being
    /// dropped).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(crate) async fn spawn_detached(
        doc_id: Uuid,
        engine: Arc<dyn Engine>,
        bus: Arc<dyn Bus>,
        subscription: Subscription,
        updates_store: Arc<dyn knot_storage::UpdatesStore>,
        snapshots: Arc<dyn knot_storage::SnapshotStore>,
        policy: crate::snapshot::SnapshotPolicy,
        dirty_tx: Option<mpsc::Sender<Uuid>>,
    ) -> Result<
        (
            RoomHandle,
            mpsc::Receiver<PersistJob>,
            mpsc::UnboundedSender<WriterEvent>,
            tokio::sync::watch::Sender<bool>,
        ),
        BootError,
    > {
        let deps = RoomDeps {
            doc_id,
            engine,
            bus,
            subscription,
            updates_store,
            snapshots,
            policy,
            dirty_tx,
        };
        let booted = Self::boot(&deps).await?;
        let (persist_tx, persist_rx) = mpsc::channel::<PersistJob>(1024);
        let (events_tx, writer_events) = mpsc::unbounded_channel();
        let (done_tx, writer_done) = WriterDone::pair();
        let handle = Self::start(deps, booted, persist_tx, writer_events, writer_done);
        Ok((handle, persist_rx, events_tx, done_tx))
    }

    async fn boot(deps: &RoomDeps) -> Result<crate::hydrate::Boot, BootError> {
        let started = std::time::Instant::now();
        let booted = crate::hydrate::boot(
            deps.engine.as_ref(),
            deps.updates_store.as_ref(),
            deps.snapshots.as_ref(),
            deps.doc_id,
            BootMode::Room,
        )
        .await?;
        metrics::histogram!("knot_room_hydrate_seconds").record(started.elapsed().as_secs_f64());
        Ok(booted)
    }

    /// Spawn the actor over a booted doc and its writer's channels.
    fn start(
        deps: RoomDeps,
        booted: crate::hydrate::Boot,
        persist_tx: mpsc::Sender<PersistJob>,
        writer_events: mpsc::UnboundedReceiver<WriterEvent>,
        writer_done: WriterDone,
    ) -> RoomHandle {
        let (tx, rx) = mpsc::channel::<Event>(256);
        let shutdown = CancellationToken::new();
        let snap_state = crate::snapshot::SnapshotState {
            last_snapshot_seq: booted.through_seq,
            updates_since_snapshot: 0,
            last_apply_at: std::time::Instant::now(),
        };
        let room = Self {
            doc_id: deps.doc_id,
            engine: deps.engine,
            doc: booted.doc,
            conns: HashMap::new(),
            last_applied_seq: booted.last_seq,
            content_epoch: booted.content_epoch,
            applied_through_rev: booted.through_rev,
            through_seq: booted.through_seq,
            in_flight: 0,
            max_own_rev: 0,
            unlogged: booted.unlogged,
            bus: deps.bus,
            shutdown: shutdown.clone(),
            rx,
            bus_updates_rx: deps.subscription.updates,
            bus_presence_rx: deps.subscription.presence,
            persist_tx,
            writer_events,
            snapshots: deps.snapshots,
            updates_store: deps.updates_store,
            policy: deps.policy,
            snap_state,
            dirty_tx: deps.dirty_tx,
            evicted: false,
        };
        tokio::spawn(room.run());
        RoomHandle {
            tx,
            shutdown,
            revoked: CancellationToken::new(),
            writer_done,
        }
    }

    #[tracing::instrument(skip_all, fields(doc_id = %self.doc_id))]
    async fn run(mut self) {
        // However the actor ends (eviction, a shutdown, a panic), cancel the
        // room's token on the way out, so the shims close their sockets and
        // the registry stops handing out this room (see `RoomHandle`).
        let _ended = self.shutdown.clone().drop_guard();
        let mut idle_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut catchup_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                // Before the inbound events: a Fatal must end the room before
                // it applies and fans out more edits it can't persist.
                Some(event) = self.writer_events.recv() => match event {
                    WriterEvent::Applied(applied) => self.on_applied(applied).await,
                    WriterEvent::Fatal(reason) => {
                        self.evict(format_args!("writer: {reason}"));
                    }
                },
                _ = catchup_tick.tick() => {
                    let _ = self.catch_up().await;
                }
                _ = idle_tick.tick() => {
                    let idle = self.snap_state.last_apply_at.elapsed();
                    if self.snap_state.updates_since_snapshot > 0 && idle >= self.policy.idle {
                        self.snapshot().await;
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
                        // Only connections the room holds share presence, as
                        // only they may write (`on_inbound`): a refused or
                        // revoked one (or one dropped as a slow consumer)
                        // can keep sending until its socket is torn down.
                        if !self.conns.contains_key(&from) { continue; }
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
                    Some(Event::ServerFrame(frame)) => {
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
                            match conn.tx.try_send(frame.clone()) {
                                Ok(_) => {}
                                Err(_) => to_close.push(*cid),
                            }
                        }
                        for cid in to_close { self.conns.remove(&cid); }
                    }
                    Some(Event::BusUpdate(_)) | Some(Event::BusPresence(_)) => {}
                    Some(Event::ApplyUpdate { update_bytes, by_user, source, meta, reply }) => {
                        let result = self.on_apply_update(update_bytes, by_user, source, meta).await;
                        let _ = reply.send(result);
                    }
                    Some(Event::ExportState(reply)) => {
                        let r = self.engine
                            .encode_state_as_update(&self.doc, None)
                            .map(|bytes| (bytes, self.last_applied_seq));
                        let _ = reply.send(r);
                    }
                    Some(Event::ReplaceWithMarkdown { update_bytes, by_user, source, meta, reply }) => {
                        let result = self.on_replace(update_bytes, by_user, source, meta).await;
                        let _ = reply.send(result);
                    }
                    Some(Event::PatchTaskChecked { item_index, checked, by_user, reply }) => {
                        let result = self.on_patch_task(item_index, checked, by_user).await;
                        let _ = reply.send(result);
                    }
                    Some(Event::ApplySuggestion { request, by_user, accepted_by, reply }) => {
                        let result = self.on_apply_suggestion(request, by_user, accepted_by).await;
                        let _ = reply.send(result);
                    }
                    Some(Event::MoveBlocksOut { start, end, by_user, commit, reply }) => {
                        let result = self.on_move_blocks_out(start, end, by_user, commit).await;
                        let _ = reply.send(result);
                    }
                    Some(Event::SuggestionApplied { suggestion_id, apply_token, reply }) => {
                        use crate::suggestions::{APPLIED_MAP, is_applied};
                        use yrs::Transact;
                        // Without the catch-up the doc may lag an apply
                        // another replica persisted, and a stale "no" would
                        // let the sweep release a row whose text is already
                        // applied. Drop `reply` instead: the sweep skips the
                        // row and asks again on its next run.
                        if !self.catch_up().await {
                            tracing::warn!(
                                doc_id = %self.doc_id,
                                %suggestion_id,
                                "not caught up; leaving the applied check unanswered"
                            );
                            continue;
                        }
                        let applied = self.doc.inner().get_or_insert_map(APPLIED_MAP);
                        let txn = self.doc.inner().transact();
                        let _ = reply.send(is_applied(&applied, &txn, suggestion_id, apply_token));
                    }
                    Some(Event::Revoke) => {
                        // Tell every connection to close (the WS shim sends
                        // 4403) and drop them: from here on `on_inbound`
                        // ignores anything they still send.
                        for conn in self.conns.values() {
                            conn.revoked.cancel();
                        }
                        self.conns.clear();
                    }
                    Some(Event::Shutdown) | None => break,
                    #[cfg(test)]
                    Some(Event::Probe(reply)) => {
                        let _ = reply.send(Probe {
                            applied_through_rev: self.applied_through_rev,
                            through_seq: self.through_seq,
                            last_applied_seq: self.last_applied_seq,
                            max_own_rev: self.max_own_rev,
                            in_flight: self.in_flight,
                            unlogged: self.unlogged,
                            content_epoch: self.content_epoch,
                        });
                    }
                },
                Some(_rev) = self.bus_updates_rx.recv() => {
                    let _ = self.catch_up().await;
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

        // Final flush: a snapshot at the watermark so the next boot is
        // cheap, only when the snapshot rule holds (`may_snapshot`); it does
        // not wait for rows in flight, which the next boot replays. Skipped
        // when the room evicted itself (`refuse_applied`, `evict`, a
        // server-side edit it could not persist, a snapshot write that found the
        // epoch changed or the page gone): the live doc may hold content
        // storage lacks, and a snapshot of it would be the one thing that
        // persisted it.
        self.snapshot().await;
    }

    /// `Event::ApplyUpdate`: apply, measure, persist what the apply changed
    /// (`ApplyMeasured::persist`), and fan it out once it is stored. An
    /// update that does not decode changes nothing and is only refused; one
    /// that changes nothing is answered `Ok` with the room's seq.
    async fn on_apply_update(
        &mut self,
        update_bytes: Vec<u8>,
        by_user: Option<Uuid>,
        source: knot_storage::UpdateSource,
        meta: Option<serde_json::Value>,
    ) -> Result<i64, EngineError> {
        let limit = crate::nesting::MAX_NESTING;
        // `Err` here: it doesn't decode, so nothing of it was applied.
        let measured = self
            .engine
            .apply_update_measuring(&self.doc, &update_bytes, limit)?;
        // Checked first, as for an inbound update (`refuse_applied`): the
        // doc holds what the apply integrated, all of it or the part before
        // a rejection, and none of it may be persisted or fanned out. No
        // production caller gets here: they degrade deep Markdown before
        // they build the update.
        if measured.max_depth > limit {
            let why = format!(
                "it nested a type {} deep, past the limit of {limit}",
                measured.max_depth
            );
            self.evict(format_args!(
                "refused a server-side update after applying it ({why})"
            ));
            return Err(EngineError::Apply(why));
        }
        // yrs rejected the update part-way: the doc holds what it integrated,
        // which no row carries (`unlogged`), and the caller is told the edit
        // failed, so the room goes too.
        if let Some(error) = measured.rejected {
            self.unlogged = true;
            self.evict(format_args!(
                "yrs rejected a server-side update part-way ({error})"
            ));
            return Err(EngineError::Apply(error));
        }
        if !measured.changed {
            return Ok(self.last_applied_seq);
        }
        // The doc changed but there is nothing to persist, which
        // `ApplyMeasured::persist` rules out: the doc holds content no row
        // carries, and the caller must not hear that the edit was saved.
        if measured.persist.is_empty() {
            self.unlogged = true;
            self.evict(format_args!(
                "a server-side update changed the doc but left nothing to persist"
            ));
            return Err(EngineError::Apply("nothing to persist".into()));
        }
        metrics::counter!("knot_room_updates_total", "source" => "local").increment(1);
        let framed = wrap_sync_update(&measured.persist);
        let seq = self
            .persist_server_edit(measured.persist, by_user, source, meta)
            .await
            .map_err(EngineError::Apply)?;
        if measured.raw_fallback {
            metrics::counter!("knot_room_updates_persisted_raw_total").increment(1);
        }
        self.fan_out(&framed, None);
        Ok(seq)
    }

    /// `Event::ReplaceWithMarkdown`: decode the replacement, then clear the
    /// page and apply it in one transaction, whose own update is persisted
    /// and, once stored, fanned out.
    async fn on_replace(
        &mut self,
        update_bytes: Vec<u8>,
        by_user: Option<Uuid>,
        source: knot_storage::UpdateSource,
        meta: Option<serde_json::Value>,
    ) -> Result<i64, String> {
        use yrs::{Update, XmlFragment, updates::decoder::Decode};
        // Before anything is cleared: bytes that don't decode leave the page
        // as it was.
        let update = Update::decode_v1(&update_bytes).map_err(|e| format!("decode: {e}"))?;
        // Get (or create) the fragment reference BEFORE opening the mutable
        // transaction, because get_or_insert_xml_fragment internally
        // acquires its own write lock. Calling it while a TransactionMut is
        // active would deadlock (write_blocking).
        let frag = self.doc.inner().get_or_insert_xml_fragment("default");
        // Capture what the transaction ACTUALLY produces rather than reusing
        // `update_bytes`. The clear and the apply happen together, but
        // `update_bytes` encodes only the insertion — persisting and fanning
        // that out ships the insert without the delete, so every peer MERGES
        // the restored content into what it already had instead of replacing
        // it, yielding "<old text><restored text>".
        let (applied, effective) = self.capturing_update(|txn| {
            let len = frag.len(txn);
            if len > 0 {
                frag.remove_range(txn, 0, len);
            }
            txn.apply_update(update).map_err(|e| format!("apply: {e}"))
        });
        if let Err(e) = applied {
            // The page is cleared, and part of the update may be in it.
            self.evict(format_args!(
                "a replace failed after clearing the page ({e})"
            ));
            return Err(e);
        }
        // A replace that changed nothing (an empty page replaced by nothing)
        // emits no update; the doc may still hold structs the apply parked,
        // which no row would carry.
        let Some(effective) = effective else {
            self.evict(format_args!("a replace produced no update"));
            return Err("replace produced no update".into());
        };
        metrics::counter!("knot_room_updates_total", "source" => "restore").increment(1);
        let framed = wrap_sync_update(&effective);
        let seq = self
            .persist_server_edit(effective, by_user, source, meta)
            .await?;
        self.fan_out(&framed, None);
        Ok(seq)
    }

    /// `Event::PatchTaskChecked`: flip the task item's attribute, persist
    /// the change as a `task` row and, once stored, fan it out. An index
    /// with no task changes nothing.
    async fn on_patch_task(
        &mut self,
        item_index: i32,
        checked: bool,
        by_user: Option<Uuid>,
    ) -> Result<i64, String> {
        let frag = self.doc.inner().get_or_insert_xml_fragment("default");
        // `patch_task_checked` finds the item before it changes anything.
        let (patched, update) =
            self.capturing_update(|txn| patch_task_checked(&frag, txn, item_index, checked));
        patched?;
        // No update: the attribute already had the requested value.
        let Some(update) = update else {
            return Ok(self.last_applied_seq);
        };
        metrics::counter!("knot_room_updates_total", "source" => "patch").increment(1);
        let framed = wrap_sync_update(&update);
        let seq = self
            .persist_server_edit(update, by_user, knot_storage::UpdateSource::Task, None)
            .await?;
        self.fan_out(&framed, None);
        Ok(seq)
    }

    /// `Event::ApplySuggestion`: catch up, apply the suggestion and its
    /// applied marker in one transaction, persist that as a `suggestion`
    /// row by the suggester, naming the suggestion and the owner who
    /// accepted it, and fan it out once it is stored: a client that got the
    /// update would push it back on its resync after an eviction, making
    /// durable an apply whose accept already answered "persist failed".
    async fn on_apply_suggestion(
        &mut self,
        request: crate::suggestions::ApplySuggestionRequest,
        by_user: Option<Uuid>,
        accepted_by: Uuid,
    ) -> Result<i64, crate::suggestions::ApplySuggestionError> {
        use crate::suggestions::{APPLIED_MAP, ApplySuggestionError, apply_suggestion};
        // Catch up first: the anchored text may come from an edit another
        // replica persisted that this room's bus hasn't delivered yet. A
        // failed read, or one that stopped at a rev gap, is let through: at
        // worst it costs a spurious `AnchorLost` or `Changed`, which writes
        // nothing.
        let _ = self.catch_up().await;
        // Both roots BEFORE the mutable transaction (see `on_replace`: they
        // take their own write lock).
        let _frag = self.doc.inner().get_or_insert_xml_fragment("default");
        let applied = self.doc.inner().get_or_insert_map(APPLIED_MAP);
        let (result, update) =
            self.capturing_update(|txn| apply_suggestion(&applied, txn, &request));
        // Every check runs before the first mutation, so a refusal left the
        // doc as it was.
        result?;
        // An apply always records its marker, so it can't be a no-op; if
        // nothing was captured, the doc holds a change we can't persist.
        let Some(update) = update else {
            self.evict(format_args!(
                "could not persist applied suggestion {}: no update",
                request.suggestion_id
            ));
            return Err(ApplySuggestionError::Persist("no update".into()));
        };
        metrics::counter!("knot_room_updates_total", "source" => "suggestion").increment(1);
        let framed = wrap_sync_update(&update);
        let meta = serde_json::json!({
            "suggestion_id": request.suggestion_id,
            "accepted_by": accepted_by,
        });
        let seq = self
            .persist_server_edit(
                update,
                by_user,
                knot_storage::UpdateSource::Suggestion,
                Some(meta),
            )
            .await
            .map_err(ApplySuggestionError::Persist)?;
        self.fan_out(&framed, None);
        Ok(seq)
    }

    /// `Event::MoveBlocksOut`: see there.
    async fn on_move_blocks_out(
        &mut self,
        start: Vec<u8>,
        end: Vec<u8>,
        by_user: Option<Uuid>,
        commit: crate::move_blocks::Commit,
    ) -> Result<i64, crate::move_blocks::MoveBlocksError> {
        use crate::move_blocks::{MoveBlocksError, Prepared, block_range, child_state, cut};
        use yrs::{ReadTxn, StateVector, Transact};
        // As `on_apply_suggestion`: the selected text may come from another
        // replica's edit this room hasn't seen yet.
        let _ = self.catch_up().await;
        // Before any transaction (see `on_replace`).
        let frag = self.doc.inner().get_or_insert_xml_fragment("default");
        let (first, last, state) = {
            let txn = self.doc.inner().transact();
            let (first, last) = block_range(&txn, &frag, &start, &end)?;
            (
                first,
                last,
                txn.encode_state_as_update_v1(&StateVector::default()),
            )
        };
        let child_state = child_state(&state, first, last)?;
        // ponytail: the room handles no other event until the commit's one DB
        // transaction ends; add a deadline if that ever takes long.
        let link = commit(Prepared { child_state })
            .await
            .map_err(MoveBlocksError::Refused)?;
        let ((), update) = self.capturing_update(|txn| {
            cut(&frag, txn, first, last, &link.title, &link.href);
        });
        let Some(update) = update else {
            self.evict(format_args!("a move to subpage produced no update"));
            return Err(MoveBlocksError::Persist("no update".into()));
        };
        metrics::counter!("knot_room_updates_total", "source" => "move").increment(1);
        let framed = wrap_sync_update(&update);
        let seq = self
            .persist_server_edit(
                update,
                by_user,
                knot_storage::UpdateSource::Edit,
                Some(link.meta),
            )
            .await
            .map_err(MoveBlocksError::Persist)?;
        self.fan_out(&framed, None);
        Ok(seq)
    }

    /// Runs `mutate` in one transaction on the live doc and returns its
    /// result with the update the transaction produced (`None` when it
    /// changed nothing).
    fn capturing_update<R>(
        &self,
        mutate: impl FnOnce(&mut yrs::TransactionMut) -> R,
    ) -> (R, Option<Vec<u8>>) {
        use std::sync::Mutex;
        use yrs::Transact;
        let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        let sub = self.doc.inner().observe_update_v1(move |_txn, e| {
            *sink.lock().unwrap() = Some(e.update.clone());
        });
        let result = {
            let mut txn = self.doc.inner().transact_mut();
            mutate(&mut txn)
        };
        // Dropping the subscription must happen AFTER the txn closes (Drop
        // on TransactionMut flushes the update notification).
        drop(sub);
        let update = captured.lock().unwrap().take();
        (result, update)
    }

    /// Hands a server-side edit the doc already holds to the writer, as one
    /// row of `source`, and waits until the writer stored it: only then may
    /// the caller fan it out, so an edit that is not stored reaches no
    /// client, and no reconnect can push it back credited to someone else.
    /// The writer bounds the wait ([`WriterLimits`]). Returns the row's seq.
    /// When the edit is not stored the room is evicted: its doc holds an
    /// edit no row carries.
    async fn persist_server_edit(
        &mut self,
        bytes: Vec<u8>,
        by_user: Option<Uuid>,
        source: knot_storage::UpdateSource,
        meta: Option<serde_json::Value>,
    ) -> Result<i64, String> {
        let (persisted, stored) = oneshot::channel();
        let job = PersistJob {
            bytes,
            by_user_id: by_user,
            source,
            meta,
            persisted: Some(persisted),
        };
        // `persist` evicts the room when the writer's channel is closed.
        if !self.persist(job).await {
            return Err("persist channel closed".into());
        }
        let error = match stored.await {
            Ok(Ok(seq)) => return Ok(seq),
            Ok(Err(e)) => format!("persist: {e}"),
            Err(_) => "persist channel closed".to_string(),
        };
        self.evict(format_args!(
            "could not persist a server-side {} edit ({error})",
            source.as_str()
        ));
        Err(error)
    }

    /// The writer appended one of this room's rows at `rev`.
    async fn on_applied(&mut self, Applied { seq, rev }: Applied) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.max_own_rev = self.max_own_rev.max(rev);
        self.last_applied_seq = self.last_applied_seq.max(seq);
        if rev == self.applied_through_rev + 1 {
            self.applied_through_rev = rev;
            self.through_seq = seq;
        } else if rev > self.applied_through_rev + 1 {
            // Other rows committed between the watermark and this one (other
            // replicas', 0.9.9 pods', or this writer's own rows of an attempt
            // whose acknowledgement was lost): revs are gapless, so read
            // them now.
            let _ = self.catch_up().await;
        }
        self.snap_state.updates_since_snapshot += 1;
        self.snap_state.last_apply_at = std::time::Instant::now();
        // Notify the server-side reindex worker that this doc has new
        // content. `try_send` so a full channel is dropped — the next
        // applied update will refire.
        if let Some(tx) = &self.dirty_tx {
            let _ = tx.try_send(self.doc_id);
        }
        if self.snap_state.updates_since_snapshot >= self.policy.every_n {
            self.snapshot().await;
        }
    }

    /// Hands `job` to the writer, counting it in flight. A closed channel
    /// means the writer gave up after a Fatal the room has not read yet: the
    /// room is evicted, and the caller neither fans the update out nor
    /// reports it persisted. Returns whether the job was handed over.
    async fn persist(&mut self, job: PersistJob) -> bool {
        match self.persist_tx.send(job).await {
            Ok(()) => {
                self.in_flight += 1;
                true
            }
            Err(_) => {
                self.evict(format_args!("writer: its channel is closed"));
                false
            }
        }
    }

    /// Apply the rows persisted past the watermark (other replicas' edits
    /// the bus may not have delivered yet, rows written past a gap), in rev
    /// order, as long as they are contiguous. A row that changes the doc is
    /// fanned out; one that fails to apply is passed (logged, counted, the
    /// room becomes `unlogged`, not fanned out). Returns whether the doc is
    /// now caught up with storage as of the read: `false` when the read
    /// failed, when it stopped at a rev gap, or when the log does not reach
    /// a rev the writer confirmed for this room.
    async fn catch_up(&mut self) -> bool {
        let rows = match self
            .updates_store
            .rows_after(
                self.doc_id,
                knot_storage::RowsAfter::Rev(self.applied_through_rev),
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!(error = ?e, "catch-up read failed");
                return false;
            }
        };
        for u in rows {
            let Some(rev) = u.rev else { continue };
            if rev <= self.applied_through_rev {
                continue;
            }
            if rev != self.applied_through_rev + 1 {
                metrics::counter!("knot_room_rev_gap_total").increment(1);
                tracing::warn!(
                    doc_id = %self.doc_id,
                    applied_through_rev = self.applied_through_rev,
                    next_rev = rev,
                    "rev gap in the page's log; catching up again on the next tick"
                );
                return false;
            }
            match self.engine.apply_update_changed(&self.doc, &u.update_bytes) {
                Ok(true) => {
                    metrics::counter!("knot_room_updates_total", "source" => "peer").increment(1);
                    self.fan_out(&wrap_sync_update(&u.update_bytes), None);
                    // Notify the server-side reindex worker so non-writing
                    // replicas refresh their /tasks index when an update
                    // arrives via the bus. Without this, only the replica
                    // that owns the WS connection ever ticks the indexer.
                    if let Some(tx) = &self.dirty_tx {
                        let _ = tx.try_send(self.doc_id);
                    }
                }
                // Already in the doc: an own row read back, or a row
                // written twice after a lost acknowledgement.
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        doc_id = %self.doc_id,
                        rev,
                        seq = u.seq,
                        %error,
                        "a log row failed to apply in catch-up; passed, and the room writes no more snapshots"
                    );
                    metrics::counter!("knot_room_row_apply_errors_total", "phase" => "catch_up")
                        .increment(1);
                    self.unlogged = true;
                }
            }
            self.applied_through_rev = rev;
            self.through_seq = u.seq;
            self.last_applied_seq = self.last_applied_seq.max(u.seq);
        }
        if self.applied_through_rev < self.max_own_rev {
            // The writer confirmed a row at `max_own_rev`, so every row below
            // it was committed before it: the log lacks rows it must hold.
            metrics::counter!("knot_room_rev_gap_total").increment(1);
            tracing::warn!(
                doc_id = %self.doc_id,
                applied_through_rev = self.applied_through_rev,
                max_own_rev = self.max_own_rev,
                "the page's log does not reach the room's own rev; catching up again on the next tick"
            );
            return false;
        }
        true
    }

    /// Send `frame` to every connection but `except`, dropping a slow or
    /// closed one.
    fn fan_out(&mut self, frame: &[u8], except: Option<ConnId>) {
        let mut to_close: Vec<ConnId> = Vec::new();
        for (cid, conn) in &self.conns {
            if Some(*cid) == except {
                continue;
            }
            if conn.tx.try_send(frame.to_vec()).is_err() {
                to_close.push(*cid);
            }
        }
        for cid in to_close {
            self.conns.remove(&cid);
        }
    }

    /// Whether a snapshot of the doc now holds exactly the page's
    /// pre-upgrade base and the log up to `applied_through_rev`: not
    /// evicted, nothing unlogged in the doc, no row in flight (the doc holds
    /// it, the log not yet) and no own row past the watermark (the doc holds
    /// it, but not the rows below it).
    fn may_snapshot(&self) -> bool {
        !self.evicted
            && !self.unlogged
            && self.in_flight == 0
            && self.max_own_rev <= self.applied_through_rev
    }

    /// Write a snapshot stamped with the watermark, if the snapshot rule
    /// holds. A write that finds the page's epoch changed evicts the room;
    /// one that finds the page gone ends it quietly (nothing broke). Either
    /// way no further snapshot is written.
    async fn snapshot(&mut self) {
        if !self.may_snapshot() {
            return;
        }
        let stamp = knot_storage::SnapshotStamp {
            snapshot_seq: self.through_seq,
            snapshot_rev: Some(self.applied_through_rev),
            epoch: Some(self.content_epoch),
        };
        match crate::snapshot::write_snapshot(
            self.doc_id,
            stamp,
            self.engine.as_ref(),
            &self.doc,
            self.snapshots.as_ref(),
        )
        .await
        {
            Ok(()) => {
                metrics::counter!("knot_room_snapshots_total").increment(1);
                self.snap_state.last_snapshot_seq = self.through_seq;
                self.snap_state.updates_since_snapshot = 0;
            }
            Err(crate::snapshot::SnapshotWriteError::EpochChanged) => {
                self.evict(format_args!("snapshot: the page's history epoch changed"));
            }
            Err(crate::snapshot::SnapshotWriteError::DocGone) => {
                tracing::debug!(doc_id = %self.doc_id, "snapshot: the page is gone; ending the room");
                self.evicted = true;
                self.shutdown.cancel();
            }
            Err(crate::snapshot::SnapshotWriteError::Other(error)) => {
                tracing::warn!(doc_id = %self.doc_id, %error, "snapshot write failed");
            }
        }
    }

    async fn on_join(
        &mut self,
        conn_id: ConnId,
        handle: ConnHandle,
        reply: oneshot::Sender<Result<JoinState, EngineError>>,
    ) {
        // A re-join (client SyncStep1) racing a revoke, or queued behind an
        // update the room refused, still carries the connection's cancelled
        // handle. Refuse it by dropping `reply`: no state for someone whose
        // access just ended or who was just refused, and no re-registration
        // that would let their updates through again.
        if handle.revoked.is_cancelled() || handle.refused.is_cancelled() {
            return;
        }
        self.conns.insert(conn_id, handle);
        let _ = reply.send(JoinState::of(self.engine.as_ref(), &self.doc));
    }

    #[tracing::instrument(skip(self, m), fields(doc_id = %self.doc_id, bytes = m.bytes.len()))]
    async fn on_inbound(&mut self, m: InMsg) {
        // Only connections the room still holds may write. A revoked one (or
        // one dropped as a slow consumer) can keep sending until its socket
        // is torn down; its access ended when it left `conns`.
        if !self.conns.contains_key(&m.from) {
            tracing::debug!(conn = %m.from, "dropping update from a connection the room no longer holds");
            return;
        }
        // Prefilter, before anything of it is applied: `check_update`
        // predicts where yrs will place the update's types and refuses one
        // it predicts nests too deep; nothing of it is applied, persisted or
        // fanned out, and the sender is closed. It is best-effort: its
        // prediction can be wrong, so it does not guarantee that applying
        // the update stays within the worker's stack (see `crate::nesting`
        // for what is not guarded). The authoritative check runs after
        // apply below.
        if let Err(too_deep) = crate::nesting::check_update(&self.doc, &m.bytes) {
            tracing::warn!(
                doc_id = %self.doc_id,
                conn = %m.from,
                depth = too_deep.depth,
                limit = crate::nesting::MAX_NESTING,
                "refused an update that nests the page too deep (prefilter); closing its connection"
            );
            self.refuse_sender(m.from);
            return;
        }
        // Apply, then run the authoritative nesting check on the real doc:
        // the prefilter predicts yrs and can be bypassed (an update that
        // hangs a short chain of new types off existing legal content nests
        // past the limit without its own structure being deep).
        let measured = match self.engine.apply_update_measuring(
            &self.doc,
            &m.bytes,
            crate::nesting::MAX_NESTING,
        ) {
            // Measured over what the apply integrated, also when yrs
            // rejected the rest of the update.
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
            // yrs rejected the update part-way, within the limit: as on main,
            // nothing of it is persisted or fanned out, and the room keeps
            // running with every connection. The error need not be the
            // sender's: yrs retries the blocks it parked earlier for a
            // missing dependency within the same apply and passes on their
            // error, so closing the sender could lock an innocent writer out
            // for as long as the parked block is stored. The doc now holds
            // what yrs integrated, which no row carries: the room writes no
            // more snapshots (`unlogged`).
            Ok(ApplyMeasured {
                rejected: Some(error),
                ..
            }) => {
                tracing::debug!(
                    doc_id = %self.doc_id,
                    conn = %m.from,
                    %error,
                    "yrs rejected an update part-way; neither persisted nor fanned out"
                );
                self.unlogged = true;
                return;
            }
            // Nothing new — typically the SyncStep2 a (re)connecting writer
            // sends in answer to the server's SyncStep1, which re-sends the
            // whole delete set even when the client has no offline edits.
            // Persisting it would write a `doc_updates` row on every page
            // open (and attribute an edit to someone who only looked);
            // fanning it out would make every peer re-apply a no-op.
            Ok(applied) if !applied.changed => return,
            // The doc changed but there is nothing to persist, which
            // `ApplyMeasured::persist` rules out once the rejected case above
            // is handled. Should it happen, the doc holds content no row
            // carries: as for a rejected update, the room writes no more
            // snapshots (`unlogged`), which would certify that content.
            Ok(applied) if applied.persist.is_empty() => {
                tracing::warn!(
                    doc_id = %self.doc_id,
                    conn = %m.from,
                    "an update changed the doc but left nothing to persist; neither persisted nor fanned out"
                );
                self.unlogged = true;
                return;
            }
            Ok(applied) => applied,
            // It doesn't decode, so nothing of it was applied: refuse its
            // sender, which is broken or crafted, and no one else.
            Err(e) => {
                tracing::warn!(
                    doc_id = %self.doc_id,
                    conn = %m.from,
                    error = %e,
                    "refused an update that doesn't decode; closing its connection"
                );
                self.refuse_sender(m.from);
                return;
            }
        };
        metrics::counter!("knot_room_updates_total", "source" => "local").increment(1);
        // What the apply changed (a resend persists only what is new), or
        // the raw update when content is pending (`ApplyMeasured::persist`).
        // Persisted first, and fanned out only once the writer has it: an
        // edit the writer can no longer take reaches no one.
        let framed = wrap_sync_update(&measured.persist);
        let handed_over = self
            .persist(PersistJob {
                bytes: measured.persist,
                by_user_id: m.by_user,
                source: knot_storage::UpdateSource::Edit,
                meta: None,
                // Inbound WS updates are fire-and-forget — the client
                // already has the update locally, so we don't need to
                // hold up acknowledgement on the durable insert.
                persisted: None,
            })
            .await;
        if !handed_over {
            return;
        }
        if measured.raw_fallback {
            metrics::counter!("knot_room_updates_persisted_raw_total").increment(1);
        }
        self.fan_out(&framed, Some(m.from));
    }

    /// Refuse an update `from` sent: count it, drop the connection, which
    /// `on_join` doesn't take back, so that nothing more it sends is applied
    /// or shared, and tell its WS shim to close it with 4422
    /// (`ConnHandle::refused`).
    fn refuse_sender(&mut self, from: ConnId) {
        metrics::counter!("knot_room_updates_refused_total").increment(1);
        if let Some(conn) = self.conns.remove(&from) {
            conn.refused.cancel();
        }
    }

    /// Refuse an inbound update the live doc already holds, all of it or
    /// the part yrs integrated before it rejected the rest, because it
    /// nested the page past [`crate::nesting::MAX_NESTING`]. The caller
    /// persists and fans out nothing of it; this closes the sender and
    /// evicts the room. The live doc can't be trusted any more, so the room
    /// ends without its final snapshot (`evicted`), and the next join
    /// reloads the page from storage, which never got the update. Dropping
    /// the doc doesn't recurse, however deep the update made it
    /// (`crate::nesting`).
    ///
    /// Every other connection is closed too, through the room's token
    /// (`RoomHandle::shutdown`), which its WS shim watches: it closes the
    /// socket with 1012, and the client reconnects to the reloaded room and
    /// re-sends its edits in its answer to the server's SyncStep1. Without
    /// that, a peer would keep editing on this dead room. The sender's
    /// `refused` is cancelled first, so the sender is closed with 4422.
    fn refuse_applied(&mut self, from: ConnId, why: std::fmt::Arguments) {
        tracing::warn!(
            doc_id = %self.doc_id,
            conn = %from,
            "refused an update after applying it ({why}); closing its connection and evicting the room"
        );
        metrics::counter!("knot_room_evicted_total").increment(1);
        self.refuse_sender(from);
        self.evicted = true;
        // Ends the actor (the loop's next turn) and closes every other
        // connection (see above).
        self.shutdown.cancel();
    }

    /// Evict the room: it ends without its final snapshot (`evicted`), and
    /// every connection is closed through the room's token, so clients
    /// reconnect to a room reloaded from storage and resend their edits.
    fn evict(&mut self, why: std::fmt::Arguments) {
        tracing::warn!(doc_id = %self.doc_id, "{why}; evicting the room");
        metrics::counter!("knot_room_evicted_total").increment(1);
        self.evicted = true;
        self.shutdown.cancel();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{MemBus, YrsEngine};

    /// Register a connection with the room, as the WS shim does before it
    /// forwards anything: the room only accepts updates from connections it
    /// holds. Keep the receiver alive — a closed one gets the conn dropped
    /// on the next fan-out.
    pub(crate) async fn join(h: &RoomHandle) -> (ConnId, mpsc::Receiver<Vec<u8>>) {
        let conn_id = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(64);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id,
            handle: ConnHandle::new(tx),
            reply: reply_tx,
        })
        .await
        .unwrap();
        reply_rx.await.unwrap().unwrap();
        (conn_id, rx)
    }

    pub(crate) struct NoopUpdates;
    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for NoopUpdates {
        async fn append(
            &self,
            _: Uuid,
            _: i32,
            rows: &[knot_storage::NewUpdate],
        ) -> Result<Vec<knot_storage::Persisted>, knot_storage::UpdatesStoreError> {
            Ok((1..=rows.len() as i64)
                .map(|seq| knot_storage::Persisted { seq, rev: seq })
                .collect())
        }
        async fn rows_after(
            &self,
            _: Uuid,
            _: knot_storage::RowsAfter,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            Ok(vec![])
        }
        async fn head(
            &self,
            _: Uuid,
        ) -> Result<knot_storage::HistoryHead, knot_storage::UpdatesStoreError> {
            Ok(knot_storage::HistoryHead::default())
        }
        async fn ensure_legacy_numbered(
            &self,
            _: Uuid,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
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
    }

    /// Records the author of every row the writer persists, in order, so
    /// tests can assert attribution reaches the store.
    struct RecordingUpdates {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Option<Uuid>>>>,
    }
    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for RecordingUpdates {
        async fn append(
            &self,
            _: Uuid,
            _: i32,
            rows: &[knot_storage::NewUpdate],
        ) -> Result<Vec<knot_storage::Persisted>, knot_storage::UpdatesStoreError> {
            let mut seen = self.seen.lock().unwrap();
            let first = seen.len() as i64 + 1;
            seen.extend(rows.iter().map(|row| row.by_user_id));
            Ok((first..first + rows.len() as i64)
                .map(|seq| knot_storage::Persisted { seq, rev: seq })
                .collect())
        }
        async fn rows_after(
            &self,
            _: Uuid,
            _: knot_storage::RowsAfter,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            Ok(vec![])
        }
        async fn head(
            &self,
            _: Uuid,
        ) -> Result<knot_storage::HistoryHead, knot_storage::UpdatesStoreError> {
            Ok(knot_storage::HistoryHead::default())
        }
        async fn ensure_legacy_numbered(
            &self,
            _: Uuid,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
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
    }

    /// Records every update the writer persists, in order.
    struct CapturingUpdates {
        persisted: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }
    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for CapturingUpdates {
        async fn append(
            &self,
            _: Uuid,
            _: i32,
            rows: &[knot_storage::NewUpdate],
        ) -> Result<Vec<knot_storage::Persisted>, knot_storage::UpdatesStoreError> {
            let mut p = self.persisted.lock().unwrap();
            let first = p.len() as i64 + 1;
            p.extend(rows.iter().map(|row| row.bytes.clone()));
            Ok((first..first + rows.len() as i64)
                .map(|seq| knot_storage::Persisted { seq, rev: seq })
                .collect())
        }
        async fn rows_after(
            &self,
            _: Uuid,
            _: knot_storage::RowsAfter,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            Ok(vec![])
        }
        async fn head(
            &self,
            _: Uuid,
        ) -> Result<knot_storage::HistoryHead, knot_storage::UpdatesStoreError> {
            Ok(knot_storage::HistoryHead::default())
        }
        async fn ensure_legacy_numbered(
            &self,
            _: Uuid,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
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
    }

    pub(crate) struct NoopSnapshots;
    #[async_trait::async_trait]
    impl knot_storage::SnapshotStore for NoopSnapshots {
        async fn insert_stamped(
            &self,
            _: Uuid,
            _: knot_storage::SnapshotStamp,
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
        async fn list(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Vec<knot_storage::SnapshotMeta>, knot_storage::SnapshotStoreError> {
            Ok(vec![])
        }
        async fn by_seq(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Option<knot_storage::DocSnapshot>, knot_storage::SnapshotStoreError> {
            Ok(None)
        }
    }

    #[test]
    fn patch_task_checked_flips_attr_at_index() {
        use yrs::{Doc, Transact, Xml, XmlElementPrelim, XmlFragment};
        let doc = Doc::new();
        let frag = doc.get_or_insert_xml_fragment("default");
        // Build a doc with a bullet_list containing three list_items —
        // first and third carry a `checked` attr (so the index over
        // task-items is 0, 1; the second is a plain bullet, skipped).
        {
            let mut txn = doc.transact_mut();
            let list = frag.push_back(&mut txn, XmlElementPrelim::empty("bullet_list"));
            let li0 = list.push_back(&mut txn, XmlElementPrelim::empty("list_item"));
            li0.insert_attribute(&mut txn, "checked", "false");
            let _plain = list.push_back(&mut txn, XmlElementPrelim::empty("list_item"));
            let li2 = list.push_back(&mut txn, XmlElementPrelim::empty("list_item"));
            li2.insert_attribute(&mut txn, "checked", "false");
        }
        {
            let mut txn = doc.transact_mut();
            super::patch_task_checked(&frag, &mut txn, 1, true).unwrap();
        }
        // Inspect: only the SECOND task-item (third overall) should be
        // checked=true.
        let txn = doc.transact();
        let len = frag.len(&txn);
        let yrs::XmlOut::Element(list) = frag.get(&txn, 0).unwrap() else {
            panic!("expected list at top of frag");
        };
        let _ = len;
        let mut snapshot: Vec<Option<String>> = Vec::new();
        for i in 0..list.len(&txn) {
            if let Some(yrs::XmlOut::Element(li)) = list.get(&txn, i) {
                snapshot.push(li.get_attribute(&txn, "checked").map(|v| v.to_string(&txn)));
            }
        }
        assert_eq!(
            snapshot,
            vec![Some("false".into()), None, Some("true".into())]
        );
    }

    #[test]
    fn patch_task_checked_returns_err_for_out_of_range() {
        use yrs::{Doc, Transact};
        let doc = Doc::new();
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        let err = super::patch_task_checked(&frag, &mut txn, 5, true).unwrap_err();
        assert!(err.starts_with("no task at index"));
    }

    #[test]
    fn patch_task_checked_rejects_negative_index() {
        use yrs::{Doc, Transact};
        let doc = Doc::new();
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        assert!(super::patch_task_checked(&frag, &mut txn, -1, true).is_err());
    }

    /// Build update bytes that populate the "default" XmlFragment with a
    /// single paragraph containing the given text. Uses raw yrs APIs to
    /// avoid a circular dependency on knot-markdown.
    fn make_replace_bytes_raw(text: &str) -> Vec<u8> {
        use yrs::{Doc, ReadTxn, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};
        let doc = Doc::new();
        {
            // Get fragment first (internal transact_mut), then open our own txn.
            let frag = doc.get_or_insert_xml_fragment("default");
            let mut txn = doc.transact_mut();
            let para = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            para.push_back(&mut txn, XmlTextPrelim::new(text));
        }
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    #[tokio::test]
    async fn replace_with_markdown_swaps_content() {
        use yrs::{GetString, Transact};

        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let updates: Arc<dyn knot_storage::UpdatesStore> = Arc::new(NoopUpdates);
        let snapshots: Arc<dyn knot_storage::SnapshotStore> = Arc::new(NoopSnapshots);
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let policy = crate::snapshot::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(60),
        };
        let h = Room::spawn(
            doc_id,
            engine.clone(),
            bus,
            sub,
            updates,
            snapshots,
            policy,
            None,
        )
        .await
        .unwrap();

        // Seed initial content.
        let initial_bytes = make_replace_bytes_raw("Hello World");
        let (tx, rx) = tokio::sync::oneshot::channel();
        h.tx.send(Event::ReplaceWithMarkdown {
            update_bytes: initial_bytes,
            by_user: None,
            source: knot_storage::UpdateSource::Import,
            meta: None,
            reply: tx,
        })
        .await
        .unwrap();
        assert!(rx.await.unwrap().is_ok());

        // Replace with different content.
        let replacement_bytes = make_replace_bytes_raw("Replaced Content");
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        h.tx.send(Event::ReplaceWithMarkdown {
            update_bytes: replacement_bytes,
            by_user: None,
            source: knot_storage::UpdateSource::Import,
            meta: None,
            reply: tx2,
        })
        .await
        .unwrap();
        assert!(rx2.await.unwrap().is_ok());

        // Export and verify the content matches the replacement.
        let (tx3, rx3) = tokio::sync::oneshot::channel();
        h.tx.send(Event::ExportState(tx3)).await.unwrap();
        let (state_bytes, _seq) = rx3.await.unwrap().unwrap();
        let transient = engine.new_doc();
        engine.apply_update(&transient, &state_bytes).unwrap();
        let doc_inner = transient.inner();
        let frag = doc_inner.get_or_insert_xml_fragment("default");
        let txn = doc_inner.transact();
        let content = frag.get_string(&txn);
        assert!(
            content.contains("Replaced Content"),
            "expected 'Replaced Content' in output: {content:?}"
        );
        assert!(
            !content.contains("Hello World"),
            "expected 'Hello World' to be gone: {content:?}"
        );

        h.shutdown.cancel();
    }

    /// A restore must REPLACE the content a connected peer already holds, not
    /// merge into it.
    ///
    /// The room's own doc was always correct (the test above covers that); the
    /// bug was in what got fanned out. `ReplaceWithMarkdown` clears the fragment
    /// and applies the new content in one transaction, but used to broadcast the
    /// caller's `update_bytes` — which encodes only the insertion. A peer
    /// applying that kept its old content and appended the restored text,
    /// producing "Hello WorldReplaced Content" in the editor.
    ///
    /// So this asserts on the frame the room actually sends, applied to a peer
    /// holding the pre-restore state. Checking the room's own doc cannot catch
    /// the regression.
    #[tokio::test]
    async fn replace_broadcast_replaces_peer_content() {
        use yrs::{GetString, Transact};

        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let updates: Arc<dyn knot_storage::UpdatesStore> = Arc::new(NoopUpdates);
        let snapshots: Arc<dyn knot_storage::SnapshotStore> = Arc::new(NoopSnapshots);
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let policy = crate::snapshot::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(60),
        };
        let h = Room::spawn(
            doc_id,
            engine.clone(),
            bus,
            sub,
            updates,
            snapshots,
            policy,
            None,
        )
        .await
        .unwrap();

        // Seed the room, then join a peer so it starts from that state.
        let (tx, rx) = tokio::sync::oneshot::channel();
        h.tx.send(Event::ReplaceWithMarkdown {
            update_bytes: make_replace_bytes_raw("Hello World"),
            by_user: None,
            source: knot_storage::UpdateSource::Import,
            meta: None,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap().unwrap();

        let (conn_tx, mut conn_rx) = mpsc::channel::<Vec<u8>>(64);
        let (join_tx, join_rx) = tokio::sync::oneshot::channel();
        h.tx.send(Event::Join {
            conn_id: Uuid::new_v4(),
            handle: ConnHandle::new(conn_tx),
            reply: join_tx,
        })
        .await
        .unwrap();
        let peer_state = join_rx.await.unwrap().unwrap().update;

        let peer = engine.new_doc();
        engine.apply_update(&peer, &peer_state).unwrap();
        {
            let inner = peer.inner();
            let frag = inner.get_or_insert_xml_fragment("default");
            let txn = inner.transact();
            assert!(
                frag.get_string(&txn).contains("Hello World"),
                "peer should start from the seeded content"
            );
        }

        // Restore over the top of what the peer is holding.
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        h.tx.send(Event::ReplaceWithMarkdown {
            update_bytes: make_replace_bytes_raw("Replaced Content"),
            by_user: None,
            source: knot_storage::UpdateSource::Import,
            meta: None,
            reply: tx2,
        })
        .await
        .unwrap();
        rx2.await.unwrap().unwrap();

        // Apply every SYNC_UPDATE frame the room pushed, exactly as a client would.
        let mut frames = 0;
        while let Ok(frame) = conn_rx.try_recv() {
            // [MSG_SYNC=0, SYNC_UPDATE=2, varuint(len), payload]
            assert_eq!(&frame[..2], &[0u8, 2u8], "expected a SYNC_UPDATE frame");
            let mut i = 2;
            let mut len = 0u64;
            let mut shift = 0;
            loop {
                let b = frame[i];
                i += 1;
                len |= u64::from(b & 0x7f) << shift;
                if b & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            engine
                .apply_update(&peer, &frame[i..i + len as usize])
                .unwrap();
            frames += 1;
        }
        assert!(frames > 0, "room fanned out nothing for the replace");

        let inner = peer.inner();
        let frag = inner.get_or_insert_xml_fragment("default");
        let txn = inner.transact();
        let content = frag.get_string(&txn);
        assert!(
            content.contains("Replaced Content"),
            "peer missing the restored text: {content:?}"
        );
        assert!(
            !content.contains("Hello World"),
            "peer MERGED instead of replacing — the broadcast is missing the \
             deletion half of the transaction: {content:?}"
        );

        h.shutdown.cancel();
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
            None,
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

        let pool = knot_test_support::fresh_db().await.pool;

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
        // Second subscription so the test can observe the writer's publish,
        // which happens only after the batch is durably inserted.
        let mut persisted = bus.subscribe(d.id).await.unwrap();
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
            None,
        )
        .await
        .unwrap();

        // Produce an actual yrs update from a separate doc. It must change
        // something: the room drops updates that leave its doc unchanged, so
        // the bytes of an empty doc would never reach the writer.
        let real_update = {
            use yrs::{ReadTxn, Text, Transact};
            let other = yrs::Doc::new();
            other
                .get_or_insert_text("t")
                .push(&mut other.transact_mut(), "hello");
            other
                .transact()
                .encode_state_as_update_v1(&yrs::StateVector::default())
        };

        // Join + send.
        let conn_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel(8);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id,
            handle: ConnHandle::new(tx),
            reply: reply_tx,
        })
        .await
        .unwrap();
        let _ = reply_rx.await.unwrap().unwrap();
        h.tx.send(Event::Inbound(InMsg {
            from: conn_id,
            by_user: None,
            bytes: real_update.clone(),
        }))
        .await
        .unwrap();

        // The writer batches for 250 ms, then inserts and publishes the seq on
        // the bus. Wait for that publish rather than sleeping a fixed amount:
        // on a loaded CI runner the insert can land well after 500 ms.
        let seq =
            tokio::time::timeout(std::time::Duration::from_secs(10), persisted.updates.recv())
                .await
                .expect("timed out waiting for the writer to persist the update")
                .expect("bus closed before the writer published");
        assert!(seq > 0, "writer published a non-positive seq: {seq}");
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

        let pool = knot_test_support::fresh_db().await.pool;

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
            .insert_batch(d.id, &[(Some(u.id), seed_bytes)])
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
            None,
        )
        .await
        .unwrap();

        let (tx, _rx) = mpsc::channel(8);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id: Uuid::new_v4(),
            handle: ConnHandle::new(tx),
            reply: reply_tx,
        })
        .await
        .unwrap();
        let state = reply_rx.await.unwrap().unwrap().update;
        assert!(
            !state.is_empty(),
            "hydrated state should include the seed update"
        );
    }
    /// A replace carries its author through to the persisted update.
    /// Before this, `ReplaceWithMarkdown` hardcoded `by_user_id: None`, so
    /// a full-document rewrite was the one mutation nobody could be blamed
    /// for.
    #[tokio::test(flavor = "multi_thread")]
    async fn replace_with_markdown_records_by_user() {
        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates: Arc<dyn knot_storage::UpdatesStore> =
            Arc::new(RecordingUpdates { seen: seen.clone() });
        let snapshots: Arc<dyn knot_storage::SnapshotStore> = Arc::new(NoopSnapshots);
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let policy = crate::snapshot::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(60),
        };
        let h = Room::spawn(
            doc_id,
            engine.clone(),
            bus,
            sub,
            updates,
            snapshots,
            policy,
            None,
        )
        .await
        .unwrap();

        let alice = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        h.tx.send(Event::ReplaceWithMarkdown {
            update_bytes: make_replace_bytes_raw("Imported"),
            by_user: Some(alice),
            source: knot_storage::UpdateSource::Import,
            meta: None,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap().expect("replace");

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![Some(alice)],
            "replace should persist with its author, got {recorded:?}"
        );
    }

    /// Once the server asks every joining writer for its state (SyncStep1),
    /// most replies carry nothing new — Yjs always includes the whole delete
    /// set. Such a no-op must not be persisted (a `doc_updates` row per page
    /// open, and the opener would count as a contributor), fanned out to
    /// peers, or counted as an update.
    #[tokio::test(flavor = "multi_thread")]
    async fn noop_inbound_update_is_neither_fanned_out_nor_persisted() {
        use yrs::{ReadTxn, Text, Transact};

        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let persisted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates: Arc<dyn knot_storage::UpdatesStore> = Arc::new(CapturingUpdates {
            persisted: persisted.clone(),
        });
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
            None,
        )
        .await
        .unwrap();

        // A client's history, including a deletion so the no-op carries a
        // non-empty delete set like a real SyncStep2 reply does.
        let client = yrs::Doc::new();
        let text = client.get_or_insert_text("t");
        text.push(&mut client.transact_mut(), "hellox");
        text.remove_range(&mut client.transact_mut(), 5, 1);
        let real = client
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        // The edit's own update, as a live client sends it (a state diff
        // would re-send the whole delete set, which the room does not
        // persist again).
        let sentinel = {
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sub = {
                let captured = captured.clone();
                client
                    .observe_update_v1(move |_, e| *captured.lock().unwrap() = e.update.clone())
                    .unwrap()
            };
            text.push(&mut client.transact_mut(), "!");
            drop(sub);
            captured.lock().unwrap().clone()
        };

        let sender = Uuid::new_v4();
        let (sender_tx, _sender_rx) = mpsc::channel(8);
        let (peer_tx, mut peer_rx) = mpsc::channel(8);
        for (conn_id, tx) in [(sender, sender_tx), (Uuid::new_v4(), peer_tx)] {
            let (reply_tx, reply_rx) = oneshot::channel();
            h.tx.send(Event::Join {
                conn_id,
                handle: ConnHandle::new(tx),
                reply: reply_tx,
            })
            .await
            .unwrap();
            reply_rx.await.unwrap().unwrap();
        }

        // Real edit, then the identical bytes again (the no-op), then a
        // second real edit. The actor handles them strictly in order.
        for bytes in [real.clone(), real.clone(), sentinel.clone()] {
            h.tx.send(Event::Inbound(InMsg {
                from: sender,
                by_user: None,
                bytes,
            }))
            .await
            .unwrap();
        }

        let mut fanned = Vec::new();
        for _ in 0..2 {
            fanned.push(
                tokio::time::timeout(std::time::Duration::from_secs(10), peer_rx.recv())
                    .await
                    .expect("peer got no fan-out")
                    .expect("peer channel closed"),
            );
        }
        // The room fans out and persists what each apply changed, which yrs
        // re-encodes (`real` carries a deletion): compared as updates.
        let (first, second) = (&fanned[0], &fanned[1]);
        assert!(same_update(&unwrap_frame(first), &real));
        assert!(
            same_update(&unwrap_frame(second), &sentinel),
            "the peer was sent the no-op instead of the next real edit"
        );

        // The writer persists in arrival order: once the sentinel is in, a
        // persisted no-op would be too.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !persisted
            .lock()
            .unwrap()
            .iter()
            .any(|p| same_update(p, &sentinel))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "writer never persisted the sentinel"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let persisted = persisted.lock().unwrap().clone();
        assert_eq!(persisted.len(), 2, "the no-op update was persisted");
        assert!(same_update(&persisted[0], &real));
        assert!(same_update(&persisted[1], &sentinel));

        h.shutdown.cancel();
    }

    /// The flip side: content the room cannot integrate yet (a dependency is
    /// missing) is parked as pending by yrs. It must still be persisted and
    /// fanned out — dropping it would lose the edit once the gap is filled.
    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_update_with_missing_dependency_is_still_persisted() {
        use yrs::{ReadTxn, Text, Transact};

        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let persisted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates: Arc<dyn knot_storage::UpdatesStore> = Arc::new(CapturingUpdates {
            persisted: persisted.clone(),
        });
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
            None,
        )
        .await
        .unwrap();

        let client = yrs::Doc::new();
        let text = client.get_or_insert_text("t");
        text.push(&mut client.transact_mut(), "a");
        let sv = client.transact().state_vector();
        text.push(&mut client.transact_mut(), "b");
        let dependent = client.transact().encode_state_as_update_v1(&sv);

        let (peer_tx, mut peer_rx) = mpsc::channel(8);
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id: Uuid::new_v4(),
            handle: ConnHandle::new(peer_tx),
            reply: reply_tx,
        })
        .await
        .unwrap();
        reply_rx.await.unwrap().unwrap();
        let (sender, _sender_rx) = join(&h).await;

        h.tx.send(Event::Inbound(InMsg {
            from: sender,
            by_user: None,
            bytes: dependent.clone(),
        }))
        .await
        .unwrap();

        let fanned = tokio::time::timeout(std::time::Duration::from_secs(10), peer_rx.recv())
            .await
            .expect("pending update was not fanned out")
            .unwrap();
        assert!(same_update(&unwrap_frame(&fanned), &dependent));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while persisted.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "pending update was never persisted"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Persisted raw (the apply parked it: `raw_fallback`), but compared
        // as an update like every persisted inbound update.
        let persisted = persisted.lock().unwrap().clone();
        assert_eq!(persisted.len(), 1);
        assert!(same_update(&persisted[0], &dependent));

        h.shutdown.cancel();
    }

    /// Live edits carry the authenticated user who typed them, row by row.
    /// The writer batches whatever arrives within 250 ms, so two people
    /// typing at once share a flush; each row must still be stored under
    /// its own author — not the batch's first — or one of them would be
    /// credited with the other's work (and the other with nothing).
    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_updates_persist_under_each_senders_own_author() {
        use yrs::{ReadTxn, Text, Transact};

        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates: Arc<dyn knot_storage::UpdatesStore> =
            Arc::new(RecordingUpdates { seen: seen.clone() });
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
            None,
        )
        .await
        .unwrap();

        // Two independent clients, each with one real edit.
        let edit = |s: &str| {
            let d = yrs::Doc::new();
            d.get_or_insert_text("t").push(&mut d.transact_mut(), s);
            d.transact()
                .encode_state_as_update_v1(&yrs::StateVector::default())
        };
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
        let (alice_conn, _alice_rx) = join(&h).await;
        let (bob_conn, _bob_rx) = join(&h).await;
        // Sent back-to-back: both reach the writer well inside one batch
        // window, which is exactly the case a per-batch author gets wrong.
        for (conn, by, bytes) in [(alice_conn, alice, edit("a")), (bob_conn, bob, edit("b"))] {
            h.tx.send(Event::Inbound(InMsg {
                from: conn,
                by_user: Some(by),
                bytes,
            }))
            .await
            .unwrap();
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while seen.lock().unwrap().len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "writer never persisted both edits"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some(alice), Some(bob)],
            "each live edit must be persisted under its sender"
        );

        h.shutdown.cancel();
    }

    /// An ACL change must take effect on sockets that are already open. The
    /// room tells each connection to close (4403) through its `revoked`
    /// token — the channel cannot carry that by closing, because the WS shim
    /// keeps its own sender for the whole session — and ignores anything a
    /// revoked connection still sends before its socket is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn revoke_signals_each_conn_and_ignores_its_updates() {
        use yrs::{ReadTxn, Text, Transact};

        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let persisted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates: Arc<dyn knot_storage::UpdatesStore> = Arc::new(CapturingUpdates {
            persisted: persisted.clone(),
        });
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
            None,
        )
        .await
        .unwrap();

        let conn_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::channel(8);
        let handle = ConnHandle::new(tx);
        let revoked = handle.revoked.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id,
            handle,
            reply: reply_tx,
        })
        .await
        .unwrap();
        reply_rx.await.unwrap().unwrap();

        h.tx.send(Event::Revoke).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), revoked.cancelled())
            .await
            .expect("the revoked connection was never told to close");

        let edit = |text: &str| {
            let d = yrs::Doc::new();
            d.get_or_insert_text("t").push(&mut d.transact_mut(), text);
            d.transact()
                .encode_state_as_update_v1(&yrs::StateVector::default())
        };
        h.tx.send(Event::Inbound(InMsg {
            from: conn_id,
            by_user: None,
            bytes: edit("after the revoke"),
        }))
        .await
        .unwrap();
        // Events are handled in order: once a later HTTP edit is durably
        // persisted, the revoked connection's update would have been too.
        let sentinel = edit("sentinel");
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::ApplyUpdate {
            update_bytes: sentinel.clone(),
            by_user: None,
            source: knot_storage::UpdateSource::Edit,
            meta: None,
            reply: reply_tx,
        })
        .await
        .unwrap();
        reply_rx.await.unwrap().unwrap();
        // The sentinel is persisted as what its apply changed, which yrs
        // re-encodes: compared as an update.
        let persisted = persisted.lock().unwrap().clone();
        assert!(
            persisted.len() == 1 && same_update(&persisted[0], &sentinel),
            "an update from a revoked connection was persisted"
        );

        h.shutdown.cancel();
    }

    /// A client SyncStep1 that races the revoke would re-join the room with
    /// the connection's (already cancelled) handle. The room must refuse it:
    /// no state for someone whose access just ended, and no re-registration
    /// that would let their updates through again.
    #[tokio::test(flavor = "multi_thread")]
    async fn join_with_a_revoked_handle_is_refused() {
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
            None,
        )
        .await
        .unwrap();

        let (tx, _rx) = mpsc::channel(8);
        let handle = ConnHandle::new(tx);
        handle.revoked.cancel();
        let (reply_tx, reply_rx) = oneshot::channel();
        h.tx.send(Event::Join {
            conn_id: Uuid::new_v4(),
            handle,
            reply: reply_tx,
        })
        .await
        .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), reply_rx)
            .await
            .expect("the room never answered the join");
        assert!(
            reply.is_err(),
            "a revoked connection was sent the document state"
        );

        h.shutdown.cancel();
    }

    /// However the actor ends, its token is cancelled: the WS shims watch it
    /// to close their sockets, and the registry to boot a fresh room. Here
    /// the actor ends on `Event::Shutdown`, without anyone cancelling it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_room_whose_actor_ends_cancels_its_token() {
        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let h = Room::spawn(
            doc_id,
            Arc::new(YrsEngine),
            bus,
            sub,
            Arc::new(NoopUpdates),
            Arc::new(NoopSnapshots),
            crate::snapshot::SnapshotPolicy {
                every_n: 1000,
                idle: std::time::Duration::from_secs(60),
            },
            None,
        )
        .await
        .unwrap();
        h.tx.send(Event::Shutdown).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
            .await
            .expect("the room's token was not cancelled when its actor ended");
    }

    /// The client id the suggestion tests build their seed content with, so
    /// they can anchor on known item ids.
    const SEED_CLIENT: u64 = 7;

    /// The text the suggestion tests seed the page with. As a fresh doc of
    /// [`SEED_CLIENT`], the paragraph is clock 0, its XmlText clock 1 and its
    /// characters start at clock 2, so "world" is clocks 8..=12.
    const SEED_TEXT: &str = "hello world";

    /// Update bytes of a fresh doc of [`SEED_CLIENT`] holding
    /// `paragraph > text`.
    fn seed_paragraph(text: &str) -> Vec<u8> {
        use yrs::{Doc, ReadTxn, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};
        let doc = Doc::with_client_id(SEED_CLIENT);
        let frag = doc.get_or_insert_xml_fragment("default");
        {
            let mut txn = doc.transact_mut();
            let para = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            para.push_back(&mut txn, XmlTextPrelim::new(text));
        }
        let txn = doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    /// A range anchor (raw RelativePosition v1 bytes) tied to the seed
    /// character at `clock`: before it (assoc ≥ 0) for a start, after it
    /// (assoc < 0) for an end.
    fn seed_anchor(clock: u32, assoc: yrs::Assoc) -> Vec<u8> {
        use yrs::updates::encoder::Encode;
        yrs::StickyIndex::from_id(
            yrs::ID::new(yrs::block::ClientID::new(SEED_CLIENT), clock),
            assoc,
        )
        .encode_v1()
    }

    /// A suggestion replacing "world" of [`SEED_TEXT`] with "there", as
    /// accepted by an owner who saw `seen`.
    fn replace_world(seen: &str) -> crate::ApplySuggestionRequest {
        crate::ApplySuggestionRequest {
            suggestion_id: Uuid::new_v4(),
            apply_token: Uuid::new_v4(),
            kind: crate::SuggestionKind::Replace,
            start: seed_anchor(8, yrs::Assoc::After),
            end: seed_anchor(12, yrs::Assoc::Before),
            seen_text: seen.into(),
            proposed_text: "there".into(),
        }
    }

    /// Spawns a room over `updates` with no snapshots and a lazy policy.
    async fn spawn_suggestion_room(
        doc_id: Uuid,
        updates: Arc<dyn knot_storage::UpdatesStore>,
    ) -> RoomHandle {
        spawn_suggestion_room_with(doc_id, updates, Arc::new(NoopSnapshots)).await
    }

    /// [`spawn_suggestion_room`] over the given snapshot store.
    async fn spawn_suggestion_room_with(
        doc_id: Uuid,
        updates: Arc<dyn knot_storage::UpdatesStore>,
        snapshots: Arc<dyn knot_storage::SnapshotStore>,
    ) -> RoomHandle {
        let bus = Arc::new(MemBus::new());
        let sub = bus.subscribe(doc_id).await.unwrap();
        Room::spawn(
            doc_id,
            Arc::new(YrsEngine),
            bus,
            sub,
            updates,
            snapshots,
            crate::snapshot::SnapshotPolicy {
                every_n: 1000,
                idle: std::time::Duration::from_secs(60),
            },
            None,
        )
        .await
        .unwrap()
    }

    async fn apply_update(h: &RoomHandle, update_bytes: Vec<u8>, by_user: Option<Uuid>) {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::ApplyUpdate {
            update_bytes,
            by_user,
            source: knot_storage::UpdateSource::Edit,
            meta: None,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap().expect("apply update");
    }

    async fn apply_suggestion(
        h: &RoomHandle,
        request: crate::ApplySuggestionRequest,
        by_user: Option<Uuid>,
    ) -> Result<i64, crate::ApplySuggestionError> {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::ApplySuggestion {
            request,
            by_user,
            accepted_by: Uuid::new_v4(),
            reply: tx,
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("the room never answered the apply")
            .unwrap()
    }

    async fn suggestion_applied(h: &RoomHandle, suggestion_id: Uuid, apply_token: Uuid) -> bool {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::SuggestionApplied {
            suggestion_id,
            apply_token,
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap()
    }

    /// The room's `default` fragment as XML.
    pub(crate) async fn exported_page(h: &RoomHandle) -> String {
        use yrs::{GetString, Transact, Update, updates::decoder::Decode};
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::ExportState(tx)).await.unwrap();
        let (bytes, _) = rx.await.unwrap().unwrap();
        let doc = yrs::Doc::new();
        let frag = doc.get_or_insert_xml_fragment("default");
        doc.transact_mut()
            .apply_update(Update::decode_v1(&bytes).unwrap())
            .unwrap();
        frag.get_string(&doc.transact())
    }

    /// An accepted suggestion lands as exactly one persisted update,
    /// credited to the suggester, and the page shows the new text.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_suggestion_persists_one_update_by_suggester() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let h = spawn_suggestion_room(
            Uuid::new_v4(),
            Arc::new(RecordingUpdates { seen: seen.clone() }),
        )
        .await;
        let seeder = Uuid::new_v4();
        apply_update(&h, seed_paragraph(SEED_TEXT), Some(seeder)).await;

        let suggester = Uuid::new_v4();
        let seq = apply_suggestion(&h, replace_world("world"), Some(suggester))
            .await
            .expect("apply suggestion");
        assert!(seq > 0, "apply returned a non-positive seq: {seq}");

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![Some(seeder), Some(suggester)],
            "the apply should persist one row by the suggester, got {recorded:?}"
        );
        assert_eq!(
            exported_page(&h).await,
            "<paragraph>hello there</paragraph>"
        );
        h.shutdown.cancel();
    }

    /// A refused apply (the text differs from what the owner saw) writes
    /// nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_suggestion_refusal_persists_nothing() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let h = spawn_suggestion_room(
            Uuid::new_v4(),
            Arc::new(RecordingUpdates { seen: seen.clone() }),
        )
        .await;
        apply_update(&h, seed_paragraph(SEED_TEXT), Some(Uuid::new_v4())).await;

        let err = apply_suggestion(&h, replace_world("wrold"), Some(Uuid::new_v4()))
            .await
            .unwrap_err();
        assert_eq!(err, crate::ApplySuggestionError::Changed);

        // A round trip through the actor, so a stray persist would have been
        // queued, then past the writer's batch interval.
        let page = exported_page(&h).await;
        tokio::time::sleep(crate::writer::BATCH_INTERVAL * 2).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "the refusal persisted a row");
        assert_eq!(page, "<paragraph>hello world</paragraph>");
        assert!(!h.shutdown.is_cancelled(), "a refusal evicted the room");
        h.shutdown.cancel();
    }

    /// `SuggestionApplied` reads the marker the apply records in the doc,
    /// and only under the claim's token.
    #[tokio::test(flavor = "multi_thread")]
    async fn suggestion_applied_reports_true_after_apply_false_before() {
        let h = spawn_suggestion_room(Uuid::new_v4(), Arc::new(NoopUpdates)).await;
        apply_update(&h, seed_paragraph(SEED_TEXT), None).await;

        let request = replace_world("world");
        let (id, token) = (request.suggestion_id, request.apply_token);
        assert!(!suggestion_applied(&h, id, token).await);
        apply_suggestion(&h, request.clone(), None)
            .await
            .expect("apply suggestion");
        assert!(suggestion_applied(&h, id, token).await);
        assert!(!suggestion_applied(&h, Uuid::new_v4(), token).await);
        assert!(
            !suggestion_applied(&h, id, Uuid::new_v4()).await,
            "a marker under another token counts as applied"
        );
        assert_eq!(
            apply_suggestion(&h, request, None).await.unwrap_err(),
            crate::ApplySuggestionError::AlreadyApplied
        );
        h.shutdown.cancel();
    }

    /// A replica whose room lags behind the edit that made the anchored text
    /// catches up before resolving, rather than answering `AnchorLost`.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_replays_bus_before_resolving() {
        use knot_storage::{
            DocStore, PgDocStore, PgUpdatesStore, PgUserStore, PgWorkspaceStore, UpdatesStore,
            UserStore, WorkspaceStore,
        };

        let pool = knot_test_support::fresh_db().await.pool;
        let ws = PgWorkspaceStore::new(pool.clone())
            .create("d", "W")
            .await
            .unwrap();
        let u = PgUserStore::new(pool.clone())
            .create_local("a@x.test", "A", "$h$")
            .await
            .unwrap();
        PgWorkspaceStore::new(pool.clone())
            .add_member(ws.id, u.id, knot_storage::WorkspaceRole::Owner)
            .await
            .unwrap();
        let d = PgDocStore::new(pool.clone())
            .create(ws.id, None, "D", "m", u.id)
            .await
            .unwrap();

        let updates = Arc::new(PgUpdatesStore::new(pool.clone()));
        let h = spawn_suggestion_room(d.id, updates.clone()).await;
        // One round trip, so the catch-up tick's immediate first run is past
        // and the next one is seconds away.
        exported_page(&h).await;

        // Another replica's edit: in the store, but not on this room's bus.
        updates
            .insert_batch(d.id, &[(Some(u.id), seed_paragraph(SEED_TEXT))])
            .await
            .unwrap();

        apply_suggestion(&h, replace_world("world"), Some(u.id))
            .await
            .expect("a lagging room should catch up before resolving");
        assert_eq!(
            exported_page(&h).await,
            "<paragraph>hello there</paragraph>"
        );
        h.shutdown.cancel();
    }

    /// An updates store whose writes always fail, permanently (so a writer
    /// gives up at once instead of retrying), and whose log holds the seed
    /// paragraph ([`SEED_TEXT`]) as its one row, seq 1 and rev 1.
    struct FailingUpdates;
    impl FailingUpdates {
        fn seed_row() -> knot_storage::DocUpdate {
            knot_storage::DocUpdate {
                seq: 1,
                doc_id: Uuid::nil(),
                update_bytes: seed_paragraph(SEED_TEXT),
                by_user_id: None,
                created_at: sqlx::types::chrono::Utc::now(),
                rev: Some(1),
                source: Some(knot_storage::UpdateSource::Edit),
                meta: None,
                epoch: Some(0),
            }
        }
    }
    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for FailingUpdates {
        async fn append(
            &self,
            _: Uuid,
            epoch: i32,
            _: &[knot_storage::NewUpdate],
        ) -> Result<Vec<knot_storage::Persisted>, knot_storage::UpdatesStoreError> {
            Err(knot_storage::UpdatesStoreError::EpochChanged {
                room: epoch,
                stored: epoch + 1,
            })
        }
        async fn rows_after(
            &self,
            _: Uuid,
            from: knot_storage::RowsAfter,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            let after = match from {
                knot_storage::RowsAfter::All => 0,
                knot_storage::RowsAfter::Rev(r) => r,
            };
            Ok(std::iter::once(Self::seed_row())
                .filter(|u| u.rev.is_some_and(|rev| rev > after))
                .collect())
        }
        async fn head(
            &self,
            _: Uuid,
        ) -> Result<knot_storage::HistoryHead, knot_storage::UpdatesStoreError> {
            Ok(knot_storage::HistoryHead {
                head_rev: 1,
                ..Default::default()
            })
        }
        async fn ensure_legacy_numbered(
            &self,
            _: Uuid,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
        }
        async fn since(
            &self,
            _: Uuid,
            after_seq: i64,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            Ok(std::iter::once(Self::seed_row())
                .filter(|u| u.seq > after_seq)
                .collect())
        }
        async fn max_seq(&self, _: Uuid) -> Result<i64, knot_storage::UpdatesStoreError> {
            Ok(1)
        }
    }

    /// A snapshot store that counts the snapshots written to it and stores
    /// none.
    #[derive(Default)]
    struct CountingSnapshots {
        inserts: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl knot_storage::SnapshotStore for CountingSnapshots {
        async fn insert_stamped(
            &self,
            _: Uuid,
            _: knot_storage::SnapshotStamp,
            _: &[u8],
            _: &[u8],
        ) -> Result<(), knot_storage::SnapshotStoreError> {
            self.inserts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        async fn list(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Vec<knot_storage::SnapshotMeta>, knot_storage::SnapshotStoreError> {
            Ok(vec![])
        }
        async fn by_seq(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Option<knot_storage::DocSnapshot>, knot_storage::SnapshotStoreError> {
            Ok(None)
        }
    }

    /// An apply the store didn't take evicts the room: no live room may keep
    /// an applied marker that storage lacks, and the evicted room writes no
    /// final snapshot, which would be the one thing that persisted it. No
    /// connection got the apply either, so none can push it back on resync.
    #[tokio::test(flavor = "multi_thread")]
    async fn persist_failure_evicts_room() {
        let snapshots = Arc::new(CountingSnapshots::default());
        // The room boots the seed paragraph from the store's log.
        let h =
            spawn_suggestion_room_with(Uuid::new_v4(), Arc::new(FailingUpdates), snapshots.clone())
                .await;
        let (_conn, mut conn_rx) = join(&h).await;
        while conn_rx.try_recv().is_ok() {}

        let err = apply_suggestion(&h, replace_world("world"), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::ApplySuggestionError::Persist(_)),
            "expected a persist error, got {err:?}"
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
            .await
            .expect("a persist failure did not evict the room");
        // The actor drops its receiver only after the final flush, so once
        // the channel is closed any snapshot it meant to write is written.
        tokio::time::timeout(std::time::Duration::from_secs(10), h.tx.closed())
            .await
            .expect("the evicted room's actor did not end");
        assert_eq!(
            snapshots.inserts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the evicted room snapshotted its unpersisted marker"
        );
        let mut fanned_out = Vec::new();
        while let Ok(frame) = conn_rx.try_recv() {
            fanned_out.push(frame);
        }
        assert!(
            fanned_out.is_empty(),
            "a connection got the unpersisted apply: {fanned_out:?}"
        );
    }

    /// A WS edit the store refuses for good evicts the room through its
    /// writer's Fatal, and the evicted room writes no final snapshot.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_writer_fatal_evicts_the_room() {
        let snapshots = Arc::new(CountingSnapshots::default());
        let h =
            spawn_suggestion_room_with(Uuid::new_v4(), Arc::new(FailingUpdates), snapshots.clone())
                .await;
        let (conn, _conn_rx) = join(&h).await;
        let edit = {
            use yrs::{ReadTxn, Text, Transact};
            let other = yrs::Doc::new();
            other
                .get_or_insert_text("t")
                .push(&mut other.transact_mut(), "hello");
            other
                .transact()
                .encode_state_as_update_v1(&yrs::StateVector::default())
        };
        h.tx.send(Event::Inbound(InMsg {
            from: conn,
            by_user: None,
            bytes: edit,
        }))
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
            .await
            .expect("the writer's Fatal did not evict the room");
        tokio::time::timeout(std::time::Duration::from_secs(10), h.tx.closed())
            .await
            .expect("the evicted room's actor did not end");
        assert_eq!(
            snapshots.inserts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the evicted room wrote a final snapshot"
        );
        let mut done = h.writer_done.clone();
        tokio::time::timeout(std::time::Duration::from_secs(10), done.wait())
            .await
            .expect("the writer did not end after its permanent error");
    }

    /// An updates store that takes every write and whose catch-up reads
    /// (`since`, `rows_after`, `head`) fail while `fail_reads` is set.
    #[derive(Default)]
    struct FlakyReads {
        next_seq: std::sync::atomic::AtomicI64,
        fail_reads: std::sync::atomic::AtomicBool,
    }
    impl FlakyReads {
        fn read(&self) -> Result<(), knot_storage::UpdatesStoreError> {
            if self.fail_reads.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(knot_storage::UpdatesStoreError::Sqlx(
                    sqlx::Error::PoolClosed,
                ));
            }
            Ok(())
        }
    }
    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for FlakyReads {
        async fn append(
            &self,
            _: Uuid,
            _: i32,
            rows: &[knot_storage::NewUpdate],
        ) -> Result<Vec<knot_storage::Persisted>, knot_storage::UpdatesStoreError> {
            Ok(rows
                .iter()
                .map(|_| {
                    let seq = self
                        .next_seq
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        + 1;
                    knot_storage::Persisted { seq, rev: seq }
                })
                .collect())
        }
        async fn rows_after(
            &self,
            _: Uuid,
            _: knot_storage::RowsAfter,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            self.read()?;
            Ok(vec![])
        }
        async fn head(
            &self,
            _: Uuid,
        ) -> Result<knot_storage::HistoryHead, knot_storage::UpdatesStoreError> {
            self.read()?;
            Ok(knot_storage::HistoryHead {
                head_rev: self.next_seq.load(std::sync::atomic::Ordering::SeqCst),
                ..Default::default()
            })
        }
        async fn ensure_legacy_numbered(
            &self,
            _: Uuid,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
        }
        async fn since(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            self.read()?;
            Ok(vec![])
        }
        async fn max_seq(&self, _: Uuid) -> Result<i64, knot_storage::UpdatesStoreError> {
            Ok(self.next_seq.load(std::sync::atomic::Ordering::SeqCst))
        }
    }

    /// When the room can't catch up it leaves `SuggestionApplied`
    /// unanswered rather than answer from a doc that may lag storage: a
    /// stale "no" would let the sweep release a row whose text is applied.
    #[tokio::test(flavor = "multi_thread")]
    async fn applied_check_unanswered_when_catch_up_fails() {
        let updates = Arc::new(FlakyReads::default());
        let h = spawn_suggestion_room(Uuid::new_v4(), updates.clone()).await;
        apply_update(&h, seed_paragraph(SEED_TEXT), None).await;
        let request = replace_world("world");
        let (id, token) = (request.suggestion_id, request.apply_token);
        apply_suggestion(&h, request, None).await.expect("apply");

        updates
            .fail_reads
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::SuggestionApplied {
            suggestion_id: id,
            apply_token: token,
            reply: tx,
        })
        .await
        .unwrap();
        let answer = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("the room neither answered nor dropped the check");
        assert!(answer.is_err(), "answered {answer:?} without catching up");
        assert!(!h.shutdown.is_cancelled(), "a failed read evicted the room");

        updates
            .fail_reads
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(suggestion_applied(&h, id, token).await);
        h.shutdown.cancel();
    }

    // -----------------------------------------------------------------------
    // The ordered log (page history M1.2): boot by rev, the watermark,
    // catch-up, the snapshot rule, effective fan-out.
    // -----------------------------------------------------------------------

    use crate::test_metrics::CountingRecorder;
    use knot_storage::{DocUpdate, NewUpdate, Persisted, RowsAfter, SnapshotStamp};

    use crate::test_fixtures::{
        para, parked_block_and_the_edit_it_fails_on, same_update, unwrap_frame,
    };

    /// A page log double: `append` stores rows at consecutive revs (or the
    /// revs queued in `own_revs`), and `rows_after` serves every stored row
    /// whose rev is not `hidden`, as Postgres does.
    #[derive(Default)]
    struct Log {
        state: std::sync::Mutex<LogState>,
    }

    #[derive(Default)]
    struct LogState {
        rows: Vec<DocUpdate>,
        hidden: std::collections::HashSet<i64>,
        next_seq: i64,
        own_revs: std::collections::VecDeque<i64>,
        /// `rows_after(Rev(r))` also returns rows at or below `r`.
        unfiltered: bool,
        /// Every row `append` stored, in order.
        appended: Vec<NewUpdate>,
        /// One per `append` call, in order: the call waits for it first.
        gates: std::collections::VecDeque<Arc<tokio::sync::Notify>>,
        /// This many successful `append` calls store their rows and then
        /// fail (a lost COMMIT acknowledgement).
        lost_acks: usize,
        /// `append` fails permanently, storing nothing.
        refuse_appends: bool,
        /// `append` fails transiently (the pool timed out: the database is
        /// down), storing nothing.
        down: bool,
    }

    impl Log {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn row(rev: i64, seq: i64, update_bytes: Vec<u8>) -> DocUpdate {
            DocUpdate {
                seq,
                doc_id: Uuid::nil(),
                update_bytes,
                by_user_id: None,
                created_at: sqlx::types::chrono::Utc::now(),
                rev: Some(rev),
                source: Some(knot_storage::UpdateSource::Edit),
                meta: None,
                epoch: Some(0),
            }
        }

        /// Stores a row of another writer at `rev` and `seq`.
        fn push(&self, rev: i64, seq: i64, bytes: Vec<u8>) {
            let mut s = self.state.lock().unwrap();
            s.next_seq = s.next_seq.max(seq);
            s.rows.push(Self::row(rev, seq, bytes));
        }

        fn with<R>(&self, f: impl FnOnce(&mut LogState) -> R) -> R {
            f(&mut self.state.lock().unwrap())
        }

        fn appended(&self) -> Vec<NewUpdate> {
            self.with(|s| s.appended.clone())
        }

        fn gate(&self) -> Arc<tokio::sync::Notify> {
            let gate = Arc::new(tokio::sync::Notify::new());
            self.with(|s| s.gates.push_back(gate.clone()));
            gate
        }
    }

    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for Log {
        async fn append(
            &self,
            _: Uuid,
            epoch: i32,
            rows: &[NewUpdate],
        ) -> Result<Vec<Persisted>, knot_storage::UpdatesStoreError> {
            if let Some(gate) = self.with(|s| s.gates.pop_front()) {
                gate.notified().await;
            }
            let mut s = self.state.lock().unwrap();
            if s.down {
                return Err(knot_storage::UpdatesStoreError::Sqlx(
                    sqlx::Error::PoolTimedOut,
                ));
            }
            if s.refuse_appends {
                return Err(knot_storage::UpdatesStoreError::EpochChanged {
                    room: epoch,
                    stored: epoch + 1,
                });
            }
            let mut out = Vec::new();
            for new in rows {
                let next = s.rows.iter().filter_map(|r| r.rev).max().unwrap_or(0) + 1;
                let rev = s.own_revs.pop_front().unwrap_or(next);
                s.next_seq += 1;
                let seq = s.next_seq;
                let mut row = Self::row(rev, seq, new.bytes.clone());
                row.by_user_id = new.by_user_id;
                row.epoch = Some(epoch);
                s.rows.push(row);
                s.appended.push(new.clone());
                out.push(Persisted { seq, rev });
            }
            if s.lost_acks > 0 {
                s.lost_acks -= 1;
                return Err(knot_storage::UpdatesStoreError::Sqlx(sqlx::Error::Io(
                    std::io::Error::new(std::io::ErrorKind::ConnectionReset, "lost ack"),
                )));
            }
            Ok(out)
        }
        async fn rows_after(
            &self,
            _: Uuid,
            from: RowsAfter,
        ) -> Result<Vec<DocUpdate>, knot_storage::UpdatesStoreError> {
            let s = self.state.lock().unwrap();
            let mut rows: Vec<DocUpdate> = s
                .rows
                .iter()
                .filter(|r| !s.hidden.contains(&r.rev.unwrap()))
                .filter(|r| match from {
                    RowsAfter::All => true,
                    RowsAfter::Rev(after) => s.unfiltered || r.rev.unwrap() > after,
                })
                .cloned()
                .collect();
            rows.sort_by_key(|r| r.rev);
            Ok(rows)
        }
        async fn head(
            &self,
            _: Uuid,
        ) -> Result<knot_storage::HistoryHead, knot_storage::UpdatesStoreError> {
            let s = self.state.lock().unwrap();
            Ok(knot_storage::HistoryHead {
                head_rev: s.rows.iter().filter_map(|r| r.rev).max().unwrap_or(0),
                ..Default::default()
            })
        }
        async fn ensure_legacy_numbered(
            &self,
            _: Uuid,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
        }
        async fn since(
            &self,
            _: Uuid,
            after: i64,
        ) -> Result<Vec<DocUpdate>, knot_storage::UpdatesStoreError> {
            let s = self.state.lock().unwrap();
            Ok(s.rows.iter().filter(|r| r.seq > after).cloned().collect())
        }
        async fn max_seq(&self, _: Uuid) -> Result<i64, knot_storage::UpdatesStoreError> {
            Ok(self.with(|s| s.next_seq))
        }
    }

    /// Records the stamp of every snapshot written, and fails each write
    /// with `fail` when set.
    #[derive(Default)]
    struct Stamps {
        written: std::sync::Mutex<Vec<SnapshotStamp>>,
        fail: std::sync::Mutex<Option<fn() -> knot_storage::SnapshotStoreError>>,
    }

    impl Stamps {
        fn written(&self) -> Vec<SnapshotStamp> {
            self.written.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl knot_storage::SnapshotStore for Stamps {
        async fn insert_stamped(
            &self,
            _: Uuid,
            stamp: SnapshotStamp,
            _: &[u8],
            _: &[u8],
        ) -> Result<(), knot_storage::SnapshotStoreError> {
            self.written.lock().unwrap().push(stamp);
            match *self.fail.lock().unwrap() {
                Some(fail) => Err(fail()),
                None => Ok(()),
            }
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
        async fn list(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Vec<knot_storage::SnapshotMeta>, knot_storage::SnapshotStoreError> {
            Ok(vec![])
        }
        async fn by_seq(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Option<knot_storage::DocSnapshot>, knot_storage::SnapshotStoreError> {
            Ok(None)
        }
    }

    /// Snapshots on every own row and on every idle tick.
    const EAGER: crate::snapshot::SnapshotPolicy = crate::snapshot::SnapshotPolicy {
        every_n: 1,
        idle: std::time::Duration::ZERO,
    };

    /// Snapshots only when idle (on every idle tick).
    const IDLE: crate::snapshot::SnapshotPolicy = crate::snapshot::SnapshotPolicy {
        every_n: 1000,
        idle: std::time::Duration::ZERO,
    };

    /// A room on `log` and `snapshots`. Its writer publishes on a bus of its
    /// own, so the room catches up only on its tick, a gap, or a message the
    /// test publishes on the returned bus.
    async fn room_on_log(
        log: &Arc<Log>,
        snapshots: &Arc<Stamps>,
        policy: crate::snapshot::SnapshotPolicy,
    ) -> (RoomHandle, MemBus, Uuid) {
        let doc_id = Uuid::new_v4();
        let listened = MemBus::new();
        let sub = listened.subscribe(doc_id).await.unwrap();
        let h = Room::spawn(
            doc_id,
            Arc::new(YrsEngine),
            Arc::new(MemBus::new()),
            sub,
            log.clone(),
            snapshots.clone(),
            policy,
            None,
        )
        .await
        .expect("the room boots");
        (h, listened, doc_id)
    }

    async fn probe(h: &RoomHandle) -> Probe {
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::Probe(tx)).await.unwrap();
        rx.await.unwrap()
    }

    pub(crate) async fn send_edit(h: &RoomHandle, from: ConnId, bytes: Vec<u8>) {
        h.tx.send(Event::Inbound(InMsg {
            from,
            by_user: None,
            bytes,
        }))
        .await
        .unwrap();
    }

    /// Waits (in virtual time when it is paused) until `done` holds.
    pub(crate) async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Waits until the room's probe satisfies `done`.
    async fn until_probe(h: &RoomHandle, what: &str, done: impl Fn(&Probe) -> bool) -> Probe {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let p = probe(h).await;
            if done(&p) {
                return p;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out: {what} ({p:?})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Every frame `rx` holds now.
    fn drain(rx: &mut mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        frames
    }

    async fn shut_down(h: &RoomHandle) {
        let _ = h.tx.send(Event::Shutdown).await;
        tokio::time::timeout(std::time::Duration::from_secs(60), h.tx.closed())
            .await
            .expect("the room did not end");
    }

    /// A row stays in flight until the writer confirms it: no snapshot
    /// meanwhile, then one stamped with the confirmed rev.
    #[tokio::test(start_paused = true)]
    async fn no_snapshot_while_rows_are_in_flight() {
        let log = Log::new();
        let (first, second) = (log.gate(), log.gate());
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, IDLE).await;
        let (sender, _rx) = join(&h).await;

        send_edit(&h, sender, para(11, "one")).await;
        // The writer's first attempt (holding only this edit) has started.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        send_edit(&h, sender, para(12, "two")).await;
        first.notify_one();
        // The first row is confirmed, the second is in flight.
        until_probe(&h, "first row confirmed", |p| p.applied_through_rev == 1).await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let p = probe(&h).await;
        assert_eq!((p.in_flight, p.max_own_rev), (1, 1), "{p:?}");
        assert!(
            snapshots.written().is_empty(),
            "a snapshot was written while a row was in flight: {:?}",
            snapshots.written()
        );

        second.notify_one();
        until("the snapshot after the last row", || {
            !snapshots.written().is_empty()
        })
        .await;
        assert_eq!(
            snapshots.written()[0],
            SnapshotStamp {
                snapshot_seq: 2,
                snapshot_rev: Some(2),
                epoch: Some(0),
            }
        );
        shut_down(&h).await;
    }

    /// An own rev past the watermark means other rows committed in between:
    /// the room reads them at once (not on its 5 s tick).
    #[tokio::test(start_paused = true)]
    async fn an_own_rev_past_a_gap_triggers_catch_up() {
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
        let (sender, _sender_rx) = join(&h).await;
        let (_peer, mut peer_rx) = join(&h).await;
        // Past the catch-up tick's first, immediate run.
        probe(&h).await;

        let peer_row = para(21, "peer");
        log.push(1, 1, peer_row.clone());
        log.with(|s| s.own_revs.push_back(2));
        let sent = tokio::time::Instant::now();
        send_edit(&h, sender, para(11, "own")).await;

        let p = until_probe(&h, "own rev 2 confirmed", |p| p.max_own_rev == 2).await;
        assert_eq!(p.applied_through_rev, 2, "{p:?}");
        assert!(
            sent.elapsed() < std::time::Duration::from_secs(4),
            "caught up on the tick, not at once"
        );
        let frames = drain(&mut peer_rx);
        assert!(
            frames.iter().any(|f| unwrap_frame(f) == peer_row),
            "the peer row was not fanned out"
        );
        until("a snapshot at rev 2", || {
            snapshots
                .written()
                .iter()
                .any(|s| s.snapshot_rev == Some(2))
        })
        .await;
        assert!(
            snapshots
                .written()
                .iter()
                .all(|s| s.snapshot_rev == Some(2)),
            "{:?}",
            snapshots.written()
        );
        shut_down(&h).await;
    }

    /// Rows are applied by rev: one whose seq is below a seq already
    /// applied is not skipped (0.9.x read `since(last_applied_seq)`).
    #[tokio::test(start_paused = true)]
    async fn catch_up_applies_a_row_whose_seq_is_below_the_watermark() {
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        let (h, bus, doc_id) = room_on_log(&log, &snapshots, IDLE).await;
        probe(&h).await;

        log.push(1, 20, para(21, "first"));
        bus.publish(doc_id, 1).await.unwrap();
        until_probe(&h, "rev 1 applied", |p| p.applied_through_rev == 1).await;
        log.push(2, 15, para(22, "second"));
        bus.publish(doc_id, 2).await.unwrap();
        let p = until_probe(&h, "rev 2 applied", |p| p.applied_through_rev == 2).await;
        assert_eq!((p.through_seq, p.last_applied_seq), (15, 20), "{p:?}");
        let page = exported_page(&h).await;
        assert!(page.contains("first") && page.contains("second"), "{page}");
        shut_down(&h).await;
    }

    /// Catch-up stops at a rev gap and says so; a row at or below the
    /// watermark that a store returns is skipped, not a gap.
    #[tokio::test(start_paused = true)]
    async fn catch_up_stops_at_a_rev_gap() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        let (h, bus, doc_id) = room_on_log(&log, &snapshots, IDLE).await;
        let (_peer, mut peer_rx) = join(&h).await;
        probe(&h).await;

        let one = para(21, "one");
        log.push(1, 1, one.clone());
        log.push(3, 3, para(23, "three"));
        bus.publish(doc_id, 3).await.unwrap();
        let p = until_probe(&h, "rev 1 applied", |p| p.applied_through_rev == 1).await;
        assert_eq!(p.through_seq, 1);
        assert!(rec.counter("knot_room_rev_gap_total", &[]) >= 1);
        assert!(!exported_page(&h).await.contains("three"));
        let frames = drain(&mut peer_rx);
        assert_eq!(frames.len(), 1, "only rev 1 was fanned out");
        assert_eq!(unwrap_frame(&frames[0]), one);

        // The store returns rev 1 again for Rev(1), and nothing past it.
        log.with(|s| {
            s.rows.retain(|r| r.rev != Some(3));
            s.unfiltered = true;
        });
        let gaps = rec.counter("knot_room_rev_gap_total", &[]);
        bus.publish(doc_id, 1).await.unwrap();
        // A round trip after the bus message.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let p = probe(&h).await;
        assert_eq!(p.applied_through_rev, 1, "{p:?}");
        assert_eq!(rec.counter("knot_room_rev_gap_total", &[]), gaps);
        assert!(drain(&mut peer_rx).is_empty(), "rev 1 was fanned out again");
        shut_down(&h).await;
    }

    /// A snapshot write that finds the page gone ends the room quietly,
    /// without an exit snapshot.
    #[tokio::test(start_paused = true)]
    async fn snapshot_doc_gone_evicts_without_exit_snapshot() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        *snapshots.fail.lock().unwrap() = Some(|| knot_storage::SnapshotStoreError::DocGone);
        let (h, _bus, _) = room_on_log(&log, &snapshots, IDLE).await;
        let (sender, _rx) = join(&h).await;
        send_edit(&h, sender, para(11, "one")).await;

        tokio::time::timeout(std::time::Duration::from_secs(60), h.shutdown.cancelled())
            .await
            .expect("the room was not ended");
        tokio::time::timeout(std::time::Duration::from_secs(60), h.tx.closed())
            .await
            .expect("the room's actor did not end");
        assert_eq!(snapshots.written().len(), 1, "an exit snapshot was written");
        assert_eq!(rec.counter("knot_room_evicted_total", &[]), 0);
    }

    /// A snapshot write that finds the page's epoch changed evicts the room,
    /// without an exit snapshot.
    #[tokio::test(start_paused = true)]
    async fn snapshot_epoch_changed_evicts_the_room() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        *snapshots.fail.lock().unwrap() = Some(|| knot_storage::SnapshotStoreError::EpochChanged);
        let (h, _bus, _) = room_on_log(&log, &snapshots, IDLE).await;
        let (sender, _rx) = join(&h).await;
        send_edit(&h, sender, para(11, "one")).await;

        tokio::time::timeout(std::time::Duration::from_secs(60), h.shutdown.cancelled())
            .await
            .expect("the room was not evicted");
        tokio::time::timeout(std::time::Duration::from_secs(60), h.tx.closed())
            .await
            .expect("the room's actor did not end");
        assert_eq!(snapshots.written().len(), 1, "an exit snapshot was written");
        assert_eq!(rec.counter("knot_room_evicted_total", &[]), 1);
    }

    /// A writer Fatal evicts the room before any snapshot.
    #[tokio::test(start_paused = true)]
    async fn writer_fatal_evicts_without_snapshot() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        log.with(|s| s.refuse_appends = true);
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
        let (sender, _rx) = join(&h).await;
        send_edit(&h, sender, para(11, "one")).await;

        tokio::time::timeout(std::time::Duration::from_secs(60), h.shutdown.cancelled())
            .await
            .expect("the room was not evicted");
        tokio::time::timeout(std::time::Duration::from_secs(60), h.tx.closed())
            .await
            .expect("the room's actor did not end");
        assert!(snapshots.written().is_empty(), "{:?}", snapshots.written());
        assert_eq!(rec.counter("knot_room_evicted_total", &[]), 1);
    }

    /// The writer-event arm comes before the inbound events in the room's
    /// select: a Fatal ends the room before it reads what is queued.
    #[tokio::test]
    async fn a_writer_fatal_is_seen_before_queued_inbound_events() {
        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let (h, _persist_rx, events, _done) = Room::spawn_detached(
            doc_id,
            Arc::new(YrsEngine),
            bus,
            sub,
            Log::new(),
            Arc::new(Stamps::default()),
            IDLE,
            None,
        )
        .await
        .unwrap();
        let (_conn, _rx) = join(&h).await;

        let mut replies = Vec::new();
        for _ in 0..20 {
            let (tx, rx) = oneshot::channel();
            h.tx.try_send(Event::ExportState(tx))
                .unwrap_or_else(|_| panic!("the room's queue is full"));
            replies.push(rx);
        }
        events.send(WriterEvent::Fatal("test".into())).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(10), h.tx.closed())
            .await
            .expect("the room did not end");
        for rx in replies {
            assert!(rx.await.is_err(), "a queued event was answered");
        }
    }

    /// An update with a missing dependency is persisted raw, and counted.
    #[tokio::test(start_paused = true)]
    async fn a_raw_fallback_persist_is_counted() {
        use yrs::{ReadTxn, Text, Transact};
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, IDLE).await;
        let (sender, _rx) = join(&h).await;

        let client = yrs::Doc::new();
        let text = client.get_or_insert_text("t");
        text.push(&mut client.transact_mut(), "a");
        let sv = client.transact().state_vector();
        text.push(&mut client.transact_mut(), "b");
        let dependent = client.transact().encode_state_as_update_v1(&sv);

        send_edit(&h, sender, dependent.clone()).await;
        until("the dependent update is stored", || {
            log.appended().len() == 1
        })
        .await;
        assert_eq!(log.appended()[0].bytes, dependent, "persisted raw");
        assert_eq!(rec.counter("knot_room_updates_persisted_raw_total", &[]), 1);

        send_edit(&h, sender, para(12, "ordinary")).await;
        until("the ordinary edit is stored", || log.appended().len() == 2).await;
        assert_eq!(rec.counter("knot_room_updates_persisted_raw_total", &[]), 1);
        shut_down(&h).await;
    }

    /// No snapshot while an own rev is past the watermark (the rows below it
    /// are not readable yet), neither every-N, idle nor at exit; once they
    /// are, the next catch-up reaches it and the next snapshot is stamped
    /// with it.
    #[tokio::test(start_paused = true)]
    async fn no_snapshot_while_an_own_rev_is_past_a_gap() {
        /// A log holding rev 1, rev 2 hidden, and own rows landing at rev 3.
        fn gapped_log() -> Arc<Log> {
            let log = Log::new();
            log.push(1, 1, para(21, "one"));
            log.push(2, 2, para(22, "two"));
            log.with(|s| {
                s.hidden.extend([2, 3]);
                s.own_revs.push_back(3);
            });
            log
        }

        let log = gapped_log();
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
        let (sender, _rx) = join(&h).await;
        send_edit(&h, sender, para(11, "own")).await;
        let p = until_probe(&h, "own rev 3 confirmed", |p| p.max_own_rev == 3).await;
        assert_eq!(p.applied_through_rev, 1, "{p:?}");
        tokio::time::sleep(std::time::Duration::from_secs(12)).await;
        assert!(snapshots.written().is_empty(), "{:?}", snapshots.written());
        // Nor is the room caught up: the log lacks rows below its own rev.
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::SuggestionApplied {
            suggestion_id: Uuid::new_v4(),
            apply_token: Uuid::new_v4(),
            reply: tx,
        })
        .await
        .unwrap();
        assert!(rx.await.is_err(), "answered below its own rev");

        // At exit, in a second room built the same way.
        {
            let log = gapped_log();
            let snapshots = Arc::new(Stamps::default());
            let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
            let (sender, _rx) = join(&h).await;
            send_edit(&h, sender, para(11, "own")).await;
            until_probe(&h, "own rev 3 confirmed", |p| p.max_own_rev == 3).await;
            shut_down(&h).await;
            assert!(snapshots.written().is_empty(), "{:?}", snapshots.written());
        }

        log.with(|s| s.hidden.clear());
        let p = until_probe(&h, "caught up to rev 3", |p| p.applied_through_rev == 3).await;
        assert_eq!(p.through_seq, 3, "{p:?}");
        until("a snapshot", || !snapshots.written().is_empty()).await;
        assert_eq!(snapshots.written()[0].snapshot_rev, Some(3));
        assert_eq!(snapshots.written()[0].snapshot_seq, 3);
        assert!(exported_page(&h).await.contains("two"));
        shut_down(&h).await;
    }

    /// `SuggestionApplied` is answered only when the room caught up: a rev
    /// gap leaves it unanswered, without evicting the room.
    #[tokio::test(start_paused = true)]
    async fn suggestion_applied_is_unanswered_at_a_rev_gap() {
        let log = Log::new();
        log.push(1, 1, para(21, "one"));
        log.push(2, 2, para(22, "two"));
        log.push(3, 3, para(23, "three"));
        log.with(|s| {
            s.hidden.insert(2);
        });
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, IDLE).await;

        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::SuggestionApplied {
            suggestion_id: Uuid::new_v4(),
            apply_token: Uuid::new_v4(),
            reply: tx,
        })
        .await
        .unwrap();
        assert!(rx.await.is_err(), "answered at a rev gap");
        assert!(!h.shutdown.is_cancelled(), "a gap evicted the room");

        log.with(|s| s.hidden.clear());
        assert!(!suggestion_applied(&h, Uuid::new_v4(), Uuid::new_v4()).await);
        assert_eq!(probe(&h).await.applied_through_rev, 3);
        shut_down(&h).await;
    }

    /// An update yrs rejects part-way leaves content no row carries in the
    /// doc: the room writes no snapshot from then on (Rulings). The cost,
    /// pinned: an edit made inside that content is persisted and fanned
    /// out, but a reboot from the log lacks it (it stays pending).
    #[tokio::test(start_paused = true)]
    async fn a_rejected_update_stops_snapshots() {
        use yrs::{GetString, ReadTxn, Text, Transact, XmlFragment, XmlOut};
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
        let (crafted, _crafted_rx) = join(&h).await;
        let (innocent, _innocent_rx) = join(&h).await;
        let (writer, _writer_rx) = join(&h).await;
        let (_peer, mut peer_rx) = join(&h).await;
        let (parked, edit, client) = parked_block_and_the_edit_it_fails_on();

        send_edit(&h, crafted, parked).await;
        until("a snapshot of the parked block", || {
            !snapshots.written().is_empty()
        })
        .await;
        let written = snapshots.written().len();
        send_edit(&h, innocent, edit).await;
        until_probe(&h, "the room is unlogged", |p| p.unlogged).await;

        // An edit inside the paragraph the rejected update integrated.
        let inside = {
            let frag = client.get_or_insert_xml_fragment("default");
            let sv = client.transact().state_vector();
            {
                let mut txn = client.transact_mut();
                let Some(XmlOut::Element(p)) = frag.get(&txn, 0) else {
                    panic!("no paragraph")
                };
                let Some(XmlOut::Text(text)) = p.get(&txn, 0) else {
                    panic!("no text")
                };
                text.insert(&mut txn, 0, "XYZ");
                assert_eq!(text.get_string(&txn), "XYZhello");
            }
            client.transact().encode_state_as_update_v1(&sv)
        };
        drain(&mut peer_rx);
        send_edit(&h, writer, inside.clone()).await;
        until("the inside edit is stored", || {
            log.appended()
                .iter()
                .any(|n| same_update(&n.bytes, &inside))
        })
        .await;
        let frames = drain(&mut peer_rx);
        assert!(
            frames
                .iter()
                .any(|f| same_update(&unwrap_frame(f), &inside)),
            "the inside edit was not fanned out"
        );
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        shut_down(&h).await;
        assert_eq!(
            snapshots.written().len(),
            written,
            "a snapshot was written after the rejected update"
        );

        let (h, _bus, _) = room_on_log(&log, &Arc::new(Stamps::default()), IDLE).await;
        let page = exported_page(&h).await;
        assert!(!page.contains("XYZ"), "{page}");
        shut_down(&h).await;
    }

    /// An engine that applies as yrs does but reports nothing to persist
    /// for one update (`silent`), although the apply changed the doc: an
    /// `ApplyMeasured` that breaks its own contract.
    struct NothingToPersist {
        silent: Vec<u8>,
    }

    impl Engine for NothingToPersist {
        fn new_doc(&self) -> DocHandle {
            YrsEngine.new_doc()
        }
        fn apply_update(&self, d: &DocHandle, update: &[u8]) -> Result<(), EngineError> {
            YrsEngine.apply_update(d, update)
        }
        fn apply_update_changed(&self, d: &DocHandle, update: &[u8]) -> Result<bool, EngineError> {
            YrsEngine.apply_update_changed(d, update)
        }
        fn apply_update_measuring(
            &self,
            d: &DocHandle,
            update: &[u8],
            limit: usize,
        ) -> Result<ApplyMeasured, EngineError> {
            let mut applied = YrsEngine.apply_update_measuring(d, update, limit)?;
            if update == self.silent {
                applied.persist.clear();
                applied.raw_fallback = false;
            }
            Ok(applied)
        }
        fn encode_state_as_update(
            &self,
            d: &DocHandle,
            peer_sv: Option<&[u8]>,
        ) -> Result<Vec<u8>, EngineError> {
            YrsEngine.encode_state_as_update(d, peer_sv)
        }
        fn encode_state_vector(&self, d: &DocHandle) -> Result<Vec<u8>, EngineError> {
            YrsEngine.encode_state_vector(d)
        }
    }

    /// An apply that changed the doc but has nothing to persist leaves
    /// content no row carries, as a rejected one does: the room writes no
    /// snapshot from then on, which would certify that content, also once a
    /// later edit is stored (review fix round 1).
    #[tokio::test(start_paused = true)]
    async fn a_change_with_nothing_to_persist_stops_snapshots() {
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        let silent = para(11, "unlogged");
        let doc_id = Uuid::new_v4();
        let bus = MemBus::new();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let h = Room::spawn(
            doc_id,
            Arc::new(NothingToPersist {
                silent: silent.clone(),
            }),
            Arc::new(MemBus::new()),
            sub,
            log.clone(),
            snapshots.clone(),
            EAGER,
            None,
        )
        .await
        .expect("the room boots");
        let (writer, _writer_rx) = join(&h).await;
        let (_peer, mut peer_rx) = join(&h).await;
        drain(&mut peer_rx);

        send_edit(&h, writer, silent).await;
        until_probe(&h, "the room is unlogged", |p| p.unlogged).await;
        assert!(log.appended().is_empty(), "nothing was to be stored");
        assert!(
            drain(&mut peer_rx).is_empty(),
            "nothing was to be fanned out"
        );

        let later = para(12, "logged");
        send_edit(&h, writer, later.clone()).await;
        until("the later edit is stored", || {
            log.appended().iter().any(|n| same_update(&n.bytes, &later))
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        shut_down(&h).await;
        assert!(
            snapshots.written().is_empty(),
            "a snapshot certified content no row carries: {:?}",
            snapshots.written()
        );
    }

    /// The four rows `catch_up_passes_...` and `boot_passes_...` read: a
    /// paragraph, the crafted parked block, the innocent edit it fails on,
    /// another paragraph.
    fn rows_with_one_that_fails() -> [Vec<u8>; 4] {
        let (parked, edit, _) = parked_block_and_the_edit_it_fails_on();
        [para(11, "one"), parked, edit, para(12, "four")]
    }

    /// A row that fails to apply in catch-up is passed: later rows still
    /// apply, the watermark moves past it, the room stays up, catches up,
    /// and writes no snapshot from then on.
    #[tokio::test(start_paused = true)]
    async fn catch_up_passes_a_row_that_fails_to_apply_and_marks_the_room_unlogged() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        let snapshots = Arc::new(Stamps::default());
        let (h, bus, doc_id) = room_on_log(&log, &snapshots, EAGER).await;
        let (sender, _sender_rx) = join(&h).await;
        let (_peer, mut peer_rx) = join(&h).await;
        probe(&h).await;

        let rows = rows_with_one_that_fails();
        for (i, row) in rows.iter().enumerate() {
            log.push(i as i64 + 1, i as i64 + 1, row.clone());
        }
        let published = tokio::time::Instant::now();
        bus.publish(doc_id, 4).await.unwrap();
        let p = until_probe(&h, "caught up to rev 4", |p| p.applied_through_rev == 4).await;
        assert!(p.unlogged, "{p:?}");
        // In the catch-up the bus message started, not a later one: the row
        // that fails is passed, not retried.
        assert!(
            published.elapsed() < std::time::Duration::from_secs(4),
            "caught up past the failing row only on a later tick"
        );
        let fanned: Vec<Vec<u8>> = drain(&mut peer_rx)
            .iter()
            .map(|f| unwrap_frame(f))
            .collect();
        assert_eq!(
            fanned,
            vec![rows[0].clone(), rows[1].clone(), rows[3].clone()],
            "rows 1, 2 and 4 are fanned out, row 3 is not"
        );
        assert_eq!(
            rec.counter("knot_room_row_apply_errors_total", &[("phase", "catch_up")]),
            1
        );
        assert!(!suggestion_applied(&h, Uuid::new_v4(), Uuid::new_v4()).await);

        send_edit(&h, sender, para(13, "own")).await;
        until_probe(&h, "own row confirmed", |p| p.max_own_rev == 5).await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(!h.shutdown.is_cancelled(), "the room was evicted");
        shut_down(&h).await;
        assert!(snapshots.written().is_empty(), "{:?}", snapshots.written());
    }

    /// A row that fails to apply at boot is passed: the page opens with the
    /// rest, and the room writes no snapshot. A read-only load fails
    /// instead.
    #[tokio::test(start_paused = true)]
    async fn boot_passes_a_row_that_fails_to_apply() {
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        for (i, row) in rows_with_one_that_fails().into_iter().enumerate() {
            log.push(i as i64 + 1, i as i64 + 1, row);
        }
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, doc_id) = room_on_log(&log, &snapshots, EAGER).await;
        let p = probe(&h).await;
        assert_eq!(p.applied_through_rev, 4, "{p:?}");
        assert!(p.unlogged, "{p:?}");
        assert_eq!(
            rec.counter("knot_room_row_apply_errors_total", &[("phase", "boot")]),
            1
        );
        let page = exported_page(&h).await;
        for part in ["one", "<partial>", "four"] {
            assert!(page.contains(part), "{part} is missing: {page}");
        }

        let (sender, _rx) = join(&h).await;
        send_edit(&h, sender, para(13, "own")).await;
        until_probe(&h, "own row confirmed", |p| p.max_own_rev == 5).await;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        shut_down(&h).await;
        assert!(snapshots.written().is_empty(), "{:?}", snapshots.written());

        let read_only = crate::hydrate::boot(
            &YrsEngine,
            log.as_ref(),
            snapshots.as_ref(),
            doc_id,
            BootMode::ReadOnly,
        )
        .await;
        assert!(
            matches!(read_only, Err(BootError::Apply(_))),
            "a read-only load passed a row that failed to apply"
        );
    }

    /// A persist channel the writer closed (after a Fatal the room has not
    /// read yet) evicts the room: the edit is not fanned out, and nothing
    /// is reported persisted after it.
    #[tokio::test]
    async fn a_closed_persist_channel_evicts_without_fan_out() {
        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let (h, persist_rx, _events, _done) = Room::spawn_detached(
            doc_id,
            Arc::new(YrsEngine),
            bus,
            sub,
            Log::new(),
            Arc::new(Stamps::default()),
            IDLE,
            None,
        )
        .await
        .unwrap();
        drop(persist_rx);
        let (a, _a_rx) = join(&h).await;
        let (_b, mut b_rx) = join(&h).await;

        send_edit(&h, a, para(11, "lost")).await;
        let (tx, rx) = oneshot::channel();
        let queued =
            h.tx.send(Event::ApplyUpdate {
                update_bytes: para(12, "server"),
                by_user: None,
                source: knot_storage::UpdateSource::Edit,
                meta: None,
                reply: tx,
            })
            .await
            .is_ok();
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
            .await
            .expect("the room was not evicted");
        tokio::time::timeout(std::time::Duration::from_secs(10), h.tx.closed())
            .await
            .expect("the room did not end");
        assert!(drain(&mut b_rx).is_empty(), "the edit was fanned out");
        if queued {
            assert!(
                !matches!(rx.await, Ok(Ok(_))),
                "a server-side edit after the closed channel was reported persisted"
            );
        }
    }

    /// An attempt whose COMMIT acknowledgement was lost is retried, so its
    /// rows land twice; the room learns of the first copy only as a gap
    /// ahead of its own rev, catches up over it, and only then snapshots.
    #[tokio::test(start_paused = true)]
    async fn a_lost_ack_attempt_is_caught_up_before_a_snapshot() {
        let log = Log::new();
        log.with(|s| s.lost_acks = 1);
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
        let (sender, _sender_rx) = join(&h).await;
        let (_peer, mut peer_rx) = join(&h).await;
        probe(&h).await;

        let edit = para(11, "twice");
        send_edit(&h, sender, edit.clone()).await;
        let p = until_probe(&h, "own rev 2 confirmed", |p| p.max_own_rev == 2).await;
        assert_eq!(p.applied_through_rev, 2, "{p:?}");
        assert_eq!(log.appended().len(), 2, "the row was not written twice");
        until("a snapshot", || !snapshots.written().is_empty()).await;
        assert!(
            snapshots
                .written()
                .iter()
                .all(|s| s.snapshot_rev == Some(2)),
            "{:?}",
            snapshots.written()
        );
        let frames = drain(&mut peer_rx);
        assert_eq!(frames.len(), 1, "a copy read back was fanned out again");
        assert!(same_update(&unwrap_frame(&frames[0]), &edit));
        shut_down(&h).await;
    }

    /// The seq a room reports counts only the rows it applied: rows past a
    /// rev gap are not.
    #[tokio::test(start_paused = true)]
    async fn boot_last_seq_ignores_rows_past_a_gap() {
        let log = Log::new();
        log.push(1, 10, para(21, "one"));
        log.push(3, 30, para(23, "three"));
        let (h, _bus, _) = room_on_log(&log, &Arc::new(Stamps::default()), IDLE).await;
        let p = probe(&h).await;
        assert_eq!(p.applied_through_rev, 1, "{p:?}");
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::ExportState(tx)).await.unwrap();
        let (_, seq) = rx.await.unwrap().unwrap();
        assert_eq!(seq, 10);
        shut_down(&h).await;
    }
    // -----------------------------------------------------------------------
    // Server-side edits: persisted before they are fanned out, tagged with
    // what made them, and evicting the room when they cannot be persisted.
    // -----------------------------------------------------------------------

    /// A page holding one unchecked task item ("todo"), from a fresh doc of
    /// `client`.
    fn task_page(client: u64) -> Vec<u8> {
        use yrs::{
            Doc, ReadTxn, StateVector, Transact, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim,
        };
        let doc = Doc::with_client_id(client);
        let frag = doc.get_or_insert_xml_fragment("default");
        {
            let mut txn = doc.transact_mut();
            let list = frag.push_back(&mut txn, XmlElementPrelim::empty("bullet_list"));
            let item = list.push_back(&mut txn, XmlElementPrelim::empty("list_item"));
            item.insert_attribute(&mut txn, "checked", "false");
            let p = item.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("todo"));
        }
        doc.transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    /// The server-side edits whose room awaits their persist.
    #[derive(Debug, Clone, Copy)]
    enum ServerEdit {
        /// Checks the [`task_page`]'s task (`PatchTaskChecked`).
        Task,
        /// Replaces the page with a paragraph (`ReplaceWithMarkdown`).
        Replace,
        /// Appends a paragraph (`ApplyUpdate`).
        Update,
    }

    /// Sends `edit` to `h` and returns its reply.
    async fn server_edit(h: &RoomHandle, edit: ServerEdit) -> Result<i64, String> {
        match edit {
            ServerEdit::Task => {
                let (reply, rx) = oneshot::channel();
                h.tx.send(Event::PatchTaskChecked {
                    item_index: 0,
                    checked: true,
                    by_user: None,
                    reply,
                })
                .await
                .unwrap();
                rx.await.expect("the room answered")
            }
            ServerEdit::Replace => {
                let (reply, rx) = oneshot::channel();
                h.tx.send(Event::ReplaceWithMarkdown {
                    update_bytes: para(31, "replaced"),
                    by_user: None,
                    source: knot_storage::UpdateSource::Import,
                    meta: None,
                    reply,
                })
                .await
                .unwrap();
                rx.await.expect("the room answered")
            }
            ServerEdit::Update => try_apply_update(h, para(32, "appended"))
                .await
                .map_err(|e| e.to_string()),
        }
    }

    /// Sends `update_bytes` as an `ApplyUpdate` and returns its reply.
    async fn try_apply_update(h: &RoomHandle, update_bytes: Vec<u8>) -> Result<i64, EngineError> {
        let (reply, rx) = oneshot::channel();
        h.tx.send(Event::ApplyUpdate {
            update_bytes,
            by_user: None,
            source: knot_storage::UpdateSource::Import,
            meta: None,
            reply,
        })
        .await
        .unwrap();
        rx.await.expect("the room answered")
    }

    /// A server-side edit whose persist fails reaches no client (Review
    /// Focus 5): nobody can push it back on a reconnect, credited to
    /// themselves. The room is evicted and writes no snapshot.
    #[tokio::test(start_paused = true)]
    async fn server_edit_persist_failure_reaches_no_client() {
        for edit in [ServerEdit::Task, ServerEdit::Replace, ServerEdit::Update] {
            let log = Log::new();
            log.push(1, 1, task_page(41));
            log.with(|s| s.refuse_appends = true);
            let snapshots = Arc::new(Stamps::default());
            let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
            let (_conn, mut conn_rx) = join(&h).await;

            let reply = server_edit(&h, edit).await;
            assert!(reply.is_err(), "{edit:?}: replied {reply:?}");
            tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
                .await
                .unwrap_or_else(|_| panic!("{edit:?}: the room was not evicted"));
            tokio::time::timeout(std::time::Duration::from_secs(10), h.tx.closed())
                .await
                .unwrap_or_else(|_| panic!("{edit:?}: the room did not end"));
            assert!(
                drain(&mut conn_rx).is_empty(),
                "{edit:?}: a client got the unpersisted edit"
            );
            assert!(
                snapshots.written().is_empty(),
                "{edit:?}: {:?}",
                snapshots.written()
            );
            assert!(log.appended().is_empty(), "{edit:?}");
        }
    }

    /// Review Focus 3: during an outage a task toggle fails within its
    /// deadline and frees the room (evicted), instead of freezing it for the
    /// outage; the WS edit buffered ahead of it, whose author already left,
    /// is still written once the database is back, and the toggle never is.
    #[tokio::test(start_paused = true)]
    async fn server_edit_during_an_outage_frees_the_room_and_keeps_the_ws_edit() {
        let log = Log::new();
        log.push(1, 1, task_page(41));
        log.with(|s| s.down = true);
        let doc_id = Uuid::new_v4();
        let listened = MemBus::new();
        let sub = listened.subscribe(doc_id).await.unwrap();
        let h = Room::spawn_with_limits(
            doc_id,
            Arc::new(YrsEngine),
            Arc::new(MemBus::new()),
            sub,
            log.clone(),
            Arc::new(Stamps::default()),
            IDLE,
            None,
            WriterLimits {
                server_job_deadline: std::time::Duration::from_millis(300),
                attempt_timeout: std::time::Duration::from_millis(300),
            },
        )
        .await
        .expect("the room boots");
        let (a, _a_rx) = join(&h).await;
        let (_b, mut b_rx) = join(&h).await;
        let a_user = Uuid::new_v4();
        let typed = para(42, "typed");
        h.tx.send(Event::Inbound(InMsg {
            from: a,
            by_user: Some(a_user),
            bytes: typed.clone(),
        }))
        .await
        .unwrap();
        // A closed the tab.
        h.tx.send(Event::Leave(a)).await.unwrap();

        let asked = tokio::time::Instant::now();
        let within = asked + std::time::Duration::from_secs(2);
        let reply = tokio::time::timeout_at(within, server_edit(&h, ServerEdit::Task))
            .await
            .expect("the toggle was not answered within 2 s");
        assert!(reply.is_err(), "{reply:?}");
        tokio::time::timeout_at(within, h.shutdown.cancelled())
            .await
            .expect("the room was not evicted within 2 s");
        let frames = drain(&mut b_rx);
        assert_eq!(
            frames.len(),
            1,
            "B gets exactly the typed edit's frame, not the failed task's: {frames:?}"
        );
        assert!(same_update(&unwrap_frame(&frames[0]), &typed));
        assert!(log.appended().is_empty(), "the database was down");

        log.with(|s| s.down = false);
        let mut done = h.writer_done.clone();
        tokio::time::timeout(std::time::Duration::from_secs(60), done.wait())
            .await
            .expect("the writer did not end once the database was back");
        let appended = log.appended();
        assert_eq!(appended.len(), 1, "{appended:?}");
        assert!(same_update(&appended[0].bytes, &typed));
        assert_eq!(appended[0].by_user_id, Some(a_user));
        assert_eq!(appended[0].source, knot_storage::UpdateSource::Edit);
    }

    /// A server-side edit the writer answers with `Err` evicts the room by
    /// itself, whether or not a writer Fatal comes with it, and reaches no
    /// client.
    #[tokio::test]
    async fn a_server_edit_the_writer_did_not_store_evicts_the_room() {
        let log = Log::new();
        log.push(1, 1, task_page(41));
        let doc_id = Uuid::new_v4();
        let bus = Arc::new(MemBus::new());
        let sub = bus.subscribe(doc_id).await.unwrap();
        let (h, mut persist_rx, _events, _done) = Room::spawn_detached(
            doc_id,
            Arc::new(YrsEngine),
            bus,
            sub,
            log,
            Arc::new(Stamps::default()),
            IDLE,
            None,
        )
        .await
        .unwrap();
        let (_conn, mut conn_rx) = join(&h).await;
        // The writer: no Fatal, only the job's `Err`.
        let writer = tokio::spawn(async move {
            let job = persist_rx.recv().await.expect("the room sent the job");
            let source = job.source;
            let _ = job
                .persisted
                .expect("a server-side job")
                .send(Err("not stored".into()));
            (source, persist_rx)
        });

        let reply = server_edit(&h, ServerEdit::Task).await;
        assert!(reply.is_err(), "{reply:?}");
        let (source, _persist_rx) = writer.await.unwrap();
        assert_eq!(source, knot_storage::UpdateSource::Task);
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
            .await
            .expect("the room was not evicted");
        tokio::time::timeout(std::time::Duration::from_secs(10), h.tx.closed())
            .await
            .expect("the room did not end");
        assert!(drain(&mut conn_rx).is_empty(), "a client got the edit");
    }

    /// An `ApplyUpdate` whose blocks park, waiting for a dependency the room
    /// lacks, persists its raw bytes, so no row misses the parked content;
    /// once the dependency arrives, the rows replayed in order rebuild the
    /// room's doc (review round 3).
    #[tokio::test(start_paused = true)]
    async fn apply_update_with_a_missing_dependency_is_still_persisted() {
        use yrs::{
            GetString, ReadTxn, StateVector, Text, Transact, Update, updates::decoder::Decode,
        };
        let rec = CountingRecorder::default();
        let _g = metrics::set_default_local_recorder(&rec);
        let log = Log::new();
        let (h, _bus, _) = room_on_log(&log, &Arc::new(Stamps::default()), IDLE).await;
        let (_peer, mut peer_rx) = join(&h).await;

        let client = yrs::Doc::new();
        let text = client.get_or_insert_text("t");
        text.push(&mut client.transact_mut(), "a");
        let sv = client.transact().state_vector();
        let dependency = client
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        text.push(&mut client.transact_mut(), "b");
        let dependent = client.transact().encode_state_as_update_v1(&sv);

        try_apply_update(&h, dependent.clone())
            .await
            .expect("a parked update is persisted");
        let appended = log.appended();
        assert_eq!(appended.len(), 1, "{appended:?}");
        assert!(same_update(&appended[0].bytes, &dependent));
        assert_eq!(rec.counter("knot_room_updates_persisted_raw_total", &[]), 1);
        let frames = drain(&mut peer_rx);
        assert_eq!(frames.len(), 1);
        assert!(same_update(&unwrap_frame(&frames[0]), &dependent));

        try_apply_update(&h, dependency)
            .await
            .expect("the dependency is persisted");
        let replayed = yrs::Doc::new();
        for row in log.appended() {
            replayed
                .transact_mut()
                .apply_update(Update::decode_v1(&row.bytes).unwrap())
                .unwrap();
        }
        let (tx, rx) = oneshot::channel();
        h.tx.send(Event::ExportState(tx)).await.unwrap();
        let (state, _) = rx.await.unwrap().unwrap();
        let live = yrs::Doc::new();
        live.transact_mut()
            .apply_update(Update::decode_v1(&state).unwrap())
            .unwrap();
        let text_of = |d: &yrs::Doc| d.get_or_insert_text("t").get_string(&d.transact());
        assert_eq!(text_of(&live), "ab");
        assert_eq!(text_of(&replayed), "ab");
        assert_eq!(
            replayed.transact().state_vector(),
            live.transact().state_vector()
        );
        shut_down(&h).await;
    }

    /// An `ApplyUpdate` the doc already holds persists and fans out nothing,
    /// and replies `Ok`.
    #[tokio::test(start_paused = true)]
    async fn apply_update_that_changes_nothing_persists_nothing() {
        let held = para(11, "held");
        let log = Log::new();
        log.push(1, 1, held.clone());
        let (h, _bus, _) = room_on_log(&log, &Arc::new(Stamps::default()), IDLE).await;
        let (_conn, mut conn_rx) = join(&h).await;

        try_apply_update(&h, held)
            .await
            .expect("a no-op update is answered Ok");
        tokio::time::sleep(crate::writer::BATCH_INTERVAL * 4).await;
        assert!(log.appended().is_empty(), "{:?}", log.appended());
        assert!(drain(&mut conn_rx).is_empty(), "a no-op was fanned out");
        assert!(!h.shutdown.is_cancelled());
        shut_down(&h).await;
    }

    /// An `ApplyUpdate` yrs rejects part-way leaves part of it in the doc,
    /// which no row carries: the caller gets `Err`, nothing is persisted or
    /// fanned out, and the room is evicted (`unlogged` as well, so no
    /// snapshot certifies it).
    #[tokio::test(start_paused = true)]
    async fn apply_update_rejected_part_way_evicts() {
        let (parked, edit, _) = parked_block_and_the_edit_it_fails_on();
        let log = Log::new();
        log.push(1, 1, parked);
        let snapshots = Arc::new(Stamps::default());
        let (h, _bus, _) = room_on_log(&log, &snapshots, EAGER).await;
        let (_conn, mut conn_rx) = join(&h).await;

        let reply = try_apply_update(&h, edit).await;
        assert!(reply.is_err(), "{reply:?}");
        tokio::time::timeout(std::time::Duration::from_secs(10), h.shutdown.cancelled())
            .await
            .expect("the room was not evicted");
        tokio::time::timeout(std::time::Duration::from_secs(10), h.tx.closed())
            .await
            .expect("the room did not end");
        assert!(log.appended().is_empty(), "{:?}", log.appended());
        assert!(drain(&mut conn_rx).is_empty(), "the edit was fanned out");
        assert!(snapshots.written().is_empty(), "{:?}", snapshots.written());
    }

    /// Replacement bytes that do not decode are refused before the page is
    /// cleared: the doc keeps its content, and the room keeps running.
    #[tokio::test(start_paused = true)]
    async fn replace_with_undecodable_bytes_leaves_the_doc_untouched() {
        let log = Log::new();
        log.push(1, 1, para(11, "kept"));
        let (h, _bus, _) = room_on_log(&log, &Arc::new(Stamps::default()), IDLE).await;
        let before = exported_page(&h).await;
        assert!(before.contains("kept"), "{before}");

        let (reply, rx) = oneshot::channel();
        h.tx.send(Event::ReplaceWithMarkdown {
            update_bytes: b"\xff".to_vec(),
            by_user: None,
            source: knot_storage::UpdateSource::Import,
            meta: None,
            reply,
        })
        .await
        .unwrap();
        assert!(rx.await.unwrap().is_err());
        assert_eq!(exported_page(&h).await, before);
        assert!(!h.shutdown.is_cancelled(), "the room was evicted");
        assert!(log.appended().is_empty());
        shut_down(&h).await;
    }
}
