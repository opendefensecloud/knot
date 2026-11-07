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
        "api_keys",
        "audit_events",
        "blob_bytes",
        "blob_copy_pending",
        "blobs",
        "board_snapshots",
        "board_updates",
        "boards",
        "comment_reactions",
        "comments",
        "doc_contributors",
        "doc_history_baselines",
        "doc_markdown_cache",
        "doc_markdown_sweep_attempts",
        "doc_snapshots",
        "doc_tasks",
        "doc_updates",
        "document_grants",
        "documents",
        "notifications",
        "sessions",
        "share_tokens",
        "suggestion_replies",
        "suggestions",
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
/// the top level (P7). Where keys tie across groups, top-level rows come
/// first, then template children by parent, as in the sidebar.
async fn root_list(pool: &PgPool, ws: Uuid) -> Vec<(Uuid, String)> {
    sqlx::query_as(
        r#"SELECT d.id, d.sort_key
             FROM documents d
             LEFT JOIN documents p ON p.id = d.parent_id
            WHERE d.workspace_id = $1 AND (d.parent_id IS NULL OR p.is_template)
            ORDER BY d.sort_key COLLATE "C", d.parent_id NULLS FIRST, d.created_at, d.id"#,
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
/// migration's `knot_heal_spread_key` computes the same keys. The sizes
/// cover the three key widths: 1 (n = 1, 2, 10), 2 (n = 35, 36, 100) and 3
/// (n = 700). For n = 35 every key of width 2 ends in '0', which is
/// trimmed, so the longest is one character.
#[tokio::test(flavor = "multi_thread")]
async fn heal_assigns_exactly_lexorank_spread_keys() {
    let (pool, full) = before_heal().await;
    let seed = Seed::new(&pool).await;
    let mut groups = Vec::new();
    for (parent_key, (n, longest)) in ["1", "2", "3", "4", "5", "6", "7"].into_iter().zip([
        (1, 1),
        (2, 1),
        (10, 1),
        (35, 1),
        (36, 2),
        (100, 2),
        (700, 3),
    ]) {
        let parent = seed.page(None, parent_key).await;
        // Every key ends in '0'; zero-padded, they sort in creation order.
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(seed.page(Some(parent), &format!("{i:03}0")).await);
        }
        groups.push((n, longest, ids));
    }

    full.run(&pool).await.expect("apply heal_sort_keys");
    let after = stored(&pool).await;

    for (n, longest, ids) in groups {
        let keys: Vec<&str> = ids.iter().map(|id| after[id].sort_key.as_str()).collect();
        assert_eq!(
            keys,
            knot_storage::lexorank::spread(n),
            "a bad group of {n}"
        );
        assert_eq!(
            keys.iter().map(|k| k.len()).max(),
            Some(longest),
            "the longest key for {n} rows"
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

/// A workspace's top-level sidebar entries, in the order the sidebar shows
/// them: `list_alive`'s rows whose parent it doesn't list (top-level pages,
/// and the children of templates, which it leaves out), sorted by key in
/// code-unit order with a stable sort, as `buildTree` does
/// (web/src/features/docs/tree.ts).
///
/// The rows come from `list_alive`'s query rather than the store itself: the
/// store selects every current column, and this runs against the schema from
/// before later migrations added some.
async fn sidebar_roots(pool: &PgPool, ws: Uuid) -> Vec<Uuid> {
    let docs: Vec<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
        r#"SELECT id, parent_id, sort_key FROM documents
            WHERE workspace_id = $1 AND archived_at IS NULL AND NOT is_template
            ORDER BY parent_id NULLS FIRST, sort_key COLLATE "C", id"#,
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .expect("list_alive rows");
    let listed: BTreeSet<Uuid> = docs.iter().map(|(id, _, _)| *id).collect();
    let mut roots: Vec<_> = docs
        .into_iter()
        .filter(|(_, parent_id, _)| parent_id.is_none_or(|p| !listed.contains(&p)))
        .collect();
    roots.sort_by(|a, b| a.2.as_bytes().cmp(b.2.as_bytes()));
    roots.into_iter().map(|(id, _, _)| id).collect()
}

/// Where keys tie across the groups of a root list, the sidebar shows the
/// top-level page first, then the template children by parent, whatever
/// their age: `list_alive` orders by `parent_id NULLS FIRST`, and
/// `buildTree`'s sort by key is stable. The heal ranks ties the same way
/// (P7, as amended), so the rendered order stays: a template child older
/// than a top-level page with its key still comes after it, and the
/// children of two templates keep their parents' order.
#[tokio::test(flavor = "multi_thread")]
async fn heal_ranks_tied_root_keys_as_the_sidebar_does() {
    let (pool, full) = before_heal().await;
    let seed = Seed::new(&pool).await;
    // Two templates, each with a child keyed `m`. The child of the template
    // with the higher id is created first, so age and parent disagree.
    let templates = [
        seed.template(None, "t").await,
        seed.template(None, "u").await,
    ];
    let (low, high) = (
        *templates.iter().min().unwrap(),
        *templates.iter().max().unwrap(),
    );
    let oldest = seed.page(Some(high), "m").await;
    let older = seed.page(Some(low), "m").await;
    // The top level, created after both children: an invalid key, so the
    // root list needs healing, and a page keyed `m` as well.
    let bad = seed.page(None, "0").await;
    let page = seed.page(None, "m").await;

    let shown = [bad, page, older, oldest];
    let in_root_list = [bad, page, older, oldest, templates[0], templates[1]];
    let ids = |list: Vec<(Uuid, String)>| list.into_iter().map(|r| r.0).collect::<Vec<_>>();
    assert_eq!(
        sidebar_roots(&pool, seed.ws).await,
        shown,
        "the sidebar before"
    );
    assert_eq!(
        ids(root_list(&pool, seed.ws).await),
        in_root_list,
        "the root list in the sidebar's order before"
    );

    full.run(&pool).await.expect("apply heal_sort_keys");

    assert_eq!(
        sidebar_roots(&pool, seed.ws).await,
        shown,
        "the sidebar after"
    );
    let after = root_list(&pool, seed.ws).await;
    let keys: Vec<String> = after.iter().map(|r| r.1.clone()).collect();
    assert_eq!(
        keys,
        knot_storage::lexorank::spread(in_root_list.len()),
        "the root list is respread as one"
    );
    assert_eq!(ids(after), in_root_list, "the root list keeps its order");
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
    let helper: Option<String> =
        sqlx::query_scalar("SELECT to_regprocedure('knot_heal_spread_key(bigint, bigint)')::text")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(helper, None, "the helper is dropped at the end");
}

/// The heal runs as a migration user that may create tables but not
/// temporary objects: TEMPORARY revoked from PUBLIC, a common hardening
/// step. Its helper function lived in `pg_temp`, so the pre-upgrade job
/// failed with "permission denied to create temporary tables", although the
/// user had the `CREATE TABLE` rights the chart asks for. The helper is an
/// ordinary function now, dropped again at the end. The whole chain runs as
/// that user, over bad keys, so the helper is called too.
///
/// Roles belong to the whole server, so the user is dropped again even when
/// a check fails.
#[tokio::test(flavor = "multi_thread")]
async fn heal_sort_keys_runs_without_the_temporary_privilege() {
    use sqlx::AssertSqlSafe;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    let url = knot_test_support::fresh_db_url().await;
    let db = url.rsplit('/').next().unwrap().to_owned();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect as the test role");
    let role = format!("migrator_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(AssertSqlSafe(format!(
        r#"CREATE ROLE "{role}" LOGIN PASSWORD 'migrator';
           REVOKE TEMPORARY ON DATABASE "{db}" FROM PUBLIC;
           GRANT CONNECT, CREATE ON DATABASE "{db}" TO "{role}";
           GRANT USAGE, CREATE ON SCHEMA public TO "{role}";"#
    )))
    .execute(&admin)
    .await
    .expect("a migration user without TEMPORARY");
    let options = url
        .parse::<PgConnectOptions>()
        .unwrap()
        .username(&role)
        .password("migrator");

    let checks = tokio::spawn(heal_as_a_user_without_temporary(options)).await;
    sqlx::raw_sql(AssertSqlSafe(format!(
        r#"REASSIGN OWNED BY "{role}" TO CURRENT_USER;
           DROP OWNED BY "{role}";
           DROP ROLE "{role}";"#
    )))
    .execute(&admin)
    .await
    .expect("drop the migration user");
    if let Err(failed) = checks {
        std::panic::resume_unwind(failed.into_panic());
    }
}

/// The checks of `heal_sort_keys_runs_without_the_temporary_privilege`, as
/// the migration user `options` connects as.
async fn heal_as_a_user_without_temporary(options: sqlx::postgres::PgConnectOptions) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("connect as the migration user");
    let temporary = sqlx::query("CREATE TEMPORARY TABLE probe (x int)")
        .execute(&pool)
        .await;
    assert!(
        matches!(&temporary, Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42501")),
        "the migration user can't create temporary objects: {temporary:?}"
    );

    let full = sqlx::migrate!("../../migrations");
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
        .expect("migrate to just before the heal as the migration user");
    let seed = Seed::new(&pool).await;
    let parent = seed.page(None, "m").await;
    seed.page(None, "m").await;
    seed.page(Some(parent), "0").await;

    full.run(&pool)
        .await
        .expect("apply heal_sort_keys as the migration user");

    let healed = stored(&pool).await;
    assert!(
        healed
            .values()
            .all(|r| knot_storage::lexorank::is_valid(&r.sort_key)),
        "the migration healed every group: {healed:?}"
    );
    let helper: Option<String> =
        sqlx::query_scalar("SELECT to_regprocedure('knot_heal_spread_key(bigint, bigint)')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(helper, None, "the helper is dropped at the end");
    pool.close().await;
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

// ---------------------------------------------------------------------------
// Page history: append-only doc_updates, permanent baselines
// ---------------------------------------------------------------------------

fn kh001(e: sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("KH001")
}

async fn seed_ws_user(pool: &PgPool) -> (Uuid, Uuid) {
    let ws: Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (slug, name) VALUES ('default', 'W') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'Alice') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    (ws, user)
}

async fn seed_doc_with_updates(
    pool: &PgPool,
    ws: Uuid,
    user: Uuid,
    parent: Option<Uuid>,
    n: usize,
) -> Uuid {
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, $2, 'D', 'm', $3) RETURNING id",
    )
    .bind(ws)
    .bind(parent)
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    for _ in 0..n {
        sqlx::query("INSERT INTO doc_updates (doc_id, update_bytes) VALUES ($1, '\\x01')")
            .bind(doc)
            .execute(pool)
            .await
            .unwrap();
    }
    doc
}

async fn count_updates_any(pool: &PgPool, docs: &[Uuid]) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE doc_id = ANY($1)")
        .bind(docs)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A pool on a fresh, fully migrated database with one doc holding two updates.
async fn history_fixture() -> (PgPool, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let doc = seed_doc_with_updates(&pool, ws, user, None, 2).await;
    (pool, ws, doc)
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_delete_of_doc_updates_is_refused() {
    let (pool, _, doc) = history_fixture().await;
    let e = sqlx::query("DELETE FROM doc_updates WHERE doc_id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(kh001(e));
    assert_eq!(count_updates_any(&pool, &[doc]).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn old_gc_delete_statement_is_refused() {
    let (pool, _, doc) = history_fixture().await;
    // The exact statement of knot <= 0.9.8's UpdatesStore::delete_up_to.
    let e = sqlx::query("DELETE FROM doc_updates WHERE doc_id = $1 AND seq <= $2")
        .bind(doc)
        .bind(i64::MAX)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(kh001(e));
    assert_eq!(count_updates_any(&pool, &[doc]).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn rewriting_update_bytes_is_refused() {
    let (pool, _, doc) = history_fixture().await;
    let e = sqlx::query("UPDATE doc_updates SET update_bytes = '\\x00' WHERE doc_id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(kh001(e));
}

#[tokio::test(flavor = "multi_thread")]
async fn flagged_history_rewrite_is_allowed() {
    let (pool, _, doc) = history_fixture().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL knot.history_rewrite = 'on'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let done = sqlx::query("UPDATE doc_updates SET update_bytes = '\\x00' WHERE doc_id = $1")
        .bind(doc)
        .execute(&mut *tx)
        .await
        .unwrap();
    // A trigger returning NULL would skip every row without an error.
    assert_eq!(done.rows_affected(), 2);
    let bytes: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT update_bytes FROM doc_updates WHERE doc_id = $1")
            .bind(doc)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(bytes, vec![vec![0u8], vec![0u8]]);
    tx.rollback().await.unwrap();

    // The flag was transaction-local: the same statement is refused again.
    let e = sqlx::query("UPDATE doc_updates SET update_bytes = '\\x00' WHERE doc_id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap_err();
    assert!(kh001(e));
}

/// The erase path (a later milestone) deletes single rows under the flag.
/// Pooled connections are reused, so a committed flag must not outlive its
/// transaction on that connection.
#[tokio::test(flavor = "multi_thread")]
async fn flagged_history_rewrite_allows_delete_and_does_not_leak() {
    let (pool, _, doc) = history_fixture().await;
    let seqs: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
            .bind(doc)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(seqs.len(), 2);

    let mut conn = pool.acquire().await.unwrap();
    let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let mut tx = sqlx::Connection::begin(&mut *conn).await.unwrap();
    sqlx::query("SET LOCAL knot.history_rewrite = 'on'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let done = sqlx::query("DELETE FROM doc_updates WHERE seq = $1")
        .bind(seqs[0])
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(done.rows_affected(), 1);
    tx.commit().await.unwrap();
    assert_eq!(count_updates_any(&pool, &[doc]).await, 1);

    // Same pooled connection, same backend, after COMMIT: the flag is gone.
    let again: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(again, backend);
    let e = sqlx::query("DELETE FROM doc_updates WHERE seq = $1")
        .bind(seqs[1])
        .execute(&mut *conn)
        .await
        .unwrap_err();
    assert!(kh001(e));
    assert_eq!(count_updates_any(&pool, &[doc]).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_cascade_through_subpages_still_deletes_updates() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let p = seed_doc_with_updates(&pool, ws, user, None, 2).await;
    let c = seed_doc_with_updates(&pool, ws, user, Some(p), 2).await;
    let g = seed_doc_with_updates(&pool, ws, user, Some(c), 2).await;
    assert_eq!(count_updates_any(&pool, &[p, c, g]).await, 6);
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(p)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count_updates_any(&pool, &[p, c, g]).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_delete_cascade_still_deletes_updates() {
    let (pool, ws, doc) = history_fixture().await;
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count_updates_any(&pool, &[doc]).await, 0);
}

/// `e2e/support/reset.ts` wipes tenant data with `TRUNCATE ... CASCADE`, which
/// fires no row triggers; the append-only trigger must not get in its way.
#[tokio::test(flavor = "multi_thread")]
async fn truncate_cascade_is_not_blocked_by_append_only_trigger() {
    let (pool, _, doc) = history_fixture().await;
    sqlx::query(
        "TRUNCATE TABLE doc_history_baselines, doc_snapshots, doc_updates, documents, \
         users, workspaces CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate");
    assert_eq!(count_updates_any(&pool, &[doc]).await, 0);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM doc_updates")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn history_migration_copies_snapshots_and_stamps_history_since() {
    use chrono::{DateTime, Utc};
    use sqlx::migrate::Migrator;

    // The trigger, baselines and copy; then history_since on its own.
    const VERSION: i64 = 20261104120000;
    const HISTORY_SINCE: i64 = 20261104120001;

    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let full = sqlx::migrate!("../../migrations");
    for v in [VERSION, HISTORY_SINCE] {
        assert!(
            full.migrations.iter().any(|m| m.version == v),
            "migration {v} not embedded (cargo clean -p knot-storage?)"
        );
    }
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

    let (ws, user) = seed_ws_user(&pool).await;
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by, created_at)
         VALUES ($1, 'D', 'm', $2, '2026-01-01T00:00:00Z') RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    for (seq, day) in [(10_i64, 2), (20, 3), (30, 4)] {
        sqlx::query(
            "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
             VALUES ($1, $2, $3, $4, make_timestamptz(2026, 1, $5, 0, 0, 0, 'UTC'))",
        )
        .bind(doc)
        .bind(seq)
        .bind(vec![seq as u8, 1, 2])
        .bind(vec![seq as u8, 9])
        .bind(day)
        .execute(&pool)
        .await
        .unwrap();
    }

    full.run(&pool).await.expect("apply history migrations");
    // Two migrations, each recorded on its own.
    let applied: Vec<i64> = sqlx::query_scalar(
        "SELECT version FROM _sqlx_migrations WHERE version >= $1 ORDER BY version",
    )
    .bind(VERSION)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(applied[..2], [VERSION, HISTORY_SINCE]);
    let t_mig: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();

    type Row = (i64, Vec<u8>, Vec<u8>, DateTime<Utc>);
    let snaps: Vec<Row> = sqlx::query_as(
        "SELECT snapshot_seq, state_bytes, state_vector, created_at
         FROM doc_snapshots WHERE doc_id = $1 ORDER BY snapshot_seq",
    )
    .bind(doc)
    .fetch_all(&pool)
    .await
    .unwrap();
    let baselines: Vec<Row> = sqlx::query_as(
        "SELECT snapshot_seq, state_bytes, state_vector, created_at
         FROM doc_history_baselines WHERE doc_id = $1 ORDER BY snapshot_seq",
    )
    .bind(doc)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(baselines.len(), 3);
    assert_eq!(baselines, snaps);

    let (created_at, history_since): (DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as("SELECT created_at, history_since FROM documents WHERE id = $1")
            .bind(doc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(created_at < history_since, "old page predates history");
    assert!((t_mig - history_since).num_seconds().abs() < 60);

    // A page created after the migration is a "new page".
    let fresh: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'N', 'n', $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    let new_page: bool =
        sqlx::query_scalar("SELECT created_at >= history_since FROM documents WHERE id = $1")
            .bind(fresh)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(new_page);

    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM doc_history_baselines WHERE doc_id = $1")
            .bind(doc)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "baselines cascade with the page");
}

/// `history_since` of every page that exists when 20261104120001 runs is the
/// moment 20261104120000 made deletes impossible, however long ago that
/// committed; later pages default to their insert time.
#[tokio::test(flavor = "multi_thread")]
async fn history_since_is_the_moment_deletes_stopped() {
    use chrono::{DateTime, Utc};
    use sqlx::migrate::Migrator;

    const KEEP_UPDATES: i64 = 20261104120000;
    const HISTORY_SINCE: i64 = 20261104120001;

    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let full = sqlx::migrate!("../../migrations");
    let up_to = |last: i64| Migrator {
        migrations: std::borrow::Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version <= last)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    up_to(KEEP_UPDATES - 1)
        .run(&pool)
        .await
        .expect("migrate to before the history migrations");
    let (ws, user) = seed_ws_user(&pool).await;
    let insert_page = |sort_key: &'static str| {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO documents (workspace_id, title, sort_key, created_by)
             VALUES ($1, 'D', $2, $3) RETURNING id",
        )
        .bind(ws)
        .bind(sort_key)
        .bind(user)
        .fetch_one(&pool)
    };
    let old = insert_page("a").await.unwrap();

    up_to(KEEP_UPDATES)
        .run(&pool)
        .await
        .expect("apply keep_updates");
    let keeps_since: DateTime<Utc> = sqlx::query_scalar("SELECT keeps_since FROM doc_history_meta")
        .fetch_one(&pool)
        .await
        .unwrap();
    // Created between the two migrations: its edits are all kept.
    let between = insert_page("b").await.unwrap();

    up_to(HISTORY_SINCE)
        .run(&pool)
        .await
        .expect("apply history_since");
    let after = insert_page("c").await.unwrap();

    let read = |id: Uuid| {
        sqlx::query_as::<_, (DateTime<Utc>, DateTime<Utc>)>(
            "SELECT created_at, history_since FROM documents WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
    };
    let (old_created, old_since) = read(old).await.unwrap();
    assert_eq!(old_since, keeps_since, "existing page: the stored moment");
    assert!(old_created < old_since, "existing page predates history");

    let (between_created, between_since) = read(between).await.unwrap();
    assert_eq!(between_since, keeps_since);
    assert!(
        between_created >= between_since,
        "a page created between the migrations counts as a new page"
    );

    let (after_created, after_since) = read(after).await.unwrap();
    assert_eq!(after_since, after_created, "default is the insert time");

    let meta: Option<String> = sqlx::query_scalar("SELECT to_regclass('doc_history_meta')::text")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(meta, None, "doc_history_meta is dropped");
}

// ---------------------------------------------------------------------------
// API keys and bots (20261106120000_api_keys_and_bots)
// ---------------------------------------------------------------------------

/// The SQLSTATE of a database error, or `None` for any other error.
fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
}

/// A workspace plus one user of the given shape. `kind` is 'human' or 'bot'.
async fn seed_user(
    pool: &PgPool,
    email: &str,
    kind: &str,
    password_hash: Option<&str>,
    oidc_subject: Option<&str>,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (email, display_name, kind, password_hash, oidc_issuer, oidc_subject)
         VALUES ($1, $1, $2, $3, CASE WHEN $4::text IS NULL THEN NULL ELSE 'https://idp.test' END, $4)
         RETURNING id",
    )
    .bind(email)
    .bind(kind)
    .bind(password_hash)
    .bind(oidc_subject)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed_workspace(pool: &PgPool) -> Uuid {
    sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('default', 'W') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Insert a key with a fresh random hash; `expires` is an SQL expression.
async fn insert_key(
    pool: &PgPool,
    ws: Uuid,
    user: Uuid,
    created_by: Uuid,
    expires: &str,
) -> Result<Uuid, sqlx::Error> {
    let sql = format!(
        "INSERT INTO api_keys (workspace_id, user_id, name, key_hash, scopes, created_by, expires_at)
         VALUES ($1, $2, 'k', gen_random_uuid()::text::bytea, ARRAY['read'], $3, {expires})
         RETURNING id"
    );
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(ws)
        .bind(user)
        .bind(created_by)
        .fetch_one(pool)
        .await
}

async fn revoked_at(pool: &PgPool, key: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = $1")
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn api_keys_table_has_expected_columns_and_constraints() {
    let pool = knot_test_support::fresh_db().await.pool;

    let cols: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name::text, data_type::text, is_nullable::text
           FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'api_keys'
          ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let cols: Vec<(&str, &str, &str)> = cols
        .iter()
        .map(|(n, t, null)| (n.as_str(), t.as_str(), null.as_str()))
        .collect();
    assert_eq!(
        cols,
        [
            ("id", "uuid", "NO"),
            ("workspace_id", "uuid", "NO"),
            ("user_id", "uuid", "NO"),
            ("name", "text", "NO"),
            ("key_hash", "bytea", "NO"),
            ("scopes", "ARRAY", "NO"),
            ("created_by", "uuid", "NO"),
            ("created_at", "timestamp with time zone", "NO"),
            ("expires_at", "timestamp with time zone", "YES"),
            ("last_used_at", "timestamp with time zone", "YES"),
            ("revoked_at", "timestamp with time zone", "YES"),
        ],
        "api_keys columns drifted from the spec"
    );

    // key_hash is UNIQUE: the auth lookup relies on it.
    let unique: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
          WHERE conrelid = 'api_keys'::regclass AND contype = 'u'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(unique, ["UNIQUE (key_hash)"], "key_hash must be UNIQUE");

    // Foreign keys: workspace and owner cascade, created_by does not.
    let fks: BTreeSet<(String, String)> = sqlx::query_as(
        "SELECT a.attname::text, c.confdeltype::text
           FROM pg_constraint c
           JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1]
          WHERE c.conrelid = 'api_keys'::regclass AND c.contype = 'f'",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    let want: BTreeSet<(String, String)> =
        [("created_by", "a"), ("user_id", "c"), ("workspace_id", "c")]
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect();
    assert_eq!(fks, want, "api_keys foreign keys / ON DELETE rules drifted");

    let idx: Option<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'api_keys_user_live_idx'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    let idx = idx.expect("partial index api_keys_user_live_idx missing");
    assert!(
        idx.contains("(user_id)") && idx.contains("WHERE (revoked_at IS NULL)"),
        "api_keys_user_live_idx must be a partial index on live keys: {idx}"
    );

    // Behaviour of the CHECK constraints.
    let ws = seed_workspace(&pool).await;
    let user = seed_user(&pool, "h@x.test", "human", Some("pw"), None).await;
    let try_insert = |name: &'static str, scopes: &'static str| {
        let pool = pool.clone();
        async move {
            let sql = format!(
                "INSERT INTO api_keys (workspace_id, user_id, name, key_hash, scopes, created_by)
                 VALUES ($1, $2, $3, gen_random_uuid()::text::bytea, {scopes}, $2)"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(ws)
                .bind(user)
                .bind(name)
                .execute(&pool)
                .await
        }
    };
    try_insert("ok", "ARRAY['read','create','update','delete']")
        .await
        .expect("all four known scopes are accepted");
    for (name, scopes, why) in [
        ("k", "ARRAY[]::text[]", "empty scopes"),
        ("k", "ARRAY['admin']", "unknown scope"),
        (
            "k",
            "ARRAY['read','write']",
            "unknown scope mixed with a known one",
        ),
        ("   ", "ARRAY['read']", "blank name"),
        ("", "ARRAY['read']", "empty name"),
    ] {
        let err = try_insert(name, scopes)
            .await
            .expect_err(&format!("api_keys CHECK must reject {why}"));
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("23514"),
            "{why}: expected check_violation, got {err}"
        );
    }
    let long: &'static str = Box::leak("n".repeat(101).into_boxed_str());
    let err = try_insert(long, "ARRAY['read']")
        .await
        .expect_err("api_keys CHECK must reject a 101-char name");
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");

    // users.kind and the bot-credential constraint.
    let err = sqlx::query(
        "INSERT INTO users (email, display_name, kind) VALUES ('r@x.test', 'R', 'robot')",
    )
    .execute(&pool)
    .await
    .expect_err("users.kind must only allow human|bot");
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    let err = sqlx::query(
        "INSERT INTO users (email, display_name, kind, password_hash) VALUES ('b@x.test', 'B', 'bot', 'pw')",
    )
    .execute(&pool)
    .await
    .expect_err("users_bot_has_no_credentials must reject a bot with a password");
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    let kind: String = sqlx::query_scalar("SELECT kind FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind, "human", "users.kind defaults to human");
}

/// S5: any write that changes password_hash revokes the user's live keys.
#[tokio::test(flavor = "multi_thread")]
async fn password_change_revokes_api_keys() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let user = seed_user(&pool, "h@x.test", "human", Some("old"), None).await;
    let other = seed_user(&pool, "o@x.test", "human", Some("old"), None).await;

    let live = insert_key(&pool, ws, user, user, "NULL").await.unwrap();
    let old_revoked = insert_key(&pool, ws, user, user, "NULL").await.unwrap();
    let others = insert_key(&pool, ws, other, other, "NULL").await.unwrap();
    // A bot key `user` minted: a stolen session must not leave it behind.
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;
    let minted = insert_key(&pool, ws, bot, user, "NULL").await.unwrap();
    let past: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("UPDATE api_keys SET revoked_at = '2020-01-01T00:00:00Z' WHERE id = $1 RETURNING revoked_at")
            .bind(old_revoked)
            .fetch_one(&pool)
            .await
            .unwrap();

    sqlx::query("UPDATE users SET password_hash = 'x' WHERE id = $1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        revoked_at(&pool, live).await.is_some(),
        "S5 broken: password change left the user's key live"
    );
    assert!(
        revoked_at(&pool, minted).await.is_some(),
        "S5 broken: password change left a bot key the user minted live"
    );
    assert_eq!(
        revoked_at(&pool, old_revoked).await,
        Some(past),
        "S5 trigger must not overwrite an earlier revocation timestamp"
    );
    assert_eq!(
        revoked_at(&pool, others).await,
        None,
        "S5 trigger revoked another user's key"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn updating_the_same_password_hash_does_not_revoke() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let user = seed_user(&pool, "h@x.test", "human", Some("same"), None).await;
    let key = insert_key(&pool, ws, user, user, "NULL").await.unwrap();

    sqlx::query("UPDATE users SET password_hash = password_hash, display_name = 'N' WHERE id = $1")
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        revoked_at(&pool, key).await,
        None,
        "S5 trigger must only fire when password_hash actually changes"
    );
}

/// S4 backstop: KB001.
#[tokio::test(flavor = "multi_thread")]
async fn bot_cannot_be_workspace_owner() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;
    let bot2 = seed_user(&pool, "bot2@x.test", "bot", None, None).await;

    let err = sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(ws)
    .bind(bot)
    .execute(&pool)
    .await
    .expect_err("S4 broken: bot inserted as workspace owner");
    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("KB001"),
        "S4: bot owner insert must raise KB001, got {err}"
    );

    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'editor')",
    )
    .bind(ws)
    .bind(bot)
    .execute(&pool)
    .await
    .expect("a bot may be an editor");

    let err = sqlx::query(
        "UPDATE workspace_members SET role = 'owner' WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(ws)
    .bind(bot)
    .execute(&pool)
    .await
    .expect_err("S4 broken: bot promoted to workspace owner");
    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("KB001"),
        "S4: bot owner update must raise KB001, got {err}"
    );

    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'viewer')",
    )
    .bind(ws)
    .bind(bot2)
    .execute(&pool)
    .await
    .expect("a bot may be a viewer");
}

/// S4 backstop: KB002.
#[tokio::test(flavor = "multi_thread")]
async fn bot_cannot_hold_doc_owner_grant() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let human = seed_user(&pool, "h@x.test", "human", Some("pw"), None).await;
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(human)
    .fetch_one(&pool)
    .await
    .unwrap();
    let principal = format!("user:{bot}");
    let grant = |principal: String, role: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("INSERT INTO document_grants (doc_id, principal, role, granted_by) VALUES ($1, $2, $3, $4)")
                .bind(doc)
                .bind(principal)
                .bind(role)
                .bind(human)
                .execute(&pool)
                .await
        }
    };

    let err = grant(principal.clone(), "owner")
        .await
        .expect_err("S4 broken: bot granted document owner");
    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("KB002"),
        "S4: bot doc-owner grant must raise KB002, got {err}"
    );

    grant(principal.clone(), "editor")
        .await
        .expect("a bot may hold an editor grant");

    let err = sqlx::query(
        "UPDATE document_grants SET role = 'owner' WHERE doc_id = $1 AND principal = $2",
    )
    .bind(doc)
    .bind(&principal)
    .execute(&pool)
    .await
    .expect_err("S4 broken: bot grant promoted to owner");
    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("KB002"),
        "S4: bot doc-owner update must raise KB002, got {err}"
    );

    grant("group:engineering".into(), "owner")
        .await
        .expect("KB002 regex guard: a group principal must not trip the bot trigger");
    grant("legacy-text".into(), "owner")
        .await
        .expect("KB002 regex guard: non-user principal must not error on ::uuid cast");
    grant(format!("user:{human}"), "owner")
        .await
        .expect("a human may own a document");
}

