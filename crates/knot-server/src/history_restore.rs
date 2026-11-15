//! Restore here (page history M2): the verified revert behind
//! `POST /api/docs/{id}/history/entries/{eid}/restore`.
//!
//! The handler (`routes::api::history::restore_entry`) replays the entry
//! into its target once (`Views::restore_target`), acquires the page's room
//! once ([`LiveRoom::acquire_within`]) and then runs [`revert_attempts`]:
//! up to [`REVERT_ATTEMPTS`] times it exports the live page from the room,
//! plans the revert against that export on a replay thread
//! (`Views::plan_revert`) and sends it back to the room, which applies it
//! only when the page has not changed since the export. A page that keeps
//! changing is planned again from a fresh export. Everything runs within
//! one [`RESTORE_DEADLINE`] from the handler's start, inside the router's
//! 30 s timeout; [`restore_answer`] maps the result to the route's answer
//! and counts `knot_history_restore_total{outcome}` once.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use knot_crdt::{RevertBase, RevertRefused};
use knot_history::{PlanError, REVERT_MIN_PLAN_TIME, RevertKind, RevertOutcome, ViewError};
use knot_storage::UpdateSource;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use crate::http_error::json_err;
use crate::routes::api::history::{RESTORE_ROOM_WAIT, view_error};

/// How long an entry restore may take from its handler's start: the
/// restore permit, the target replay, the room's acquire and every
/// attempt (the router's `TimeoutLayer` answers at 30 s).
pub(crate) const RESTORE_DEADLINE: Duration = Duration::from_secs(27);

/// How many times a revert is planned from a fresh export when the page
/// changed between the export and the apply (or the room could not catch
/// up with the page's log).
pub(crate) const REVERT_ATTEMPTS: usize = 5;

/// The pause before the export that follows an export that could not
/// catch up with the page's log (never past the deadline).
pub(crate) const NOT_CAUGHT_UP_PAUSE: Duration = Duration::from_millis(200);

/// Why a room call answered nothing usable.
#[derive(Debug)]
pub(crate) enum RoomCall {
    /// The call's wait ran out.
    TimedOut,
    /// The event never reached the room (its channel was closed at the
    /// send), or the export failed in the engine.
    Gone(String),
    /// The event was handed to the room and its reply was dropped: an
    /// apply may still have been stored (the lost-reply Ruling).
    ReplyLost,
    /// The room refused the revert, applying nothing.
    Refused(RevertRefused),
}

/// The two room calls a restore makes, over the page's room
/// ([`LiveRoom`]) or a test's.
#[async_trait]
pub(crate) trait RevertRoom: Send + Sync {
    /// The live page a revert is planned on (`Event::ExportForRevert`).
    async fn export(&self) -> Result<RevertBase, RoomCall>;

    /// Applies `update` if the page is still at `expected_version`
    /// (`Event::ApplyRevert`); the stored row's seq.
    async fn apply(
        &self,
        update: Vec<u8>,
        expected_version: u64,
        source: UpdateSource,
        by: Uuid,
        meta: Value,
    ) -> Result<i64, RoomCall>;
}

/// The page's room, acquired once per restore: `content_version` counts
/// from 0 in every room instance, so a version exported from one instance
/// must never be applied to another. The handle is kept until the restore
/// ends, so the registry cannot drop the room between export and apply.
pub(crate) struct LiveRoom<H: Send + Sync> {
    _handle: H,
    tx: mpsc::Sender<knot_crdt::Event>,
    /// The bound of each call (`RESTORE_ROOM_WAIT` in production).
    call_wait: Duration,
}

impl<H: Send + Sync> LiveRoom<H> {
    pub(crate) fn new(
        handle: H,
        tx: mpsc::Sender<knot_crdt::Event>,
        call_wait: Duration,
    ) -> LiveRoom<H> {
        LiveRoom {
            _handle: handle,
            tx,
            call_wait,
        }
    }
}

impl<H: Send + Sync + 'static> LiveRoom<H> {
    /// The room `acquire` yields, waited for within `wait`. The acquire
    /// runs on a task of its own: when the wait runs out, the room's boot
    /// keeps going and the room stays registered for the next click (the
    /// cancellation Ruling), and this restore is [`RestoreFailure::Busy`]
    /// (nothing was sent). An acquire that fails or panics is
    /// [`RestoreFailure::Failed`].
    pub(crate) async fn acquire_within(
        acquire: impl Future<Output = Result<(H, mpsc::Sender<knot_crdt::Event>), String>>
        + Send
        + 'static,
        wait: Duration,
    ) -> Result<LiveRoom<H>, RestoreFailure> {
        let task = tokio::spawn(acquire);
        match tokio::time::timeout(wait, task).await {
            Ok(Ok(Ok((handle, tx)))) => Ok(LiveRoom::new(handle, tx, RESTORE_ROOM_WAIT)),
            Ok(Ok(Err(e))) => Err(RestoreFailure::Failed(format!("room acquire: {e}"))),
            Ok(Err(e)) => Err(RestoreFailure::Failed(format!("room acquire: {e}"))),
            Err(_) => Err(RestoreFailure::Busy),
        }
    }
}

#[async_trait]
impl<H: Send + Sync> RevertRoom for LiveRoom<H> {
    async fn export(&self) -> Result<RevertBase, RoomCall> {
        let (reply, replied) = oneshot::channel();
        let call = async {
            self.tx
                .send(knot_crdt::Event::ExportForRevert(reply))
                .await
                .map_err(|_| RoomCall::Gone("the room is closed".to_string()))?;
            match replied.await {
                Ok(Ok(base)) => Ok(base),
                Ok(Err(e)) => Err(RoomCall::Gone(format!("export: {e}"))),
                Err(_) => Err(RoomCall::ReplyLost),
            }
        };
        tokio::time::timeout(self.call_wait, call)
            .await
            .unwrap_or(Err(RoomCall::TimedOut))
    }

