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

use axum::extract::ws::{Message, WebSocket};
use futures::StreamExt;
use knot_crdt::{ConnHandle, ConnId, Event, InMsg};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::protocol::{YSyncMessage, decode, encode_sync_step1, encode_sync_step2};

pub async fn serve(
    rooms: Arc<knot_crdt::Rooms>,
    doc_id: Uuid,
    user_id: Uuid,
    socket: WebSocket,
    can_write: bool,
    shutdown: CancellationToken,
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
    // Cancelled by the room when an ACL change revokes this connection; the
    // writer then closes the socket with 4403.
    let revoked = CancellationToken::new();
    let conn_handle = || ConnHandle {
        tx: out_tx.clone(),
        revoked: revoked.clone(),
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
    let _ = out_tx.send(encode_sync_step2(&joined.update)).await;
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
    if can_write {
        let _ = out_tx.send(encode_sync_step1(&joined.state_vector)).await;
    }
    // Server-side share of "open a doc, wait for content": includes the
    // hydrate of a cold room (see knot_room_hydrate_seconds) and the wait for
    // the room actor's reply.
    metrics::histogram!("knot_collab_initial_sync_seconds")
        .record(join_started.elapsed().as_secs_f64());

    let (sink, mut stream) = socket.split();
    // Cancelled by whichever half ends the session first, so the other one
    // stops too: the writer on a revoke, a server shutdown or a dead socket;
    // this read loop when the client goes away.
    let done = CancellationToken::new();
    let writer = tokio::spawn(crate::ws_writer::run(
        sink,
        out_rx,
        shutdown.clone(),
        revoked.clone(),
        done.clone(),
    ));

    loop {
        let msg = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = done.cancelled() => break,
            m = stream.next() => match m {
                Some(Ok(m)) => m,
                _ => break,
            },
        };
        match msg {
            Message::Binary(bytes) => {
                match decode(&bytes) {
                    Ok(YSyncMessage::SyncStep1(_sv)) => {
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
                    Ok(YSyncMessage::Awareness) => {
                        let _ = handle
                            .tx
                            .send(Event::AwarenessIn {
                                from: conn_id,
                                payload: bytes.to_vec(),
                            })
                            .await;
                    }
                    Err(_) => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    // Stop the writer before waiting for it: this function still holds a
    // sender, so the channel alone would never end it.
    done.cancel();
    let _ = handle.tx.send(Event::Leave(conn_id)).await;
    let _ = writer.await;
}