/// S6: pure-OIDC humans get keys of at most 90 days, in the DB.
#[tokio::test(flavor = "multi_thread")]
async fn sso_user_key_capped_at_90_days() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let sso = seed_user(&pool, "sso@x.test", "human", None, Some("sub-1")).await;

    for (expires, why) in [
        ("NULL", "a never-expiring key"),
        ("now() + interval '91 days'", "a 91-day key"),
    ] {
        let err = insert_key(&pool, ws, sso, sso, expires)
            .await
            .expect_err(&format!("S6 broken: OIDC-only user got {why}"));
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("KB003"),
            "S6: {why} for an OIDC-only user must raise KB003, got {err}"
        );
    }

    let key = insert_key(&pool, ws, sso, sso, "now() + interval '90 days'")
        .await
        .expect("S6: exactly 90 days is allowed");

    let err = sqlx::query("UPDATE api_keys SET expires_at = NULL WHERE id = $1")
        .bind(key)
        .execute(&pool)
        .await
        .expect_err("S6 broken: OIDC-only key extended to never expire");
    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("KB003"),
        "S6: extending an OIDC-only key must raise KB003, got {err}"
    );

    // A human with a password is not capped by the DB.
    let pw = seed_user(&pool, "pw@x.test", "human", Some("pw"), Some("sub-2")).await;
    insert_key(&pool, ws, pw, pw, "NULL")
        .await
        .expect("S6 must only apply to users without a password");
}

