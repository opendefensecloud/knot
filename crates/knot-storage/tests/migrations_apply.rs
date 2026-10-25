//! Verify the v0.1 migration applies cleanly against a fresh Postgres
//! and creates the expected 13 user tables.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::PgPool;
use uuid::Uuid;

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

/// Users who exist at upgrade time are backfilled to 0.8.1, the last release
/// without the what's-new card, so they get the card for the release that
/// ships it. Users created afterwards stay NULL until their first visit.
#[tokio::test(flavor = "multi_thread")]
async fn last_seen_version_migration_backfills_existing_users() {
    use sqlx::migrate::Migrator;
    use uuid::Uuid;

    const VERSION: i64 = 20261023120000;

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

    let existing: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('old@x.test', 'Old') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    full.run(&pool).await.expect("apply user_last_seen_version");
    let created_after: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('new@x.test', 'New') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let seen = |id: Uuid| {
        sqlx::query_scalar::<_, Option<String>>("SELECT last_seen_version FROM users WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
    };
    assert_eq!(seen(existing).await.unwrap().as_deref(), Some("0.8.1"));
    assert_eq!(seen(created_after).await.unwrap(), None);
}

// ---------------------------------------------------------------------
// heal_sort_keys (P7): rewrite the sibling groups holding a bad key, then
// make keys unique at the top level too.
// ---------------------------------------------------------------------

const HEAL_SORT_KEYS: i64 = 20261024120000;

/// The whole migration file, to run it again as a re-applied one would.
const HEAL_SORT_KEYS_SQL: &str =
    include_str!("../../../migrations/20261024120000_heal_sort_keys.sql");

/// A database migrated up to just before the heal, and the migrator that
/// applies the rest.
async fn before_heal() -> (PgPool, sqlx::migrate::Migrator) {
    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let full = sqlx::migrate!("../../migrations");
    assert!(
        full.migrations.iter().any(|m| m.version == HEAL_SORT_KEYS),
        "migration {HEAL_SORT_KEYS} not embedded (cargo clean -p knot-storage?)"
    );
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version < HEAL_SORT_KEYS)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before
        .run(&pool)
        .await
        .expect("migrate to previous version");
    (pool, full)
}

/// What a seeded row is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Page,
    Archived,
    Template,
}

/// Raw-SQL rows for the heal tests, in one workspace (the stores already
/// speak the new schema). Each row is created a second after the one
/// before, unless a test pins its `created_at`.
struct Seed {
    pool: PgPool,
    ws: Uuid,
    user: Uuid,
    clock: std::sync::atomic::AtomicI64,
}

