//! The builder's SQL: claims, leases, the reads of a pass, its writes and
//! the candidate queries. Crate-private; [`crate::builder`] drives it.
//!
//! Lock order (M1.2, binding): the page row's `FOR KEY SHARE` first, then
//! `doc_history_state`, then `doc_history_heads`; checkpoints and entries
//! are written only under the state row's `FOR UPDATE` by the holder of the
//! page's lease (`claimed_by`), or by `give_up`'s claim form while nobody
//! holds it. No transaction stays open across a compute: reads run in short
//! `REPEATABLE READ READ ONLY` transactions, every write re-checks the lease.
//!
//! `doc_history_state.retry_after` is never decoded: `'infinity'` overflows
//! chrono. Every read computes what it needs from it in SQL, as booleans.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use knot_storage::{DocUpdate, UpdateSource, rows_in_range};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;
use yrs::IdSet;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;

use crate::builder::{BUILDER_VERSION, MAX_CRASHES, MAX_FAILURES, MAX_TIMEOUTS};
use crate::claims::{Claims, ClaimsError};
use crate::genesis::{BaselineInput, GenesisOutput};
use crate::pass::{CheckpointKind, CheckpointWrite, KeptGap, PassOutput, SnapshotProbe};
use crate::sessions::{EntryDraft, EntryKind, SEAL_MARGIN, SESSION_GAP};
use crate::words::WordCounts;

type Tx = Transaction<'static, Postgres>;

/// `last_error` while a claim's pass runs: the crash marker.
pub(crate) const PASS_INTERRUPTED: &str = "pass interrupted";

/// What a graceful shutdown writes instead (not a crash).
pub(crate) const SHUTDOWN: &str = "shutdown";

const PAGE_KEY_SHARE: &str = "SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE";

/// The state row, under `FOR UPDATE`, while `$2` holds a live lease.
const LEASED_STATE: &str = "SELECT built_rev, genesis_rev FROM doc_history_state
                             WHERE doc_id = $1 AND claimed_by = $2 AND claimed_until > now()
                             FOR UPDATE";

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("parked claims: {0}")]
    Claims(#[from] ClaimsError),
    #[error("{0}")]
    Invalid(String),
}

impl StoreError {
    /// SQLSTATE 55P03: a lock wait hit `lock_timeout`.
    pub(crate) fn is_lock_timeout(&self) -> bool {
        match self {
            StoreError::Sqlx(e) => e
                .as_database_error()
                .and_then(|d| d.code())
                .is_some_and(|c| c == "55P03"),
            _ => false,
        }
    }
}

type Result<T> = std::result::Result<T, StoreError>;

async fn begin(pool: &PgPool) -> Result<Tx> {
    Ok(knot_storage::begin(pool).await?)
}

/// A short read transaction: one snapshot for every read of a pass.
async fn begin_read(pool: &PgPool) -> Result<Tx> {
    let mut tx = begin(pool).await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// A write transaction: `lock_timeout`, then the page row's KEY SHARE.
/// `None`: the page is gone.
async fn begin_page_write(pool: &PgPool, doc_id: Uuid, lock_timeout: &str) -> Result<Option<Tx>> {
    let mut tx = begin(pool).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL lock_timeout = '{lock_timeout}'"
    )))
    .execute(&mut *tx)
    .await?;
    let page: Option<i32> = sqlx::query_scalar(PAGE_KEY_SHARE)
        .bind(doc_id)
        .fetch_optional(&mut *tx)
        .await?;
    Ok(page.map(|_| tx))
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

/// How a lease-checked write ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Write {
    Done,
    /// Someone else holds the page's lease (or ours expired): nothing was
    /// written.
    LeaseLost,
    Gone,
}

/// A lease-checked write transaction: `None` (rolled back) when the page is
/// gone or the lease is not ours; else the transaction and the state row's
/// `built_rev` and `genesis_rev`.
async fn begin_leased(
    pool: &PgPool,
    doc_id: Uuid,
    me: Uuid,
) -> Result<std::result::Result<(Tx, i64, Option<i64>), Write>> {
    let Some(mut tx) = begin_page_write(pool, doc_id, "10s").await? else {
        return Ok(Err(Write::Gone));
    };
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(LEASED_STATE)
        .bind(doc_id)
        .bind(me)
        .fetch_optional(&mut *tx)
        .await?;
    match row {
        Some((built_rev, genesis_rev)) => Ok(Ok((tx, built_rev, genesis_rev))),
        None => Ok(Err(Write::LeaseLost)),
    }
}

// ---------------------------------------------------------------------------
// Claim, lease, give-up, failure, release
// ---------------------------------------------------------------------------

/// What a claim found.
#[derive(Debug)]
pub(crate) enum Claim {
    Gone,
    Busy,
    Deferred,
    Clean,
    /// Given up in the claim's transaction (`give_up`'s claim form).
    GaveUp {
        reason: &'static str,
        message: String,
    },
    Claimed(Claimed),
}

/// The state row a claim took.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Claimed {
    pub genesis_rev: Option<i64>,
    pub builder_version: i32,
}

type StateRead = (
    i64,
    Option<i64>,
    i32,
    i64,
    i32,
    i32,
    i32,
    Option<String>,
    bool,
    bool,
    bool,
);

/// The claim (Global Constraints, builder pass step 1).
pub(crate) async fn claim(pool: &PgPool, doc_id: Uuid, me: Uuid, lease: Duration) -> Result<Claim> {
    let mut tx = begin(pool).await?;
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(&mut *tx)
        .await?;
    let page: Option<i32> = sqlx::query_scalar(PAGE_KEY_SHARE)
        .bind(doc_id)
        .fetch_optional(&mut *tx)
        .await?;
    if page.is_none() {
        return Ok(Claim::Gone);
    }
    sqlx::query(
        "INSERT INTO doc_history_state (doc_id) VALUES ($1) ON CONFLICT (doc_id) DO NOTHING",
    )
    .bind(doc_id)
    .execute(&mut *tx)
    .await?;
    let row: Option<StateRead> = sqlx::query_as(
        "SELECT built_rev, genesis_rev, builder_version, snapshot_checked_rev,
                failures, timeouts, crashes, last_error,
                coalesce(retry_after = 'infinity', false) AS given_up,
                coalesce(retry_after > now(), false) AS deferred,
                coalesce(claimed_until > now(), false) AS leased
           FROM doc_history_state WHERE doc_id = $1
            FOR UPDATE SKIP LOCKED",
    )
    .bind(doc_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((
        built_rev,
        genesis_rev,
        builder_version,
        snapshot_checked_rev,
        mut failures,
        mut timeouts,
        mut crashes,
        last_error,
        given_up,
        mut deferred,
        leased,
    )) = row
    else {
        return Ok(Claim::Busy);
    };

    // The version lift, before every other check (Rulings): a page an older
    // builder built or gave up starts over with clean counts.
    if builder_version < BUILDER_VERSION && (given_up || genesis_rev.is_some()) {
        failures = 0;
        timeouts = 0;
        crashes = 0;
        deferred = false;
    }
    if builder_version > BUILDER_VERSION || deferred {
        tx.commit().await?;
        return Ok(Claim::Deferred);
    }
    if leased {
        tx.commit().await?;
        return Ok(Claim::Busy);
    }
    if last_error.as_deref() == Some(PASS_INTERRUPTED) {
        // The previous claim's pass died with its process.
        crashes += 1;
    }
    let gave_up = if failures >= MAX_FAILURES {
        Some((
            "failures",
            format!(
                "given up after {failures} failed passes (last: {})",
                last_error.as_deref().unwrap_or("none")
            ),
        ))
    } else if crashes >= MAX_CRASHES {
        Some((
            "crash",
            format!("given up after {crashes} passes that crashed the process"),
        ))
    } else {
        None
    };
    if let Some((reason, message)) = gave_up {
        give_up_in(&mut tx, doc_id, None, &message).await?;
        tx.commit().await?;
        return Ok(Claim::GaveUp { reason, message });
    }

    // The clean check (Rulings): nothing to do, no lease taken.
    if genesis_rev.is_some() && builder_version == BUILDER_VERSION {
        let (head_rev, dirty, late_snapshot, idle_session): (i64, bool, bool, bool) =
            sqlx::query_as(
                "SELECT coalesce((SELECT head_rev FROM doc_history_heads WHERE doc_id = $1), 0),
                        coalesce((SELECT dirty FROM doc_history_heads WHERE doc_id = $1), false),
                        EXISTS (SELECT 1 FROM doc_snapshots
                                 WHERE doc_id = $1 AND snapshot_rev > $2
                                   AND snapshot_rev <> ALL (
                                       SELECT unnest(skip_snapshots) FROM doc_history_state
                                        WHERE doc_id = $1)),
                        EXISTS (SELECT 1 FROM doc_history_entries
                                 WHERE doc_id = $1 AND NOT sealed
                                   AND ended_at < now() - make_interval(secs => $3))",
            )
            .bind(doc_id)
            .bind(snapshot_checked_rev)
            .bind((SESSION_GAP + SEAL_MARGIN).num_seconds() as f64)
            .fetch_one(&mut *tx)
            .await?;
        if !dirty && built_rev == head_rev && !late_snapshot && !idle_session {
            tx.commit().await?;
            return Ok(Claim::Clean);
        }
    }

    // The lease, with the crash marker (Rulings).
    let failures = failures + 1;
    let backoff = crate::builder::failure_backoff(failures - 1);
    sqlx::query(
        "UPDATE doc_history_state
            SET claimed_by = $2, claimed_until = now() + make_interval(secs => $3),
                failures = $4, timeouts = $5, crashes = $6,
                retry_after = now() + make_interval(secs => $7),
                last_error = $8, updated_at = now()
          WHERE doc_id = $1",
    )
    .bind(doc_id)
    .bind(me)
    .bind(secs(lease))
    .bind(failures)
    .bind(timeouts)
    .bind(crashes)
    .bind(backoff.num_seconds() as f64)
    .bind(PASS_INTERRUPTED)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Claim::Claimed(Claimed {
        genesis_rev,
        builder_version,
    }))
}

