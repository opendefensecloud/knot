//! WebSocket → Room shim. Auth happens at upgrade in lib.rs's
//! `collab_upgrade`; this shim just plumbs an authenticated socket into
//! the knot-crdt Rooms registry.
//!
//! `can_write` is the effective-role gate decided at upgrade: Viewers may
//! connect and hydrate (read), but their inbound CRDT updates are dropped so
//! a read-only grant cannot mutate the document over the socket.
//!
//! `user_id` is the authenticated user behind the socket. Every update the
//! connection forwards carries it, so the room persists live edits under
//! their author and credits that user as a contributor.
//!
//! `acl_epoch_at_check` is the doc's ACL epoch (`Rooms::acl_epoch`), read at
//! upgrade before the access check. Once joined, a different epoch means a
//! revoke landed between that check and the join and may have missed this
//! connection, which is then closed with 4403 before it is sent anything.
//!
//! `rate_limits` holds the collab budgets (`rate_limit::collab`): before every
//! read the loop waits off the user's byte and message debt (backpressure;
//! nothing is dropped), presence past its budget is coalesced to the newest
//! state per awareness client, and a second client `SyncStep1` is left
//! unanswered. With limits on, the writer also pings the client, and a
//! client the loop hears nothing from for the silence limit is let go
//! (`rate_limit::Liveness`). With limits off, the loop behaves exactly as
//! without them.

