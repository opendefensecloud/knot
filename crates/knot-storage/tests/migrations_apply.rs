//! Verify the v0.1 migration applies cleanly against a fresh Postgres
//! and creates the expected 13 user tables.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::PgPool;
use uuid::Uuid;

/// knot 0.9.9's `INSERT_UPDATES_SQL` (`updates_store.rs`), verbatim: 0.9.9
/// pods keep running it against the M1.2 schema during the rolling upgrade.
const OLD_INSERT_UPDATES_SQL: &str = "WITH ins AS (
             INSERT INTO doc_updates (doc_id, by_user_id, update_bytes)
             SELECT $1, u.by_user_id, u.update_bytes
             FROM UNNEST($2::uuid[], $3::bytea[])
                  WITH ORDINALITY AS u(by_user_id, update_bytes, ord)
             ORDER BY u.ord
             RETURNING seq, by_user_id
         ),
         contrib AS (
             INSERT INTO doc_contributors (doc_id, user_id)
             SELECT DISTINCT $1::uuid, ins.by_user_id
             FROM ins
             WHERE ins.by_user_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM doc_contributors c
                   WHERE c.doc_id = $1
                     AND c.user_id = ins.by_user_id
                     AND c.last_edited_at >= now() - interval '1 minute'
               )
             ORDER BY ins.by_user_id
             ON CONFLICT (doc_id, user_id) DO UPDATE
                 SET last_edited_at = now()
                 WHERE doc_contributors.last_edited_at < now() - interval '1 minute'
         )
         SELECT seq FROM ins ORDER BY seq";

/// knot 0.9.9's snapshot upsert (`PgSnapshotStore::insert`), verbatim.
const OLD_SNAPSHOT_UPSERT_SQL: &str =
    "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (doc_id, snapshot_seq) DO UPDATE
             SET state_bytes = EXCLUDED.state_bytes,
                 state_vector = EXCLUDED.state_vector,
                 created_at = now()";

/// A 0.9.10 snapshot write, stamped with the rev it holds the log up to and
/// the content epoch it was taken at.
const STAMPED_SNAPSHOT_INSERT_SQL: &str = "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, $2, $3, $4, $5, $6)";

/// The shape of 0.9.10's stamped snapshot upsert (`PgSnapshotStore::insert_stamped`).
const STAMPED_SNAPSHOT_UPSERT_SQL: &str = "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (doc_id, snapshot_seq) DO UPDATE
             SET state_bytes = EXCLUDED.state_bytes,
                 state_vector = EXCLUDED.state_vector,
                 created_at = now(),
                 snapshot_rev = EXCLUDED.snapshot_rev,
                 epoch = EXCLUDED.epoch";

/// The second statement of every 0.9.10 flush (plan, Global Constraints):
/// `$1` the page, `$2` the number of rows it writes.
const HEADS_UPSERT_SQL: &str =
    "INSERT INTO doc_history_heads AS h (doc_id, head_rev, dirty, last_write_at)
     VALUES ($1, $2, true, clock_timestamp())
     ON CONFLICT (doc_id) DO UPDATE
     SET head_rev = h.head_rev + $2, dirty = true, last_write_at = clock_timestamp()
     RETURNING h.head_rev - $2 AS base, h.content_epoch";

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
        "blob_copy_pending",
        "blobs",
        "board_snapshots",
        "board_updates",
        "boards",
        "comment_reactions",
        "comments",
        "doc_contributors",
        "doc_history_baselines",
        "doc_history_heads",
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
// Page history M1.2: the lock helper and the log columns
// ---------------------------------------------------------------------------

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
}

/// The deadlock M1.1's loop was written for, now through `knot_lock_tables`:
/// a page write T1 holds a page row, an edit save T2 holds ROW EXCLUSIVE on
/// `doc_updates` and waits for that row in its FK check, and T1 then writes
/// `documents`. A migrator that waited for `doc_updates` while holding
/// `documents` would close the cycle; the helper lets go and tries again.
#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_backs_off_instead_of_deadlocking() {
    use std::time::{Duration, Instant};

    let (pool, _, doc) = history_fixture().await;

    let mut t1 = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM documents WHERE id = $1 FOR UPDATE")
        .bind(doc)
        .execute(&mut *t1)
        .await
        .unwrap();

    let t2 = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await?;
            sqlx::query("INSERT INTO doc_updates (doc_id, update_bytes) VALUES ($1, '\\x01')")
                .bind(doc)
                .execute(&mut *tx)
                .await?;
            tx.commit().await
        }
    });
    // T2 waits for T1's page row, holding ROW EXCLUSIVE on doc_updates.
    knot_test_support::locks::wait_for_lock_waits(&pool, 1).await;

    let m = tokio::spawn({
        let pool = pool.clone();
        async move {
            let started = Instant::now();
            let mut tx = pool.begin().await?;
            sqlx::query(
                "SELECT knot_lock_tables('t', ARRAY['documents','doc_updates']::regclass[],
                                         'SHARE ROW EXCLUSIVE', '20 seconds')",
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(started.elapsed())
        }
    });
    // M has failed an attempt on doc_updates and let go of documents.
    tokio::time::sleep(Duration::from_millis(300)).await;
    sqlx::query("UPDATE documents SET title = 'x' WHERE id = $1")
        .bind(doc)
        .execute(&mut *t1)
        .await
        .unwrap();
    t1.commit().await.unwrap();

    let t2 = t2.await.unwrap();
    let m = m.await.unwrap();
    assert!(
        t2.is_ok() && m.is_ok(),
        "no 40P01 anywhere: t2 {t2:?}, m {m:?}"
    );
    let m_elapsed = m.unwrap();
    assert!(m_elapsed < Duration::from_secs(20), "{m_elapsed:?}");
    assert!(
        m_elapsed >= Duration::from_millis(250),
        "M got doc_updates only after T2 committed: {m_elapsed:?}"
    );
    assert_eq!(count_updates_any(&pool, &[doc]).await, 3);
}

/// After `give_up_after` the helper raises, naming the session that holds a
/// conflicting lock, and not one whose lock does not conflict.
#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_gives_up_naming_the_holder() {
    let (pool, _, _) = history_fixture().await;

    let mut h = pool.begin().await.unwrap();
    let h_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *h)
        .await
        .unwrap();
    sqlx::query("LOCK TABLE doc_updates IN ROW EXCLUSIVE MODE")
        .execute(&mut *h)
        .await
        .unwrap();
    // A reader: ACCESS SHARE does not conflict with SHARE.
    let mut r = pool.begin().await.unwrap();
    let r_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *r)
        .await
        .unwrap();
    sqlx::query("SELECT count(*) FROM doc_updates")
        .execute(&mut *r)
        .await
        .unwrap();

    let mut m = pool.begin().await.unwrap();
    let e = sqlx::query(
        "SELECT knot_lock_tables('t', ARRAY['doc_updates']::regclass[], 'SHARE', '1 second')",
    )
    .execute(&mut *m)
    .await
    .unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("t: could not get SHARE on doc_updates within 1 s"),
        "{msg}"
    );
    assert!(msg.contains(&format!("pid {h_pid}")), "{msg}");
    assert!(!msg.contains(&format!("pid {r_pid}")), "{msg}");
    assert!(msg.contains("The migrate Job will retry."), "{msg}");
    drop(m);
    r.rollback().await.unwrap();
    h.rollback().await.unwrap();
}

