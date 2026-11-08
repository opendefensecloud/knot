//! Snapshot trigger logic. The actor calls `write_snapshot` after the
//! N-trigger fires (in the writer-event arm), the idle-trigger fires (in a
//! 1s tick) and when it ends, each time only while the room's snapshot rule
//! holds (see `crate::room`): the doc then holds nothing beyond the page's
//! pre-upgrade base and the log up to the stamp's rev.

use crate::engine::{DocHandle, Engine};
use knot_storage::{SnapshotStamp, SnapshotStore, SnapshotStoreError};
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
pub struct SnapshotPolicy {
    pub every_n: u32,
    pub idle: Duration,
}

pub struct SnapshotState {
    pub last_snapshot_seq: i64,
    pub updates_since_snapshot: u32,
    pub last_apply_at: Instant,
}

/// Why a snapshot was not written.
#[derive(Debug, Error)]
pub enum SnapshotWriteError {
    /// The page's content epoch is no longer the stamp's: its history was
    /// rewritten under the room (KH002).
    #[error("page history epoch changed")]
    EpochChanged,
    /// The page does not exist any more (purged).
    #[error("page does not exist")]
    DocGone,
    /// Anything else (encoding, a database error); nothing was written.
    #[error("{0}")]
    Other(String),
}

/// Writes the state of `doc` as a snapshot at `stamp`.
pub async fn write_snapshot(
    doc_id: Uuid,
    stamp: SnapshotStamp,
    engine: &dyn Engine,
    doc: &DocHandle,
    store: &dyn SnapshotStore,
) -> Result<(), SnapshotWriteError> {
    let state_bytes = engine
        .encode_state_as_update(doc, None)
        .map_err(|e| SnapshotWriteError::Other(e.to_string()))?;
    let sv = engine
        .encode_state_vector(doc)
        .map_err(|e| SnapshotWriteError::Other(e.to_string()))?;
    store
        .insert_stamped(doc_id, stamp, &state_bytes, &sv)
        .await
        .map_err(|e| match e {
            SnapshotStoreError::EpochChanged => SnapshotWriteError::EpochChanged,
            SnapshotStoreError::DocGone => SnapshotWriteError::DocGone,
            other => SnapshotWriteError::Other(other.to_string()),
        })
}