use axum::extract::ws::{Message, WebSocket};
use futures::StreamExt;
use knot_crdt::{ConnHandle, ConnId, Event, InMsg};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::protocol::{YSyncMessage, decode, encode_sync_step1, encode_sync_step2};
use crate::rate_limit::collab::{silence_deadline, sleep_until_due};
use crate::rate_limit::{CollabKind, ConnectionThrottle, RateLimits, SessionSignals, WaitOutcome};

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    rooms: Arc<knot_crdt::Rooms>,
    doc_id: Uuid,
    user_id: Uuid,
    socket: WebSocket,
    can_write: bool,
    acl_epoch_at_check: u64,
    shutdown: CancellationToken,
    rate_limits: Arc<RateLimits>,
) {
    let join_started = std::time::Instant::now();
    let handle = match rooms.acquire(doc_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "room acquire failed; closing socket");
            return;
        }
    };
    let conn_id: ConnId = Uuid::new_v4();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(256);
    // Cancelled by the room when an ACL change revokes this connection, and
    // with the room's own when a purge revokes them all; the writer then
    // closes the socket with 4403.
    let revoked = handle.revoked.child_token();
    // Cancelled by the room when it refuses an update this connection sent
    // (it nests the page too deep, or it doesn't decode); the writer then
    // closes with 4422.
    let refused = CancellationToken::new();
    let conn_handle = || ConnHandle {
        tx: out_tx.clone(),
        revoked: revoked.clone(),
        refused: refused.clone(),
    };

    // Join — receive hydrated state as bytes; wrap in sync_step_2 frame.
    let (reply_tx, reply_rx) = oneshot::channel();
    if handle
        .tx
        .send(Event::Join {
            conn_id,
            handle: conn_handle(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return;
    }
    let joined = match reply_rx.await {
        Ok(Ok(j)) => j,
        _ => return,
    };
    // The join race: a revoke between the upgrade's access check and this
    // join found no connection to revoke. It bumped the epoch before it
    // revoked anything, so the change shows here: revoke this connection as
    // the revoke would have. It is sent nothing; the writer closes it with
    // 4403 and the read loop below stops at once.
    if rooms.acl_epoch(doc_id) != acl_epoch_at_check {
        revoked.cancel();
    }
    if !revoked.is_cancelled() {
        let _ = out_tx.send(encode_sync_step2(&joined.update)).await;
    }
    // Ask the client for what WE lack (standard y-protocol: both sides send
    // SyncStep1). The web provider applies edits made while its socket was
    // down to its local doc without queueing them, and its own SyncStep1 only
    // fetches what the client lacks — so without this ask, offline edits never
    // reach the server, and every later edit that builds on them is parked as
    // pending by yrs and stays invisible to everyone else. The provider
    // already answers a server SyncStep1 with
    // `encodeStateAsUpdate(doc, serverSV)`, so already-deployed bundles are
    // fixed by this alone. Not sent to viewers: they cannot have edits to
    // upload, and their reply (at least the whole delete set) would only be
    // dropped below.
    if can_write && !revoked.is_cancelled() {
        let _ = out_tx.send(encode_sync_step1(&joined.state_vector)).await;
    }
    // Server-side share of "open a doc, wait for content": includes the
    // hydrate of a cold room (see knot_room_hydrate_seconds) and the wait for
    // the room actor's reply.
    metrics::histogram!("knot_collab_initial_sync_seconds")
        .record(join_started.elapsed().as_secs_f64());

    let (sink, mut stream) = socket.split();
    // Cancelled by whichever half ends the session first, so the other one
    // stops too: the writer on a revoke, a refusal, the room's end, a server
    // shutdown or a dead socket; this read loop when the client goes away.
    let done = CancellationToken::new();
    // The room's own token, cancelled when the room ends: it evicts itself
    // after refusing an update it had applied, which nested the page too
    // deep (this connection's or another one's), it is evicted, or its
    // actor stops. The writer then closes with 1012 so the client reconnects
    // to a fresh room and re-sends its edits; this session's room would no
    // longer apply, persist or share them. A refused sender gets 4422, and
    // a revoked connection 4403 (both checked first).
    let writer = tokio::spawn(crate::ws_writer::run(
        sink,
        out_rx,
        shutdown.clone(),
        revoked.clone(),
        refused.clone(),
        handle.shutdown.clone(),
        done.clone(),
        rate_limits.liveness.map(|liveness| liveness.ping_interval),
    ));

    let liveness = rate_limits.liveness;
    let mut throttle = ConnectionThrottle::new(rate_limits, user_id, CollabKind::Doc);
    let signals = SessionSignals {
        shutdown: &shutdown,
        revoked: &revoked,
        refused: &refused,
        room_ended: &handle.shutdown,
        done: &done,
    };
    // When the loop last read anything from the client (see `Liveness`).
    let mut heard_from_client_at = Instant::now();
    loop {
        // Before every read, the first included: wait off this user's debt.
        // The writer keeps sending meanwhile; any end of the session ends
        // the wait at once.
        let wait_started_at = Instant::now();
        if throttle.wait_for_budget(&signals).await == WaitOutcome::Stop {
            break;
        }
        // The loop read nothing while it waited: not the client's silence.
        heard_from_client_at += wait_started_at.elapsed();
        let awareness_due = throttle.pending_awareness_due();
        let msg = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = done.cancelled() => break,
            // Forward nothing more from a revoked connection: the room may
            // still hold it (one revoked for the join race) until it leaves.
            _ = revoked.cancelled() => break,
            // A presence frame held for want of a token (never with limits
            // off): forward it once one is back.
            _ = sleep_until_due(awareness_due) => {
                if let Some(payload) = throttle.take_due_awareness() {
                    let _ = handle
                        .tx
                        .send(Event::AwarenessIn {
                            from: conn_id,
                            payload,
                        })
                        .await;
                }
                continue;
            }
            m = stream.next() => match m {
                Some(Ok(m)) => m,
                _ => break,
            },
            // Nothing read, not even a pong, for the silence limit: the
            // client vanished without closing (see `Liveness`).
            _ = sleep_until_due(silence_deadline(liveness, heard_from_client_at)) => {
                tracing::debug!(user = %user_id, "collab client silent; ending the session");
                break;
            }
        };
        heard_from_client_at = Instant::now();
        throttle.charge_inbound(&msg);
        match msg {
            Message::Binary(bytes) => {
                match decode(&bytes) {
                    // With limits on, only the first is answered: the full
                    // state went out on join, and each answer costs a
                    // full-state encode in the room actor.
                    Ok(YSyncMessage::SyncStep1(_sv)) if throttle.admit_sync_step1() => {
                        // Reply with full state again — cheap, idempotent.
                        let (rtx, rrx) = oneshot::channel();
                        let _ = handle
                            .tx
                            .send(Event::Join {
                                conn_id,
                                handle: conn_handle(),
                                reply: rtx,
                            })
                            .await;
                        if let Ok(Ok(joined)) = rrx.await {
                            let _ = out_tx.send(encode_sync_step2(&joined.update)).await;
                        }
                    }
                    Ok(YSyncMessage::SyncStep2(inner)) | Ok(YSyncMessage::Update(inner)) => {
                        // Read-only (Viewer) connections may hydrate but must
                        // never mutate the document — drop their inbound updates.
                        if can_write {
                            let _ = handle
                                .tx
                                .send(Event::Inbound(InMsg {
                                    from: conn_id,
                                    by_user: Some(user_id),
                                    bytes: inner,
                                }))
                                .await;
                        }
                    }
                    Ok(YSyncMessage::SyncStep1(_sv)) => {}
                    Ok(YSyncMessage::Awareness) => {
                        // Past the presence budget the frame is held, and a
                        // newer one is merged into it (per awareness client
                        // the newest state always arrives).
                        if let Some(payload) = throttle.admit_awareness(bytes.to_vec()) {
                            let _ = handle
                                .tx
                                .send(Event::AwarenessIn {
                                    from: conn_id,
                                    payload,
                                })
                                .await;
                        }
                    }
                    Err(_) => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    // The client is gone (a send to it failed), possibly during a budget
    // wait that left frames it had already sent unread: forward the edits
    // among them that are readable now, so none is lost. Not after a revoke,
    // a refusal, a room end or a shutdown, which forward nothing more.
    if can_write && signals.client_gone() {
        for update in throttle.drain_readable_updates(&mut stream) {
            let _ = handle
                .tx
                .send(Event::Inbound(InMsg {
                    from: conn_id,
                    by_user: Some(user_id),
                    bytes: update,
                }))
                .await;
        }
    }
    // Stop the writer before waiting for it: this function still holds a
    // sender, so the channel alone would never end it.
    done.cancel();
    let _ = handle.tx.send(Event::Leave(conn_id)).await;
    let _ = writer.await;
}
