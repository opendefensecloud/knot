//! Storage layer for knot — Postgres pool + storage traits.

pub mod audit;
pub mod blobs;
pub mod boards;
pub mod comments;
pub mod contributors;
pub mod doc_store;
pub mod duplicate;
pub mod grant_store;
pub mod invalidations;
pub mod lexorank;
pub mod markdown_cache;
pub mod placement;
pub mod pool;
pub mod search;
pub mod session_store;
pub mod share_tokens;
pub mod snapshot_store;
pub mod tasks;
pub mod updates_store;
pub mod user_store;
pub mod workspace_store;

pub use blobs::{BlobMeta, BlobMetadata, BlobStore, BlobStoreError, PgBytesStore, S3Store};
pub use boards::{Board, BoardStore, BoardStoreError, PgBoardStore};
pub use comments::{Comment, CommentStore, CommentStoreError, PgCommentStore, Reaction};
pub use contributors::{
    Contributor, ContributorStore, ContributorStoreError, DocByline, PersonRef, PgContributorStore,
};
pub use doc_store::{ArchivedRoot, DocStore, DocStoreError, Document, PgDocStore, SubtreeRow};
pub use grant_store::{Grant, GrantStore, GrantStoreError, PgGrantStore};
pub use markdown_cache::{
    MarkdownCacheEntry, MarkdownCacheError, MarkdownCacheStore, PgMarkdownCache,
    STALE_RENDERER_VERSION,
};
pub use placement::Placement;
pub use pool::{Pool, PoolError, begin, connect};
pub use search::{PgSearchStore, SearchHit, SearchStore, SearchStoreError};
pub use session_store::{PgSessionStore, Session, SessionStore, SessionStoreError};
pub use share_tokens::{PgShareTokenStore, ShareStoreError, ShareToken, ShareTokenStore};
pub use snapshot_store::{
    DocSnapshot, PgSnapshotStore, SnapshotMeta, SnapshotStore, SnapshotStoreError,
};
pub use tasks::{DocTask, DocTaskInput, PgTaskStore, TaskStore, TaskStoreError};
pub use updates_store::{DocUpdate, PgUpdatesStore, UpdatesStore, UpdatesStoreError};
pub use user_store::{PgUserStore, User, UserStore, UserStoreError};
pub use workspace_store::{
    Member, PgWorkspaceStore, Workspace, WorkspaceRole, WorkspaceStore, WorkspaceStoreError,
};

/// A sort key between two neighbours (`None`: an open end).
///
/// # Panics
///
/// When a neighbour is not a valid key, or `a >= b`. `lexorank::between`
/// returns those cases as a `KeyError` instead.
pub fn sort_key_between(a: Option<&str>, b: Option<&str>) -> String {
    lexorank::between(a, b).expect("valid sort keys")
}
