//! A past version of a page, as the history viewer shows it (M1.4): the
//! entry's replay rendered as ProseMirror JSON, plain ([`pm_json`]) or with
//! the entry's own changes marked ([`pm_json_diff`]), and the input caps
//! that keep a view within the builder's memory rule.
//!
//! [`render`], [`render_staged`], [`shows_changes`], [`merge_gap_at_end`]
//! and [`view_max_input_bytes`] are pure (no database, no tokio): a render
//! runs on the replay thread (`isolate::run_isolated`) under a compute
//! permit. [`Views`] is the service around them: it loads an entry's input
//! in a read transaction, bounds who renders what and when, renders, and
//! caches the serialized result.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use uuid::Uuid;
use yrs::IdSet;

use crate::diff::{DiffError, entry_diff};
use crate::isolate::{IsolateError, run_isolated};
use crate::pass::{Progress, WindowError, WindowInput, replay_window};
use crate::permits::{ComputeGuard, HistoryPermits, StallWatch};
use crate::pm_json::{RENDERER_VERSION, pm_json, pm_json_diff};
use crate::replay::{ReplayMode, Replayer};
use crate::sessions::EntryKind;
use crate::store::{self, StoreError, ViewEntry, ViewLoaded};

/// M1.3's doc-memory factor F, which `DEFAULT_HISTORY_REPLAY_MAX_BYTES`
/// was set by: a build's peak heap plus its input, over its input, at
/// most 40, so `workers × 40 × cap` bounds the builder's replay memory.
pub const BUILD_MEMORY_FACTOR: u64 = 40;

/// The Show changes view's memory factor: (input + peak heap above the
/// start) / input of a `changes=1` render, which holds the skip-GC replay,
/// the union doc, `D_lo`, the JSON `Value` and its serialization at once.
/// Set by M1.4 Task 3's spike (`cargo run --release -p knot-history
/// --example replay_throughput 7`).
///
/// On workload 6's 4 MiB pages, a session of a few rows measured 59.7
/// (interleaved one-character typing) and 5.1 (pastes). But the render
/// emits one text node per changed *item*: yrs 0.27.3's `DiffAssembler`
/// starts a chunk at every added or removed item (`types/text.rs:617-631`),
/// and each node costs about 3.5 KiB of heap. So a session whose one row
/// deletes the whole typing page (400,020 struck text nodes) peaked
/// 1,321 MiB above its start, F = 317.1 (321.1 at 1.36 MiB); one that
/// undoes one client's typing (every third character) measured 173.6; and
/// `tests/diff_memory.rs`'s page, whose client ids 1-3 encode denser than
/// real 32-bit ids, with a third of it highlighted, 327.5. Pastes stay
/// near 5 in every shape.
///
/// F grows on smaller pages, because fixed costs weigh more. So the factor
/// is set where the rule binds, on a typing page of the diff cap's own
/// size. With 360 the diff cap of the default 4 MiB replay cap is
/// 4 MiB × 40 / 360 = 0.44 MiB. There, the whole-page deletion measured
/// F = 338.5 and held 151.8 MiB (input + peak), within the builder's
/// 40 × 4 MiB = 160 MiB. 330 would give a 0.48 MiB cap, where it measured
/// 343.4 and 168.6 MiB.
///
/// The render time grows with texts × deleted blocks. The whole-page
/// deletion took 26.2 s at 4.18 MiB and 2.8 s at 1.36 MiB, but only 0.38 s
/// at the diff cap, well under the 10 s the spike allows.
pub const VIEW_MEMORY_FACTOR: u64 = 360;

/// The plain view's and the restore's memory factor (M1.4 Task 3's spike,
/// as [`VIEW_MEMORY_FACTOR`]), so the plain cap is the full replay cap.
///
/// On workload 6's 4 MiB pages (interleaved typing / pastes): a plain view
/// of a session's window measured 39.1 / 4.6; a baseline view of the state
/// 39.1 / 4.6; and a restore (the replay, `encode_state`, and the Markdown
/// round trip of `apply_update`, `to_markdown::serialise` and
/// `from_markdown::parse`) 39.1 / 6.7. A 4 MiB page whose every other word
/// is bold, one text node per word, measured 16.4. The decoded checkpoint
/// dominates, as in M1.3's F. Each took at most 0.14 s, well within
/// `RESTORE_TIMEOUT` − 5 s.
pub const PLAIN_MEMORY_FACTOR: u64 = 40;

/// The memory factor of a view or restore of a baseline that may be
/// shifted (M1.4 Task 8's measurement, workload 7's (h) and (i)): its
/// bytes are cut to their trusted prefix first ([`state_input`]), which
/// builds a skip-GC scratch doc and a fresh GC doc of its own before the
/// render builds one. On workload 6's 4 MiB typing page with the knot-crdt
/// fix's overlapping Skips merged into it as yrs encodes them (4.18 MiB),
/// the view and the restore (the replay, `encode_state` and the Markdown
/// round trip) each measured 43.40, against 39.1 for the same page's
/// unshifted state; rounded up, 44. Only `baseline` entries pay it (the
/// rollout entry and every window keep the plain cap):
/// [`baseline_max_input_bytes`].
pub const SHIFTED_MEMORY_FACTOR: u64 = 44;

/// The largest baseline state a view or restore of a `baseline` entry may
/// load, given the plain cap: `max_plain_input_bytes ×
/// max(PLAIN_MEMORY_FACTOR, BUILD_MEMORY_FACTOR) /
/// max(SHIFTED_MEMORY_FACTOR, BUILD_MEMORY_FACTOR)`, so cutting a shifted
/// baseline to its prefix stays within the builder's memory rule. 3.81 MiB
/// at the default 4 MiB replay cap.
pub fn baseline_max_input_bytes(max_plain_input_bytes: i64) -> i64 {
    let cap = i128::from(max_plain_input_bytes)
        * i128::from(PLAIN_MEMORY_FACTOR.max(BUILD_MEMORY_FACTOR))
        / i128::from(SHIFTED_MEMORY_FACTOR.max(BUILD_MEMORY_FACTOR));
    i64::try_from(cap).unwrap_or(i64::MAX)
}

