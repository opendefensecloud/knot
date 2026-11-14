//! WebSocket → BoardRoom shim. Mirrors `room::serve` but against the
//! `BoardRooms` registry, since boards have their own y-protocol session.
//!
//! A board's access is its page's (`doc_id`). `acl_epoch_at_check` is the
//! page's ACL epoch (`BoardRooms::acl_epoch`), read at upgrade before the
//! access check; once joined, a different epoch closes the connection with
//! 4403 before it is sent anything, as in `room::serve`.
//!
//! `user_id` is the authenticated user behind the socket, whose board
//! budgets (`rate_limits`) the read loop charges and waits off exactly as
//! `room::serve` does with the doc budgets.

use axum::extract::ws::{Message, WebSocket};
use futures::StreamExt;
use knot_crdt::board_room::{ConnHandle, ConnId, Event, InMsg};
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
    rooms: Arc<knot_crdt::BoardRooms>,
    board_id: Uuid,
    doc_id: Uuid,
    user_id: Uuid,
    acl_epoch_at_check: u64,
    socket: WebSocket,
    shutdown: CancellationToken,
    rate_limits: Arc<RateLimits>,
) {
    let handle = match rooms.acquire(board_id, doc_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, %board_id, "board room acquire failed; closing socket");
            return;
        }
    };
    let conn_id: ConnId = Uuid::new_v4();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(256);
    // Cancelled by the board room when an ACL change on the page revokes
    // this connection, and with the room's own when `revoke_for_doc`
    // revokes them all; the writer then closes the socket with 4403.
    let revoked = handle.revoked.child_token();
    // Cancelled by the board room when it refuses an update this connection
    // sent (it nests the board too deep, or it doesn't decode); the writer
    // then closes with 4422.
    let refused = CancellationToken::new();
    let conn_handle = || ConnHandle {
        tx: out_tx.clone(),
        revoked: revoked.clone(),
        refused: refused.clone(),
    };

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
    // The join race, as in `room::serve`: a revoke that missed this
    // connection shows as a changed epoch. It is sent nothing.
    if rooms.acl_epoch(doc_id) != acl_epoch_at_check {
        revoked.cancel();
    }
    if !revoked.is_cancelled() {
        let _ = out_tx.send(encode_sync_step2(&joined.update)).await;
        // Ask the client for what we lack, exactly as `room::serve` does: the
        // board provider has the same offline gap (edits made while the
        // socket is down are applied locally but never sent) and the same
        // SyncStep1 answer. Sent as its own WS message — BoardProvider
        // decodes one y-protocol message per frame.
        let _ = out_tx.send(encode_sync_step1(&joined.state_vector)).await;
    }

    let (sink, mut stream) = socket.split();
    // As in `room::serve`: whichever half ends the session cancels `done` so
    // the other stops. A revoked connection is closed with 4403, and a
    // refused sender with 4422. When the board room ends (it evicts itself
    // after refusing an update it had applied, which nested the board too
    // deep, or its actor stops), its token closes every other connection with
    // 1012, so each client reconnects to a fresh board room and re-sends
    // its edits.
    let done = CancellationToken::new();
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

    // As in `room::serve`: wait off the debt before every read, coalesce
    // presence, answer one client SyncStep1 (every one with limits off).
    let liveness = rate_limits.liveness;
    let mut throttle = ConnectionThrottle::new(rate_limits, user_id, CollabKind::Board);
    let signals = SessionSignals {
        shutdown: &shutdown,
        revoked: &revoked,
        refused: &refused,
        room_ended: &handle.shutdown,
        done: &done,
    };
    let mut heard_from_client_at = Instant::now();
    loop {
        let wait_started_at = Instant::now();
        if throttle.wait_for_budget(&signals).await == WaitOutcome::Stop {
            break;
        }
        heard_from_client_at += wait_started_at.elapsed();
        let awareness_due = throttle.pending_awareness_due();
        let msg = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = done.cancelled() => break,
            // A board room takes updates from any connection: forward
            // nothing more from a revoked one.
            _ = revoked.cancelled() => break,
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
            Message::Binary(bytes) => match decode(&bytes) {
                Ok(YSyncMessage::SyncStep1(_sv)) if throttle.admit_sync_step1() => {
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
                    let _ = handle
                        .tx
                        .send(Event::Inbound(InMsg {
                            from: conn_id,
                            bytes: inner,
                        }))
                        .await;
                }
                Ok(YSyncMessage::SyncStep1(_sv)) => {}
                Ok(YSyncMessage::Awareness) => {
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
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    // The client is gone (a send to it failed), possibly during a budget
    // wait that left frames it had already sent unread: forward the edits
    // among them that are readable now, so none is lost. Not after a revoke,
    // a refusal, a room end or a shutdown, which forward nothing more.
    if signals.client_gone() {
        for update in throttle.drain_readable_updates(&mut stream) {
            let _ = handle
                .tx
                .send(Event::Inbound(InMsg {
                    from: conn_id,
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
