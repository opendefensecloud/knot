use knot_storage::{
    DocStore, DocStoreError, PgDocStore, PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole,
    WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

async fn setup() -> (PgDocStore, Uuid, Uuid) {
    let (store, ws, user, _pool) = setup_with_pool().await;
    (store, ws, user)
}

async fn setup_with_pool() -> (PgDocStore, Uuid, Uuid, sqlx::PgPool) {
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
    (PgDocStore::new(pool.clone()), ws.id, u.id, pool)
}

#[tokio::test(flavor = "multi_thread")]
async fn create_get_list_lifecycle() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "Hello", &sk, user).await.unwrap();
    assert_eq!(doc.title, "Hello");
    assert_eq!(doc.workspace_id, ws);
    let got = store.get(doc.id).await.unwrap().unwrap();
    assert_eq!(got.id, doc.id);
    let list = store.list_alive(ws).await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_updates_title_and_icon() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "Old", &sk, user).await.unwrap();
    let new = store
        .rename(ws, doc.id, user, "New", Some("📄"))
        .await
        .unwrap();
    assert_eq!(new.title, "New");
    assert_eq!(new.icon.as_deref(), Some("📄"));
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_hides_and_restore_brings_back() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "X", &sk, user).await.unwrap();
    store.archive(ws, doc.id, user).await.unwrap();
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 0);
    store.restore(ws, doc.id, user).await.unwrap();
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_to_under_new_parent() {
    let (store, ws, user) = setup().await;
    let a = store.create(ws, None, "A", "m", user).await.unwrap();
    let b = store.create(ws, None, "B", "n", user).await.unwrap();
    let moved = store
        .move_to(ws, b.id, user, Some(a.id), "m")
        .await
        .unwrap();
    assert_eq!(moved.parent_id, Some(a.id));
    let kids = store.siblings(ws, Some(a.id)).await.unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].id, b.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_not_found() {
    let (store, ws, user) = setup().await;
    let err = store
        .rename(ws, Uuid::new_v4(), user, "X", None)
        .await
        .unwrap_err();
    assert!(matches!(err, knot_storage::DocStoreError::NotFound));
}

