//! Outbound half of a collab WebSocket, shared by the doc and board shims.
//!
//! The room actor hands frames to a connection through an mpsc channel; this
//! task forwards them to the socket until the session ends. It cannot learn
//! that from the channel closing: the read half holds a sender of its own for
//! the whole session (to re-join on a client SyncStep1), so the channel stays
//! open until both halves are gone. The end is signalled explicitly instead.

use axum::extract::ws::{CloseFrame, Message};
use std::time::Duration;

use futures::{Sink, SinkExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Forward room frames to `sink` until the session ends, then cancel `done`
/// so the read half stops too.
///
/// - `shutdown` (server draining): close with 1001 so the client reconnects
///   elsewhere.
/// - `revoked` (the room revoked this connection's access): close with 4403.
/// - `refused` (the room refused an update this connection sent: one that
///   would nest the page too deep, or one that doesn't decode): close with
///   4422, so the client stops sending on a session whose state the room no
///   longer shares.
/// - `room_ended` (the room this session joined has ended: it evicted
///   itself after refusing an update it had applied, which nested the page
///   too deep, from this connection or another one, it was evicted, or its
///   actor stopped any other way): close with 1012 (service restart),
///   so the client reconnects to a fresh room, reloaded from storage, and
///   re-sends what that room lacks in its answer to the server's SyncStep1.
///   Without it the session would stay open on a room that no longer
///   applies, persists or shares anything. A refused sender is closed with
///   4422, which is checked first.
/// - `done` (the read half ended — the client went away): stop, even in the
///   middle of a send that hangs on a vanished client. Without it this task
///   would wait on the still-open channel forever, leaking itself and the
///   socket for every disconnect.
///
/// With `ping_interval`, a ping goes out that often, so the read half hears
/// a pong from a live client however quiet its room is (see
/// [`crate::rate_limit::Liveness`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run<S>(
    mut sink: S,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    shutdown: CancellationToken,
    revoked: CancellationToken,
    refused: CancellationToken,
    room_ended: CancellationToken,
    done: CancellationToken,
    ping_interval: Option<Duration>,
) where
    S: Sink<Message> + Unpin,
{
    let mut ping_timer = ping_interval.map(|every| {
        let mut timer = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer
    });
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
            _ = refused.cancelled() => {
                close(&mut sink, 4422, "doc.update_refused").await;
                break;
            }
            _ = room_ended.cancelled() => {
                close(&mut sink, 1012, "room.restart").await;
                break;
            }
            _ = done.cancelled() => break,
            maybe = out_rx.recv() => match maybe {
                Some(bytes) => {
                    if !send_unless_done(&mut sink, Message::Binary(bytes.into()), &done).await {
                        break;
                    }
                }
                // Every sender is gone; nothing more can arrive.
                None => break,
            },
            _ = next_ping(ping_timer.as_mut()) => {
                if !send_unless_done(&mut sink, Message::Ping(Default::default()), &done).await {
                    break;
                }
            }
        }
    }
    done.cancel();
}

/// Send `message`, giving up when `done` is cancelled first: whether it was
/// sent. A client that vanished without closing (a laptop that slept) never
/// ACKs, so once the kernel's send buffer is full a send hangs until TCP
/// gives up, many minutes later. When the read half has ended meanwhile
/// (it heard nothing for the silence limit, see
/// [`crate::rate_limit::Liveness`]), the session must end now: it holds a
/// place under the per-user socket cap until it does. The read half cancels
/// `done` only once it has stopped, so nothing a live session needs is cut.
async fn send_unless_done<S>(sink: &mut S, message: Message, done: &CancellationToken) -> bool
where
    S: Sink<Message> + Unpin,
{
    tokio::select! {
        biased;
        _ = done.cancelled() => false,
        sent = sink.send(message) => sent.is_ok(),
    }
}

