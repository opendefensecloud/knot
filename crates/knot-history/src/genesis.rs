//! Genesis: a page's history before `history_since`, as one checkpoint.
//!
//! The genesis state is the newest baseline that applies plus every row with
//! `rev ≤ genesis_rev` (`G`), in rev order (Rulings: genesis ends at the
//! highest rev written before `history_since`; replay is idempotent, so rows
//! the baseline already holds change nothing). It is written as the
//! `genesis` checkpoint at `G` (an empty state at rev 0 for a page created
//! after `history_since`). Legacy rows are never credited: genesis records
//! what each one newly parks as its claim (author `None`), so a later row
//! that drains it is not credited with it either. Every baseline gets a
//! metadata-only `baseline` entry, and the "state at rollout" entry is
//! written when genesis shows something the baseline it started from does
//! not.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use knot_storage::DocUpdate;
use serde_json::{Value, json};
use thiserror::Error;
use yrs::IdSet;

use crate::canonical::visible_content;
use crate::claims::Claims;
use crate::pass::{
    CheckpointKind, CheckpointWrite, PassStats, Progress, ReplayHooks, Stage, verified_state,
};
use crate::replay::{Parked, ReplayMode, Replayer, RowOutcome};
use crate::sessions::{EntryDraft, EntryKind};

/// One `doc_history_baselines` row of the page.
#[derive(Debug, Clone)]
pub struct BaselineInput {
    pub id: i64,
    pub snapshot_seq: i64,
    /// `Some` only for the baseline genesis starts from (the newest whose
    /// bytes the builder loaded); the others only make entries.
    pub state_bytes: Option<Vec<u8>>,
    pub created_at: DateTime<Utc>,
}

/// The inputs of genesis.
pub struct GenesisInput {
    pub page_created_at: DateTime<Utc>,
    pub history_since: DateTime<Utc>,
    pub genesis_rev: i64,
    /// Ascending by `snapshot_seq`.
    pub baselines: Vec<BaselineInput>,
    /// Rows with `rev ≤ genesis_rev`, in rev order.
    pub rows: Vec<DocUpdate>,
    pub skip_revs: BTreeSet<i64>,
    pub progress: Progress,
    #[doc(hidden)]
    pub hooks: ReplayHooks,
}

/// What genesis built.
#[derive(Debug)]
pub struct GenesisOutput {
    /// The `genesis` checkpoint at `genesis_rev`.
    pub checkpoint: CheckpointWrite,
    /// One baseline entry per baseline, then the rollout entry if any.
    pub entries: Vec<EntryDraft>,
    /// One claim per legacy row that newly parked content (author `None`).
    pub claims: Claims,
    /// Legacy rows whose outcome was `Refused`.
    pub refused_revs: Vec<i64>,
    pub stats: PassStats,
}

/// Why genesis stopped.
#[derive(Debug, Error)]
pub enum GenesisError {
    /// The baseline genesis started from does not apply; the builder tries
    /// the next older one.
    #[error("baseline {id} does not apply")]
    BaselineRejected { id: i64 },
    /// A legacy row corrupted the replay.
    #[error("row {rev} corrupted the genesis replay")]
    Corrupt { rev: i64 },
    /// The genesis state does not decode again; no row is to blame.
    #[error("the genesis state does not decode again")]
    Encode,
    /// The builder cancelled the timed-out compute.
    #[error("genesis was cancelled")]
    Cancelled,
}

