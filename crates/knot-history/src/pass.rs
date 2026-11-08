//! One builder pass, as pure compute: replays a page's new rows from its
//! latest checkpoint, credits each row's effect (and late, parked content)
//! to its author, gap-checks room snapshots, places checkpoints, and groups
//! the result into timeline entries ([`run_pass`]). Also the replay every
//! reader that does not build uses ([`replay_window`]: word counts, M1.4's
//! views).
//!
//! Everything here runs on the replay thread (`isolate::run_isolated`):
//! no database, no tokio. What runs is published through [`Progress`], so
//! the builder can tell which input a panic or a timeout belongs to, and a
//! timed-out compute stops at the next row once [`Progress::cancel`] froze
//! it (Rulings).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use knot_storage::{DocUpdate, UpdateSource};
use thiserror::Error;
use yrs::{ReadTxn, Transact};

use crate::claims::Claims;
use crate::replay::{Parked, ReplayError, ReplayMode, Replayer, RowEffect, RowOutcome};
use crate::sessions::{Contribution, ContributionKind, EntryDraft, Grouper};

/// The smallest number of row bytes between two checkpoints.
pub const CHECKPOINT_FLOOR_BYTES: u64 = 64 * 1024;

/// The share of the state size (numerator, denominator) that the row bytes
/// since the last checkpoint must reach before the next one, when that is
/// more than the floor.
pub const CHECKPOINT_STATE_FRACTION: (u64, u64) = (1, 1);

/// The row bytes after which the next checkpoint is due, for a state of
/// `state_size` bytes: `max(floor, state_size × CHECKPOINT_STATE_FRACTION)`.
pub fn checkpoint_threshold(floor: u64, state_size: usize) -> u64 {
    let (num, den) = CHECKPOINT_STATE_FRACTION;
    floor.max((state_size as u64).saturating_mul(num) / den)
}

/// `doc_history_checkpoints.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckpointKind {
    /// The state at `genesis_rev`.
    Genesis,
    /// Written by the size policy.
    Periodic,
    /// The state right after a gap merge: the only lasting copy of content
    /// no row carries.
    Gap,
}

impl CheckpointKind {
    /// The SQL spelling (`doc_history_checkpoints.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            CheckpointKind::Genesis => "genesis",
            CheckpointKind::Periodic => "periodic",
            CheckpointKind::Gap => "gap",
        }
    }
}

impl fmt::Display for CheckpointKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `kind` that is none of `CheckpointKind`'s spellings.
#[derive(Debug, Error)]
#[error("unknown history checkpoint kind {0:?}")]
pub struct UnknownCheckpointKind(pub String);

impl FromStr for CheckpointKind {
    type Err = UnknownCheckpointKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "genesis" => CheckpointKind::Genesis,
            "periodic" => CheckpointKind::Periodic,
            "gap" => CheckpointKind::Gap,
            other => return Err(UnknownCheckpointKind(other.to_string())),
        })
    }
}

/// A checkpoint to write: the replay's state at `rev` (its blocks and
/// delete set, no pending content: the pending fallback, `replay` module
/// docs), verified to decode again.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointWrite {
    pub rev: i64,
    pub kind: CheckpointKind,
    pub state_bytes: Vec<u8>,
    pub state_vector: Vec<u8>,
    /// The revs of the row claims open at `rev`, ascending: every replay
    /// from this checkpoint first re-applies those rows, in rev order, to
    /// park their pending content again.
    pub pending_revs: Vec<i64>,
}

/// A stamped room snapshot to gap-check.
#[derive(Debug, Clone)]
pub struct SnapshotProbe {
    pub rev: i64,
    pub created_at: DateTime<Utc>,
    /// `None`: too large to probe even alone (Rulings); the pass leaves it
    /// unchecked (`too_large`) when it reaches its rev.
    pub state_bytes: Option<Vec<u8>>,
}

/// A gap checkpoint above `built_rev` that a rebuild kept (Rulings, gap
/// states): merged right after row `rev`.
#[derive(Debug, Clone)]
pub struct KeptGap {
    pub rev: i64,
    /// When the gap was first found; the gap entry's time.
    pub created_at: DateTime<Utc>,
    pub state_bytes: Vec<u8>,
}

/// What the replay is doing, for blaming a panic or a timeout on an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    #[default]
    Idle,
    /// Decoding the starting checkpoint; in genesis, merging the baseline.
    Checkpoint,
    /// Re-applying a row at or below `built_rev`: a row of the starting
    /// checkpoint's `pending_revs`, or of the replay prefix.
    Prefix(i64),
    /// Applying a new row.
    Row(i64),
    /// Probing (or gap-merging) the snapshot at this rev.
    Probe(i64),
    /// Merging the kept gap state at this rev (in [`replay_window`], the
    /// gap state at this rev).
    KeptGap(i64),
    /// Encoding and verifying a checkpoint at this rev.
    Emit(i64),
}

