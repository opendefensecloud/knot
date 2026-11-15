//! Loading a page from storage: a snapshot, then the log rows after it.
//!
//! A room boots with [`BootMode::Room`]; the Markdown refresh, its sweep and
//! `knot-server export-check` load with [`BootMode::ReadOnly`], which never
//! writes. Both modes share every rule below; only a room fills
//! [`Boot::parked`] and [`Boot::carry`].
//!
//! A snapshot's bytes leave pending content out from the pending-encoding
//! fix on: the snapshot names the log rows that park it
//! (`DocSnapshot::pending_revs`), and boot applies them again. Snapshots
//! written before the fix ("legacy", `pending_revs` NULL) hold yrs's own
//! encoding, which can write pending blocks at shifted clocks ("phantoms";
//! `crate::pending`). Boot decides on the page's latest snapshot:
//!
//! 0. No snapshot: every row of the page ([`RowsAfter::All`]), over an empty
//!    doc.
//! 1. `pending_revs` set (trusted): the snapshot's bytes, then the rows
//!    after its rev, then the rows it names, in rev order (the live doc held
//!    their content only pending while it applied the rows after the
//!    snapshot). A named row the log lacks, or that fails to apply, is
//!    skipped (`missing_row`); a row the guards refuse falls back
//!    (`underflow`, rule 4).
//! 2. Written by an older pod during the rollout (`older_writer`): the
//!    page's newest trusted snapshot boots by its own rule (1 or 3), and
//!    every fallback starts from its bytes; without one, rule 4 over the
//!    flagged row's bytes. Reason `rollout` either way.
//! 3. Legacy, not flagged: bytes that may hold shifted blocks
//!    (`pending::may_be_shifted`) fall back (`legacy`, rule 4 from their
//!    trusted prefix). Other bytes boot as before: the rows after a stamped
//!    snapshot's rev, or every row of the page after an unstamped one (a
//!    legacy snapshot can lack a row with a lower seq that a 0.9.x room
//!    skipped, and reading only the rows past its seq would keep that loss).
//! 4. Fallback: a new doc from the snapshot's trusted prefix when its bytes
//!    may be shifted (and are not a fixed snapshot's, which are never
//!    shifted), else from its bytes, then every row of the page. Over a
//!    snapshot stamped at R, every row at or below R is applied without the
//!    contiguity stop below (the stamp certifies a missing one), and the
//!    watermark starts at R.
//!
//! Every row boot applies goes through one [`GuardedDoc`] over the boot doc
//! (the skip-underflow guards), and every rule ends with its closing
//! [`GuardedDoc::close`]: yrs's retry of the pending update, forced, then
//! one more apply of each applied row whose ids are still pending. A row after the snapshot or a named row
//! that the guards refuse sends rules 1 and 3 to rule 4. A whole-log row
//! they refuse is skipped when the doc already holds every id it names, and
//! is otherwise a row that fails to apply.
//!
//! A room applies only the run of rows contiguous in rev from its watermark
//! and stops at a gap, so its watermark (`Boot::through_rev`) is exact. A
//! row that fails to apply is passed: the page stays openable, the run goes
//! on past it, and the room is marked `unlogged` (it writes no snapshot). A
//! read-only load instead fails on such a row, so it never yields a page
//! with part of it missing.
//!
//! A boot that did not boot from the latest snapshot as stored, skipped a
//! named row or had a row refused warns and counts
//! `knot_room_boot_fallbacks_total{reason}` once, as it returns `Ok`.

use knot_storage::{
    DocSnapshot, DocUpdate, HistoryHead, RowsAfter, SnapshotStore, UpdatesStore, UpdatesStoreError,
};
use thiserror::Error;
use uuid::Uuid;
use yrs::updates::decoder::Decode;
use yrs::{IdSet, StateVector, Transact, Update};

use crate::engine::{DocHandle, Engine, EngineError};
use crate::pending::{self, GuardedDoc, ParkedRows, PastGap, Refused};

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
    /// The blocks the doc holds past its clients' first gap, after the
    /// closing retry: the live room's guard starts from it (Ruling 25).
    pub past_gap: PastGap,
    /// The rows boot applied whose content the doc holds pending, trimmed
    /// to their cover (room mode, and only when the doc holds pending
    /// content).
    pub parked: ParkedRows,
    /// Rows that carry pending content the snapshot's own bytes held and no
    /// applied row covers (room mode; Ruling 19). The room persists them.
    pub carry: Vec<Vec<u8>>,
    /// How the boot left the latest snapshot's path, if it did.
    pub fallback: Option<BootFallback>,
}