#[tokio::test(flavor = "multi_thread")]
async fn descendant_ids_returns_full_subtree() {
    let (store, ws, user) = setup().await;
    let a = store.create(ws, None, "A", "m", user).await.unwrap();
    let b = store.create(ws, Some(a.id), "B", "m", user).await.unwrap();
    let c = store.create(ws, Some(b.id), "C", "m", user).await.unwrap();
    let _d = store.create(ws, None, "D", "n", user).await.unwrap(); // unrelated root

    let descendants = store.descendant_ids(a.id).await.unwrap();
    assert_eq!(descendants.len(), 2);
    assert!(descendants.contains(&b.id));
    assert!(descendants.contains(&c.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn templates_flow_set_and_list() {
    let (store, ws, user) = setup().await;
    // Two regular docs + one template.
    let sk = sort_key_between(None, None);
    let a = store.create(ws, None, "A", &sk, user).await.unwrap();
    assert!(!a.is_template);
    let sk = sort_key_between(None, None);
    let b = store.create(ws, None, "B", &sk, user).await.unwrap();
    let sk = sort_key_between(None, None);
    let tpl = store
        .create(ws, None, "Meeting notes", &sk, user)
        .await
        .unwrap();
    // Flip the template flag.
    let after = store.set_template(ws, tpl.id, user, true).await.unwrap();
    assert!(after.is_template);
    // list_alive must exclude the template.
    let alive = store.list_alive(ws).await.unwrap();
    let ids: Vec<_> = alive.iter().map(|d| d.id).collect();
    assert!(ids.contains(&a.id));
    assert!(ids.contains(&b.id));
    assert!(!ids.contains(&tpl.id));
    // list_templates returns just the template.
    let templates = store.list_templates(ws).await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].id, tpl.id);
    // Unmark restores it to the main tree.
    store.set_template(ws, tpl.id, user, false).await.unwrap();
    let alive2 = store.list_alive(ws).await.unwrap();
    assert!(alive2.iter().any(|d| d.id == tpl.id));
    let templates2 = store.list_templates(ws).await.unwrap();
    assert!(templates2.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn move_to_descendant_is_rejected_as_cycle() {
    let (store, ws, u) = setup().await;
    let a = store.create(ws, None, "A", "m", u).await.unwrap();
    let b = store.create(ws, Some(a.id), "B", "m", u).await.unwrap();
    let err = store
        .move_to(ws, a.id, u, Some(b.id), "n")
        .await
        .unwrap_err();
    assert!(
        matches!(err, DocStoreError::Cycle),
        "expected Cycle, got {err:?}"
    );
    let err = store
        .move_to(ws, a.id, u, Some(a.id), "n")
        .await
        .unwrap_err();
    assert!(matches!(err, DocStoreError::Cycle));
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_reorder_and_nest_preserve_all_docs() {
    let (store, ws, u) = setup().await;
    let a = store.create(ws, None, "A", "a", u).await.unwrap();
    let b = store.create(ws, None, "B", "b", u).await.unwrap();
    let c = store.create(ws, None, "C", "c", u).await.unwrap();
    store.move_to(ws, c.id, u, Some(a.id), "m").await.unwrap();
    store.move_to(ws, b.id, u, None, "z").await.unwrap();
    let all = store.list_alive(ws).await.unwrap();
    assert_eq!(all.len(), 3, "no doc lost");
    assert_eq!(
        all.iter().find(|d| d.id == c.id).unwrap().parent_id,
        Some(a.id)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn heal_query_promotes_cycle_members_to_root() {
    let pool = knot_test_support::fresh_db().await.pool;
    let w = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let u = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "U", "$h$")
        .await
        .unwrap();
    let store = PgDocStore::new(pool.clone());
    let a = store.create(w.id, None, "A", "a", u.id).await.unwrap();
    let b = store
        .create(w.id, Some(a.id), "B", "b", u.id)
        .await
        .unwrap();

    // Inject a cycle directly (bypassing the move guard): A.parent = B.
    sqlx::query("UPDATE documents SET parent_id = $1 WHERE id = $2")
        .bind(b.id)
        .bind(a.id)
        .execute(&pool)
        .await
        .unwrap();

    // Run the heal statement (identical SQL to the migration).
    sqlx::query(
        "WITH RECURSIVE anc(start, cur, depth) AS (
             SELECT id, parent_id, 1 FROM documents WHERE parent_id IS NOT NULL
             UNION ALL
             SELECT a.start, d.parent_id, a.depth + 1
             FROM anc a JOIN documents d ON d.id = a.cur
             WHERE a.cur IS NOT NULL AND a.depth < 1000
         )
         UPDATE documents SET parent_id = NULL, updated_at = now()
         WHERE id IN (SELECT start FROM anc WHERE cur = start)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let all = store.list_alive(w.id).await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|d| d.parent_id.is_none()));
}

async fn alive_ids(store: &PgDocStore, ws: Uuid) -> Vec<Uuid> {
    store
        .list_alive(ws)
        .await
        .unwrap()
        .iter()
        .map(|d| d.id)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_stamps_live_subtree_with_one_root() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();
    let grandchild = store
        .create(ws, Some(child.id), "Grandchild", "m", user)
        .await
        .unwrap();
    let other = store.create(ws, None, "Other", "n", user).await.unwrap();

    let stamped = store.archive(ws, folder.id, user).await.unwrap();

    assert_eq!(stamped.first(), Some(&folder.id), "root comes first");
    assert_eq!(stamped.len(), 3);
    for id in [folder.id, child.id, grandchild.id] {
        let doc = store.get(id).await.unwrap().unwrap();
        assert!(doc.archived_at.is_some(), "{} not archived", doc.title);
        assert_eq!(doc.archive_root_id, Some(folder.id));
        assert_eq!(doc.archived_by, Some(user));
    }
    assert_eq!(alive_ids(&store, ws).await, vec![other.id]);
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_leaves_earlier_archived_subtree_alone() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();
    let grandchild = store
        .create(ws, Some(child.id), "Grandchild", "m", user)
        .await
        .unwrap();
    store.archive(ws, child.id, user).await.unwrap();
    let child_archived_at = store.get(child.id).await.unwrap().unwrap().archived_at;

    let stamped = store.archive(ws, folder.id, user).await.unwrap();

    assert_eq!(stamped, vec![folder.id]);
    for id in [child.id, grandchild.id] {
        let doc = store.get(id).await.unwrap().unwrap();
        assert_eq!(
            doc.archive_root_id,
            Some(child.id),
            "{} re-rooted",
            doc.title
        );
    }
    assert_eq!(
        store.get(child.id).await.unwrap().unwrap().archived_at,
        child_archived_at,
        "the earlier archive keeps its own clock"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_twice_is_archived_error() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();
    store.archive(ws, folder.id, user).await.unwrap();

    let again = store.archive(ws, folder.id, user).await.unwrap_err();
    assert!(matches!(again, DocStoreError::Archived), "got {again:?}");
    let member = store.archive(ws, child.id, user).await.unwrap_err();
    assert!(matches!(member, DocStoreError::Archived), "got {member:?}");
    let missing = store.archive(ws, Uuid::new_v4(), user).await.unwrap_err();
    assert!(
        matches!(missing, DocStoreError::NotFound),
        "got {missing:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_brings_back_exactly_the_archive_set() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();
    let grandchild = store
        .create(ws, Some(child.id), "Grandchild", "m", user)
        .await
        .unwrap();
    let separate = store
        .create(ws, Some(folder.id), "Separate", "n", user)
        .await
        .unwrap();
    store.archive(ws, separate.id, user).await.unwrap();
    store.archive(ws, folder.id, user).await.unwrap();

    let mut restored = store.restore(ws, folder.id, user).await.unwrap();

    restored.sort();
    let mut expected = vec![folder.id, child.id, grandchild.id];
    expected.sort();
    assert_eq!(restored, expected);
    for id in [folder.id, child.id, grandchild.id] {
        let doc = store.get(id).await.unwrap().unwrap();
        assert!(doc.archived_at.is_none() && doc.archive_root_id.is_none());
        assert!(doc.archived_by.is_none());
    }
    let back = store.get(folder.id).await.unwrap().unwrap();
    assert_eq!((back.parent_id, back.sort_key.as_str()), (None, "m"));
    let still = store.get(separate.id).await.unwrap().unwrap();
    assert_eq!(
        still.archive_root_id,
        Some(separate.id),
        "own archive stays"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_under_archived_parent_goes_to_top_level() {
    let (store, ws, user) = setup().await;
    let top = store.create(ws, None, "Top", "m", user).await.unwrap();
    let folder = store
        .create(ws, Some(top.id), "Folder", "m", user)
        .await
        .unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();
    store.archive(ws, child.id, user).await.unwrap();
    store.archive(ws, top.id, user).await.unwrap();

    store.restore(ws, child.id, user).await.unwrap();

    let back = store.get(child.id).await.unwrap().unwrap();
    assert_eq!(back.parent_id, None, "parent is archived, so top level");
    assert!(back.sort_key.as_str() > "m", "after the last top-level key");
    assert_eq!(alive_ids(&store, ws).await, vec![child.id]);
    assert!(
        store
            .get(top.id)
            .await
            .unwrap()
            .unwrap()
            .archived_at
            .is_some()
    );
}

/// A restore running while an archive of the parent holds the parent's row
/// lock must wait for it and then see the parent archived. Reading the
/// parent unlocked left the restored doc live under an archived parent,
/// where the parent's purge would later delete it.
#[tokio::test(flavor = "multi_thread")]
async fn restore_waits_for_a_concurrent_archive_of_its_parent() {
    let (store, ws, user, pool) = setup_with_pool().await;
    let parent = store.create(ws, None, "Parent", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(parent.id), "Child", "m", user)
        .await
        .unwrap();
    store.archive(ws, child.id, user).await.unwrap();
    // Stands in for an archive of the parent that has locked and stamped it
    // but not committed yet.
    let mut archiver = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE documents SET archived_at = now(), archived_by = $2, archive_root_id = id
          WHERE id = $1",
    )
    .bind(parent.id)
    .bind(user)
    .execute(&mut *archiver)
    .await
    .unwrap();

    let restorer = {
        let store = store.clone();
        tokio::spawn(async move { store.restore(ws, child.id, user).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    archiver.commit().await.unwrap();
    restorer.await.unwrap().unwrap();

    let back = store.get(child.id).await.unwrap().unwrap();
    assert!(back.archived_at.is_none());
    assert_eq!(
        back.parent_id, None,
        "the parent was archived first: top level"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_of_non_root_is_rejected() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();
    let live = store.create(ws, None, "Live", "n", user).await.unwrap();
    store.archive(ws, folder.id, user).await.unwrap();

    let member = store.restore(ws, child.id, user).await.unwrap_err();
    assert!(
        matches!(member, DocStoreError::NotArchiveRoot),
        "got {member:?}"
    );
    let not_archived = store.restore(ws, live.id, user).await.unwrap_err();
    assert!(
        matches!(not_archived, DocStoreError::NotFound),
        "got {not_archived:?}"
    );
}

/// Purge deletes one root row and relies on every table holding doc content
/// to cascade from `documents`. A table added later without the cascade
/// would block purge (or keep content); this pins the invariant.
#[tokio::test(flavor = "multi_thread")]
async fn every_documents_foreign_key_cascades() {
    let (_store, _ws, _user, pool) = setup_with_pool().await;
    let not_cascading: Vec<String> = sqlx::query_scalar(
        "SELECT conrelid::regclass::text || '.' || conname FROM pg_constraint
          WHERE contype = 'f' AND confrelid = 'documents'::regclass
            AND confdeltype <> 'c'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(not_cascading.is_empty(), "not cascading: {not_cascading:?}");
}

async fn rows_for(pool: &sqlx::PgPool, table: &str, doc_id: Uuid) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE doc_id = $1"
    )))
    .bind(doc_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_deletes_subtree_and_child_rows() {
    let (store, ws, user, pool) = setup_with_pool().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();
    let keep = store.create(ws, None, "Keep", "n", user).await.unwrap();
    for doc_id in [child.id, keep.id] {
        sqlx::query("INSERT INTO doc_updates (doc_id, update_bytes) VALUES ($1, '\\x00')")
            .bind(doc_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO doc_markdown_cache (doc_id, rendered_at_seq, markdown_text)
             VALUES ($1, 1, 'secret')",
        )
        .bind(doc_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO document_grants (doc_id, principal, role) VALUES ($1, 'user:x', 'viewer')",
        )
        .bind(doc_id)
        .execute(&pool)
        .await
        .unwrap();
    }
    store.archive(ws, folder.id, user).await.unwrap();

    let mut purged = store.purge(ws, folder.id, Some(user)).await.unwrap();

    purged.sort();
    let mut expected = vec![folder.id, child.id];
    expected.sort();
    assert_eq!(purged, expected);
    assert!(store.get(folder.id).await.unwrap().is_none());
    assert!(store.get(child.id).await.unwrap().is_none());
    for table in ["doc_updates", "doc_markdown_cache", "document_grants"] {
        assert_eq!(
            rows_for(&pool, table, child.id).await,
            0,
            "{table} kept rows"
        );
        assert_eq!(
            rows_for(&pool, table, keep.id).await,
            1,
            "{table} lost rows"
        );
    }
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
          WHERE action = 'doc.purge' AND target_id = $1 AND actor_id = $2",
    )
    .bind(folder.id)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

/// A move racing an archive can leave a row stamped with the archive's root
/// but outside its subtree (the archive's UPDATE re-checks the moved row,
/// not the subtree). Purging the root must take that row too: on its own it
/// can't be restored, listed or purged, so it would stay forever.
#[tokio::test(flavor = "multi_thread")]
async fn purge_takes_rows_stamped_with_the_root_outside_its_subtree() {
    let (store, ws, user, pool) = setup_with_pool().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let moved = store
        .create(ws, Some(folder.id), "Moved away", "m", user)
        .await
        .unwrap();
    store.archive(ws, folder.id, user).await.unwrap();
    // The state the race leaves behind.
    sqlx::query("UPDATE documents SET parent_id = NULL WHERE id = $1")
        .bind(moved.id)
        .execute(&pool)
        .await
        .unwrap();

    let purged = store.purge(ws, folder.id, None).await.unwrap();

    assert!(purged.contains(&moved.id), "purged: {purged:?}");
    assert!(store.get(moved.id).await.unwrap().is_none());
}

/// Purge deletes blob bytes outside the database (S3) before the rows go.
/// `begin_purge` fences that window: once it has committed, the doc and
/// every archive root inside it refuse to be restored, so nothing can come
/// back with half its attachments gone, and the job retries the purge.
#[tokio::test(flavor = "multi_thread")]
async fn begin_purge_blocks_restore_and_makes_the_purge_due() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let nested = store
        .create(ws, Some(folder.id), "Nested", "m", user)
        .await
        .unwrap();
    let unrelated = store
        .create(ws, None, "Unrelated", "n", user)
        .await
        .unwrap();
    store.archive(ws, nested.id, user).await.unwrap();
    store.archive(ws, folder.id, user).await.unwrap();
    store.archive(ws, unrelated.id, user).await.unwrap();

    store.begin_purge(ws, folder.id).await.unwrap();

    for id in [folder.id, nested.id] {
        let err = store.restore(ws, id, user).await.unwrap_err();
        assert!(matches!(err, DocStoreError::Purging), "got {err:?}");
    }
    let long_ago = chrono::Utc::now() - chrono::Duration::days(365);
    let mut due: Vec<Uuid> = store
        .due_for_purge(long_ago, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, id)| id)
        .collect();
    due.sort();
    let mut expected = vec![folder.id, nested.id];
    expected.sort();
    assert_eq!(due, expected, "requested purges are due whatever their age");
    let roots = store.list_archive_roots(ws).await.unwrap();
    let purging: Vec<(&str, bool)> = roots
        .iter()
        .map(|r| (r.title.as_str(), r.purging))
        .collect();
    assert!(purging.contains(&("Folder", true)) && purging.contains(&("Unrelated", false)));
    store.restore(ws, unrelated.id, user).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn begin_purge_requires_an_archive_root() {
    let (store, ws, user) = setup().await;
    let live = store.create(ws, None, "Live", "m", user).await.unwrap();
    let err = store.begin_purge(ws, live.id).await.unwrap_err();
    assert!(matches!(err, DocStoreError::NotArchived), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_requires_an_archive_root() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();

    let live = store.purge(ws, folder.id, None).await.unwrap_err();
    assert!(matches!(live, DocStoreError::NotArchived), "got {live:?}");

    store.archive(ws, folder.id, user).await.unwrap();
    let member = store.purge(ws, child.id, None).await.unwrap_err();
    assert!(
        matches!(member, DocStoreError::NotArchiveRoot),
        "got {member:?}"
    );
    let missing = store.purge(ws, Uuid::new_v4(), None).await.unwrap_err();
    assert!(
        matches!(missing, DocStoreError::NotFound),
        "got {missing:?}"
    );
    assert!(
        store.get(child.id).await.unwrap().is_some(),
        "nothing deleted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_archive_roots_reports_roots_only() {
    let (store, ws, user, pool) = setup_with_pool().await;
    let parent = store.create(ws, None, "Parent", "m", user).await.unwrap();
    let folder = store
        .create(ws, Some(parent.id), "Folder", "m", user)
        .await
        .unwrap();
    store
        .create(ws, Some(folder.id), "One", "m", user)
        .await
        .unwrap();
    store
        .create(ws, Some(folder.id), "Two", "n", user)
        .await
        .unwrap();
    let older = store.create(ws, None, "Older", "n", user).await.unwrap();
    store.archive(ws, older.id, user).await.unwrap();
    sqlx::query("UPDATE documents SET archived_at = now() - interval '2 days' WHERE id = $1")
        .bind(older.id)
        .execute(&pool)
        .await
        .unwrap();
    store.archive(ws, folder.id, user).await.unwrap();

    let roots = store.list_archive_roots(ws).await.unwrap();

    let titles: Vec<&str> = roots.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, vec!["Folder", "Older"], "roots only, newest first");
    let folder_entry = &roots[0];
    assert_eq!(folder_entry.parent_id, Some(parent.id));
    assert_eq!(folder_entry.parent_title.as_deref(), Some("Parent"));
    assert_eq!(folder_entry.subpage_count, 2);
    assert_eq!(folder_entry.archived_by, Some(user));
    assert_eq!(folder_entry.archived_by_name.as_deref(), Some("A"));
    assert_eq!(roots[1].parent_title, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn due_for_purge_honours_cutoff() {
    let (store, ws, user, pool) = setup_with_pool().await;
    let old = store.create(ws, None, "Old", "m", user).await.unwrap();
    let old_child = store
        .create(ws, Some(old.id), "Old child", "m", user)
        .await
        .unwrap();
    let recent = store.create(ws, None, "Recent", "n", user).await.unwrap();
    store.archive(ws, old.id, user).await.unwrap();
    store.archive(ws, recent.id, user).await.unwrap();
    sqlx::query(
        "UPDATE documents SET archived_at = now() - interval '31 days'
          WHERE archive_root_id = $1",
    )
    .bind(old.id)
    .execute(&pool)
    .await
    .unwrap();

    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let due = store.due_for_purge(cutoff, 100).await.unwrap();

    assert_eq!(due, vec![(ws, old.id)], "roots past the cutoff only");
    assert!(!due.iter().any(|(_, id)| *id == old_child.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_ids_for_docs_and_live_listing() {
    let (store, ws, user, pool) = setup_with_pool().await;
    let live = store.create(ws, None, "Live", "m", user).await.unwrap();
    let gone = store.create(ws, None, "Gone", "n", user).await.unwrap();
    let blobs = knot_storage::BlobMeta::new(pool.clone());
    let mut blob_for = std::collections::HashMap::new();
    for doc_id in [live.id, gone.id] {
        let meta = knot_storage::BlobMetadata {
            id: Uuid::new_v4(),
            workspace_id: ws,
            doc_id,
            content_type: "image/png".into(),
            byte_size: 1,
            sha256: vec![0; 32],
            original_name: None,
            created_by: user,
            created_at: chrono::Utc::now(),
        };
        blobs.insert(&meta).await.unwrap();
        blob_for.insert(doc_id, meta.id);
    }
    store.archive(ws, gone.id, user).await.unwrap();

    let ids = blobs.ids_for_docs(&[gone.id]).await.unwrap();
    assert_eq!(ids, vec![blob_for[&gone.id]]);
    let listed: Vec<Uuid> = blobs
        .list_for_workspace(ws)
        .await
        .unwrap()
        .iter()
        .map(|b| b.id)
        .collect();
    assert_eq!(
        listed,
        vec![blob_for[&live.id]],
        "export skips archived docs"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_under_archived_parent_is_rejected() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    store.archive(ws, folder.id, user).await.unwrap();

    let err = store
        .create(ws, Some(folder.id), "Late", "m", user)
        .await
        .unwrap_err();

    assert!(matches!(err, DocStoreError::Archived), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn move_into_or_of_archived_doc_is_rejected() {
    let (store, ws, user) = setup().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let loose = store.create(ws, None, "Loose", "n", user).await.unwrap();
    store.archive(ws, folder.id, user).await.unwrap();

    let into = store
        .move_to(ws, loose.id, user, Some(folder.id), "m")
        .await
        .unwrap_err();
    assert!(matches!(into, DocStoreError::Archived), "got {into:?}");
    let of = store
        .move_to(ws, folder.id, user, None, "z")
        .await
        .unwrap_err();
    assert!(matches!(of, DocStoreError::Archived), "got {of:?}");
}

/// Creates under a folder's subpage racing an archive of the folder: each
/// create either fails with `Archived` or its child ends up archived with
/// the folder. Never a live child under an archived parent.
#[tokio::test(flavor = "multi_thread")]
async fn create_racing_archive_never_leaves_live_child() {
    let (store, ws, user, pool) = setup_with_pool().await;
    let folder = store.create(ws, None, "Folder", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(folder.id), "Child", "m", user)
        .await
        .unwrap();

    let mut creates = Vec::new();
    for n in 0..20 {
        let store = store.clone();
        creates.push(tokio::spawn(async move {
            let key = format!("k{n:02}");
            store.create(ws, Some(child.id), "Racer", &key, user).await
        }));
    }
    let archiver = {
        let store = store.clone();
        tokio::spawn(async move { store.archive(ws, folder.id, user).await })
    };
    for create in creates {
        match create.await.unwrap() {
            Ok(_) | Err(DocStoreError::Archived) => {}
            Err(e) => panic!("unexpected create error: {e:?}"),
        }
    }
    archiver.await.unwrap().unwrap();

    let orphans: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM documents c JOIN documents p ON c.parent_id = p.id
          WHERE c.archived_at IS NULL AND p.archived_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0, "live child left under an archived parent");
}

/// The archive migration must turn docs soft-deleted before it into
/// archive roots with a fresh clock, and pull their live children to the
/// top level so purging the old parent can't cascade-delete them.
#[tokio::test(flavor = "multi_thread")]
async fn migration_backfill_detaches_live_children() {
    const ARCHIVE_MIGRATION: i64 = 20261013120000;
    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let migrator = sqlx::migrate!("../../migrations");
    for migration in migrator
        .iter()
        .filter(|m| m.version < ARCHIVE_MIGRATION && !m.migration_type.is_down_migration())
    {
        sqlx::raw_sql(migration.sql.clone())
            .execute(&pool)
            .await
            .unwrap();
    }
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let user = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    let folder: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by, archived_at)
         VALUES ($1, 'Old folder', 'm', $2, now() - interval '90 days') RETURNING id",
    )
    .bind(ws.id)
    .bind(user.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let child: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, $2, 'Still used', 'm', $3) RETURNING id",
    )
    .bind(ws.id)
    .bind(folder)
    .bind(user.id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let archive_migration = migrator
        .iter()
        .find(|m| m.version == ARCHIVE_MIGRATION)
        .expect("archive migration exists");
    sqlx::raw_sql(archive_migration.sql.clone())
        .execute(&pool)
        .await
        .unwrap();

    let (root, fresh): (Option<Uuid>, bool) = sqlx::query_as(
        "SELECT archive_root_id, archived_at > now() - interval '1 minute'
           FROM documents WHERE id = $1",
    )
    .bind(folder)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(root, Some(folder));
    assert!(fresh, "retention clock restarts at the upgrade");
    let (parent, archived): (Option<Uuid>, bool) =
        sqlx::query_as("SELECT parent_id, archived_at IS NOT NULL FROM documents WHERE id = $1")
            .bind(child)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(parent, None, "live child detached to the top level");
    assert!(!archived);
}

#[tokio::test(flavor = "multi_thread")]
async fn siblings_include_archived_so_sort_keys_are_not_reused() {
    let (store, ws, user) = setup().await;
    let parent = store.create(ws, None, "P", "m", user).await.unwrap();
    let child = store
        .create(ws, Some(parent.id), "C", "m", user)
        .await
        .unwrap();
    store.archive(ws, child.id, user).await.unwrap();

    // The archived child keeps its sort_key and still counts towards
    // UNIQUE (workspace_id, parent_id, sort_key). If generation cannot see
    // it, the next subpage regenerates "m" and the INSERT conflicts.
    let sibs = store.siblings(ws, Some(parent.id)).await.unwrap();
    assert_eq!(sibs.len(), 1, "archived siblings must stay visible here");

    let next = sort_key_between(None, sibs.first().map(|d| d.sort_key.as_str()));
    assert_ne!(next, child.sort_key);
    store
        .create(ws, Some(parent.id), "C2", &next, user)
        .await
        .unwrap();
}
