//! Outbound half of a collab WebSocket, shared by the doc and board shims.
//!
//! The room actor hands frames to a connection through an mpsc channel; this
//! task forwards them to the socket until the session ends. It cannot learn
//! that from the channel closing: the read half holds a sender of its own for
//! the whole session (to re-join on a client SyncStep1), so the channel stays
//! open until both halves are gone. The end is signalled explicitly instead.
//!
//! The task also sends the join handshake (Ruling 5): the shim hands it the
//! frames the room's join reply gave (SyncStep2, the rows that park the
//! page's pending content, SyncStep1), and the task sends them before any
//! frame from the room. The room has registered the connection by then and
//! fans out into its channel, which the task reads into a bounded queue of
//! its own while the handshake goes out, so a slow handshake does not get
//! the connection dropped as a slow consumer.

use axum::extract::ws::{CloseFrame, Message};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{Sink, SinkExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The most frame bytes the writer queues from the room while it sends the
/// handshake and then drains that queue.
pub const HANDSHAKE_BUFFER_BYTES: usize = 8 * 1024 * 1024;
/// The most frames it queues meanwhile (presence frames are tiny and
/// frequent, so bytes alone would let hundreds of thousands queue).
pub const HANDSHAKE_BUFFER_FRAMES: usize = 65_536;

/// Room frames queued during a handshake, process-wide.
static HANDSHAKE_FRAMES_QUEUED: AtomicU64 = AtomicU64::new(0);

/// How many room frames writers have queued during a handshake since the
/// process started, for tests that observe a stalled handshake.
#[doc(hidden)]
pub fn handshake_frames_queued() -> u64 {
    HANDSHAKE_FRAMES_QUEUED.load(Ordering::Relaxed)
}

/// Send the `handshake` frames, then forward room frames to `sink` until
/// the session ends, then cancel `done` so the read half stops too.
///
/// The handshake goes out before any room frame, and before the first ping
/// (the timer starts now; a ping due meanwhile goes out right after). Each
/// of its frames is sent under the cancellations below, which interrupt a
/// send that waits on a full socket. Meanwhile room frames are read from
/// `out_rx` into a queue, up to [`HANDSHAKE_BUFFER_BYTES`] and
/// [`HANDSHAKE_BUFFER_FRAMES`] (past either the room's channel fills as it
/// always does); the queue is sent next, in order, still reading
/// `out_rx` into it, and the forwarding below starts once it is empty.
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
    handshake: Vec<Vec<u8>>,
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
    let ends = Ends {
        shutdown: &shutdown,
        revoked: &revoked,
        refused: &refused,
        room_ended: &room_ended,
        done: &done,
    };
    if !send_handshake(&mut sink, handshake, &mut out_rx, &ends).await {
        done.cancel();
        return;
    }
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

/// The signals that end a session, in the order the writer checks them.
struct Ends<'a> {
    shutdown: &'a CancellationToken,
    revoked: &'a CancellationToken,
    refused: &'a CancellationToken,
    room_ended: &'a CancellationToken,
    done: &'a CancellationToken,
}

/// Room frames read from `out_rx` while the handshake goes out.
#[derive(Default)]
struct Queue {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    /// `out_rx` closed: nothing more can arrive.
    closed: bool,
}

impl Queue {
    fn has_room(&self) -> bool {
        !self.closed
            && self.bytes < HANDSHAKE_BUFFER_BYTES
            && self.frames.len() < HANDSHAKE_BUFFER_FRAMES
    }

    fn push(&mut self, frame: Option<Vec<u8>>) {
        match frame {
            Some(frame) => {
                HANDSHAKE_FRAMES_QUEUED.fetch_add(1, Ordering::Relaxed);
                self.bytes += frame.len();
                self.frames.push_back(frame);
            }
            None => self.closed = true,
        }
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        let frame = self.frames.pop_front()?;
        self.bytes -= frame.len();
        Some(frame)
    }
}