/// Extends this builder's lease by `lease`, before a compute.
pub(crate) async fn renew_lease(
    pool: &PgPool,
    doc_id: Uuid,
    me: Uuid,
    lease: Duration,
) -> Result<Write> {
    let (mut tx, _, _) = match begin_leased(pool, doc_id, me).await? {
        Ok(t) => t,
        Err(w) => return Ok(w),
    };
    sqlx::query(
        "UPDATE doc_history_state SET claimed_until = now() + make_interval(secs => $3)
          WHERE doc_id = $1 AND claimed_by = $2 AND claimed_until > now()",
    )
    .bind(doc_id)
    .bind(me)
    .bind(secs(lease))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Write::Done)
}

/// Gives the page up in the caller's transaction (which holds the page's
/// KEY SHARE): `me` `None` is the claim form (no `claimed_by` predicate;
/// nobody holds the lease), `Some` the after-claim form. When genesis was
/// never built, also lists the page's baselines (Rulings). Returns whether
/// the state row matched.
async fn give_up_in(
    conn: &mut PgConnection,
    doc_id: Uuid,
    me: Option<Uuid>,
    message: &str,
) -> Result<bool> {
    let before_genesis: Option<bool> = sqlx::query_scalar(
        "UPDATE doc_history_state
            SET retry_after = 'infinity',
                builder_version = CASE WHEN genesis_rev IS NULL THEN $3 ELSE builder_version END,
                last_error = $2, claimed_by = NULL, claimed_until = NULL, updated_at = now()
          WHERE doc_id = $1 AND ($4::uuid IS NULL OR claimed_by = $4)
      RETURNING genesis_rev IS NULL",
    )
    .bind(doc_id)
    .bind(message)
    .bind(BUILDER_VERSION)
    .bind(me)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(before_genesis) = before_genesis else {
        return Ok(false);
    };
    if before_genesis {
        sqlx::query(
            "INSERT INTO doc_history_entries
                 (doc_id, kind, author_id, started_at, ended_at, sealed, baseline_id)
             SELECT b.doc_id, 'baseline', NULL, b.created_at, b.created_at, true, b.id
               FROM doc_history_baselines b
              WHERE b.doc_id = $1
                AND NOT EXISTS (SELECT 1 FROM doc_history_entries e WHERE e.baseline_id = b.id)
              ORDER BY b.snapshot_seq",
        )
        .bind(doc_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(true)
}

/// `give_up`'s after-claim form in its own transaction. Whether it matched
/// (the lease was still ours).
pub(crate) async fn give_up(pool: &PgPool, doc_id: Uuid, me: Uuid, message: &str) -> Result<bool> {
    let Some(mut tx) = begin_page_write(pool, doc_id, "10s").await? else {
        return Ok(false);
    };
    let held: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM doc_history_state WHERE doc_id = $1 AND claimed_by = $2 FOR UPDATE",
    )
    .bind(doc_id)
    .bind(me)
    .fetch_optional(&mut *tx)
    .await?;
    if held.is_none() {
        return Ok(false);
    }
    let matched = give_up_in(&mut tx, doc_id, Some(me), message).await?;
    tx.commit().await?;
    Ok(matched)
}

/// What a failed pass changes besides `last_error` (Task 7 step 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureAction {
    None,
    SkipRev(i64),
    SkipSnapshot(i64),
    SkipBaseline(i64),
    DropCheckpoint(i64),
    DropKeptGap(i64),
    Rebuild { drop_gap: Option<i64> },
}

/// How `record_failure` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// The lease is not ours: nothing written.
    NoMatch,
    Done,
    /// The second timeout with nothing to blame gave the page up.
    GaveUp,
}

