//! Loading a page from storage: the latest snapshot, then the log rows
//! after it.
//!
//! A room boots with [`BootMode::Room`]; the Markdown refresh, its sweep and
//! `knot-server export-check` load with [`BootMode::ReadOnly`], which never
//! writes.
//!
//! The snapshot is the one with the highest `snapshot_rev`, else (only
//! legacy ones, written before page history M1.2) the highest
//! `snapshot_seq`. After a stamped snapshot the rows with `rev >
//! snapshot_rev` follow; after a legacy snapshot, or none, every row of the
//! page is applied again ([`RowsAfter::All`]; applying is idempotent): a
//! legacy snapshot can lack a row with a lower seq that a 0.9.x room skipped,
//! and reading only the rows past its seq would keep that loss.
//!
//! A room applies only the run of rows contiguous in rev from the snapshot's
//! rev (0 without a stamped snapshot) and stops at a gap, so its watermark
//! (`Boot::through_rev`) is exact. A row that fails to apply is passed: the
//! page stays openable, the run goes on past it, and the room is marked
//! `unlogged` (it writes no snapshot). A read-only load instead fails on such
//! a row, so it never yields a page with part of it missing.

use knot_storage::{HistoryHead, RowsAfter, SnapshotStore, UpdatesStore, UpdatesStoreError};
use thiserror::Error;
use uuid::Uuid;

use crate::engine::{DocHandle, Engine, EngineError};

/// Who loads the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMode {
    /// A room: numbers the page's legacy rows first (its only write),
    /// applies the rows contiguous in rev and passes a row that fails to
    /// apply.
    Room,
    /// A read-only load (Markdown refresh, export-check): writes nothing,
    /// applies every row read and fails on the first that fails to apply.
    ReadOnly,
}

/// A loaded page.
pub struct Boot {
    pub doc: DocHandle,
    /// The page's content epoch (0 when it has no heads row).
    pub content_epoch: i32,
    /// The highest rev such that every rev at or below it is applied (room
    /// mode; 0 in read-only mode).
    pub through_rev: i64,
    /// The seq of the row at `through_rev`, else the snapshot's seq, else 0.
    pub through_seq: i64,
    /// The highest seq applied: the snapshot's and the rows'.
    pub last_seq: i64,
    /// A row failed to apply and was passed (room mode).
    pub unlogged: bool,
}

#[derive(Debug, Error)]
pub enum BootError {
    /// A store read (or the legacy numbering) failed.
    #[error("storage: {0}")]
    Store(String),
    /// The snapshot's bytes failed to apply, or, in read-only mode, a row's.
    #[error("apply: {0}")]
    Apply(EngineError),
}

fn store_error(e: impl std::fmt::Display) -> BootError {
    BootError::Store(e.to_string())
}

/// Whether `e` says the page-history tables or columns do not exist: a
/// database never migrated past 0.9.9, which `knot-server export-check`
/// reads on a staging copy (undefined table 42P01, undefined column 42703).
fn predates_page_history(e: &UpdatesStoreError) -> bool {
    match e {
        UpdatesStoreError::Sqlx(e) => e
            .as_database_error()
            .and_then(|d| d.code())
            .is_some_and(|code| code == "42P01" || code == "42703"),
        _ => false,
    }
}

/// Loads page `doc_id` (module doc).
pub async fn boot(
    engine: &dyn Engine,
    updates: &dyn UpdatesStore,
    snapshots: &dyn SnapshotStore,
    doc_id: Uuid,
    mode: BootMode,
) -> Result<Boot, BootError> {
    if mode == BootMode::Room {
        updates
            .ensure_legacy_numbered(doc_id)
            .await
            .map_err(store_error)?;
    }
    let head = match updates.head(doc_id).await {
        Ok(head) => head,
        // A read-only load of a database from before page history: no
        // heads row exists, so the epoch is 0.
        Err(e) if mode == BootMode::ReadOnly && predates_page_history(&e) => HistoryHead::default(),
        Err(e) => return Err(store_error(e)),
    };
    let snapshot = snapshots.latest(doc_id).await.map_err(store_error)?;

    let doc = engine.new_doc();
    let (base_rev, snapshot_seq, from) = match &snapshot {
        Some(s) => {
            engine
                .apply_update(&doc, &s.state_bytes)
                .map_err(BootError::Apply)?;
            match s.snapshot_rev {
                Some(rev) => (rev, s.snapshot_seq, RowsAfter::Rev(rev)),
                None => (0, s.snapshot_seq, RowsAfter::All),
            }
        }
        None => (0, 0, RowsAfter::All),
    };
    let rows = match updates.rows_after(doc_id, from).await {
        Ok(rows) => rows,
        // The same rows in the same order on such a database: none of them
        // has a rev, and `All` orders unnumbered rows by seq.
        Err(e)
            if mode == BootMode::ReadOnly
                && from == RowsAfter::All
                && predates_page_history(&e) =>
        {
            updates.since(doc_id, 0).await.map_err(store_error)?
        }
        Err(e) => return Err(store_error(e)),
    };

    let mut loaded = Boot {
        doc,
        content_epoch: head.content_epoch,
        through_rev: base_rev,
        through_seq: snapshot_seq,
        last_seq: snapshot_seq,
        unlogged: false,
    };
    match mode {
        BootMode::ReadOnly => {
            loaded.through_rev = 0;
            for row in rows {
                engine
                    .apply_update(&loaded.doc, &row.update_bytes)
                    .map_err(BootError::Apply)?;
                loaded.last_seq = loaded.last_seq.max(row.seq);
            }
        }
        BootMode::Room => {
            for row in rows {
                // Numbering ran first, and every later row gets its rev on
                // insert.
                let Some(rev) = row.rev else {
                    return Err(BootError::Store(format!(
                        "row seq {} has no rev after legacy numbering",
                        row.seq
                    )));
                };
                if rev <= loaded.through_rev {
                    continue;
                }
                if rev != loaded.through_rev + 1 {
                    // Rev order is commit order, so a gap is a bug; the
                    // room's catch-up retries past it.
                    metrics::counter!("knot_room_rev_gap_total").increment(1);
                    tracing::warn!(
                        %doc_id,
                        through_rev = loaded.through_rev,
                        next_rev = rev,
                        "rev gap in the page's log at boot"
                    );
                    break;
                }
                if let Err(error) = engine.apply_update(&loaded.doc, &row.update_bytes) {
                    tracing::warn!(
                        %doc_id,
                        rev,
                        seq = row.seq,
                        %error,
                        "a log row failed to apply at boot; passed, and the room writes no snapshot"
                    );
                    metrics::counter!("knot_room_row_apply_errors_total", "phase" => "boot")
                        .increment(1);
                    loaded.unlogged = true;
                }
                loaded.through_rev = rev;
                loaded.through_seq = row.seq;
                loaded.last_seq = loaded.last_seq.max(row.seq);
            }
        }
    }
    Ok(loaded)
}