/// Builds a page's genesis checkpoint and its pre-rollout entries.
pub fn build_genesis(input: GenesisInput) -> Result<GenesisOutput, GenesisError> {
    let GenesisInput {
        page_created_at,
        history_since,
        genesis_rev,
        baselines,
        rows,
        skip_revs,
        progress,
        hooks,
    } = input;
    let mut replayer = Replayer::new(ReplayMode::Gc);
    let mut stats = PassStats::default();
    let mut claims = Claims::default();
    let mut refused_revs = Vec::new();

    // The baseline genesis starts from: the newest one whose bytes it got.
    let start = baselines
        .iter()
        .rev()
        .find_map(|b| b.state_bytes.as_deref().map(|bytes| (b.id, bytes)));
    let baseline_content = match start {
        Some((id, bytes)) => {
            if progress.is_cancelled() {
                return Err(GenesisError::Cancelled);
            }
            progress.set(Stage::Checkpoint);
            if let Some((at, d)) = hooks.sleep_at_baseline
                && at == id
            {
                std::thread::sleep(d);
            }
            let e = replayer.merge_state(bytes);
            if e.outcome != RowOutcome::Applied {
                return Err(GenesisError::BaselineRejected { id });
            }
            progress.set(Stage::Idle);
            Some(visible_content(replayer.doc()))
        }
        None => None,
    };

    // Parked legacy rows, retried from their bytes as a pass does.
    let mut parked = Parked::new(hooks.retry_every_row);
    for row in rows.iter().filter(|r| r.rev.unwrap_or(0) <= genesis_rev) {
        let rev = row.rev.unwrap_or(0);
        if progress.is_cancelled() {
            return Err(GenesisError::Cancelled);
        }
        if skip_revs.contains(&rev) {
            *stats.unapplied.entry("skipped").or_default() += 1;
            continue;
        }
        progress.set(Stage::Row(rev));
        hooks.at_row(rev);
        let mut e = replayer.apply(&row.update_bytes);
        if e.outcome == RowOutcome::Corrupt || hooks.corrupt_at_rev == Some(rev) {
            return Err(GenesisError::Corrupt { rev });
        }
        parked.mark(&e.inserted);
        if !e.inserted.is_empty() && !parked.retry(&mut replayer, &mut e) {
            return Err(GenesisError::Corrupt { rev });
        }
        progress.set(Stage::Idle);
        if let Some(reason) = e.outcome.metric_reason() {
            *stats.unapplied.entry(reason).or_default() += 1;
        }
        if e.outcome == RowOutcome::Refused {
            refused_revs.push(rev);
        }
        // What an earlier legacy row parked and this one drained is
        // genesis content: the claim shrinks, nobody is credited.
        if !claims.credit(&mut e.inserted, &mut e.deleted).is_empty() {
            parked.retain(&claims.revs());
        }
        if !(e.parked_inserts.is_empty() && e.parked_deletes.is_empty()) {
            claims.record(rev, None, e.parked_inserts, e.parked_deletes);
            parked.add(rev, &row.update_bytes);
            stats.parked += 1;
        }
        stats.rows += 1;
    }
    stats.retried = parked.retried;

    progress.set(Stage::Emit(genesis_rev));
    if let Some(d) = hooks.sleep_at_emit {
        std::thread::sleep(d);
    }
    let state = replayer.encode_state();
    let mut checkpoint = verified_state(&replayer, state).ok_or(GenesisError::Encode)?;
    progress.set(Stage::Idle);
    checkpoint.rev = genesis_rev;
    checkpoint.kind = CheckpointKind::Genesis;
    checkpoint.pending_revs = claims.revs();

    let mut entries: Vec<EntryDraft> = baselines
        .iter()
        .map(|b| EntryDraft {
            id: None,
            kind: EntryKind::Baseline,
            author: None,
            started_at: b.created_at,
            ended_at: b.created_at,
            start_rev: None,
            end_rev: None,
            sealed: true,
            inserted: IdSet::new(),
            deleted: IdSet::new(),
            meta: json!({}),
            baseline_id: Some(b.id),
        })
        .collect();

    // The state at rollout: a page that predates `history_since`, whose
    // genesis is not empty and shows something other than the baseline it
    // started from (or there is none).
    let genesis_empty = holds_nothing(replayer.doc());
    if page_created_at < history_since
        && !genesis_empty
        && baseline_content.as_ref() != Some(&visible_content(replayer.doc()))
    {
        entries.push(EntryDraft {
            id: None,
            kind: EntryKind::Baseline,
            author: None,
            started_at: history_since,
            ended_at: history_since,
            start_rev: None,
            end_rev: Some(genesis_rev),
            sealed: true,
            inserted: IdSet::new(),
            deleted: IdSet::new(),
            meta: rollout_meta(),
            baseline_id: None,
        });
    }

    Ok(GenesisOutput {
        checkpoint,
        entries,
        claims,
        refused_revs,
        stats,
    })
}

fn rollout_meta() -> Value {
    json!({ "rollout": true })
}

/// Whether a doc holds nothing at all: no block of any client, nothing
/// pending.
fn holds_nothing(doc: &yrs::Doc) -> bool {
    use yrs::{ReadTxn, Transact};
    let txn = doc.transact();
    txn.state_vector().is_empty() && txn.store().pending_update().is_none()
}
