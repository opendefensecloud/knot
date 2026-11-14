//! Collab WebSocket limits (spec "Collab WebSocket"): per-user byte and
//! message budgets enforced by backpressure, presence coalescing, one
//! answered `SyncStep1` per connection, the open-socket cap and the upgrade
//! bucket.
//!
//! Nothing here drops or refuses an edit. [`CollabLimits::charge_frame`]
//! charges every inbound frame after it was read (tokens may go negative),
//! and [`ConnectionThrottle::wait_for_budget`] waits off the debt before the
//! next read, so a frame larger than what is left still passes and the
//! socket simply reads more slowly. The writer is its own task, so the
//! client keeps receiving during a wait, and a revoke, refusal, room end or
//! shutdown ends the wait at once.
//!
//! Budgets are per user and per kind ([`CollabKind`]): doc and board
//! sockets have separate budgets, so a big board drag never slows typing.
//! The disabled form ([`CollabLimits::disabled`]) passes everything: the
//! read loops then behave exactly as they did before limits existed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::ws::Message;
use futures::{FutureExt, Stream, StreamExt};
use knot_config::Config;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use yrs::encoding::read::{Cursor, Read};
use yrs::sync::awareness::AwarenessUpdate;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;

use super::RateLimits;
use super::bucket::{KeyedBuckets, Rate, retry_after_secs};
use crate::protocol::{YSyncMessage, decode};

const MIB: f64 = 1024.0 * 1024.0;
const KIB: f64 = 1024.0;

/// The lowest a scaled byte budget goes: a burst of one max-size frame
/// (4 MiB, the upgrade's `max_message_size`) and 64 KiB/s, so no scale can
/// make a wait last days.
pub const BYTE_FLOOR: Rate = Rate {
    burst: 4.0 * MIB,
    per_second: 64.0 * KIB,
};
/// The lowest a scaled message budget goes: one token, 20 per second.
pub const MESSAGE_FLOOR: Rate = Rate {
    burst: 1.0,
    per_second: 20.0,
};
/// `Retry-After` of an upgrade refused for too many open sockets: no bucket
/// says when one closes.
pub const SOCKET_CAP_RETRY_AFTER: Duration = Duration::from_secs(5);
/// Longest single sleep of a budget wait before it re-checks the debt
/// (another socket of the same user may have added to it).
const MAX_WAIT_SLICE: Duration = Duration::from_secs(1);
/// Shortest delay before a held presence frame is retried, so float rounding
/// can never make the retry spin.
const MIN_AWARENESS_RETRY: Duration = Duration::from_millis(1);

/// Which collab socket a budget belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CollabKind {
    /// A page's text (`/collab/doc/{doc_id}`).
    Doc,
    /// A board (`/collab/board/{board_id}`).
    Board,
}

impl CollabKind {
    pub const ALL: [CollabKind; 2] = [CollabKind::Doc, CollabKind::Board];

    fn index(self) -> usize {
        self as usize
    }

    /// Bytes per user, every inbound frame: doc 32 MiB, 1 MiB/s; board
    /// 64 MiB, 4 MiB/s (before `rate_limit_scale`).
    pub fn byte_rate(self) -> Rate {
        match self {
            CollabKind::Doc => Rate::per_second(32.0 * MIB, MIB),
            CollabKind::Board => Rate::per_second(64.0 * MIB, 4.0 * MIB),
        }
    }

    /// Messages per user, every non-awareness frame: doc 1 000, 300/s;
    /// board 2 000, 600/s (before `rate_limit_scale`).
    pub fn message_rate(self) -> Rate {
        match self {
            CollabKind::Doc => Rate::per_second(1000.0, 300.0),
            CollabKind::Board => Rate::per_second(2000.0, 600.0),
        }
    }
}

/// Presence frames per user and kind: 240, 120/s (before scale).
pub fn awareness_rate() -> Rate {
    Rate::per_second(240.0, 120.0)
}

/// Upgrades per user and target doc or board: 20, 20/min (before scale).
pub fn upgrade_rate() -> Rate {
    Rate::per_minute(20, 20)
}

/// The scaled budgets of one [`CollabKind`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KindRates {
    pub bytes: Rate,
    pub messages: Rate,
    pub awareness: Rate,
}

/// What a read loop waits off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebtKind {
    Bytes,
    Messages,
}

impl DebtKind {
    /// The `kind` label of `knot_collab_throttled_total`.
    fn label(self) -> &'static str {
        match self {
            DebtKind::Bytes => "bytes",
            DebtKind::Messages => "messages",
        }
    }
}

/// One kind's per-user buckets.
struct KindBudgets {
    bytes: KeyedBuckets<Uuid>,
    messages: KeyedBuckets<Uuid>,
    awareness: KeyedBuckets<Uuid>,
}

/// The buckets of enabled limits.
struct Budgets {
    /// By [`CollabKind::index`].
    by_kind: [KindBudgets; 2],
    /// Per (user, kind, target doc or board).
    upgrades: KeyedBuckets<(Uuid, CollabKind, Uuid)>,
    open_sockets: Arc<OpenSockets>,
}

/// The collab WebSocket limits of one replica.
pub struct CollabLimits {
    /// `None` when off.
    budgets: Option<Budgets>,
}

impl Default for CollabLimits {
    fn default() -> Self {
        Self::disabled()
    }
}

