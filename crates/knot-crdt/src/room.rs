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
use crate::protocol::wrap_sync_update;

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
    Revoke,
    ExportState(oneshot::Sender<Result<(Vec<u8>, i64), EngineError>>),
    ApplyUpdate {
        update_bytes: Vec<u8>,
        by_user: Option<Uuid>,
        reply: oneshot::Sender<Result<i64, EngineError>>,
    },
    /// Replace the entire document content with the given pre-parsed update bytes.
    /// Clears the live doc's "default" XmlFragment then applies the update,
    /// all in a single Yrs transaction. Persists + fans out via the writer,
    /// same as `ApplyUpdate`. The caller is responsible for converting markdown
    /// to update bytes (e.g. via `knot_markdown::from_markdown::parse`).
    ReplaceWithMarkdown {
        /// Full-state update bytes encoding the replacement content.
        update_bytes: Vec<u8>,
        reply: oneshot::Sender<Result<i64, String>>,
    },
    /// Flip the `checked` attribute on the Nth task `list_item` in the doc.
    /// `item_index` counts list_item nodes with a `checked` attr in
    /// document order — matches the index produced by
    /// `knot_markdown::tasks::extract_tasks`. The mutation happens
    /// directly on the live doc; the resulting update is persisted and
    /// fanned out via the same pipeline as `ApplyUpdate`.
    PatchTaskChecked {
        item_index: i32,
        checked: bool,
        by_user: Option<Uuid>,
        reply: oneshot::Sender<Result<i64, String>>,
    },
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
    updates_store: Arc<dyn knot_storage::UpdatesStore>,
    policy: crate::snapshot::SnapshotPolicy,
    snap_state: crate::snapshot::SnapshotState,
    /// Server-side reindex notifier (see [`crate::Rooms::with_dirty_tx`]).
    /// Fires after the writer confirms a successful persist; consumers
    /// debounce + reindex tasks for the doc.
    dirty_tx: Option<mpsc::Sender<Uuid>>,
}

pub struct RoomHandle {
    pub tx: mpsc::Sender<Event>,
    pub shutdown: CancellationToken,
}