/// `lock_mode` is spliced into a LOCK statement, so only the three modes the
/// migrations use are accepted.
#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_refuses_an_unknown_mode() {
    let pool = knot_test_support::fresh_db().await.pool;
    for mode in [
        "ROW SHARE; DROP TABLE doc_updates",
        "ROW EXCLUSIVE",
        "share",
    ] {
        let e = sqlx::query("SELECT knot_lock_tables('t', ARRAY['doc_updates']::regclass[], $1)")
            .bind(mode)
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&e).as_deref(), Some("22023"), "{mode}: {e}");
        assert!(e.to_string().contains("lock mode"), "{mode}: {e}");
    }
    let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('doc_updates')::text")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(exists.as_deref(), Some("doc_updates"));
}

/// The lone wait (for an autovacuum worker's deadlock-check cancel) asks for
/// SHARE UPDATE EXCLUSIVE whatever the target mode: in ACCESS EXCLUSIVE it
/// would stop every page load for about 1.5 s. An autovacuum can't be staged
/// on demand, so this pins the function's source against a revert.
#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_lone_wait_never_requests_the_target_mode() {
    let pool = knot_test_support::fresh_db().await.pool;
    let def: String = sqlx::query_scalar("SELECT pg_get_functiondef('knot_lock_tables'::regproc)")
        .fetch_one(&pool)
        .await
        .unwrap();
    let lines: Vec<&str> = def.lines().collect();
    let locks: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.contains("LOCK TABLE"))
        .collect();
    assert_eq!(
        locks.len(),
        2,
        "one attempt LOCK and one lone-wait LOCK: {locks:#?}"
    );
    let khlw1 = lines
        .iter()
        .position(|l| l.contains("ERRCODE = 'KHLW1'"))
        .expect("the lone wait ends by raising KHLW1");
    let lone = lines[..khlw1]
        .iter()
        .rev()
        .find(|l| l.contains("LOCK TABLE"))
        .expect("the lone wait's LOCK");
    assert!(lone.contains("SHARE UPDATE EXCLUSIVE"), "{lone}");
    assert!(!lone.contains("lock_mode"), "{lone}");
}

/// Migration 1 only adds nullable columns and a NOT VALID check, so the
/// statements 0.9.9 pods keep running during the upgrade still work.
#[tokio::test(flavor = "multi_thread")]
async fn log_columns_exist_and_old_statements_still_work() {
    use chrono::{DateTime, Utc};

    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let doc = seed_doc_with_updates(&pool, ws, user, None, 0).await;

    let cols: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text, data_type::text, is_nullable::text
           FROM information_schema.columns
          WHERE table_schema = 'public'
            AND ((table_name = 'doc_updates' AND column_name IN ('rev', 'source', 'meta', 'epoch'))
              OR (table_name = 'doc_snapshots' AND column_name IN ('snapshot_rev', 'epoch')))
          ORDER BY table_name, column_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let cols: Vec<[&str; 4]> = cols
        .iter()
        .map(|(t, c, d, n)| [t.as_str(), c.as_str(), d.as_str(), n.as_str()])
        .collect();
    assert_eq!(
        cols,
        [
            ["doc_snapshots", "epoch", "integer", "YES"],
            ["doc_snapshots", "snapshot_rev", "bigint", "YES"],
            ["doc_updates", "epoch", "integer", "YES"],
            ["doc_updates", "meta", "jsonb", "YES"],
            ["doc_updates", "rev", "bigint", "YES"],
            ["doc_updates", "source", "text", "YES"],
        ]
    );
    let validated: bool = sqlx::query_scalar(
        "SELECT convalidated FROM pg_constraint WHERE conname = 'doc_updates_source_check'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        !validated,
        "added NOT VALID: no scan under ACCESS EXCLUSIVE"
    );

    // 0.9.9's edit save.
    let seqs: Vec<i64> = sqlx::query_scalar(OLD_INSERT_UPDATES_SQL)
        .bind(doc)
        .bind(vec![Some(user), None])
        .bind(vec![vec![1u8], vec![2u8]])
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(seqs.len(), 2);
    // 0.9.9's boot read.
    type OldRow = (i64, Uuid, Vec<u8>, Option<Uuid>, DateTime<Utc>);
    let rows: Vec<OldRow> = sqlx::query_as(
        "SELECT seq, doc_id, update_bytes, by_user_id, created_at
           FROM doc_updates WHERE doc_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(doc)
    .bind(0_i64)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.iter()
            .map(|(s, d, b, u, _)| (*s, *d, b.clone(), *u))
            .collect::<Vec<_>>(),
        [
            (seqs[0], doc, vec![1u8], Some(user)),
            (seqs[1], doc, vec![2u8], None),
        ]
    );
    let unlabelled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM doc_updates
          WHERE doc_id = $1 AND source IS NULL AND meta IS NULL AND epoch IS NULL",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unlabelled, 2);

    // 0.9.9's snapshot upsert, as an insert and on conflict.
    for bytes in [b"one".as_slice(), b"two".as_slice()] {
        sqlx::query(OLD_SNAPSHOT_UPSERT_SQL)
            .bind(doc)
            .bind(seqs[1])
            .bind(bytes)
            .bind(b"sv".as_slice())
            .execute(&pool)
            .await
            .unwrap();
    }
    let snap: (Vec<u8>, Option<i64>, Option<i32>) = sqlx::query_as(
        "SELECT state_bytes, snapshot_rev, epoch FROM doc_snapshots WHERE doc_id = $1",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(snap, (b"two".to_vec(), None, None));

    let label = |source: &'static str| {
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, source) VALUES ($1, '\\x03', $2)",
        )
        .bind(doc)
        .bind(source)
        .execute(&pool)
    };
    let e = label("bogus").await.unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("23514"), "{e}");
    label("edit").await.unwrap();
}

// ---------------------------------------------------------------------------
// Page history M1.2: the heads table, the rev trigger and the epoch triggers
// ---------------------------------------------------------------------------

/// 0.9.9's schema: M1.1's last migration.
const V0_9_9_SCHEMA: i64 = 20261104120001;
const HISTORY_LOG_COLUMNS: i64 = 20261107120000;
const HISTORY_HEADS: i64 = 20261107120001;

fn kh002(e: &sqlx::Error) -> bool {
    sqlstate(e).as_deref() == Some("KH002")
}

/// The embedded migrations up to and including `last`.
fn migrator_up_to(last: i64) -> sqlx::migrate::Migrator {
    let full = sqlx::migrate!("../../migrations");
    assert!(
        full.migrations.iter().any(|m| m.version == last),
        "migration {last} not embedded (cargo clean -p knot-storage?)"
    );
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version <= last)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
}

async fn migrate_up_to(pool: &PgPool, last: i64) {
    migrator_up_to(last)
        .run(pool)
        .await
        .unwrap_or_else(|e| panic!("migrate up to {last}: {e}"));
}

async fn migrate_all(pool: &PgPool) {
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .expect("migrate");
}