/// A boot that did not boot from the latest snapshot as stored, skipped a
/// named row, or had a row refused (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootFallback {
    /// `legacy`, `rollout`, `missing_row` or `underflow`.
    pub reason: &'static str,
    /// Whether it replayed the page's whole log.
    pub whole_log: bool,
    /// Clocks of the snapshot's blocks below their client's first gap that
    /// the booted doc neither holds nor has pending.
    pub dropped: u64,
    /// The same at or above it: misplaced (shifted) blocks and text past a
    /// lost edit, an upper bound on lost text.
    pub dropped_past_gap: u64,
    /// Whole-log rows the guards refused that the doc already held, skipped.
    pub refused: u64,
}

#[derive(Debug, Error)]
pub enum BootError {
    /// A store read (or the legacy numbering) failed.
    #[error("storage: {0}")]
    Store(String),
    /// The snapshot's bytes failed to apply, or, in read-only mode, a row's
    /// (or the guards refused it).
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
    let latest = snapshots.latest(doc_id).await.map_err(store_error)?;

    let boot = Booter {
        engine,
        updates,
        doc_id,
        mode,
    };
    let loaded = match &latest {
        // Rule 0.
        None => {
            let empty = GuardedDoc::new(engine.new_doc());
            boot.whole_log_over(empty, None, 0).await?
        }
        // Rule 2.
        Some(flagged) if flagged.older_writer => {
            match snapshots
                .latest_trusted(doc_id)
                .await
                .map_err(store_error)?
            {
                Some(trusted) => boot.boot_snapshot(&trusted, Some("rollout")).await?,
                None => boot.fall_back(flagged, "rollout").await?,
            }
        }
        Some(s) => boot.boot_snapshot(s, None).await?,
    };
    Ok(boot.finish(loaded, latest.as_ref().map(|s| s.snapshot_seq), head))
}

/// A doc boot is building, and how it got there.
struct Loaded {
    doc: GuardedDoc,
    through_rev: i64,
    through_seq: i64,
    last_seq: i64,
    unlogged: bool,
    /// The rows applied cleanly, for `ParkedRows::derive` (room mode).
    applied: Vec<(i64, Vec<u8>)>,
    /// `carry_rows` of the base, when the base is a snapshot's own bytes
    /// and they park content (room mode).
    carry: Option<Vec<Vec<u8>>>,
    reason: Option<&'static str>,
    whole_log: bool,
    dropped: u64,
    dropped_past_gap: u64,
    refused: u64,
    /// A row was refused (skipped or passed): `underflow` unless the boot
    /// has another reason.
    any_refused: bool,
}

impl Loaded {
    fn new(doc: GuardedDoc, through_rev: i64, snapshot_seq: i64) -> Loaded {
        Loaded {
            doc,
            through_rev,
            through_seq: snapshot_seq,
            last_seq: snapshot_seq,
            unlogged: false,
            applied: Vec::new(),
            carry: None,
            reason: None,
            whole_log: false,
            dropped: 0,
            dropped_past_gap: 0,
            refused: 0,
            any_refused: false,
        }
    }
}

/// What a rule did: loaded the page, or asked for rule 4 with a reason.
enum Ruled {
    Loaded(Loaded),
    FallBack(&'static str),
}

struct Booter<'a> {
    engine: &'a dyn Engine,
    updates: &'a dyn UpdatesStore,
    doc_id: Uuid,
    mode: BootMode,
}

