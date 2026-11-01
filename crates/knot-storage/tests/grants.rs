use knot_storage::{
    ChainNode, DocStore, GrantStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore,
    UserStore, WorkspaceRole, WorkspaceStore,
};
use uuid::Uuid;

async fn setup() -> (PgDocStore, PgGrantStore, Uuid, Uuid) {
    let (docs, grants, ws, user, _pool) = setup_with_pool().await;
    (docs, grants, ws, user)
}

async fn setup_with_pool() -> (PgDocStore, PgGrantStore, Uuid, Uuid, sqlx::PgPool) {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let u = users.create_local("a@x.test", "A", "$h$").await.unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    (
        PgDocStore::new(pool.clone()),
        PgGrantStore::new(pool.clone()),
        ws.id,
        u.id,
        pool,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn put_list_delete() {
    let (docs, grants, ws, user) = setup().await;
    let d = docs.create(ws, None, "X", "m", user).await.unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, d.id, &principal, WorkspaceRole::Editor, true, user)
        .await
        .unwrap();
    let l = grants.list(d.id).await.unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].role, WorkspaceRole::Editor);
    grants.delete(ws, d.id, &principal, user).await.unwrap();
    assert!(grants.list(d.id).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_updates_existing() {
    let (docs, grants, ws, user) = setup().await;
    let d = docs.create(ws, None, "X", "m", user).await.unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, d.id, &principal, WorkspaceRole::Viewer, true, user)
        .await
        .unwrap();
    grants
        .put(ws, d.id, &principal, WorkspaceRole::Owner, false, user)
        .await
        .unwrap();
    let l = grants.list(d.id).await.unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].role, WorkspaceRole::Owner);
    assert!(!l[0].inherit);
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_includes_ancestor_inherit_true() {
    let (docs, grants, ws, user) = setup().await;
    let root = docs.create(ws, None, "Root", "m", user).await.unwrap();
    let child = docs
        .create(ws, Some(root.id), "Child", "m", user)
        .await
        .unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, root.id, &principal, WorkspaceRole::Editor, true, user)
        .await
        .unwrap();
    let inh = grants.list_inherited(ws, child.id).await.unwrap();
    assert_eq!(inh.len(), 1);
    assert_eq!(inh[0].role, WorkspaceRole::Editor);
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_skips_ancestor_inherit_false() {
    let (docs, grants, ws, user) = setup().await;
    let root = docs.create(ws, None, "Root", "m", user).await.unwrap();
    let child = docs
        .create(ws, Some(root.id), "Child", "m", user)
        .await
        .unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, root.id, &principal, WorkspaceRole::Editor, false, user)
        .await
        .unwrap();
    let inh = grants.list_inherited(ws, child.id).await.unwrap();
    assert!(
        inh.is_empty(),
        "ancestor inherit=false should not propagate"
    );
    // Sanity: own grants always returned regardless of inherit flag.
    grants
        .put(ws, child.id, &principal, WorkspaceRole::Viewer, false, user)
        .await
        .unwrap();
    let inh2 = grants.list_inherited(ws, child.id).await.unwrap();
    assert_eq!(inh2.len(), 1);
    assert_eq!(inh2[0].doc_id, child.id);
    let _ = ws;
    let _ = Uuid::nil(); // silence unused
}