impl std::fmt::Debug for CollabLimits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CollabLimits")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl CollabLimits {
    /// The limits `config` asks for: every budget × `rate_limit_scale`
    /// (never below its floor), at most `rate_limit_collab_max_sockets`
    /// open sockets per user. Off when `rate_limit_enabled` is false.
    pub fn from_config(config: &Config) -> Self {
        if !config.rate_limit_enabled {
            return Self::disabled();
        }
        let max_sockets =
            usize::try_from(config.rate_limit_collab_max_sockets).unwrap_or(usize::MAX);
        Self::new(super::effective_scale(config), max_sockets)
    }

    /// Enabled limits at `scale`, with at most `max_sockets` (at least one)
    /// open sockets per user. The socket cap is not scaled.
    pub fn new(scale: f64, max_sockets: usize) -> Self {
        let kind_budgets = |kind: CollabKind| KindBudgets {
            bytes: KeyedBuckets::new(kind.byte_rate().scaled(scale, BYTE_FLOOR)),
            messages: KeyedBuckets::new(kind.message_rate().scaled(scale, MESSAGE_FLOOR)),
            awareness: KeyedBuckets::new(awareness_rate().scaled(scale, Rate::MINIMUM)),
        };
        Self {
            budgets: Some(Budgets {
                by_kind: CollabKind::ALL.map(kind_budgets),
                upgrades: KeyedBuckets::new(upgrade_rate().scaled(scale, Rate::MINIMUM)),
                open_sockets: Arc::new(OpenSockets::new(max_sockets.max(1))),
            }),
        }
    }

    /// No collab limit at all.
    pub fn disabled() -> Self {
        Self { budgets: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.budgets.is_some()
    }

    /// `kind`'s scaled budgets; `None` when off.
    pub fn rates(&self, kind: CollabKind) -> Option<KindRates> {
        let kind_budgets = self.kind_budgets(kind)?;
        Some(KindRates {
            bytes: kind_budgets.bytes.rate(),
            messages: kind_budgets.messages.rate(),
            awareness: kind_budgets.awareness.rate(),
        })
    }

    fn kind_budgets(&self, kind: CollabKind) -> Option<&KindBudgets> {
        self.budgets
            .as_ref()
            .map(|budgets| &budgets.by_kind[kind.index()])
    }

    /// Admit an upgrade of `user_id` to `target_id` (a doc or a board): the
    /// open-socket cap per user, doc and board sockets together, and the
    /// upgrade bucket per user and target (it catches a socket reconnecting
    /// to one doc in a loop, never skimming the tree). The guard
    /// holds the socket's place until it is dropped, with the session. A
    /// refusal says how long to wait and counts
    /// `knot_rate_limited_total{bucket="collab_upgrade"|"collab_sockets"}`.
    pub fn open_socket(
        &self,
        user_id: Uuid,
        kind: CollabKind,
        target_id: Uuid,
        now: Instant,
    ) -> Result<SocketGuard, Duration> {
        let Some(budgets) = self.budgets.as_ref() else {
            return Ok(SocketGuard::unlimited());
        };
        // The cap first: an upgrade refused for it takes no upgrade token,
        // and one refused by the upgrade bucket holds no place (the guard is
        // dropped).
        let guard = match budgets.open_sockets.try_open(user_id) {
            Ok(()) => SocketGuard {
                open_sockets: Some(budgets.open_sockets.clone()),
                user_id,
            },
            Err(newly_limited) => {
                metrics::counter!("knot_rate_limited_total", "bucket" => "collab_sockets")
                    .increment(1);
                if newly_limited {
                    tracing::warn!(
                        user = %user_id,
                        max_sockets = budgets.open_sockets.max_sockets,
                        "too many open collab sockets for one user; refusing upgrades with 429"
                    );
                }
                return Err(SOCKET_CAP_RETRY_AFTER);
            }
        };
        if let Err(refusal) =
            budgets
                .upgrades
                .try_take_noting_start(&(user_id, kind, target_id), 1.0, now)
        {
            metrics::counter!("knot_rate_limited_total", "bucket" => "collab_upgrade").increment(1);
            if refusal.newly_limited {
                tracing::warn!(
                    user = %user_id,
                    %target_id,
                    retry_after_secs = retry_after_secs(refusal.wait),
                    "collab socket reconnecting to one target too fast; refusing upgrades with 429"
                );
            }
            return Err(refusal.wait);
        }
        Ok(guard)
    }

    /// Charge one inbound frame of `frame_bytes` to `user_id`'s `kind`
    /// budgets: its bytes always, one message unless it is presence
    /// (awareness has a budget of its own). Tokens may go negative; the
    /// next read waits the debt off.
    pub fn charge_frame(
        &self,
        user_id: Uuid,
        kind: CollabKind,
        frame_bytes: usize,
        is_awareness: bool,
        now: Instant,
    ) {
        let Some(kind_budgets) = self.kind_budgets(kind) else {
            return;
        };
        kind_budgets.bytes.charge(&user_id, frame_bytes as f64, now);
        if !is_awareness {
            kind_budgets.messages.charge(&user_id, 1.0, now);
        }
    }

    /// How long until `user_id`'s `kind` budgets are out of debt (the longer
    /// of bytes and messages); zero when they aren't.
    pub fn deficit(&self, user_id: Uuid, kind: CollabKind, now: Instant) -> Duration {
        self.debt(user_id, kind, now)
            .map_or(Duration::ZERO, |(wait, _)| wait)
    }

    /// [`CollabLimits::deficit`] with the budget it comes from; `None` when
    /// there is no debt.
    fn debt(&self, user_id: Uuid, kind: CollabKind, now: Instant) -> Option<(Duration, DebtKind)> {
        let kind_budgets = self.kind_budgets(kind)?;
        let byte_wait = kind_budgets.bytes.deficit(&user_id, now);
        let message_wait = kind_budgets.messages.deficit(&user_id, now);
        let (wait, debt_kind) = if byte_wait >= message_wait {
            (byte_wait, DebtKind::Bytes)
        } else {
            (message_wait, DebtKind::Messages)
        };
        (!wait.is_zero()).then_some((wait, debt_kind))
    }

    /// Take a presence token of `user_id`'s `kind`, or say how long until
    /// one is back.
    pub fn try_take_awareness(
        &self,
        user_id: Uuid,
        kind: CollabKind,
        now: Instant,
    ) -> Result<(), Duration> {
        match self.kind_budgets(kind) {
            Some(kind_budgets) => kind_budgets.awareness.try_take(&user_id, 1.0, now),
            None => Ok(()),
        }
    }
}

/// Open collab sockets per user on this replica. An entry exists only while
/// the user has a socket open (or was just refused one), so the map is
/// bounded by the open sockets.
struct OpenSockets {
    max_sockets: usize,
    by_user: Mutex<HashMap<Uuid, SocketCount>>,
}

#[derive(Default)]
struct SocketCount {
    open: usize,
    /// An upgrade was refused at the cap since the count last went down
    /// (log once). The count goes down when a session's socket closes, and
    /// also when an upgrade the upgrade bucket refused gives back the place
    /// it had taken: either way the user is below the cap again, so the
    /// next refusal at the cap is logged anew.
    refusing: bool,
}

impl OpenSockets {
    fn new(max_sockets: usize) -> Self {
        Self {
            max_sockets,
            by_user: Mutex::new(HashMap::new()),
        }
    }

