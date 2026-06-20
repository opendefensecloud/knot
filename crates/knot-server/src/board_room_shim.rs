//! WebSocket → BoardRoom shim. Mirrors `room::serve` but against the
//! `BoardRooms` registry, since boards have their own y-protocol session.

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use knot_crdt::board_room::{ConnHandle, ConnId, Event, InMsg};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::protocol::{YSyncMessage, decode, encode_sync_step2};

pub async fn serve(
    rooms: Arc<knot_crdt::BoardRooms>,
    board_id: Uuid,
    socket: WebSocket,
    shutdown: CancellationToken,
) {
    let handle = rooms.acquire(board_id).await;
    let conn_id: ConnId = Uuid::new_v4();
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);

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
    let writer_shutdown = shutdown.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
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
                        if sink.send(Message::Binary(bytes)).await.is_err() {
                            return;
                        }
                    }
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
            Message::Binary(bytes) => match decode(&bytes) {
                Ok(YSyncMessage::SyncStep1(_sv)) => {
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
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    let _ = handle.tx.send(Event::Leave(conn_id)).await;
    let _ = writer.await;
}
