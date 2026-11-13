//! The page audit log's storage: `record_for_targets` (which hands a failed
//! insert back, for the export routes to fail closed on) and the per-page
//! reads (`list_doc_events`, `doc_stats`) over `audit_events`, the index
//! they run on, and `bots_among`, which marks the bots the log names.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use knot_storage::audit::{self, DocAuditStats};
use serde_json::json;
use sqlx::types::JsonValue;
use sqlx::{PgPool, Row};
use uuid::Uuid;

struct Fixture {
    pool: PgPool,
    workspace_id: Uuid,
    other_workspace_id: Uuid,
}

async fn setup() -> Fixture {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut workspace_ids = Vec::new();
    for slug in ["default", "other"] {
        let workspace_id: Uuid =
            sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ($1, $1) RETURNING id")
                .bind(slug)
                .fetch_one(&pool)
                .await
                .unwrap();
        workspace_ids.push(workspace_id);
    }
    Fixture {
        pool,
        workspace_id: workspace_ids[0],
        other_workspace_id: workspace_ids[1],
    }
}

/// One audit row to insert as given, `created_at` `age_days` ago.
struct StoredEvent<'text> {
    workspace_id: Uuid,
    actor_id: Option<Uuid>,
    action: &'text str,
    target_kind: &'text str,
    target_id: Uuid,
    data: JsonValue,
    age_days: i32,
}