#[tokio::test(flavor = "multi_thread")]
async fn bot_key_may_never_expire() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let owner = seed_user(&pool, "o@x.test", "human", None, Some("sub-1")).await;
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;

    let key = insert_key(&pool, ws, bot, owner, "NULL")
        .await
        .expect("a bot key may have NULL expires_at (S6 must not apply to bots)");
    let exp: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM api_keys WHERE id = $1")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exp, None);
}

async fn add_member(pool: &PgPool, ws: Uuid, user: Uuid, role: &str) {
    sqlx::query("INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(user)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
}

/// S8: leaving the workspace revokes the member's keys and the keys it
/// minted for others, so a re-invite brings nothing back and a departed
/// owner keeps no bot access.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_member_revokes_its_keys_and_the_keys_it_minted() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let owner = seed_user(&pool, "o@x.test", "human", Some("pw"), None).await;
    let owner2 = seed_user(&pool, "o2@x.test", "human", Some("pw"), None).await;
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;
    for (u, role) in [(owner, "owner"), (owner2, "owner"), (bot, "editor")] {
        add_member(&pool, ws, u, role).await;
    }
    let owners_own = insert_key(&pool, ws, owner, owner, "NULL").await.unwrap();
    let minted_for_bot = insert_key(&pool, ws, bot, owner, "NULL").await.unwrap();
    let other_bot_key = insert_key(&pool, ws, bot, owner2, "NULL").await.unwrap();
    let owner2_own = insert_key(&pool, ws, owner2, owner2, "NULL").await.unwrap();

    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
        .bind(ws)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked_at(&pool, owners_own).await.is_some(),
        "own key live"
    );
    assert!(
        revoked_at(&pool, minted_for_bot).await.is_some(),
        "a key the departed owner minted for a bot stays live"
    );
    assert_eq!(revoked_at(&pool, other_bot_key).await, None);
    assert_eq!(revoked_at(&pool, owner2_own).await, None);

    // Removing the bot revokes its remaining key; re-adding it revives none.
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND user_id = $2")
        .bind(ws)
        .bind(bot)
        .execute(&pool)
        .await
        .unwrap();
    assert!(revoked_at(&pool, other_bot_key).await.is_some());
    add_member(&pool, ws, bot, "editor").await;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM api_keys WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(bot)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 0, "re-adding the bot brought a key back");
    assert_eq!(revoked_at(&pool, owner2_own).await, None);

    // Cascading deletes still work with the trigger in place.
    sqlx::query("DELETE FROM api_keys WHERE created_by = $1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(owner)
        .execute(&pool)
        .await
        .expect("deleting a user with no minted keys left");
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ws)
        .execute(&pool)
        .await
        .expect("deleting a workspace cascades through members and keys");
}

