//! Grants never outlive the membership they belong to: the restricted-docs
//! migration drops the ones that already did, `GrantStore::put` refuses a
//! non-member under a lock that orders it against `remove_member`, and the
//! last-owner guard only counts owners who are still members. Also the
//! break-glass `put_break_glass` and the one-snapshot `chain_with_member_role`.

use std::time::Duration;

use knot_storage::{
    DocStore, GrantStore, GrantStoreError, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore,
    UserStore, WorkspaceRole, WorkspaceStore,
};
use sqlx::PgPool;
use uuid::Uuid;

/// A workspace with two members, Alice (owner) and Bob (editor).
struct Fixture {
    pool: PgPool,
    docs: PgDocStore,
    grants: PgGrantStore,
    workspaces: PgWorkspaceStore,
    workspace_id: Uuid,
    alice: Uuid,
    bob: Uuid,
}

async fn fixture() -> Fixture {
    let pool = knot_test_support::fresh_db().await.pool;
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let workspace_id = workspaces.create("default", "W").await.unwrap().id;
    let users = PgUserStore::new(pool.clone());
    let alice = users
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap()
        .id;
    let bob = users
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap()
        .id;
    workspaces
        .add_member(workspace_id, alice, WorkspaceRole::Owner)
        .await
        .unwrap();
    workspaces
        .add_member(workspace_id, bob, WorkspaceRole::Editor)
        .await
        .unwrap();
    Fixture {
        docs: PgDocStore::new(pool.clone()),
        grants: PgGrantStore::new(pool.clone()),
        workspaces,
        pool,
        workspace_id,
        alice,
        bob,
    }
}

