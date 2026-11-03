//! Recovery for suggestions stuck in `applying`.
//!
//! Accept claims a row (`pending → applying`), has the doc's room apply the
//! change, then marks the row `accepted`. A crash, a room that times out or
//! a failed persist can stop it between the claim and the finish, leaving
//! the row `applying` with nobody to settle it. Every 60 s this sweep takes
//! the rows that have been `applying` for longer than [`STUCK_AFTER`] and
//! asks the doc's room whether the doc records the suggestion as applied
//! under the row's `apply_token` (`knot_crdt::Event::SuggestionApplied`).
//! The room writes that marker in the same transaction as the text, so it is
//! the single source of truth; a marker with another value is one a client
//! wrote (any Editor can write the map) and reads as not applied:
//!
//! - applied → `finish_accept`, so the row says `accepted` like the text,
//!   and the author gets the `suggestion_decided` row the accept never sent;
//! - not applied → `release_if_unchanged`, back to `pending`, compared and
//!   set on the `updated_at` the sweep read, so a row an accept claimed
//!   again meanwhile is left alone;
//! - no answer (the room could not catch up on storage) → skipped until the
//!   next run.
//!
//! Runs on every replica with no leader election: two sweeps settling the
//! same row both read the same marker, `finish_accept` refuses a row that is
//! already `accepted`, and only one compare-and-set release can win.
//!
//! **Ordering.** Accept spends at most 20 s from its claim on queueing the
//! event and waiting for the reply, under the 30 s request timeout and well
//! under [`STUCK_AFTER`] (120 s). It does that in a task of its own, so a
//! request the timeout cancels still settles its claim; only a crash leaves
//! the row to this sweep.
//!
//! **Residual window.** If an `ApplySuggestion` event is still queued in a
//! room on another replica after 120 s, the sweep may read "not applied" and
//! release the row, and the queued event then applies the text with nobody
//! left to finish the row. The row can then stay `pending` while the text is
//! applied (a known v1 gap). It converges only if someone accepts it again:
//! the claim keeps the row's `apply_token`, so the marker makes that apply
//! `AlreadyApplied`, and the accept (or a later sweep, if that claim gets
//! stuck) finishes the row without touching the text a second time. Nothing
//! prompts that accept, though: the applied text leaves the anchors on
//! deleted items, so the card shows the suggestion as removed and offers no
//! Accept.

use chrono::{DateTime, Utc};
use knot_storage::{Suggestion, SuggestionStoreError};
use tokio::task::JoinHandle;

use crate::AppState;

/// How often the sweep runs.
const INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a row must have been `applying` before the sweep settles it.
/// Accept gives its room 20 s, so a row this old has no accept waiting.
pub const STUCK_AFTER: chrono::Duration = chrono::Duration::seconds(120);

/// How long the sweep waits for a room to answer whether a suggestion was
/// applied before it skips the row until the next run.
const ROOM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What one [`run_once`] settled.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    /// Rows the doc records as applied, now `accepted`.
    pub finished: usize,
    /// Rows the doc does not record as applied, now `pending` again.
    pub released: usize,
}

/// Settle every suggestion that has been `applying` since before
/// `now - STUCK_AFTER`. A state without a suggestion store or doc rooms has
/// nothing to settle. A row whose room cannot be reached or does not answer
/// is skipped and retried on the next run; a database error ends the run.
pub async fn run_once(
    state: &AppState,
    now: DateTime<Utc>,
) -> Result<SweepOutcome, SuggestionStoreError> {
    let mut out = SweepOutcome::default();
    let (Some(store), Some(rooms)) = (state.suggestions.clone(), state.rooms_v2.clone()) else {
        return Ok(out);
    };
    for row in store.list_stuck_applying(now - STUCK_AFTER).await? {
        let Some(applied) = applied_in_doc(&rooms, &row).await else {
            continue;
        };
        if applied {
            let Some(decided_by) = row.decided_by else {
                // A claim always sets `decided_by`; without it there is no
                // owner to credit with the decision.
                tracing::error!(suggestion_id = %row.id, "applying suggestion without decided_by");
                continue;
            };
            match store.finish_accept(row.id, decided_by).await {
                Ok(finished) => {
                    out.finished += 1;
                    crate::routes::api::suggestions::notify_suggestion_change(state, row.doc_id);
                    // The accept that claimed this row never got to tell the
                    // author; the dedupe key keeps it to one row if it did.
                    crate::suggestion_notifications::emit_suggestion_decided_from_sweep(
                        state, &finished,
                    )
                    .await;
                }
                // Settled by someone else meanwhile.
                Err(SuggestionStoreError::NotPending | SuggestionStoreError::NotFound) => {}
                Err(e) => return Err(e),
            }
        } else if store.release_if_unchanged(row.id, row.updated_at).await? {
            out.released += 1;
            crate::routes::api::suggestions::notify_suggestion_change(state, row.doc_id);
        }
    }
    Ok(out)
}

/// Ask the doc's room whether it records `row` as applied under the row's
/// `apply_token`. `None` when the room cannot be reached or does not answer
/// in time.
async fn applied_in_doc(rooms: &knot_crdt::Rooms, row: &Suggestion) -> Option<bool> {
    let suggestion_id = row.id;
    let Some(apply_token) = row.apply_token else {
        // Every claim mints a token and the room records exactly that one,
        // so a row without one was never applied.
        return Some(false);
    };
    let room = match rooms.acquire(row.doc_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error=?e, %suggestion_id, doc_id = %row.doc_id, "suggestion sweep: room acquire failed");
            return None;
        }
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(knot_crdt::Event::SuggestionApplied {
            suggestion_id,
            apply_token,
            reply: tx,
        })
        .await
        .ok()?;
    match tokio::time::timeout(ROOM_TIMEOUT, rx).await {
        Ok(Ok(applied)) => Some(applied),
        // The room drops the reply when it cannot catch up on storage.
        Ok(Err(_)) | Err(_) => {
            tracing::warn!(%suggestion_id, "suggestion sweep: no answer from the room; retrying next run");
            None
        }
    }
}

/// Spawn the 60-second loop. One per process; every replica runs its own.
pub fn spawn(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        // The first tick fires immediately; skip it so a rolling restart
        // doesn't have every pod sweep at once on boot.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match run_once(&state, Utc::now()).await {
                Ok(out) => tracing::debug!(
                    finished = out.finished,
                    released = out.released,
                    "suggestion sweep"
                ),
                Err(e) => tracing::warn!(error=?e, "suggestion sweep failed"),
            }
        }
    })
}