struct ProgressState {
    stage: Stage,
    since: Instant,
    cancelled: bool,
}

impl Default for ProgressState {
    fn default() -> Self {
        ProgressState {
            stage: Stage::Idle,
            since: Instant::now(),
            cancelled: false,
        }
    }
}

/// The stage a replay is in, shared between the replay thread and the
/// builder task that awaits it.
#[derive(Clone, Default)]
pub struct Progress(Arc<Mutex<ProgressState>>);

impl Progress {
    fn lock(&self) -> MutexGuard<'_, ProgressState> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Enters stage `s`, recording when; a no-op once cancelled.
    pub fn set(&self, s: Stage) {
        let mut state = self.lock();
        if !state.cancelled {
            state.stage = s;
            state.since = Instant::now();
        }
    }

    pub fn get(&self) -> Stage {
        self.lock().stage
    }

    /// Freezes the stage and returns it with how long it had been current.
    pub fn cancel(&self) -> (Stage, Duration) {
        let mut state = self.lock();
        if !state.cancelled {
            state.cancelled = true;
        }
        (state.stage, state.since.elapsed())
    }

    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }
}

impl fmt::Debug for Progress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("Progress")
            .field("stage", &state.stage)
            .field("cancelled", &state.cancelled)
            .finish()
    }
}

/// Test hooks; production code never sets them.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct ReplayHooks {
    /// Panics while that row's apply is in progress (`Row(rev)`, or
    /// `Prefix(rev)` when the replay re-applies it from the checkpoint's
    /// `pending_revs` or in the prefix).
    pub panic_at_rev: Option<i64>,
    /// Turns that row's outcome into `Corrupt` after its apply.
    pub corrupt_at_rev: Option<i64>,
    /// Panics in the probe of the snapshot at that rev.
    pub panic_probe_at: Option<i64>,
    /// Turns that snapshot's gap merge into `Corrupt`.
    pub corrupt_merge_at: Option<i64>,
    /// Sleeps at that row's stage, like `panic_at_rev`.
    pub sleep_at_rev: Option<(i64, Duration)>,
    /// Sleeps inside the first emit, at `Stage::Emit` (in genesis, the
    /// genesis checkpoint's).
    pub sleep_at_emit: Option<Duration>,
    /// Sleeps in genesis at `Stage::Checkpoint` while merging the baseline
    /// with that id.
    pub sleep_at_baseline: Option<(i64, Duration)>,
    /// Turns the merge of the kept gap at that rev into `Corrupt`.
    pub corrupt_kept_gap_at: Option<i64>,
    /// Retries every parked row in every retry round, as if each waited
    /// for what just arrived (the retry gate off: `replay::Parked`).
    pub retry_every_row: bool,
}

impl ReplayHooks {
    /// The hooks a row's apply runs at its stage.
    pub(crate) fn at_row(&self, rev: i64) {
        if self.panic_at_rev == Some(rev) {
            panic!("test hook: panic at rev {rev}");
        }
        if let Some((at, d)) = self.sleep_at_rev
            && at == rev
        {
            std::thread::sleep(d);
        }
    }
}

/// The inputs of one pass (Task 7 loads them in one read transaction).
pub struct PassInput {
    /// The rev of the latest checkpoint at or below `built_rev`.
    pub checkpoint_rev: i64,
    pub checkpoint_state: Vec<u8>,
    /// The rows of the checkpoint's `pending_revs`, in rev order (the
    /// pending fallback was taken: `replay` module docs).
    pub reapply: Vec<DocUpdate>,
    /// The rows after the checkpoint through `built_rev` (the prefix).
    pub replay: Vec<DocUpdate>,
    pub genesis_rev: i64,
    pub built_rev: i64,
    /// New rows from `built_rev + 1`, in rev order.
    pub rows: Vec<DocUpdate>,
    /// The last row of `rows` is the page's head.
    pub rows_exhausted: bool,
    /// The page's open sessions as of `built_rev`.
    pub open: Vec<EntryDraft>,
    pub claims: Claims,
    /// Gap checkpoints above `built_rev` that a rebuild kept, ascending,
    /// only those at or below the last row in `rows`.
    pub kept_gaps: Vec<KeptGap>,
    /// Stamped snapshots with `snapshot_checked_rev < rev`, ascending, one
    /// per rev.
    pub snapshots: Vec<SnapshotProbe>,
    pub snapshot_checked_rev: i64,
    pub skip_revs: BTreeSet<i64>,
    pub progress: Progress,
    pub now: DateTime<Utc>,
    /// Compute time for new rows, counted from the first new row's apply.
    pub rows_budget: Option<Duration>,
    /// `CHECKPOINT_FLOOR_BYTES` in production.
    pub checkpoint_floor: u64,
    #[doc(hidden)]
    pub hooks: ReplayHooks,
}