/// The largest replay input a view may load (the diff cap with `changes`,
/// the plain cap without): `replay_max_bytes × BUILD_MEMORY_FACTOR /
/// max(F, BUILD_MEMORY_FACTOR)` with F = [`VIEW_MEMORY_FACTOR`] or
/// [`PLAIN_MEMORY_FACTOR`], so a view holding a compute permit takes no
/// more than the builder's `40 × replay_max_bytes`. Never above
/// `replay_max_bytes`; saturates at `i64::MAX` (the database's sum type).
pub fn view_max_input_bytes(replay_max_bytes: u64, changes: bool) -> i64 {
    let f = if changes {
        VIEW_MEMORY_FACTOR
    } else {
        PLAIN_MEMORY_FACTOR
    };
    let cap = u128::from(replay_max_bytes) * u128::from(BUILD_MEMORY_FACTOR)
        / u128::from(f.max(BUILD_MEMORY_FACTOR));
    i64::try_from(cap).unwrap_or(i64::MAX)
}

/// What a view replays.
// Built once per render and moved into the replay thread; a box would only
// add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum ViewInput {
    /// A baseline's state, rendered from [`state_input`] (its trusted
    /// prefix when it may be shifted).
    State(Vec<u8>),
    /// An entry's window: the checkpoint at or before its start, the rows
    /// through its end.
    Window(WindowInput),
}

/// Why a version could not be rendered at all.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// The baseline state does not decode or apply.
    #[error("the state does not apply: {0}")]
    State(String),
    /// The window replay failed or was cancelled.
    #[error("the replay failed: {0}")]
    Window(#[from] WindowError),
    /// The diff step was cancelled ([`DiffError::Cancelled`], the only
    /// variant it carries: every other diff failure falls back to the plain
    /// render, [`Rendered::diff_error`]).
    #[error("the render failed: {0}")]
    Diff(#[from] DiffError),
}

/// A rendered version.
#[derive(Debug)]
pub struct Rendered {
    /// ProseMirror JSON, `{"type": "doc", "content": [...]}`.
    pub doc: Value,
    /// Whether the entry's own changes are marked in `doc`.
    pub changes: bool,
    /// Why the diff step failed or panicked, when changes were asked for
    /// and `doc` is the plain render instead.
    pub diff_error: Option<String>,
}

/// Test hooks; production code leaves them at their default.
#[doc(hidden)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RenderHooks {
    /// Panics inside the diff step.
    pub panic_in_diff: bool,
    /// Sleeps inside the diff step, before the union doc is built; like
    /// the union build it stands for, the sleep does not see a cancel.
    pub hold_in_diff: Duration,
}

/// Renders `input` ([`render_staged`] with a stage nobody reads).
pub fn render(
    input: ViewInput,
    changes: Option<(IdSet, IdSet)>,
    hooks: RenderHooks,
) -> Result<Rendered, RenderError> {
    render_staged(input, changes, hooks, &StageCell::default())
}

/// What a render thread is doing, read by the async side when the render
/// times out (Rulings on skipping the diff): a hang in the diff step only
/// keeps the Show changes render from being tried again; a hang anywhere
/// else blocks the version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RenderStage {
    /// The replay: `replay_window`, or `Replayer::from_state`.
    Replay = 0,
    /// A `pm_json` walk (the fallback after a failed diff included) and
    /// its serialization.
    Plain = 1,
    /// `entry_diff`, `D_lo`, `pm_json_diff` and the serialization of their
    /// output.
    Diff = 2,
}

/// A render's [`RenderStage`], shared between its thread and the async
/// side; [`RenderStage::Replay`] until set.
#[derive(Clone, Default, Debug)]
pub struct StageCell(Arc<AtomicU8>);

impl StageCell {
    pub fn set(&self, s: RenderStage) {
        self.0.store(s as u8, Ordering::SeqCst);
    }

    pub fn get(&self) -> RenderStage {
        match self.0.load(Ordering::SeqCst) {
            1 => RenderStage::Plain,
            2 => RenderStage::Diff,
            _ => RenderStage::Replay,
        }
    }
}

/// Renders `input`, publishing its stage through `stage`. A
/// [`ViewInput::State`] renders plain from [`state_input`] (`changes` is
/// ignored). A
/// [`ViewInput::Window`] is replayed with its own `progress`; with
/// `changes` (the entry's insert and delete sets) the diff step
/// ([`entry_diff`], then [`pm_json_diff`]) runs under `catch_unwind`, and
/// when it fails, other than by a cancel, or panics, the version is the
/// plain render of the same replay with the reason in
/// [`Rendered::diff_error`]: the diff step only reads the replay, so the
/// replay is intact after the unwind.
pub fn render_staged(
    input: ViewInput,
    changes: Option<(IdSet, IdSet)>,
    hooks: RenderHooks,
    stage: &StageCell,
) -> Result<Rendered, RenderError> {
    stage.set(RenderStage::Replay);
    let window = match input {
        ViewInput::State(bytes) => {
            let bytes = owned_state_input(bytes)?;
            let r = Replayer::from_state(ReplayMode::KeepDeleted, &bytes)
                .map_err(|e| RenderError::State(e.to_string()))?;
            stage.set(RenderStage::Plain);
            return Ok(plain(r.doc(), None));
        }
        ViewInput::Window(w) => w,
    };
    let progress = window.progress.clone();
    let replay = replay_window(window)?;
    let h = replay.doc();
    let Some((inserted, deleted)) = changes else {
        stage.set(RenderStage::Plain);
        return Ok(plain(h, None));
    };
    stage.set(RenderStage::Diff);
    let diffed = catch_unwind(AssertUnwindSafe(|| {
        if hooks.panic_in_diff {
            panic!("test hook: panic in the diff");
        }
        if !hooks.hold_in_diff.is_zero() {
            std::thread::sleep(hooks.hold_in_diff);
        }
        let d = entry_diff(h, &inserted, &deleted)?;
        pm_json_diff(h, &d, &progress)
    }));
    match diffed {
        Ok(Ok(doc)) => Ok(Rendered {
            doc,
            changes: true,
            diff_error: None,
        }),
        Ok(Err(DiffError::Cancelled)) => Err(RenderError::Diff(DiffError::Cancelled)),
        Ok(Err(e)) => {
            stage.set(RenderStage::Plain);
            Ok(plain(h, Some(e.to_string())))
        }
        Err(panic) => {
            stage.set(RenderStage::Plain);
            Ok(plain(
                h,
                Some(format!("the diff panicked: {}", panic_message(&*panic))),
            ))
        }
    }
}