/// Losing the owner role revokes the keys the user minted for others; its
/// own keys stay (its role is read at auth time).
#[tokio::test(flavor = "multi_thread")]
async fn demoting_an_owner_revokes_the_keys_it_minted_for_others() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let owner = seed_user(&pool, "o@x.test", "human", Some("pw"), None).await;
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;
    add_member(&pool, ws, owner, "owner").await;
    add_member(&pool, ws, bot, "editor").await;
    let own = insert_key(&pool, ws, owner, owner, "NULL").await.unwrap();
    let for_bot = insert_key(&pool, ws, bot, owner, "NULL").await.unwrap();

    // A bot changing between editor and viewer is not a demotion from owner.
    sqlx::query("UPDATE workspace_members SET role = 'viewer' WHERE user_id = $1")
        .bind(bot)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(revoked_at(&pool, for_bot).await, None);

    sqlx::query("UPDATE workspace_members SET role = 'editor' WHERE user_id = $1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        revoked_at(&pool, for_bot).await.is_some(),
        "bot key outlived the demotion"
    );
    assert_eq!(
        revoked_at(&pool, own).await,
        None,
        "own key revoked on demotion"
    );
}

/// S4: users.kind is immutable, so an owner can never become a bot owner.
#[tokio::test(flavor = "multi_thread")]
async fn user_kind_cannot_change() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let human = seed_user(&pool, "h@x.test", "human", None, None).await;
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;
    add_member(&pool, ws, human, "owner").await;
    for (id, to) in [(human, "bot"), (bot, "human")] {
        let err = sqlx::query("UPDATE users SET kind = $2 WHERE id = $1")
            .bind(id)
            .bind(to)
            .execute(&pool)
            .await
            .expect_err("users.kind changed");
        assert_eq!(sqlstate(&err).as_deref(), Some("KB004"), "{err}");
    }
    sqlx::query("UPDATE users SET kind = kind, display_name = 'N' WHERE id = $1")
        .bind(human)
        .execute(&pool)
        .await
        .expect("writing the same kind is not a change");
}