/// How one handshake send ended.
enum Sent {
    Yes,
    /// Close the socket with this code and reason, and end.
    Close(u16, &'static str),
    /// End without a word (the client left, or the send failed).
    End,
}

/// Sends the handshake, then the room frames queued meanwhile (Ruling 5).
/// Whether the session goes on to the main loop: not when it ended, nor
/// when `out_rx` closed (the main loop would end on it at once).
async fn send_handshake<S>(
    sink: &mut S,
    handshake: Vec<Vec<u8>>,
    out_rx: &mut mpsc::Receiver<Vec<u8>>,
    ends: &Ends<'_>,
) -> bool
where
    S: Sink<Message> + Unpin,
{
    let mut queue = Queue::default();
    let mut frames = handshake.into_iter();
    loop {
        let Some(frame) = frames.next().or_else(|| queue.pop()) else {
            return !queue.closed;
        };
        match send_queuing(sink, frame, out_rx, &mut queue, ends).await {
            Sent::Yes => {}
            Sent::Close(code, reason) => {
                close(sink, code, reason).await;
                return false;
            }
            Sent::End => return false,
        }
    }
}

/// Sends `frame`, reading room frames into `queue` while the send waits.
/// The send is created once and polled until it completes, so a frame
/// never goes out twice or out of order; a cancellation drops it.
async fn send_queuing<S>(
    sink: &mut S,
    frame: Vec<u8>,
    out_rx: &mut mpsc::Receiver<Vec<u8>>,
    queue: &mut Queue,
    ends: &Ends<'_>,
) -> Sent
where
    S: Sink<Message> + Unpin,
{
    let send = sink.send(Message::Binary(frame.into()));
    tokio::pin!(send);
    loop {
        tokio::select! {
            biased;
            _ = ends.shutdown.cancelled() => return Sent::Close(1001, "server.shutdown"),
            _ = ends.revoked.cancelled() => return Sent::Close(4403, "acl.revoked"),
            _ = ends.refused.cancelled() => return Sent::Close(4422, "doc.update_refused"),
            _ = ends.room_ended.cancelled() => return Sent::Close(1012, "room.restart"),
            _ = ends.done.cancelled() => return Sent::End,
            sent = &mut send => return if sent.is_ok() { Sent::Yes } else { Sent::End },
            frame = out_rx.recv(), if queue.has_room() => queue.push(frame),
        }
    }
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
        sent: futures::stream::BoxStream<'static, Message>,
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
        start_with(vec![], vec![], None, ping_interval)
    }