/// A fresh database with no migrations applied, and a pool on it.
async fn bare_db(max_connections: u32) -> (String, PgPool) {
    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(&url)
        .await
        .expect("connect");
    (url, pool)
}

async fn try_new_page<'e>(
    ex: impl sqlx::PgExecutor<'e>,
    ws: Uuid,
    user: Uuid,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', gen_random_uuid()::text, $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(ex)
    .await
}

/// A top-level page with a key of its own, so any number fit one workspace.
async fn new_page(pool: &PgPool, ws: Uuid, user: Uuid) -> Uuid {
    try_new_page(pool, ws, user).await.unwrap()
}

/// `n` rows inserted one by one with a raw INSERT (no rev, no epoch).
async fn raw_rows(pool: &PgPool, doc: Uuid, n: usize) {
    for _ in 0..n {
        sqlx::query("INSERT INTO doc_updates (doc_id, update_bytes) VALUES ($1, '\\x01')")
            .bind(doc)
            .execute(pool)
            .await
            .unwrap();
    }
}

/// 0.9.9's edit save of `n` rows by `user`; returns their seqs.
async fn old_insert<'e>(
    ex: impl sqlx::PgExecutor<'e>,
    doc: Uuid,
    user: Option<Uuid>,
    n: usize,
) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(OLD_INSERT_UPDATES_SQL)
        .bind(doc)
        .bind(vec![user; n])
        .bind(vec![vec![1u8]; n])
        .fetch_all(ex)
        .await
}

/// 0.9.9's snapshot write.
async fn old_snapshot<'e>(
    ex: impl sqlx::PgExecutor<'e>,
    doc: Uuid,
    seq: i64,
    bytes: &[u8],
) -> Result<(), sqlx::Error> {
    sqlx::query(OLD_SNAPSHOT_UPSERT_SQL)
        .bind(doc)
        .bind(seq)
        .bind(bytes)
        .bind(b"sv".as_slice())
        .execute(ex)
        .await
        .map(drop)
}

/// A stamped snapshot write: (doc, seq, bytes, rev, epoch).
async fn stamped_snapshot<'e>(
    ex: impl sqlx::PgExecutor<'e>,
    sql: &'static str,
    doc: Uuid,
    seq: i64,
    bytes: &[u8],
    rev: i64,
    epoch: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(sql)
        .bind(doc)
        .bind(seq)
        .bind(bytes)
        .bind(b"sv".as_slice())
        .bind(rev)
        .bind(epoch)
        .execute(ex)
        .await
        .map(drop)
}