/// Records a failed pass (Rulings, "a failed pass"): the claim's crash
/// marker stays, the lease is released, `last_error` names the failure and
/// `action` applies. `timed_out` with `FailureAction::None` counts a
/// timeout, and the second gives the page up in the same transaction.
pub(crate) async fn record_failure(
    pool: &PgPool,
    doc_id: Uuid,
    me: Uuid,
    message: &str,
    action: FailureAction,
    timed_out: bool,
) -> Result<Recorded> {
    let Some(mut tx) = begin_page_write(pool, doc_id, "1s").await? else {
        return Ok(Recorded::NoMatch);
    };
    let row: Option<(i64, i32)> = sqlx::query_as(
        "SELECT built_rev, timeouts FROM doc_history_state
          WHERE doc_id = $1 AND claimed_by = $2 FOR UPDATE",
    )
    .bind(doc_id)
    .bind(me)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((built_rev, timeouts)) = row else {
        return Ok(Recorded::NoMatch);
    };
    let count_timeout = timed_out && action == FailureAction::None;
    if count_timeout && timeouts + 1 >= MAX_TIMEOUTS {
        sqlx::query("UPDATE doc_history_state SET timeouts = timeouts + 1 WHERE doc_id = $1")
            .bind(doc_id)
            .execute(&mut *tx)
            .await?;
        let message = format!("given up after {} replay timeouts: {message}", timeouts + 1);
        give_up_in(&mut tx, doc_id, Some(me), &message).await?;
        tx.commit().await?;
        return Ok(Recorded::GaveUp);
    }
    // A snapshot whose probe failed is left out by its rev alone:
    // `snapshot_checked_rev` stays, so the snapshots below it that the
    // failed pass had gap-checked (its output is discarded) are probed again.
    let (skip_rev, skip_snapshot, skip_baseline, rebuild) = match action {
        FailureAction::SkipRev(rev) => (Some(rev), None, None, false),
        FailureAction::SkipSnapshot(rev) => (None, Some(rev), None, false),
        FailureAction::SkipBaseline(id) => (None, None, Some(id), false),
        FailureAction::Rebuild { .. } => (None, None, None, true),
        _ => (None, None, None, false),
    };
    sqlx::query(
        "UPDATE doc_history_state
            SET last_error = $2, claimed_by = NULL, claimed_until = NULL, updated_at = now(),
                timeouts = timeouts + $3,
                skip_revs = CASE WHEN $4::bigint IS NULL OR $4 = ANY(skip_revs) THEN skip_revs
                                 ELSE array_append(skip_revs, $4) END,
                skip_snapshots = CASE WHEN $5::bigint IS NULL OR $5 = ANY(skip_snapshots)
                                      THEN skip_snapshots
                                      ELSE array_append(skip_snapshots, $5) END,
                skip_baselines = CASE WHEN $6::bigint IS NULL OR $6 = ANY(skip_baselines)
                                      THEN skip_baselines
                                      ELSE array_append(skip_baselines, $6) END,
                builder_version = CASE WHEN $7 THEN 0 ELSE builder_version END
          WHERE doc_id = $1",
    )
    .bind(doc_id)
    .bind(message)
    .bind(i32::from(count_timeout))
    .bind(skip_rev)
    .bind(skip_snapshot)
    .bind(skip_baseline)
    .bind(rebuild)
    .execute(&mut *tx)
    .await?;
    let dropped = match action {
        FailureAction::DropCheckpoint(rev) => Some((rev, "periodic")),
        FailureAction::DropKeptGap(rev) if rev > built_rev => Some((rev, "gap")),
        FailureAction::Rebuild {
            drop_gap: Some(rev),
        } => Some((rev, "gap")),
        _ => None,
    };
    if let Some((rev, kind)) = dropped {
        sqlx::query(
            "DELETE FROM doc_history_checkpoints WHERE doc_id = $1 AND rev = $2 AND kind = $3",
        )
        .bind(doc_id)
        .bind(rev)
        .bind(kind)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Recorded::Done)
}

/// The release in the caller's lease-checked transaction: clears the crash
/// marker and the lease, then the heads row's `dirty` (Global Constraints
/// step 7).
async fn release_in(conn: &mut PgConnection, doc_id: Uuid, built_rev: i64) -> Result<()> {
    sqlx::query(
        "UPDATE doc_history_state
            SET failures = 0, timeouts = 0, crashes = 0, retry_after = NULL, last_error = NULL,
                claimed_by = NULL, claimed_until = NULL, updated_at = now()
          WHERE doc_id = $1",
    )
    .bind(doc_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE doc_history_heads
            SET dirty = (head_rev > $2
                         OR EXISTS (SELECT 1 FROM doc_history_entries
                                     WHERE doc_id = $1 AND words_pending))
          WHERE doc_id = $1",
    )
    .bind(doc_id)
    .bind(built_rev)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The release in a transaction of its own (step 10).
pub(crate) async fn release(pool: &PgPool, doc_id: Uuid, me: Uuid) -> Result<Write> {
    let (mut tx, built_rev, _) = match begin_leased(pool, doc_id, me).await? {
        Ok(t) => t,
        Err(w) => return Ok(w),
    };
    release_in(&mut tx, doc_id, built_rev).await?;
    tx.commit().await?;
    Ok(Write::Done)
}

/// A graceful shutdown's release of the pages this builder had in flight:
/// not a crash, so the claim's `failures` is taken back.
pub(crate) async fn release_on_shutdown(pool: &PgPool, me: Uuid, ids: &[Uuid]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut tx = begin(pool).await?;
    sqlx::query("SET LOCAL lock_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT 1 FROM documents WHERE id = ANY($1) ORDER BY id FOR KEY SHARE")
        .bind(ids)
        .execute(&mut *tx)
        .await?;
    let done = sqlx::query(
        "UPDATE doc_history_state
            SET claimed_by = NULL, claimed_until = NULL, failures = greatest(failures - 1, 0),
                retry_after = NULL, last_error = $3, updated_at = now()
          WHERE doc_id = ANY($1) AND claimed_by = $2",
    )
    .bind(ids)
    .bind(me)
    .bind(SHUTDOWN)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(done.rows_affected())
}

/// Starts a rebuild (Rulings): deletes the page's entries except `erase`
/// ones and its genesis and periodic checkpoints (gap checkpoints are
/// source data and stay), resets the state row's build columns
/// (`skip_revs`, `skip_baselines` and `skip_snapshots` stay) and marks the
/// page dirty.
pub(crate) async fn reset_for_rebuild(pool: &PgPool, doc_id: Uuid, me: Uuid) -> Result<Write> {
    let (mut tx, _, _) = match begin_leased(pool, doc_id, me).await? {
        Ok(t) => t,
        Err(w) => return Ok(w),
    };
    sqlx::query("DELETE FROM doc_history_entries WHERE doc_id = $1 AND kind <> 'erase'")
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM doc_history_checkpoints
          WHERE doc_id = $1 AND kind IN ('genesis', 'periodic')",
    )
    .bind(doc_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE doc_history_state
            SET built_rev = 0, genesis_rev = NULL, snapshot_checked_rev = 0, parked = '[]',
                updated_at = now()
          WHERE doc_id = $1",
    )
    .bind(doc_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE doc_history_heads SET dirty = true WHERE doc_id = $1")
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Write::Done)
}

// ---------------------------------------------------------------------------
// Genesis
// ---------------------------------------------------------------------------

/// What genesis loaded.
pub(crate) struct GenesisLoad {
    pub page_created_at: DateTime<Utc>,
    pub history_since: DateTime<Utc>,
    pub genesis_rev: i64,
    /// Every baseline, ascending by `snapshot_seq`; only the one genesis
    /// starts from carries its bytes.
    pub baselines: Vec<BaselineInput>,
    /// The baseline genesis starts from.
    pub start: Option<i64>,
    pub rows: Vec<DocUpdate>,
    pub skip_revs: BTreeSet<i64>,
    /// The bytes loaded (the baseline and the rows).
    pub bytes: i64,
}

pub(crate) enum GenesisLoaded {
    Gone,
    TooLarge(i64),
    Load(GenesisLoad),
}

