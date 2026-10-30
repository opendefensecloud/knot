//! moka-backed caches around the access rule.
//!
//! - Per doc: `acl::resolve`, keyed `(workspace_id, doc_id, user_id)`.
//!   TTL: 60 s (spec §7.5). Capacity: 100k entries.
//! - Per workspace tree: [`Visibility`], keyed `(workspace_id, user_id)`.
//!   TTL: 60 s. Capacity: 10k entries.
//!
//! Invalidations: `evict_doc(doc_id)` is called by the listener task
//! (Task 9) when an `acl_invalidate` NOTIFY arrives. For broader sweeps,
//! `evict_all` exists as a defensive fallback. Both clear the visibility
//! cache completely, since ACL changes are rare.
//!
//! Generation guard: every eviction bumps a generation counter. A load
//! snapshots it before reading the DB and keeps its result cached only if
//! no eviction happened meanwhile, so a read that started before an access
//! change can't re-insert the old access after the change's eviction.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use knot_storage::{DocStore, GrantStore, WorkspaceRole, WorkspaceStore};
use moka::future::Cache;
use uuid::Uuid;

use crate::acl::{DocAccess, ResolveError, resolve};
use crate::rule::{self, AclNode, PrincipalGrant};

/// How often a load that raced an eviction is repeated before its result
/// is returned uncached.
const MAX_STALE_RETRIES: usize = 3;

/// One user's access to every doc of a workspace, folded in one pass by
/// [`rule::compute_access`]. Listings filter through it instead of
/// resolving each doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visibility {
    access_by_doc: HashMap<Uuid, DocAccess>,
    hidden_ids: HashSet<Uuid>,
}

impl Visibility {
    /// Folds `nodes` (every doc of the workspace) with the user's `grants`
    /// and `workspace_role` (`None` when they are not a member).
    pub fn from_nodes(
        nodes: &[AclNode],
        grants: &[PrincipalGrant],
        workspace_role: Option<WorkspaceRole>,
    ) -> Self {
        let access_by_doc = rule::compute_access(nodes, grants, workspace_role);
        let hidden_ids = nodes
            .iter()
            .map(|node| node.id)
            .filter(|doc_id| {
                !access_by_doc
                    .get(doc_id)
                    .is_some_and(|access| access.can_read())
            })
            .collect();
        Self {
            access_by_doc,
            hidden_ids,
        }
    }

    /// Raw access (archived included), like [`AclCache::access`].
    pub fn access(&self, doc_id: Uuid) -> Option<DocAccess> {
        self.access_by_doc.get(&doc_id).copied()
    }

    /// Readable = access with `can_read()`; the same as
    /// `AclCache::effective_role(..).is_some()`.
    pub fn readable(&self, doc_id: Uuid) -> bool {
        self.effective_role(doc_id).is_some()
    }

    /// The user's role on a readable doc, like [`AclCache::effective_role`].
    pub fn effective_role(&self, doc_id: Uuid) -> Option<WorkspaceRole> {
        self.access(doc_id)
            .filter(|access| access.can_read())
            .map(|access| access.role)
    }

    /// Ids of every doc in the workspace that is NOT readable, for SQL
    /// deny-lists. In no particular order.
    pub fn hidden_ids(&self) -> Vec<Uuid> {
        self.hidden_ids.iter().copied().collect()
    }

    /// Whether the doc existed when this snapshot was folded, readable or
    /// not. A doc created since is unknown to it, so it is neither
    /// readable nor hidden here; see [`AclCache::visibility_covering`].
    pub fn covers(&self, doc_id: Uuid) -> bool {
        self.access_by_doc.contains_key(&doc_id) || self.hidden_ids.contains(&doc_id)
    }
}