    async fn apply(
        &self,
        update: Vec<u8>,
        expected_version: u64,
        source: UpdateSource,
        by: Uuid,
        meta: Value,
    ) -> Result<i64, RoomCall> {
        let (reply, replied) = oneshot::channel();
        let event = knot_crdt::Event::ApplyRevert {
            update_bytes: update,
            expected_version,
            by_user: Some(by),
            source,
            meta,
            reply,
        };
        let call = async {
            self.tx
                .send(event)
                .await
                .map_err(|_| RoomCall::Gone("the room is closed".to_string()))?;
            match replied.await {
                Ok(Ok(seq)) => Ok(seq),
                Ok(Err(refused)) => Err(RoomCall::Refused(refused)),
                Err(_) => Err(RoomCall::ReplyLost),
            }
        };
        tokio::time::timeout(self.call_wait, call)
            .await
            .unwrap_or(Err(RoomCall::TimedOut))
    }
}

/// What a restore wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Restored {
    /// Reverted by yrs's undo (source `restore`).
    Undo,
    /// Reverted by the whole-page replace (source `restore_fallback`).
    Fallback,
    /// The page already matched the version; nothing written.
    Unchanged,
}

impl From<RevertKind> for Restored {
    fn from(kind: RevertKind) -> Restored {
        match kind {
            RevertKind::Undo => Restored::Undo,
            RevertKind::Fallback => Restored::Fallback,
        }
    }
}

/// Why a restore wrote nothing, or may not have.
#[derive(Debug)]
pub(crate) enum RestoreFailure {
    /// The page changed between export and apply on every attempt.
    Stale,
    /// The room could not catch up with the page's log on the last attempt.
    NotCaughtUp,
    /// Over the revert cap or too slow to plan, now or recorded.
    TooLarge,
    /// Neither the undo nor the whole-page replace verified.
    Unverified,
    /// The deadline ran out (or too little was left) before a revert was
    /// sent, or no permit, slot or room within its wait.
    Busy,
    /// The deadline ran out, or the reply was lost, after a revert was
    /// sent: it may still apply.
    TimedOutAfterSend,
    /// The room's acquire failed, or the room closed or failed its export.
    Failed(String),
    /// The view's refusal (an export at another content epoch is
    /// [`ViewError::Moved`]).
    View(ViewError),
    /// The room refused the verified update.
    Apply(String),
}

/// A plan's refusal as the restore's.
fn plan_failure(e: PlanError, doc_id: Uuid) -> RestoreFailure {
    match e {
        PlanError::OverCap | PlanError::TooSlow => RestoreFailure::TooLarge,
        PlanError::View(ViewError::Busy) => RestoreFailure::Busy,
        PlanError::View(e) => RestoreFailure::View(e),
        PlanError::Unverified(reason) => {
            tracing::warn!(%doc_id, reason, "restore: no revert verified");
            RestoreFailure::Unverified
        }
    }
}

/// Plans and applies the revert of `doc_id` on `room`, up to
/// [`REVERT_ATTEMPTS`] times, all before `deadline`. `plan` plans a revert
/// against the live state it is given (`Views::plan_revert`), `epoch` is
/// the content epoch the version was replayed at, and `by` and `meta` are
/// the restore row's author and meta.
///
/// Each attempt: with less than [`REVERT_MIN_PLAN_TIME`] left it is
/// [`RestoreFailure::Busy`] without a room call; it exports; an export at
/// another epoch is [`ViewError::Moved`]; one that could not catch up ends
/// the attempt as `NotCaughtUp` with no plan (the next attempt first
/// pauses [`NOT_CAUGHT_UP_PAUSE`]); otherwise it plans, and an unchanged
/// page returns at once; else it applies with the attempt's own source.
/// `Stale` and `NotCaughtUp` refusals go on to the next attempt (the last
/// one's decides); every other answer returns at once. Running out of time
/// before the attempt's apply is sent is `Busy`; after, `TimedOutAfterSend`.
pub(crate) async fn revert_attempts<P, F>(
    room: &dyn RevertRoom,
    mut plan: P,
    deadline: Instant,
    epoch: i32,
    doc_id: Uuid,
    by: Uuid,
    meta: Value,
) -> Result<Restored, RestoreFailure>
where
    P: FnMut(Vec<u8>) -> F,
    F: Future<Output = Result<RevertOutcome, PlanError>>,
{
    use tokio::time::timeout_at;

    let mut last = RestoreFailure::Stale;
    let mut pause = false;
    for _ in 0..REVERT_ATTEMPTS {
        if pause {
            tokio::time::sleep_until((Instant::now() + NOT_CAUGHT_UP_PAUSE).min(deadline)).await;
        }
        if deadline.saturating_duration_since(Instant::now()) < REVERT_MIN_PLAN_TIME {
            return Err(RestoreFailure::Busy);
        }

        let base = match timeout_at(deadline, room.export()).await {
            Ok(Ok(base)) => base,
            Err(_) | Ok(Err(RoomCall::TimedOut)) => return Err(RestoreFailure::Busy),
            Ok(Err(RoomCall::Gone(e))) => return Err(RestoreFailure::Failed(e)),
            Ok(Err(RoomCall::ReplyLost)) => {
                return Err(RestoreFailure::Failed(
                    "the room dropped its export's reply".to_string(),
                ));
            }
            Ok(Err(RoomCall::Refused(r))) => {
                return Err(RestoreFailure::Failed(format!("export refused: {r}")));
            }
        };
        if base.epoch != epoch {
            return Err(RestoreFailure::View(ViewError::Moved));
        }
        if !base.caught_up {
            last = RestoreFailure::NotCaughtUp;
            pause = true;
            continue;
        }
        pause = false;

        let outcome = match timeout_at(deadline, plan(base.state)).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => return Err(plan_failure(e, doc_id)),
            Err(_) => return Err(RestoreFailure::Busy),
        };
        let (update, kind, other_roots) = match outcome {
            RevertOutcome::Unchanged => return Ok(Restored::Unchanged),
            RevertOutcome::Revert {
                update,
                kind,
                other_roots_changed,
            } => (update, kind, other_roots_changed),
        };

        // From here on the attempt's apply is in flight.
        let applied = timeout_at(
            deadline,
            room.apply(update, base.version, kind.source(), by, meta.clone()),
        )
        .await;
        match applied {
            Ok(Ok(_seq)) => {
                if !other_roots.is_empty() {
                    tracing::info!(
                        %doc_id,
                        target_entry_id = %meta["target_entry_id"],
                        roots = %other_roots.join(","),
                        "restore left other Y.Doc roots as they are"
                    );
                }
                return Ok(kind.into());
            }
            Ok(Err(RoomCall::Refused(RevertRefused::Stale))) => last = RestoreFailure::Stale,
            Ok(Err(RoomCall::Refused(RevertRefused::NotCaughtUp))) => {
                last = RestoreFailure::NotCaughtUp;
            }
            Ok(Err(RoomCall::Refused(RevertRefused::Apply(e)))) => {
                return Err(RestoreFailure::Apply(e));
            }
            Err(_) | Ok(Err(RoomCall::TimedOut | RoomCall::ReplyLost)) => {
                return Err(RestoreFailure::TimedOutAfterSend);
            }
            Ok(Err(RoomCall::Gone(e))) => return Err(RestoreFailure::Failed(e)),
        }
    }
    Err(last)
}