/// What a pass counted, for the builder's metrics.
#[derive(Debug, Default)]
pub struct PassStats {
    /// New rows replayed (skipped ones not included).
    pub rows: u64,
    /// `knot_history_rows_unapplied_total` by reason.
    pub unapplied: BTreeMap<&'static str, u64>,
    /// `knot_history_snapshots_unchecked_total` by reason.
    pub snapshots_unchecked: BTreeMap<&'static str, u64>,
    /// Rows that newly parked content.
    pub parked: u64,
    /// Gap entries fed.
    pub gaps: u64,
    /// The rows stopped at a missing rev.
    pub rev_gap: bool,
    /// The rows budget ran out before the last row.
    pub stopped_early: bool,
    /// Rev and size of every `encode_state` the checkpoint policy ran.
    pub encodes: Vec<(i64, usize)>,
    /// Parked rows re-applied from their bytes (`Replayer::retry`).
    pub retried: u64,
}

/// What a pass built.
#[derive(Debug)]
pub struct PassOutput {
    pub built_rev: i64,
    pub snapshot_checked_rev: i64,
    /// Entries created or changed, in creation order.
    pub entries: Vec<EntryDraft>,
    /// At most one per rev, ascending.
    pub checkpoints: Vec<CheckpointWrite>,
    pub claims: Claims,
    /// New rows whose outcome was `Refused`; the write adds them to
    /// `skip_revs`.
    pub refused_revs: Vec<i64>,
    pub stats: PassStats,
}

