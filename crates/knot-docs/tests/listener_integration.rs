//! Verifies end-to-end: ACL change → NOTIFY → listener → cache evict and
//! socket hooks, and the listener's fallbacks when it may have missed one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use knot_docs::{AclCache, spawn_listener};
use knot_storage::{
    DocStore, GrantStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore, invalidations,
};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn grant_change_evicts_cache_entry() {
    let pool = knot_test_support::fresh_db().await.pool;

    let ws_s = PgWorkspaceStore::new(pool.clone());
    let us = PgUserStore::new(pool.clone());
    let ds = PgDocStore::new(pool.clone());
    let gs = PgGrantStore::new(pool.clone());

    let ws = ws_s.create("default", "W").await.unwrap();
    let u = us.create_local("a@x.test", "A", "$h$").await.unwrap();
    ws_s.add_member(ws.id, u.id, WorkspaceRole::Viewer)
        .await
        .unwrap();
    let d = ds.create(ws.id, None, "X", "m", u.id).await.unwrap();

    let cache = Arc::new(AclCache::new(
        Arc::new(ws_s.clone()),
        Arc::new(gs.clone()),
        Arc::new(ds.clone()),
    ));
    let _handle = spawn_listener(
        pool.clone(),
        cache.clone(),
        Arc::new(ds.clone()),
        Arc::new(|_: uuid::Uuid| {}),
    );
    // Let the listener subscribe before emitting NOTIFYs.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Prime the cache via a read.
    let r1 = cache.effective_role(ws.id, d.id, u.id).await.unwrap();
    assert_eq!(r1, Some(WorkspaceRole::Viewer));

    // Grant upgrade emits NOTIFY (via GrantStore::put → invalidations::record_in_tx).
    gs.put(
        ws.id,
        d.id,
        &format!("user:{}", u.id),
        WorkspaceRole::Owner,
        true,
        u.id,
    )
    .await
    .unwrap();
    // Wait for the listener to receive + process the NOTIFY.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Re-resolve — the cache entry for (doc, user) should have been evicted,
    // so the read goes back to GrantStore and sees the new role.
    let r2 = cache.effective_role(ws.id, d.id, u.id).await.unwrap();
    assert_eq!(r2, Some(WorkspaceRole::Owner));
}