/// The state a baseline is viewed and restored from: when the bytes
/// [`may_be_shifted`](crate::replay::may_be_shifted) (a baseline a room
/// wrote before the knot-crdt fix), their
/// [`trusted_prefix`](crate::replay::trusted_prefix), else the bytes as
/// they are. Runs on the replay thread, in stage [`RenderStage::Replay`];
/// it returns bytes only, so every doc the two checks build is dropped
/// before the render builds its own.
pub fn state_input(bytes: &[u8]) -> Result<Vec<u8>, RenderError> {
    Ok(shifted_prefix(bytes)?.unwrap_or_else(|| bytes.to_vec()))
}

/// [`state_input`] for a caller that owns the bytes: they come back as
/// they are, not copied, when they cannot be shifted (a copy of a 4 MiB
/// state would add 1 to the plain view's memory factor), and are dropped
/// once the prefix replaces them.
fn owned_state_input(bytes: Vec<u8>) -> Result<Vec<u8>, RenderError> {
    Ok(shifted_prefix(&bytes)?.unwrap_or(bytes))
}

/// The trusted prefix of `bytes` when they may be shifted; `None` when
/// they cannot be.
fn shifted_prefix(bytes: &[u8]) -> Result<Option<Vec<u8>>, RenderError> {
    if !crate::replay::may_be_shifted(bytes) {
        return Ok(None);
    }
    crate::replay::trusted_prefix(bytes)
        .map(Some)
        .ok_or_else(|| RenderError::State("the baseline's trusted prefix does not decode".into()))
}

fn plain(h: &yrs::Doc, diff_error: Option<String>) -> Rendered {
    Rendered {
        doc: pm_json(h),
        changes: false,
        diff_error,
    }
}

/// A panic payload's message, as `std`'s default hook prints it.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a non-string payload")
}

// ---------------------------------------------------------------------------
// The views service
// ---------------------------------------------------------------------------

/// How long a view or restore waits for its principal's render slot and a
/// compute permit, together.
pub const VIEW_PERMIT_WAIT: Duration = Duration::from_secs(5);
/// How long a view's render may run (under the router's 30 s with
/// [`VIEW_PERMIT_WAIT`] and the loads).
pub const VIEW_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a restore's replay and Markdown round trip may run (with the
/// permit wait and the room's `RESTORE_ROOM_WAIT`, under the router's
/// 30 s).
pub const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);
/// The view cache's size, in serialized bytes.
pub const VIEW_CACHE_BYTES: u64 = 32 * 1024 * 1024;
/// How long a cached view, and the record of a timed-out render, lasts.
pub const VIEW_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Cache-miss renders (views and restores together) one principal may have
/// inside their deadline (Rulings on rate limits).
pub const PRINCIPAL_RENDERS: usize = 1;

/// Keys `Views::timed_out` holds at most (a key whose thread still runs is
/// kept in `stalled_keys` as well, so the cap never drops it).
const TIMED_OUT_KEYS: u64 = 1_024;
/// The principals map is pruned of dead slots past this many entries.
const PRINCIPALS_PRUNE_AT: usize = 1_024;

const EARLIER_TIMEOUT: &str = "an earlier render of this version timed out";
const SKIPPED_BASELINE: &str = "the history builder skipped this baseline";

/// A Show changes view (`changes=1`) is offered for every kind but these.
pub fn shows_changes(kind: EntryKind) -> bool {
    !matches!(kind, EntryKind::Baseline | EntryKind::Erase)
}

/// Whether an entry's window replay merges the gap state at its `end_rev`
/// ("After M1.3"): for the gap entry itself, and for a session the gap was
/// credited to.
pub fn merge_gap_at_end(kind: EntryKind, meta: &Value, end_rev: i64) -> bool {
    kind == EntryKind::Gap || meta.get("gap_credit_rev").and_then(Value::as_i64) == Some(end_rev)
}

/// What bounds the views of one process.
#[derive(Clone)]
pub struct ViewLimits {
    /// A `changes=1` view tries the diff only up to this input.
    pub max_changes_input_bytes: i64,
    /// Every view and restore answers `TooLarge` above this input (a
    /// `baseline` entry above [`baseline_max_input_bytes`] of it).
    pub max_plain_input_bytes: i64,
    pub permit_wait: Duration,
    pub timeout: Duration,
    pub restore_timeout: Duration,
    pub cache_bytes: u64,
    pub cache_ttl: Duration,
    #[doc(hidden)]
    pub hooks: ViewHooks,
}

impl ViewLimits {
    pub fn from_config(cfg: &knot_config::Config) -> ViewLimits {
        ViewLimits {
            max_changes_input_bytes: view_max_input_bytes(cfg.history_replay_max_bytes, true),
            max_plain_input_bytes: view_max_input_bytes(cfg.history_replay_max_bytes, false),
            permit_wait: VIEW_PERMIT_WAIT,
            timeout: VIEW_TIMEOUT,
            restore_timeout: RESTORE_TIMEOUT,
            cache_bytes: VIEW_CACHE_BYTES,
            cache_ttl: VIEW_CACHE_TTL,
            hooks: ViewHooks::default(),
        }
    }
}

impl Default for ViewLimits {
    fn default() -> ViewLimits {
        ViewLimits::from_config(&knot_config::Config::default())
    }
}

/// Test hooks; production code leaves them at their default.
#[doc(hidden)]
#[derive(Default, Clone)]
pub struct ViewHooks {
    /// Sleeps on the render thread before the replay (stage `Replay`).
    pub hold_render: Duration,
    /// Panics on the render thread before the replay.
    pub panic_render: bool,
    /// [`RenderHooks::panic_in_diff`].
    pub panic_in_diff: bool,
    /// [`RenderHooks::hold_in_diff`].
    pub hold_in_diff: Duration,
    /// Counts render threads started (views and restores).
    pub renders: Option<Arc<AtomicUsize>>,
}