/// One pass over a page's new rows and snapshots (module docs).
pub fn run_pass(input: PassInput) -> Result<PassOutput, ReplayError> {
    let PassInput {
        checkpoint_rev: _,
        checkpoint_state,
        reapply,
        replay,
        genesis_rev,
        built_rev,
        rows,
        rows_exhausted,
        open,
        claims,
        kept_gaps,
        snapshots,
        snapshot_checked_rev,
        skip_revs,
        progress,
        now,
        rows_budget,
        checkpoint_floor,
        hooks,
    } = input;

    let mut pass = Pass {
        parked: Parked::new(hooks.retry_every_row),
        grouper: Grouper::new(open),
        claims,
        genesis_rev,
        stats: PassStats::default(),
        checkpoints: BTreeMap::new(),
        progress,
        hooks,
        emitted: false,
        bytes_since_cp: 0,
        encoded_at: None,
        last_state_size: checkpoint_state.len(),
    };
    let mut built_rev = built_rev;
    let mut snapshot_checked_rev = snapshot_checked_rev;
    let mut refused_revs = Vec::new();

    // Nothing below needs the replay: no decode, no prefix.
    if !(rows.is_empty() && snapshots.is_empty()) {
        if pass.progress.is_cancelled() {
            return Err(ReplayError::Cancelled);
        }
        pass.progress.set(Stage::Checkpoint);
        let mut replayer = Replayer::from_state(ReplayMode::Gc, &checkpoint_state)?;
        pass.progress.set(Stage::Idle);

        // The rows retried in the prefix: the claims open at the checkpoint,
        // then every prefix row that parks (the claims of the prefix are
        // not known; a row that drained waits for nothing and is not
        // retried). A re-applied row that fails blames the checkpoint, as a
        // prefix row does: its block layout can differ from the live one.
        for row in &reapply {
            let rev = rev_of(row);
            if skip_revs.contains(&rev) {
                continue;
            }
            if pass.progress.is_cancelled() {
                return Err(ReplayError::Cancelled);
            }
            pass.progress.set(Stage::Prefix(rev));
            pass.hooks.at_row(rev);
            let e = replayer.apply(&row.update_bytes);
            if matches!(e.outcome, RowOutcome::Corrupt | RowOutcome::Refused)
                || pass.hooks.corrupt_at_rev == Some(rev)
            {
                return Err(ReplayError::Prefix { rev });
            }
            pass.parked.mark(&e.inserted);
            pass.parked.add(rev, &row.update_bytes);
            pass.progress.set(Stage::Idle);
        }

        for row in replay.iter().filter(|r| rev_of(r) <= built_rev) {
            let rev = rev_of(row);
            if skip_revs.contains(&rev) {
                continue;
            }
            if pass.progress.is_cancelled() {
                return Err(ReplayError::Cancelled);
            }
            pass.progress.set(Stage::Prefix(rev));
            pass.hooks.at_row(rev);
            let mut e = replayer.apply(&row.update_bytes);
            if matches!(e.outcome, RowOutcome::Corrupt | RowOutcome::Refused)
                || pass.hooks.corrupt_at_rev == Some(rev)
            {
                return Err(ReplayError::Prefix { rev });
            }
            pass.parked.mark(&e.inserted);
            if !e.inserted.is_empty() && !pass.parked.retry(&mut replayer, &mut e) {
                return Err(ReplayError::Prefix { rev });
            }
            if !(e.parked_inserts.is_empty() && e.parked_deletes.is_empty()) {
                pass.parked.add(rev, &row.update_bytes);
            }
            pass.progress.set(Stage::Idle);
            pass.bytes_since_cp += row.update_bytes.len() as u64;
        }
        // From here on, the open claims' rows.
        pass.parked.retain(&pass.claims.revs());

        // Late snapshots: written after the builder passed their rev.
        let (late, ahead): (Vec<_>, Vec<_>) = snapshots.iter().partition(|s| s.rev <= built_rev);
        for snapshot in late {
            pass.gap(&mut replayer, snapshot, built_rev)?;
            snapshot_checked_rev = snapshot_checked_rev.max(snapshot.rev);
        }
        let mut ahead = ahead.into_iter().peekable();
        let start_rev = built_rev;
        let mut kept = kept_gaps.iter().filter(|k| k.rev > start_rev).peekable();

        let mut deadline: Option<Instant> = None;
        for (i, row) in rows.iter().enumerate() {
            let rev = rev_of(row);
            if rev != start_rev + 1 + i as i64 {
                // Rev order is commit order: a gap is a bug.
                pass.stats.rev_gap = true;
                break;
            }
            if skip_revs.contains(&rev) {
                *pass.stats.unapplied.entry("skipped").or_default() += 1;
            } else {
                if pass.progress.is_cancelled() {
                    return Err(ReplayError::Cancelled);
                }
                pass.progress.set(Stage::Row(rev));
                if deadline.is_none() {
                    deadline = rows_budget.map(|b| Instant::now() + b);
                }
                pass.hooks.at_row(rev);
                let mut effect = replayer.apply(&row.update_bytes);
                if effect.outcome == RowOutcome::Corrupt || pass.hooks.corrupt_at_rev == Some(rev) {
                    return Err(ReplayError::Corrupt { rev });
                }
                // What this row brought may be what parked rows waited for.
                pass.parked.mark(&effect.inserted);
                if !effect.inserted.is_empty() && !pass.parked.retry(&mut replayer, &mut effect) {
                    return Err(ReplayError::Corrupt { rev });
                }
                pass.progress.set(Stage::Idle);
                if let Some(reason) = effect.outcome.metric_reason() {
                    *pass.stats.unapplied.entry(reason).or_default() += 1;
                }
                if effect.outcome == RowOutcome::Refused {
                    refused_revs.push(rev);
                }
                pass.bytes_since_cp += row.update_bytes.len() as u64;
                pass.credit(
                    &mut effect,
                    rev,
                    row.created_at,
                    ContributionKind::Row(Some(UpdateSource::Edit)),
                );
                if !(effect.parked_inserts.is_empty() && effect.parked_deletes.is_empty()) {
                    pass.claims.record(
                        rev,
                        row.by_user_id,
                        effect.parked_inserts.clone(),
                        effect.parked_deletes.clone(),
                    );
                    pass.parked.add(rev, &row.update_bytes);
                    pass.stats.parked += 1;
                }
                let first_copy = rev == genesis_rev + 1 && row.source == Some(UpdateSource::Copy);
                if !effect.is_empty() || first_copy {
                    pass.grouper.feed(Contribution {
                        rev,
                        at: row.created_at,
                        author: row.by_user_id,
                        kind: ContributionKind::Row(row.source),
                        meta: row.meta.clone(),
                        inserted: effect.inserted,
                        deleted: effect.deleted,
                    });
                }
            }

            // Right after row `rev`, applied, skipped or passed over: the
            // kept gap there, then the snapshots there.
            while let Some(k) = kept.next_if(|k| k.rev <= rev) {
                pass.kept(&mut replayer, k)?;
            }
            while let Some(s) = ahead.next_if(|s| s.rev <= rev) {
                pass.gap(&mut replayer, s, rev)?;
                snapshot_checked_rev = snapshot_checked_rev.max(s.rev);
            }

            if !skip_revs.contains(&rev) {
                // At least the floor's worth of rows between two encodes
                // (Rulings: a state that grows as fast as the rows would
                // otherwise be encoded on every row).
                let spaced = pass
                    .encoded_at
                    .is_none_or(|at| pass.bytes_since_cp - at >= checkpoint_floor);
                if spaced
                    && pass.bytes_since_cp
                        >= checkpoint_threshold(checkpoint_floor, pass.last_state_size)
                {
                    pass.progress.set(Stage::Emit(rev));
                    let state = replayer.encode_state();
                    pass.progress.set(Stage::Idle);
                    pass.last_state_size = state.len();
                    pass.stats.encodes.push((rev, state.len()));
                    pass.encoded_at = Some(pass.bytes_since_cp);
                    if pass.bytes_since_cp >= checkpoint_threshold(checkpoint_floor, state.len()) {
                        // A state that does not decode again is not emitted.
                        pass.emit(&replayer, CheckpointKind::Periodic, rev, Some(state));
                        pass.bytes_since_cp = 0;
                        pass.encoded_at = None;
                    }
                }
                pass.stats.rows += 1;
            }
            built_rev = rev;

            let another_follows = i + 1 < rows.len();
            if another_follows && deadline.is_some_and(|d| Instant::now() >= d) {
                pass.stats.stopped_early = true;
                break;
            }
        }
    }

    if rows_exhausted && !pass.stats.rev_gap && !pass.stats.stopped_early {
        pass.grouper.seal_idle(now);
    }
    pass.stats.retried = pass.parked.retried;
    Ok(PassOutput {
        built_rev,
        snapshot_checked_rev,
        entries: pass.grouper.finish(),
        checkpoints: pass.checkpoints.into_values().collect(),
        claims: pass.claims,
        refused_revs,
        stats: pass.stats,
    })
}