#[derive(Clone)]
pub struct AclCache {
    inner: Cache<(Uuid, Uuid, Uuid), Option<DocAccess>>,
    visibility: Cache<(Uuid, Uuid), Arc<Visibility>>,
    /// Bumped by every eviction; see the module docs.
    generation: Arc<AtomicU64>,
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
        let visibility = Cache::builder()
            .max_capacity(10_000)
            .time_to_live(Duration::from_secs(60))
            .build();
        Self {
            inner,
            visibility,
            generation: Arc::new(AtomicU64::new(0)),
            workspaces,
            grants,
            docs,
        }
    }

    /// The caller's role on the doc, or `None` when they may not read it:
    /// no membership or grant, or the doc is archived and they are a viewer.
    /// Routes that also need to know whether the doc is archived (to refuse
    /// writes) use [`AclCache::access`].
    pub async fn effective_role(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<WorkspaceRole>, ResolveError> {
        Ok(self
            .access(workspace_id, doc_id, user_id)
            .await?
            .filter(|access| access.can_read())
            .map(|access| access.role))
    }

    /// The caller's role and the doc's archive state, unfiltered: a viewer
    /// on an archived doc gets `Some` with `can_read() == false`.
    pub async fn access(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<DocAccess>, ResolveError> {
        self.access_after_load(workspace_id, doc_id, user_id, || std::future::ready(()))
            .await
    }

    /// [`AclCache::access`] with `after_load` awaited between each resolve
    /// and the generation check, so a test can land an eviction there.
    #[cfg(test)]
    pub(crate) async fn access_with_hook<Hook, HookFuture>(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        user_id: Uuid,
        after_load: Hook,
    ) -> Result<Option<DocAccess>, ResolveError>
    where
        Hook: FnMut() -> HookFuture,
        HookFuture: Future<Output = ()>,
    {
        self.access_after_load(workspace_id, doc_id, user_id, after_load)
            .await
    }

    async fn access_after_load<Hook, HookFuture>(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        user_id: Uuid,
        after_load: Hook,
    ) -> Result<Option<DocAccess>, ResolveError>
    where
        Hook: FnMut() -> HookFuture,
        HookFuture: Future<Output = ()>,
    {
        let key = (workspace_id, doc_id, user_id);
        if let Some(cached_access) = self.inner.get(&key).await {
            return Ok(cached_access);
        }
        let load = || {
            resolve(
                self.workspaces.as_ref(),
                self.grants.as_ref(),
                self.docs.as_ref(),
                workspace_id,
                doc_id,
                user_id,
            )
        };
        self.load_guarded(&self.inner, key, load, after_load).await
    }

    /// One user's access to every doc of the workspace. Three queries (the
    /// workspace tree, the user's grants, their membership) folded with
    /// [`rule::compute_access`], cached per `(workspace_id, user_id)`.
    pub async fn visibility(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<Arc<Visibility>, ResolveError> {
        let key = (workspace_id, user_id);
        if let Some(cached_visibility) = self.visibility.get(&key).await {
            return Ok(cached_visibility);
        }
        let load = || self.load_visibility(workspace_id, user_id);
        self.load_guarded(&self.visibility, key, load, || std::future::ready(()))
            .await
    }

    /// [`AclCache::visibility`] for a listing of `doc_ids`. Creating a doc
    /// evicts nothing, so a cached snapshot can predate a listed doc and
    /// would hide it for up to the TTL; such a snapshot is replaced by a
    /// fresh load (generation-guarded like `visibility`).
    pub async fn visibility_covering(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        doc_ids: &[Uuid],
    ) -> Result<Arc<Visibility>, ResolveError> {
        let visibility = self.visibility(workspace_id, user_id).await?;
        if doc_ids.iter().all(|doc_id| visibility.covers(*doc_id)) {
            return Ok(visibility);
        }
        // Load directly rather than through the cache, which a concurrent
        // caller may have refilled with an equally old snapshot.
        let load = || self.load_visibility(workspace_id, user_id);
        self.load_guarded(&self.visibility, (workspace_id, user_id), load, || {
            std::future::ready(())
        })
        .await
    }

    async fn load_visibility(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<Arc<Visibility>, ResolveError> {
        let principal = format!("user:{user_id}");
        let nodes: Vec<AclNode> = self
            .docs
            .acl_nodes(workspace_id)
            .await?
            .into_iter()
            .map(|node| AclNode {
                id: node.id,
                parent_id: node.parent_id,
                restricted: node.restricted,
                archived: node.archived,
            })
            .collect();
        let principal_grants: Vec<PrincipalGrant> = self
            .grants
            .list_for_principal(workspace_id, &principal)
            .await?
            .into_iter()
            .map(|grant| PrincipalGrant {
                doc_id: grant.doc_id,
                role: grant.role,
                inherit: grant.inherit,
            })
            .collect();
        let workspace_role = self
            .workspaces
            .get_member_role(workspace_id, user_id)
            .await?;
        Ok(Arc::new(Visibility::from_nodes(
            &nodes,
            &principal_grants,
            workspace_role,
        )))
    }

    /// Runs `load` and caches its result under `key` unless an eviction
    /// happened since the load began; then the load is repeated, at most
    /// [`MAX_STALE_RETRIES`] times, and the last result is returned
    /// uncached.
    ///
    /// The generation is checked again after the insert: an eviction that
    /// slips in between the first check and the insert would not remove
    /// the new entry (moka invalidations only cover entries inserted before
    /// them), so the entry is removed here instead.
    async fn load_guarded<Key, Value, Load, LoadFuture, Hook, HookFuture>(
        &self,
        cache: &Cache<Key, Value>,
        key: Key,
        mut load: Load,
        mut after_load: Hook,
    ) -> Result<Value, ResolveError>
    where
        Key: Hash + Eq + Clone + Send + Sync + 'static,
        Value: Clone + Send + Sync + 'static,
        Load: FnMut() -> LoadFuture,
        LoadFuture: Future<Output = Result<Value, ResolveError>>,
        Hook: FnMut() -> HookFuture,
        HookFuture: Future<Output = ()>,
    {
        let mut retries_left = MAX_STALE_RETRIES;
        loop {
            let generation_before = self.generation();
            let value = load().await?;
            after_load().await;
            if self.generation() == generation_before {
                cache.insert(key.clone(), value.clone()).await;
                if self.generation() == generation_before {
                    return Ok(value);
                }
                cache.invalidate(&key).await;
            }
            if retries_left == 0 {
                return Ok(value);
            }
            retries_left -= 1;
        }
    }

    /// The eviction counter of the generation guard.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn evict_doc(&self, doc_id: Uuid) {
        // Bump before invalidating: a load that inserts before this bump is
        // covered by the invalidation below, and one that inserts after it
        // sees the new generation.
        self.generation.fetch_add(1, Ordering::SeqCst);
        // moka's invalidate_entries_if takes a sync predicate. The actual
        // invalidation runs lazily on the next read of each matching key,
        // OR via the background drain task.
        self.inner
            .invalidate_entries_if(move |k, _| k.1 == doc_id)
            .ok();
        // Any doc's access change can change which docs a listing shows.
        self.visibility.invalidate_all();
    }

    pub fn evict_all(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.inner.invalidate_all();
        self.visibility.invalidate_all();
    }

    /// Force moka to drain pending invalidations and other background
    /// maintenance. The listener calls this after each NOTIFY so evictions
    /// take effect immediately rather than waiting for the next read or the
    /// background drain task.
    pub async fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks().await;
        self.visibility.run_pending_tasks().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::DocAccess;
    use knot_storage::{
        DocStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    };

    /// Viewers lose an archived doc entirely through `effective_role` (the
    /// read gate most routes use); `access` still reports the raw state so
    /// the doc middleware can answer 404 rather than 403.
    #[tokio::test(flavor = "multi_thread")]
    async fn effective_role_hides_archived_doc_from_viewers() {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());
        let ws = ws_s.create("default", "W").await.unwrap();
        let owner = us.create_local("o@x.test", "O", "$h$").await.unwrap();
        let viewer = us.create_local("v@x.test", "V", "$h$").await.unwrap();
        ws_s.add_member(ws.id, owner.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        ws_s.add_member(ws.id, viewer.id, WorkspaceRole::Viewer)
            .await
            .unwrap();
        let doc = ds.create(ws.id, None, "Doc", "m", owner.id).await.unwrap();
        ds.archive(ws.id, doc.id, owner.id).await.unwrap();
        let cache = AclCache::new(Arc::new(ws_s), Arc::new(gs), Arc::new(ds));

        let viewer_role = cache
            .effective_role(ws.id, doc.id, viewer.id)
            .await
            .unwrap();
        let owner_role = cache.effective_role(ws.id, doc.id, owner.id).await.unwrap();
        let viewer_access = cache.access(ws.id, doc.id, viewer.id).await.unwrap();

        assert_eq!(viewer_role, None);
        assert_eq!(owner_role, Some(WorkspaceRole::Owner));
        assert_eq!(
            viewer_access,
            Some(DocAccess {
                role: WorkspaceRole::Viewer,
                archived: true,
                restriction_root: None,
            })
        );
    }

    async fn restrict(pool: &sqlx::PgPool, doc_id: Uuid) {
        sqlx::query("UPDATE documents SET restricted = true WHERE id = $1")
            .bind(doc_id)
            .execute(pool)
            .await
            .unwrap();
    }

    /// The batched evaluator agrees with the per-doc one on every doc of a
    /// tree with a restricted boundary, a grant inside it and an archived
    /// doc, for a member with that grant and an owner without one.
    #[tokio::test(flavor = "multi_thread")]
    async fn visibility_matches_effective_role_for_every_doc() {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());
        let ws = ws_s.create("default", "W").await.unwrap();
        let owner = us.create_local("o@x.test", "O", "$h$").await.unwrap();
        let editor = us.create_local("e@x.test", "E", "$h$").await.unwrap();
        ws_s.add_member(ws.id, owner.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        ws_s.add_member(ws.id, editor.id, WorkspaceRole::Editor)
            .await
            .unwrap();
        // root ─┬─ boundary (restricted) ── granted ── archived_leaf
        //       └─ sibling
        // other_root
        let root = ds.create(ws.id, None, "Root", "m", owner.id).await.unwrap();
        let boundary = ds
            .create(ws.id, Some(root.id), "Boundary", "m", owner.id)
            .await
            .unwrap();
        let granted = ds
            .create(ws.id, Some(boundary.id), "Granted", "m", owner.id)
            .await
            .unwrap();
        let archived_leaf = ds
            .create(ws.id, Some(granted.id), "Archived", "m", owner.id)
            .await
            .unwrap();
        let sibling = ds
            .create(ws.id, Some(root.id), "Sibling", "n", owner.id)
            .await
            .unwrap();
        let other_root = ds
            .create(ws.id, None, "Other", "n", owner.id)
            .await
            .unwrap();
        restrict(&pool, boundary.id).await;
        gs.put(
            ws.id,
            granted.id,
            &format!("user:{}", editor.id),
            WorkspaceRole::Viewer,
            true,
            owner.id,
        )
        .await
        .unwrap();
        // Only the grant gives the editor access below the boundary, and it
        // is Viewer, so the archived leaf is unreadable for them.
        ds.archive(ws.id, archived_leaf.id, owner.id).await.unwrap();
        let all_doc_ids = [
            root.id,
            boundary.id,
            granted.id,
            archived_leaf.id,
            sibling.id,
            other_root.id,
        ];
        let cache = AclCache::new(Arc::new(ws_s), Arc::new(gs), Arc::new(ds));

        for user_id in [owner.id, editor.id] {
            let visibility = cache.visibility(ws.id, user_id).await.unwrap();
            for doc_id in all_doc_ids {
                let per_doc_role = cache.effective_role(ws.id, doc_id, user_id).await.unwrap();
                let per_doc_access = cache.access(ws.id, doc_id, user_id).await.unwrap();
                assert_eq!(visibility.effective_role(doc_id), per_doc_role);
                assert_eq!(visibility.access(doc_id), per_doc_access);
                assert_eq!(visibility.readable(doc_id), per_doc_role.is_some());
                assert_eq!(
                    visibility.hidden_ids().contains(&doc_id),
                    per_doc_role.is_none()
                );
            }
        }
        let editor_visibility = cache.visibility(ws.id, editor.id).await.unwrap();
        assert_eq!(
            editor_visibility.effective_role(granted.id),
            Some(WorkspaceRole::Viewer)
        );
        let mut editor_hidden = editor_visibility.hidden_ids();
        editor_hidden.sort();
        let mut expected_hidden = vec![boundary.id, archived_leaf.id];
        expected_hidden.sort();
        assert_eq!(editor_hidden, expected_hidden);
    }

    /// `evict_doc` clears the whole visibility cache, so a restriction set
    /// in the DB shows up on the next `visibility` call.
    #[tokio::test(flavor = "multi_thread")]
    async fn visibility_is_invalidated_by_evict_doc() {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());
        let ws = ws_s.create("default", "W").await.unwrap();
        let owner = us.create_local("o@x.test", "O", "$h$").await.unwrap();
        ws_s.add_member(ws.id, owner.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let doc = ds.create(ws.id, None, "Doc", "m", owner.id).await.unwrap();
        let cache = AclCache::new(Arc::new(ws_s), Arc::new(gs), Arc::new(ds));

        let before = cache.visibility(ws.id, owner.id).await.unwrap();
        assert!(before.readable(doc.id));

        restrict(&pool, doc.id).await;
        let still_cached = cache.visibility(ws.id, owner.id).await.unwrap();
        assert!(
            still_cached.readable(doc.id),
            "without an eviction the cached visibility is served"
        );

        cache.evict_doc(doc.id);
        cache.run_pending_tasks().await;
        let after = cache.visibility(ws.id, owner.id).await.unwrap();
        assert!(!after.readable(doc.id));
        assert_eq!(after.hidden_ids(), vec![doc.id]);
    }

    /// A cached snapshot that predates a doc (created without any ACL
    /// eviction) doesn't cover it; `visibility_covering` reloads it then,
    /// and serves the cached one otherwise.
    #[tokio::test(flavor = "multi_thread")]
    async fn visibility_covering_reloads_snapshot_missing_a_doc() {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());
        let ws = ws_s.create("default", "W").await.unwrap();
        let owner = us.create_local("o@x.test", "O", "$h$").await.unwrap();
        ws_s.add_member(ws.id, owner.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let old_doc = ds.create(ws.id, None, "Old", "m", owner.id).await.unwrap();
        let ds = Arc::new(ds);
        let cache = AclCache::new(Arc::new(ws_s), Arc::new(gs), ds.clone());

        let before = cache.visibility(ws.id, owner.id).await.unwrap();
        assert!(before.covers(old_doc.id));
        let new_doc = ds.create(ws.id, None, "New", "n", owner.id).await.unwrap();
        assert!(!before.covers(new_doc.id));
        assert!(!before.readable(new_doc.id));

        let unchanged = cache
            .visibility_covering(ws.id, owner.id, &[old_doc.id])
            .await
            .unwrap();
        assert!(
            Arc::ptr_eq(&before, &unchanged),
            "a covering snapshot is reused"
        );

        let reloaded = cache
            .visibility_covering(ws.id, owner.id, &[old_doc.id, new_doc.id])
            .await
            .unwrap();
        assert!(reloaded.covers(new_doc.id));
        assert_eq!(
            reloaded.effective_role(new_doc.id),
            Some(WorkspaceRole::Owner)
        );
        let cached = cache.visibility(ws.id, owner.id).await.unwrap();
        assert!(cached.covers(new_doc.id), "the reload is cached");

        // A hidden doc is covered too: it is known, just not readable.
        restrict(&pool, new_doc.id).await;
        cache.evict_doc(new_doc.id);
        cache.run_pending_tasks().await;
        let restricted = cache.visibility(ws.id, owner.id).await.unwrap();
        assert!(restricted.covers(new_doc.id));
        assert!(!restricted.readable(new_doc.id));
    }

    /// A resolve that read the DB before an access change must not be
    /// cached when the change's eviction lands before its insert.
    #[tokio::test(flavor = "multi_thread")]
    async fn stale_resolve_is_not_cached_after_concurrent_eviction() {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());
        let ws = ws_s.create("default", "W").await.unwrap();
        let owner = us.create_local("o@x.test", "O", "$h$").await.unwrap();
        ws_s.add_member(ws.id, owner.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let doc = ds.create(ws.id, None, "Doc", "m", owner.id).await.unwrap();
        let cache = AclCache::new(Arc::new(ws_s), Arc::new(gs), Arc::new(ds));

        let generation_before = cache.generation();
        let mut hook_calls = 0;
        let first = cache
            .access_with_hook(ws.id, doc.id, owner.id, || {
                hook_calls += 1;
                let first_call = hook_calls == 1;
                let pool = pool.clone();
                let cache = cache.clone();
                async move {
                    if first_call {
                        // The resolve above already read the unrestricted
                        // doc; the restriction and its eviction land now.
                        restrict(&pool, doc.id).await;
                        cache.evict_doc(doc.id);
                    }
                }
            })
            .await
            .unwrap();

        assert!(cache.generation() > generation_before);
        assert_eq!(hook_calls, 2, "the stale resolve is retried once");
        assert_eq!(first, None, "the retry reads the restriction");
        assert_eq!(
            cache.access(ws.id, doc.id, owner.id).await.unwrap(),
            None,
            "the stale Owner access must not have been cached"
        );
    }

    /// When every attempt races an eviction, the last value is returned
    /// but never cached.
    #[tokio::test(flavor = "multi_thread")]
    async fn persistent_eviction_returns_value_uncached() {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());
        let ws = ws_s.create("default", "W").await.unwrap();
        let owner = us.create_local("o@x.test", "O", "$h$").await.unwrap();
        ws_s.add_member(ws.id, owner.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let doc = ds.create(ws.id, None, "Doc", "m", owner.id).await.unwrap();
        let cache = AclCache::new(Arc::new(ws_s), Arc::new(gs), Arc::new(ds));

        let mut hook_calls = 0;
        let access = cache
            .access_with_hook(ws.id, doc.id, owner.id, || {
                hook_calls += 1;
                cache.evict_all();
                std::future::ready(())
            })
            .await
            .unwrap();

        assert_eq!(access.map(|access| access.role), Some(WorkspaceRole::Owner));
        assert_eq!(hook_calls, 1 + MAX_STALE_RETRIES);
        cache.run_pending_tasks().await;
        assert!(!cache.inner.contains_key(&(ws.id, doc.id, owner.id)));
    }

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
        let doc = ds.create(ws_a.id, None, "Doc", "m", user.id).await.unwrap();

        // Workspace B: same user, NOT a member.
        let ws_b = ws_s.create("wsb", "B").await.unwrap();

        let cache = AclCache::new(Arc::new(ws_s), Arc::new(gs), Arc::new(ds));

        // Prime the cache for workspace A — resolves to Owner.
        let r_a = cache
            .effective_role(ws_a.id, doc.id, user.id)
            .await
            .unwrap();
        assert_eq!(r_a, Some(WorkspaceRole::Owner));

        // Look up the same (doc_id, user_id) under workspace B.
        // With the old (doc_id, user_id) key this would return the poisoned
        // Owner value; with the correct (workspace_id, doc_id, user_id) key
        // it must resolve independently and return None (user not a member of B).
        let r_b = cache
            .effective_role(ws_b.id, doc.id, user.id)
            .await
            .unwrap();
        assert_eq!(
            r_b, None,
            "cached Owner role from ws_a must not bleed into ws_b lookup"
        );
    }
}
