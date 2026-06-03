//! Storage layer for knot — Postgres pool + storage traits.

pub mod blobs;
pub mod audit;
pub mod doc_store;
pub mod grant_store;
pub mod invalidations;
pub mod lexorank;
pub mod markdown_cache;
pub mod pool;
pub mod session_store;
pub mod snapshot_store;
pub mod updates_store;
pub mod user_store;
pub mod workspace_store;

pub use blobs::{BlobMeta, BlobMetadata, BlobStore, BlobStoreError, PgBytesStore};
#[cfg(feature = "s3")]
pub use blobs::S3Store;
pub use doc_store::{DocStore, DocStoreError, Document, PgDocStore};
pub use grant_store::{Grant, GrantStore, GrantStoreError, PgGrantStore};
pub use lexorank::between as sort_key_between;
pub use markdown_cache::{
    MarkdownCacheEntry, MarkdownCacheError, MarkdownCacheStore, PgMarkdownCache,
};
pub use pool::{Pool, PoolError, connect};
pub use session_store::{PgSessionStore, Session, SessionStore, SessionStoreError};
pub use snapshot_store::{DocSnapshot, PgSnapshotStore, SnapshotStore, SnapshotStoreError};
pub use updates_store::{DocUpdate, PgUpdatesStore, UpdatesStore, UpdatesStoreError};
pub use user_store::{PgUserStore, User, UserStore, UserStoreError};
pub use workspace_store::{
    Member, PgWorkspaceStore, Workspace, WorkspaceRole, WorkspaceStore, WorkspaceStoreError,
};