/// A row's rev; every row a pass reads is numbered, and a 0 here turns into
/// a rev gap.
fn rev_of(row: &DocUpdate) -> i64 {
    row.rev.unwrap_or(0)
}

/// Whether a merge took its state (also part-way).
fn took(e: &RowEffect) -> bool {
    matches!(e.outcome, RowOutcome::Applied | RowOutcome::ApplyError(_))
}

/// The `snapshots_unchecked` reason for a merge that did not take.
fn unchecked_reason(outcome: &RowOutcome) -> &'static str {
    match outcome {
        RowOutcome::Refused => "refused",
        RowOutcome::Undecodable(_) => "undecodable",
        _ => "corrupt",
    }
}

/// A checkpoint of `state` (the replay's [`Replayer::encode_state`]) once
/// it decodes again to the replay's state vector; `None` when it does not.
/// Its `rev`, `kind` and `pending_revs` are the caller's to set.
pub(crate) fn verified_state(replayer: &Replayer, state: Vec<u8>) -> Option<CheckpointWrite> {
    let state_vector = replayer.state_vector();
    let decoded = Replayer::from_state(ReplayMode::Gc, &state).ok()?;
    // Compared as values: the v1 encoding follows a hash map's order.
    let same = decoded.doc().transact().state_vector() == replayer.doc().transact().state_vector();
    same.then_some(CheckpointWrite {
        rev: 0,
        kind: CheckpointKind::Periodic,
        state_bytes: state,
        state_vector,
        pending_revs: Vec::new(),
    })
}

/// A pass's running state, apart from the replay itself.
struct Pass<'a> {
    /// The open claims' rows (in the prefix, every row that parked), retried
    /// once the replay gains what they wait for.
    parked: Parked<'a>,
    grouper: Grouper,
    claims: Claims,
    genesis_rev: i64,
    stats: PassStats,
    checkpoints: BTreeMap<i64, CheckpointWrite>,
    progress: Progress,
    hooks: ReplayHooks,
    /// Whether an emit ran (`sleep_at_emit` sleeps in the first only).
    emitted: bool,
    /// Row bytes applied since the last checkpoint.
    bytes_since_cp: u64,
    /// `bytes_since_cp` at the policy's last encode since that checkpoint.
    encoded_at: Option<u64>,
    /// The size of the state last encoded (the starting checkpoint's, a
    /// policy encode's or an emitted checkpoint's), which sets the next
    /// checkpoint's threshold.
    last_state_size: usize,
}