impl Booter<'_> {
    /// Rules 1 and 3 for snapshot `s`, then rule 4 from `s` when they fall
    /// back. `forced` is rule 2's `rollout`, which the boot reports
    /// whatever the rule reports.
    async fn boot_snapshot(
        &self,
        s: &DocSnapshot,
        forced: Option<&'static str>,
    ) -> Result<Loaded, BootError> {
        let ruled = match &s.pending_revs {
            Some(revs) => self.trusted(s, revs).await?,
            None => self.legacy(s).await?,
        };
        let mut loaded = match ruled {
            Ruled::Loaded(loaded) => loaded,
            Ruled::FallBack(reason) => self.fall_back(s, forced.unwrap_or(reason)).await?,
        };
        if forced.is_some() {
            loaded.reason = forced;
        }
        Ok(loaded)
    }

    /// A new doc holding `bytes`, through the guards.
    fn base(&self, bytes: &[u8]) -> Result<GuardedDoc, BootError> {
        let doc = self.engine.new_doc();
        self.engine
            .apply_update(&doc, bytes)
            .map_err(BootError::Apply)?;
        Ok(GuardedDoc::new(doc))
    }

    /// `carry_rows` of a base built from a snapshot's own bytes, when they
    /// park content (room mode; Ruling 19).
    fn carry_of(&self, base: &GuardedDoc) -> Option<Vec<Vec<u8>>> {
        if self.mode != BootMode::Room {
            return None;
        }
        pending::claims(base.doc())?;
        pending::carry_rows(base.doc())
    }

    /// Rule 1: a snapshot that names the rows of its pending content.
    async fn trusted(&self, s: &DocSnapshot, revs: &[i64]) -> Result<Ruled, BootError> {
        let rev = s.snapshot_rev.unwrap_or(0);
        let mut l = Loaded::new(self.base(&s.state_bytes)?, rev, s.snapshot_seq);
        let rows = self
            .updates
            .rows_after(self.doc_id, RowsAfter::Rev(rev))
            .await
            .map_err(store_error)?;
        if !self.rows_after(&mut l, rows)? {
            return Ok(Ruled::FallBack("underflow"));
        }
        if !revs.is_empty() {
            let named = self
                .updates
                .rows_at(self.doc_id, revs)
                .await
                .map_err(store_error)?;
            let mut missing = revs.len() > named.len();
            for row in named {
                let Some(rev) = row.rev else {
                    missing = true;
                    continue;
                };
                match l.doc.apply(&row.update_bytes) {
                    Ok(()) => {
                        l.last_seq = l.last_seq.max(row.seq);
                        l.applied.push((rev, row.update_bytes));
                    }
                    Err(Refused::Underflow | Refused::Corrupt) => {
                        return Ok(Ruled::FallBack("underflow"));
                    }
                    Err(Refused::Apply(error)) => {
                        tracing::debug!(
                            doc_id = %self.doc_id,
                            rev,
                            %error,
                            "a row the snapshot names failed to apply at boot; skipped"
                        );
                        missing = true;
                    }
                }
            }
            if missing {
                l.reason = Some("missing_row");
            }
        }
        if !self.close_rows_after(&mut l)? {
            return Ok(Ruled::FallBack("underflow"));
        }
        Ok(Ruled::Loaded(l))
    }

    /// Rule 3: a legacy snapshot that is not flagged.
    async fn legacy(&self, s: &DocSnapshot) -> Result<Ruled, BootError> {
        if pending::may_be_shifted(&s.state_bytes) {
            return Ok(Ruled::FallBack("legacy"));
        }
        let base = self.base(&s.state_bytes)?;
        let carry = self.carry_of(&base);
        let Some(rev) = s.snapshot_rev else {
            // Unstamped: every row of the page, as rule 4 replays it.
            let mut l = self.whole_log_over(base, None, s.snapshot_seq).await?;
            l.carry = carry;
            return Ok(Ruled::Loaded(l));
        };
        let mut l = Loaded::new(base, rev, s.snapshot_seq);
        l.carry = carry;
        let rows = self
            .updates
            .rows_after(self.doc_id, RowsAfter::Rev(rev))
            .await
            .map_err(store_error)?;
        if !self.rows_after(&mut l, rows)? || !self.close_rows_after(&mut l)? {
            return Ok(Ruled::FallBack("underflow"));
        }
        Ok(Ruled::Loaded(l))
    }

    /// Rule 4 from snapshot `s` with `reason`.
    async fn fall_back(&self, s: &DocSnapshot, reason: &'static str) -> Result<Loaded, BootError> {
        // A fixed snapshot's bytes are pending-free, so never shifted: its
        // blocks past a gap are real text.
        let prefix = s.pending_revs.is_none() && pending::may_be_shifted(&s.state_bytes);
        let base = if prefix {
            let bytes = pending::trusted_prefix(&s.state_bytes).map_err(BootError::Apply)?;
            self.base(&bytes)?
        } else {
            self.base(&s.state_bytes)?
        };
        let carry = if prefix { None } else { self.carry_of(&base) };
        let stamp = s.snapshot_rev.map(|rev| (rev, s.snapshot_seq));
        let mut l = self.whole_log_over(base, stamp, s.snapshot_seq).await?;
        l.carry = carry;
        l.reason = Some(reason);
        (l.dropped, l.dropped_past_gap) = dropped(&s.state_bytes, l.doc.doc());
        Ok(l)
    }

    /// Every row of the page over `base`, through the guards. `stamp` is the
    /// base snapshot's `(rev, seq)` when it is stamped: the rows at or below
    /// its rev are applied without the contiguity stop.
    async fn whole_log_over(
        &self,
        base: GuardedDoc,
        stamp: Option<(i64, i64)>,
        snapshot_seq: i64,
    ) -> Result<Loaded, BootError> {
        let mut l = Loaded::new(base, 0, snapshot_seq);
        l.whole_log = true;
        let rows = self.all_rows().await?;
        match self.mode {
            BootMode::ReadOnly => {
                for row in rows {
                    let rev = row.rev.unwrap_or(0);
                    self.whole_log_row(&mut l, rev, row)?;
                }
            }
            BootMode::Room => {
                let mut rows = rows.into_iter().peekable();
                if let Some((rev, seq)) = stamp {
                    while let Some(row) = rows.next_if(|r| r.rev.is_none_or(|r| r <= rev)) {
                        let row_rev = self.numbered(&row)?;
                        self.whole_log_row(&mut l, row_rev, row)?;
                    }
                    l.through_rev = rev;
                    l.through_seq = seq;
                }
                for row in rows {
                    let rev = self.numbered(&row)?;
                    if rev <= l.through_rev {
                        continue;
                    }
                    if rev != l.through_rev + 1 {
                        self.rev_gap(l.through_rev, rev);
                        break;
                    }
                    let seq = row.seq;
                    self.whole_log_row(&mut l, rev, row)?;
                    l.through_rev = rev;
                    l.through_seq = seq;
                }
            }
        }
        match close(&mut l) {
            Ok(()) | Err(Refused::Underflow) => {}
            Err(refused) => {
                l.any_refused |= refused == Refused::Corrupt;
                let rev = l.through_rev;
                self.failed_row(&mut l, rev, 0, refused)?;
            }
        }
        if l.any_refused && l.reason.is_none() {
            l.reason = Some("underflow");
        }
        Ok(l)
    }

    /// One whole-log row: applied, skipped when refused and held, else a row
    /// that fails to apply.
    fn whole_log_row(&self, l: &mut Loaded, rev: i64, row: DocUpdate) -> Result<(), BootError> {
        l.last_seq = l.last_seq.max(row.seq);
        match l.doc.apply(&row.update_bytes) {
            Ok(()) => {
                l.applied.push((rev, row.update_bytes));
                Ok(())
            }
            Err(refused @ (Refused::Underflow | Refused::Corrupt)) => {
                l.any_refused = true;
                if refused == Refused::Underflow && l.doc.holds(&row.update_bytes) {
                    l.refused += 1;
                    tracing::debug!(
                        doc_id = %self.doc_id,
                        rev,
                        "the guards refused a row the page already holds; skipped"
                    );
                    return Ok(());
                }
                self.failed_row(l, rev, row.seq, refused)
            }
            Err(refused @ Refused::Apply(_)) => self.failed_row(l, rev, row.seq, refused),
        }
    }

    /// The rows after a stamped snapshot, with the room's contiguity stop
    /// (rules 1 and 3). `false` when the guards refused one.
    fn rows_after(&self, l: &mut Loaded, rows: Vec<DocUpdate>) -> Result<bool, BootError> {
        match self.mode {
            BootMode::ReadOnly => {
                for row in rows {
                    l.last_seq = l.last_seq.max(row.seq);
                    let rev = row.rev.unwrap_or(0);
                    match l.doc.apply(&row.update_bytes) {
                        Ok(()) => l.applied.push((rev, row.update_bytes)),
                        Err(Refused::Underflow | Refused::Corrupt) => return Ok(false),
                        Err(refused @ Refused::Apply(_)) => {
                            self.failed_row(l, rev, row.seq, refused)?;
                        }
                    }
                }
            }
            BootMode::Room => {
                for row in rows {
                    let rev = self.numbered(&row)?;
                    if rev <= l.through_rev {
                        continue;
                    }
                    if rev != l.through_rev + 1 {
                        self.rev_gap(l.through_rev, rev);
                        break;
                    }
                    match l.doc.apply(&row.update_bytes) {
                        Ok(()) => l.applied.push((rev, row.update_bytes)),
                        Err(Refused::Underflow | Refused::Corrupt) => return Ok(false),
                        Err(refused @ Refused::Apply(_)) => {
                            self.failed_row(l, rev, row.seq, refused)?;
                        }
                    }
                    l.through_rev = rev;
                    l.through_seq = row.seq;
                    l.last_seq = l.last_seq.max(row.seq);
                }
            }
        }
        Ok(true)
    }

    /// The close of rules 1 and 3. `false` when the post-check refused it
    /// (a refused row of those rules).
    fn close_rows_after(&self, l: &mut Loaded) -> Result<bool, BootError> {
        match close(l) {
            Ok(()) | Err(Refused::Underflow) => Ok(true),
            Err(Refused::Corrupt) => Ok(false),
            Err(refused @ Refused::Apply(_)) => {
                let rev = l.through_rev;
                self.failed_row(l, rev, 0, refused)?;
                Ok(true)
            }
        }
    }

    /// A row that fails to apply (or that the guards refused and the doc
    /// does not hold): passed in room mode, which then writes no snapshot;
    /// an error in read-only mode.
    fn failed_row(
        &self,
        l: &mut Loaded,
        rev: i64,
        seq: i64,
        refused: Refused,
    ) -> Result<(), BootError> {
        let error = match refused {
            Refused::Apply(error) => error,
            Refused::Underflow | Refused::Corrupt => {
                "the update would corrupt the page (skip underflow)".to_string()
            }
        };
        if self.mode == BootMode::ReadOnly {
            return Err(BootError::Apply(EngineError::Apply(format!(
                "rev {rev}: {error}"
            ))));
        }
        tracing::warn!(
            doc_id = %self.doc_id,
            rev,
            seq,
            %error,
            "a log row failed to apply at boot; passed, and the room writes no snapshot"
        );
        metrics::counter!("knot_room_row_apply_errors_total", "phase" => "boot").increment(1);
        l.unlogged = true;
        Ok(())
    }

    /// Every row of the page. On a database from before page history (a
    /// read-only load), the same rows in the same order: none of them has a
    /// rev, and `All` orders unnumbered rows by seq.
    async fn all_rows(&self) -> Result<Vec<DocUpdate>, BootError> {
        match self.updates.rows_after(self.doc_id, RowsAfter::All).await {
            Ok(rows) => Ok(rows),
            Err(e) if self.mode == BootMode::ReadOnly && predates_page_history(&e) => self
                .updates
                .since(self.doc_id, 0)
                .await
                .map_err(store_error),
            Err(e) => Err(store_error(e)),
        }
    }

    /// A room's row has a rev: numbering ran first, and every later row
    /// gets its rev on insert.
    fn numbered(&self, row: &DocUpdate) -> Result<i64, BootError> {
        row.rev.ok_or_else(|| {
            BootError::Store(format!(
                "row seq {} has no rev after legacy numbering",
                row.seq
            ))
        })
    }

    /// Rev order is commit order, so a gap is a bug; the room's catch-up
    /// retries past it.
    fn rev_gap(&self, through_rev: i64, next_rev: i64) {
        metrics::counter!("knot_room_rev_gap_total").increment(1);
        tracing::warn!(
            doc_id = %self.doc_id,
            through_rev,
            next_rev,
            "rev gap in the page's log at boot"
        );
    }

    /// The boot as it returns: the parked rows and the carried ones (room
    /// mode), the fallback, its warn line and its count.
    fn finish(&self, l: Loaded, snapshot_seq: Option<i64>, head: HistoryHead) -> Boot {
        let (doc, past_gap) = l.doc.into_parts();
        let (mut parked, mut carry) = (ParkedRows::default(), Vec::new());
        if self.mode == BootMode::Room
            && let Some(claims) = pending::claims(&doc)
        {
            let mut applied = l.applied;
            applied.sort_by_key(|(rev, _)| *rev);
            let (rows, covered) = ParkedRows::derive(
                applied.iter().map(|(rev, bytes)| (*rev, bytes.as_slice())),
                &claims,
            );
            parked = rows;
            if covered.is_err() {
                carry = l.carry.unwrap_or_default();
            }
        }
        let fallback = l.reason.map(|reason| BootFallback {
            reason,
            whole_log: l.whole_log,
            dropped: l.dropped,
            dropped_past_gap: l.dropped_past_gap,
            refused: l.refused,
        });
        if let Some(f) = &fallback {
            self.report(f, snapshot_seq);
        }
        Boot {
            doc,
            content_epoch: head.content_epoch,
            through_rev: match self.mode {
                BootMode::Room => l.through_rev,
                BootMode::ReadOnly => 0,
            },
            through_seq: l.through_seq,
            last_seq: l.last_seq,
            unlogged: l.unlogged,
            past_gap,
            parked,
            carry,
            fallback,
        }
    }

    /// The fallback's warn line and count (Global Constraints).
    fn report(&self, f: &BootFallback, snapshot_seq: Option<i64>) {
        let doc_id = self.doc_id;
        let BootFallback {
            reason,
            whole_log,
            dropped,
            dropped_past_gap,
            refused,
        } = *f;
        match (reason, snapshot_seq) {
            ("legacy" | "rollout", Some(snapshot_seq)) => tracing::warn!(
                %doc_id, snapshot_seq, reason, whole_log, dropped, dropped_past_gap, refused,
                "the page's latest snapshot cannot be trusted; booting without it"
            ),
            ("legacy" | "rollout", None) => tracing::warn!(
                %doc_id, reason, whole_log, dropped, dropped_past_gap, refused,
                "the page's latest snapshot cannot be trusted; booting without it"
            ),
            (_, Some(snapshot_seq)) => tracing::warn!(
                %doc_id, snapshot_seq, reason, whole_log, dropped, dropped_past_gap, refused,
                "the page booted on a fallback path"
            ),
            (_, None) => tracing::warn!(
                %doc_id, reason, whole_log, dropped, dropped_past_gap, refused,
                "the page booted on a fallback path"
            ),
        }
        metrics::counter!("knot_room_boot_fallbacks_total", "reason" => reason).increment(1);
    }
}