/// A rendered version: ProseMirror JSON, serialized.
#[derive(Debug)]
pub struct RenderedView {
    pub entry_id: i64,
    pub kind: EntryKind,
    /// Whether the entry's own changes are marked in `doc`.
    pub changes: bool,
    pub doc: Bytes,
}

/// Why a view or restore was not served.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ViewError {
    /// No such entry on this page, or what it replays is gone (a rebuild).
    #[error("no such history entry")]
    NotFound,
    /// The page's history was built by an older builder version, until
    /// its rebuild.
    #[error("the page's history is being rebuilt")]
    Rebuilding,
    /// The replay input is over the plain cap.
    #[error("the version is too large to render")]
    TooLarge,
    /// No render slot of the principal, or no compute permit, within the
    /// permit wait.
    #[error("no replay slot within the wait")]
    Busy,
    /// The replay or render failed, panicked, or is known to hang.
    #[error("the version could not be rendered: {0}")]
    Failed(String),
    /// The render did not finish within its timeout.
    #[error("the render timed out")]
    TimedOut,
    #[error("store: {0}")]
    Store(String),
}

impl ViewError {
    /// Its `knot_history_views_total` outcome.
    fn outcome(&self) -> &'static str {
        match self {
            ViewError::NotFound => "not_found",
            ViewError::Rebuilding => "rebuilding",
            ViewError::TooLarge => "too_large",
            ViewError::Busy => "busy",
            ViewError::Failed(_) | ViewError::Store(_) => "failed",
            ViewError::TimedOut => "timeout",
        }
    }
}

fn store_error(e: StoreError) -> ViewError {
    ViewError::Store(e.to_string())
}

/// A restore's input: what `finish` made of the entry's state, and the
/// entry as read in the transaction that loaded it.
pub struct Restorable<T> {
    pub value: T,
    pub kind: EntryKind,
    /// As stored: the genesis rev for the rollout entry, `None` for a
    /// legacy baseline.
    pub end_rev: Option<i64>,
    pub ended_at: DateTime<Utc>,
}

/// What the cache, and the record of timed-out renders, are keyed by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ViewKey {
    entry_id: i64,
    updated_at: DateTime<Utc>,
    content_epoch: i32,
    renderer: u32,
    variant: ViewVariant,
}

/// The cache holds `Plain` and `Changes` only; `Restore` is a record key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ViewVariant {
    Plain,
    Changes,
    Restore,
}

impl ViewKey {
    fn of(e: &ViewEntry, variant: ViewVariant) -> ViewKey {
        ViewKey {
            entry_id: e.id,
            updated_at: e.updated_at,
            content_epoch: e.content_epoch,
            renderer: RENDERER_VERSION,
            variant,
        }
    }

    fn with(&self, variant: ViewVariant) -> ViewKey {
        ViewKey {
            variant,
            ..self.clone()
        }
    }

    /// The key a render of this key that hung in `stage` is recorded
    /// under: a Show changes render that hung in its diff step blocks only
    /// the diff, one that hung anywhere else the plain version; a restore
    /// blocks only restores.
    fn hung_in(&self, stage: RenderStage) -> ViewKey {
        match (self.variant, stage) {
            (ViewVariant::Restore, _) | (ViewVariant::Changes, RenderStage::Diff) => self.clone(),
            _ => self.with(ViewVariant::Plain),
        }
    }
}

/// The record of renders that timed out, and the stalled gauge.
#[derive(Clone)]
struct Records {
    permits: HistoryPermits,
    /// Recorded keys, for `VIEW_CACHE_TTL` after their thread ended.
    timed_out: moka::sync::Cache<ViewKey, ()>,
    /// Recorded keys whose thread still runs.
    stalled_keys: Arc<Mutex<HashSet<ViewKey>>>,
}

impl Records {
    fn stalled_keys(&self) -> std::sync::MutexGuard<'_, HashSet<ViewKey>> {
        self.stalled_keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn recorded(&self, k: &ViewKey) -> bool {
        self.timed_out.contains_key(k) || self.stalled_keys().contains(k)
    }

    /// A render of `k` timed out; its thread still runs.
    fn record(&self, k: ViewKey) {
        self.stalled_keys().insert(k.clone());
        self.timed_out.insert(k, ());
    }

    /// The thread of a render recorded under `k` ended: the record lasts
    /// `VIEW_CACHE_TTL` from now.
    fn thread_ended(&self, k: &ViewKey) {
        self.timed_out.insert(k.clone(), ());
        self.stalled_keys().remove(k);
    }

    fn set_stalled_gauge(&self) {
        metrics::gauge!("knot_history_replay_threads_stalled").set(self.permits.stalled() as f64);
    }

    /// A render of `key` hung past its timeout in `stage`: counted, logged
    /// and recorded; returns the key recorded.
    fn record_timeout(&self, key: &ViewKey, stage: RenderStage, dropped: bool) -> ViewKey {
        let recorded = key.hung_in(stage);
        self.record(recorded.clone());
        metrics::counter!("knot_history_replay_timeouts_total", "phase" => "view").increment(1);
        tracing::error!(
            entry_id = key.entry_id,
            variant = ?key.variant,
            ?stage,
            dropped,
            "history view timed out"
        );
        recorded
    }
}

/// A principal's render slot while a render holds it: emptied when the
/// render's thread ends or at the render's deadline, whichever comes first.
#[derive(Clone)]
struct PrincipalSlot(Arc<Mutex<Option<OwnedSemaphorePermit>>>);

impl PrincipalSlot {
    fn new(permit: OwnedSemaphorePermit) -> PrincipalSlot {
        PrincipalSlot(Arc::new(Mutex::new(Some(permit))))
    }