impl Pass<'_> {
    /// Credits what `effect` integrated of parked claims to the claims'
    /// authors (gap claims as gap content); genesis claims are dropped.
    /// What stays in `effect` is the apply's own.
    fn credit(
        &mut self,
        effect: &mut RowEffect,
        rev: i64,
        at: DateTime<Utc>,
        kind: ContributionKind,
    ) {
        let credits = self
            .claims
            .credit(&mut effect.inserted, &mut effect.deleted);
        if !credits.is_empty() {
            // A drained claim's row waits for nothing any more.
            self.parked.retain(&self.claims.revs());
        }
        for c in credits {
            if c.parked_rev <= self.genesis_rev {
                // Genesis content, never credited (Rulings).
                continue;
            }
            let (author, kind) = if c.gap {
                self.stats.gaps += 1;
                (None, ContributionKind::Gap)
            } else {
                (c.author, kind)
            };
            self.grouper.feed(Contribution {
                rev,
                at,
                author,
                kind,
                meta: None,
                inserted: c.inserted,
                deleted: c.deleted,
            });
        }
    }

    /// Encodes and verifies the state at `rev` (or takes the one the policy
    /// just encoded) and records it as a checkpoint of `kind`, replacing a
    /// periodic one at that rev. `false` when the state does not decode
    /// again: nothing is recorded.
    fn emit(
        &mut self,
        replayer: &Replayer,
        kind: CheckpointKind,
        rev: i64,
        state: Option<Vec<u8>>,
    ) -> bool {
        self.progress.set(Stage::Emit(rev));
        if !self.emitted {
            self.emitted = true;
            if let Some(d) = self.hooks.sleep_at_emit {
                std::thread::sleep(d);
            }
        }
        let state = state.unwrap_or_else(|| replayer.encode_state());
        self.last_state_size = state.len();
        let verified = verified_state(replayer, state);
        self.progress.set(Stage::Idle);
        let Some(mut cp) = verified else {
            return false;
        };
        cp.rev = rev;
        cp.kind = kind;
        cp.pending_revs = self.claims.revs();
        let keep_existing = kind == CheckpointKind::Periodic
            && self
                .checkpoints
                .get(&rev)
                .is_some_and(|c| c.kind == CheckpointKind::Gap);
        if !keep_existing {
            self.checkpoints.insert(rev, cp);
        }
        true
    }

    /// Applies what a gap merge did (from a probe at `at_rev` or a kept gap):
    /// credits parked claims, records what it newly parked as a gap claim,
    /// feeds the rest as a gap entry, and emits the gap checkpoint at
    /// `at_rev`. `false` when that checkpoint does not decode again.
    fn after_gap_merge(
        &mut self,
        replayer: &Replayer,
        mut e: RowEffect,
        at_rev: i64,
        at: DateTime<Utc>,
    ) -> bool {
        self.credit(&mut e, at_rev, at, ContributionKind::GapCredit);
        if !(e.parked_inserts.is_empty() && e.parked_deletes.is_empty()) {
            self.claims
                .record_gap(at_rev, e.parked_inserts.clone(), e.parked_deletes.clone());
        }
        if !e.is_empty() {
            self.grouper.feed(Contribution {
                rev: at_rev,
                at,
                author: None,
                kind: ContributionKind::Gap,
                meta: None,
                inserted: e.inserted,
                deleted: e.deleted,
            });
            self.stats.gaps += 1;
        }
        let ok = self.emit(replayer, CheckpointKind::Gap, at_rev, None);
        self.bytes_since_cp = 0;
        self.encoded_at = None;
        ok
    }

    /// Merges a gap state kept across a rebuild, right after row `k.rev`.
    fn kept(&mut self, replayer: &mut Replayer, k: &KeptGap) -> Result<(), ReplayError> {
        if self.progress.is_cancelled() {
            return Err(ReplayError::Cancelled);
        }
        self.progress.set(Stage::KeptGap(k.rev));
        // `Replayer::merge_gap_state`, parking the claims' content again
        // after a union.
        let mut e = replayer.merge_state(&k.state_bytes);
        if e.outcome == RowOutcome::Refused {
            e = replayer.union(&k.state_bytes);
        }
        if !took(&e)
            || self.hooks.corrupt_kept_gap_at == Some(k.rev)
            || !self.retry_claims(replayer, &mut e)
        {
            return Err(ReplayError::KeptGap { rev: k.rev });
        }
        self.progress.set(Stage::Idle);
        // Emitted even when the effect is empty: it replaces the old gap
        // checkpoint at that rev.
        if !self.after_gap_merge(replayer, e, k.rev, k.created_at) {
            return Err(ReplayError::KeptGap { rev: k.rev });
        }
        Ok(())
    }

    /// Gap-checks `snapshot` against the replay at `at_rev` (its own rev, or
    /// `built_rev` for a late snapshot).
    fn gap(
        &mut self,
        replayer: &mut Replayer,
        snapshot: &SnapshotProbe,
        at_rev: i64,
    ) -> Result<(), ReplayError> {
        let Some(bytes) = snapshot.state_bytes.as_deref() else {
            self.unchecked("too_large");
            return Ok(());
        };
        if self.progress.is_cancelled() {
            return Err(ReplayError::Cancelled);
        }
        self.progress.set(Stage::Probe(snapshot.rev));
        if self.hooks.panic_probe_at == Some(snapshot.rev) {
            panic!(
                "test hook: panic probing the snapshot at rev {}",
                snapshot.rev
            );
        }
        let g = replayer.probe(bytes);
        let e = match g.outcome {
            RowOutcome::Undecodable(_) => return self.leave_unchecked("undecodable"),
            RowOutcome::Corrupt => return self.leave_unchecked("corrupt"),
            // The refused shape: take the snapshot through a fresh doc.
            RowOutcome::Refused => {
                let e = replayer.union(bytes);
                if !took(&e) {
                    return self.leave_unchecked(unchecked_reason(&e.outcome));
                }
                e
            }
            RowOutcome::Applied | RowOutcome::ApplyError(_) => {
                if g.is_empty() {
                    self.progress.set(Stage::Idle);
                    return Ok(());
                }
                let mut e = replayer.merge_state(bytes);
                if e.outcome == RowOutcome::Corrupt
                    || self.hooks.corrupt_merge_at == Some(snapshot.rev)
                {
                    return Err(ReplayError::CorruptSnapshot {
                        snapshot_rev: snapshot.rev,
                    });
                }
                if e.outcome == RowOutcome::Refused {
                    e = replayer.union(bytes);
                }
                if !took(&e) {
                    return self.leave_unchecked(unchecked_reason(&e.outcome));
                }
                e
            }
        };
        // The merge (or the union, which also dropped the replay's pending
        // content) may hold what parked rows waited for.
        let mut e = e;
        if !self.retry_claims(replayer, &mut e) {
            return Err(ReplayError::CorruptSnapshot {
                snapshot_rev: snapshot.rev,
            });
        }
        self.progress.set(Stage::Idle);
        if !self.after_gap_merge(replayer, e, at_rev, snapshot.created_at) {
            return Err(ReplayError::CorruptSnapshot {
                snapshot_rev: snapshot.rev,
            });
        }
        Ok(())
    }

    /// Retries every open claim's row from its original bytes after a gap
    /// merge ([`Parked::retry_all`]), adding what they integrate now to `e`,
    /// where [`Pass::credit`] finds it. After a union, which replaced the
    /// replay and dropped its pending content, this also parks that content
    /// again. `false` when a retried row corrupts the replay.
    fn retry_claims(&mut self, replayer: &mut Replayer, e: &mut RowEffect) -> bool {
        self.parked.retry_all(replayer, e)
    }

    fn unchecked(&mut self, reason: &'static str) {
        *self.stats.snapshots_unchecked.entry(reason).or_default() += 1;
    }

    /// Leaves the snapshot being probed unchecked: the pass goes on.
    fn leave_unchecked(&mut self, reason: &'static str) -> Result<(), ReplayError> {
        self.unchecked(reason);
        self.progress.set(Stage::Idle);
        Ok(())
    }
}

