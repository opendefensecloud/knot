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

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
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
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);

    // Join — receive hydrated state as bytes; wrap in sync_step_2 frame.
    let (reply_tx, reply_rx) = oneshot::channel();
    if handle
        .tx
        .send(Event::Join {
            conn_id,
            handle: ConnHandle { tx: out_tx.clone() },
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
    // fixed by this alone. Viewers answer too; their reply is dropped below
    // like any other inbound update from a read-only connection.
    let _ = out_tx.send(encode_sync_step1(&joined.state_vector)).await;
    // Server-side share of "open a doc, wait for content": includes the
    // hydrate of a cold room (see knot_room_hydrate_seconds) and the wait for
    // the room actor's reply.
    metrics::histogram!("knot_collab_initial_sync_seconds")
        .record(join_started.elapsed().as_secs_f64());

    let (mut sink, mut stream) = socket.split();
    let writer_shutdown = shutdown.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                // Server is draining (SIGTERM): tell the client we're going
                // away so it reconnects elsewhere, rather than being severed.
                _ = writer_shutdown.cancelled() => {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: 1001,
                            reason: "server.shutdown".into(),
                        })))
                        .await;
                    return;
                }
                maybe = out_rx.recv() => match maybe {
                    Some(bytes) => {
                        if sink.send(Message::Binary(bytes.into())).await.is_err() {
                            return;
                        }
                    }
                    // Channel closed — likely an ACL revoke. Send 4403.
                    None => {
                        let _ = sink
                            .send(Message::Close(Some(CloseFrame {
                                code: 4403,
                                reason: "acl.revoked".into(),
                            })))
                            .await;
                        return;
                    }
                },
            }
        }
    });

    loop {
        let msg = tokio::select! {
            _ = shutdown.cancelled() => break,
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
                                handle: ConnHandle { tx: out_tx.clone() },
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
    let _ = handle.tx.send(Event::Leave(conn_id)).await;
    let _ = writer.await;
}