#[tokio::test(flavor = "multi_thread")]
async fn chain_for_principal_is_root_first_and_filters_principal() {
    let (docs, grants, ws, user, pool) = setup_with_pool().await;
    let root = docs.create(ws, None, "Root", "m", user).await.unwrap();
    let middle = docs
        .create(ws, Some(root.id), "Middle", "m", user)
        .await
        .unwrap();
    let leaf = docs
        .create(ws, Some(middle.id), "Leaf", "m", user)
        .await
        .unwrap();
    sqlx::query("UPDATE documents SET restricted = true WHERE id = $1")
        .bind(middle.id)
        .execute(&pool)
        .await
        .unwrap();
    let principal = format!("user:{user}");
    grants
        .put(ws, root.id, &principal, WorkspaceRole::Editor, false, user)
        .await
        .unwrap();
    grants
        .put(ws, leaf.id, &principal, WorkspaceRole::Viewer, true, user)
        .await
        .unwrap();
    grants
        .put(
            ws,
            middle.id,
            "group:someone-else",
            WorkspaceRole::Owner,
            true,
            user,
        )
        .await
        .unwrap();

    let (chain, principal_grants) = grants
        .chain_for_principal(ws, leaf.id, &principal)
        .await
        .unwrap();

    assert_eq!(
        chain,
        vec![
            ChainNode {
                id: root.id,
                parent_id: None,
                restricted: false,
                archived: false,
            },
            ChainNode {
                id: middle.id,
                parent_id: Some(root.id),
                restricted: true,
                archived: false,
            },
            ChainNode {
                id: leaf.id,
                parent_id: Some(middle.id),
                restricted: false,
                archived: false,
            },
        ]
    );
    let mut granted_docs: Vec<(Uuid, WorkspaceRole, bool)> = principal_grants
        .iter()
        .map(|grant| (grant.doc_id, grant.role, grant.inherit))
        .collect();
    granted_docs.sort_by_key(|entry| entry.0);
    let mut expected = vec![
        (root.id, WorkspaceRole::Editor, false),
        (leaf.id, WorkspaceRole::Viewer, true),
    ];
    expected.sort_by_key(|entry| entry.0);
    assert_eq!(granted_docs, expected, "only this principal's grants");
    assert!(
        principal_grants
            .iter()
            .all(|grant| grant.principal == principal)
    );

    // The archived flag is carried per node.
    docs.archive(ws, leaf.id, user).await.unwrap();
    let (archived_chain, _) = grants
        .chain_for_principal(ws, leaf.id, &principal)
        .await
        .unwrap();
    assert!(archived_chain.last().unwrap().archived);
    assert!(!archived_chain[0].archived);

    // A missing doc and a doc in another workspace give an empty chain.
    let (missing_chain, missing_grants) = grants
        .chain_for_principal(ws, Uuid::new_v4(), &principal)
        .await
        .unwrap();
    assert!(missing_chain.is_empty() && missing_grants.is_empty());
    let (foreign_chain, foreign_grants) = grants
        .chain_for_principal(Uuid::new_v4(), leaf.id, &principal)
        .await
        .unwrap();
    assert!(foreign_chain.is_empty() && foreign_grants.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_for_principal_scoped_to_workspace() {
    let (docs, grants, ws, user, pool) = setup_with_pool().await;
    let other_ws = PgWorkspaceStore::new(pool.clone())
        .create("other", "O")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(other_ws.id, user, WorkspaceRole::Owner)
        .await
        .unwrap();
    let here = docs.create(ws, None, "Here", "m", user).await.unwrap();
    let here_child = docs
        .create(ws, Some(here.id), "Here child", "m", user)
        .await
        .unwrap();
    let elsewhere = docs
        .create(other_ws.id, None, "Elsewhere", "m", user)
        .await
        .unwrap();
    let principal = format!("user:{user}");
    grants
        .put(ws, here.id, &principal, WorkspaceRole::Owner, true, user)
        .await
        .unwrap();
    grants
        .put(
            ws,
            here_child.id,
            &principal,
            WorkspaceRole::Viewer,
            false,
            user,
        )
        .await
        .unwrap();
    grants
        .put(
            ws,
            here.id,
            "group:someone-else",
            WorkspaceRole::Editor,
            true,
            user,
        )
        .await
        .unwrap();
    grants
        .put(
            other_ws.id,
            elsewhere.id,
            &principal,
            WorkspaceRole::Editor,
            true,
            user,
        )
        .await
        .unwrap();

    let listed = grants.list_for_principal(ws, &principal).await.unwrap();

    let mut listed_docs: Vec<(Uuid, WorkspaceRole, bool)> = listed
        .iter()
        .map(|grant| (grant.doc_id, grant.role, grant.inherit))
        .collect();
    listed_docs.sort_by_key(|entry| entry.0);
    let mut expected = vec![
        (here.id, WorkspaceRole::Owner, true),
        (here_child.id, WorkspaceRole::Viewer, false),
    ];
    expected.sort_by_key(|entry| entry.0);
    assert_eq!(listed_docs, expected);
    assert!(listed.iter().all(|grant| grant.principal == principal));
}

#[tokio::test(flavor = "multi_thread")]
async fn set_restricted_grants_actor_and_revokes_subtree_shares() {
    let (docs, grants, ws, user, pool) = setup_with_pool().await;
    let boundary = docs.create(ws, None, "B", "m", user).await.unwrap();
    let child = docs
        .create(ws, Some(boundary.id), "C", "m", user)
        .await
        .unwrap();
    let sibling = docs.create(ws, None, "S", "n", user).await.unwrap();
    let shares = knot_storage::PgShareTokenStore::new(pool.clone());
    use knot_storage::ShareTokenStore;
    let child_share = shares.create(ws, child.id, None, user).await.unwrap();
    let sibling_share = shares.create(ws, sibling.id, None, user).await.unwrap();

    let restricted = docs
        .set_restricted(ws, boundary.id, user, true)
        .await
        .unwrap();
    assert!(restricted.restricted);
    assert!(docs.get(boundary.id).await.unwrap().unwrap().restricted);
    let boundary_grants = grants.list(boundary.id).await.unwrap();
    assert_eq!(boundary_grants.len(), 1);
    assert_eq!(boundary_grants[0].principal, format!("user:{user}"));
    assert_eq!(boundary_grants[0].role, WorkspaceRole::Owner);
    assert!(boundary_grants[0].inherit);
    assert!(
        shares
            .find_alive(&child_share.token)
            .await
            .unwrap()
            .is_none(),
        "a link inside the subtree is revoked"
    );
    assert!(
        shares
            .find_alive(&sibling_share.token)
            .await
            .unwrap()
            .is_some(),
        "a link outside it is not"
    );

    // Idempotent, and audited and invalidated each time.
    docs.set_restricted(ws, boundary.id, user, true)
        .await
        .unwrap();
    assert_eq!(grants.list(boundary.id).await.unwrap().len(), 1);
    let unrestricted = docs
        .set_restricted(ws, boundary.id, user, false)
        .await
        .unwrap();
    assert!(!unrestricted.restricted);
    let reasons: Vec<String> = sqlx::query_scalar(
        "SELECT reason FROM acl_invalidations
          WHERE doc_id = $1 AND reason LIKE '%restrict' ORDER BY id",
    )
    .bind(boundary.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(reasons, ["restrict", "restrict", "unrestrict"]);
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE target_id = $1 AND action LIKE 'doc.%restrict'
         ORDER BY created_at, id",
    )
    .bind(boundary.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(actions, ["doc.restrict", "doc.restrict", "doc.unrestrict"]);

    // A doc of another workspace is not found.
    let other_ws = PgWorkspaceStore::new(pool.clone())
        .create("other", "O")
        .await
        .unwrap();
    assert!(matches!(
        docs.set_restricted(other_ws.id, boundary.id, user, true)
            .await,
        Err(knot_storage::DocStoreError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn last_owner_guard_only_on_restricted_docs() {
    let (docs, grants, ws, user, pool) = setup_with_pool().await;
    let doc = docs.create(ws, None, "D", "m", user).await.unwrap();
    let principal = format!("user:{user}");
    // Grants are only for members, and only members' owner grants count.
    let other_user = PgUserStore::new(pool.clone())
        .create_local("b@x.test", "B", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool)
        .add_member(ws, other_user.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let other_principal = format!("user:{}", other_user.id);

    // Unrestricted: the only owner grant can go.
    grants
        .put(ws, doc.id, &principal, WorkspaceRole::Owner, true, user)
        .await
        .unwrap();
    grants.delete(ws, doc.id, &principal, user).await.unwrap();

    docs.set_restricted(ws, doc.id, user, true).await.unwrap();
    assert!(matches!(
        grants.delete(ws, doc.id, &principal, user).await,
        Err(knot_storage::GrantStoreError::LastOwner)
    ));
    assert!(matches!(
        grants
            .put(ws, doc.id, &principal, WorkspaceRole::Editor, true, user)
            .await,
        Err(knot_storage::GrantStoreError::LastOwner)
    ));
    // Refused changes leave the grant as it was.
    let kept = grants.list(doc.id).await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].role, WorkspaceRole::Owner);
    // Re-granting owner, and grants of others, are fine.
    grants
        .put(ws, doc.id, &principal, WorkspaceRole::Owner, false, user)
        .await
        .unwrap();
    grants
        .put(
            ws,
            doc.id,
            &other_principal,
            WorkspaceRole::Viewer,
            true,
            user,
        )
        .await
        .unwrap();
    grants
        .delete(ws, doc.id, &other_principal, user)
        .await
        .unwrap();
    // With a second owner, either may go.
    grants
        .put(
            ws,
            doc.id,
            &other_principal,
            WorkspaceRole::Owner,
            true,
            user,
        )
        .await
        .unwrap();
    grants.delete(ws, doc.id, &principal, user).await.unwrap();
    assert!(matches!(
        grants.delete(ws, doc.id, &other_principal, user).await,
        Err(knot_storage::GrantStoreError::LastOwner)
    ));
}