impl Seed {
    async fn new(pool: &PgPool) -> Seed {
        let user = sqlx::query_scalar(
            "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'A') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        Seed::in_workspace(pool, user, "default").await
    }

    /// Another workspace, seeded by the same user.
    async fn workspace(&self, slug: &str) -> Seed {
        Seed::in_workspace(&self.pool, self.user, slug).await
    }

    async fn in_workspace(pool: &PgPool, user: Uuid, slug: &str) -> Seed {
        let ws =
            sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ($1, $1) RETURNING id")
                .bind(slug)
                .fetch_one(pool)
                .await
                .unwrap();
        Seed {
            pool: pool.clone(),
            ws,
            user,
            clock: std::sync::atomic::AtomicI64::new(0),
        }
    }

    async fn page(&self, parent: Option<Uuid>, key: &str) -> Uuid {
        self.try_page(parent, key).await.expect("seed a page")
    }

    /// A page created `at` seconds into the clock, so that rows can tie.
    async fn page_at(&self, parent: Option<Uuid>, key: &str, at: i64) -> Uuid {
        self.insert(parent, key, Kind::Page, at)
            .await
            .expect("seed a page")
    }

    async fn archived(&self, parent: Option<Uuid>, key: &str) -> Uuid {
        self.insert(parent, key, Kind::Archived, self.tick())
            .await
            .expect("seed an archived page")
    }

    async fn template(&self, parent: Option<Uuid>, key: &str) -> Uuid {
        self.insert(parent, key, Kind::Template, self.tick())
            .await
            .expect("seed a template")
    }

    async fn try_page(&self, parent: Option<Uuid>, key: &str) -> Result<Uuid, sqlx::Error> {
        self.insert(parent, key, Kind::Page, self.tick()).await
    }

    fn tick(&self) -> i64 {
        self.clock
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    async fn insert(
        &self,
        parent: Option<Uuid>,
        key: &str,
        kind: Kind,
        at: i64,
    ) -> Result<Uuid, sqlx::Error> {
        use chrono::TimeZone;
        let id = Uuid::new_v4();
        let at = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            + chrono::Duration::seconds(at);
        let archived = kind == Kind::Archived;
        sqlx::query(
            "INSERT INTO documents
                 (id, workspace_id, parent_id, title, sort_key, created_by,
                  created_at, updated_at, is_template, archived_at, archive_root_id)
             VALUES ($1, $2, $3, $4, $4, $5, $6, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(self.ws)
        .bind(parent)
        .bind(key)
        .bind(self.user)
        .bind(at)
        .bind(kind == Kind::Template)
        .bind(archived.then_some(at))
        .bind(archived.then_some(id))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }
}

/// A row as the heal may change it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    workspace_id: Uuid,
    parent_id: Option<Uuid>,
    sort_key: String,
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// Every row, by id.
async fn stored(pool: &PgPool) -> BTreeMap<Uuid, Stored> {
    type Row = (
        Uuid,
        Uuid,
        Option<Uuid>,
        String,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<Row> =
        sqlx::query_as("SELECT id, workspace_id, parent_id, sort_key, updated_at FROM documents")
            .fetch_all(pool)
            .await
            .unwrap();
    rows.into_iter()
        .map(|(id, workspace_id, parent_id, sort_key, updated_at)| {
            let row = Stored {
                workspace_id,
                parent_id,
                sort_key,
                updated_at,
            };
            (id, row)
        })
        .collect()
}

/// A sibling group: the rows of one parent in a workspace.
type Group = (Uuid, Option<Uuid>);

/// Each sibling group's ids in the order users see (P7): by key in C
/// order, then `created_at`, then `id`.
async fn group_orders(pool: &PgPool) -> BTreeMap<Group, Vec<Uuid>> {
    let rows: Vec<(Uuid, Option<Uuid>, Uuid)> = sqlx::query_as(
        r#"SELECT workspace_id, parent_id, id FROM documents
            ORDER BY sort_key COLLATE "C", created_at, id"#,
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut groups = BTreeMap::<Group, Vec<Uuid>>::new();
    for (ws, parent, id) in rows {
        groups.entry((ws, parent)).or_default().push(id);
    }
    groups
}

/// A workspace's root list in the order users see, with its keys: the
/// top-level rows and the children of templates, which the sidebar shows at
/// the top level (P7).
async fn root_list(pool: &PgPool, ws: Uuid) -> Vec<(Uuid, String)> {
    sqlx::query_as(
        r#"SELECT d.id, d.sort_key
             FROM documents d
             LEFT JOIN documents p ON p.id = d.parent_id
            WHERE d.workspace_id = $1 AND (d.parent_id IS NULL OR p.is_template)
            ORDER BY d.sort_key COLLATE "C", d.created_at, d.id"#,
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The unique constraints on `documents`, by name, with their definitions.
async fn unique_constraints(pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint
          WHERE conrelid = 'documents'::regclass AND contype = 'u'
          ORDER BY conname",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Compare `sort_key` by a locale, as production databases often do: there
/// `M` sorts beside `m`, not before `a` as in C order. The test server
/// compares by byte, so without this a heal that ranked rows by the
/// database's order rather than C order would pass.
async fn compare_keys_by_locale(pool: &PgPool) {
    sqlx::query(r#"ALTER TABLE documents ALTER COLUMN sort_key TYPE text COLLATE "und-x-icu""#)
        .execute(pool)
        .await
        .expect("an ICU collation on sort_key");
}

/// Groups the heal has no reason to rewrite keep their keys and
/// `updated_at` byte for byte: valid keys up to 64 characters long,
/// archived rows, and a template's child sharing a top-level page's key
/// (another group, so no reason). This holds next to a bad group of the
/// same workspace, and next to another workspace's bad root list.
#[tokio::test(flavor = "multi_thread")]
async fn heal_sort_keys_leaves_healthy_groups_untouched() {
    let (pool, full) = before_heal().await;
    let seed = Seed::new(&pool).await;
    // A healthy root list.
    let a = seed.page(None, "a").await;
    let m = seed.page(None, "m").await;
    seed.archived(None, "z").await;
    let template = seed.template(None, "t").await;
    seed.page(Some(template), "m").await;
    seed.page(Some(template), "n").await;
    // A healthy group below it, with unusual but valid keys.
    seed.page(Some(a), "0z").await;
    seed.page(Some(a), "1").await;
    seed.page(Some(a), &"z".repeat(64)).await;
    seed.archived(Some(a), "y").await;
    // A bad group of this workspace, outside its root list.
    let bad_group = [seed.page(Some(m), "0").await, seed.page(Some(m), "m").await];
    // Another workspace: a bad root list, a healthy group below it.
    let other = seed.workspace("other").await;
    let x = other.page(None, "m").await;
    let bad_root_list = [
        x,
        other.page(None, "m").await,
        other.archived(None, "n").await,
    ];
    other.page(Some(x), "a").await;

    let before = stored(&pool).await;
    full.run(&pool).await.expect("apply heal_sort_keys");
    let after = stored(&pool).await;

    let rewritten: BTreeSet<Uuid> = bad_group.into_iter().chain(bad_root_list).collect();
    for id in &rewritten {
        assert_ne!(
            after[id].sort_key, before[id].sort_key,
            "a row of a bad group is rewritten"
        );
    }
    let untouched = |rows: &BTreeMap<Uuid, Stored>| {
        rows.iter()
            .filter(|(id, _)| !rewritten.contains(*id))
            .map(|(id, row)| (*id, row.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(untouched(&after), untouched(&before));
    assert_eq!(untouched(&before).len(), 11, "the healthy rows seeded");
}

/// The heal rewrites each bad group in the order users see (by key in C
/// order, then `created_at`, then `id`), archived rows included, and leaves
/// keys that are valid and distinct in each group. Only the key changes.
#[tokio::test(flavor = "multi_thread")]
async fn heal_sort_keys_preserves_order_and_fixes_bad_groups() {
    let (pool, full) = before_heal().await;
    compare_keys_by_locale(&pool).await;
    let seed = Seed::new(&pool).await;
    // The top level: a repeated key, archived once and created twice at
    // the same instant (so `id` decides), and `M`, which C order puts
    // before `a`.
    let p = seed.page(None, "m").await;
    seed.page(None, "m").await;
    seed.page_at(None, "m", 1000).await;
    seed.page_at(None, "m", 1000).await;
    seed.archived(None, "m").await;
    seed.page(None, "M").await;
    seed.page(None, "a").await;
    let q = seed.page(None, "n").await;
    // Keys ending in '0', and an empty key.
    for key in ["0", "00", "m0", "", "a"] {
        seed.page(Some(p), key).await;
    }
    seed.archived(Some(p), "b").await;
    // A key over 64 characters, and `M` again.
    for key in [&"a".repeat(70), "b", "M"] {
        seed.page(Some(q), key).await;
    }

    let before = stored(&pool).await;
    let order = group_orders(&pool).await;
    full.run(&pool).await.expect("apply heal_sort_keys");
    let after = stored(&pool).await;

    assert_eq!(
        group_orders(&pool).await,
        order,
        "each group keeps its order"
    );
    let mut seen = BTreeSet::new();
    for row in after.values() {
        let key = &row.sort_key;
        assert!(knot_storage::lexorank::is_valid(key), "invalid key {key:?}");
        assert!(
            seen.insert((row.workspace_id, row.parent_id, key.clone())),
            "repeated key {key:?}"
        );
    }
    for (id, row) in &before {
        let new = &after[id];
        assert_eq!(
            (new.workspace_id, new.parent_id, new.updated_at),
            (row.workspace_id, row.parent_id, row.updated_at),
            "only the key changes"
        );
    }
}

/// A bad group of n rows gets exactly `lexorank::spread(n)`, in order: the
/// migration's `pg_temp.knot_spread_key` computes the same keys.
#[tokio::test(flavor = "multi_thread")]
async fn heal_assigns_exactly_lexorank_spread_keys() {
    let (pool, full) = before_heal().await;
    let seed = Seed::new(&pool).await;
    let mut groups = Vec::new();
    for (parent_key, n) in ["1", "2", "3", "4", "5", "6"]
        .into_iter()
        .zip([1, 2, 10, 35, 36, 100])
    {
        let parent = seed.page(None, parent_key).await;
        // Every key ends in '0'; zero-padded, they sort in creation order.
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(seed.page(Some(parent), &format!("{i:03}0")).await);
        }
        groups.push((n, ids));
    }

    full.run(&pool).await.expect("apply heal_sort_keys");
    let after = stored(&pool).await;

    for (n, ids) in groups {
        let keys: Vec<&str> = ids.iter().map(|id| after[id].sort_key.as_str()).collect();
        assert_eq!(
            keys,
            knot_storage::lexorank::spread(n),
            "a bad group of {n}"
        );
    }
}

/// The sidebar shows the children of templates at the top level, sorted
/// with the top-level pages (P7). A bad top-level group interleaved, by key,
/// with them is respread together with them as one list, so the combined
/// order users see stays, and keys stay distinct within each group.
#[tokio::test(flavor = "multi_thread")]
async fn heal_keeps_rendered_root_order_with_template_children() {
    let (pool, full) = before_heal().await;
    compare_keys_by_locale(&pool).await;
    let seed = Seed::new(&pool).await;
    // A bad top level: a repeated key, and `M`.
    let c = seed.page(None, "c").await;
    seed.page(None, "m").await;
    seed.page(None, "m").await;
    seed.page(None, "M").await;
    seed.archived(None, "q").await;
    let template = seed.template(None, "t").await;
    seed.page(None, "x").await;
    // A healthy template group, interleaved with it.
    for key in ["a", "n", "w"] {
        seed.page(Some(template), key).await;
    }
    seed.archived(Some(template), "o").await;
    // A template below a page: its children show at the top level too,
    // while the template itself stays in its parent's healthy group.
    let nested = seed.template(Some(c), "m").await;
    seed.page(Some(nested), "b").await;
    seed.page(Some(nested), "s").await;

    let before = root_list(&pool, seed.ws).await;
    let nested_row = stored(&pool).await[&nested].clone();
    full.run(&pool).await.expect("apply heal_sort_keys");
    let after = root_list(&pool, seed.ws).await;

    let ids = |list: &[(Uuid, String)]| list.iter().map(|r| r.0).collect::<Vec<_>>();
    assert_eq!(ids(&after), ids(&before), "the root list keeps its order");
    let keys: Vec<&str> = after.iter().map(|r| r.1.as_str()).collect();
    assert_eq!(
        keys,
        knot_storage::lexorank::spread(before.len()),
        "the root list is respread as one"
    );
    assert_eq!(
        stored(&pool).await[&nested],
        nested_row,
        "the nested template keeps its key"
    );
}

/// Running the whole file again, as a re-applied migration would, changes
/// nothing: no group needs healing, the constraint is the same, and the
/// helper function is gone again.
#[tokio::test(flavor = "multi_thread")]
async fn heal_sort_keys_is_idempotent() {
    let (pool, full) = before_heal().await;
    let seed = Seed::new(&pool).await;
    let template = seed.template(None, "t").await;
    let a = seed.page(None, "m").await;
    seed.page(None, "m").await;
    seed.page(Some(template), "0").await;
    seed.page(Some(a), &"a".repeat(70)).await;
    seed.archived(Some(a), "m0").await;
    full.run(&pool).await.expect("apply heal_sort_keys");
    let healed = stored(&pool).await;
    assert!(
        healed
            .values()
            .all(|r| knot_storage::lexorank::is_valid(&r.sort_key)),
        "the migration healed every group: {healed:?}"
    );
    let constraints = unique_constraints(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(HEAL_SORT_KEYS_SQL)
        .execute(&mut *conn)
        .await
        .expect("run heal_sort_keys again");

    assert_eq!(stored(&pool).await, healed);
    assert_eq!(unique_constraints(&pool).await, constraints);
    let helper: Option<String> = sqlx::query_scalar(
        "SELECT to_regprocedure('pg_temp.knot_spread_key(bigint, bigint)')::text",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(helper, None, "the helper is dropped at the end");
}

/// After the heal, keys are unique at the top level as well as under a
/// parent, archived rows included, within one workspace.
#[tokio::test(flavor = "multi_thread")]
async fn top_level_duplicates_are_rejected_after_migration() {
    let (pool, full) = before_heal().await;
    full.run(&pool).await.expect("apply heal_sort_keys");
    assert_eq!(
        unique_constraints(&pool).await,
        [(
            "documents_sort_key_unique".to_owned(),
            "UNIQUE NULLS NOT DISTINCT (workspace_id, parent_id, sort_key) DEFERRABLE".to_owned()
        )]
    );
    let seed = Seed::new(&pool).await;
    let refused_by = |r: Result<Uuid, sqlx::Error>| match r {
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            e.constraint().map(str::to_owned)
        }
        other => panic!("expected a unique violation, got {other:?}"),
    };
    let unique = Some("documents_sort_key_unique".to_owned());

    let top = seed.page(None, "m").await;
    assert_eq!(refused_by(seed.try_page(None, "m").await), unique);
    seed.archived(None, "n").await;
    assert_eq!(refused_by(seed.try_page(None, "n").await), unique);
    seed.page(Some(top), "m").await;
    assert_eq!(refused_by(seed.try_page(Some(top), "m").await), unique);
    // The same key in another group or workspace is fine.
    let template = seed.template(None, "t").await;
    seed.page(Some(template), "m").await;
    seed.workspace("other").await.page(None, "m").await;
}

/// A group's keys are rewritten in one statement (P7). The constraint is
/// checked at the end of the statement, so it may pass through repeated
/// keys: here it swaps two, at the top level and under a parent.
#[tokio::test(flavor = "multi_thread")]
async fn one_statement_may_swap_keys_after_migration() {
    let (pool, full) = before_heal().await;
    full.run(&pool).await.expect("apply heal_sort_keys");
    let seed = Seed::new(&pool).await;
    let a = seed.page(None, "a").await;
    let b = seed.page(None, "b").await;
    let c = seed.page(Some(a), "c").await;
    let d = seed.page(Some(a), "d").await;

    sqlx::query(
        "UPDATE documents d SET sort_key = v.k
           FROM (VALUES ($1::uuid, 'b'), ($2::uuid, 'a'), ($3::uuid, 'd'), ($4::uuid, 'c'))
                AS v(id, k)
          WHERE d.id = v.id",
    )
    .bind(a)
    .bind(b)
    .bind(c)
    .bind(d)
    .execute(&pool)
    .await
    .expect("swap keys in one statement");

    let after = stored(&pool).await;
    let keys: Vec<&str> = [a, b, c, d]
        .iter()
        .map(|id| after[id].sort_key.as_str())
        .collect();
    assert_eq!(keys, ["b", "a", "d", "c"]);
}