/// (state_bytes, snapshot_rev, epoch) of one snapshot.
async fn snapshot_at(pool: &PgPool, doc: Uuid, seq: i64) -> (Vec<u8>, Option<i64>, Option<i32>) {
    sqlx::query_as(
        "SELECT state_bytes, snapshot_rev, epoch FROM doc_snapshots
          WHERE doc_id = $1 AND snapshot_seq = $2",
    )
    .bind(doc)
    .bind(seq)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// (head_rev, legacy_rows, content_epoch, dirty, last_write_at).
type HeadsRow = (i64, i64, i32, bool, Option<chrono::DateTime<chrono::Utc>>);

async fn heads_row(pool: &PgPool, doc: Uuid) -> Option<HeadsRow> {
    sqlx::query_as(
        "SELECT head_rev, legacy_rows, content_epoch, dirty, last_write_at
           FROM doc_history_heads WHERE doc_id = $1",
    )
    .bind(doc)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn rev_of(pool: &PgPool, seq: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT rev FROM doc_updates WHERE seq = $1")
        .bind(seq)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The page's legacy rows keep rev NULL, and its numbered rows are exactly
/// `legacy_rows + 1 ..= head_rev`, each once. Returns (legacy_rows, head_rev).
async fn assert_gapless(pool: &PgPool, doc: Uuid) -> (i64, i64) {
    let (head_rev, legacy_rows, ..) = heads_row(pool, doc).await.expect("a heads row");
    let unnumbered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE doc_id = $1 AND rev IS NULL")
            .bind(doc)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(unnumbered, legacy_rows, "{doc}: legacy rows keep rev NULL");
    let revs: Vec<i64> = sqlx::query_scalar(
        "SELECT rev FROM doc_updates WHERE doc_id = $1 AND rev IS NOT NULL ORDER BY rev",
    )
    .bind(doc)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        revs,
        (legacy_rows + 1..=head_rev).collect::<Vec<_>>(),
        "{doc}: revs after the legacy rows, gapless"
    );
    (legacy_rows, head_rev)
}

fn err_text(e: sqlx::Error) -> String {
    format!("{e} (SQLSTATE {:?})", sqlstate(&e))
}

/// What one background loop of the concurrency tests saw.
#[derive(Debug, Default)]
struct LoopReport {
    runs: u64,
    errors: Vec<String>,
}

struct Loop {
    handle: tokio::task::JoinHandle<LoopReport>,
    /// The longest single run so far, in microseconds.
    longest_us: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Loop {
    fn longest(&self) -> std::time::Duration {
        std::time::Duration::from_micros(self.longest_us.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// Runs `body(pool, n)` for n = 0, 1, ... until `stop`, pausing `pause`
/// after each run, recording every error and the longest run; closes `pool`
/// at the end (a backend flushes its statistics when it exits).
fn spawn_loop<F, Fut>(
    pool: PgPool,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pause: std::time::Duration,
    body: F,
) -> Loop
where
    F: Fn(PgPool, u64) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    use std::sync::atomic::Ordering::SeqCst;
    let longest_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let handle = tokio::spawn({
        let longest_us = longest_us.clone();
        async move {
            let mut report = LoopReport::default();
            while !stop.load(SeqCst) {
                let started = std::time::Instant::now();
                if let Err(e) = body(pool.clone(), report.runs).await {
                    report.errors.push(e);
                }
                longest_us.fetch_max(started.elapsed().as_micros() as u64, SeqCst);
                report.runs += 1;
                if !pause.is_zero() {
                    tokio::time::sleep(pause).await;
                }
            }
            pool.close().await;
            report
        }
    });
    Loop { handle, longest_us }
}

async fn loop_pool(url: &str) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("loop pool")
}

/// Rows a 0.9.9 pod inserts after the upgrade are numbered after the page's
/// legacy rows, by the trigger, in commit order.
#[tokio::test(flavor = "multi_thread")]
async fn old_style_inserts_are_numbered_after_the_legacy_rows() {
    use chrono::{DateTime, Utc};

    let (_url, pool) = bare_db(4).await;
    migrate_up_to(&pool, HISTORY_LOG_COLUMNS).await;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 3).await;
    migrate_all(&pool).await;

    let (head_rev, legacy_rows, epoch, dirty, last_write_at) = heads_row(&pool, d).await.unwrap();
    assert_eq!((head_rev, legacy_rows, epoch, dirty), (3, 3, 0, true));
    assert!(last_write_at.is_some(), "seeded rows are never NULL");

    // A builder cleared the row: the ON CONFLICT arm must set both again.
    sqlx::query(
        "UPDATE doc_history_heads SET dirty = false, last_write_at = '2000-01-01'
          WHERE doc_id = $1",
    )
    .bind(d)
    .execute(&pool)
    .await
    .unwrap();

    let s4 = old_insert(&pool, d, Some(user), 1).await.unwrap()[0];
    let s5 = old_insert(&pool, d, Some(user), 1).await.unwrap()[0];
    assert_eq!(rev_of(&pool, s4).await, Some(4));
    assert_eq!(rev_of(&pool, s5).await, Some(5));
    assert_gapless(&pool, d).await;
    let at = |seq: i64| {
        sqlx::query_scalar::<_, DateTime<Utc>>("SELECT created_at FROM doc_updates WHERE seq = $1")
            .bind(seq)
            .fetch_one(&pool)
    };
    assert!(at(s5).await.unwrap() >= at(s4).await.unwrap());
    let (head_rev, _, _, dirty, last_write_at) = heads_row(&pool, d).await.unwrap();
    assert_eq!((head_rev, dirty), (5, true));
    let cleared: DateTime<Utc> = "2000-01-01T00:00:00Z".parse().unwrap();
    assert!(last_write_at.unwrap() > cleared);

    // Tests and tools stamp rows explicitly: that timestamp stays.
    let explicit: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
    let s6: i64 = sqlx::query_scalar(
        "INSERT INTO doc_updates (doc_id, update_bytes, created_at)
         VALUES ($1, '\\x01', $2) RETURNING seq",
    )
    .bind(d)
    .bind(explicit)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(at(s6).await.unwrap(), explicit);
    assert_eq!(rev_of(&pool, s6).await, Some(6));

    // A page with no heads row yet: the trigger's INSERT arm starts at rev 1.
    let n = new_page(&pool, ws, user).await;
    assert_eq!(heads_row(&pool, n).await, None);
    let s = old_insert(&pool, n, None, 1).await.unwrap()[0];
    assert_eq!(rev_of(&pool, s).await, Some(1));
    let (head_rev, legacy_rows, epoch, dirty, last_write_at) = heads_row(&pool, n).await.unwrap();
    assert_eq!((head_rev, legacy_rows, epoch, dirty), (1, 0, 0, true));
    assert!(last_write_at.is_some());
}

/// Spike "migration ordering": 0.9.9 saves keep coming while the heads
/// migration runs, and every one of them is either counted as legacy or
/// numbered by the trigger, never both, never neither.
#[tokio::test(flavor = "multi_thread")]
async fn old_inserts_racing_the_heads_migration_are_counted_exactly() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::time::Duration;

    let (url, pool) = bare_db(4).await;
    migrate_up_to(&pool, HISTORY_LOG_COLUMNS).await;
    let (ws, user) = seed_ws_user(&pool).await;
    let a = new_page(&pool, ws, user).await;
    let b = new_page(&pool, ws, user).await;
    raw_rows(&pool, a, 5).await;
    raw_rows(&pool, b, 5).await;
    // A quiet page as the pre-M1.1 GC left it: a snapshot and no rows.
    let c = new_page(&pool, ws, user).await;
    old_snapshot(&pool, c, 7, b"c").await.unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let inserts = spawn_loop(
        loop_pool(&url).await,
        stop.clone(),
        Duration::ZERO,
        move |pool, n| async move {
            let doc = if n % 2 == 0 { a } else { b };
            old_insert(&pool, doc, Some(user), 1 + (n % 3) as usize)
                .await
                .map(drop)
                .map_err(err_text)
        },
    );
    let snapshots = spawn_loop(
        loop_pool(&url).await,
        stop.clone(),
        Duration::ZERO,
        move |pool, _| async move { old_snapshot(&pool, a, 1, b"a").await.map_err(err_text) },
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    migrate_up_to(&pool, HISTORY_HEADS).await;
    let longest = snapshots.longest();
    assert!(
        longest < Duration::from_secs(1),
        "the heads migration does not lock doc_snapshots: {longest:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    migrate_all(&pool).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.store(true, SeqCst);
    let inserts = inserts.handle.await.unwrap();
    let snapshots = snapshots.handle.await.unwrap();
    assert!(inserts.errors.is_empty(), "{:?}", inserts.errors);
    assert!(snapshots.errors.is_empty(), "{:?}", snapshots.errors);

    let mut legacy = 0;
    let mut numbered = 0;
    for doc in [a, b, c] {
        let (legacy_rows, head_rev) = assert_gapless(&pool, doc).await;
        legacy += legacy_rows;
        numbered += head_rev - legacy_rows;
    }
    // The loop saved on both sides of the migration.
    assert!(legacy > 10, "{legacy} legacy rows");
    assert!(numbered > 0, "{numbered} numbered rows");
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM doc_updates")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, legacy + numbered, "no row counted twice or missed");

    let (head_rev, legacy_rows, epoch, dirty, last_write_at) = heads_row(&pool, c).await.unwrap();
    assert_eq!(
        (head_rev, legacy_rows, epoch, dirty),
        (0, 0, 0, true),
        "every page has a heads row"
    );
    assert!(last_write_at.is_some());
    let unstamped: i64 =
        sqlx::query_scalar("SELECT count(*) FROM doc_history_heads WHERE last_write_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unstamped, 0);
    eprintln!(
        "{} old saves, {} old snapshot writes",
        inserts.runs, snapshots.runs
    );
}

/// Review round 5: the M1.1 deadlock class (page writes, purges, Duplicate,
/// readers) against the real migration files, under 0.9.9-shaped traffic.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn migrations_one_to_five_run_under_live_0_9_9_traffic() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::time::{Duration, Instant};

    use knot_storage::{DocStore, PgDocStore};
    use sqlx::Connection;

    let (url, pool) = bare_db(4).await;
    migrate_up_to(&pool, V0_9_9_SCHEMA).await;
    let (ws, user) = seed_ws_user(&pool).await;
    let mut pages = Vec::new();
    for _ in 0..4 {
        let p = new_page(&pool, ws, user).await;
        raw_rows(&pool, p, 5).await;
        pages.push(p);
    }
    let (a, b) = (pages[0], pages[1]);
    let c = new_page(&pool, ws, user).await;
    old_snapshot(&pool, c, 7, b"c").await.unwrap();
    let deadlocks0: i64 = sqlx::query_scalar(
        "SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let mut loops = Vec::new();
    // (i) One back-to-back inserter per page.
    for &p in &pages {
        loops.push((
            "insert",
            spawn_loop(
                loop_pool(&url).await,
                stop.clone(),
                Duration::ZERO,
                move |pool, n| async move {
                    old_insert(&pool, p, Some(user), 1 + (n % 3) as usize)
                        .await
                        .map(drop)
                        .map_err(err_text)
                },
            ),
        ));
    }
    // (ii) A page write on A.
    loops.push((
        "page write",
        spawn_loop(
            loop_pool(&url).await,
            stop.clone(),
            Duration::from_millis(20),
            move |pool, n| async move {
                let mut tx = pool.begin().await.map_err(err_text)?;
                sqlx::query("SELECT id FROM documents WHERE id = $1 FOR UPDATE")
                    .bind(a)
                    .execute(&mut *tx)
                    .await
                    .map_err(err_text)?;
                sqlx::query("UPDATE documents SET title = 'w' || $2::text WHERE id = $1")
                    .bind(a)
                    .bind(n as i64)
                    .execute(&mut *tx)
                    .await
                    .map_err(err_text)?;
                tx.commit().await.map_err(err_text)
            },
        ),
    ));
    // (iii) Archive and purge a throwaway page.
    loops.push((
        "purge",
        spawn_loop(
            loop_pool(&url).await,
            stop.clone(),
            Duration::from_millis(50),
            move |pool, _| async move {
                let t = try_new_page(&pool, ws, user).await.map_err(err_text)?;
                old_insert(&pool, t, Some(user), 1)
                    .await
                    .map_err(err_text)?;
                let store = PgDocStore::new(pool);
                store
                    .archive(ws, t, user)
                    .await
                    .map_err(|e| format!("archive: {e:?}"))?;
                store
                    .purge(ws, t, None)
                    .await
                    .map_err(|e| format!("purge: {e:?}"))?;
                Ok(())
            },
        ),
    ));
    // (iv) 0.9.9's Duplicate: a new page and its row, then placement locks
    // the source's page row.
    loops.push((
        "duplicate",
        spawn_loop(
            loop_pool(&url).await,
            stop.clone(),
            Duration::from_millis(50),
            move |pool, n| async move {
                let mut tx = pool.begin().await.map_err(err_text)?;
                let id: Uuid = sqlx::query_scalar(
                    "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
                     VALUES ($1, NULL, 'D', '~dup' || $3::text, $2) RETURNING id",
                )
                .bind(ws)
                .bind(user)
                .bind(n as i64)
                .fetch_one(&mut *tx)
                .await
                .map_err(err_text)?;
                old_insert(&mut *tx, id, Some(user), 1)
                    .await
                    .map_err(err_text)?;
                sqlx::query("SELECT id FROM documents WHERE id = $1 FOR UPDATE")
                    .bind(a)
                    .execute(&mut *tx)
                    .await
                    .map_err(err_text)?;
                sqlx::query("UPDATE documents SET sort_key = 'n' || $2::text WHERE id = $1")
                    .bind(id)
                    .bind(n as i64)
                    .execute(&mut *tx)
                    .await
                    .map_err(err_text)?;
                tx.commit().await.map_err(err_text)
            },
        ),
    ));
    // (v) 0.9.9's snapshot write on A.
    loops.push((
        "snapshot",
        spawn_loop(
            loop_pool(&url).await,
            stop.clone(),
            Duration::from_millis(50),
            move |pool, _| async move { old_snapshot(&pool, a, 1, b"a").await.map_err(err_text) },
        ),
    ));
    // (vi) A reader, one call per run: 0.9.9's statements. Not `SELECT *`,
    // which no 0.9.9 path runs on these tables: a statement prepared before
    // migration 1 adds columns fails with 0A000 ("cached plan must not
    // change result type") on its next execution.
    let reader = spawn_loop(
        loop_pool(&url).await,
        stop.clone(),
        Duration::ZERO,
        move |pool, n| async move {
            if n % 2 == 0 {
                sqlx::query("SELECT count(*) FROM doc_updates WHERE doc_id = $1")
                    .bind(b)
                    .execute(&pool)
                    .await
                    .map(drop)
                    .map_err(err_text)
            } else {
                // PgSnapshotStore::latest in 0.9.9.
                sqlx::query(
                    "SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at
                       FROM doc_snapshots WHERE doc_id = $1 ORDER BY snapshot_seq DESC LIMIT 1",
                )
                .bind(c)
                .execute(&pool)
                .await
                .map(drop)
                .map_err(err_text)
            }
        },
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    let started = Instant::now();
    let migrated = tokio::time::timeout(
        Duration::from_secs(30),
        sqlx::migrate!("../../migrations").run(&pool),
    )
    .await;
    let took = started.elapsed();
    tokio::time::sleep(Duration::from_secs(1)).await;
    stop.store(true, SeqCst);
    let mut reports = Vec::new();
    for (name, l) in loops {
        reports.push((name, l.handle.await.unwrap()));
    }
    let reader_longest = reader.longest();
    reports.push(("reader", reader.handle.await.unwrap()));

    match migrated {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("migrator failed after {took:?}: {e}"),
        Err(_) => panic!("migrator still running after 30 s"),
    }
    eprintln!("migrations 1-5 under load took {took:?}");
    for (name, report) in &reports {
        eprintln!("{name}: {} runs", report.runs);
        assert!(report.runs > 0, "{name} never ran");
        assert!(report.errors.is_empty(), "{name}: {:?}", report.errors);
    }
    assert!(
        reader_longest < Duration::from_secs(1),
        "reads are blocked only by migration 1's short attempts: {reader_longest:?}"
    );
    for &p in &pages {
        assert_gapless(&pool, p).await;
    }
    assert!(
        matches!(rev_index(&pool).await, Some((_, true, _))),
        "the concurrent build under load left a valid index"
    );

    // Every backend but this probe has exited, so each flushed its
    // statistics; a 40P01 anywhere (even one a retry hid) shows here.
    pool.close().await;
    let mut probe = sqlx::PgConnection::connect(&url).await.unwrap();
    let mut others: i64 = -1;
    for _ in 0..50 {
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(&mut probe)
            .await
            .unwrap();
        others = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE datname = current_database() AND backend_type = 'client backend'
                AND pid <> pg_backend_pid()",
        )
        .fetch_one(&mut probe)
        .await
        .unwrap();
        if others == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(others, 0, "every loop backend exited within 5 s");
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut probe)
        .await
        .unwrap();
    let deadlocks: i64 = sqlx::query_scalar(
        "SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()",
    )
    .fetch_one(&mut probe)
    .await
    .unwrap();
    assert_eq!(deadlocks, deadlocks0, "no deadlock anywhere");
}

/// Once an erase bumps `content_epoch`, a writer that saw the old epoch
/// (0.9.9 carries none) is refused with KH002, rows and snapshots alike.
#[tokio::test(flavor = "multi_thread")]
async fn writes_are_refused_once_the_epoch_moves() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 1).await;
    let other = new_page(&pool, ws, user).await;
    let bumped = sqlx::query("UPDATE doc_history_heads SET content_epoch = 1 WHERE doc_id = $1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(bumped.rows_affected(), 1);

    let e = old_insert(&pool, d, Some(user), 1).await.unwrap_err();
    assert!(kh002(&e), "{e}");
    let with_epoch = |epoch: i32| {
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, rev, epoch) VALUES ($1, '\\x01', 2, $2)",
        )
        .bind(d)
        .bind(epoch)
        .execute(&pool)
    };
    let e = with_epoch(0).await.unwrap_err();
    assert!(kh002(&e), "{e}");
    with_epoch(1).await.unwrap();

    let e = old_snapshot(&pool, d, 1, b"x").await.unwrap_err();
    assert!(kh002(&e), "a snapshot without an epoch: {e}");
    let e = stamped_snapshot(&pool, STAMPED_SNAPSHOT_INSERT_SQL, d, 2, b"x", 2, 0)
        .await
        .unwrap_err();
    assert!(kh002(&e), "{e}");
    stamped_snapshot(&pool, STAMPED_SNAPSHOT_INSERT_SQL, d, 2, b"x", 2, 1)
        .await
        .unwrap();

    // A page still at epoch 0 (no heads row yet counts as 0).
    old_snapshot(&pool, other, 1, b"o").await.unwrap();
}

/// A 0.9.9 upsert at a stamped snapshot's seq would keep its snapshot_rev
/// over other bytes; the trigger skips it instead (Rulings: stamped
/// snapshots are immutable).
#[tokio::test(flavor = "multi_thread")]
async fn an_old_snapshot_upsert_leaves_a_stamped_snapshot_alone() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 3).await;

    stamped_snapshot(&pool, STAMPED_SNAPSHOT_INSERT_SQL, d, 10, b"X", 3, 0)
        .await
        .unwrap();
    old_snapshot(&pool, d, 10, b"Y")
        .await
        .expect("0.9.9 sees no error");
    assert_eq!(
        snapshot_at(&pool, d, 10).await,
        (b"X".to_vec(), Some(3), Some(0))
    );
    stamped_snapshot(&pool, STAMPED_SNAPSHOT_UPSERT_SQL, d, 10, b"Z", 3, 0)
        .await
        .unwrap();
    assert_eq!(
        snapshot_at(&pool, d, 10).await,
        (b"X".to_vec(), Some(3), Some(0))
    );

    // A legacy snapshot still takes 0.9.9's upsert, and a stamped one
    // promotes it.
    old_snapshot(&pool, d, 20, b"L1").await.unwrap();
    old_snapshot(&pool, d, 20, b"L2").await.unwrap();
    assert_eq!(
        snapshot_at(&pool, d, 20).await,
        (b"L2".to_vec(), None, None)
    );
    stamped_snapshot(&pool, STAMPED_SNAPSHOT_UPSERT_SQL, d, 20, b"S", 3, 0)
        .await
        .unwrap();
    assert_eq!(
        snapshot_at(&pool, d, 20).await,
        (b"S".to_vec(), Some(3), Some(0))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_and_trigger_path_writes_to_a_purged_page_fail_with_23503() {
    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();

    let e = old_insert(&pool, d, Some(user), 1).await.unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("23503"), "{e}");
    let e = old_snapshot(&pool, d, 1, b"x").await.unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("23503"), "{e}");
    assert_eq!(
        heads_row(&pool, d).await,
        None,
        "no heads row for a gone page"
    );
}