    fn empty(&self) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

/// Empties the slot when the render thread's closure ends (or unwinds).
struct SlotRelease(PrincipalSlot);

impl Drop for SlotRelease {
    fn drop(&mut self) {
        self.0.empty();
    }
}

/// A render that may start: its compute permit, its principal's slot, and
/// when the permit was granted.
struct Granted {
    guard: ComputeGuard,
    slot: PrincipalSlot,
    started: Instant,
}

/// Held by a render's async side while its thread runs. Dropped armed (the
/// request was dropped), it cancels the render, counts the thread stalled
/// and, while the thread runs, hands it to a stall watcher (Rulings on
/// cancellation).
struct Abandon {
    armed: bool,
    progress: Progress,
    watch: StallWatch,
    records: Records,
    key: ViewKey,
    stage: StageCell,
    slot: PrincipalSlot,
    deadline: Instant,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.progress.cancel();
        self.watch.timed_out();
        self.records.set_stalled_gauge();
        if !self.watch.running() {
            return;
        }
        // No runtime (a test without one): nothing to watch with.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(watch_dropped(
                self.watch.clone(),
                self.records.clone(),
                self.key.clone(),
                self.stage.clone(),
                self.slot.clone(),
                self.deadline,
            ));
        }
    }
}

/// The stall watcher of a render whose request was dropped: at the
/// render's deadline, if its thread still runs, it records the timeout as
/// the request would have and frees the principal's slot; when the thread
/// ends, it sets the stalled gauge and starts the record's TTL.
async fn watch_dropped(
    watch: StallWatch,
    records: Records,
    key: ViewKey,
    stage: StageCell,
    slot: PrincipalSlot,
    deadline: Instant,
) {
    let ended = watch.ended();
    tokio::pin!(ended);
    let mut recorded = None;
    tokio::select! {
        () = &mut ended => {}
        () = tokio::time::sleep_until(deadline) => {
            if watch.running() {
                recorded = Some(records.record_timeout(&key, stage.get(), true));
            }
            slot.empty();
            ended.await;
        }
    }
    if let Some(k) = recorded {
        records.thread_ended(&k);
    }
    records.set_stalled_gauge();
}

/// The stall watcher of a render that timed out: when its thread ends, it
/// sets the stalled gauge and starts the record's TTL.
async fn watch_timed_out(watch: StallWatch, records: Records, recorded: ViewKey) {
    watch.ended().await;
    records.thread_ended(&recorded);
    records.set_stalled_gauge();
}

/// The history views of one process: an entry's version rendered as
/// ProseMirror JSON (plain or with its changes), cached, and the input of
/// its restore.
pub struct Views {
    pool: PgPool,
    permits: HistoryPermits,
    limits: ViewLimits,
    cache: moka::future::Cache<ViewKey, Arc<RenderedView>>,
    records: Records,
    /// Each principal's render slot, while someone holds or waits for it.
    principals: Mutex<HashMap<Uuid, Weak<Semaphore>>>,
}

impl Views {
    pub fn new(pool: PgPool, permits: HistoryPermits, limits: ViewLimits) -> Views {
        let cache = moka::future::Cache::builder()
            .max_capacity(limits.cache_bytes)
            .weigher(|_: &ViewKey, v: &Arc<RenderedView>| {
                u32::try_from(v.doc.len()).unwrap_or(u32::MAX)
            })
            .time_to_live(limits.cache_ttl)
            .build();
        let records = Records {
            permits: permits.clone(),
            timed_out: moka::sync::Cache::builder()
                .max_capacity(TIMED_OUT_KEYS)
                .time_to_live(limits.cache_ttl)
                .build(),
            stalled_keys: Arc::new(Mutex::new(HashSet::new())),
        };
        Views {
            pool,
            permits,
            limits,
            cache,
            records,
            principals: Mutex::new(HashMap::new()),
        }
    }

    /// Entry `entry_id` of page `doc_id` as it was right after it, with its
    /// own changes marked when `changes` is asked and possible. `principal`
    /// is the caller (`AuthContext::user_id`), whose cache-miss renders run
    /// one at a time.
    pub async fn view(
        &self,
        principal: Uuid,
        doc_id: Uuid,
        entry_id: i64,
        changes: bool,
    ) -> Result<Arc<RenderedView>, ViewError> {
        let (result, outcome) = match self
            .view_counted(principal, doc_id, entry_id, changes)
            .await
        {
            Ok((view, outcome)) => (Ok(view), outcome),
            Err(e) => {
                let outcome = e.outcome();
                (Err(e), outcome)
            }
        };
        metrics::counter!("knot_history_views_total", "outcome" => outcome).increment(1);
        result
    }

    async fn view_counted(
        &self,
        principal: Uuid,
        doc_id: Uuid,
        entry_id: i64,
        changes: bool,
    ) -> Result<(Arc<RenderedView>, &'static str), ViewError> {
        let entry = self.entry(doc_id, entry_id).await?;
        let plain = ViewKey::of(&entry, ViewVariant::Plain);
        if self.records.recorded(&plain) {
            return Err(ViewError::Failed(EARLIER_TIMEOUT.to_string()));
        }
        let asked = changes && shows_changes(entry.kind);
        let diff = plain.with(ViewVariant::Changes);
        let mut effective = asked && !entry.count_failed() && !self.records.recorded(&diff);
        if effective {
            let bytes = self.input_bytes(&entry).await?;
            effective = bytes <= self.limits.max_changes_input_bytes;
        }
        let lowered = asked && !effective;
        let key = if effective { diff } else { plain };
        if let Some(hit) = self.cache.get(&key).await {
            return Ok((hit, if lowered { "no_changes" } else { "cached" }));
        }
        let deadline = Instant::now() + self.limits.permit_wait;
        let slot = self.slot_within(principal, deadline).await?;
        let diff_failed = AtomicBool::new(false);
        let init = self.render_view(entry, key.clone(), effective, slot, deadline, &diff_failed);
        let got = self
            .cache
            .entry(key)
            .or_try_insert_with(init)
            .await
            .map_err(|e| (*e).clone())?;
        let outcome = if lowered {
            "no_changes"
        } else if !got.is_fresh() {
            "cached"
        } else if diff_failed.load(Ordering::SeqCst) {
            "diff_failed"
        } else {
            "rendered"
        };
        Ok((got.into_value(), outcome))
    }