/// KB002 matches only canonical user principals: a 36-char hex-and-hyphen
/// string that is not a UUID passes instead of failing the ::uuid cast.
#[tokio::test(flavor = "multi_thread")]
async fn bot_owner_guard_ignores_non_uuid_user_principals() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let human = seed_user(&pool, "h@x.test", "human", Some("pw"), None).await;
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(human)
    .fetch_one(&pool)
    .await
    .unwrap();
    for principal in [
        format!("user:{}", "a".repeat(36)),
        format!("user:{}", "-".repeat(36)),
    ] {
        sqlx::query(
            "INSERT INTO document_grants (doc_id, principal, role, granted_by)
             VALUES ($1, $2, 'owner', $3)",
        )
        .bind(doc)
        .bind(&principal)
        .bind(human)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("{principal}: {e}"));
    }
}

/// KB002 recognises the bot in every spelling the app's UUID parser takes,
/// so a row written past the app's canonicalisation can't make it an owner.
#[tokio::test(flavor = "multi_thread")]
async fn bot_owner_guard_matches_every_uuid_spelling() {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = seed_workspace(&pool).await;
    let human = seed_user(&pool, "h@x.test", "human", Some("pw"), None).await;
    let bot = seed_user(&pool, "bot@x.test", "bot", None, None).await;
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(human)
    .fetch_one(&pool)
    .await
    .unwrap();
    let simple = bot.simple().to_string();
    for spelling in [
        bot.to_string().to_uppercase(),
        simple.clone(),
        simple.to_uppercase(),
        format!("{{{bot}}}"),
        format!("urn:uuid:{bot}"),
    ] {
        let principal = format!("user:{spelling}");
        let err = sqlx::query(
            "INSERT INTO document_grants (doc_id, principal, role, granted_by)
             VALUES ($1, $2, 'owner', $3)",
        )
        .bind(doc)
        .bind(&principal)
        .bind(human)
        .execute(&pool)
        .await
        .expect_err(&format!("S4 broken: {principal} inserted as owner"));
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("KB002"),
            "{principal}: {err}"
        );

        sqlx::query(
            "INSERT INTO document_grants (doc_id, principal, role, granted_by)
             VALUES ($1, $2, 'editor', $3)",
        )
        .bind(doc)
        .bind(&principal)
        .bind(human)
        .execute(&pool)
        .await
        .unwrap();
        let err = sqlx::query(
            "UPDATE document_grants SET role = 'owner' WHERE doc_id = $1 AND principal = $2",
        )
        .bind(doc)
        .bind(&principal)
        .execute(&pool)
        .await
        .expect_err(&format!("S4 broken: {principal} promoted to owner"));
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("KB002"),
            "{principal}: {err}"
        );
    }
    // A human in a non-canonical spelling is not refused.
    sqlx::query(
        "INSERT INTO document_grants (doc_id, principal, role, granted_by)
         VALUES ($1, $2, 'owner', $3)",
    )
    .bind(doc)
    .bind(format!("user:{}", human.simple()))
    .bind(human)
    .execute(&pool)
    .await
    .expect("a human may own a document in any spelling");
}

/// The migration waits at most 5 s for its table locks. `SET LOCAL` only
/// holds inside a transaction, so the file must not opt out of sqlx's.
#[test]
fn api_keys_migration_bounds_its_lock_wait() {
    let sql = include_str!("../../../migrations/20261106120000_api_keys_and_bots.sql");
    assert!(
        sql.contains("SET LOCAL lock_timeout = '5s';"),
        "lock_timeout went missing"
    );
    assert!(
        !sql.lines().any(|l| l.trim() == "-- no-transaction"),
        "SET LOCAL does nothing outside sqlx's migration transaction"
    );
}