/// The trigger path takes the page row before the heads row, so a purge and
/// a 0.9.9 save wait for each other instead of deadlocking, in either order.
#[tokio::test(flavor = "multi_thread")]
async fn a_purge_racing_a_trigger_path_insert_never_deadlocks() {
    use std::time::Duration;

    use knot_test_support::locks::wait_for_lock_waits;

    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let delete_page = |pool: PgPool, doc: Uuid| async move {
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM documents WHERE id = $1")
            .bind(doc)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    };

    // The save first: it holds the page row's KEY SHARE and the heads row.
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 1).await;
    let mut t = pool.begin().await.unwrap();
    old_insert(&mut *t, d, Some(user), 2).await.unwrap();
    let p = tokio::spawn(tokio::time::timeout(
        Duration::from_secs(5),
        delete_page(pool.clone(), d),
    ));
    wait_for_lock_waits(&pool, 1).await;
    t.commit().await.unwrap();
    let p = p.await.unwrap().expect("purge within 5 s");
    assert!(p.is_ok(), "the purge waited, then ran: {p:?}");
    assert_eq!(heads_row(&pool, d).await, None, "the heads row cascades");
    assert_eq!(count_updates_any(&pool, &[d]).await, 0);

    // The purge first: the save waits on the page row, then finds it gone.
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 1).await;
    let mut p = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(d)
        .execute(&mut *p)
        .await
        .unwrap();
    let t = tokio::spawn({
        let pool = pool.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(5), old_insert(&pool, d, Some(user), 1)).await
        }
    });
    wait_for_lock_waits(&pool, 1).await;
    p.commit().await.unwrap();
    let e = t.await.unwrap().expect("save within 5 s").unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("23503"), "{e}");
}