    /// The cache's init for `key`: the size check, a compute permit, the
    /// load and the render. Records the render's time, or its timeout.
    async fn render_view(
        &self,
        entry: ViewEntry,
        key: ViewKey,
        changes: bool,
        slot: OwnedSemaphorePermit,
        deadline: Instant,
        diff_failed: &AtomicBool,
    ) -> Result<Arc<RenderedView>, ViewError> {
        self.input_bytes(&entry).await?;
        let granted = self.compute_until(slot, deadline).await?;
        let (input, ins, del, _) = self.load(&entry).await?;
        let sets = changes.then_some((ins, del));
        let hooks = RenderHooks {
            panic_in_diff: self.limits.hooks.panic_in_diff,
            hold_in_diff: self.limits.hooks.hold_in_diff,
        };
        let stage = StageCell::default();
        let thread_stage = stage.clone();
        let started = granted.started;
        let (doc, rendered_changes, diff_error) = self
            .run(
                granted,
                key,
                self.limits.timeout,
                input,
                stage,
                move |input| {
                    let r = render_staged(input, sets, hooks, &thread_stage)
                        .map_err(|e| e.to_string())?;
                    let doc = serde_json::to_vec(&r.doc).map_err(|e| e.to_string())?;
                    Ok((doc, r.changes, r.diff_error))
                },
            )
            .await?;
        metrics::histogram!("knot_history_view_seconds").record(started.elapsed().as_secs_f64());
        if let Some(reason) = diff_error {
            diff_failed.store(true, Ordering::SeqCst);
            tracing::warn!(
                entry_id = entry.id,
                reason,
                "history view rendered without changes"
            );
        }
        Ok(Arc::new(RenderedView {
            entry_id: entry.id,
            kind: entry.kind,
            changes: rendered_changes,
            doc: Bytes::from(doc),
        }))
    }