/// The inputs of a replay that builds nothing (word counts, M1.4 views).
pub struct WindowInput {
    pub mode: ReplayMode,
    pub checkpoint_state: Vec<u8>,
    /// The rows of the checkpoint's `pending_revs`, in rev order.
    pub reapply: Vec<DocUpdate>,
    /// The rows after the checkpoint through `end_rev`, in rev order.
    pub rows: Vec<DocUpdate>,
    /// The gap checkpoints after the checkpoint through `end_rev`,
    /// ascending.
    pub gap_states: Vec<(i64, Vec<u8>)>,
    pub skip_revs: BTreeSet<i64>,
    pub end_rev: i64,
    /// Whether a gap state at `end_rev` itself is merged (Rulings): only
    /// when replaying for that gap entry, or for a session credited by it.
    pub merge_gap_at_end: bool,
    pub progress: Progress,
    /// Test hooks; production code leaves them at their default. A window
    /// runs `panic_at_rev`, `sleep_at_rev`, `corrupt_at_rev` and
    /// `retry_every_row`, as the pass does for its rows.
    #[doc(hidden)]
    pub hooks: ReplayHooks,
}

/// Why a window replay stopped.
#[derive(Debug, Error)]
pub enum WindowError {
    /// The starting checkpoint does not decode or apply.
    #[error("the starting state does not apply: {0}")]
    State(String),
    /// A row corrupted the replay.
    #[error("row {rev} corrupted the replay")]
    Corrupt { rev: i64 },
    /// A gap state neither `merge_state` nor `union` takes.
    #[error("the gap state at rev {rev} does not merge")]
    GapState { rev: i64 },
    /// The caller cancelled the timed-out replay.
    #[error("the replay was cancelled")]
    Cancelled,
}