impl Room {
    /// Spawn a room, hydrating its `DocHandle` from the latest snapshot and
    /// replaying any updates persisted after that snapshot. The actor starts
    /// with `last_applied_seq` set to the highest seq applied during hydration.
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
        crate::writer::spawn(
            doc_id,
            updates_store.clone(),
            bus.clone(),
            persist_rx,
            applied_tx,
        );

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
            updates_store,
            policy,
            snap_state,
            dirty_tx,
        };
        tokio::spawn(room.run());
        Ok(RoomHandle { tx, shutdown })
    }

    #[tracing::instrument(skip_all, fields(doc_id = %self.doc_id))]
    async fn run(mut self) {
        let mut idle_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut catchup_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = catchup_tick.tick() => {
                    self.replay_since_watermark().await;
                }
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
                        metrics::counter!("knot_room_snapshots_total").increment(1);
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
                    Some(Event::ApplyUpdate { update_bytes, by_user, reply }) => {
                        // Apply to the live doc.
                        if let Err(e) = self.engine.apply_update(&self.doc, &update_bytes) {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        metrics::counter!("knot_room_updates_total", "source" => "local").increment(1);
                        // Persist via the writer (backpressured). Wait for
                        // the durable-insert confirmation before replying so
                        // a crash between reply and persist doesn't lose the
                        // update.
                        let (persisted_tx, persisted_rx) = tokio::sync::oneshot::channel();
                        let _ = self.persist_tx.send(crate::writer::PersistJob {
                            bytes: update_bytes.clone(),
                            by_user_id: by_user,
                            persisted: Some(persisted_tx),
                        }).await;
                        // Local fan-out (slow-consumer eviction).
                        let framed = wrap_sync_update(&update_bytes);
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
                            match conn.tx.try_send(framed.clone()) {
                                Ok(_) => {}
                                Err(_) => to_close.push(*cid),
                            }
                        }
                        for cid in to_close { self.conns.remove(&cid); }
                        let final_seq = match persisted_rx.await {
                            Ok(Ok(seq)) => seq,
                            Ok(Err(e)) => {
                                let _ = reply.send(Err(EngineError::Apply(format!("persist: {e}"))));
                                continue;
                            }
                            Err(_) => {
                                let _ = reply.send(Err(EngineError::Apply("persist channel closed".into())));
                                continue;
                            }
                        };
                        let _ = reply.send(Ok(final_seq));
                    }
                    Some(Event::ExportState(reply)) => {
                        let r = self.engine
                            .encode_state_as_update(&self.doc, None)
                            .map(|bytes| (bytes, self.last_applied_seq));
                        let _ = reply.send(r);
                    }
                    Some(Event::ReplaceWithMarkdown { update_bytes, reply }) => {
                        // In a single transaction on the live doc:
                        //   1. Clear the existing "default" XmlFragment content.
                        //   2. Apply the pre-parsed update bytes.
                        {
                            use yrs::{Transact, Update, XmlFragment, updates::decoder::Decode};
                            // Get (or create) the fragment reference BEFORE opening the
                            // mutable transaction, because get_or_insert_xml_fragment
                            // internally acquires its own write lock. Calling it while
                            // a TransactionMut is active would deadlock (write_blocking).
                            let frag = self.doc.inner().get_or_insert_xml_fragment("default");
                            let mut txn = self.doc.inner().transact_mut();
                            let len = frag.len(&txn);
                            if len > 0 {
                                frag.remove_range(&mut txn, 0, len);
                            }
                            let upd = match Update::decode_v1(&update_bytes) {
                                Ok(u) => u,
                                Err(e) => {
                                    let _ = reply.send(Err(format!("decode: {e}")));
                                    continue;
                                }
                            };
                            if let Err(e) = txn.apply_update(upd) {
                                let _ = reply.send(Err(format!("apply: {e}")));
                                continue;
                            }
                        }
                        metrics::counter!("knot_room_updates_total", "source" => "restore").increment(1);
                        // Persist via the writer (mirrors ApplyUpdate path).
                        let (persisted_tx, persisted_rx) = tokio::sync::oneshot::channel();
                        let _ = self.persist_tx.send(crate::writer::PersistJob {
                            bytes: update_bytes.clone(),
                            by_user_id: None,
                            persisted: Some(persisted_tx),
                        }).await;
                        // Fan out the replace to local connections.
                        let framed = wrap_sync_update(&update_bytes);
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
                            match conn.tx.try_send(framed.clone()) {
                                Ok(_) => {}
                                Err(_) => to_close.push(*cid),
                            }
                        }
                        for cid in to_close { self.conns.remove(&cid); }
                        let final_seq = match persisted_rx.await {
                            Ok(Ok(seq)) => seq,
                            Ok(Err(e)) => {
                                let _ = reply.send(Err(format!("persist: {e}")));
                                continue;
                            }
                            Err(_) => {
                                let _ = reply.send(Err("persist channel closed".into()));
                                continue;
                            }
                        };
                        let _ = reply.send(Ok(final_seq));
                    }
                    Some(Event::PatchTaskChecked { item_index, checked, by_user, reply }) => {
                        use std::sync::{Arc, Mutex};
                        use yrs::Transact;
                        let frag = self.doc.inner().get_or_insert_xml_fragment("default");

                        // Subscribe to the v1 update stream so we can capture
                        // the bytes the mutation produces. We immediately
                        // drop the subscription after the txn closes.
                        let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
                        let captured_clone = captured.clone();
                        let sub = self.doc.inner().observe_update_v1(move |_txn, e| {
                            *captured_clone.lock().unwrap() = Some(e.update.clone());
                        });
                        let patch_result = {
                            let mut txn = self.doc.inner().transact_mut();
                            patch_task_checked(&frag, &mut txn, item_index, checked)
                        };
                        // Dropping the subscription must happen AFTER the
                        // txn closes (Drop on TransactionMut flushes the
                        // update notification).
                        drop(sub);

                        if let Err(e) = patch_result {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                        let Some(update_bytes) = captured.lock().unwrap().take() else {
                            // No update emitted means the attribute was
                            // already set to the requested value — nothing
                            // to do, treat as success.
                            let _ = reply.send(Ok(self.last_applied_seq));
                            continue;
                        };
                        metrics::counter!("knot_room_updates_total", "source" => "patch").increment(1);
                        let (persisted_tx, persisted_rx) = tokio::sync::oneshot::channel();
                        let _ = self.persist_tx.send(crate::writer::PersistJob {
                            bytes: update_bytes.clone(),
                            by_user_id: by_user,
                            persisted: Some(persisted_tx),
                        }).await;
                        let framed = wrap_sync_update(&update_bytes);
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
                            match conn.tx.try_send(framed.clone()) {
                                Ok(_) => {}
                                Err(_) => to_close.push(*cid),
                            }
                        }
                        for cid in to_close { self.conns.remove(&cid); }
                        let final_seq = match persisted_rx.await {
                            Ok(Ok(seq)) => seq,
                            Ok(Err(e)) => {
                                let _ = reply.send(Err(format!("persist: {e}")));
                                continue;
                            }
                            Err(_) => {
                                let _ = reply.send(Err("persist channel closed".into()));
                                continue;
                            }
                        };
                        let _ = reply.send(Ok(final_seq));
                    }
                    Some(Event::Revoke) => {
                        // Drop all conns. WS shim's writer task sees the
                        // closed channel and closes the socket with 4403.
                        self.conns.clear();
                    }
                    Some(Event::Shutdown) | None => break,
                },
                Some(applied) = self.applied_rx.recv() => {
                    if applied.seq > self.last_applied_seq {
                        self.last_applied_seq = applied.seq;
                    }
                    self.snap_state.updates_since_snapshot += 1;
                    self.snap_state.last_apply_at = std::time::Instant::now();
                    // Notify the server-side reindex worker that this
                    // doc has new content. `try_send` so a full channel
                    // is dropped — the next applied update will refire.
                    if let Some(tx) = &self.dirty_tx {
                        let _ = tx.try_send(self.doc_id);
                    }
                    if self.snap_state.updates_since_snapshot >= self.policy.every_n {
                        if let Err(e) = crate::snapshot::write_snapshot(
                            self.doc_id, self.last_applied_seq,
                            self.engine.as_ref(), &self.doc,
                            self.snapshots.as_ref(),
                        ).await {
                            tracing::warn!(error=?e, "snapshot write failed");
                        } else {
                            metrics::counter!("knot_room_snapshots_total").increment(1);
                            self.snap_state.last_snapshot_seq = self.last_applied_seq;
                            self.snap_state.updates_since_snapshot = 0;
                        }
                    }
                }
                Some(_seq) = self.bus_updates_rx.recv() => {
                    self.replay_since_watermark().await;
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

        // Final flush: write a snapshot at the current seq so the next boot
        // is cheap. Best-effort.
        let _ = crate::snapshot::write_snapshot(
            self.doc_id,
            self.last_applied_seq,
            self.engine.as_ref(),
            &self.doc,
            self.snapshots.as_ref(),
        )
        .await;
    }

    async fn replay_since_watermark(&mut self) {
        match self
            .updates_store
            .since(self.doc_id, self.last_applied_seq)
            .await
        {
            Ok(rows) => {
                for u in rows {
                    if u.seq <= self.last_applied_seq {
                        continue;
                    }
                    if self.engine.apply_update(&self.doc, &u.update_bytes).is_ok() {
                        metrics::counter!("knot_room_updates_total", "source" => "peer")
                            .increment(1);
                        let framed = wrap_sync_update(&u.update_bytes);
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
                            match conn.tx.try_send(framed.clone()) {
                                Ok(_) => {}
                                Err(_) => to_close.push(*cid),
                            }
                        }
                        for cid in to_close {
                            self.conns.remove(&cid);
                        }
                        self.last_applied_seq = u.seq;
                        // Notify the server-side reindex worker so
                        // non-writing replicas refresh their /tasks
                        // index when an update arrives via the bus.
                        // Without this, only the replica that owns
                        // the WS connection ever ticks the indexer.
                        if let Some(tx) = &self.dirty_tx {
                            let _ = tx.try_send(self.doc_id);
                        }
                    }
                }
            }
            Err(e) => tracing::debug!(error=?e, "catch-up replay failed"),
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

    #[tracing::instrument(skip(self, m), fields(doc_id = %self.doc_id, bytes = m.bytes.len()))]
    async fn on_inbound(&mut self, m: InMsg) {
        if let Err(e) = self.engine.apply_update(&self.doc, &m.bytes) {
            tracing::debug!(error=?e, "apply_update failed");
            return;
        }
        metrics::counter!("knot_room_updates_total", "source" => "local").increment(1);
        let framed = wrap_sync_update(&m.bytes);
        let mut to_close: Vec<ConnId> = Vec::new();
        for (cid, conn) in &self.conns {
            if *cid == m.from {
                continue;
            }
            match conn.tx.try_send(framed.clone()) {
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
                // Inbound WS updates are fire-and-forget — the client
                // already has the update locally, so we don't need to
                // hold up acknowledgement on the durable insert.
                persisted: None,
            })
            .await
        {
            tracing::error!(error=?e, "persist channel closed; dropping update");
        }
    }
}