/// Spike "row-lock recheck": a snapshot write and a flush that queue behind
/// an erase's lock on the heads row see the epoch it committed.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_insert_waits_for_an_epoch_bump_then_fails() {
    use std::time::Duration;

    use knot_test_support::locks::wait_for_lock_waits;

    let pool = knot_test_support::fresh_db().await.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 1).await;

    let mut e = pool.begin().await.unwrap();
    sqlx::query("SELECT content_epoch FROM doc_history_heads WHERE doc_id = $1 FOR UPDATE")
        .bind(d)
        .execute(&mut *e)
        .await
        .unwrap();
    sqlx::query("UPDATE doc_history_heads SET content_epoch = 1 WHERE doc_id = $1")
        .bind(d)
        .execute(&mut *e)
        .await
        .unwrap();

    let s = tokio::spawn({
        let pool = pool.clone();
        async move {
            tokio::time::timeout(
                Duration::from_secs(5),
                stamped_snapshot(&pool, STAMPED_SNAPSHOT_INSERT_SQL, d, 1, b"x", 1, 0),
            )
            .await
        }
    });
    wait_for_lock_waits(&pool, 1).await;
    let w = tokio::spawn({
        let pool = pool.clone();
        async move {
            tokio::time::timeout(
                Duration::from_secs(5),
                sqlx::query_as::<_, (i64, i32)>(HEADS_UPSERT_SQL)
                    .bind(d)
                    .bind(1_i64)
                    .fetch_one(&pool),
            )
            .await
        }
    });
    wait_for_lock_waits(&pool, 2).await;
    e.commit().await.unwrap();

    let s = s
        .await
        .unwrap()
        .expect("snapshot write within 5 s")
        .unwrap_err();
    assert!(kh002(&s), "{s}");
    let (base, epoch) = w.await.unwrap().expect("flush within 5 s").unwrap();
    assert_eq!((base, epoch), (1, 1));
}

/// Numbering a legacy row (rev NULL to a value, nothing else changed) is
/// the one update the append-only trigger lets through.
#[tokio::test(flavor = "multi_thread")]
async fn append_only_allows_only_the_legacy_rev_numbering() {
    let (_url, pool) = bare_db(4).await;
    migrate_up_to(&pool, HISTORY_LOG_COLUMNS).await;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 2).await;
    migrate_all(&pool).await;
    let other: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('b@x.test', 'Bob') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let seqs: Vec<i64> =
        sqlx::query_scalar("SELECT seq FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
            .bind(d)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(seqs.len(), 2);

    // `$1` the row's seq, `$2` another user (unused by some statements).
    let update = |set: &'static str, seq: i64| {
        let sql = format!("UPDATE doc_updates SET {set} WHERE seq = $1");
        let pool = pool.clone();
        async move {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(seq)
                .bind(other)
                .execute(&pool)
                .await
        }
    };
    let numbered = update("rev = 1", seqs[0]).await.unwrap();
    // A trigger returning NULL would skip the row without an error.
    assert_eq!(numbered.rows_affected(), 1, "numbering a legacy row");
    assert_eq!(rev_of(&pool, seqs[0]).await, Some(1));
    let e = update("rev = 2", seqs[0]).await.unwrap_err();
    assert!(kh001(e), "a numbered row is never renumbered");
    assert_eq!(rev_of(&pool, seqs[0]).await, Some(1));

    for set in [
        "rev = 9, by_user_id = $2",
        "by_user_id = $2",
        "created_at = created_at - interval '1 day'",
        "source = 'edit'",
    ] {
        let e = update(set, seqs[1]).await.unwrap_err();
        assert_eq!(sqlstate(&e).as_deref(), Some("KH001"), "{set}: {e}");
    }
    assert_eq!(rev_of(&pool, seqs[1]).await, None);
}