/// Every rule's end: the closing retry, then the re-apply pass over the
/// rows boot applied, in rev order ([`GuardedDoc::close`]).
fn close(l: &mut Loaded) -> Result<(), Refused> {
    let mut rows: Vec<&(i64, Vec<u8>)> = l.applied.iter().collect();
    rows.sort_by_key(|(rev, _)| *rev);
    l.doc
        .close(rows.into_iter().map(|(_, bytes)| bytes.as_slice()))
}

/// The clocks of `bytes`' blocks that `doc` neither holds nor has pending:
/// below their client's first gap in the bytes, and at or above it.
fn dropped(bytes: &[u8], doc: &DocHandle) -> (u64, u64) {
    let Ok(update) = Update::decode_v1(bytes) else {
        return (0, 0);
    };
    let state = pending::encode_without_pending(&doc.inner().transact(), &StateVector::default());
    let held = Update::decode_v1(&state)
        .map(|u| u.insertions(true))
        .unwrap_or_default();
    let pending = pending::claims(doc).map(|c| c.structs).unwrap_or_default();
    let have = held.merge(&pending);
    let lost = update.insertions(true).diff(&have);
    let past = pending::at_or_above(&lost, &update.state_vector());
    let below = lost.diff(&past);
    (clocks(&below), clocks(&past))
}

/// The number of clocks `set` holds.
fn clocks(set: &IdSet) -> u64 {
    set.iter()
        .map(|(_, ranges)| {
            ranges
                .iter()
                .map(|r| u64::from(r.end - r.start))
                .sum::<u64>()
        })
        .sum()
}
