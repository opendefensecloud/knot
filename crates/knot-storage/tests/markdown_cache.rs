use knot_storage::{
    DocStore, MarkdownCacheStore, PgDocStore, PgMarkdownCache, PgUserStore, PgWorkspaceStore,
    UserStore, WorkspaceRole, WorkspaceStore,
};
async fn setup() -> (PgMarkdownCache, uuid::Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;

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
    let d = PgDocStore::new(pool.clone())
        .create(ws.id, None, "D", "m", u.id)
        .await
        .unwrap();
    (PgMarkdownCache::new(pool), d.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn put_then_get_if_fresh() {
    let (s, doc) = setup().await;
    s.put(doc, 42, "# hi\n").await.unwrap();
    let got = s.get_if_fresh(doc, 42).await.unwrap().unwrap();
    assert_eq!(got.markdown_text, "# hi\n");
    assert_eq!(got.rendered_at_seq, 42);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_seq_returns_none() {
    let (s, doc) = setup().await;
    s.put(doc, 42, "# hi\n").await.unwrap();
    assert!(s.get_if_fresh(doc, 43).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_upserts_on_repeat() {
    let (s, doc) = setup().await;
    s.put(doc, 1, "v1").await.unwrap();
    s.put(doc, 2, "v2").await.unwrap();
    let got = s.get_if_fresh(doc, 2).await.unwrap().unwrap();
    assert_eq!(got.markdown_text, "v2");
    assert!(s.get_if_fresh(doc, 1).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn invalidate_removes_row() {
    let (s, doc) = setup().await;
    s.put(doc, 1, "v").await.unwrap();
    s.invalidate(doc).await.unwrap();
    assert!(s.get_if_fresh(doc, 1).await.unwrap().is_none());
}

/// A workspace with its owner, for the tests that need several pages.
struct Pages {
    pool: sqlx::PgPool,
    cache: PgMarkdownCache,
    docs: PgDocStore,
    ws: uuid::Uuid,
    user: uuid::Uuid,
}

impl Pages {
    async fn new() -> Self {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws = PgWorkspaceStore::new(pool.clone())
            .create("default", "W")
            .await
            .unwrap();
        let user = PgUserStore::new(pool.clone())
            .create_local("a@x.test", "A", "$h$")
            .await
            .unwrap();
        PgWorkspaceStore::new(pool.clone())
            .add_member(ws.id, user.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        Self {
            cache: PgMarkdownCache::new(pool.clone()),
            docs: PgDocStore::new(pool.clone()),
            pool,
            ws: ws.id,
            user: user.id,
        }
    }

    /// A new top-level page, with its own order key.
    async fn page(&self, sort_key: &str) -> uuid::Uuid {
        self.docs
            .create(self.ws, None, "D", sort_key, self.user)
            .await
            .unwrap()
            .id
    }

    /// The renderer version stored with the page's cache row.
    async fn version(&self, doc: uuid::Uuid) -> i32 {
        sqlx::query_scalar("SELECT renderer_version FROM doc_markdown_cache WHERE doc_id = $1")
            .bind(doc)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_ids_returns_old_renders_only_and_pages_by_cursor() {
    let p = Pages::new().await;
    let legacy = p.page("a").await;
    let fresh = p.page("b").await;
    let unrendered = p.page("c").await;
    let archived = p.page("d").await;
    p.cache.put(legacy, 1, "old").await.unwrap();
    assert!(p.cache.put_rendered(fresh, 1, "new", 2).await.unwrap());
    p.cache.put(archived, 1, "old").await.unwrap();
    p.docs.archive(p.ws, archived, p.user).await.unwrap();

    let mut stale = vec![legacy, unrendered];
    stale.sort();
    assert_eq!(p.cache.stale_ids(2, None, 10).await.unwrap(), stale);

    // One at a time, each page after the cursor, then nothing.
    assert_eq!(p.cache.stale_ids(2, None, 1).await.unwrap(), [stale[0]]);
    assert_eq!(
        p.cache.stale_ids(2, Some(stale[0]), 1).await.unwrap(),
        [stale[1]]
    );
    assert!(
        p.cache
            .stale_ids(2, Some(stale[1]), 1)
            .await
            .unwrap()
            .is_empty()
    );

    // Against an older renderer, only the page without a row is stale.
    assert_eq!(p.cache.stale_ids(1, None, 10).await.unwrap(), [unrendered]);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_put_marks_row_stale() {
    let p = Pages::new().await;
    let doc = p.page("a").await;
    assert!(p.cache.put_rendered(doc, 1, "new", 2).await.unwrap());
    assert_eq!(p.version(doc).await, 2);
    assert!(p.cache.stale_ids(2, None, 10).await.unwrap().is_empty());

    // An old pod's write during a rolling upgrade.
    p.cache.put(doc, 2, "legacy").await.unwrap();
    assert_eq!(p.version(doc).await, 1);
    assert_eq!(p.cache.stale_ids(2, None, 10).await.unwrap(), [doc]);
}

#[tokio::test(flavor = "multi_thread")]
async fn trigger_keeps_explicit_insert_version() {
    let p = Pages::new().await;
    let doc = p.page("a").await;
    sqlx::query(
        "INSERT INTO doc_markdown_cache (doc_id, rendered_at_seq, markdown_text, renderer_version)
         VALUES ($1, 1, 'seeded', 2)",
    )
    .bind(doc)
    .execute(&p.pool)
    .await
    .unwrap();
    assert_eq!(p.version(doc).await, 2);

    sqlx::query("UPDATE doc_markdown_cache SET markdown_text = 'edited' WHERE doc_id = $1")
        .bind(doc)
        .execute(&p.pool)
        .await
        .unwrap();
    assert_eq!(p.version(doc).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn late_render_does_not_overwrite_a_newer_one() {
    let p = Pages::new().await;
    let doc = p.page("a").await;
    assert!(p.cache.put_rendered(doc, 6, "six", 2).await.unwrap());

    assert!(!p.cache.put_rendered(doc, 5, "five", 2).await.unwrap());
    let got = p.cache.get(doc).await.unwrap().unwrap();
    assert_eq!(
        (got.rendered_at_seq, got.markdown_text.as_str()),
        (6, "six")
    );

    // A render at the same seq replaces it: that is how a newer renderer
    // re-renders a stale row.
    p.cache.put(doc, 6, "legacy six").await.unwrap();
    assert!(p.cache.put_rendered(doc, 6, "six again", 2).await.unwrap());
    let got = p.cache.get(doc).await.unwrap().unwrap();
    assert_eq!(got.markdown_text, "six again");
    assert_eq!(p.version(doc).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn mark_stale_makes_a_current_row_stale_again() {
    let p = Pages::new().await;
    let doc = p.page("a").await;
    assert!(p.cache.put_rendered(doc, 3, "three", 2).await.unwrap());
    let written = p.cache.get(doc).await.unwrap().unwrap();

    p.cache.mark_stale(doc).await.unwrap();

    assert_eq!(p.version(doc).await, 1);
    assert_eq!(p.cache.stale_ids(2, None, 10).await.unwrap(), [doc]);
    // The render itself stays as it was.
    assert_eq!(p.cache.get(doc).await.unwrap().unwrap(), written);

    // A page without a row is stale already, and keeps no row.
    let unrendered = p.page("b").await;
    p.cache.mark_stale(unrendered).await.unwrap();
    assert!(p.cache.get(unrendered).await.unwrap().is_none());
}