/// Resolves at the timer's next tick, never without a timer.
async fn next_ping(ping_timer: Option<&mut tokio::time::Interval>) {
    match ping_timer {
        Some(timer) => {
            timer.tick().await;
        }
        None => std::future::pending().await,
    }
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

    struct Harness {
        out_tx: mpsc::Sender<Vec<u8>>,
        sent: futures::channel::mpsc::UnboundedReceiver<Message>,
        shutdown: CancellationToken,
        revoked: CancellationToken,
        refused: CancellationToken,
        room_ended: CancellationToken,
        done: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    fn start() -> Harness {
        start_pinging(None)
    }

    fn start_pinging(ping_interval: Option<Duration>) -> Harness {
        let (sink, sent) = futures::channel::mpsc::unbounded::<Message>();
        let (out_tx, out_rx) = mpsc::channel(8);
        let (shutdown, revoked, refused, room_ended, done) = (
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        let task = tokio::spawn(run(
            sink,
            out_rx,
            shutdown.clone(),
            revoked.clone(),
            refused.clone(),
            room_ended.clone(),
            done.clone(),
            ping_interval,
        ));
        Harness {
            out_tx,
            sent,
            shutdown,
            revoked,
            refused,
            room_ended,
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

    /// A socket whose peer vanished without closing: the kernel's send
    /// buffer is full and no ACK ever comes, so a send never completes.
    struct StalledSink;

    impl futures::Sink<Message> for StalledSink {
        type Error = ();

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Pending
        }

        fn start_send(self: std::pin::Pin<&mut Self>, _message: Message) -> Result<(), ()> {
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Pending
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), ()>> {
            std::task::Poll::Pending
        }
    }

    /// The read half gave up on a vanished client (it heard nothing for the
    /// silence limit) while a send to it hangs: the writer must stop at
    /// once, not when TCP gives up many minutes later, since the session,
    /// and its place under the per-user socket cap, lasts until it does.
    #[tokio::test]
    async fn stops_when_the_read_half_ends_during_a_send_that_never_completes() {
        for ping_interval in [None, Some(Duration::from_millis(10))] {
            let (out_tx, out_rx) = mpsc::channel(8);
            let done = CancellationToken::new();
            let task = tokio::spawn(run(
                StalledSink,
                out_rx,
                CancellationToken::new(),
                CancellationToken::new(),
                CancellationToken::new(),
                CancellationToken::new(),
                done.clone(),
                ping_interval,
            ));
            out_tx.send(vec![1, 2, 3]).await.unwrap();
            // Let the writer take the frame (or a ping) and stall on it.
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!task.is_finished(), "the stalled send must be pending");
            done.cancel();
            finished(task).await;
        }
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
    async fn refused_update_closes_with_4422_and_ends_the_session() {
        let mut h = start();
        h.refused.cancel();
        finished(h.task).await;
        let m = h.sent.next().await.expect("no close frame sent");
        assert_eq!(close_code(&m), Some(4422));
        assert!(h.done.is_cancelled(), "the read half must be told to stop");
    }

    /// The room this session joined evicted itself over another connection's
    /// update: close so the client reconnects to a fresh room, instead of
    /// editing on the dead one.
    #[tokio::test]
    async fn room_end_closes_with_1012_and_ends_the_session() {
        let mut h = start();
        h.room_ended.cancel();
        finished(h.task).await;
        let m = h.sent.next().await.expect("no close frame sent");
        assert_eq!(close_code(&m), Some(1012));
        assert!(h.done.is_cancelled(), "the read half must be told to stop");
    }

    /// The sender of the refused update: the room refuses it and ends in
    /// the same turn, and the sender still learns why (4422).
    #[tokio::test]
    async fn a_refused_sender_is_closed_with_4422_although_the_room_ended_too() {
        let mut h = start();
        h.refused.cancel();
        h.room_ended.cancel();
        finished(h.task).await;
        let m = h.sent.next().await.expect("no close frame sent");
        assert_eq!(close_code(&m), Some(4422));
    }

    #[tokio::test(start_paused = true)]
    async fn pings_the_client_every_interval_and_not_at_once() {
        let mut h = start_pinging(Some(Duration::from_secs(30)));
        tokio::time::sleep(Duration::from_secs(29)).await;
        h.out_tx.send(vec![7]).await.unwrap();
        let first = h.sent.next().await.unwrap();
        assert!(
            matches!(first, Message::Binary(_)),
            "no ping before the interval: {first:?}"
        );
        for _ in 0..2 {
            let m = tokio::time::timeout(Duration::from_secs(31), h.sent.next())
                .await
                .expect("no ping within the interval")
                .unwrap();
            assert!(matches!(m, Message::Ping(_)), "{m:?}");
        }
        h.done.cancel();
        finished(h.task).await;
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