/// Genesis's inputs (step 5): the sizes are checked in SQL before any bytes
/// are read. Starts from the newest baseline that is neither in
/// `skip_baselines` nor in `rejected` (baselines a previous attempt of this
/// claim found not to apply).
pub(crate) async fn load_genesis_inputs(
    pool: &PgPool,
    doc_id: Uuid,
    cap: i64,
    rejected: &[i64],
) -> Result<GenesisLoaded> {
    let mut tx = begin_read(pool).await?;
    let page: Option<(DateTime<Utc>, DateTime<Utc>)> =
        sqlx::query_as("SELECT created_at, history_since FROM documents WHERE id = $1")
            .bind(doc_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((page_created_at, history_since)) = page else {
        return Ok(GenesisLoaded::Gone);
    };
    let (skip_revs, skip_baselines): (Vec<i64>, Vec<i64>) =
        sqlx::query_as("SELECT skip_revs, skip_baselines FROM doc_history_state WHERE doc_id = $1")
            .bind(doc_id)
            .fetch_one(&mut *tx)
            .await?;
    let genesis_rev: i64 = sqlx::query_scalar(
        "SELECT coalesce(max(rev), 0) FROM doc_updates WHERE doc_id = $1 AND created_at < $2",
    )
    .bind(doc_id)
    .bind(history_since)
    .fetch_one(&mut *tx)
    .await?;
    let metas: Vec<(i64, i64, DateTime<Utc>, i64)> = sqlx::query_as(
        "SELECT id, snapshot_seq, created_at, octet_length(state_bytes)::bigint
           FROM doc_history_baselines WHERE doc_id = $1 ORDER BY snapshot_seq, id",
    )
    .bind(doc_id)
    .fetch_all(&mut *tx)
    .await?;
    let rows_bytes: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(octet_length(update_bytes)), 0)::bigint
           FROM doc_updates WHERE doc_id = $1 AND rev <= $2",
    )
    .bind(doc_id)
    .bind(genesis_rev)
    .fetch_one(&mut *tx)
    .await?;
    let start = metas
        .iter()
        .rev()
        .find(|(id, ..)| !skip_baselines.contains(id) && !rejected.contains(id))
        .map(|&(id, _, _, size)| (id, size));
    let bytes = rows_bytes + start.map_or(0, |(_, size)| size);
    if bytes > cap {
        return Ok(GenesisLoaded::TooLarge(bytes));
    }
    let start_bytes: Option<Vec<u8>> = match start {
        Some((id, _)) => Some(
            sqlx::query_scalar("SELECT state_bytes FROM doc_history_baselines WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?,
        ),
        None => None,
    };
    let rows = rows_in_range(&mut tx, doc_id, 0, Some(genesis_rev), i64::MAX, i64::MAX)
        .await
        .map_err(updates_error)?;
    tx.commit().await?;
    let mut start_bytes = start_bytes;
    let baselines = metas
        .into_iter()
        .map(|(id, snapshot_seq, created_at, _)| BaselineInput {
            id,
            snapshot_seq,
            state_bytes: if start.is_some_and(|(s, _)| s == id) {
                start_bytes.take()
            } else {
                None
            },
            created_at,
        })
        .collect();
    Ok(GenesisLoaded::Load(GenesisLoad {
        page_created_at,
        history_since,
        genesis_rev,
        baselines,
        start: start.map(|(id, _)| id),
        rows,
        skip_revs: skip_revs.into_iter().collect(),
        bytes,
    }))
}

/// Writes genesis (step 5's write transaction) and releases the page.
pub(crate) async fn write_genesis(
    pool: &PgPool,
    doc_id: Uuid,
    me: Uuid,
    out: &GenesisOutput,
) -> Result<Write> {
    let (mut tx, _, genesis_rev) = match begin_leased(pool, doc_id, me).await? {
        Ok(t) => t,
        Err(w) => return Ok(w),
    };
    if genesis_rev.is_some() {
        return Ok(Write::LeaseLost);
    }
    // What a give-up at genesis listed: genesis lists them again.
    sqlx::query(
        "DELETE FROM doc_history_entries
          WHERE doc_id = $1 AND kind = 'baseline' AND baseline_id IS NOT NULL",
    )
    .bind(doc_id)
    .execute(&mut *tx)
    .await?;
    insert_checkpoint(&mut tx, doc_id, &out.checkpoint, None).await?;
    for e in &out.entries {
        write_entry(&mut tx, doc_id, e).await?;
    }
    let g = out.checkpoint.rev;
    sqlx::query(
        "UPDATE doc_history_state
            SET built_rev = $2, snapshot_checked_rev = $2, genesis_rev = $2, parked = $3,
                skip_revs = skip_revs || $4::bigint[], builder_version = $5, updated_at = now()
          WHERE doc_id = $1",
    )
    .bind(doc_id)
    .bind(g)
    .bind(out.claims.to_json())
    .bind(&out.refused_revs)
    .bind(BUILDER_VERSION)
    .execute(&mut *tx)
    .await?;
    release_in(&mut tx, doc_id, g).await?;
    tx.commit().await?;
    Ok(Write::Done)
}

// ---------------------------------------------------------------------------
// A pass
// ---------------------------------------------------------------------------

/// A snapshot or kept gap's size, before any bytes are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sized {
    pub rev: i64,
    pub size: i64,
    /// The bytes of the rows with `built_rev < rev ≤ this rev`.
    pub rows_to: i64,
}

/// What the walk over snapshots and kept gaps decided (step 6).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Walk {
    /// Indexes into the snapshots taken.
    pub snapshots: Vec<usize>,
    /// Indexes into the snapshots too large to probe even alone.
    pub oversized: Vec<usize>,
    /// Indexes into the kept gaps taken.
    pub kept: Vec<usize>,
    /// The bytes taken.
    pub taken: i64,
    /// The last row to load (`None`: no limit); `Some(built_rev)` loads no
    /// row.
    pub through: Option<i64>,
    /// A snapshot was left out.
    pub left_out: bool,
    /// A kept gap at `built_rev + 1` that cannot fit even alone.
    pub give_up: bool,
}

/// The walk of step 6, as a pure function: snapshots and kept gaps merged
/// in rev order (a kept gap first at a rev), each taken while `base`, the
/// rows it needs and what was taken stay under `cap`. `limit_rev` is the
/// rev of the first snapshot or kept gap the query cut off
/// (`PASS_MAX_SNAPSHOTS`): no row at or past it is loaded.
pub(crate) fn walk(
    snapshots: &[Sized],
    kept: &[Sized],
    base: i64,
    first: i64,
    built_rev: i64,
    cap: i64,
    limit_rev: Option<i64>,
) -> Walk {
    let mut w = Walk::default();
    let mut any_taken = false;
    let (mut s, mut k) = (0, 0);
    loop {
        // A kept gap before a snapshot at the same rev.
        let take_kept = match (snapshots.get(s), kept.get(k)) {
            (None, None) => break,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (Some(sn), Some(kg)) => kg.rev <= sn.rev,
        };
        if take_kept {
            let kg = kept[k];
            if base + kg.rows_to + w.taken + kg.size <= cap {
                w.taken += kg.size;
                w.kept.push(k);
                any_taken = true;
            } else if kg.rev == built_rev + 1 && !any_taken {
                w.give_up = true;
                return w;
            } else {
                w.through = Some(kg.rev - 1);
                break;
            }
            k += 1;
        } else {
            let sn = snapshots[s];
            if sn.rev <= built_rev + 1 && base + sn.rows_to + sn.size > cap {
                w.oversized.push(s);
            } else if base + sn.rows_to + w.taken + sn.size <= cap {
                w.taken += sn.size;
                w.snapshots.push(s);
                any_taken = true;
            } else {
                w.through = Some(sn.rev - 1);
                w.left_out = true;
                break;
            }
            s += 1;
        }
    }
    if let Some(limit) = limit_rev {
        w.through = Some(w.through.map_or(limit - 1, |t| t.min(limit - 1)));
    }
    if cap - base - w.taken < first {
        w.through = Some(built_rev);
    }
    if let Some(t) = w.through {
        w.through = Some(t.max(built_rev));
    }
    w
}

/// What a pass loaded.
pub(crate) struct PassLoad {
    pub built_rev: i64,
    pub checkpoint_rev: i64,
    pub checkpoint_kind: CheckpointKind,
    pub checkpoint_state: Vec<u8>,
    pub reapply: Vec<DocUpdate>,
    pub replay: Vec<DocUpdate>,
    pub genesis_rev: i64,
    pub rows: Vec<DocUpdate>,
    pub rows_exhausted: bool,
    pub open: Vec<EntryDraft>,
    pub claims: Claims,
    pub kept_gaps: Vec<KeptGap>,
    pub snapshots: Vec<SnapshotProbe>,
    pub snapshot_checked_rev: i64,
    pub skip_revs: BTreeSet<i64>,
    pub now: DateTime<Utc>,
    /// The bytes loaded.
    pub bytes: i64,
}

pub(crate) enum PassLoaded {
    /// No checkpoint at or below `built_rev`: the page must be rebuilt.
    NoCheckpoint,
    /// The smallest input is over the cap.
    TooLarge(String),
    Load(Box<PassLoad>),
}

