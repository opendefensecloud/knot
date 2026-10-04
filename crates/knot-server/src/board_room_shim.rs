//! WebSocket → BoardRoom shim. Mirrors `room::serve` but against the
//! `BoardRooms` registry, since boards have their own y-protocol session.

use axum::extract::ws::{Message, WebSocket};
use futures::StreamExt;
use knot_crdt::board_room::{ConnHandle, ConnId, Event, InMsg};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::protocol::{YSyncMessage, decode, encode_sync_step1, encode_sync_step2};

pub async fn serve(
    rooms: Arc<knot_crdt::BoardRooms>,
    board_id: Uuid,
    socket: WebSocket,
    shutdown: CancellationToken,
) {
    let handle = match rooms.acquire(board_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, %board_id, "board room acquire failed; closing socket");
            return;
        }
    };
    let conn_id: ConnId = Uuid::new_v4();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(256);

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
    // Ask the client for what we lack, exactly as `room::serve` does: the
    // board provider has the same offline gap (edits made while the socket is
    // down are applied locally but never sent) and the same SyncStep1 answer.
    // Sent as its own WS message — BoardProvider decodes one y-protocol
    // message per frame.
    let _ = out_tx.send(encode_sync_step1(&joined.state_vector)).await;

    let (sink, mut stream) = socket.split();
    // As in `room::serve`: whichever half ends the session cancels `done` so
    // the other stops. Boards have no ACL revocation of their own, so the
    // writer's revoke signal is never raised here.
    let done = CancellationToken::new();
    let writer = tokio::spawn(crate::ws_writer::run(
        sink,
        out_rx,
        shutdown.clone(),
        CancellationToken::new(),
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
            },
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
