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
    inner: Cache<(Uuid, Uuid, Uuid), Option<WorkspaceRole>>,
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
        let key = (workspace_id, doc_id, user_id);
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
            .invalidate_entries_if(move |k, _| k.1 == doc_id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use knot_storage::{DocStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore};

    /// Regression: the cache must be keyed by (workspace_id, doc_id, user_id).
    /// If the key were only (doc_id, user_id), a cached role for workspace A
    /// would be returned when the same (doc_id, user_id) pair is looked up
    /// under workspace B — cross-tenant poisoning.
    ///
    /// This test creates two workspaces with the same user and the same
    /// doc_id (impossible in real data, but we directly insert the cache
    /// entries to isolate the key logic), then verifies that inserting under
    /// ws A does not serve the cached value under ws B.
    #[tokio::test(flavor = "multi_thread")]
    async fn per_workspace_isolation() {
        let pool = knot_test_support::fresh_db().await.pool;

        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());

        // Workspace A: user is an Owner.
        let ws_a = ws_s.create("wsa", "A").await.unwrap();
        let user = us.create_local("u@x.test", "U", "$h$").await.unwrap();
        ws_s.add_member(ws_a.id, user.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let doc = ds
            .create(ws_a.id, None, "Doc", "m", user.id)
            .await
            .unwrap();

        // Workspace B: same user, NOT a member.
        let ws_b = ws_s.create("wsb", "B").await.unwrap();

        let cache = AclCache::new(
            Arc::new(ws_s),
            Arc::new(gs),
            Arc::new(ds),
        );

        // Prime the cache for workspace A — resolves to Owner.
        let r_a = cache.effective_role(ws_a.id, doc.id, user.id).await.unwrap();
        assert_eq!(r_a, Some(WorkspaceRole::Owner));

        // Look up the same (doc_id, user_id) under workspace B.
        // With the old (doc_id, user_id) key this would return the poisoned
        // Owner value; with the correct (workspace_id, doc_id, user_id) key
        // it must resolve independently and return None (user not a member of B).
        let r_b = cache.effective_role(ws_b.id, doc.id, user.id).await.unwrap();
        assert_eq!(
            r_b,
            None,
            "cached Owner role from ws_a must not bleed into ws_b lookup"
        );
    }
}
