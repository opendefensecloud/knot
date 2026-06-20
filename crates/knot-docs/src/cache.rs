//! moka-backed cache around `acl::resolve`.
//!
//! TTL: 60 s (spec §7.5). Capacity: 100k entries.
//!
//! Invalidations: `evict_doc(doc_id)` is called by the listener task
//! (Task 9) when an `acl_invalidate` NOTIFY arrives. For broader sweeps,
//! `evict_all` exists as a defensive fallback.

use std::sync::Arc;
use std::time::Duration;

use knot_storage::{DocStore, GrantStore, WorkspaceRole, WorkspaceStore};
use moka::future::Cache;
use uuid::Uuid;

use crate::acl::{ResolveError, resolve};

#[derive(Clone)]
pub struct AclCache {
    inner: Cache<(Uuid, Uuid), Option<WorkspaceRole>>,
    workspaces: Arc<dyn WorkspaceStore>,
    grants: Arc<dyn GrantStore>,
    docs: Arc<dyn DocStore>,
}

impl AclCache {
    pub fn new(
        workspaces: Arc<dyn WorkspaceStore>,
        grants: Arc<dyn GrantStore>,
        docs: Arc<dyn DocStore>,
    ) -> Self {
        let inner = Cache::builder()
            .max_capacity(100_000)
            .time_to_live(Duration::from_secs(60))
            .support_invalidation_closures()
            .build();
        Self {
            inner,
            workspaces,
            grants,
            docs,
        }
    }

    pub async fn effective_role(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<WorkspaceRole>, ResolveError> {
        let key = (doc_id, user_id);
        if let Some(v) = self.inner.get(&key).await {
            return Ok(v);
        }
        let v = resolve(
            self.workspaces.as_ref(),
            self.grants.as_ref(),
            self.docs.as_ref(),
            workspace_id,
            doc_id,
            user_id,
        )
        .await?;
        self.inner.insert(key, v).await;
        Ok(v)
    }

    pub fn evict_doc(&self, doc_id: Uuid) {
        // moka's invalidate_entries_if takes a sync predicate. The actual
        // invalidation runs lazily on the next read of each matching key,
        // OR via the background drain task.
        self.inner
            .invalidate_entries_if(move |k, _| k.0 == doc_id)
            .ok();
    }

    pub fn evict_all(&self) {
        self.inner.invalidate_all();
    }

    /// Force moka to drain pending invalidations and other background
    /// maintenance. The listener calls this after each NOTIFY so evictions
    /// take effect immediately rather than waiting for the next read or the
    /// background drain task.
    pub async fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks().await;
    }
}
