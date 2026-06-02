//! WebSocket → Room shim. Auth happens at upgrade in lib.rs's
//! `collab_upgrade`; this shim just plumbs an authenticated socket into
//! the knot-crdt Rooms registry.

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use knot_crdt::{ConnHandle, ConnId, Event, InMsg};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::protocol::{YSyncMessage, decode, encode_sync_step2};

pub async fn serve(rooms: Arc<knot_crdt::Rooms>, doc_id: Uuid, socket: WebSocket) {
    let handle = rooms.acquire(doc_id).await;
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
    let initial = match reply_rx.await {
        Ok(Ok(b)) => encode_sync_step2(&b),
        _ => return,
    };
    let _ = out_tx.send(initial).await;

    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if sink.send(Message::Binary(bytes)).await.is_err() {
                return;
            }
        }
        // Channel closed — likely an ACL revoke. Send 4403.
        let _ = sink
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 4403,
                reason: "acl.revoked".into(),
            })))
            .await;
    });

    while let Some(Ok(msg)) = stream.next().await {
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
                        if let Ok(Ok(state)) = rrx.await {
                            let _ = out_tx.send(encode_sync_step2(&state)).await;
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
                    Ok(YSyncMessage::Awareness) => {
                        let _ = handle
                            .tx
                            .send(Event::AwarenessIn {
                                from: conn_id,
                                payload: bytes,
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