/// Walk a fragment in document order, find the Nth `list_item` element that
/// carries a `checked` attribute, and update that attribute. Returns the
/// number of matching items walked (so callers can distinguish "no match"
/// from a successful patch).
fn patch_task_checked(
    frag: &yrs::XmlFragmentRef,
    txn: &mut yrs::TransactionMut,
    target_index: i32,
    checked: bool,
) -> Result<(), String> {
    use yrs::{Xml, XmlFragment, XmlOut};
    if target_index < 0 {
        return Err("negative item_index".into());
    }
    let mut counter: i32 = 0;
    let mut found: Option<yrs::XmlElementRef> = None;
    // Iterative DFS to avoid recursion + lifetime juggling on the txn.
    let mut stack: Vec<yrs::XmlElementRef> = Vec::new();
    // Seed with top-level children.
    let top_len = frag.len(txn);
    for i in (0..top_len).rev() {
        if let Some(XmlOut::Element(el)) = frag.get(txn, i) {
            stack.push(el);
        }
    }
    while let Some(el) = stack.pop() {
        if el.tag().as_ref() == "list_item" && el.get_attribute(txn, "checked").is_some() {
            if counter == target_index {
                found = Some(el);
                break;
            }
            counter += 1;
        }
        // Push children (in reverse so document order is preserved on pop).
        let n = el.len(txn);
        for i in (0..n).rev() {
            if let Some(XmlOut::Element(child)) = el.get(txn, i) {
                stack.push(child);
            }
        }
    }
    let Some(el) = found else {
        return Err(format!("no task at index {target_index}"));
    };
    el.insert_attribute(txn, "checked", if checked { "true" } else { "false" });
    Ok(())
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
                snapshot.push(li.get_attribute(&txn, "checked"));
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
            None,
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