/// Inserts `event`; returns its id.
async fn insert_event(pool: &PgPool, event: StoredEvent<'_>) -> i64 {
    let StoredEvent {
        workspace_id,
        actor_id,
        action,
        target_kind,
        target_id,
        data,
        age_days,
    } = event;
    sqlx::query_scalar(
        "INSERT INTO audit_events (workspace_id, actor_id, action, target_kind, target_id, data, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() - make_interval(days => $7))
         RETURNING id",
    )
    .bind(workspace_id)
    .bind(actor_id)
    .bind(action)
    .bind(target_kind)
    .bind(target_id)
    .bind(data)
    .bind(age_days)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn doc_event(f: &Fixture, doc_id: Uuid, action: &str, actor_id: Option<Uuid>) -> i64 {
    insert_event(
        &f.pool,
        StoredEvent {
            workspace_id: f.workspace_id,
            actor_id,
            action,
            target_kind: "doc",
            target_id: doc_id,
            data: json!({}),
            age_days: 0,
        },
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn list_doc_events_is_newest_first_and_only_this_page() {
    let f = setup().await;
    let (doc_id, other_doc_id, actor_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let created = doc_event(&f, doc_id, "doc.create", Some(actor_id)).await;
    // Another page, the same id in another workspace, and a non-doc target
    // with the same uuid: none is this page's.
    doc_event(&f, other_doc_id, "doc.rename", Some(actor_id)).await;
    insert_event(
        &f.pool,
        StoredEvent {
            workspace_id: f.other_workspace_id,
            actor_id: Some(actor_id),
            action: "doc.rename",
            target_kind: "doc",
            target_id: doc_id,
            data: json!({}),
            age_days: 0,
        },
    )
    .await;
    insert_event(
        &f.pool,
        StoredEvent {
            workspace_id: f.workspace_id,
            actor_id: Some(actor_id),
            action: "quota.exceeded",
            target_kind: "user",
            target_id: doc_id,
            data: json!({}),
            age_days: 0,
        },
    )
    .await;
    let renamed = doc_event(&f, doc_id, "doc.rename", Some(actor_id)).await;
    let exported = insert_event(
        &f.pool,
        StoredEvent {
            workspace_id: f.workspace_id,
            actor_id: None,
            action: "doc.export",
            target_kind: "doc",
            target_id: doc_id,
            data: json!({"format": "docx"}),
            age_days: 0,
        },
    )
    .await;

    let rows = audit::list_doc_events(&f.pool, f.workspace_id, doc_id, None, None, 50)
        .await
        .unwrap();
    let listed: Vec<(i64, &str)> = rows.iter().map(|r| (r.id, r.action.as_str())).collect();
    assert_eq!(
        listed,
        vec![
            (exported, "doc.export"),
            (renamed, "doc.rename"),
            (created, "doc.create")
        ]
    );
    assert_eq!(rows[0].actor_id, None);
    assert_eq!(rows[0].data, json!({"format": "docx"}));
    assert_eq!(rows[1].actor_id, Some(actor_id));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_doc_events_pages_below_the_cursor_and_filters_by_action() {
    let f = setup().await;
    let doc_id = Uuid::new_v4();
    let mut ids = Vec::new();
    for action in [
        "doc.create",
        "doc.export",
        "doc.grant",
        "doc.export",
        "doc.rename",
    ] {
        ids.push(doc_event(&f, doc_id, action, None).await);
    }

    let first = audit::list_doc_events(&f.pool, f.workspace_id, doc_id, None, None, 2)
        .await
        .unwrap();
    assert_eq!(
        first.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[4], ids[3]]
    );
    let below = audit::list_doc_events(&f.pool, f.workspace_id, doc_id, None, Some(ids[3]), 10)
        .await
        .unwrap();
    assert_eq!(
        below.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[2], ids[1], ids[0]]
    );

    let exports = audit::list_doc_events(
        &f.pool,
        f.workspace_id,
        doc_id,
        Some(&["doc.export"]),
        None,
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        exports.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[3], ids[1]]
    );
    let older_exports = audit::list_doc_events(
        &f.pool,
        f.workspace_id,
        doc_id,
        Some(&["doc.export", "doc.create"]),
        Some(ids[3]),
        10,
    )
    .await
    .unwrap();
    assert_eq!(
        older_exports.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![ids[1], ids[0]]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn record_for_targets_writes_one_row_per_page_and_nothing_for_none() {
    let f = setup().await;
    let (first_doc, second_doc, actor_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let data = json!({"format": "markdown_zip", "scope": "workspace"});

    audit::record_for_targets(
        &f.pool,
        f.workspace_id,
        Some(actor_id),
        "doc.export",
        "doc",
        &[],
        &data,
    )
    .await
    .expect("an empty slice is a no-op");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "an empty slice writes nothing");

    audit::record_for_targets(
        &f.pool,
        f.workspace_id,
        Some(actor_id),
        "doc.export",
        "doc",
        &[first_doc, second_doc],
        &data,
    )
    .await
    .expect("the export rows are written");
    let mut rows: Vec<(Uuid, Option<Uuid>, String, String, Uuid, JsonValue)> = sqlx::query_as(
        "SELECT workspace_id, actor_id, action, target_kind, target_id, data FROM audit_events",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    rows.sort_by_key(|row| row.4);
    let mut want = vec![
        (
            f.workspace_id,
            Some(actor_id),
            "doc.export".to_string(),
            "doc".to_string(),
            first_doc,
            data.clone(),
        ),
        (
            f.workspace_id,
            Some(actor_id),
            "doc.export".to_string(),
            "doc".to_string(),
            second_doc,
            data.clone(),
        ),
    ];
    want.sort_by_key(|row| row.4);
    assert_eq!(rows, want);
}

/// Counts `knot_audit_write_failures_total` by `action`, as a thread-local
/// recorder (so the test runs on a current-thread runtime).
#[derive(Default)]
struct AuditFailureCounter {
    by_action: Mutex<HashMap<String, Arc<AtomicU64>>>,
}

impl AuditFailureCounter {
    fn failures(&self, action: &str) -> u64 {
        self.by_action
            .lock()
            .unwrap()
            .get(action)
            .map_or(0, |count| count.load(Ordering::Relaxed))
    }
}

impl metrics::Recorder for AuditFailureCounter {
    fn describe_counter(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_gauge(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn describe_histogram(
        &self,
        _: metrics::KeyName,
        _: Option<metrics::Unit>,
        _: metrics::SharedString,
    ) {
    }
    fn register_counter(&self, key: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Counter {
        if key.name() != "knot_audit_write_failures_total" {
            return metrics::Counter::noop();
        }
        let action = key
            .labels()
            .find(|label| label.key() == "action")
            .map(|label| label.value().to_owned())
            .unwrap_or_default();
        let count = Arc::clone(self.by_action.lock().unwrap().entry(action).or_default());
        metrics::Counter::from_arc(count)
    }
    fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
        metrics::Gauge::noop()
    }
    fn register_histogram(
        &self,
        _: &metrics::Key,
        _: &metrics::Metadata<'_>,
    ) -> metrics::Histogram {
        metrics::Histogram::noop()
    }
}

/// Makes every `doc.export` insert into `audit_events` fail, as an audit
/// database that can't take the write would.
async fn refuse_export_rows(pool: &PgPool) {
    sqlx::query(
        "CREATE FUNCTION refuse_export_rows() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.action = 'doc.export' THEN
             RAISE EXCEPTION 'doc.export rows refused by the test';
           END IF;
           RETURN NEW;
         END $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER refuse_export_rows BEFORE INSERT ON audit_events
         FOR EACH ROW EXECUTE FUNCTION refuse_export_rows()",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// The export routes refuse a file they couldn't record, so the writer
/// hands them the error, and still logs and counts it for operators.
#[tokio::test]
async fn record_for_targets_returns_a_failed_insert_and_counts_it() {
    let f = setup().await;
    refuse_export_rows(&f.pool).await;
    let failure_counter = AuditFailureCounter::default();
    let _recorder_guard = metrics::set_default_local_recorder(&failure_counter);

    let outcome = audit::record_for_targets(
        &f.pool,
        f.workspace_id,
        Some(Uuid::new_v4()),
        "doc.export",
        "doc",
        &[Uuid::new_v4(), Uuid::new_v4()],
        &json!({"format": "docx", "scope": "page"}),
    )
    .await;

    assert!(outcome.is_err(), "{outcome:?}");
    assert_eq!(failure_counter.failures("doc.export"), 1);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "one INSERT: no page of the export is half recorded"
    );
}

/// An insert that hangs (here on a lock) instead of failing is given up on
/// after `EXPORT_AUDIT_WRITE_TIMEOUT`, well inside the request timeout, and
/// counts as a failed write like any other; it never lands later either.
#[tokio::test]
async fn record_for_targets_gives_up_on_a_stalled_insert_and_counts_it() {
    let f = setup().await;
    let failure_counter = AuditFailureCounter::default();
    let _recorder_guard = metrics::set_default_local_recorder(&failure_counter);
    // SHARE blocks every INSERT into audit_events until the rollback.
    let mut lock_holder = f.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE audit_events IN SHARE MODE")
        .execute(&mut *lock_holder)
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let outcome = audit::record_for_targets(
        &f.pool,
        f.workspace_id,
        Some(Uuid::new_v4()),
        "doc.export",
        "doc",
        &[Uuid::new_v4(), Uuid::new_v4()],
        &json!({"format": "docx", "scope": "page"}),
    )
    .await;
    let waited = started.elapsed();

    assert!(outcome.is_err(), "{outcome:?}");
    assert!(
        waited < audit::EXPORT_AUDIT_WRITE_TIMEOUT + std::time::Duration::from_secs(3),
        "gave up only after {waited:?}"
    );
    assert_eq!(failure_counter.failures("doc.export"), 1);
    lock_holder.rollback().await.unwrap();
    // Long enough for an insert still queued on the server to commit.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "the abandoned insert committed after all");
}

#[tokio::test(flavor = "multi_thread")]
async fn doc_stats_counts_actions_formats_scopes_and_exporters() {
    let f = setup().await;
    let (doc_id, ada, bo) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let export = |format: &str, scope: &str| json!({"format": format, "scope": scope});
    for (actor_id, data, age_days) in [
        (Some(ada), export("docx", "page"), 40),
        (Some(bo), export("markdown_zip", "subtree"), 10),
        (Some(ada), export("docx", "page"), 2),
        (None, export("odt", "page"), 1),
        (Some(bo), json!({}), 1),
    ] {
        insert_event(
            &f.pool,
            StoredEvent {
                workspace_id: f.workspace_id,
                actor_id,
                action: "doc.export",
                target_kind: "doc",
                target_id: doc_id,
                data,
                age_days,
            },
        )
        .await;
    }
    doc_event(&f, doc_id, "doc.create", Some(ada)).await;
    doc_event(&f, doc_id, "doc.rename", Some(ada)).await;
    doc_event(&f, doc_id, "doc.rename", Some(bo)).await;
    // Another page's export is not this page's.
    doc_event(&f, Uuid::new_v4(), "doc.export", Some(ada)).await;
    let latest_export = insert_event(
        &f.pool,
        StoredEvent {
            workspace_id: f.workspace_id,
            actor_id: Some(bo),
            action: "doc.export",
            target_kind: "doc",
            target_id: doc_id,
            data: export("odt", "workspace"),
            age_days: 0,
        },
    )
    .await;

    let DocAuditStats {
        by_action,
        export_groups,
        exporters,
        last_export,
    } = audit::doc_stats(&f.pool, f.workspace_id, doc_id)
        .await
        .unwrap();

    let actions: Vec<(&str, i64)> = by_action
        .iter()
        .map(|a| (a.action.as_str(), a.count))
        .collect();
    assert_eq!(
        actions,
        vec![("doc.export", 6), ("doc.rename", 2), ("doc.create", 1)]
    );

    let mut groups: Vec<(Option<&str>, Option<&str>, i64, i64)> = export_groups
        .iter()
        .map(|g| {
            (
                g.format.as_deref(),
                g.scope.as_deref(),
                g.count,
                g.last_30_days,
            )
        })
        .collect();
    groups.sort();
    assert_eq!(
        groups,
        vec![
            (None, None, 1, 1),
            (Some("docx"), Some("page"), 2, 1),
            (Some("markdown_zip"), Some("subtree"), 1, 1),
            (Some("odt"), Some("page"), 1, 1),
            (Some("odt"), Some("workspace"), 1, 1),
        ]
    );

    // The NULL actor is not an exporter; newest exporter first.
    let people: Vec<(Uuid, i64)> = exporters.iter().map(|e| (e.actor_id, e.count)).collect();
    assert_eq!(people, vec![(bo, 3), (ada, 2)]);
    assert!(exporters[0].last_at >= exporters[1].last_at);

    let last_export = last_export.expect("a last export");
    assert_eq!(last_export.id, latest_export);
    assert_eq!(last_export.actor_id, Some(bo));
    assert_eq!(last_export.data, export("odt", "workspace"));

    let empty = audit::doc_stats(&f.pool, f.workspace_id, Uuid::new_v4())
        .await
        .unwrap();
    assert!(empty.by_action.is_empty());
    assert!(empty.export_groups.is_empty());
    assert!(empty.exporters.is_empty());
    assert!(empty.last_export.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_target_index_exists() {
    let f = setup().await;
    let index_definitions: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname::text, indexdef FROM pg_indexes
          WHERE tablename = 'audit_events' AND indexname = 'audit_events_target_idx'",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(index_definitions.len(), 1, "{index_definitions:?}");
    let (_, index_definition) = &index_definitions[0];
    assert!(
        index_definition.contains("(target_id, id)"),
        "{index_definition}"
    );
}

/// Postgres switches a prepared statement to its generic plan after five
/// runs, so the cursor bound must be an index condition there too, not a
/// filter applied to every newer row of the page.
#[tokio::test(flavor = "multi_thread")]
async fn the_cursor_bound_is_an_index_condition_in_the_generic_plan() {
    let f = setup().await;
    // Many pages with a few events each, as in a real workspace.
    let doc_ids: Vec<Uuid> = (0..400).map(|_| Uuid::new_v4()).collect();
    sqlx::query(
        "INSERT INTO audit_events (workspace_id, action, target_kind, target_id, data)
         SELECT $1, 'doc.export', 'doc', ($2::uuid[])[1 + series.n % 400], '{}'
           FROM generate_series(1, 4000) AS series(n)",
    )
    .bind(f.workspace_id)
    .bind(&doc_ids)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query("ANALYZE audit_events")
        .execute(&f.pool)
        .await
        .unwrap();

    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let explain_sql = format!("EXPLAIN (GENERIC_PLAN) {}", audit::list_doc_events_sql());
    // The simple protocol: the extended one would want values for `$1..$5`.
    let plan_lines: Vec<String> = sqlx::raw_sql(sqlx::AssertSqlSafe(explain_sql))
        .fetch_all(&mut *tx)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>(0))
        .collect();
    let plan = plan_lines.join("\n");
    let index_condition = plan_lines
        .iter()
        .find(|line| line.contains("Index Cond"))
        .unwrap_or_else(|| panic!("no index condition in\n{plan}"));
    assert!(
        index_condition.contains("target_id = $2") && index_condition.contains("id <"),
        "the cursor bound is not part of the index condition:\n{plan}"
    );
}

/// Catches a person counted as a bot, a bot missed, and an unknown id
/// failing the read.
#[tokio::test(flavor = "multi_thread")]
async fn bots_among_finds_only_the_bots() {
    let f = setup().await;
    let mut user_ids = Vec::new();
    for (email, kind) in [("ada@audit.test", "human"), ("ci@audit.test", "bot")] {
        let user_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email, display_name, kind) VALUES ($1, $1, $2) RETURNING id",
        )
        .bind(email)
        .bind(kind)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        user_ids.push(user_id);
    }
    let (person, bot, unknown) = (user_ids[0], user_ids[1], Uuid::new_v4());
    let bots = audit::bots_among(&f.pool, &[person, bot, unknown])
        .await
        .unwrap();
    assert_eq!(bots, std::collections::HashSet::from([bot]));
    assert!(audit::bots_among(&f.pool, &[]).await.unwrap().is_empty());
}