/// The limits `load_pass_inputs` applies.
pub(crate) struct PassLimits {
    pub cap: i64,
    pub max_rows: i64,
    pub max_row_bytes: i64,
    pub max_snapshots: i64,
}

type SnapshotSize = (i64, i64, DateTime<Utc>, i64, i64);
type KeptSize = (i64, DateTime<Utc>, i64, i64);

/// A pass's inputs (step 6), in one read transaction, sizes first.
pub(crate) async fn load_pass_inputs(
    pool: &PgPool,
    doc_id: Uuid,
    limits: &PassLimits,
) -> Result<PassLoaded> {
    let mut tx = begin_read(pool).await?;
    let (built_rev, genesis_rev, snapshot_checked_rev, parked, skip_revs): (
        i64,
        Option<i64>,
        i64,
        Value,
        Vec<i64>,
    ) = sqlx::query_as(
        "SELECT built_rev, genesis_rev, snapshot_checked_rev, parked, skip_revs
           FROM doc_history_state WHERE doc_id = $1",
    )
    .bind(doc_id)
    .fetch_one(&mut *tx)
    .await?;
    let genesis_rev =
        genesis_rev.ok_or_else(|| StoreError::Invalid("a pass before genesis".into()))?;
    let cp: Option<(i64, String, i64, Vec<i64>)> = sqlx::query_as(
        "SELECT rev, kind, octet_length(state_bytes)::bigint, pending_revs
           FROM doc_history_checkpoints WHERE doc_id = $1 AND rev <= $2
          ORDER BY rev DESC LIMIT 1",
    )
    .bind(doc_id)
    .bind(built_rev)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((cp_rev, cp_kind, cp_size, pending_revs)) = cp else {
        return Ok(PassLoaded::NoCheckpoint);
    };
    let checkpoint_kind: CheckpointKind = cp_kind
        .parse()
        .map_err(|e: crate::pass::UnknownCheckpointKind| StoreError::Invalid(e.to_string()))?;
    let (reapply_size, prefix_size, first, head): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT coalesce((SELECT sum(octet_length(update_bytes)) FROM doc_updates
                           WHERE doc_id = $1 AND rev = ANY($3)), 0)::bigint,
                coalesce((SELECT sum(octet_length(update_bytes)) FROM doc_updates
                           WHERE doc_id = $1 AND rev > $2 AND rev <= $4), 0)::bigint,
                coalesce((SELECT octet_length(update_bytes) FROM doc_updates
                           WHERE doc_id = $1 AND rev = $4 + 1), 0)::bigint,
                coalesce((SELECT max(rev) FROM doc_updates WHERE doc_id = $1), 0)",
    )
    .bind(doc_id)
    .bind(cp_rev)
    .bind(&pending_revs)
    .bind(built_rev)
    .fetch_one(&mut *tx)
    .await?;
    let base = cp_size + reapply_size + prefix_size;
    if base + first > limits.cap {
        return Ok(PassLoaded::TooLarge(format!(
            "too large: the replay input is {} bytes, over the cap of {}",
            base + first,
            limits.cap
        )));
    }

    // One more than a pass takes, to know where the cut is.
    let fetch = limits.max_snapshots + 1;
    let snapshot_sizes: Vec<SnapshotSize> = sqlx::query_as(
        "SELECT s.snapshot_rev, s.snapshot_seq, s.created_at, s.size,
                coalesce((SELECT sum(octet_length(u.update_bytes)) FROM doc_updates u
                           WHERE u.doc_id = $1 AND u.rev > $3 AND u.rev <= s.snapshot_rev),
                         0)::bigint
           FROM (SELECT DISTINCT ON (snapshot_rev)
                        snapshot_rev, snapshot_seq, created_at,
                        octet_length(state_bytes)::bigint AS size
                   FROM doc_snapshots
                  WHERE doc_id = $1 AND snapshot_rev > $2
                    AND snapshot_rev <> ALL (
                        SELECT unnest(skip_snapshots) FROM doc_history_state WHERE doc_id = $1)
                  ORDER BY snapshot_rev, snapshot_seq DESC) s
          ORDER BY s.snapshot_rev LIMIT $4",
    )
    .bind(doc_id)
    .bind(snapshot_checked_rev)
    .bind(built_rev)
    .bind(fetch)
    .fetch_all(&mut *tx)
    .await?;
    let kept_sizes: Vec<KeptSize> = sqlx::query_as(
        "SELECT c.rev, c.created_at, octet_length(c.state_bytes)::bigint,
                coalesce((SELECT sum(octet_length(u.update_bytes)) FROM doc_updates u
                           WHERE u.doc_id = $1 AND u.rev > $2 AND u.rev <= c.rev), 0)::bigint
           FROM doc_history_checkpoints c
          WHERE c.doc_id = $1 AND c.rev > $2 AND c.kind = 'gap'
          ORDER BY c.rev LIMIT $3",
    )
    .bind(doc_id)
    .bind(built_rev)
    .bind(fetch)
    .fetch_all(&mut *tx)
    .await?;
    let max = limits.max_snapshots as usize;
    let limit_rev = snapshot_sizes
        .get(max)
        .map(|s| s.0)
        .into_iter()
        .chain(kept_sizes.get(max).map(|k| k.0))
        .min();
    let snaps: Vec<Sized> = snapshot_sizes
        .iter()
        .take(max)
        .filter(|s| limit_rev.is_none_or(|l| s.0 < l))
        .map(|s| Sized {
            rev: s.0,
            size: s.3,
            rows_to: if s.0 <= built_rev { 0 } else { s.4 },
        })
        .collect();
    let kept: Vec<Sized> = kept_sizes
        .iter()
        .take(max)
        .filter(|k| limit_rev.is_none_or(|l| k.0 < l))
        .map(|k| Sized {
            rev: k.0,
            size: k.2,
            rows_to: k.3,
        })
        .collect();
    let w = walk(&snaps, &kept, base, first, built_rev, limits.cap, limit_rev);
    if w.give_up {
        return Ok(PassLoaded::TooLarge(format!(
            "too large: the kept gap at rev {} does not fit the replay cap of {}",
            built_rev + 1,
            limits.cap
        )));
    }

    // The bytes.
    let rows = if w.through == Some(built_rev) {
        Vec::new()
    } else {
        let room = (limits.cap - base - w.taken)
            .min(limits.max_row_bytes)
            .max(1);
        rows_in_range(&mut tx, doc_id, built_rev, w.through, limits.max_rows, room)
            .await
            .map_err(updates_error)?
    };
    let last = rows.last().and_then(|r| r.rev).unwrap_or(built_rev);
    let checkpoint_state: Vec<u8> = sqlx::query_scalar(
        "SELECT state_bytes FROM doc_history_checkpoints WHERE doc_id = $1 AND rev = $2",
    )
    .bind(doc_id)
    .bind(cp_rev)
    .fetch_one(&mut *tx)
    .await?;
    let reapply = rows_at(&mut tx, doc_id, &pending_revs).await?;
    let replay = rows_in_range(&mut tx, doc_id, cp_rev, Some(built_rev), i64::MAX, i64::MAX)
        .await
        .map_err(updates_error)?;
    let mut snapshots = Vec::new();
    let mut taken_snapshots = w.snapshots.clone();
    taken_snapshots.extend(w.oversized.iter().copied());
    taken_snapshots.sort_unstable();
    for i in taken_snapshots {
        let (rev, seq, created_at, ..) = snapshot_sizes[i];
        if rev > last {
            continue;
        }
        let state_bytes = if w.oversized.contains(&i) {
            None
        } else {
            Some(
                sqlx::query_scalar(
                    "SELECT state_bytes FROM doc_snapshots WHERE doc_id = $1 AND snapshot_seq = $2",
                )
                .bind(doc_id)
                .bind(seq)
                .fetch_one(&mut *tx)
                .await?,
            )
        };
        snapshots.push(SnapshotProbe {
            rev,
            created_at,
            state_bytes,
        });
    }
    let mut kept_gaps = Vec::new();
    for &i in &w.kept {
        let (rev, created_at, ..) = kept_sizes[i];
        if rev > last {
            continue;
        }
        let state_bytes: Vec<u8> = sqlx::query_scalar(
            "SELECT state_bytes FROM doc_history_checkpoints WHERE doc_id = $1 AND rev = $2",
        )
        .bind(doc_id)
        .bind(rev)
        .fetch_one(&mut *tx)
        .await?;
        kept_gaps.push(KeptGap {
            rev,
            created_at,
            state_bytes,
        });
    }
    let open = open_sessions(&mut tx, doc_id).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;

    let bytes = checkpoint_state.len() as i64
        + rows_len(&reapply)
        + rows_len(&replay)
        + rows_len(&rows)
        + snapshots
            .iter()
            .map(|s| s.state_bytes.as_ref().map_or(0, |b| b.len() as i64))
            .sum::<i64>()
        + kept_gaps
            .iter()
            .map(|k| k.state_bytes.len() as i64)
            .sum::<i64>();
    Ok(PassLoaded::Load(Box::new(PassLoad {
        built_rev,
        checkpoint_rev: cp_rev,
        checkpoint_kind,
        checkpoint_state,
        reapply,
        replay,
        genesis_rev,
        rows_exhausted: last == head && !w.left_out,
        rows,
        open,
        claims: Claims::from_json(&parked)?,
        kept_gaps,
        snapshots,
        snapshot_checked_rev,
        skip_revs: skip_revs.into_iter().collect(),
        now,
        bytes,
    })))
}