async fn principals_on(pool: &PgPool, doc_id: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT principal FROM document_grants WHERE doc_id = $1 ORDER BY principal")
        .bind(doc_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Grants of members removed before the upgrade (the old `remove_member`
/// left them behind) and of people never in the doc's workspace (the old
/// grants route did not check) are dropped, so a re-invite or an OIDC
/// re-provision can't revive them. Members' and `group:` grants stay.
#[tokio::test(flavor = "multi_thread")]
async fn restricted_docs_migration_drops_grants_of_non_members() {
    use sqlx::migrate::Migrator;

    const VERSION: i64 = 20261030120000;

    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let full = sqlx::migrate!("../../migrations");
    assert!(
        full.migrations.iter().any(|m| m.version == VERSION),
        "migration {VERSION} not embedded (cargo clean -p knot-storage?)"
    );
    let before = Migrator {
        migrations: std::borrow::Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version < VERSION)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    before
        .run(&pool)
        .await
        .expect("migrate to previous version");

    let insert_workspace = |slug: &'static str| {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO workspaces (slug, name) VALUES ($1, $1) RETURNING id",
        )
        .bind(slug)
        .fetch_one(&pool)
    };
    let workspace_id = insert_workspace("default").await.unwrap();
    let other_workspace_id = insert_workspace("other").await.unwrap();
    let mut user_ids = Vec::new();
    for email in ["member@x.test", "removed@x.test", "outsider@x.test"] {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email, display_name) VALUES ($1, $1) RETURNING id",
        )
        .bind(email)
        .fetch_one(&pool)
        .await
        .unwrap();
        user_ids.push(user_id);
    }
    let (member, removed, outsider) = (user_ids[0], user_ids[1], user_ids[2]);
    for (member_workspace_id, user_id) in [(workspace_id, member), (other_workspace_id, outsider)] {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, user_id, role)
             VALUES ($1, $2, 'editor')",
        )
        .bind(member_workspace_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    }
    let doc_id: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(workspace_id)
    .bind(member)
    .fetch_one(&pool)
    .await
    .unwrap();
    for principal in [
        format!("user:{member}"),
        format!("user:{removed}"),
        format!("user:{outsider}"),
        "group:eng".to_string(),
    ] {
        sqlx::query(
            "INSERT INTO document_grants (doc_id, principal, role, inherit, granted_by)
             VALUES ($1, $2, 'owner', true, $3)",
        )
        .bind(doc_id)
        .bind(principal)
        .bind(member)
        .execute(&pool)
        .await
        .unwrap();
    }

    full.run(&pool).await.expect("apply restricted_docs");

    assert_eq!(
        principals_on(&pool, doc_id).await,
        vec!["group:eng".to_string(), format!("user:{member}")],
        "only the member's grant (and the inert group grant) survive"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn put_refuses_a_non_member() {
    let fx = fixture().await;
    let doc = fx
        .docs
        .create(fx.workspace_id, None, "D", "m", fx.alice)
        .await
        .unwrap();
    let outsider = PgUserStore::new(fx.pool.clone())
        .create_local("outsider@x.test", "O", "$h$")
        .await
        .unwrap()
        .id;
    for user_id in [outsider, Uuid::new_v4()] {
        assert!(matches!(
            fx.grants
                .put(
                    fx.workspace_id,
                    doc.id,
                    &format!("user:{user_id}"),
                    WorkspaceRole::Editor,
                    true,
                    fx.alice,
                )
                .await,
            Err(GrantStoreError::NotMember)
        ));
    }
    assert!(principals_on(&fx.pool, doc.id).await.is_empty());
    // A member is fine.
    fx.grants
        .put(
            fx.workspace_id,
            doc.id,
            &format!("user:{}", fx.bob),
            WorkspaceRole::Editor,
            true,
            fx.alice,
        )
        .await
        .unwrap();
}

/// A removal that has deleted the member row but not yet committed holds
/// `put` until it does; `put` then sees the member gone. Without the lock
/// `put` would insert a grant the removal's grant cleanup never saw.
#[tokio::test(flavor = "multi_thread")]
async fn put_waits_for_a_concurrent_member_removal() {
    let fx = fixture().await;
    let doc = fx
        .docs
        .create(fx.workspace_id, None, "D", "m", fx.alice)
        .await
        .unwrap();
    let mut removal = knot_storage::begin(&fx.pool).await.unwrap();
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
        .bind(fx.workspace_id)
        .bind(fx.bob)
        .execute(&mut *removal)
        .await
        .unwrap();

    let grants = fx.grants.clone();
    let (workspace_id, doc_id, bob, alice) = (fx.workspace_id, doc.id, fx.bob, fx.alice);
    let pending_put = tokio::spawn(async move {
        grants
            .put(
                workspace_id,
                doc_id,
                &format!("user:{bob}"),
                WorkspaceRole::Editor,
                true,
                alice,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !pending_put.is_finished(),
        "put must wait for the removal's lock on the member row"
    );
    removal.commit().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), pending_put)
        .await
        .expect("put finishes once the removal commits")
        .unwrap();
    assert!(matches!(outcome, Err(GrantStoreError::NotMember)));
    assert!(principals_on(&fx.pool, doc.id).await.is_empty());
}

/// A stale owner grant of someone who is no longer a member gives nobody a
/// way to manage the doc, so it doesn't count as "another owner".
#[tokio::test(flavor = "multi_thread")]
async fn last_owner_guard_ignores_owner_grants_of_non_members() {
    let fx = fixture().await;
    let doc = fx
        .docs
        .create(fx.workspace_id, None, "D", "m", fx.alice)
        .await
        .unwrap();
    fx.docs
        .set_restricted(fx.workspace_id, doc.id, fx.alice, true)
        .await
        .unwrap();
    fx.grants
        .put(
            fx.workspace_id,
            doc.id,
            &format!("user:{}", fx.bob),
            WorkspaceRole::Owner,
            true,
            fx.alice,
        )
        .await
        .unwrap();
    // Bob leaves the workspace without his grant going with him, as a
    // removal before this release did.
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
        .bind(fx.workspace_id)
        .bind(fx.bob)
        .execute(&fx.pool)
        .await
        .unwrap();

    let alice_principal = format!("user:{}", fx.alice);
    assert!(matches!(
        fx.grants
            .delete(fx.workspace_id, doc.id, &alice_principal, fx.alice)
            .await,
        Err(GrantStoreError::LastOwner)
    ));
    assert!(matches!(
        fx.grants
            .put(
                fx.workspace_id,
                doc.id,
                &alice_principal,
                WorkspaceRole::Editor,
                true,
                fx.alice,
            )
            .await,
        Err(GrantStoreError::LastOwner)
    ));
    // The stale grant itself may go: it was nobody's way in.
    fx.grants
        .delete(
            fx.workspace_id,
            doc.id,
            &format!("user:{}", fx.bob),
            fx.alice,
        )
        .await
        .unwrap();
}

/// The break-glass grant has no acting user: `granted_by` is NULL and the
/// only audit event is `admin.grant` (actor NULL, `break_glass: true`),
/// written with the grant.
#[tokio::test(flavor = "multi_thread")]
async fn put_break_glass_records_no_actor_and_one_audit_event() {
    let fx = fixture().await;
    let doc = fx
        .docs
        .create(fx.workspace_id, None, "D", "m", fx.alice)
        .await
        .unwrap();
    let bob_principal = format!("user:{}", fx.bob);
    fx.grants
        .put_break_glass(
            fx.workspace_id,
            doc.id,
            &bob_principal,
            WorkspaceRole::Owner,
            false,
        )
        .await
        .unwrap();

    let stored = fx.grants.list(doc.id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].principal, bob_principal);
    assert_eq!(stored[0].role, WorkspaceRole::Owner);
    assert!(!stored[0].inherit);
    assert_eq!(stored[0].granted_by, None, "no user granted it");

    let events: Vec<(String, Option<Uuid>, sqlx::types::JsonValue)> = sqlx::query_as(
        "SELECT action, actor_id, data FROM audit_events
          WHERE target_id = $1 AND action LIKE '%grant%'",
    )
    .bind(doc.id)
    .fetch_all(&fx.pool)
    .await
    .unwrap();
    assert_eq!(
        events.len(),
        1,
        "one event, not a doc.grant by the recipient"
    );
    let (action, actor, data) = &events[0];
    assert_eq!(action, "admin.grant");
    assert_eq!(*actor, None);
    assert_eq!(data["break_glass"], true);
    assert_eq!(data["principal"], bob_principal);
    assert_eq!(data["role"], "owner");
    assert_eq!(data["inherit"], false);

    let invalidated: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM acl_invalidations WHERE doc_id = $1")
            .bind(doc.id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(invalidated >= 2, "create and the grant each invalidate");

    // Members only, and the last-owner guard still applies.
    let outsider = format!("user:{}", Uuid::new_v4());
    assert!(matches!(
        fx.grants
            .put_break_glass(
                fx.workspace_id,
                doc.id,
                &outsider,
                WorkspaceRole::Owner,
                true
            )
            .await,
        Err(GrantStoreError::NotMember)
    ));
    fx.docs
        .set_restricted(fx.workspace_id, doc.id, fx.bob, true)
        .await
        .unwrap();
    fx.grants
        .delete(
            fx.workspace_id,
            doc.id,
            &format!("user:{}", fx.alice),
            fx.alice,
        )
        .await
        .unwrap();
    assert!(matches!(
        fx.grants
            .put_break_glass(
                fx.workspace_id,
                doc.id,
                &bob_principal,
                WorkspaceRole::Viewer,
                true,
            )
            .await,
        Err(GrantStoreError::LastOwner)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn chain_with_member_role_reads_chain_grants_and_role() {
    let fx = fixture().await;
    let root = fx
        .docs
        .create(fx.workspace_id, None, "Root", "m", fx.alice)
        .await
        .unwrap();
    let leaf = fx
        .docs
        .create(fx.workspace_id, Some(root.id), "Leaf", "m", fx.alice)
        .await
        .unwrap();
    fx.grants
        .put(
            fx.workspace_id,
            root.id,
            &format!("user:{}", fx.bob),
            WorkspaceRole::Owner,
            false,
            fx.alice,
        )
        .await
        .unwrap();

    let (chain, bob_grants, bob_role) = fx
        .grants
        .chain_with_member_role(fx.workspace_id, leaf.id, fx.bob)
        .await
        .unwrap();
    assert_eq!(
        chain.iter().map(|node| node.id).collect::<Vec<_>>(),
        vec![root.id, leaf.id],
        "root first"
    );
    assert_eq!(bob_grants.len(), 1);
    assert_eq!(bob_grants[0].doc_id, root.id);
    assert_eq!(bob_role, Some(WorkspaceRole::Editor));

    fx.workspaces
        .remove_member(fx.workspace_id, fx.bob)
        .await
        .unwrap();
    let (_, bob_grants, bob_role) = fx
        .grants
        .chain_with_member_role(fx.workspace_id, leaf.id, fx.bob)
        .await
        .unwrap();
    assert!(bob_grants.is_empty());
    assert_eq!(bob_role, None);
}