    /// The input of restoring entry `entry_id`: its state at its end,
    /// replayed as its plain view is (without the cache) and handed to
    /// `finish` on the replay thread, within [`ViewLimits::restore_timeout`].
    pub async fn restore_input<T: Send + 'static>(
        &self,
        principal: Uuid,
        doc_id: Uuid,
        entry_id: i64,
        finish: impl FnOnce(&[u8]) -> Result<T, String> + Send + 'static,
    ) -> Result<Restorable<T>, ViewError> {
        let entry = self.entry(doc_id, entry_id).await?;
        let plain = ViewKey::of(&entry, ViewVariant::Plain);
        let key = plain.with(ViewVariant::Restore);
        if self.records.recorded(&plain) || self.records.recorded(&key) {
            return Err(ViewError::Failed(EARLIER_TIMEOUT.to_string()));
        }
        self.input_bytes(&entry).await?;
        let deadline = Instant::now() + self.limits.permit_wait;
        let slot = self.slot_within(principal, deadline).await?;
        let granted = self.compute_until(slot, deadline).await?;
        let (input, _, _, loaded) = self.load(&entry).await?;
        let stage = StageCell::default();
        let value = self
            .run(
                granted,
                key,
                self.limits.restore_timeout,
                input,
                stage,
                move |input| {
                    let replayer = match input {
                        ViewInput::State(bytes) => {
                            let bytes = owned_state_input(bytes).map_err(|e| e.to_string())?;
                            Replayer::from_state(ReplayMode::KeepDeleted, &bytes)
                                .map_err(|e| format!("the state does not apply: {e}"))?
                        }
                        ViewInput::Window(w) => replay_window(w).map_err(|e| e.to_string())?,
                    };
                    let state = replayer.encode_state();
                    drop(replayer);
                    finish(&state)
                },
            )
            .await?;
        Ok(Restorable {
            value,
            kind: loaded.kind,
            end_rev: loaded.end_rev,
            ended_at: loaded.ended_at,
        })
    }

    /// The entry, unless the page is being rebuilt or it is a baseline the
    /// builder skipped.
    async fn entry(&self, doc_id: Uuid, entry_id: i64) -> Result<ViewEntry, ViewError> {
        let entry = store::load_view_entry(&self.pool, doc_id, entry_id)
            .await
            .map_err(store_error)?
            .ok_or(ViewError::NotFound)?;
        if entry.stale {
            return Err(ViewError::Rebuilding);
        }
        if entry.skipped {
            return Err(ViewError::Failed(SKIPPED_BASELINE.to_string()));
        }
        Ok(entry)
    }

    /// The entry's replay input size, within its cap.
    async fn input_bytes(&self, entry: &ViewEntry) -> Result<i64, ViewError> {
        match store::view_input_bytes(&self.pool, entry)
            .await
            .map_err(store_error)?
        {
            None => Err(ViewError::NotFound),
            Some(n) if n > self.max_input_bytes(entry) => Err(ViewError::TooLarge),
            Some(n) => Ok(n),
        }
    }

    /// The plain cap, or for a baseline's state (cut to its trusted prefix
    /// when it may be shifted) the lower [`baseline_max_input_bytes`].
    fn max_input_bytes(&self, entry: &ViewEntry) -> i64 {
        if entry.kind == EntryKind::Baseline && entry.baseline_id.is_some() {
            baseline_max_input_bytes(self.limits.max_plain_input_bytes)
        } else {
            self.limits.max_plain_input_bytes
        }
    }

    /// The principal's render slot by `deadline`.
    async fn slot_within(
        &self,
        principal: Uuid,
        deadline: Instant,
    ) -> Result<OwnedSemaphorePermit, ViewError> {
        let slot = {
            let mut principals = self
                .principals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match principals.get(&principal).and_then(Weak::upgrade) {
                Some(s) => s,
                None => {
                    let s = Arc::new(Semaphore::new(PRINCIPAL_RENDERS));
                    principals.insert(principal, Arc::downgrade(&s));
                    if principals.len() > PRINCIPALS_PRUNE_AT {
                        principals.retain(|_, w| w.strong_count() > 0);
                    }
                    s
                }
            }
        };
        match tokio::time::timeout_at(deadline, slot.acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(ViewError::Busy),
        }
    }

    /// A compute permit by `deadline`, for the render holding `slot`.
    async fn compute_until(
        &self,
        slot: OwnedSemaphorePermit,
        deadline: Instant,
    ) -> Result<Granted, ViewError> {
        let wait = deadline.saturating_duration_since(Instant::now());
        let guard = self
            .permits
            .compute_within(wait)
            .await
            .ok_or(ViewError::Busy)?;
        Ok(Granted {
            guard,
            slot: PrincipalSlot::new(slot),
            started: Instant::now(),
        })
    }

    /// The entry's replay input, its id sets and the entry as the load
    /// read it.
    async fn load(
        &self,
        entry: &ViewEntry,
    ) -> Result<(ViewInput, IdSet, IdSet, ViewEntry), ViewError> {
        match store::load_view_input(
            &self.pool,
            entry.doc_id,
            entry.id,
            self.max_input_bytes(entry),
        )
        .await
        .map_err(store_error)?
        {
            ViewLoaded::NotFound => Err(ViewError::NotFound),
            ViewLoaded::Stale => Err(ViewError::Rebuilding),
            ViewLoaded::TooLarge => Err(ViewError::TooLarge),
            ViewLoaded::Skipped => Err(ViewError::Failed(SKIPPED_BASELINE.to_string())),
            ViewLoaded::Load {
                input,
                ins,
                del,
                entry,
            } => Ok((input, ins, del, entry)),
        }
    }

    /// Runs `f(input)` on a replay thread holding `granted`'s permit and
    /// slot, within `timeout`. A timeout is counted, logged and recorded
    /// under `key` by the stage the thread is in, the slot is freed and the
    /// thread watched until it ends; a dropped call does the same through
    /// [`Abandon`].
    async fn run<T: Send + 'static>(
        &self,
        granted: Granted,
        key: ViewKey,
        timeout: Duration,
        mut input: ViewInput,
        stage: StageCell,
        f: impl FnOnce(ViewInput) -> Result<T, String> + Send + 'static,
    ) -> Result<T, ViewError> {
        let Granted {
            guard,
            slot,
            started,
        } = granted;
        let progress = Progress::default();
        if let ViewInput::Window(w) = &mut input {
            w.progress = progress.clone();
        }
        let watch = guard.stall_watch();
        let mut abandon = Abandon {
            armed: true,
            progress: progress.clone(),
            watch: watch.clone(),
            records: self.records.clone(),
            key: key.clone(),
            stage: stage.clone(),
            slot: slot.clone(),
            deadline: started + timeout,
        };
        let release = SlotRelease(slot.clone());
        let hooks = self.limits.hooks.clone();
        let thread_stage = stage.clone();
        let (result, _) = run_isolated(guard, timeout, move || {
            let _release = release;
            if let Some(n) = &hooks.renders {
                n.fetch_add(1, Ordering::SeqCst);
            }
            thread_stage.set(RenderStage::Replay);
            if !hooks.hold_render.is_zero() {
                std::thread::sleep(hooks.hold_render);
            }
            if hooks.panic_render {
                panic!("test hook: panic in the render");
            }
            f(input)
        })
        .await;
        abandon.armed = false;
        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(ViewError::Failed(e)),
            Err(IsolateError::Panicked(m)) => {
                Err(ViewError::Failed(format!("the render panicked: {m}")))
            }
            Err(IsolateError::TimedOut) => {
                progress.cancel();
                watch.timed_out();
                let recorded = self.records.record_timeout(&key, stage.get(), false);
                self.records.set_stalled_gauge();
                slot.empty();
                tokio::spawn(watch_timed_out(watch, self.records.clone(), recorded));
                Err(ViewError::TimedOut)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::{TimeZone, Utc};
    use knot_storage::{DocUpdate, UpdateSource};
    use uuid::Uuid;
    use yrs::updates::decoder::Decode;
    use yrs::{
        Doc, IdSet, ReadTxn, Text, Transact, Update, XmlElementPrelim, XmlFragment, XmlOut,
        XmlTextPrelim,
    };

    use super::*;
    use crate::pass::{Progress, ReplayHooks};

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_view_cap_keeps_the_build_memory_rule() {
        let cap = 4 * MIB;
        let diff = view_max_input_bytes(cap, true);
        let plain = view_max_input_bytes(cap, false);
        assert_eq!(
            diff,
            i64::try_from(cap * 40 / VIEW_MEMORY_FACTOR.max(40)).unwrap()
        );
        assert_eq!(
            plain,
            i64::try_from(cap * 40 / PLAIN_MEMORY_FACTOR.max(40)).unwrap()
        );
        assert!(
            plain >= diff,
            "the plain cap {plain} under the diff cap {diff}"
        );
        assert!(plain <= i64::try_from(cap).unwrap());
        assert!(diff <= i64::try_from(cap).unwrap());
        // `workers × F × cap` stays what the builder's F allows.
        let baseline = baseline_max_input_bytes(plain);
        assert_eq!(baseline, 3_813_003, "4 MiB × 40 / 44");
        assert!(baseline <= plain);
        for (f, c) in [
            (VIEW_MEMORY_FACTOR, diff),
            (PLAIN_MEMORY_FACTOR, plain),
            (SHIFTED_MEMORY_FACTOR, baseline),
        ] {
            assert!(u64::try_from(c).unwrap() * f.max(BUILD_MEMORY_FACTOR) <= cap * 40);
        }
        assert_eq!(
            i128::from(baseline_max_input_bytes(i64::MAX)),
            i128::from(i64::MAX) * 40 / 44
        );
        // A cap too large for the arithmetic saturates instead of wrapping.
        assert_eq!(view_max_input_bytes(u64::MAX, false), i64::MAX);
        assert!(view_max_input_bytes(u64::MAX, true) > 0);
    }

    /// A doc of two paragraphs, as a state.
    fn two_paragraphs() -> (Doc, Vec<u8>) {
        let doc = Doc::with_client_id(7);
        let frag = doc.get_or_insert_xml_fragment("default");
        {
            let mut txn = doc.transact_mut();
            for s in ["one", "two"] {
                let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
                p.push_back(&mut txn, XmlTextPrelim::new(s));
            }
        }
        let state = doc
            .transact()
            .encode_state_as_update_v1(&yrs::StateVector::default());
        (doc, state)
    }

    #[test]
    fn a_state_input_renders_pm_json() {
        let (doc, state) = two_paragraphs();
        let r = render(ViewInput::State(state), None, RenderHooks::default()).expect("the render");
        assert_eq!(r.doc, pm_json(&doc));
        assert!(!r.changes);
        assert_eq!(r.diff_error, None);
        assert_eq!(r.doc["content"].as_array().map(Vec::len), Some(2));

        let r = render(
            ViewInput::State(vec![0xff, 0x01]),
            None,
            RenderHooks::default(),
        );
        assert!(matches!(r, Err(RenderError::State(_))), "{r:?}");
    }

    fn row(rev: i64, by: Uuid, update_bytes: Vec<u8>) -> DocUpdate {
        DocUpdate {
            seq: rev,
            doc_id: Uuid::nil(),
            update_bytes,
            by_user_id: Some(by),
            created_at: Utc.with_ymd_and_hms(2026, 11, 8, 9, 0, 0).unwrap()
                + chrono::Duration::seconds(rev),
            rev: Some(rev),
            source: Some(UpdateSource::Edit),
            meta: None,
            epoch: Some(0),
        }
    }

    /// Two authors on one page: A writes a paragraph, B appends to it, A
    /// adds a second one. The window input from an empty checkpoint, and
    /// A's merged insert and delete sets.
    fn two_authors() -> (WindowInput, (IdSet, IdSet)) {
        let a = Uuid::from_u128(0xa);
        let b = Uuid::from_u128(0xb);
        let docs = [Doc::with_client_id(1), Doc::with_client_id(2)];
        let mut rows = Vec::new();
        let mut write = |who: usize, f: &dyn Fn(&Doc)| {
            let before = docs[who].transact().state_vector();
            f(&docs[who]);
            let update = docs[who].transact().encode_state_as_update_v1(&before);
            let other = &docs[1 - who];
            other
                .transact_mut()
                .apply_update(Update::decode_v1(&update).unwrap())
                .unwrap();
            let by = if who == 0 { a } else { b };
            rows.push(row(rows.len() as i64 + 1, by, update));
        };
        write(0, &|d| {
            let frag = d.get_or_insert_xml_fragment("default");
            let mut txn = d.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("alpha"));
        });
        write(1, &|d| {
            let frag = d.get_or_insert_xml_fragment("default");
            let mut txn = d.transact_mut();
            let Some(XmlOut::Element(p)) = frag.get(&txn, 0) else {
                panic!("a paragraph");
            };
            let Some(XmlOut::Text(t)) = p.get(&txn, 0) else {
                panic!("a text");
            };
            t.insert(&mut txn, 5, " beta");
        });
        write(0, &|d| {
            let frag = d.get_or_insert_xml_fragment("default");
            let mut txn = d.transact_mut();
            let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
            p.push_back(&mut txn, XmlTextPrelim::new("gamma"));
        });

        let mut replay = Replayer::new(ReplayMode::KeepDeleted);
        let (mut ins, mut del) = (IdSet::new(), IdSet::new());
        for r in &rows {
            let e = replay.apply(&r.update_bytes);
            if r.by_user_id == Some(a) {
                ins.merge_with(e.inserted);
                del.merge_with(e.deleted);
            }
        }
        let end_rev = rows.len() as i64;
        let input = WindowInput {
            mode: ReplayMode::KeepDeleted,
            checkpoint_state: Replayer::new(ReplayMode::KeepDeleted).encode_state(),
            reapply: Vec::new(),
            rows,
            gap_states: Vec::new(),
            skip_revs: BTreeSet::new(),
            end_rev,
            merge_gap_at_end: false,
            progress: Progress::default(),
            hooks: ReplayHooks::default(),
        };
        (input, (ins, del))
    }

    /// Runs `f` with the panic hook silenced, so a deliberate panic prints
    /// nothing.
    fn quietly<T>(f: impl FnOnce() -> T) -> T {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out = f();
        std::panic::set_hook(hook);
        out
    }

    #[test]
    fn a_window_renders_with_and_without_changes() {
        let (input, sets) = two_authors();
        let plain = render(ViewInput::Window(input), None, RenderHooks::default()).unwrap();
        assert!(!plain.changes);
        assert_eq!(plain.diff_error, None);

        let (input, _) = two_authors();
        let diff = render(ViewInput::Window(input), Some(sets), RenderHooks::default()).unwrap();
        assert!(diff.changes, "{:?}", diff.diff_error);
        assert_eq!(diff.diff_error, None);
        assert_ne!(diff.doc, plain.doc, "A's changes are marked");
        assert!(
            diff.doc
                .to_string()
                .contains(crate::pm_json::HISTORY_INSERT)
        );
    }

    #[test]
    fn a_diff_that_panics_falls_back_to_the_plain_render() {
        let (input, sets) = two_authors();
        let hooks = RenderHooks {
            panic_in_diff: true,
            ..RenderHooks::default()
        };
        let r = quietly(|| render(ViewInput::Window(input), Some(sets), hooks))
            .expect("the plain render");
        assert!(!r.changes);
        let reason = r.diff_error.expect("the diff's failure");
        assert!(reason.contains("panicked"), "{reason}");

        let (input, _) = two_authors();
        let plain = render(ViewInput::Window(input), None, RenderHooks::default()).unwrap();
        assert_eq!(r.doc, plain.doc);
    }

    #[test]
    fn a_cancelled_diff_is_an_error_not_a_fallback() {
        let (input, sets) = two_authors();
        let progress = input.progress.clone();
        let hooks = RenderHooks {
            hold_in_diff: Duration::from_secs(2),
            ..RenderHooks::default()
        };
        let render =
            std::thread::spawn(move || render(ViewInput::Window(input), Some(sets), hooks));
        // The three-row replay is long done; the diff step is holding.
        std::thread::sleep(Duration::from_millis(500));
        progress.cancel();
        let r = render.join().expect("no panic");
        assert!(
            matches!(r, Err(RenderError::Diff(DiffError::Cancelled))),
            "{r:?}"
        );
    }
}