/// Replays a window: the checkpoint, the reapply rows, then each row not in
/// `skip_revs` (rows that are refused, rejected or undecodable are passed
/// over, as the pass did), merging each gap state once the replay has
/// passed its rev (the one at `end_rev` only when `merge_gap_at_end`).
pub fn replay_window(input: WindowInput) -> Result<Replayer, WindowError> {
    let WindowInput {
        mode,
        checkpoint_state,
        reapply,
        rows,
        gap_states,
        skip_revs,
        end_rev,
        merge_gap_at_end,
        progress,
        hooks,
    } = input;
    let mut replayer = Replayer::from_state(mode, &checkpoint_state)
        .map_err(|e| WindowError::State(e.to_string()))?;
    // Rows whose content may still be pending, retried from their bytes
    // once the replay gains what they wait for (`replay::Parked`), as the
    // pass does. The window knows no claims: every row that parks stays,
    // and one that drained waits for nothing.
    let mut parked = Parked::new(hooks.retry_every_row);
    for row in &reapply {
        let rev = rev_of(row);
        if skip_revs.contains(&rev) {
            continue;
        }
        if progress.is_cancelled() {
            return Err(WindowError::Cancelled);
        }
        progress.set(Stage::Prefix(rev));
        hooks.at_row(rev);
        let e = replayer.apply(&row.update_bytes);
        if e.outcome == RowOutcome::Corrupt || hooks.corrupt_at_rev == Some(rev) {
            return Err(WindowError::Corrupt { rev });
        }
        parked.mark(&e.inserted);
        parked.add(rev, &row.update_bytes);
        progress.set(Stage::Idle);
    }
    let mut gaps = gap_states
        .iter()
        .filter(|(rev, _)| *rev < end_rev || (*rev == end_rev && merge_gap_at_end))
        .peekable();
    for row in rows.iter().filter(|r| rev_of(r) <= end_rev) {
        let rev = rev_of(row);
        // The replay has passed every gap state below this row.
        while let Some((at, state)) = gaps.next_if(|(at, _)| *at < rev) {
            merge_window_gap(&mut replayer, &mut parked, &progress, *at, state)?;
        }
        if progress.is_cancelled() {
            return Err(WindowError::Cancelled);
        }
        if skip_revs.contains(&rev) {
            continue;
        }
        progress.set(Stage::Row(rev));
        hooks.at_row(rev);
        let mut e = replayer.apply(&row.update_bytes);
        if e.outcome == RowOutcome::Corrupt || hooks.corrupt_at_rev == Some(rev) {
            return Err(WindowError::Corrupt { rev });
        }
        parked.mark(&e.inserted);
        if !e.inserted.is_empty() && !parked.retry(&mut replayer, &mut e) {
            return Err(WindowError::Corrupt { rev });
        }
        progress.set(Stage::Idle);
        if !(e.parked_inserts.is_empty() && e.parked_deletes.is_empty()) {
            parked.add(rev, &row.update_bytes);
        }
    }
    for (at, state) in gaps {
        merge_window_gap(&mut replayer, &mut parked, &progress, *at, state)?;
    }
    Ok(replayer)
}

/// A window's gap state at `rev`: `Replayer::merge_gap_state` at
/// `Stage::KeptGap(rev)`, then every parked row retried (after a union,
/// which drops the replay's pending content, that parks it again).
fn merge_window_gap(
    replayer: &mut Replayer,
    parked: &mut Parked<'_>,
    progress: &Progress,
    rev: i64,
    state: &[u8],
) -> Result<(), WindowError> {
    if progress.is_cancelled() {
        return Err(WindowError::Cancelled);
    }
    progress.set(Stage::KeptGap(rev));
    let mut e = replayer.merge_gap_state(state);
    if !(took(&e) && parked.retry_all(replayer, &mut e)) {
        return Err(WindowError::GapState { rev });
    }
    progress.set(Stage::Idle);
    Ok(())
}