fn rows_len(rows: &[DocUpdate]) -> i64 {
    rows.iter().map(|r| r.update_bytes.len() as i64).sum()
}

fn updates_error(e: knot_storage::UpdatesStoreError) -> StoreError {
    match e {
        knot_storage::UpdatesStoreError::Sqlx(e) => StoreError::Sqlx(e),
        other => StoreError::Invalid(other.to_string()),
    }
}

type RowCols = (
    i64,
    Uuid,
    Vec<u8>,
    Option<Uuid>,
    DateTime<Utc>,
    Option<i64>,
    Option<String>,
    Option<Value>,
    Option<i32>,
);

/// The rows with these revs, in rev order.
async fn rows_at(conn: &mut PgConnection, doc_id: Uuid, revs: &[i64]) -> Result<Vec<DocUpdate>> {
    if revs.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<RowCols> = sqlx::query_as(
        "SELECT seq, doc_id, update_bytes, by_user_id, created_at, rev, source, meta, epoch
           FROM doc_updates WHERE doc_id = $1 AND rev = ANY($2) ORDER BY rev",
    )
    .bind(doc_id)
    .bind(revs)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            let source =
                r.6.map(|s| s.parse::<UpdateSource>())
                    .transpose()
                    .map_err(|e| StoreError::Invalid(e.to_string()))?;
            Ok(DocUpdate {
                seq: r.0,
                doc_id: r.1,
                update_bytes: r.2,
                by_user_id: r.3,
                created_at: r.4,
                rev: r.5,
                source,
                meta: r.7,
                epoch: r.8,
            })
        })
        .collect()
}

type EntryCols = (
    i64,
    String,
    Option<Uuid>,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<i64>,
    Option<i64>,
    bool,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Value,
    Option<i64>,
);

fn id_set(bytes: Option<&[u8]>) -> Result<IdSet> {
    match bytes {
        None => Ok(IdSet::new()),
        Some(b) => IdSet::decode_v1(b).map_err(|e| StoreError::Invalid(format!("an id set: {e}"))),
    }
}

/// The page's open sessions.
async fn open_sessions(conn: &mut PgConnection, doc_id: Uuid) -> Result<Vec<EntryDraft>> {
    let rows: Vec<EntryCols> = sqlx::query_as(
        "SELECT id, kind, author_id, started_at, ended_at, start_rev, end_rev, sealed,
                ins_ids, del_ids, meta, baseline_id
           FROM doc_history_entries WHERE doc_id = $1 AND NOT sealed ORDER BY id",
    )
    .bind(doc_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(EntryDraft {
                id: Some(r.0),
                kind: r
                    .1
                    .parse::<EntryKind>()
                    .map_err(|e| StoreError::Invalid(e.to_string()))?,
                author: r.2,
                started_at: r.3,
                ended_at: r.4,
                start_rev: r.5,
                end_rev: r.6,
                sealed: r.7,
                inserted: id_set(r.8.as_deref())?,
                deleted: id_set(r.9.as_deref())?,
                meta: r.10,
                baseline_id: r.11,
            })
        })
        .collect()
}

