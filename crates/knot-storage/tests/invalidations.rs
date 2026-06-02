use knot_storage::{
    DocStore, GrantStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};
use sqlx::postgres::PgPoolOptions;
use testcontainers_modules::{postgres::Postgres, testcontainers::runners::AsyncRunner};

async fn count_invalidations(pool: &sqlx::PgPool, doc_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM acl_invalidations WHERE doc_id = $1")
        .bind(doc_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn doc_create_move_grant_each_write_invalidation() {
    let container = Postgres::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    std::mem::forget(container);

    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let u = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    let docs = PgDocStore::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());

    let d = docs.create(ws.id, None, "X", "m", u.id).await.unwrap();
    assert_eq!(count_invalidations(&pool, d.id).await, 1, "create");

    docs.move_to(ws.id, d.id, u.id, None, "n").await.unwrap();
    assert_eq!(count_invalidations(&pool, d.id).await, 2, "+ move");

    grants
        .put(
            ws.id,
            d.id,
            &format!("user:{}", u.id),
            WorkspaceRole::Editor,
            true,
            u.id,
        )
        .await
        .unwrap();
    assert_eq!(count_invalidations(&pool, d.id).await, 3, "+ grant put");

    grants
        .delete(ws.id, d.id, &format!("user:{}", u.id), u.id)
        .await
        .unwrap();
    assert_eq!(count_invalidations(&pool, d.id).await, 4, "+ grant delete");
}
