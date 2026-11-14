//! In-memory `UpdatesStore` and `SnapshotStore` for boot tests
//! (`knot_crdt::boot`) that need no database: they serve the rows and
//! snapshots a test pushes, order `latest` like Postgres, and record which
//! rows boot read. Tests include this file by path
//! (`#[path = "common/mem_store.rs"] mod mem_store;`).
#![allow(dead_code)]

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use knot_storage::{
    DocSnapshot, DocUpdate, HistoryHead, NewUpdate, Persisted, RowsAfter, SnapshotMeta,
    SnapshotStamp, SnapshotStore, SnapshotStoreError, UpdateSource, UpdatesStore,
    UpdatesStoreError,
};
use uuid::Uuid;

/// What boot read: `rows_after(from)` or `rows_at(revs)`.
#[derive(Clone, Debug, PartialEq)]
pub enum RowsRead {
    After(RowsAfter),
    At(Vec<i64>),
}

/// One page's log rows and snapshots, in memory.
#[derive(Default)]
pub struct MemStore {
    rows: Mutex<Vec<DocUpdate>>,
    snapshots: Mutex<Vec<DocSnapshot>>,
    reads: Mutex<Vec<RowsRead>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A row at `rev` (seq = rev), an edit at epoch 0 by nobody, created
    /// `rev` minutes after the epoch.
    pub fn push_row(&self, rev: i64, bytes: Vec<u8>) {
        let mut row = DocUpdate {
            seq: rev,
            doc_id: Uuid::nil(),
            update_bytes: bytes,
            by_user_id: None,
            created_at: Default::default(),
            rev: Some(rev),
            source: Some(UpdateSource::Edit),
            meta: None,
            epoch: Some(0),
        };
        row.created_at += Duration::from_secs(60 * rev.max(0) as u64);
        self.rows.lock().unwrap().push(row);
    }

    pub fn push_snapshot(&self, s: DocSnapshot) {
        self.snapshots.lock().unwrap().push(s);
    }

    /// Every `rows_after` and `rows_at` call so far, in order.
    pub fn reads(&self) -> Vec<RowsRead> {
        self.reads.lock().unwrap().clone()
    }

    /// The rows, in rev order.
    pub fn rows(&self) -> Vec<DocUpdate> {
        let mut rows = self.rows.lock().unwrap().clone();
        rows.sort_by_key(|r| r.rev);
        rows
    }

    /// `latest`'s order: `snapshot_rev DESC NULLS LAST, snapshot_seq DESC`.
    fn newest<'a>(snapshots: impl Iterator<Item = &'a DocSnapshot>) -> Option<DocSnapshot> {
        snapshots
            .max_by_key(|s| (s.snapshot_rev.is_some(), s.snapshot_rev, s.snapshot_seq))
            .cloned()
    }
}

/// A snapshot of `state` at `rev` (seq = rev), as `DocSnapshot` reads it.
pub fn snapshot(
    rev: Option<i64>,
    seq: i64,
    state: Vec<u8>,
    pending_revs: Option<Vec<i64>>,
    older_writer: bool,
) -> DocSnapshot {
    DocSnapshot {
        doc_id: Uuid::nil(),
        snapshot_seq: seq,
        state_bytes: state,
        state_vector: Vec::new(),
        created_at: Default::default(),
        snapshot_rev: rev,
        epoch: rev.map(|_| 0),
        pending_revs,
        older_writer,
    }
}

#[async_trait]
impl UpdatesStore for MemStore {
    async fn append(
        &self,
        _doc_id: Uuid,
        _epoch: i32,
        _rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError> {
        unimplemented!("MemStore serves boot reads only")
    }

    async fn rows_after(
        &self,
        _doc_id: Uuid,
        from: RowsAfter,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        self.reads.lock().unwrap().push(RowsRead::After(from));
        Ok(self
            .rows()
            .into_iter()
            .filter(|r| match from {
                RowsAfter::Rev(after) => r.rev.is_some_and(|rev| rev > after),
                RowsAfter::All => true,
            })
            .collect())
    }

    async fn rows_at(
        &self,
        _doc_id: Uuid,
        revs: &[i64],
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        self.reads.lock().unwrap().push(RowsRead::At(revs.to_vec()));
        Ok(self
            .rows()
            .into_iter()
            .filter(|r| r.rev.is_some_and(|rev| revs.contains(&rev)))
            .collect())
    }

    async fn head(&self, _doc_id: Uuid) -> Result<HistoryHead, UpdatesStoreError> {
        Ok(HistoryHead::default())
    }

    async fn ensure_legacy_numbered(&self, _doc_id: Uuid) -> Result<u64, UpdatesStoreError> {
        Ok(0)
    }

    async fn since(
        &self,
        _doc_id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        let mut rows: Vec<DocUpdate> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.seq > after_seq)
            .cloned()
            .collect();
        rows.sort_by_key(|r| r.seq);
        Ok(rows)
    }

    async fn max_seq(&self, _doc_id: Uuid) -> Result<i64, UpdatesStoreError> {
        unimplemented!("MemStore serves boot reads only")
    }
}

#[async_trait]
impl SnapshotStore for MemStore {
    async fn insert_stamped(
        &self,
        _doc_id: Uuid,
        _stamp: SnapshotStamp,
        _state_bytes: &[u8],
        _state_vector: &[u8],
        _pending_revs: Option<&[i64]>,
    ) -> Result<bool, SnapshotStoreError> {
        unimplemented!("MemStore serves boot reads only")
    }

    async fn latest(&self, _doc_id: Uuid) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        Ok(Self::newest(self.snapshots.lock().unwrap().iter()))
    }

    async fn latest_trusted(
        &self,
        _doc_id: Uuid,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        Ok(Self::newest(
            self.snapshots
                .lock()
                .unwrap()
                .iter()
                .filter(|s| !s.older_writer),
        ))
    }

    async fn gc(
        &self,
        _doc_id: Uuid,
        _keep_recent: i64,
        _retain_days: i32,
    ) -> Result<u64, SnapshotStoreError> {
        unimplemented!("MemStore serves boot reads only")
    }

    async fn list(
        &self,
        _doc_id: Uuid,
        _limit: i64,
    ) -> Result<Vec<SnapshotMeta>, SnapshotStoreError> {
        unimplemented!("MemStore serves boot reads only")
    }

    async fn by_seq(
        &self,
        _doc_id: Uuid,
        snapshot_seq: i64,
    ) -> Result<Option<DocSnapshot>, SnapshotStoreError> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.snapshot_seq == snapshot_seq)
            .cloned())
    }
}
