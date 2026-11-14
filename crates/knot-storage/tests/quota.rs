//! Quota usage counts (issue #88): pages and file bytes per workspace and
//! per creator. Archived rows count until they are purged; other users and
//! other workspaces never do.

use knot_storage::quota::{
    blob_bytes_created_by, blob_bytes_in_workspace, pages_created_by, pages_in_workspace,
};
use knot_storage::{
    BlobMeta, BlobMetadata, DocStore, PgDocStore, PgUserStore, PgWorkspaceStore, Placement,
    UserStore, WorkspaceRole, WorkspaceStore,
};
use sqlx::PgPool;
use sqlx::types::JsonValue;
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    docs: PgDocStore,
    blobs: BlobMeta,
    workspace_id: Uuid,
    other_workspace_id: Uuid,
    alice: Uuid,
    bob: Uuid,
}

async fn setup() -> Fixture {
    let pool = knot_test_support::fresh_db().await.pool;
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let users = PgUserStore::new(pool.clone());
    let workspace = workspaces.create("default", "W").await.unwrap();
    let other_workspace = workspaces.create("other", "Other").await.unwrap();
    let alice = users
        .create_local("alice@x.test", "A", "$h$")
        .await
        .unwrap();
    let bob = users.create_local("bob@x.test", "B", "$h$").await.unwrap();
    for (workspace_id, user_id) in [
        (workspace.id, alice.id),
        (workspace.id, bob.id),
        (other_workspace.id, alice.id),
    ] {
        workspaces
            .add_member(workspace_id, user_id, WorkspaceRole::Owner)
            .await
            .unwrap();
    }
    Fixture {
        docs: PgDocStore::new(pool.clone()),
        blobs: BlobMeta::new(pool.clone()),
        pool,
        workspace_id: workspace.id,
        other_workspace_id: other_workspace.id,
        alice: alice.id,
        bob: bob.id,
    }
}

impl Fixture {
    async fn page(&self, workspace_id: Uuid, created_by: Uuid) -> Uuid {
        self.docs
            .create_placed(workspace_id, None, "Page", Placement::Last, created_by)
            .await
            .unwrap()
            .id
    }

    async fn blob(&self, workspace_id: Uuid, doc_id: Uuid, created_by: Uuid, byte_size: i64) {
        self.blobs
            .insert(&BlobMetadata {
                id: Uuid::new_v4(),
                workspace_id,
                doc_id,
                content_type: "image/png".into(),
                byte_size,
                sha256: vec![0; 32],
                original_name: None,
                created_by,
                created_at: chrono::Utc::now(),
            })
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_workspace_counts_zero() {
    let f = setup().await;
    assert_eq!(
        pages_in_workspace(&f.pool, f.workspace_id).await.unwrap(),
        0
    );
    assert_eq!(
        pages_created_by(&f.pool, f.workspace_id, f.alice)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        blob_bytes_in_workspace(&f.pool, f.workspace_id)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        blob_bytes_created_by(&f.pool, f.workspace_id, f.alice)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pages_count_archived_ones_and_templates_but_not_other_users_or_workspaces() {
    let f = setup().await;
    let alice_live = f.page(f.workspace_id, f.alice).await;
    let alice_archived = f.page(f.workspace_id, f.alice).await;
    f.docs
        .archive(f.workspace_id, alice_archived, f.alice)
        .await
        .unwrap();
    let alice_template = f.page(f.workspace_id, f.alice).await;
    sqlx::query("UPDATE documents SET is_template = true WHERE id = $1")
        .bind(alice_template)
        .execute(&f.pool)
        .await
        .unwrap();
    f.page(f.workspace_id, f.bob).await;
    f.page(f.other_workspace_id, f.alice).await;
    let _ = alice_live;

    assert_eq!(
        pages_created_by(&f.pool, f.workspace_id, f.alice)
            .await
            .unwrap(),
        3,
        "live, archived and template pages of alice in this workspace"
    );
    assert_eq!(
        pages_created_by(&f.pool, f.workspace_id, f.bob)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        pages_in_workspace(&f.pool, f.workspace_id).await.unwrap(),
        4
    );
    assert_eq!(
        pages_in_workspace(&f.pool, f.other_workspace_id)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_bytes_sum_archived_pages_files_but_not_other_users_or_workspaces() {
    let f = setup().await;
    let live_page = f.page(f.workspace_id, f.alice).await;
    let archived_page = f.page(f.workspace_id, f.alice).await;
    let other_page = f.page(f.other_workspace_id, f.alice).await;
    f.blob(f.workspace_id, live_page, f.alice, 100).await;
    f.blob(f.workspace_id, archived_page, f.alice, 20).await;
    f.docs
        .archive(f.workspace_id, archived_page, f.alice)
        .await
        .unwrap();
    // Bob uploads to alice's page: his bytes, not hers.
    f.blob(f.workspace_id, live_page, f.bob, 3).await;
    f.blob(f.other_workspace_id, other_page, f.alice, 5000)
        .await;

    assert_eq!(
        blob_bytes_created_by(&f.pool, f.workspace_id, f.alice)
            .await
            .unwrap(),
        120
    );
    assert_eq!(
        blob_bytes_created_by(&f.pool, f.workspace_id, f.bob)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        blob_bytes_in_workspace(&f.pool, f.workspace_id)
            .await
            .unwrap(),
        123
    );
    assert_eq!(
        blob_bytes_in_workspace(&f.pool, f.other_workspace_id)
            .await
            .unwrap(),
        5000
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_quota_indexes_exist() {
    let f = setup().await;
    let index_names: Vec<String> = sqlx::query_scalar(
        "SELECT indexname::text FROM pg_indexes
          WHERE indexname IN ('documents_created_by_idx', 'blobs_created_by_bytes_idx')
          ORDER BY indexname",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        index_names,
        ["blobs_created_by_bytes_idx", "documents_created_by_idx"]
    );
    // The definitions the quota counts rely on: the right columns, and the
    // byte sum index-only through INCLUDE.
    let index_definitions: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname::text, indexdef FROM pg_indexes
          WHERE indexname IN ('documents_created_by_idx', 'blobs_created_by_bytes_idx')
          ORDER BY indexname",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    for (index_name, index_definition) in &index_definitions {
        assert!(
            index_definition.contains("(workspace_id, created_by)"),
            "{index_name}: {index_definition}"
        );
    }
    assert_eq!(index_definitions.len(), 2, "{index_definitions:?}");
    let (blobs_index_name, blobs_definition) = &index_definitions[0];
    assert_eq!(blobs_index_name, "blobs_created_by_bytes_idx");
    assert!(
        blobs_definition.contains("INCLUDE (byte_size)"),
        "{blobs_definition}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn record_with_data_writes_the_audit_row_with_its_data() {
    let f = setup().await;
    let data = serde_json::json!({"quota": "pages", "scope": "user", "limit": 3, "used": 3, "requested": 1});
    knot_storage::audit::record_with_data(
        &f.pool,
        f.workspace_id,
        Some(f.alice),
        "quota.exceeded",
        "user",
        f.alice,
        &data,
    )
    .await;
    let stored: (Option<Uuid>, String, String, Uuid, JsonValue) = sqlx::query_as(
        "SELECT actor_id, action, target_kind, target_id, data FROM audit_events
          WHERE workspace_id = $1",
    )
    .bind(f.workspace_id)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        (
            Some(f.alice),
            "quota.exceeded".to_string(),
            "user".to_string(),
            f.alice,
            data
        )
    );
}
