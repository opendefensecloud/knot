//! Outbound half of a collab WebSocket, shared by the doc and board shims.
//!
//! The room actor hands frames to a connection through an mpsc channel; this
//! task forwards them to the socket until the session ends. It cannot learn
//! that from the channel closing: the read half holds a sender of its own for
//! the whole session (to re-join on a client SyncStep1), so the channel stays
//! open until both halves are gone. The end is signalled explicitly instead.

use axum::extract::ws::{CloseFrame, Message};
use futures::{Sink, SinkExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Forward room frames to `sink` until the session ends, then cancel `done`
/// so the read half stops too.
///
/// - `shutdown` (server draining): close with 1001 so the client reconnects
///   elsewhere.
/// - `revoked` (the room revoked this connection's access): close with 4403.
/// - `done` (the read half ended — the client went away): stop. Without it
///   this task would wait on the still-open channel forever, leaking itself
///   and the socket for every disconnect.
pub(crate) async fn run<S>(
    mut sink: S,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    shutdown: CancellationToken,
    revoked: CancellationToken,
    done: CancellationToken,
) where
    S: Sink<Message> + Unpin,
{
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                close(&mut sink, 1001, "server.shutdown").await;
                break;
            }
            _ = revoked.cancelled() => {
                close(&mut sink, 4403, "acl.revoked").await;
                break;
            }
            _ = done.cancelled() => break,
            maybe = out_rx.recv() => match maybe {
                Some(bytes) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                // Every sender is gone; nothing more can arrive.
                None => break,
            },
        }
    }
    done.cancel();
}

async fn close<S>(sink: &mut S, code: u16, reason: &'static str)
where
    S: Sink<Message> + Unpin,
{
    let _ = sink
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Duration;

    struct Harness {
        out_tx: mpsc::Sender<Vec<u8>>,
        sent: futures::channel::mpsc::UnboundedReceiver<Message>,
        shutdown: CancellationToken,
        revoked: CancellationToken,
        done: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    fn start() -> Harness {
        let (sink, sent) = futures::channel::mpsc::unbounded::<Message>();
        let (out_tx, out_rx) = mpsc::channel(8);
        let (shutdown, revoked, done) = (
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        let task = tokio::spawn(run(
            sink,
            out_rx,
            shutdown.clone(),
            revoked.clone(),
            done.clone(),
        ));
        Harness {
            out_tx,
            sent,
            shutdown,
            revoked,
            done,
            task,
        }
    }

    async fn finished(task: tokio::task::JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the writer never stopped")
            .unwrap();
    }

    fn close_code(m: &Message) -> Option<u16> {
        match m {
            Message::Close(Some(CloseFrame { code, .. })) => Some(*code),
            _ => None,
        }
    }

    #[tokio::test]
    async fn forwards_room_frames_as_binary_messages() {
        let mut h = start();
        h.out_tx.send(vec![1, 2, 3]).await.unwrap();
        let m = tokio::time::timeout(Duration::from_secs(5), h.sent.next())
            .await
            .expect("frame never forwarded")
            .unwrap();
        assert!(matches!(m, Message::Binary(b) if b.as_ref() == [1, 2, 3]));
        h.done.cancel();
        finished(h.task).await;
    }

    /// The leak: the read half still holds a sender when the client goes
    /// away, so the channel never closes. The writer must stop anyway — and
    /// say nothing, the socket is gone.
    #[tokio::test]
    async fn stops_when_the_read_half_ends_although_the_channel_is_open() {
        let mut h = start();
        h.done.cancel();
        finished(h.task).await;
        drop(h.out_tx);
        assert!(
            h.sent.next().await.is_none(),
            "nothing may be sent after the client left"
        );
    }

    #[tokio::test]
    async fn revoked_access_closes_with_4403_and_ends_the_session() {
        let mut h = start();
        h.revoked.cancel();
        finished(h.task).await;
        let m = h.sent.next().await.expect("no close frame sent");
        assert_eq!(close_code(&m), Some(4403));
        assert!(h.done.is_cancelled(), "the read half must be told to stop");
    }

    #[tokio::test]
    async fn server_shutdown_closes_with_1001() {
        let mut h = start();
        h.shutdown.cancel();
        finished(h.task).await;
        let m = h.sent.next().await.expect("no close frame sent");
        assert_eq!(close_code(&m), Some(1001));
        assert!(h.done.is_cancelled());
    }
}