async fn insert_checkpoint(
    conn: &mut PgConnection,
    doc_id: Uuid,
    cp: &CheckpointWrite,
    created_at: Option<DateTime<Utc>>,
) -> Result<()> {
    // A gap checkpoint replaces whatever is at its rev, except its
    // `created_at`: a kept gap re-emitted by a rebuild keeps the time it
    // was first found. Any other kind leaves an existing row alone.
    sqlx::query(
        "INSERT INTO doc_history_checkpoints
             (doc_id, rev, kind, state_bytes, state_vector, pending_revs, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, coalesce($7, now()))
         ON CONFLICT (doc_id, rev) DO UPDATE
            SET kind = EXCLUDED.kind, state_bytes = EXCLUDED.state_bytes,
                state_vector = EXCLUDED.state_vector, pending_revs = EXCLUDED.pending_revs
          WHERE EXCLUDED.kind = 'gap'",
    )
    .bind(doc_id)
    .bind(cp.rev)
    .bind(cp.kind.as_str())
    .bind(&cp.state_bytes)
    .bind(&cp.state_vector)
    .bind(&cp.pending_revs)
    .bind(created_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn id_bytes(set: &IdSet) -> Option<Vec<u8>> {
    (!set.is_empty()).then(|| set.encode_v1())
}

/// Updates an entry by id, or inserts it. A session sealed here gets
/// `words_pending`; every update moves `updated_at`.
async fn write_entry(conn: &mut PgConnection, doc_id: Uuid, e: &EntryDraft) -> Result<()> {
    let words_pending = e.kind == EntryKind::Session && e.sealed;
    match e.id {
        Some(id) => {
            sqlx::query(
                "UPDATE doc_history_entries
                    SET kind = $3, author_id = $4, started_at = $5, ended_at = $6,
                        start_rev = $7, end_rev = $8, sealed = $9, ins_ids = $10, del_ids = $11,
                        meta = $12, words_pending = words_pending OR $13, updated_at = now()
                  WHERE id = $1 AND doc_id = $2",
            )
            .bind(id)
            .bind(doc_id)
            .bind(e.kind.as_str())
            .bind(e.author)
            .bind(e.started_at)
            .bind(e.ended_at)
            .bind(e.start_rev)
            .bind(e.end_rev)
            .bind(e.sealed)
            .bind(id_bytes(&e.inserted))
            .bind(id_bytes(&e.deleted))
            .bind(&e.meta)
            .bind(words_pending)
            .execute(&mut *conn)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO doc_history_entries
                     (doc_id, kind, author_id, started_at, ended_at, start_rev, end_rev, sealed,
                      baseline_id, ins_ids, del_ids, meta, words_pending)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            )
            .bind(doc_id)
            .bind(e.kind.as_str())
            .bind(e.author)
            .bind(e.started_at)
            .bind(e.ended_at)
            .bind(e.start_rev)
            .bind(e.end_rev)
            .bind(e.sealed)
            .bind(e.baseline_id)
            .bind(id_bytes(&e.inserted))
            .bind(id_bytes(&e.deleted))
            .bind(&e.meta)
            .bind(words_pending)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// How a pass's write ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassWrite {
    /// Written and released in the same transaction.
    Released,
    /// Written; sealed sessions wait for their word counts.
    WordsPending,
    /// Written up to a rev gap; not released.
    RevGap,
    LeaseLost,
    Gone,
}

/// A pass's write transaction (step 8). `gap_times` gives a new gap
/// checkpoint's `created_at`: when the snapshot it came from was written.
pub(crate) async fn write_pass(
    pool: &PgPool,
    doc_id: Uuid,
    me: Uuid,
    loaded_built_rev: i64,
    out: &PassOutput,
    gap_times: &[(i64, DateTime<Utc>)],
) -> Result<PassWrite> {
    let (mut tx, built_rev, _) = match begin_leased(pool, doc_id, me).await? {
        Ok(t) => t,
        Err(Write::Gone) => return Ok(PassWrite::Gone),
        Err(_) => return Ok(PassWrite::LeaseLost),
    };
    if built_rev != loaded_built_rev {
        return Ok(PassWrite::LeaseLost);
    }
    for cp in &out.checkpoints {
        let created_at = (cp.kind == CheckpointKind::Gap)
            .then(|| {
                gap_times
                    .iter()
                    .find(|(r, _)| *r == cp.rev)
                    .map(|(_, t)| *t)
            })
            .flatten();
        insert_checkpoint(&mut tx, doc_id, cp, created_at).await?;
    }
    for e in &out.entries {
        write_entry(&mut tx, doc_id, e).await?;
    }
    sqlx::query(
        "UPDATE doc_history_state
            SET built_rev = $2, snapshot_checked_rev = $3, parked = $4,
                skip_revs = skip_revs || $5::bigint[], updated_at = now()
          WHERE doc_id = $1",
    )
    .bind(doc_id)
    .bind(out.built_rev)
    .bind(out.snapshot_checked_rev)
    .bind(out.claims.to_json())
    .bind(&out.refused_revs)
    .execute(&mut *tx)
    .await?;
    if out.stats.rev_gap {
        tx.commit().await?;
        return Ok(PassWrite::RevGap);
    }
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM doc_history_entries WHERE doc_id = $1 AND words_pending)",
    )
    .bind(doc_id)
    .fetch_one(&mut *tx)
    .await?;
    if pending {
        tx.commit().await?;
        return Ok(PassWrite::WordsPending);
    }
    release_in(&mut tx, doc_id, out.built_rev).await?;
    tx.commit().await?;
    Ok(PassWrite::Released)
}

// ---------------------------------------------------------------------------
// Word counts
// ---------------------------------------------------------------------------

/// A sealed session waiting for its word counts.
pub(crate) struct WordsEntry {
    pub id: i64,
    pub start_rev: i64,
    pub end_rev: i64,
    pub inserted: IdSet,
    pub deleted: IdSet,
    /// `meta.gap_credit_rev`.
    pub gap_credit_rev: Option<i64>,
}

/// The page's oldest session (by `end_rev`) waiting for its word counts.
pub(crate) async fn next_words_entry(pool: &PgPool, doc_id: Uuid) -> Result<Option<WordsEntry>> {
    type Row = (
        i64,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Value,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, start_rev, end_rev, ins_ids, del_ids, meta FROM doc_history_entries
          WHERE doc_id = $1 AND words_pending ORDER BY end_rev, id LIMIT 1",
    )
    .bind(doc_id)
    .fetch_optional(pool)
    .await?;
    let Some((id, start_rev, end_rev, ins, del, meta)) = row else {
        return Ok(None);
    };
    let (Some(start_rev), Some(end_rev)) = (start_rev, end_rev) else {
        return Err(StoreError::Invalid(format!(
            "entry {id} waits for word counts but has no revs"
        )));
    };
    Ok(Some(WordsEntry {
        id,
        start_rev,
        end_rev,
        inserted: id_set(ins.as_deref())?,
        deleted: id_set(del.as_deref())?,
        gap_credit_rev: meta.get("gap_credit_rev").and_then(Value::as_i64),
    }))
}

/// Marks the entry's count attempted (Rulings: each count is attempted
/// once), under the lease.
pub(crate) async fn take_words_entry(
    pool: &PgPool,
    doc_id: Uuid,
    me: Uuid,
    id: i64,
) -> Result<Write> {
    let (mut tx, _, _) = match begin_leased(pool, doc_id, me).await? {
        Ok(t) => t,
        Err(w) => return Ok(w),
    };
    sqlx::query(
        "UPDATE doc_history_entries SET words_pending = false, updated_at = now()
          WHERE id = $1 AND doc_id = $2 AND words_pending",
    )
    .bind(id)
    .bind(doc_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Write::Done)
}

/// A word count's replay input.
pub(crate) struct WordsLoad {
    pub checkpoint_state: Vec<u8>,
    pub reapply: Vec<DocUpdate>,
    pub rows: Vec<DocUpdate>,
    pub gap_states: Vec<(i64, Vec<u8>)>,
    pub skip_revs: BTreeSet<i64>,
    pub bytes: i64,
}

pub(crate) enum WordsLoaded {
    TooLarge,
    Load(WordsLoad),
}

/// The replay input of the session `start_rev..=end_rev`'s word count: the
/// latest checkpoint at or before `start_rev − 1`, the gap checkpoints
/// after it through `end_rev`, its reapply rows and the rows through
/// `end_rev`; sizes checked in SQL first.
pub(crate) async fn load_words_inputs(
    pool: &PgPool,
    doc_id: Uuid,
    start_rev: i64,
    end_rev: i64,
    cap: i64,
) -> Result<WordsLoaded> {
    let mut tx = begin_read(pool).await?;
    let skip_revs: Vec<i64> =
        sqlx::query_scalar("SELECT skip_revs FROM doc_history_state WHERE doc_id = $1")
            .bind(doc_id)
            .fetch_one(&mut *tx)
            .await?;
    let cp: Option<(i64, i64, Vec<i64>)> = sqlx::query_as(
        "SELECT rev, octet_length(state_bytes)::bigint, pending_revs
           FROM doc_history_checkpoints WHERE doc_id = $1 AND rev <= $2
          ORDER BY rev DESC LIMIT 1",
    )
    .bind(doc_id)
    .bind(start_rev - 1)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((cp_rev, cp_size, pending_revs)) = cp else {
        return Err(StoreError::Invalid(format!(
            "no checkpoint at or before rev {}",
            start_rev - 1
        )));
    };
    let (gaps_size, rows_size): (i64, i64) = sqlx::query_as(
        "SELECT coalesce((SELECT sum(octet_length(state_bytes)) FROM doc_history_checkpoints
                           WHERE doc_id = $1 AND kind = 'gap' AND rev > $2 AND rev <= $3), 0)::bigint,
                coalesce((SELECT sum(octet_length(update_bytes)) FROM doc_updates
                           WHERE doc_id = $1 AND ((rev > $2 AND rev <= $3) OR rev = ANY($4))),
                         0)::bigint",
    )
    .bind(doc_id)
    .bind(cp_rev)
    .bind(end_rev)
    .bind(&pending_revs)
    .fetch_one(&mut *tx)
    .await?;
    let bytes = cp_size + gaps_size + rows_size;
    if bytes > cap {
        return Ok(WordsLoaded::TooLarge);
    }
    let checkpoint_state: Vec<u8> = sqlx::query_scalar(
        "SELECT state_bytes FROM doc_history_checkpoints WHERE doc_id = $1 AND rev = $2",
    )
    .bind(doc_id)
    .bind(cp_rev)
    .fetch_one(&mut *tx)
    .await?;
    let gap_states: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT rev, state_bytes FROM doc_history_checkpoints
          WHERE doc_id = $1 AND kind = 'gap' AND rev > $2 AND rev <= $3 ORDER BY rev",
    )
    .bind(doc_id)
    .bind(cp_rev)
    .bind(end_rev)
    .fetch_all(&mut *tx)
    .await?;
    let reapply = rows_at(&mut tx, doc_id, &pending_revs).await?;
    let rows = rows_in_range(&mut tx, doc_id, cp_rev, Some(end_rev), i64::MAX, i64::MAX)
        .await
        .map_err(updates_error)?;
    tx.commit().await?;
    Ok(WordsLoaded::Load(WordsLoad {
        checkpoint_state,
        reapply,
        rows,
        gap_states,
        skip_revs: skip_revs.into_iter().collect(),
        bytes,
    }))
}