#[tokio::test(flavor = "multi_thread")]
async fn grant_change_on_parent_evicts_descendants() {
    let pool = knot_test_support::fresh_db().await.pool;

    let ws_s = PgWorkspaceStore::new(pool.clone());
    let us = PgUserStore::new(pool.clone());
    let ds = PgDocStore::new(pool.clone());
    let gs = PgGrantStore::new(pool.clone());

    let ws = ws_s.create("default", "W").await.unwrap();
    let u = us.create_local("a@x.test", "A", "$h$").await.unwrap();
    ws_s.add_member(ws.id, u.id, WorkspaceRole::Viewer)
        .await
        .unwrap();
    let parent = ds.create(ws.id, None, "Parent", "m", u.id).await.unwrap();
    let child = ds
        .create(ws.id, Some(parent.id), "Child", "m", u.id)
        .await
        .unwrap();

    let cache = Arc::new(AclCache::new(
        Arc::new(ws_s.clone()),
        Arc::new(gs.clone()),
        Arc::new(ds.clone()),
    ));
    let _handle = spawn_listener(
        pool.clone(),
        cache.clone(),
        Arc::new(ds.clone()),
        Arc::new(|_: uuid::Uuid| {}),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Prime BOTH cache entries.
    assert_eq!(
        cache.effective_role(ws.id, parent.id, u.id).await.unwrap(),
        Some(WorkspaceRole::Viewer)
    );
    assert_eq!(
        cache.effective_role(ws.id, child.id, u.id).await.unwrap(),
        Some(WorkspaceRole::Viewer)
    );

    // Grant Owner on parent with inherit=true.
    gs.put(
        ws.id,
        parent.id,
        &format!("user:{}", u.id),
        WorkspaceRole::Owner,
        true,
        u.id,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Both parent AND child should now resolve to Owner. If subtree
    // eviction failed, the child entry would still serve the cached Viewer.
    assert_eq!(
        cache.effective_role(ws.id, parent.id, u.id).await.unwrap(),
        Some(WorkspaceRole::Owner)
    );
    assert_eq!(
        cache.effective_role(ws.id, child.id, u.id).await.unwrap(),
        Some(WorkspaceRole::Owner),
        "child entry must be evicted on parent grant change"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restrict_toggle_evicts_descendants_and_calls_on_invalidate() {
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
    let parent = ds
        .create(ws.id, None, "Parent", "m", owner.id)
        .await
        .unwrap();
    let child = ds
        .create(ws.id, Some(parent.id), "Child", "m", owner.id)
        .await
        .unwrap();

    let cache = Arc::new(AclCache::new(
        Arc::new(ws_s.clone()),
        Arc::new(gs.clone()),
        Arc::new(ds.clone()),
    ));
    let invalidated_ids: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_ids = invalidated_ids.clone();
    let _handle = spawn_listener(
        pool.clone(),
        cache.clone(),
        Arc::new(ds.clone()),
        Arc::new(move |doc_id: Uuid| recorded_ids.lock().unwrap().push(doc_id)),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Prime the per-doc and the batched caches with the unrestricted state.
    assert_eq!(
        cache
            .effective_role(ws.id, child.id, owner.id)
            .await
            .unwrap(),
        Some(WorkspaceRole::Owner)
    );
    assert!(
        cache
            .visibility(ws.id, owner.id)
            .await
            .unwrap()
            .readable(child.id)
    );

    // Restrict the parent the way the restrict route does: flag and
    // invalidation in one transaction.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("UPDATE documents SET restricted = true WHERE id = $1")
        .bind(parent.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    invalidations::record_in_tx(&mut tx, ws.id, parent.id, "restrict")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let recorded = invalidated_ids.lock().unwrap().clone();
    assert!(
        recorded.contains(&parent.id),
        "parent invalidated: {recorded:?}"
    );
    assert!(
        recorded.contains(&child.id),
        "child invalidated: {recorded:?}"
    );
    // The workspace owner holds no grant inside the new boundary, so both
    // caches now hide the subtree.
    assert_eq!(
        cache
            .effective_role(ws.id, child.id, owner.id)
            .await
            .unwrap(),
        None
    );
    let visibility = cache.visibility(ws.id, owner.id).await.unwrap();
    assert!(!visibility.readable(parent.id));
    assert!(!visibility.readable(child.id));
}

/// Listener hooks that record what they were called with: the doc ids
/// passed to `on_invalidate`, and how often `on_invalidate_all` ran.
fn recording_hooks() -> (
    knot_docs::InvalidationHooks,
    Arc<Mutex<Vec<Uuid>>>,
    Arc<AtomicUsize>,
) {
    let invalidated_ids: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
    let invalidate_all_calls = Arc::new(AtomicUsize::new(0));
    let recorded_ids = invalidated_ids.clone();
    let recorded_all_calls = invalidate_all_calls.clone();
    let hooks = knot_docs::InvalidationHooks {
        on_invalidate: Arc::new(move |doc_id: Uuid| recorded_ids.lock().unwrap().push(doc_id)),
        on_invalidate_all: Arc::new(move || {
            recorded_all_calls.fetch_add(1, Ordering::SeqCst);
        }),
    };
    (hooks, invalidated_ids, invalidate_all_calls)
}

/// Wait until the listener has subscribed (again): a NOTIFY sent before
/// that is lost.
async fn wait_for_listener(pool: &sqlx::PgPool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let listening: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND query ILIKE 'LISTEN%acl_invalidate%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if listening > 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the ACL listener never subscribed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait until `on_invalidate_all` has run more than `previous_calls` times.
async fn wait_for_invalidate_all(invalidate_all_calls: &AtomicUsize, previous_calls: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while invalidate_all_calls.load(Ordering::SeqCst) <= previous_calls {
        assert!(
            tokio::time::Instant::now() < deadline,
            "on_invalidate_all never ran"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A workspace with one member of `role` and one top-level doc.
async fn member_and_doc(pool: &sqlx::PgPool, role: WorkspaceRole) -> (Uuid, Uuid, Uuid) {
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let workspace = workspaces.create("default", "W").await.unwrap();
    let member = PgUserStore::new(pool.clone())
        .create_local("m@x.test", "M", "$h$")
        .await
        .unwrap();
    workspaces
        .add_member(workspace.id, member.id, role)
        .await
        .unwrap();
    let doc = PgDocStore::new(pool.clone())
        .create(workspace.id, None, "Doc", "m", member.id)
        .await
        .unwrap();
    (workspace.id, member.id, doc.id)
}

fn acl_cache(pool: &sqlx::PgPool) -> Arc<AclCache> {
    Arc::new(AclCache::new(
        Arc::new(PgWorkspaceStore::new(pool.clone())),
        Arc::new(PgGrantStore::new(pool.clone())),
        Arc::new(PgDocStore::new(pool.clone())),
    ))
}

/// sqlx reconnects a dropped listener connection by itself, and drops the
/// NOTIFYs sent meanwhile. The listener must notice: it evicts the whole
/// cache and has every open socket re-check its access, so a change it
/// never heard of takes effect anyway.
#[tokio::test(flavor = "multi_thread")]
async fn lost_connection_evicts_all_and_rechecks_every_socket() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (workspace_id, member_id, doc_id) = member_and_doc(&pool, WorkspaceRole::Owner).await;
    let cache = acl_cache(&pool);
    let (hooks, _invalidated_ids, invalidate_all_calls) = recording_hooks();
    let _handle = spawn_listener(
        pool.clone(),
        cache.clone(),
        Arc::new(PgDocStore::new(pool.clone())),
        hooks,
    );
    wait_for_listener(&pool).await;
    assert_eq!(
        cache
            .effective_role(workspace_id, doc_id, member_id)
            .await
            .unwrap(),
        Some(WorkspaceRole::Owner)
    );
    let calls_before_loss = invalidate_all_calls.load(Ordering::SeqCst);

    // A change whose NOTIFY the listener never gets.
    sqlx::query("UPDATE workspace_members SET role = 'viewer' WHERE user_id = $1")
        .bind(member_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() AND query ILIKE 'LISTEN%acl_invalidate%'",
    )
    .execute(&pool)
    .await
    .unwrap();

    wait_for_invalidate_all(&invalidate_all_calls, calls_before_loss).await;
    assert_eq!(
        cache
            .effective_role(workspace_id, doc_id, member_id)
            .await
            .unwrap(),
        Some(WorkspaceRole::Viewer),
        "the stale cached role survived the lost connection"
    );
}

/// When the subtree walk fails, the descendants' sockets can't be named:
/// every socket re-checks its access instead of the subtree's staying open.
#[tokio::test(flavor = "multi_thread")]
async fn failed_descendant_walk_rechecks_every_socket() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (workspace_id, _member_id, doc_id) = member_and_doc(&pool, WorkspaceRole::Owner).await;
    // The listener's doc store runs on a closed pool, so `descendant_ids`
    // fails.
    let closed_pool = sqlx::PgPool::connect_lazy_with(pool.connect_options().as_ref().clone());
    closed_pool.close().await;
    let (hooks, invalidated_ids, invalidate_all_calls) = recording_hooks();
    let _handle = spawn_listener(
        pool.clone(),
        acl_cache(&pool),
        Arc::new(PgDocStore::new(closed_pool)),
        hooks,
    );
    wait_for_listener(&pool).await;
    let calls_before_restrict = invalidate_all_calls.load(Ordering::SeqCst);

    let mut tx = pool.begin().await.unwrap();
    invalidations::record_in_tx(&mut tx, workspace_id, doc_id, "restrict")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    wait_for_invalidate_all(&invalidate_all_calls, calls_before_restrict).await;
    assert!(
        invalidated_ids.lock().unwrap().contains(&doc_id),
        "the boundary's own sockets are still revoked"
    );
}

/// A role change is recorded workspace-wide: the listener evicts the whole
/// cache and has every socket re-check its access, so a demoted member
/// loses the cached role on every replica, and their open sockets with it.
#[tokio::test(flavor = "multi_thread")]
async fn role_change_evicts_all_and_rechecks_every_socket() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (workspace_id, member_id, doc_id) = member_and_doc(&pool, WorkspaceRole::Owner).await;
    let cache = acl_cache(&pool);
    let (hooks, _invalidated_ids, invalidate_all_calls) = recording_hooks();
    let _handle = spawn_listener(
        pool.clone(),
        cache.clone(),
        Arc::new(PgDocStore::new(pool.clone())),
        hooks,
    );
    wait_for_listener(&pool).await;
    assert_eq!(
        cache
            .effective_role(workspace_id, doc_id, member_id)
            .await
            .unwrap(),
        Some(WorkspaceRole::Owner)
    );
    let calls_before_demotion = invalidate_all_calls.load(Ordering::SeqCst);

    PgWorkspaceStore::new(pool.clone())
        .update_role(workspace_id, member_id, WorkspaceRole::Viewer)
        .await
        .unwrap();

    wait_for_invalidate_all(&invalidate_all_calls, calls_before_demotion).await;
    assert_eq!(
        cache
            .effective_role(workspace_id, doc_id, member_id)
            .await
            .unwrap(),
        Some(WorkspaceRole::Viewer),
        "the demoted member kept the cached owner role"
    );
}

/// A payload the listener can't read may stand for any change.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_payload_rechecks_every_socket() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (hooks, _invalidated_ids, invalidate_all_calls) = recording_hooks();
    let _handle = spawn_listener(
        pool.clone(),
        acl_cache(&pool),
        Arc::new(PgDocStore::new(pool.clone())),
        hooks,
    );
    wait_for_listener(&pool).await;
    let calls_before_payload = invalidate_all_calls.load(Ordering::SeqCst);

    sqlx::query("SELECT pg_notify('acl_invalidate', 'not-a-doc-id')")
        .execute(&pool)
        .await
        .unwrap();

    wait_for_invalidate_all(&invalidate_all_calls, calls_before_payload).await;
}