    fn lock_by_user(&self) -> MutexGuard<'_, HashMap<Uuid, SocketCount>> {
        // A panic elsewhere can't leave a count half-written; keep using it.
        self.by_user
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count one more socket of `user_id`, or `Err(newly_limited)` at the
    /// cap (an entry at the cap always has open sockets, so a refusal adds
    /// no key).
    fn try_open(&self, user_id: Uuid) -> Result<(), bool> {
        let mut by_user = self.lock_by_user();
        let count = by_user.entry(user_id).or_default();
        if count.open >= self.max_sockets {
            let newly_limited = !count.refusing;
            count.refusing = true;
            return Err(newly_limited);
        }
        count.open += 1;
        Ok(())
    }

    /// Give back one of `user_id`'s places: a session's socket closed, or an
    /// upgrade refused after `try_open` dropped its guard. Clears `refusing`.
    fn close(&self, user_id: Uuid) {
        let mut by_user = self.lock_by_user();
        if let Some(count) = by_user.get_mut(&user_id) {
            count.open = count.open.saturating_sub(1);
            count.refusing = false;
            if count.open == 0 {
                by_user.remove(&user_id);
            }
        }
    }

    #[cfg(test)]
    fn open_count(&self, user_id: Uuid) -> usize {
        self.lock_by_user()
            .get(&user_id)
            .map_or(0, |count| count.open)
    }
}

/// One open collab socket's place under the per-user cap; dropping it (with
/// the session) frees the place.
#[must_use = "the socket's place is freed when the guard is dropped"]
pub struct SocketGuard {
    /// `None` when limits are off.
    open_sockets: Option<Arc<OpenSockets>>,
    user_id: Uuid,
}

impl SocketGuard {
    fn unlimited() -> Self {
        Self {
            open_sockets: None,
            user_id: Uuid::nil(),
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Some(open_sockets) = self.open_sockets.take() {
            open_sockets.close(self.user_id);
        }
    }
}

/// The tokens that end a collab session, all watched during a budget wait.
pub struct SessionSignals<'session> {
    /// Server draining.
    pub shutdown: &'session CancellationToken,
    /// The room revoked this connection's access.
    pub revoked: &'session CancellationToken,
    /// The room refused an update this connection sent.
    pub refused: &'session CancellationToken,
    /// The room this session joined has ended.
    pub room_ended: &'session CancellationToken,
    /// Either half of the session ended.
    pub done: &'session CancellationToken,
}

impl SessionSignals<'_> {
    /// The session ended only because the client is gone: the writer's send
    /// failed and cancelled `done`, with no revoke, refusal, room end or
    /// shutdown.
    pub fn client_gone(&self) -> bool {
        self.done.is_cancelled()
            && !self.shutdown.is_cancelled()
            && !self.revoked.is_cancelled()
            && !self.refused.is_cancelled()
            && !self.room_ended.is_cancelled()
    }

    /// Resolves as soon as any of the tokens is cancelled.
    async fn any_cancelled(&self) {
        tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {}
            _ = self.revoked.cancelled() => {}
            _ = self.refused.cancelled() => {}
            _ = self.room_ended.cancelled() => {}
            _ = self.done.cancelled() => {}
        }
    }
}

/// How a budget wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Read the next frame.
    Continue,
    /// The session is ending: stop reading.
    Stop,
}

/// One connection's share of the collab limits, used by its read loop
/// (`room::serve`, `board_room_shim::serve`): the budget wait before every
/// read, the charge of every frame read, the presence coalescer and the
/// one-`SyncStep1` rule. With limits off every method passes at once.
pub struct ConnectionThrottle {
    limits: Arc<RateLimits>,
    user_id: Uuid,
    kind: CollabKind,
    /// The newest presence frame held back for want of a token.
    pending_awareness: Option<Vec<u8>>,
    /// When a token for `pending_awareness` should be back.
    pending_awareness_due: Option<Instant>,
    /// The client's first `SyncStep1` was answered.
    sync_step1_answered: bool,
}

impl ConnectionThrottle {
    pub fn new(limits: Arc<RateLimits>, user_id: Uuid, kind: CollabKind) -> Self {
        Self {
            limits,
            user_id,
            kind,
            pending_awareness: None,
            pending_awareness_due: None,
            sync_step1_answered: false,
        }
    }

    fn collab(&self) -> &CollabLimits {
        &self.limits.collab
    }

    /// Before every read: wait until this user's budgets of this kind are
    /// out of debt, re-checking at least every second (other sockets of the
    /// same user add debt too). Any session signal ends the wait at once
    /// with [`WaitOutcome::Stop`]. Returns at once, without yielding, when
    /// there is no debt or limits are off. One wait counts once in
    /// `knot_collab_throttled_total{kind="bytes"|"messages",action="delayed"}`
    /// and `knot_collab_throttle_wait_seconds`.
    pub async fn wait_for_budget(&self, signals: &SessionSignals<'_>) -> WaitOutcome {
        let mut wait_started: Option<(Instant, DebtKind)> = None;
        let outcome = loop {
            let now = Instant::now();
            let Some((deficit, debt_kind)) = self.collab().debt(self.user_id, self.kind, now)
            else {
                break WaitOutcome::Continue;
            };
            if wait_started.is_none() {
                tracing::debug!(
                    user = %self.user_id,
                    kind = ?self.kind,
                    debt = debt_kind.label(),
                    wait_ms = deficit.as_millis(),
                    "collab socket over its budget; waiting before the next read"
                );
                wait_started = Some((now, debt_kind));
            }
            tokio::select! {
                biased;
                _ = signals.any_cancelled() => break WaitOutcome::Stop,
                _ = tokio::time::sleep(deficit.min(MAX_WAIT_SLICE)) => {}
            }
        };
        if let Some((started_at, debt_kind)) = wait_started {
            metrics::counter!(
                "knot_collab_throttled_total",
                "kind" => debt_kind.label(),
                "action" => "delayed"
            )
            .increment(1);
            metrics::histogram!("knot_collab_throttle_wait_seconds")
                .record(started_at.elapsed().as_secs_f64());
        }
        outcome
    }