    /// A writer with `handshake`, whose room channel (8 slots) holds
    /// `queued` before it starts, on an unbounded sink or, with
    /// `sink_capacity`, on a bounded one (`futures`' channel: one message
    /// per sender beyond the capacity, so `Some(0)` takes one message and
    /// then blocks until it is read).
    fn start_with(
        handshake: Vec<Vec<u8>>,
        queued: Vec<Vec<u8>>,
        sink_capacity: Option<usize>,
        ping_interval: Option<Duration>,
    ) -> Harness {
        let (out_tx, out_rx) = mpsc::channel(8);
        for frame in queued {
            out_tx.try_send(frame).expect("the room channel has room");
        }
        let (shutdown, revoked, refused, room_ended, done) = (
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
            CancellationToken::new(),
        );
        let (task, sent) = match sink_capacity {
            None => {
                let (sink, sent) = futures::channel::mpsc::unbounded::<Message>();
                let task = tokio::spawn(run(
                    sink,
                    handshake,
                    out_rx,
                    shutdown.clone(),
                    revoked.clone(),
                    refused.clone(),
                    room_ended.clone(),
                    done.clone(),
                    ping_interval,
                ));
                (task, sent.boxed())
            }
            Some(n) => {
                let (sink, sent) = futures::channel::mpsc::channel::<Message>(n);
                let task = tokio::spawn(run(
                    sink,
                    handshake,
                    out_rx,
                    shutdown.clone(),
                    revoked.clone(),
                    refused.clone(),
                    room_ended.clone(),
                    done.clone(),
                    ping_interval,
                ));
                (task, sent.boxed())
            }
        };
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
                vec![],
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

    /// The next message the sink got, within 5 s.
    async fn next_sent(h: &mut Harness) -> Message {
        tokio::time::timeout(Duration::from_secs(5), h.sent.next())
            .await
            .expect("nothing was sent")
            .expect("the sink ended")
    }

    fn binary(m: &Message) -> Vec<u8> {
        match m {
            Message::Binary(b) => b.to_vec(),
            other => panic!("not a binary message: {other:?}"),
        }
    }

    /// Sends `frames` into the room channel, each within `within`.
    async fn send_all(out_tx: &mpsc::Sender<Vec<u8>>, frames: Vec<Vec<u8>>, within: Duration) {
        for (i, frame) in frames.into_iter().enumerate() {
            tokio::time::timeout(within, out_tx.send(frame))
                .await
                .unwrap_or_else(|_| panic!("room frame {i} was not taken"))
                .unwrap();
        }
    }

    fn numbered(n: usize, from: usize) -> Vec<Vec<u8>> {
        (from..from + n)
            .map(|i| (i as u32).to_be_bytes().to_vec())
            .collect()
    }

    #[tokio::test]
    async fn the_handshake_goes_out_before_queued_room_frames() {
        let (a, b, q) = (vec![1], vec![2], vec![3]);
        let mut h = start_with(vec![a.clone(), b.clone()], vec![q.clone()], None, None);
        for want in [a, b, q] {
            let m = next_sent(&mut h).await;
            assert_eq!(binary(&m), want);
        }
        h.done.cancel();
        finished(h.task).await;
    }

    /// The room registers a connection before its handshake goes out, and
    /// drops one whose channel is full: frames fanned out during a slow
    /// handshake must wait in the writer, not fill the channel.
    #[tokio::test]
    async fn room_frames_queued_during_the_handshake_are_buffered_not_dropped() {
        let (h1, h2) = (vec![0xa1], vec![0xa2]);
        let mut h = start_with(vec![h1.clone(), h2.clone()], vec![], Some(0), None);
        let frames = numbered(300, 0);
        send_all(&h.out_tx, frames.clone(), Duration::from_secs(2)).await;
        for want in [h1, h2].into_iter().chain(frames) {
            let m = next_sent(&mut h).await;
            assert_eq!(binary(&m), want);
        }
        h.done.cancel();
        finished(h.task).await;
    }

    /// Past `HANDSHAKE_BUFFER_BYTES` the writer reads no more: the room's
    /// channel fills, as it does for any slow socket.
    #[tokio::test]
    async fn the_handshake_queue_stops_at_its_byte_bound() {
        let h = start_with(vec![vec![0xa1], vec![0xa2]], vec![], Some(0), None);
        let mib = vec![0u8; 1024 * 1024];
        let mut taken = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::timeout_at(deadline, h.out_tx.send(mib.clone()))
            .await
            .is_ok()
        {
            taken += 1;
        }
        assert_eq!(
            taken,
            HANDSHAKE_BUFFER_BYTES / mib.len() + 8,
            "the queue's bound plus the channel's 8 slots"
        );
        assert!(matches!(
            h.out_tx.try_send(mib),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        h.done.cancel();
        finished(h.task).await;
    }

    #[tokio::test]
    async fn the_handshake_queue_stops_at_its_frame_bound() {
        let h = start_with(vec![vec![0xa1], vec![0xa2]], vec![], Some(0), None);
        let mut taken = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::timeout_at(deadline, h.out_tx.send(vec![7]))
            .await
            .is_ok()
        {
            taken += 1;
        }
        assert_eq!(taken, HANDSHAKE_BUFFER_FRAMES + 8);
        assert!(matches!(
            h.out_tx.try_send(vec![7]),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        h.done.cancel();
        finished(h.task).await;
    }

    /// While the queue drains on the same slow socket, frames keep arriving
    /// from the room: they join the queue's end, none lost or sent twice.
    #[tokio::test]
    async fn frames_that_arrive_while_the_queue_drains_keep_their_place() {
        let (h1, h2) = (vec![0xa1], vec![0xa2]);
        let mut h = start_with(vec![h1.clone(), h2.clone()], vec![], Some(0), None);
        let early = numbered(100, 0);
        send_all(&h.out_tx, early.clone(), Duration::from_secs(2)).await;
        let late = numbered(300, 100);
        let feeder = {
            let (out_tx, late) = (h.out_tx.clone(), late.clone());
            tokio::spawn(async move { send_all(&out_tx, late, Duration::from_secs(2)).await })
        };
        let mut got = Vec::new();
        for _ in 0..402 {
            got.push(binary(&next_sent(&mut h).await));
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        feeder.await.unwrap();
        let want: Vec<Vec<u8>> = [h1, h2].into_iter().chain(early).chain(late).collect();
        assert_eq!(got, want);
        h.done.cancel();
        finished(h.task).await;
    }

    /// A revoked joiner of a page with many parked rows is closed at once,
    /// not after its whole handshake went out on a slow socket.
    #[tokio::test]
    async fn revoking_during_the_handshake_closes_with_4403_and_sends_no_more_of_it() {
        let frames: Vec<Vec<u8>> = (1..=5).map(|i| vec![i]).collect();
        let mut h = start_with(frames.clone(), vec![], Some(0), None);
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.revoked.cancel();
        assert_eq!(binary(&next_sent(&mut h).await), frames[0]);
        assert_eq!(close_code(&next_sent(&mut h).await), Some(4403));
        assert!(
            tokio::time::timeout(Duration::from_secs(5), h.sent.next())
                .await
                .expect("the sink did not end")
                .is_none(),
            "nothing more may be sent"
        );
        finished(h.task).await;
        assert!(h.done.is_cancelled(), "the read half must be told to stop");
    }

    #[tokio::test]
    async fn a_room_end_during_the_handshake_closes_with_1012() {
        let frames: Vec<Vec<u8>> = (1..=5).map(|i| vec![i]).collect();
        let mut h = start_with(frames.clone(), vec![], Some(0), None);
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.room_ended.cancel();
        assert_eq!(binary(&next_sent(&mut h).await), frames[0]);
        assert_eq!(close_code(&next_sent(&mut h).await), Some(1012));
        assert!(
            tokio::time::timeout(Duration::from_secs(5), h.sent.next())
                .await
                .expect("the sink did not end")
                .is_none(),
            "nothing more may be sent"
        );
        finished(h.task).await;
        assert!(h.done.is_cancelled(), "the read half must be told to stop");
    }
}