/// Review round 3: the rewrite flag (an erase) skips the epoch check and the
/// immutability guard, never the page-row lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_flagged_rewrite_may_write_a_snapshot_at_any_epoch() {
    use sqlx::Connection;

    let db = knot_test_support::fresh_db().await;
    let pool = db.pool;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 1).await;
    stamped_snapshot(&pool, STAMPED_SNAPSHOT_INSERT_SQL, d, 10, b"X", 1, 0)
        .await
        .unwrap();

    // A session that never set the flag reads it as NULL (review round 4).
    let mut conn = sqlx::PgConnection::connect(&db.url).await.unwrap();
    let flag: Option<String> =
        sqlx::query_scalar("SELECT current_setting('knot.history_rewrite', true)")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(flag, None);
    let e = stamped_snapshot(&mut conn, STAMPED_SNAPSHOT_INSERT_SQL, d, 11, b"N", 1, 1)
        .await
        .unwrap_err();
    assert!(kh002(&e), "{e}");
    old_snapshot(&mut conn, d, 10, b"Y").await.unwrap();
    assert_eq!(
        snapshot_at(&pool, d, 10).await,
        (b"X".to_vec(), Some(1), Some(0))
    );

    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SET LOCAL knot.history_rewrite = 'on'")
        .execute(&mut *tx)
        .await
        .unwrap();
    stamped_snapshot(&mut *tx, STAMPED_SNAPSHOT_INSERT_SQL, d, 11, b"N", 1, 1)
        .await
        .unwrap();
    stamped_snapshot(&mut *tx, STAMPED_SNAPSHOT_UPSERT_SQL, d, 10, b"Z", 1, 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        snapshot_at(&pool, d, 11).await,
        (b"N".to_vec(), Some(1), Some(1))
    );
    assert_eq!(
        snapshot_at(&pool, d, 10).await,
        (b"Z".to_vec(), Some(1), Some(1))
    );

    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SET LOCAL knot.history_rewrite = 'on'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let e = stamped_snapshot(&mut *tx, STAMPED_SNAPSHOT_INSERT_SQL, d, 12, b"P", 1, 1)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("23503"), "{e}");
    assert!(
        e.to_string().contains("does not exist"),
        "the trigger's page check, before the FK: {e}"
    );
    tx.rollback().await.unwrap();
}

/// Rule (b) of `doc_updates_append_only` compares every column but `rev`
/// by name; a new column must be added there.
#[tokio::test(flavor = "multi_thread")]
async fn doc_updates_columns_are_the_ones_rule_b_compares() {
    let pool = knot_test_support::fresh_db().await.pool;
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
          WHERE table_schema = 'public' AND table_name = 'doc_updates'
          ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        cols,
        [
            "by_user_id",
            "created_at",
            "doc_id",
            "epoch",
            "meta",
            "rev",
            "seq",
            "source",
            "update_bytes"
        ],
        "a new doc_updates column must be added to rule (b) in doc_updates_append_only"
    );
}

/// Between the heads migration and its FK a purge leaves an orphan heads
/// row; the FK migration deletes it, and from then on heads rows cascade.
#[tokio::test(flavor = "multi_thread")]
async fn heads_fk_migration_drops_orphans_and_cascades() {
    let (_url, pool) = bare_db(4).await;
    migrate_up_to(&pool, HISTORY_HEADS).await;
    let (ws, user) = seed_ws_user(&pool).await;
    let d = new_page(&pool, ws, user).await;
    raw_rows(&pool, d, 2).await;
    assert!(heads_row(&pool, d).await.is_some());
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(d)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(count_updates_any(&pool, &[d]).await, 0);
    assert!(heads_row(&pool, d).await.is_some(), "no FK yet: an orphan");

    migrate_all(&pool).await;
    assert_eq!(heads_row(&pool, d).await, None, "the orphan is gone");
    let delete_rule: String = sqlx::query_scalar(
        "SELECT confdeltype::text FROM pg_constraint WHERE conname = 'doc_history_heads_doc_fk'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(delete_rule, "c", "ON DELETE CASCADE");

    let n = new_page(&pool, ws, user).await;
    raw_rows(&pool, n, 1).await;
    assert!(heads_row(&pool, n).await.is_some());
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(n)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(heads_row(&pool, n).await, None);
}

// --- Task 4: the (doc_id, rev) unique index (migrations 4 and 5) ---

const REV_INDEX_SQL: &str =
    include_str!("../../../migrations/20261107120003_history_rev_index.sql");
const REV_INDEX_CHECK_SQL: &str =
    include_str!("../../../migrations/20261107120004_history_rev_index_check.sql");

/// `doc_updates_doc_rev_uq`'s oid, `indisvalid` and definition, or `None`
/// when there is no such index on `doc_updates`.
async fn rev_index(pool: &PgPool) -> Option<(i64, bool, String)> {
    sqlx::query_as(
        "SELECT i.indexrelid::oid::int8, i.indisvalid, pg_get_indexdef(i.indexrelid)
           FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
          WHERE i.indrelid = 'doc_updates'::regclass AND c.relname = 'doc_updates_doc_rev_uq'",
    )
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// What a CIC that the deadlock detector aborted, or that was cancelled,
/// leaves behind: the index exists and is not valid.
async fn mark_rev_index_invalid(pool: &PgPool) {
    sqlx::query(
        "UPDATE pg_index SET indisvalid = false
          WHERE indexrelid = 'doc_updates_doc_rev_uq'::regclass",
    )
    .execute(pool)
    .await
    .unwrap();
    assert!(matches!(rev_index(pool).await, Some((_, false, _))));
}

/// Migration 5 as sqlx runs it: the whole file in one transaction.
async fn run_rev_index_check(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(REV_INDEX_CHECK_SQL).execute(&mut *tx).await?;
    tx.commit().await
}

const REV_INDEX_DEF: &str = "CREATE UNIQUE INDEX doc_updates_doc_rev_uq ON public.doc_updates \
                             USING btree (doc_id, rev) WHERE (rev IS NOT NULL)";

#[tokio::test(flavor = "multi_thread")]
async fn rev_index_is_built_valid_and_unique() {
    let (pool, ws, doc) = history_fixture().await;
    let (_, valid, def) = rev_index(&pool).await.expect("index exists");
    assert!(valid, "indisvalid");
    assert_eq!(def, REV_INDEX_DEF);

    // history_fixture's two rows went through the trigger: revs 1 and 2.
    let explicit = "INSERT INTO doc_updates (doc_id, update_bytes, rev, epoch)
                    VALUES ($1, '\\x01', $2, 0)";
    let e = sqlx::query(explicit)
        .bind(doc)
        .bind(1_i64)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&e).as_deref(), Some("23505"), "{e}");
    // The epoch was right: the next rev goes in.
    sqlx::query(explicit)
        .bind(doc)
        .bind(3_i64)
        .execute(&pool)
        .await
        .unwrap();
    // Unique per page: another page has its own rev 1.
    let user: Uuid = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    let child = seed_doc_with_updates(&pool, ws, user, Some(doc), 1).await;
    assert_eq!(
        sqlx::query_scalar::<_, Option<i64>>("SELECT rev FROM doc_updates WHERE doc_id = $1")
            .bind(child)
            .fetch_one(&pool)
            .await
            .unwrap(),
        Some(1)
    );
}