    /// The edits (`SyncStep2` and `Update` payloads) among the frames
    /// `stream` holds readable right now, read without waiting and without a
    /// budget wait (each frame is still charged). For a session whose client
    /// is gone ([`SessionSignals::client_gone`]): a budget wait leaves the
    /// frames the client already sent unread in the socket's receive buffer,
    /// and the client may have shown them as saved, so they are forwarded
    /// rather than dropped. The socket is broken, so nothing new can arrive
    /// and the drain ends at the first frame not yet readable.
    pub fn drain_readable_updates<S>(&self, stream: &mut S) -> Vec<Vec<u8>>
    where
        S: Stream<Item = Result<Message, axum::Error>> + Unpin,
    {
        let mut updates = Vec::new();
        while let Some(Some(Ok(message))) = stream.next().now_or_never() {
            self.charge_inbound(&message);
            match message {
                Message::Binary(bytes) => {
                    if let Ok(YSyncMessage::SyncStep2(update) | YSyncMessage::Update(update)) =
                        decode(&bytes)
                    {
                        updates.push(update);
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        updates
    }

    /// Charge a frame just read, before it is decoded: its payload bytes
    /// (any type, any role), and one message unless it is a presence frame
    /// (y-protocol type 1).
    pub fn charge_inbound(&self, message: &Message) {
        if !self.collab().is_enabled() {
            return;
        }
        let (frame_bytes, is_awareness) = match message {
            Message::Binary(bytes) => (
                bytes.len(),
                bytes.first() == Some(&crate::protocol::MSG_AWARENESS),
            ),
            Message::Text(text) => (text.len(), false),
            Message::Ping(bytes) | Message::Pong(bytes) => (bytes.len(), false),
            Message::Close(_) => (0, false),
        };
        self.collab().charge_frame(
            self.user_id,
            self.kind,
            frame_bytes,
            is_awareness,
            Instant::now(),
        );
    }

    /// A presence frame read: `Some(payload)` to forward now, or `None` when
    /// it is held until a token is back. A newer frame is merged into a held
    /// one ([`merge_awareness_frames`]): per awareness client only the
    /// newest state matters, and it always arrives.
    pub fn admit_awareness(&mut self, payload: Vec<u8>) -> Option<Vec<u8>> {
        if let Some(held) = self.pending_awareness.take() {
            self.pending_awareness = Some(merge_awareness_frames(held, payload));
            metrics::counter!(
                "knot_collab_throttled_total",
                "kind" => "awareness",
                "action" => "coalesced"
            )
            .increment(1);
            return None;
        }
        let now = Instant::now();
        match self
            .collab()
            .try_take_awareness(self.user_id, self.kind, now)
        {
            Ok(()) => Some(payload),
            Err(wait) => {
                self.pending_awareness = Some(payload);
                self.pending_awareness_due = Some(now + wait.max(MIN_AWARENESS_RETRY));
                None
            }
        }
    }

    /// When the held presence frame should be retried; `None` when none is
    /// held.
    pub fn pending_awareness_due(&self) -> Option<Instant> {
        self.pending_awareness
            .as_ref()
            .and(self.pending_awareness_due)
    }

    /// At [`ConnectionThrottle::pending_awareness_due`]: the held frame to
    /// forward now, or `None` when there is none or its token isn't back yet
    /// (another socket of the user took it; the due time moves on).
    pub fn take_due_awareness(&mut self) -> Option<Vec<u8>> {
        self.pending_awareness.as_ref()?;
        let now = Instant::now();
        match self
            .collab()
            .try_take_awareness(self.user_id, self.kind, now)
        {
            Ok(()) => {
                self.pending_awareness_due = None;
                self.pending_awareness.take()
            }
            Err(wait) => {
                self.pending_awareness_due = Some(now + wait.max(MIN_AWARENESS_RETRY));
                None
            }
        }
    }

    /// A client `SyncStep1` read: whether to answer it. The first one of a
    /// connection is; with limits on, later ones are ignored (the server
    /// sent the full state on join, an honest client sends one per
    /// connection, and each answer costs a full-state encode in the room
    /// actor). With limits off, every one is answered, as before.
    pub fn admit_sync_step1(&mut self) -> bool {
        if !self.sync_step1_answered || !self.collab().is_enabled() {
            self.sync_step1_answered = true;
            return true;
        }
        metrics::counter!(
            "knot_collab_throttled_total",
            "kind" => "sync_step1",
            "action" => "ignored"
        )
        .increment(1);
        false
    }
}

/// A held presence frame and a newer one from the same socket, as one frame:
/// per awareness client, the entry with the higher clock (the newer frame's
/// on a tie, as y-protocols applies them). A socket's frames don't all
/// describe the same clients: a provider also relays the removals its
/// awareness times out for other clients, so replacing the held frame
/// outright could drop the client's own newest cursor for good. When
/// either frame doesn't decode as a y-protocols awareness update, the newer
/// one replaces the held one.
pub fn merge_awareness_frames(held: Vec<u8>, newer: Vec<u8>) -> Vec<u8> {
    let (Some(mut merged), Some(newer_update)) = (
        decode_awareness_frame(&held),
        decode_awareness_frame(&newer),
    ) else {
        return newer;
    };
    for (client_id, newer_entry) in newer_update.clients {
        let newer_wins = merged
            .clients
            .get(&client_id)
            .is_none_or(|held_entry| newer_entry.clock >= held_entry.clock);
        if newer_wins {
            merged.clients.insert(client_id, newer_entry);
        }
    }
    let update_bytes = merged.encode_v1();
    let mut frame = vec![crate::protocol::MSG_AWARENESS];
    crate::protocol::append_var_uint(&mut frame, update_bytes.len() as u64);
    frame.extend_from_slice(&update_bytes);
    frame
}

/// The awareness update a presence frame (type byte, then the update as a
/// length-prefixed buffer) carries; `None` when it doesn't decode.
fn decode_awareness_frame(frame: &[u8]) -> Option<AwarenessUpdate> {
    let (&message_type, rest) = frame.split_first()?;
    if message_type != crate::protocol::MSG_AWARENESS {
        return None;
    }
    let mut cursor = Cursor::new(rest);
    let update_bytes = cursor.read_buf().ok()?;
    AwarenessUpdate::decode_v1(update_bytes).ok()
}

/// How a collab session notices a client that vanished without closing its
/// socket (a laptop that slept, a network change): the half-open socket
/// would otherwise stay a session, holding one of the user's
/// `collabMaxSockets` places, until a proxy timed it out or TCP gave up.
/// WebSocket pings cross proxies, so this works behind an ingress too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    /// The writer pings the client this often (browsers answer by
    /// themselves, without the page's code).
    pub ping_interval: Duration,
    /// The read loop ends the session when it has read nothing at all from
    /// the client (no frame, no pong) for this long. Time spent in a budget
    /// wait doesn't count: the loop reads nothing then.
    pub silence_limit: Duration,
}

impl Liveness {
    /// A ping every 30 s; silent for 75 s (two missed pongs and then some)
    /// ends the session.
    pub const DEFAULT: Liveness = Liveness {
        ping_interval: Duration::from_secs(30),
        silence_limit: Duration::from_secs(75),
    };
}

/// When a read loop that last heard from its client at `heard_at` ends the
/// session for silence; `None` (never) without [`Liveness`].
pub fn silence_deadline(liveness: Option<Liveness>, heard_at: Instant) -> Option<Instant> {
    liveness.map(|liveness| heard_at + liveness.silence_limit)
}

/// Resolves at `due`, never when it is `None` (a `select!` arm that is off).
pub async fn sleep_until_due(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_limit::RateLimits;

    #[track_caller]
    fn assert_close(actual: Duration, expected: Duration) {
        let difference = actual.abs_diff(expected);
        assert!(
            difference <= Duration::from_millis(1),
            "expected {expected:?}, got {actual:?}"
        );
    }

    fn enabled_limits() -> CollabLimits {
        CollabLimits::new(1.0, 64)
    }

    // -- CollabLimits arithmetic --------------------------------------------

    #[test]
    fn budgets_follow_the_spec_table() {
        let limits = enabled_limits();
        assert_eq!(
            limits.rates(CollabKind::Doc),
            Some(KindRates {
                bytes: Rate::per_second(32.0 * MIB, MIB),
                messages: Rate::per_second(1000.0, 300.0),
                awareness: Rate::per_second(240.0, 120.0),
            })
        );
        assert_eq!(
            limits.rates(CollabKind::Board),
            Some(KindRates {
                bytes: Rate::per_second(64.0 * MIB, 4.0 * MIB),
                messages: Rate::per_second(2000.0, 600.0),
                awareness: Rate::per_second(240.0, 120.0),
            })
        );
        assert_eq!(upgrade_rate(), Rate::per_minute(20, 20));
    }

    #[test]
    fn scale_multiplies_the_budgets() {
        let limits = CollabLimits::new(2.0, 64);
        let doc_rates = limits.rates(CollabKind::Doc).unwrap();
        assert_eq!(doc_rates.bytes, Rate::per_second(64.0 * MIB, 2.0 * MIB));
        assert_eq!(doc_rates.messages, Rate::per_second(2000.0, 600.0));
        assert_eq!(doc_rates.awareness, Rate::per_second(480.0, 240.0));
    }

    #[test]
    fn scale_never_goes_below_the_ws_floors() {
        let limits = CollabLimits::new(0.0001, 64);
        for kind in CollabKind::ALL {
            let rates = limits.rates(kind).unwrap();
            assert_eq!(rates.bytes, BYTE_FLOOR, "{kind:?}");
            assert_eq!(rates.messages, MESSAGE_FLOOR, "{kind:?}");
            assert_eq!(rates.awareness, Rate::MINIMUM, "{kind:?}");
        }
        assert_eq!(BYTE_FLOOR.burst, 4.0 * MIB, "one max-size frame");
        assert_eq!(BYTE_FLOOR.per_second, 64.0 * KIB);
        assert_eq!(MESSAGE_FLOOR.per_second, 20.0);
    }

    #[test]
    fn from_config_follows_enabled_scale_and_socket_cap() {
        let disabled = CollabLimits::from_config(&Config {
            rate_limit_enabled: false,
            ..Default::default()
        });
        assert!(!disabled.is_enabled());
        let scaled = CollabLimits::from_config(&Config {
            rate_limit_scale: 0.5,
            rate_limit_collab_max_sockets: 2,
            ..Default::default()
        });
        assert_eq!(
            scaled.rates(CollabKind::Doc).unwrap().messages,
            Rate::per_second(500.0, 150.0)
        );
        let user_id = Uuid::new_v4();
        let now = Instant::now();
        let _first = scaled
            .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), now)
            .unwrap();
        let _second = scaled
            .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), now)
            .unwrap();
        assert!(
            scaled
                .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), now)
                .is_err()
        );
    }

    #[test]
    fn a_frame_past_the_byte_budget_passes_and_its_debt_is_waited_off() {
        let limits = enabled_limits();
        let user_id = Uuid::new_v4();
        let start = Instant::now();
        assert_eq!(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::ZERO
        );
        // 32 MiB of burst, then a 4 MiB paste: 4 MiB of debt at 1 MiB/s.
        for _ in 0..8 {
            limits.charge_frame(user_id, CollabKind::Doc, 4 * 1024 * 1024, false, start);
        }
        assert_eq!(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::ZERO
        );
        limits.charge_frame(user_id, CollabKind::Doc, 4 * 1024 * 1024, false, start);
        assert_close(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::from_secs(4),
        );
        assert_close(
            limits.deficit(user_id, CollabKind::Doc, start + Duration::from_secs(3)),
            Duration::from_secs(1),
        );
        assert_eq!(
            limits.deficit(user_id, CollabKind::Doc, start + Duration::from_secs(4)),
            Duration::ZERO
        );
    }

    #[test]
    fn messages_past_their_budget_wait_too() {
        let limits = enabled_limits();
        let user_id = Uuid::new_v4();
        let start = Instant::now();
        for _ in 0..1000 {
            limits.charge_frame(user_id, CollabKind::Doc, 10, false, start);
        }
        assert_eq!(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::ZERO
        );
        for _ in 0..300 {
            limits.charge_frame(user_id, CollabKind::Doc, 10, false, start);
        }
        assert_close(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::from_secs(1),
        );
        assert_eq!(
            limits
                .debt(user_id, CollabKind::Doc, start)
                .map(|(_, kind)| kind),
            Some(DebtKind::Messages)
        );
    }

    #[test]
    fn awareness_frames_cost_bytes_but_no_messages() {
        let limits = enabled_limits();
        let user_id = Uuid::new_v4();
        let start = Instant::now();
        for _ in 0..5000 {
            limits.charge_frame(user_id, CollabKind::Doc, 10, true, start);
        }
        assert_eq!(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::ZERO,
            "presence has its own budget"
        );
        limits.charge_frame(user_id, CollabKind::Doc, 33 * 1024 * 1024, true, start);
        assert_close(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::from_secs(1) + Duration::from_secs_f64(50_000.0 / MIB),
        );
        assert_eq!(
            limits
                .debt(user_id, CollabKind::Doc, start)
                .map(|(_, kind)| kind),
            Some(DebtKind::Bytes)
        );
    }

    #[test]
    fn doc_board_and_user_budgets_are_separate() {
        let limits = enabled_limits();
        let drawing_user = Uuid::new_v4();
        let other_user = Uuid::new_v4();
        let start = Instant::now();
        limits.charge_frame(
            drawing_user,
            CollabKind::Board,
            65 * 1024 * 1024,
            false,
            start,
        );
        assert!(
            !limits
                .deficit(drawing_user, CollabKind::Board, start)
                .is_zero()
        );
        assert_eq!(
            limits.deficit(drawing_user, CollabKind::Doc, start),
            Duration::ZERO,
            "a board drag never slows typing"
        );
        assert_eq!(
            limits.deficit(other_user, CollabKind::Board, start),
            Duration::ZERO
        );
    }

    #[test]
    fn awareness_tokens_run_out_and_come_back() {
        let limits = enabled_limits();
        let user_id = Uuid::new_v4();
        let start = Instant::now();
        for _ in 0..240 {
            limits
                .try_take_awareness(user_id, CollabKind::Doc, start)
                .unwrap();
        }
        let wait = limits
            .try_take_awareness(user_id, CollabKind::Doc, start)
            .unwrap_err();
        assert_close(wait, Duration::from_secs_f64(1.0 / 120.0));
        assert_eq!(
            limits.try_take_awareness(user_id, CollabKind::Board, start),
            Ok(()),
            "boards have their own presence budget"
        );
    }

    #[test]
    fn disabled_limits_pass_everything() {
        let limits = CollabLimits::disabled();
        let user_id = Uuid::new_v4();
        let start = Instant::now();
        limits.charge_frame(user_id, CollabKind::Doc, usize::MAX, false, start);
        assert_eq!(
            limits.deficit(user_id, CollabKind::Doc, start),
            Duration::ZERO
        );
        for _ in 0..10_000 {
            limits
                .try_take_awareness(user_id, CollabKind::Doc, start)
                .unwrap();
        }
        let target_id = Uuid::new_v4();
        let guards: Vec<SocketGuard> = (0..1000)
            .map(|_| {
                limits
                    .open_socket(user_id, CollabKind::Doc, target_id, start)
                    .unwrap()
            })
            .collect();
        assert_eq!(guards.len(), 1000);
        assert_eq!(limits.rates(CollabKind::Doc), None);
    }

    // -- Upgrades and the socket cap ----------------------------------------

    #[test]
    fn upgrades_are_limited_per_user_and_target() {
        let limits = enabled_limits();
        let user_id = Uuid::new_v4();
        let looping_target = Uuid::new_v4();
        let start = Instant::now();
        for _ in 0..20 {
            drop(
                limits
                    .open_socket(user_id, CollabKind::Doc, looping_target, start)
                    .unwrap(),
            );
        }
        let wait = limits
            .open_socket(user_id, CollabKind::Doc, looping_target, start)
            .err()
            .expect("the 21st reconnect to one doc is refused");
        assert_close(wait, Duration::from_secs(3));
        assert!(
            limits
                .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), start)
                .is_ok(),
            "skimming to another doc is not limited"
        );
        assert!(
            limits
                .open_socket(Uuid::new_v4(), CollabKind::Doc, looping_target, start)
                .is_ok(),
            "another user is not limited"
        );
        assert!(
            limits
                .open_socket(
                    user_id,
                    CollabKind::Doc,
                    looping_target,
                    start + Duration::from_secs(3)
                )
                .is_ok(),
            "the bucket refills"
        );
    }

    #[test]
    fn open_sockets_are_capped_per_user_and_freed_on_drop() {
        let limits = CollabLimits::new(1.0, 2);
        let user_id = Uuid::new_v4();
        let start = Instant::now();
        let doc_socket = limits
            .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), start)
            .unwrap();
        let _board_socket = limits
            .open_socket(user_id, CollabKind::Board, Uuid::new_v4(), start)
            .unwrap();
        assert_eq!(
            limits
                .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), start)
                .err(),
            Some(SOCKET_CAP_RETRY_AFTER),
            "doc and board sockets count together"
        );
        assert!(
            limits
                .open_socket(Uuid::new_v4(), CollabKind::Doc, Uuid::new_v4(), start)
                .is_ok(),
            "the cap is per user"
        );
        drop(doc_socket);
        let _reopened = limits
            .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), start)
            .expect("a closed socket frees its place");
    }

    #[test]
    fn the_socket_count_forgets_users_without_sockets() {
        let limits = CollabLimits::new(1.0, 2);
        let user_id = Uuid::new_v4();
        let open_sockets = limits.budgets.as_ref().unwrap().open_sockets.clone();
        let guard = limits
            .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), Instant::now())
            .unwrap();
        assert_eq!(open_sockets.open_count(user_id), 1);
        drop(guard);
        assert_eq!(open_sockets.open_count(user_id), 0);
        assert!(open_sockets.lock_by_user().is_empty());
    }

    #[test]
    fn a_refused_upgrade_holds_no_socket_place() {
        let limits = CollabLimits::new(1.0, 1);
        let user_id = Uuid::new_v4();
        let open_sockets = limits.budgets.as_ref().unwrap().open_sockets.clone();
        let target_id = Uuid::new_v4();
        let start = Instant::now();
        let _open = limits
            .open_socket(user_id, CollabKind::Doc, target_id, start)
            .unwrap();
        assert!(
            limits
                .open_socket(user_id, CollabKind::Doc, target_id, start)
                .is_err()
        );
        assert_eq!(open_sockets.open_count(user_id), 1);
    }

    #[test]
    fn an_upgrade_refused_at_the_socket_cap_costs_no_upgrade_token() {
        // At the floor, one upgrade per target a minute.
        let limits = CollabLimits::new(0.0001, 1);
        let user_id = Uuid::new_v4();
        let start = Instant::now();
        let first_socket = limits
            .open_socket(user_id, CollabKind::Doc, Uuid::new_v4(), start)
            .unwrap();
        let waiting_target = Uuid::new_v4();
        for _ in 0..3 {
            assert_eq!(
                limits
                    .open_socket(user_id, CollabKind::Doc, waiting_target, start)
                    .err(),
                Some(SOCKET_CAP_RETRY_AFTER)
            );
        }
        drop(first_socket);
        assert!(
            limits
                .open_socket(user_id, CollabKind::Doc, waiting_target, start)
                .is_ok(),
            "the refusals at the cap took no upgrade token"
        );
    }

    // -- ConnectionThrottle -------------------------------------------------

    struct Tokens {
        shutdown: CancellationToken,
        revoked: CancellationToken,
        refused: CancellationToken,
        room_ended: CancellationToken,
        done: CancellationToken,
    }

    impl Tokens {
        fn new() -> Self {
            Self {
                shutdown: CancellationToken::new(),
                revoked: CancellationToken::new(),
                refused: CancellationToken::new(),
                room_ended: CancellationToken::new(),
                done: CancellationToken::new(),
            }
        }

        fn signals(&self) -> SessionSignals<'_> {
            SessionSignals {
                shutdown: &self.shutdown,
                revoked: &self.revoked,
                refused: &self.refused,
                room_ended: &self.room_ended,
                done: &self.done,
            }
        }

        fn all(&self) -> [(&'static str, &CancellationToken); 5] {
            [
                ("shutdown", &self.shutdown),
                ("revoked", &self.revoked),
                ("refused", &self.refused),
                ("room_ended", &self.room_ended),
                ("done", &self.done),
            ]
        }
    }

    fn limited_state(collab: CollabLimits) -> Arc<RateLimits> {
        Arc::new(RateLimits {
            enabled: true,
            http: crate::rate_limit::HttpLimits::disabled(),
            collab,
            liveness: None,
        })
    }

    fn awareness_frame(marker: u8) -> Vec<u8> {
        vec![crate::protocol::MSG_AWARENESS, 1, marker]
    }

    fn sync_update_frame(payload_bytes: usize) -> Message {
        let mut frame = vec![crate::protocol::MSG_SYNC, crate::protocol::SYNC_UPDATE];
        crate::protocol::append_var_uint(&mut frame, payload_bytes as u64);
        frame.resize(frame.len() + payload_bytes, 0);
        Message::Binary(frame.into())
    }

    fn small_update_frame() -> Message {
        Message::Binary(vec![crate::protocol::MSG_SYNC, crate::protocol::SYNC_UPDATE, 0].into())
    }

    #[tokio::test(start_paused = true)]
    async fn without_debt_the_wait_returns_at_once() {
        let limits = limited_state(enabled_limits());
        let throttle = ConnectionThrottle::new(limits, Uuid::new_v4(), CollabKind::Doc);
        let tokens = Tokens::new();
        let start = Instant::now();
        assert_eq!(
            throttle.wait_for_budget(&tokens.signals()).await,
            WaitOutcome::Continue
        );
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_next_read_waits_off_the_debt_of_a_frame_read() {
        // At the floors: 4 MiB of bytes, 64 KiB/s.
        let limits = limited_state(CollabLimits::new(0.0001, 64));
        let throttle = ConnectionThrottle::new(limits, Uuid::new_v4(), CollabKind::Doc);
        let tokens = Tokens::new();
        let start = Instant::now();
        // A 4 MiB frame and then 128 KiB more: 2 s of byte debt, and the
        // message debt of the second frame (1/20 s) is shorter.
        throttle.charge_inbound(&sync_update_frame(4 * 1024 * 1024));
        throttle.charge_inbound(&sync_update_frame(128 * 1024));
        assert_eq!(
            throttle.wait_for_budget(&tokens.signals()).await,
            WaitOutcome::Continue
        );
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_secs(2) && waited < Duration::from_millis(2100),
            "waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn debt_another_socket_adds_during_a_wait_is_waited_off_too() {
        // At the floors: one message, 20 per second.
        let limits = limited_state(CollabLimits::new(0.0001, 64));
        let user_id = Uuid::new_v4();
        let first_socket = ConnectionThrottle::new(limits.clone(), user_id, CollabKind::Doc);
        let second_socket = ConnectionThrottle::new(limits, user_id, CollabKind::Doc);
        let tokens = Tokens::new();
        let start = Instant::now();
        for _ in 0..21 {
            first_socket.charge_inbound(&small_update_frame());
        }
        // 20 messages of debt: 1 s. Half a second in, 10 are paid off and
        // the other socket adds 40 more: 50 of debt, 2.5 s from then.
        let add_debt_later = async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for _ in 0..40 {
                second_socket.charge_inbound(&small_update_frame());
            }
            std::future::pending::<()>().await
        };
        let signals = tokens.signals();
        let outcome = tokio::select! {
            outcome = first_socket.wait_for_budget(&signals) => outcome,
            _ = add_debt_later => unreachable!(),
        };
        assert_eq!(outcome, WaitOutcome::Continue);
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_secs(3) && waited < Duration::from_millis(3100),
            "waited {waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_session_signal_ends_a_wait_at_once() {
        for signal_index in 0..5 {
            let limits = limited_state(CollabLimits::new(0.0001, 64));
            let throttle = ConnectionThrottle::new(limits, Uuid::new_v4(), CollabKind::Board);
            // 4 MiB of burst, 8 MiB read: a minute of byte debt.
            throttle.charge_inbound(&sync_update_frame(4 * 1024 * 1024));
            throttle.charge_inbound(&sync_update_frame(4 * 1024 * 1024));
            let tokens = Tokens::new();
            let (signal_name, signal) = tokens.all()[signal_index];
            let start = Instant::now();
            let cancel_later = async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                signal.cancel();
                std::future::pending::<()>().await
            };
            let signals = tokens.signals();
            let outcome = tokio::select! {
                outcome = throttle.wait_for_budget(&signals) => outcome,
                _ = cancel_later => unreachable!(),
            };
            assert_eq!(outcome, WaitOutcome::Stop, "{signal_name}");
            assert_eq!(
                start.elapsed(),
                Duration::from_millis(100),
                "{signal_name} ended the wait at once"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_limits_never_wait_hold_or_ignore() {
        let limits = Arc::new(RateLimits::disabled());
        let mut throttle = ConnectionThrottle::new(limits, Uuid::new_v4(), CollabKind::Doc);
        let tokens = Tokens::new();
        for _ in 0..100 {
            throttle.charge_inbound(&sync_update_frame(4 * 1024 * 1024));
        }
        let start = Instant::now();
        assert_eq!(
            throttle.wait_for_budget(&tokens.signals()).await,
            WaitOutcome::Continue
        );
        assert_eq!(start.elapsed(), Duration::ZERO);
        for marker in 0..=255 {
            assert_eq!(
                throttle.admit_awareness(awareness_frame(marker)),
                Some(awareness_frame(marker))
            );
        }
        assert_eq!(throttle.pending_awareness_due(), None);
        for _ in 0..3 {
            assert!(
                throttle.admit_sync_step1(),
                "every SyncStep1 is answered with limits off"
            );
        }
    }

    /// A y-protocols awareness frame with `(client id, clock, state JSON)`
    /// entries.
    fn y_awareness_frame(entries: &[(u64, u32, &str)]) -> Vec<u8> {
        use yrs::updates::encoder::Encode;
        let update = yrs::sync::awareness::AwarenessUpdate {
            clients: entries
                .iter()
                .map(|&(client_id, clock, json)| {
                    (
                        yrs::ClientID::new(client_id),
                        yrs::sync::awareness::AwarenessUpdateEntry {
                            clock,
                            json: json.into(),
                        },
                    )
                })
                .collect(),
        };
        let update_bytes = update.encode_v1();
        let mut frame = vec![crate::protocol::MSG_AWARENESS];
        crate::protocol::append_var_uint(&mut frame, update_bytes.len() as u64);
        frame.extend_from_slice(&update_bytes);
        frame
    }

    /// The `(client id, clock, state JSON)` entries of an awareness frame,
    /// sorted by client id.
    fn y_awareness_entries(frame: &[u8]) -> Vec<(u64, u32, String)> {
        use yrs::encoding::read::Read;
        use yrs::updates::decoder::Decode;
        let mut cursor = yrs::encoding::read::Cursor::new(&frame[1..]);
        let update_bytes = cursor.read_buf().unwrap();
        let update = yrs::sync::awareness::AwarenessUpdate::decode_v1(update_bytes).unwrap();
        let mut entries: Vec<(u64, u32, String)> = update
            .clients
            .iter()
            .map(|(client_id, entry)| (client_id.get(), entry.clock, entry.json.to_string()))
            .collect();
        entries.sort();
        entries
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_presence_frame_keeps_its_clients_when_a_frame_about_others_follows() {
        // A provider also relays the removals its awareness times out for
        // other clients: such a frame must not drop the held frame's own
        // cursor, which would then never arrive.
        let limits = limited_state(CollabLimits::new(0.0001, 64));
        let mut throttle = ConnectionThrottle::new(limits, Uuid::new_v4(), CollabKind::Board);
        let own_client = 7;
        let departed_client = 9;
        throttle.admit_awareness(y_awareness_frame(&[(own_client, 3, r#"{"p":1}"#)]));
        assert_eq!(
            throttle.admit_awareness(y_awareness_frame(&[(own_client, 4, r#"{"p":2}"#)])),
            None
        );
        throttle.admit_awareness(y_awareness_frame(&[(departed_client, 6, "null")]));
        // An entry with an older clock than the held one doesn't win.
        throttle.admit_awareness(y_awareness_frame(&[(own_client, 2, r#"{"p":0}"#)]));
        sleep_until_due(throttle.pending_awareness_due()).await;
        let forwarded = throttle.take_due_awareness().expect("the held frame");
        assert_eq!(
            y_awareness_entries(&forwarded),
            vec![
                (own_client, 4, r#"{"p":2}"#.to_string()),
                (departed_client, 6, "null".to_string()),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn awareness_past_its_budget_is_coalesced_to_the_newest_frame() {
        // Awareness at the floor: one token, one per minute.
        let limits = limited_state(CollabLimits::new(0.0001, 64));
        let mut throttle = ConnectionThrottle::new(limits, Uuid::new_v4(), CollabKind::Doc);
        let start = Instant::now();
        assert_eq!(
            throttle.admit_awareness(awareness_frame(1)),
            Some(awareness_frame(1))
        );
        assert_eq!(throttle.admit_awareness(awareness_frame(2)), None);
        let due = throttle.pending_awareness_due().expect("frame 2 is held");
        assert_close(due - start, Duration::from_secs(60));
        assert_eq!(throttle.admit_awareness(awareness_frame(3)), None);
        assert_eq!(throttle.admit_awareness(awareness_frame(4)), None);
        assert_eq!(
            throttle.pending_awareness_due(),
            Some(due),
            "replacing the held frame keeps its due time"
        );
        assert_eq!(
            throttle.take_due_awareness(),
            None,
            "not due yet: the token isn't back"
        );
        sleep_until_due(throttle.pending_awareness_due()).await;
        assert_eq!(
            throttle.take_due_awareness(),
            Some(awareness_frame(4)),
            "the newest state arrives"
        );
        assert_eq!(throttle.pending_awareness_due(), None);
        assert_eq!(throttle.take_due_awareness(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_frame_whose_token_another_socket_took_waits_on() {
        let limits = limited_state(CollabLimits::new(0.0001, 64));
        let user_id = Uuid::new_v4();
        let mut first_socket = ConnectionThrottle::new(limits.clone(), user_id, CollabKind::Doc);
        let mut second_socket = ConnectionThrottle::new(limits, user_id, CollabKind::Doc);
        assert!(first_socket.admit_awareness(awareness_frame(1)).is_some());
        assert!(first_socket.admit_awareness(awareness_frame(2)).is_none());
        let first_due = first_socket.pending_awareness_due().unwrap();
        tokio::time::sleep_until(first_due).await;
        assert!(
            second_socket.admit_awareness(awareness_frame(9)).is_some(),
            "the other socket took the token back first"
        );
        assert_eq!(first_socket.take_due_awareness(), None);
        let second_due = first_socket.pending_awareness_due().unwrap();
        assert!(second_due > first_due, "the due time moves on");
        tokio::time::sleep_until(second_due).await;
        assert_eq!(first_socket.take_due_awareness(), Some(awareness_frame(2)));
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_first_sync_step1_is_answered_with_limits_on() {
        let limits = limited_state(enabled_limits());
        let mut throttle = ConnectionThrottle::new(limits.clone(), Uuid::new_v4(), CollabKind::Doc);
        assert!(throttle.admit_sync_step1());
        assert!(!throttle.admit_sync_step1());
        assert!(!throttle.admit_sync_step1());
        let mut next_connection = ConnectionThrottle::new(limits, Uuid::new_v4(), CollabKind::Doc);
        assert!(
            next_connection.admit_sync_step1(),
            "the rule is per connection"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_until_due_never_resolves_without_a_due_time() {
        let resolved = tokio::time::timeout(Duration::from_secs(3600), sleep_until_due(None)).await;
        assert!(resolved.is_err());
    }
}