/// Stores an entry's word counts, under the lease.
pub(crate) async fn write_words(
    pool: &PgPool,
    doc_id: Uuid,
    me: Uuid,
    id: i64,
    counts: WordCounts,
) -> Result<Write> {
    let (mut tx, _, _) = match begin_leased(pool, doc_id, me).await? {
        Ok(t) => t,
        Err(w) => return Ok(w),
    };
    sqlx::query(
        "UPDATE doc_history_entries
            SET words_added = $3, words_removed = $4, updated_at = now()
          WHERE id = $1 AND doc_id = $2",
    )
    .bind(id)
    .bind(doc_id)
    .bind(counts.added)
    .bind(counts.removed)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Write::Done)
}

// ---------------------------------------------------------------------------
// Candidates and gauges
// ---------------------------------------------------------------------------

/// Dirty pages to build, oldest write first, debounced (Rulings).
pub(crate) async fn dirty_candidates(
    pool: &PgPool,
    exclude: &[Uuid],
    limit: i64,
    debounce: Duration,
    max_lag_revs: i64,
) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "SELECT h.doc_id FROM doc_history_heads h
           LEFT JOIN doc_history_state s USING (doc_id)
          WHERE h.dirty AND NOT h.doc_id = ANY($2)
            AND (s.doc_id IS NULL
                 OR ((s.retry_after IS NULL OR s.retry_after <= now())
                     AND (s.claimed_until IS NULL OR s.claimed_until < now())
                     AND s.builder_version <= $1))
            AND (h.last_write_at < now() - make_interval(secs => $4)
                 OR h.head_rev - coalesce(s.built_rev, 0) >= $5)
          ORDER BY h.last_write_at
          LIMIT $3",
    )
    .bind(BUILDER_VERSION)
    .bind(exclude)
    .bind(limit)
    .bind(secs(debounce))
    .bind(max_lag_revs)
    .fetch_all(pool)
    .await?)
}

/// Pages the sweep visits (Rulings): idle open sessions, older builds
/// (given-up ones included) and late snapshots, at most `limit` of each.
pub(crate) async fn sweep_candidates(
    pool: &PgPool,
    exclude: &[Uuid],
    limit: i64,
) -> Result<Vec<Uuid>> {
    const FILTER: &str = "(s.retry_after IS NULL OR s.retry_after <= now())
                          AND (s.claimed_until IS NULL OR s.claimed_until < now())
                          AND s.builder_version <= $1";
    let idle: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT e.doc_id FROM doc_history_entries e
           JOIN doc_history_state s USING (doc_id)
          WHERE NOT e.sealed AND e.ended_at < now() - make_interval(secs => $4)
            AND NOT e.doc_id = ANY($2) AND {FILTER}
          GROUP BY e.doc_id ORDER BY min(e.ended_at) LIMIT $3"
    )))
    .bind(BUILDER_VERSION)
    .bind(exclude)
    .bind(limit)
    .bind((SESSION_GAP + SEAL_MARGIN).num_seconds() as f64)
    .fetch_all(pool)
    .await?;
    let older: Vec<Uuid> = sqlx::query_scalar(
        "SELECT s.doc_id FROM doc_history_state s
          WHERE s.builder_version < $1
            AND (s.genesis_rev IS NOT NULL OR s.retry_after = 'infinity')
            AND (s.retry_after IS NULL OR s.retry_after <= now() OR s.retry_after = 'infinity')
            AND (s.claimed_until IS NULL OR s.claimed_until < now())
            AND NOT s.doc_id = ANY($2)
          ORDER BY s.updated_at LIMIT $3",
    )
    .bind(BUILDER_VERSION)
    .bind(exclude)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let late: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT s.doc_id FROM doc_history_state s
          WHERE s.genesis_rev IS NOT NULL AND {FILTER} AND NOT s.doc_id = ANY($2)
            AND EXISTS (SELECT 1 FROM doc_snapshots n
                         WHERE n.doc_id = s.doc_id AND n.snapshot_rev > s.snapshot_checked_rev
                           AND NOT n.snapshot_rev = ANY(s.skip_snapshots))
          ORDER BY s.updated_at LIMIT $3"
    )))
    .bind(BUILDER_VERSION)
    .bind(exclude)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut ids = Vec::new();
    for id in idle.into_iter().chain(older).chain(late) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The largest `head_rev − built_rev` over dirty pages not given up.
pub(crate) async fn max_lag(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(max(h.head_rev - coalesce(s.built_rev, 0)), 0)::bigint
           FROM doc_history_heads h LEFT JOIN doc_history_state s USING (doc_id)
          WHERE h.dirty AND (s.retry_after IS NULL OR s.retry_after <> 'infinity')",
    )
    .fetch_one(pool)
    .await?)
}

/// The number of given-up pages.
pub(crate) async fn given_up_count(pool: &PgPool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM doc_history_state WHERE retry_after = 'infinity'")
            .fetch_one(pool)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sized(rev: i64, size: i64, rows_to: i64) -> Sized {
        Sized { rev, size, rows_to }
    }

    #[test]
    fn the_walk_takes_what_fits_and_stops_at_the_first_left_out() {
        // built_rev 10, base 50, cap 100, first row 5.
        let snaps = [sized(9, 20, 0), sized(12, 20, 10), sized(15, 30, 20)];
        let w = walk(&snaps, &[], 50, 5, 10, 100, None);
        // 9: 50 + 20 ≤ 100; 12: 50 + 10 + 20 + 20 ≤ 100; 15: 50 + 20 + 40
        // + 30 > 100, left out.
        assert_eq!(w.snapshots, vec![0, 1]);
        assert_eq!(w.through, Some(14), "left out at 15: rows through 14");
        assert!(w.left_out);
    }

    #[test]
    fn a_snapshot_that_cannot_fit_alone_is_oversized_when_next() {
        let w = walk(&[sized(11, 60, 5)], &[], 50, 5, 10, 100, None);
        assert_eq!(w.oversized, vec![0]);
        assert_eq!(w.through, None);
        // Far ahead it is left out instead, so the rows before it load.
        let w = walk(&[sized(20, 60, 30)], &[], 50, 5, 10, 100, None);
        assert_eq!(w.through, Some(19));
    }

    #[test]
    fn a_kept_gap_next_that_cannot_fit_gives_up() {
        let w = walk(&[], &[sized(11, 60, 5)], 50, 5, 10, 100, None);
        assert!(w.give_up);
        let w = walk(&[], &[sized(14, 60, 5)], 50, 5, 10, 100, None);
        assert_eq!(w.through, Some(13));
    }

    #[test]
    fn no_rows_when_taken_snapshots_leave_less_than_the_first_row() {
        let w = walk(&[sized(9, 45, 0)], &[], 50, 10, 10, 100, None);
        assert_eq!(w.snapshots, vec![0]);
        assert_eq!(w.through, Some(10));
    }

    #[test]
    fn the_cut_off_query_limits_the_rows() {
        let w = walk(&[sized(12, 1, 1)], &[], 10, 1, 10, 100, Some(15));
        assert_eq!(w.through, Some(14));
        let w = walk(&[], &[], 10, 1, 10, 100, Some(8));
        assert_eq!(w.through, Some(10), "never below built_rev");
    }
}