#[test]
fn no_transaction_migration_is_one_statement() {
    let sql = REV_INDEX_SQL;
    assert!(sql.starts_with("-- no-transaction"));
    assert_eq!(
        sql.matches(';').count(),
        1,
        "sqlx sends the file as one query"
    );
    assert!(
        sql.contains(
            "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS doc_updates_doc_rev_uq \
             ON doc_updates (doc_id, rev) WHERE rev IS NOT NULL;"
        ),
        "{sql}"
    );
    for line in sql.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with("--") || line.starts_with("CREATE UNIQUE INDEX CONCURRENTLY"),
            "only comment lines besides the statement: {line}"
        );
    }
}

/// An aborted CIC leaves an invalid index that the next run's `IF NOT EXISTS`
/// skips; a missing one (dropped by hand) is no better. Migration 5 rebuilds
/// both, and leaves a valid index alone.
#[tokio::test(flavor = "multi_thread")]
async fn index_check_rebuilds_an_invalid_index() {
    let (pool, _, _) = history_fixture().await;
    let (oid0, _, _) = rev_index(&pool).await.unwrap();

    run_rev_index_check(&pool).await.unwrap();
    assert_eq!(
        rev_index(&pool).await.map(|(oid, ..)| oid),
        Some(oid0),
        "a valid index is not rebuilt"
    );

    mark_rev_index_invalid(&pool).await;
    run_rev_index_check(&pool).await.unwrap();
    let (oid1, valid, def) = rev_index(&pool).await.expect("index exists");
    assert!(valid, "rebuilt valid");
    assert_ne!(oid1, oid0, "dropped and created again");
    assert_eq!(def, REV_INDEX_DEF);

    sqlx::query("DROP INDEX doc_updates_doc_rev_uq")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(rev_index(&pool).await, None);
    run_rev_index_check(&pool).await.unwrap();
    let (_, valid, def) = rev_index(&pool).await.expect("index exists again");
    assert!(valid);
    assert_eq!(def, REV_INDEX_DEF);
}

/// The repair takes ACCESS EXCLUSIVE through `knot_lock_tables`. A plain
/// LOCK would queue, and a page write T1 that holds a page row and then reads
/// `doc_updates` would queue behind it, while an old-style save T2 holds ROW
/// EXCLUSIVE on `doc_updates` and waits for T1's page row in the trigger:
/// a three-way cycle. (Two sessions cannot show it: a reader that already
/// holds ACCESS SHARE jumps the queued request.) Its T1 -> M edge is only
/// queue order, so Postgres breaks it by moving T1 ahead of M, but only when
/// a waiter's deadlock check runs, `deadlock_timeout` after it began to
/// wait: every page load behind a plain LOCK stalls that long. So the read
/// starts after T2's and M's own checks have run, and must not wait for its
/// own; with the helper it waits at most one 100 ms attempt.
#[tokio::test(flavor = "multi_thread")]
async fn index_check_backs_off_instead_of_deadlocking() {
    use std::time::{Duration, Instant};

    let (pool, _, doc) = history_fixture().await;
    mark_rev_index_invalid(&pool).await;
    let ten_s = Duration::from_secs(10);

    let mut t1 = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM documents WHERE id = $1 FOR UPDATE")
        .bind(doc)
        .execute(&mut *t1)
        .await
        .unwrap();

    let t2 = tokio::spawn({
        let pool = pool.clone();
        async move {
            let started = Instant::now();
            let mut tx = pool.begin().await?;
            sqlx::query("INSERT INTO doc_updates (doc_id, update_bytes) VALUES ($1, '\\x01')")
                .bind(doc)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(started.elapsed())
        }
    });
    // T2 holds ROW EXCLUSIVE on doc_updates and waits for T1's page row in
    // doc_updates_assign_rev's FOR KEY SHARE.
    knot_test_support::locks::wait_for_lock_waits(&pool, 1).await;

    let m = tokio::spawn({
        let pool = pool.clone();
        async move {
            let started = Instant::now();
            run_rev_index_check(&pool).await?;
            Ok::<_, sqlx::Error>(started.elapsed())
        }
    });
    // M's 100 ms attempts keep failing on T2's lock. Past deadlock_timeout,
    // T2 and M have each run their one deadlock check.
    let deadlock_timeout: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM current_setting('deadlock_timeout')::interval)::float8",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let deadlock_timeout = Duration::from_secs_f64(deadlock_timeout);
    tokio::time::sleep(deadlock_timeout + Duration::from_millis(200)).await;

    let read_started = Instant::now();
    let n: i64 = tokio::time::timeout(
        ten_s,
        sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE doc_id = $1")
            .bind(doc)
            .fetch_one(&mut *t1),
    )
    .await
    .expect("T1's read never queues behind M for long")
    .expect("T1's read");
    let read_took = read_started.elapsed();
    assert_eq!(n, 2);
    t1.commit().await.unwrap();

    let t2 = tokio::time::timeout(ten_s, t2)
        .await
        .expect("T2 finishes")
        .unwrap();
    let m = tokio::time::timeout(ten_s, m)
        .await
        .expect("M finishes")
        .unwrap();
    assert!(
        t2.is_ok() && m.is_ok(),
        "no 40P01 anywhere: t2 {t2:?}, m {m:?}"
    );
    let (t2_took, m_took) = (t2.unwrap(), m.unwrap());
    assert!(
        read_took < deadlock_timeout / 2,
        "T1's read queued behind M: {read_took:?}"
    );
    assert!(
        t2_took < ten_s && m_took < ten_s,
        "t2 {t2_took:?}, m {m_took:?}"
    );
    assert!(
        m_took >= deadlock_timeout,
        "M got doc_updates only after T2 committed: {m_took:?}"
    );
    assert!(matches!(rev_index(&pool).await, Some((_, true, _))));
    assert_eq!(count_updates_any(&pool, &[doc]).await, 3);
}

/// With a valid index the check reads the catalog and nothing else, so a
/// session writing `doc_updates` does not hold it up.
#[tokio::test(flavor = "multi_thread")]
async fn index_check_takes_no_lock_when_the_index_is_valid() {
    use std::time::Duration;

    let (pool, _, _) = history_fixture().await;
    let before = rev_index(&pool).await;

    let mut h = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE doc_updates IN ROW EXCLUSIVE MODE")
        .execute(&mut *h)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(1), run_rev_index_check(&pool))
        .await
        .expect("the check commits within 1 s")
        .unwrap();
    h.rollback().await.unwrap();
    assert_eq!(rev_index(&pool).await, before, "untouched");
}
