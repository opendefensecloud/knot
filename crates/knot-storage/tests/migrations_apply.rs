//! Verify the v0.1 migration applies cleanly against a fresh Postgres
//! and creates the expected 13 user tables.

#[tokio::test(flavor = "multi_thread")]
async fn migrations_apply_cleanly() {
    // Empty DB on shared container; let `connect()` apply migrations
    // so this test actually exercises that code path.
    let url = knot_test_support::fresh_db_url().await;

    let pool = knot_storage::connect(&url, 4)
        .await
        .expect("connect + migrate");

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name::text \
         FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name != '_sqlx_migrations' \
         ORDER BY table_name",
    )
    .fetch_all(&pool)
    .await
    .expect("query tables");
    let names: Vec<String> = rows.into_iter().map(|(n,)| n).collect();

    let expected: &[&str] = &[
        "acl_invalidations",
        "audit_events",
        "blob_bytes",
        "blobs",
        "board_snapshots",
        "board_updates",
        "boards",
        "comment_reactions",
        "comments",
        "doc_contributors",
        "doc_markdown_cache",
        "doc_snapshots",
        "doc_tasks",
        "doc_updates",
        "document_grants",
        "documents",
        "sessions",
        "share_tokens",
        "users",
        "workspace_members",
        "workspaces",
    ];
    assert_eq!(
        names.iter().map(String::as_str).collect::<Vec<_>>(),
        expected,
        "v0.1 schema must define exactly these tables"
    );
}

/// The doc_contributors migration backfills from the attribution already in
/// `doc_updates`: one row per (doc, author) spanning that author's first and
/// last surviving update. Anonymous rows (`by_user_id IS NULL`) credit nobody.
/// Existing docs get `contributors_since` = migration time, since history
/// before that is only partially attributed (live edits never were).
#[tokio::test(flavor = "multi_thread")]
async fn doc_contributors_migration_backfills_from_doc_updates() {
    use chrono::{DateTime, TimeZone, Utc};
    use sqlx::migrate::Migrator;
    use uuid::Uuid;

    const VERSION: i64 = 20261003120000;

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

    // Seed the pre-migration world with raw SQL (the stores already speak the
    // new schema).
    let ws: Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (slug, name) VALUES ('default', 'W') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut users = Vec::new();
    for (email, name) in [("a@x.test", "Alice"), ("b@x.test", "Bob")] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email, display_name) VALUES ($1, $2) RETURNING id",
        )
        .bind(email)
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
        users.push(id);
    }
    let (alice, bob) = (users[0], users[1]);
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by, created_at)
         VALUES ($1, 'D', 'm', $2, '2026-01-01T00:00:00Z') RETURNING id",
    )
    .bind(ws)
    .bind(alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    let t = |h: u32| -> DateTime<Utc> { Utc.with_ymd_and_hms(2026, 3, 3, h, 0, 0).unwrap() };
    for (by, at) in [
        (Some(alice), t(10)),
        (Some(bob), t(11)),
        (None, t(12)),
        (Some(alice), t(13)),
    ] {
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, by_user_id, created_at)
             VALUES ($1, '\\x00', $2, $3)",
        )
        .bind(doc)
        .bind(by)
        .bind(at)
        .execute(&pool)
        .await
        .unwrap();
    }

    let migrated_from: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    full.run(&pool).await.expect("apply doc_contributors");

    let mut rows: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT user_id, first_edited_at, last_edited_at
         FROM doc_contributors WHERE doc_id = $1",
    )
    .bind(doc)
    .fetch_all(&pool)
    .await
    .unwrap();
    rows.sort_by_key(|r| r.1);
    assert_eq!(
        rows,
        vec![(alice, t(10), t(13)), (bob, t(11), t(11))],
        "backfill must yield one row per author spanning first..last update"
    );

    let since: DateTime<Utc> =
        sqlx::query_scalar("SELECT contributors_since FROM documents WHERE id = $1")
            .bind(doc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        since >= migrated_from,
        "existing docs must get the migration time as contributors_since, got {since}"
    );
}
