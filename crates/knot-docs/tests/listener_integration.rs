//! Verifies end-to-end: grant change → NOTIFY → listener → cache evict.

use std::sync::Arc;
use std::time::Duration;

use knot_docs::{AclCache, spawn_listener};
use knot_storage::{
    DocStore, GrantStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};

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