/// The route's answer to a restore, counted once in
/// `knot_history_restore_total{outcome}`.
pub(crate) fn restore_answer(r: Result<Restored, RestoreFailure>) -> Response {
    let busy = || {
        let mut r = json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "history.busy",
            "history is busy",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        r
    };
    let ok = |outcome: &'static str| (outcome, Json(json!({ "outcome": outcome })).into_response());
    let (outcome, response) = match r {
        Ok(Restored::Undo) => ok(RevertKind::Undo.outcome()),
        Ok(Restored::Fallback) => ok(RevertKind::Fallback.outcome()),
        Ok(Restored::Unchanged) => ok("unchanged"),
        Err(RestoreFailure::Stale) => (
            "stale",
            json_err(
                StatusCode::CONFLICT,
                "history.restore_busy",
                "the page kept changing while it was restored",
            ),
        ),
        Err(RestoreFailure::NotCaughtUp) => {
            tracing::warn!("restore: the room could not catch up with the page's log");
            ("failed", busy())
        }
        Err(RestoreFailure::TooLarge) => (
            "too_large",
            json_err(
                StatusCode::CONFLICT,
                "restore.too_large",
                "the page is too large to restore in place",
            ),
        ),
        Err(RestoreFailure::Unverified) => (
            "unverified",
            json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history.restore_failed",
                "the restore could not be verified",
            ),
        ),
        Err(RestoreFailure::Busy) => ("busy", busy()),
        Err(RestoreFailure::TimedOutAfterSend) => {
            tracing::warn!("restore did not finish in time");
            (
                "timeout",
                json_err(
                    StatusCode::GATEWAY_TIMEOUT,
                    "restore.timeout",
                    "the restore did not finish in time; it may still apply",
                ),
            )
        }
        Err(RestoreFailure::Apply(e)) => {
            tracing::warn!(error = %e, "restore refused by the room");
            (
                "apply_failed",
                json_err(StatusCode::INTERNAL_SERVER_ERROR, "restore.apply", ""),
            )
        }
        Err(RestoreFailure::Failed(e)) => {
            tracing::error!(error = %e, "restore failed");
            (
                "failed",
                json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""),
            )
        }
        Err(RestoreFailure::View(e)) => {
            if matches!(e, ViewError::Failed(_) | ViewError::Store(_)) {
                tracing::error!(error = %e, "restore plan");
            }
            let outcome = if matches!(e, ViewError::Busy) {
                "busy"
            } else {
                "failed"
            };
            (outcome, view_error(&e))
        }
    };
    metrics::counter!("knot_history_restore_total", "outcome" => outcome).increment(1);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use knot_crdt::{EngineError, Event};

    const DOC: Uuid = Uuid::from_u128(0xd0c);
    const BY: Uuid = Uuid::from_u128(0xb1);

    /// The restore row's meta, as the handler builds it.
    fn meta() -> Value {
        json!({
            "target_entry_id": 7,
            "target_rev": 3,
            "target_ended_at": "2026-11-14T09:40:00+00:00",
        })
    }

    /// The `n`th export of a room at epoch 0: its bytes and version name it.
    fn base(n: usize) -> RevertBase {
        RevertBase {
            state: vec![n as u8, 0xee],
            version: n as u64,
            caught_up: true,
            epoch: 0,
        }
    }

    fn behind(n: usize) -> RevertBase {
        RevertBase {
            caught_up: false,
            ..base(n)
        }
    }

    fn at_epoch(n: usize, epoch: i32) -> RevertBase {
        RevertBase { epoch, ..base(n) }
    }

    fn revert(kind: RevertKind) -> RevertOutcome {
        RevertOutcome::Revert {
            update: vec![1, 2, 3],
            kind,
            other_roots_changed: Vec::new(),
        }
    }

    fn stale() -> RoomCall {
        RoomCall::Refused(RevertRefused::Stale)
    }

    fn not_caught_up() -> RoomCall {
        RoomCall::Refused(RevertRefused::NotCaughtUp)
    }

    fn in_27s() -> Instant {
        Instant::now() + RESTORE_DEADLINE
    }

    // -----------------------------------------------------------------------
    // A fake room and a fake plan

    /// What a fake room call answers, and when.
    enum Step<T> {
        Now(Result<T, RoomCall>),
        After(Duration, Result<T, RoomCall>),
        Until(Instant, Result<T, RoomCall>),
    }

    type Script<T> = Box<dyn Fn(usize) -> Step<T> + Send + Sync>;

    /// A room whose `n`th export and `n`th apply answer as scripted.
    struct FakeRoom {
        export: Script<RevertBase>,
        apply: Script<i64>,
        exported_at: Mutex<Vec<Instant>>,
        applied: Mutex<Vec<(u64, UpdateSource)>>,
    }

    impl FakeRoom {
        fn new(
            export: impl Fn(usize) -> Step<RevertBase> + Send + Sync + 'static,
            apply: impl Fn(usize) -> Step<i64> + Send + Sync + 'static,
        ) -> FakeRoom {
            FakeRoom {
                export: Box::new(export),
                apply: Box::new(apply),
                exported_at: Mutex::new(Vec::new()),
                applied: Mutex::new(Vec::new()),
            }
        }

        /// Fresh exports; the `n`th apply answers `apply(n)` at once.
        fn applying(
            apply: impl Fn(usize) -> Result<i64, RoomCall> + Send + Sync + 'static,
        ) -> FakeRoom {
            FakeRoom::new(|n| Step::Now(Ok(base(n))), move |n| Step::Now(apply(n)))
        }

        fn exports(&self) -> usize {
            self.exported_at.lock().unwrap().len()
        }

        fn applies(&self) -> usize {
            self.applied.lock().unwrap().len()
        }
    }

    async fn play<T>(step: Step<T>) -> Result<T, RoomCall> {
        match step {
            Step::Now(r) => r,
            Step::After(d, r) => {
                tokio::time::sleep(d).await;
                r
            }
            Step::Until(at, r) => {
                tokio::time::sleep_until(at).await;
                r
            }
        }
    }

    #[async_trait]
    impl RevertRoom for FakeRoom {
        async fn export(&self) -> Result<RevertBase, RoomCall> {
            let n = {
                let mut at = self.exported_at.lock().unwrap();
                at.push(Instant::now());
                at.len() - 1
            };
            play((self.export)(n)).await
        }

        async fn apply(
            &self,
            _update: Vec<u8>,
            expected_version: u64,
            source: UpdateSource,
            by: Uuid,
            meta: Value,
        ) -> Result<i64, RoomCall> {
            assert_eq!(by, BY);
            assert_eq!(meta, super::tests::meta());
            let n = {
                let mut applied = self.applied.lock().unwrap();
                applied.push((expected_version, source));
                applied.len() - 1
            };
            play((self.apply)(n)).await
        }
    }

    type PlanFuture = BoxFuture<'static, Result<RevertOutcome, PlanError>>;

    /// The live states each plan was given.
    #[derive(Clone, Default)]
    struct Plans(Arc<Mutex<Vec<Vec<u8>>>>);

    impl Plans {
        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }

        fn seen(&self) -> Vec<Vec<u8>> {
            self.0.lock().unwrap().clone()
        }

        /// A plan whose `n`th call answers `answer(n)` after its delay.
        fn planner<A>(&self, answer: A) -> impl FnMut(Vec<u8>) -> PlanFuture + use<A>
        where
            A: Fn(usize) -> (Duration, Result<RevertOutcome, PlanError>) + Send + Sync + 'static,
        {
            let seen = self.0.clone();
            move |live| {
                let n = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(live);
                    seen.len() - 1
                };
                let (after, r) = answer(n);
                Box::pin(async move {
                    if !after.is_zero() {
                        tokio::time::sleep(after).await;
                    }
                    r
                })
            }
        }

        /// A plan that answers `r()` at once, every time.
        fn always<R>(&self, r: R) -> impl FnMut(Vec<u8>) -> PlanFuture + use<R>
        where
            R: Fn() -> Result<RevertOutcome, PlanError> + Send + Sync + 'static,
        {
            self.planner(move |_| (Duration::ZERO, r()))
        }
    }

    async fn attempts(
        room: &dyn RevertRoom,
        plan: impl FnMut(Vec<u8>) -> PlanFuture,
        deadline: Instant,
    ) -> Result<Restored, RestoreFailure> {
        revert_attempts(room, plan, deadline, 0, DOC, BY, meta()).await
    }

    // -----------------------------------------------------------------------
    // The answer and its count

    /// Remembers the key of every counter increment.
    #[derive(Default)]
    struct Counters(Mutex<Vec<String>>);

    impl metrics::Recorder for Counters {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(
            &self,
            key: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            let labels: Vec<String> = key
                .labels()
                .map(|label| format!("{}={}", label.key(), label.value()))
                .collect();
            self.0
                .lock()
                .unwrap()
                .push(format!("{}{{{}}}", key.name(), labels.join(",")));
            metrics::Counter::noop()
        }
        fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            metrics::Gauge::noop()
        }
        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    /// What [`restore_answer`] answered and counted.
    #[derive(Debug)]
    struct Answered {
        status: StatusCode,
        retry_after: Option<String>,
        body: Value,
        counted: Vec<String>,
    }

    impl Answered {
        /// A 200 with `outcome`, counted once under it.
        #[track_caller]
        fn ok(&self, outcome: &str) {
            assert_eq!(self.status, StatusCode::OK, "{self:?}");
            assert_eq!(self.body, json!({ "outcome": outcome }), "{self:?}");
            self.counted_once(outcome);
        }

        /// A JSON error `status` `code`, counted once under `outcome`.
        #[track_caller]
        fn error(&self, status: StatusCode, code: &str, outcome: &str) {
            assert_eq!(self.status, status, "{self:?}");
            assert_eq!(self.body["error"]["code"], code, "{self:?}");
            self.counted_once(outcome);
        }

        /// 503 `history.busy` with `Retry-After: 5`, counted under `outcome`.
        #[track_caller]
        fn busy(&self, outcome: &str) {
            self.error(StatusCode::SERVICE_UNAVAILABLE, "history.busy", outcome);
            assert_eq!(self.retry_after.as_deref(), Some("5"), "{self:?}");
        }

        #[track_caller]
        fn counted_once(&self, outcome: &str) {
            assert_eq!(
                self.counted,
                [format!("knot_history_restore_total{{outcome={outcome}}}")],
                "{self:?}"
            );
        }
    }

    async fn answered(r: Result<Restored, RestoreFailure>) -> Answered {
        let counters = Counters::default();
        let response = metrics::with_local_recorder(&counters, || restore_answer(r));
        let status = response.status();
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let counted = counters.0.lock().unwrap().clone();
        Answered {
            status,
            retry_after,
            body,
            counted,
        }
    }

    // -----------------------------------------------------------------------
    // revert_attempts over a fake room

    #[tokio::test]
    async fn a_stale_revert_is_planned_again_from_a_fresh_export() {
        let room = FakeRoom::applying(|n| if n == 0 { Err(stale()) } else { Ok(7) });
        let plans = Plans::default();
        let r = attempts(
            &room,
            plans.always(|| Ok(revert(RevertKind::Undo))),
            in_27s(),
        )
        .await;
        assert!(matches!(r, Ok(Restored::Undo)), "{r:?}");
        assert_eq!(room.exports(), 2);
        assert_eq!(plans.count(), 2);
        assert_eq!(plans.seen(), [base(0).state, base(1).state]);
        // Each apply names the version of the export its plan saw.
        let applied = room.applied.lock().unwrap().clone();
        assert_eq!(
            applied,
            [(0, UpdateSource::Restore), (1, UpdateSource::Restore)]
        );
        answered(r).await.ok("restored");
    }

    #[tokio::test]
    async fn five_stale_attempts_answer_restore_busy() {
        let room = FakeRoom::applying(|_| Err(stale()));
        let plans = Plans::default();
        let r = attempts(
            &room,
            plans.always(|| Ok(revert(RevertKind::Undo))),
            in_27s(),
        )
        .await;
        assert!(matches!(r, Err(RestoreFailure::Stale)), "{r:?}");
        assert_eq!(room.exports(), REVERT_ATTEMPTS);
        assert_eq!(REVERT_ATTEMPTS, 5);
        assert_eq!(plans.count(), 5);
        assert_eq!(room.applies(), 5);
        answered(r)
            .await
            .error(StatusCode::CONFLICT, "history.restore_busy", "stale");
    }

    #[tokio::test]
    async fn an_unchanged_page_writes_nothing() {
        let room = FakeRoom::applying(|_| Ok(7));
        let plans = Plans::default();
        let r = attempts(
            &room,
            plans.always(|| Ok(RevertOutcome::Unchanged)),
            in_27s(),
        )
        .await;
        assert!(matches!(r, Ok(Restored::Unchanged)), "{r:?}");
        assert_eq!(room.exports(), 1);
        assert_eq!(room.applies(), 0);
        answered(r).await.ok("unchanged");
    }

    #[tokio::test]
    async fn the_attempt_applies_its_own_kind() {
        for (kind, source, outcome) in [
            (
                RevertKind::Fallback,
                UpdateSource::RestoreFallback,
                "fallback",
            ),
            (RevertKind::Undo, UpdateSource::Restore, "restored"),
        ] {
            let room = FakeRoom::applying(|_| Ok(7));
            let plans = Plans::default();
            let r = attempts(&room, plans.always(move || Ok(revert(kind))), in_27s()).await;
            assert_eq!(*room.applied.lock().unwrap(), [(0, source)]);
            answered(r).await.ok(outcome);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_before_sending_is_busy_and_after_sending_is_a_504() {
        let undo = || Ok(revert(RevertKind::Undo));

        // A plan that runs past the deadline: nothing was sent.
        let room = FakeRoom::applying(|_| Ok(7));
        let plans = Plans::default();
        let slow = plans.planner(move |_| (Duration::from_secs(30), undo()));
        let r = attempts(&room, slow, in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::Busy)), "{r:?}");
        assert_eq!(room.applies(), 0);
        answered(r).await.busy("busy");

        // An export that timed out.
        let room = FakeRoom::new(|_| Step::Now(Err(RoomCall::TimedOut)), |_| Step::Now(Ok(7)));
        let r = attempts(&room, Plans::default().always(undo), in_27s()).await;
        answered(r).await.busy("busy");

        // An apply that runs past the deadline, or times out, or whose
        // reply was lost after it was sent: it may still apply.
        for apply in [
            Box::new(|_| Step::After(Duration::from_secs(30), Ok(7_i64))) as Script<i64>,
            Box::new(|_| Step::Now(Err(RoomCall::TimedOut))),
            Box::new(|_| Step::Now(Err(RoomCall::ReplyLost))),
        ] {
            let room = FakeRoom::new(|n| Step::Now(Ok(base(n))), apply);
            let r = attempts(&room, Plans::default().always(undo), in_27s()).await;
            assert!(matches!(r, Err(RestoreFailure::TimedOutAfterSend)), "{r:?}");
            assert_eq!(room.applies(), 1);
            answered(r)
                .await
                .error(StatusCode::GATEWAY_TIMEOUT, "restore.timeout", "timeout");
        }

        // An apply that never reached the room.
        let room = FakeRoom::applying(|_| Err(RoomCall::Gone("closed".into())));
        let r = attempts(&room, Plans::default().always(undo), in_27s()).await;
        answered(r)
            .await
            .error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "failed");

        // An export of a room that is gone, or whose reply was dropped.
        for export in [
            Box::new(|_| Step::Now(Err(RoomCall::Gone("closed".into())))) as Script<RevertBase>,
            Box::new(|_| Step::Now(Err(RoomCall::ReplyLost))),
        ] {
            let room = FakeRoom::new(export, |_| Step::Now(Ok(7)));
            let plans = Plans::default();
            let r = attempts(&room, plans.always(undo), in_27s()).await;
            assert!(matches!(r, Err(RestoreFailure::Failed(_))), "{r:?}");
            assert_eq!(plans.count(), 0);
            answered(r)
                .await
                .error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "failed");
        }
    }

    #[tokio::test]
    async fn a_refused_apply_is_restore_apply() {
        let room = FakeRoom::applying(|_| {
            Err(RoomCall::Refused(RevertRefused::Apply("pre-check".into())))
        });
        let r = attempts(
            &room,
            Plans::default().always(|| Ok(revert(RevertKind::Undo))),
            in_27s(),
        )
        .await;
        assert!(
            matches!(&r, Err(RestoreFailure::Apply(e)) if e == "pre-check"),
            "{r:?}"
        );
        assert_eq!(room.exports(), 1);
        assert_eq!(room.applies(), 1);
        answered(r).await.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "restore.apply",
            "apply_failed",
        );
    }

    #[tokio::test]
    async fn plan_errors_map_to_their_answers() {
        for (error, status, code, outcome) in [
            (
                PlanError::OverCap,
                StatusCode::CONFLICT,
                "restore.too_large",
                "too_large",
            ),
            (
                PlanError::TooSlow,
                StatusCode::CONFLICT,
                "restore.too_large",
                "too_large",
            ),
            (
                PlanError::Unverified("nothing verified".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "history.restore_failed",
                "unverified",
            ),
            (
                PlanError::View(ViewError::Busy),
                StatusCode::SERVICE_UNAVAILABLE,
                "history.busy",
                "busy",
            ),
            (
                PlanError::View(ViewError::Failed("panicked".into())),
                StatusCode::INTERNAL_SERVER_ERROR,
                "history.view_failed",
                "failed",
            ),
        ] {
            let room = FakeRoom::applying(|_| Ok(7));
            let plans = Plans::default();
            let e = error.clone();
            let r = attempts(&room, plans.always(move || Err(e.clone())), in_27s()).await;
            assert_eq!(room.exports(), 1, "{error:?}");
            assert_eq!(plans.count(), 1, "{error:?}");
            assert_eq!(room.applies(), 0, "{error:?}");
            answered(r).await.error(status, code, outcome);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_room_that_cannot_catch_up_is_busy_not_contention() {
        let undo = || Ok(revert(RevertKind::Undo));

        // A row arrived between export and apply: retried at once.
        let room = FakeRoom::applying(|n| if n == 0 { Err(not_caught_up()) } else { Ok(7) });
        let plans = Plans::default();
        let r = attempts(&room, plans.always(undo), in_27s()).await;
        assert!(matches!(r, Ok(Restored::Undo)), "{r:?}");
        assert_eq!((room.exports(), plans.count()), (2, 2));

        // Never caught up at the apply: busy, counted as failed, not stale.
        let room = FakeRoom::applying(|_| Err(not_caught_up()));
        let r = attempts(&room, Plans::default().always(undo), in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::NotCaughtUp)), "{r:?}");
        assert_eq!(room.exports(), 5);
        answered(r).await.busy("failed");

        // The last attempt's refusal decides.
        let room = FakeRoom::applying(|n| Err(if n < 4 { stale() } else { not_caught_up() }));
        let r = attempts(&room, Plans::default().always(undo), in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::NotCaughtUp)), "{r:?}");
        answered(r).await.busy("failed");

        // No export caught up: no plan, no apply, a pause between exports.
        let room = FakeRoom::new(|n| Step::Now(Ok(behind(n))), |_| Step::Now(Ok(7)));
        let plans = Plans::default();
        let r = attempts(&room, plans.always(undo), in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::NotCaughtUp)), "{r:?}");
        assert_eq!((room.exports(), plans.count(), room.applies()), (5, 0, 0));
        let at = room.exported_at.lock().unwrap().clone();
        assert!(
            at[4] - at[0] >= NOT_CAUGHT_UP_PAUSE * 4,
            "{:?}",
            at[4] - at[0]
        );
        answered(r).await.busy("failed");

        // An export behind, then one caught up.
        let room = FakeRoom::new(
            |n| Step::Now(Ok(if n == 0 { behind(n) } else { base(n) })),
            |_| Step::Now(Ok(7)),
        );
        let plans = Plans::default();
        let r = attempts(&room, plans.always(undo), in_27s()).await;
        assert!(matches!(r, Ok(Restored::Undo)), "{r:?}");
        assert_eq!((room.exports(), plans.count()), (2, 1));
        assert_eq!(plans.seen(), [base(1).state]);
    }

    #[tokio::test]
    async fn an_export_at_another_epoch_is_moved() {
        let undo = || Ok(revert(RevertKind::Undo));

        let room = FakeRoom::new(|n| Step::Now(Ok(at_epoch(n, 1))), |_| Step::Now(Ok(7)));
        let plans = Plans::default();
        let r = attempts(&room, plans.always(undo), in_27s()).await;
        assert!(
            matches!(r, Err(RestoreFailure::View(ViewError::Moved))),
            "{r:?}"
        );
        assert_eq!((room.exports(), plans.count(), room.applies()), (1, 0, 0));
        answered(r)
            .await
            .error(StatusCode::CONFLICT, "history.moved", "failed");

        // An erase between two attempts.
        let room = FakeRoom::new(
            |n| Step::Now(Ok(at_epoch(n, if n == 0 { 0 } else { 1 }))),
            |_| Step::Now(Err(stale())),
        );
        let plans = Plans::default();
        let r = attempts(&room, plans.always(undo), in_27s()).await;
        assert!(
            matches!(r, Err(RestoreFailure::View(ViewError::Moved))),
            "{r:?}"
        );
        assert_eq!((room.exports(), plans.count(), room.applies()), (2, 1, 1));
    }

    #[tokio::test]
    async fn an_export_timeout_after_a_stale_apply_is_busy() {
        let room = FakeRoom::new(
            |n| {
                if n == 0 {
                    Step::Now(Ok(base(n)))
                } else {
                    Step::Now(Err(RoomCall::TimedOut))
                }
            },
            |_| Step::Now(Err(stale())),
        );
        let r = attempts(
            &room,
            Plans::default().always(|| Ok(revert(RevertKind::Undo))),
            in_27s(),
        )
        .await;
        assert!(matches!(r, Err(RestoreFailure::Busy)), "{r:?}");
        assert_eq!((room.exports(), room.applies()), (2, 1));
        answered(r).await.busy("busy");
    }

    #[tokio::test(start_paused = true)]
    async fn too_little_time_left_starts_no_export() {
        let room = FakeRoom::applying(|_| Ok(7));
        let plans = Plans::default();
        let r = attempts(
            &room,
            plans.always(|| Ok(revert(RevertKind::Undo))),
            Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert!(matches!(r, Err(RestoreFailure::Busy)), "{r:?}");
        assert_eq!((room.exports(), plans.count()), (0, 0));

        // A stale apply that leaves 1.5 s: no second export.
        let deadline = in_27s();
        let left = deadline - Duration::from_millis(1500);
        let room = FakeRoom::new(
            |n| Step::Now(Ok(base(n))),
            move |_| Step::Until(left, Err(stale())),
        );
        let plans = Plans::default();
        let r = attempts(
            &room,
            plans.always(|| Ok(revert(RevertKind::Undo))),
            deadline,
        )
        .await;
        assert!(matches!(r, Err(RestoreFailure::Busy)), "{r:?}");
        assert_eq!((room.exports(), plans.count(), room.applies()), (1, 1, 1));
        answered(r).await.busy("busy");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn other_roots_are_logged_when_applied() {
        const LINE: &str = "restore left other Y.Doc roots as they are";
        let with_other = || {
            Ok(RevertOutcome::Revert {
                update: vec![1],
                kind: RevertKind::Undo,
                other_roots_changed: vec!["other".to_string()],
            })
        };

        let (logs, _guard) = crate::auth::log_capture::capture();
        let room = FakeRoom::applying(|_| Ok(7));
        let r = attempts(&room, Plans::default().always(with_other), in_27s()).await;
        assert!(matches!(r, Ok(Restored::Undo)), "{r:?}");
        let text = logs.text();
        let line = text
            .lines()
            .find(|l| l.contains(LINE))
            .unwrap_or_else(|| panic!("no line: {text}"));
        assert!(line.contains("INFO"), "{line}");
        assert!(line.contains(&DOC.to_string()), "{line}");
        assert!(line.contains("target_entry_id=7"), "{line}");
        assert!(line.contains("roots=other"), "{line}");
        drop(_guard);

        // Nothing else changed: no line.
        let (logs, _guard) = crate::auth::log_capture::capture();
        let room = FakeRoom::applying(|_| Ok(7));
        let r = attempts(
            &room,
            Plans::default().always(|| Ok(revert(RevertKind::Undo))),
            in_27s(),
        )
        .await;
        assert!(matches!(r, Ok(Restored::Undo)), "{r:?}");
        assert!(!logs.text().contains(LINE), "{}", logs.text());
        drop(_guard);

        // Logged only when applied.
        let (logs, _guard) = crate::auth::log_capture::capture();
        let room = FakeRoom::applying(|_| Err(stale()));
        let r = attempts(&room, Plans::default().always(with_other), in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::Stale)), "{r:?}");
        assert!(!logs.text().contains(LINE), "{}", logs.text());
    }

    #[tokio::test]
    async fn a_room_acquire_that_fails_or_times_out() {
        let wait = Duration::from_millis(50);
        let channel = || mpsc::channel::<Event>(1).0;

        let r = LiveRoom::<()>::acquire_within(
            async { Err::<((), mpsc::Sender<Event>), _>("boom".to_string()) },
            wait,
        )
        .await;
        let r = r.map(|_| Restored::Unchanged);
        assert!(
            matches!(&r, Err(RestoreFailure::Failed(e)) if e.contains("boom")),
            "{r:?}"
        );
        answered(r)
            .await
            .error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "failed");

        // An acquire that never ends.
        let started = std::time::Instant::now();
        let r = LiveRoom::<()>::acquire_within(std::future::pending(), wait).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        let r = r.map(|_| Restored::Unchanged);
        assert!(matches!(r, Err(RestoreFailure::Busy)), "{r:?}");
        answered(r).await.busy("busy");

        // One the wait cut off runs to its end on its own task.
        let booted = Arc::new(AtomicBool::new(false));
        let flag = booted.clone();
        let tx = channel();
        let started = std::time::Instant::now();
        let r = LiveRoom::<()>::acquire_within(
            async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                flag.store(true, Ordering::SeqCst);
                Ok(((), tx))
            },
            wait,
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(r, Err(RestoreFailure::Busy)));
        let polled = tokio::time::timeout(Duration::from_secs(1), async {
            while !booted.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(polled.is_ok(), "the cut-off acquire never finished");

        // One that panics.
        let tx = channel();
        let r = LiveRoom::<()>::acquire_within(
            async move {
                if std::hint::black_box(true) {
                    panic!("test hook: the acquire panics");
                }
                Ok(((), tx))
            },
            wait,
        )
        .await;
        let r = r.map(|_| Restored::Unchanged);
        assert!(matches!(r, Err(RestoreFailure::Failed(_))), "{r:?}");
        answered(r)
            .await
            .error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "failed");
    }

    // -----------------------------------------------------------------------
    // LiveRoom over a hand-made channel

    /// What the scripted room does with a call's reply.
    enum Act<T> {
        Answer(T),
        Drop,
        Hold,
    }

    type Answers<T> = Box<dyn FnMut(usize) -> Act<T> + Send>;

    /// A room's event sender whose receiver answers the `n`th export and
    /// apply as `export(n)` and `apply(n)` say; with `close_after_export`
    /// it ends (dropping its receiver) after answering its first export.
    fn scripted(
        mut export: Answers<Result<RevertBase, EngineError>>,
        mut apply: Answers<Result<i64, RevertRefused>>,
        close_after_export: bool,
    ) -> mpsc::Sender<Event> {
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let mut held: Vec<Box<dyn std::any::Any + Send>> = Vec::new();
            let (mut exports, mut applies) = (0, 0);
            while let Some(event) = rx.recv().await {
                match event {
                    Event::ExportForRevert(reply) => {
                        match export(exports) {
                            Act::Answer(r) => {
                                let _ = reply.send(r);
                            }
                            Act::Drop => drop(reply),
                            Act::Hold => held.push(Box::new(reply)),
                        }
                        exports += 1;
                        if close_after_export {
                            return;
                        }
                    }
                    Event::ApplyRevert { reply, .. } => {
                        match apply(applies) {
                            Act::Answer(r) => {
                                let _ = reply.send(r);
                            }
                            Act::Drop => drop(reply),
                            Act::Hold => held.push(Box::new(reply)),
                        }
                        applies += 1;
                    }
                    _ => panic!("not a revert call"),
                }
            }
        });
        tx
    }

    fn exports_ok() -> Answers<Result<RevertBase, EngineError>> {
        Box::new(|n| Act::Answer(Ok(base(n))))
    }

    fn applies_ok() -> Answers<Result<i64, RevertRefused>> {
        Box::new(|_| Act::Answer(Ok(7)))
    }

    const CALL_WAIT: Duration = Duration::from_millis(50);

    fn live(tx: mpsc::Sender<Event>) -> LiveRoom<Arc<()>> {
        LiveRoom::new(Arc::new(()), tx, CALL_WAIT)
    }

    async fn apply_on(room: &LiveRoom<Arc<()>>) -> Result<i64, RoomCall> {
        room.apply(vec![1], 0, UpdateSource::Restore, BY, meta())
            .await
    }

    fn undo_plan() -> impl FnMut(Vec<u8>) -> PlanFuture {
        Plans::default().always(|| Ok(revert(RevertKind::Undo)))
    }

    #[tokio::test]
    async fn live_room_maps_a_closed_room_to_gone() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let room = live(tx);
        assert!(matches!(room.export().await, Err(RoomCall::Gone(_))));
        assert!(matches!(apply_on(&room).await, Err(RoomCall::Gone(_))));

        // An export the engine failed.
        let room = live(scripted(
            Box::new(|_| Act::Answer(Err(EngineError::Encode("broken".into())))),
            applies_ok(),
            false,
        ));
        assert!(matches!(room.export().await, Err(RoomCall::Gone(e)) if e.contains("broken")));

        // The room closes between the export and the apply.
        let tx = scripted(exports_ok(), applies_ok(), true);
        let probe = tx.clone();
        let room = live(tx);
        let plan = move |_live: Vec<u8>| -> PlanFuture {
            let probe = probe.clone();
            Box::pin(async move {
                while !probe.is_closed() {
                    tokio::task::yield_now().await;
                }
                Ok(revert(RevertKind::Undo))
            })
        };
        let r = attempts(&room, plan, in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::Failed(_))), "{r:?}");
        answered(r)
            .await
            .error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "failed");
    }

    #[tokio::test]
    async fn live_room_maps_a_dropped_reply_to_reply_lost() {
        let room = live(scripted(exports_ok(), Box::new(|_| Act::Drop), false));
        assert!(matches!(apply_on(&room).await, Err(RoomCall::ReplyLost)));
        let r = attempts(&room, undo_plan(), in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::TimedOutAfterSend)), "{r:?}");
        answered(r)
            .await
            .error(StatusCode::GATEWAY_TIMEOUT, "restore.timeout", "timeout");

        let room = live(scripted(Box::new(|_| Act::Drop), applies_ok(), false));
        assert!(matches!(room.export().await, Err(RoomCall::ReplyLost)));
        let r = attempts(&room, undo_plan(), in_27s()).await;
        assert!(matches!(r, Err(RestoreFailure::Failed(_))), "{r:?}");
        answered(r)
            .await
            .error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "failed");
    }

    #[tokio::test]
    async fn live_room_bounds_each_call_by_its_wait() {
        let within = |started: std::time::Instant| {
            let took = started.elapsed();
            assert!(
                took >= CALL_WAIT && took < Duration::from_secs(1),
                "{took:?}"
            );
        };
        let room = live(scripted(
            Box::new(|_| Act::Hold),
            Box::new(|_| Act::Hold),
            false,
        ));
        let started = std::time::Instant::now();
        assert!(matches!(room.export().await, Err(RoomCall::TimedOut)));
        within(started);
        let started = std::time::Instant::now();
        assert!(matches!(apply_on(&room).await, Err(RoomCall::TimedOut)));
        within(started);

        let room = live(scripted(Box::new(|_| Act::Hold), applies_ok(), false));
        let r = attempts(&room, undo_plan(), in_27s()).await;
        answered(r).await.busy("busy");

        let room = live(scripted(exports_ok(), Box::new(|_| Act::Hold), false));
        let r = attempts(&room, undo_plan(), in_27s()).await;
        answered(r)
            .await
            .error(StatusCode::GATEWAY_TIMEOUT, "restore.timeout", "timeout");
    }

    #[tokio::test]
    async fn live_room_passes_refusals_on_unchanged() {
        for refused in [
            RevertRefused::Stale,
            RevertRefused::NotCaughtUp,
            RevertRefused::Apply("x".into()),
        ] {
            let answer = refused.clone();
            let room = live(scripted(
                exports_ok(),
                Box::new(move |_| Act::Answer(Err(answer.clone()))),
                false,
            ));
            let r = apply_on(&room).await;
            assert!(
                matches!(&r, Err(RoomCall::Refused(got)) if *got == refused),
                "{r:?}"
            );
        }

        // Stale re-exports through the adapter.
        let exports = Arc::new(AtomicUsize::new(0));
        let counted = exports.clone();
        let room = live(scripted(
            Box::new(move |n| {
                counted.fetch_add(1, Ordering::SeqCst);
                Act::Answer(Ok(base(n)))
            }),
            Box::new(|n| {
                Act::Answer(if n == 0 {
                    Err(RevertRefused::Stale)
                } else {
                    Ok(7)
                })
            }),
            false,
        ));
        let r = attempts(&room, undo_plan(), in_27s()).await;
        assert!(matches!(r, Ok(Restored::Undo)), "{r:?}");
        assert_eq!(exports.load(Ordering::SeqCst), 2);

        // An apply refusal is not retried.
        let applies = Arc::new(AtomicUsize::new(0));
        let counted = applies.clone();
        let room = live(scripted(
            exports_ok(),
            Box::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Act::Answer(Err(RevertRefused::Apply("x".into())))
            }),
            false,
        ));
        let r = attempts(&room, undo_plan(), in_27s()).await;
        assert_eq!(applies.load(Ordering::SeqCst), 1);
        answered(r).await.error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "restore.apply",
            "apply_failed",
        );
    }

    #[tokio::test]
    async fn live_room_keeps_its_handle_until_dropped() {
        let h = Arc::new(());
        let room = LiveRoom::new(
            h.clone(),
            scripted(exports_ok(), applies_ok(), false),
            CALL_WAIT,
        );
        assert_eq!(Arc::strong_count(&h), 2);
        assert!(room.export().await.is_ok());
        assert_eq!(Arc::strong_count(&h), 2);
        assert!(apply_on(&room).await.is_ok());
        assert_eq!(Arc::strong_count(&h), 2);
        drop(room);
        assert_eq!(Arc::strong_count(&h), 1);

        let handle = h.clone();
        let tx = scripted(exports_ok(), applies_ok(), false);
        let room =
            LiveRoom::acquire_within(async move { Ok((handle, tx)) }, Duration::from_secs(1))
                .await
                .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(Arc::strong_count(&h), 2);
        assert!(room.export().await.is_ok());
        assert_eq!(Arc::strong_count(&h), 2);
        drop(room);
        assert_eq!(Arc::strong_count(&h), 1);
    }
}
