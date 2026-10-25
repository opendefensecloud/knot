//! The real-data compatibility check (spec "Compatibility guarantees", 2):
//! what upgrading a database to this release changes, found without
//! changing anything.
//!
//! [`compat_report`] reads every live page from storage, the way the
//! refresh sweep loads it (P18: no room is booted), and compares:
//! - its Markdown export by the legacy and by the new serializer, classified
//!   with `knot_markdown::legacy::compat`. A healthy page (its legacy export
//!   is a fixed point) outside the named export classes must export byte
//!   for byte as before;
//! - its `/tasks` listing by legacy (the legacy export, then
//!   `extract_tasks`) and by the new Y-walk (`extract_tasks_from_doc`).
//!   Where the legacy listing agrees with the room, a task outside the
//!   named task classes must be listed as before. The rule is the task
//!   differential of `knot_markdown::legacy::compat`, the one
//!   `knot-markdown/tests/tasks_compat.rs` proves over the corpus.
//!
//! It also reports the sort-key groups the heal migration would rewrite
//! (P7), and how many pages the refresh sweep would render again (P17).
//!
//! `real_data_compat` runs it before a deploy, against a restored backup or
//! a staging copy, from before the upgrade or after it:
//!
//! ```text
//! KNOT_COMPAT_DATABASE_URL=postgres://… \
//!   cargo test -p knot-server --test markdown_compat -- --ignored --nocapture
//! ```
//!
//! It connects read-only and runs no migrations. It prints the report and
//! fails if a healthy page or task outside the named classes would change,
//! or if a page can't be read, so wasn't compared.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::str::FromStr;

use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::RENDERER_VERSION;
use knot_markdown::legacy;
use knot_markdown::legacy::compat::{self, TaskVerdict};
use knot_storage::{MarkdownCacheStore, PgMarkdownCache};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

// Outcomes of a page's export, besides the export classes.

/// It exports, or lists its tasks, as before.
const UNCHANGED: &str = "unchanged";
/// Its legacy export fails or isn't a fixed point, so it may change
/// (serializer rule 2).
const NOT_HEALTHY: &str = "not_healthy";
/// A healthy page outside the export classes exports otherwise. Fails the
/// check.
const HEALTHY_CHANGED: &str = "healthy_changed";
/// Its stored state doesn't load: it doesn't decode, or the database can't
/// be read (a missing privilege or column). The page wasn't compared, so
/// this fails the check, as it fails `knot-server export-check`.
const LOAD_FAILED: &str = "load_failed";
/// A writer, or the check itself, panicked on the page. Fails the check.
const PANICKED: &str = "panicked";

// Outcomes of a page's task listing, besides `UNCHANGED`, the task classes,
// `LOAD_FAILED` and `PANICKED`.

/// It has no task item, and legacy listed no task.
const NO_TASKS: &str = "no_tasks";
/// Legacy couldn't export it, so it listed none of its tasks.
const NO_LEGACY_LISTING: &str = "no_legacy_listing";
/// The legacy listing doesn't agree with the room: another number of tasks,
/// or a toggle that flips another task. It was wrong, so it may change.
const LEGACY_DISAGREES: &str = "legacy_disagrees_with_room";
/// Where the legacy listing agrees with the room, a task outside the task
/// classes is listed otherwise. Fails the check.
const LISTED_DIFFERENTLY: &str = "listed_differently";

/// The outcomes that fail the check.
const FAILING: &[&str] = &[HEALTHY_CHANGED, LISTED_DIFFERENTLY, LOAD_FAILED, PANICKED];

/// How many failing pages, sort-key groups, and lines of a diff's side, the
/// report shows.
const SHOWN_PAGES: usize = 20;
const SHOWN_GROUPS: usize = 20;
const SHOWN_LINES: usize = 20;
/// Lines of context around a diff.
const CONTEXT: usize = 2;

/// What upgrading a database to this release changes.
#[derive(Debug)]
pub struct CompatReport {
    /// The live pages read.
    pub pages: usize,
    /// Pages by how their Markdown export changes.
    pub export: Outcomes,
    /// Pages by how their task listing changes.
    pub tasks: Outcomes,
    /// The pages with an outcome that fails the check, in id order.
    pub failing_pages: Vec<Uuid>,
    /// The sort-key groups the heal migration would rewrite.
    pub sort_key_groups: Vec<SortKeyGroup>,
    /// The live pages the refresh sweep would render again.
    pub stale_pages: usize,
    /// Whether `doc_markdown_cache.renderer_version` exists. A database from
    /// before the upgrade has none, and every live page there is stale.
    pub renderer_version_column: bool,
}

impl CompatReport {
    /// How many pages have an outcome that fails the check.
    pub fn violations(&self) -> usize {
        self.failing_pages.len()
    }
}

/// Pages by outcome, with the first page of each outcome that changes.
#[derive(Debug, Default)]
pub struct Outcomes {
    counts: BTreeMap<&'static str, usize>,
    first: BTreeMap<&'static str, String>,
}

impl Outcomes {
    /// Counts a page under `outcome`. `example` shows how the page changes
    /// (`None` when it doesn't), and is kept for the outcome's first page.
    fn add(&mut self, outcome: &'static str, example: impl FnOnce() -> Option<String>) {
        *self.counts.entry(outcome).or_default() += 1;
        if !self.first.contains_key(outcome)
            && let Some(example) = example()
        {
            self.first.insert(outcome, example);
        }
    }

    /// How many pages had each outcome.
    pub fn counts(&self) -> &BTreeMap<&'static str, usize> {
        &self.counts
    }

    /// How the first page with `outcome` changes.
    pub fn first(&self, outcome: &str) -> Option<&str> {
        self.first.get(outcome).map(String::as_str)
    }
}

/// A sibling group the heal migration rewrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKeyGroup {
    pub workspace_id: Uuid,
    /// `None` for the top level.
    pub parent_id: Option<Uuid>,
    /// Its rows, archived ones included.
    pub rows: i64,
    /// Why: `invalid_key`, `long_key` (over 64 characters), `duplicate_key`,
    /// or `root_list` (another group of the workspace's root list needs it).
    pub reason: String,
}

/// What upgrading the database behind `pool` to this release changes. It
/// only reads, and needs none of this release's migrations.
pub async fn compat_report(pool: &PgPool) -> CompatReport {
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM documents WHERE archived_at IS NULL ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("list the live pages");
    let mut export = Outcomes::default();
    let mut tasks = Outcomes::default();
    let mut failing_pages = Vec::new();
    for &id in &ids {
        let page = |example: String| format!("page {id}\n{example}");
        let outcomes = match knot_server::export_check::hydrate(pool, id).await {
            Ok(doc) => {
                let (exported, example) = guarded(|| export_outcome(&doc));
                export.add(exported, || example.map(page));
                let (listed, example) = guarded(|| task_outcome(&doc));
                tasks.add(listed, || example.map(page));
                [exported, listed]
            }
            Err(e) => {
                let why = page(format!("{e:#}"));
                export.add(LOAD_FAILED, || Some(why.clone()));
                tasks.add(LOAD_FAILED, || Some(why));
                [LOAD_FAILED; 2]
            }
        };
        if outcomes.iter().any(|outcome| FAILING.contains(outcome)) {
            failing_pages.push(id);
        }
    }
    let renderer_version_column = has_renderer_version(pool).await;
    CompatReport {
        pages: ids.len(),
        export,
        tasks,
        failing_pages,
        sort_key_groups: sort_key_groups(pool).await,
        stale_pages: stale_pages(pool, renderer_version_column).await,
        renderer_version_column,
    }
}

/// `check`'s outcome, or [`PANICKED`] with the panic's message.
fn guarded(
    check: impl FnOnce() -> (&'static str, Option<String>),
) -> (&'static str, Option<String>) {
    catch_unwind(AssertUnwindSafe(check)).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_owned());
        (PANICKED, Some(message))
    })
}

/// How `doc`'s Markdown export changes, and the diff when it does.
fn export_outcome(doc: &DocHandle) -> (&'static str, Option<String>) {
    let old = legacy::serialise(&YrsEngine, doc).map_err(|e| e.to_string());
    let new = knot_markdown::serialise(&YrsEngine, doc).map_err(|e| e.to_string());
    if old.is_ok() && old == new {
        return (UNCHANGED, None);
    }
    let outcome = compat::export_class(doc).unwrap_or_else(|| {
        if compat::healthy(doc).is_some() {
            HEALTHY_CHANGED
        } else {
            NOT_HEALTHY
        }
    });
    let diff = match (&old, &new) {
        (Ok(old), Ok(new)) => line_diff(old, new),
        _ => format!("legacy: {}\nnew:    {}", shown(&old), shown(&new)),
    };
    (outcome, Some(diff))
}

/// An export, or why it failed.
fn shown(export: &Result<String, String>) -> String {
    match export {
        Ok(md) => format!("{md:?}"),
        Err(e) => format!("error: {e}"),
    }
}

/// The lines that differ between `old` and `new`, between the lines both
/// start and end with, with up to [`CONTEXT`] lines around them. Each line
/// is quoted, so trailing spaces and blank lines show.
fn line_diff(old: &str, new: &str) -> String {
    fn lines(out: &mut String, sign: char, lines: &[&str]) {
        for line in lines.iter().take(SHOWN_LINES) {
            let _ = writeln!(out, "{sign} {line:?}");
        }
        if lines.len() > SHOWN_LINES {
            let _ = writeln!(out, "{sign} … {} more lines", lines.len() - SHOWN_LINES);
        }
    }
    let old: Vec<&str> = old.split_inclusive('\n').collect();
    let new: Vec<&str> = new.split_inclusive('\n').collect();
    let start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let end = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = String::from("--- legacy\n+++ new\n");
    lines(&mut out, ' ', &old[start.saturating_sub(CONTEXT)..start]);
    lines(&mut out, '-', &old[start..old.len() - end]);
    lines(&mut out, '+', &new[start..new.len() - end]);
    lines(&mut out, ' ', &old[old.len() - end..][..end.min(CONTEXT)]);
    out
}

/// How `doc`'s task listing changes, and the first task that does. Pages
/// whose legacy listing agrees with the room are judged by the task
/// differential `knot-markdown/tests/tasks_compat.rs` proves over the
/// corpus (`compat::task_verdicts`). A listing that breaks the lockstep
/// property there panics, which fails the check as [`PANICKED`].
fn task_outcome(doc: &DocHandle) -> (&'static str, Option<String>) {
    let new = compat::listing(doc);
    let old = compat::legacy_listing(doc);
    if new.is_empty() && old.as_ref().is_none_or(Vec::is_empty) {
        return (NO_TASKS, None);
    }
    let Some(old) = old else {
        return (NO_LEGACY_LISTING, Some(format!("new: {new:?}")));
    };
    if !compat::agrees_with_room(doc, &old) {
        let listings = format!("legacy: {old:?}\nnew:    {new:?}");
        return (LEGACY_DISAGREES, Some(listings));
    }
    let mut outcome = (UNCHANGED, None);
    for (k, (old, new, verdict)) in compat::task_verdicts(doc, old).into_iter().enumerate() {
        let example = || Some(format!("task {k}\n  legacy: {old:?}\n  new:    {new:?}"));
        match verdict {
            TaskVerdict::Same => {}
            TaskVerdict::Exempt(_) if old == new => {}
            TaskVerdict::Exempt(class) => {
                if outcome.0 == UNCHANGED {
                    outcome = (class, example());
                }
            }
            TaskVerdict::Differs => return (LISTED_DIFFERENTLY, example()),
        }
    }
    outcome
}

/// The sibling groups the heal migration rewrites (P7), in a `SELECT` that
/// mirrors its predicate. A group is the rows of one parent in a workspace,
/// archived ones included. It is rewritten when a key is invalid (outside
/// `^[0-9a-z]*[1-9a-z]$`), over 64 characters or repeated. A workspace's
/// root list (its top-level rows, and the children of its templates, which
/// the sidebar shows at the top level) is respread as one: when one of its
/// groups needs it, all of them are rewritten.
const SORT_KEY_GROUPS: &str = r#"
WITH sibling_groups AS (
    SELECT d.workspace_id,
           d.parent_id,
           d.parent_id IS NULL OR coalesce(bool_or(p.is_template), false) AS in_root_list,
           count(*) AS row_count,
           bool_or(d.sort_key COLLATE "C" !~ '^[0-9a-z]*[1-9a-z]$') AS invalid,
           bool_or(length(d.sort_key) > 64) AS too_long,
           count(DISTINCT d.sort_key COLLATE "C") < count(*) AS repeated
      FROM documents d
      LEFT JOIN documents p ON p.id = d.parent_id
     GROUP BY d.workspace_id, d.parent_id
)
SELECT g.workspace_id,
       g.parent_id,
       g.row_count,
       CASE WHEN g.invalid THEN 'invalid_key'
            WHEN g.too_long THEN 'long_key'
            WHEN g.repeated THEN 'duplicate_key'
            ELSE 'root_list'
       END
  FROM sibling_groups g
 WHERE g.invalid OR g.too_long OR g.repeated
    OR (g.in_root_list AND EXISTS (
            SELECT 1
              FROM sibling_groups bad
             WHERE bad.workspace_id = g.workspace_id
               AND bad.in_root_list
               AND (bad.invalid OR bad.too_long OR bad.repeated)))
 ORDER BY g.workspace_id, g.parent_id NULLS FIRST
"#;

async fn sort_key_groups(pool: &PgPool) -> Vec<SortKeyGroup> {
    let rows: Vec<(Uuid, Option<Uuid>, i64, String)> = sqlx::query_as(SORT_KEY_GROUPS)
        .fetch_all(pool)
        .await
        .expect("sort-key groups");
    rows.into_iter()
        .map(|(workspace_id, parent_id, rows, reason)| SortKeyGroup {
            workspace_id,
            parent_id,
            rows,
            reason,
        })
        .collect()
}

/// Whether the database has `doc_markdown_cache.renderer_version`, which
/// this release's migrations add.
async fn has_renderer_version(pool: &PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM information_schema.columns
              WHERE table_schema = current_schema()
                AND table_name = 'doc_markdown_cache'
                AND column_name = 'renderer_version')",
    )
    .fetch_one(pool)
    .await
    .expect("look up renderer_version")
}

/// The live pages the refresh sweep would render again: those the sweep's
/// own query finds, or, before the upgrade, every live page.
async fn stale_pages(pool: &PgPool, renderer_version_column: bool) -> usize {
    if renderer_version_column {
        return PgMarkdownCache::new(pool.clone())
            .stale_ids(RENDERER_VERSION, None, i64::MAX)
            .await
            .expect("stale pages")
            .len();
    }
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM documents WHERE archived_at IS NULL")
        .fetch_one(pool)
        .await
        .expect("count the live pages");
    usize::try_from(live).expect("a count")
}

impl fmt::Display for CompatReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} live pages", self.pages)?;
        write_outcomes(f, "Markdown export, pages by outcome", &self.export)?;
        write_outcomes(f, "Task listing, pages by outcome", &self.tasks)?;

        let rows: i64 = self.sort_key_groups.iter().map(|g| g.rows).sum();
        writeln!(
            f,
            "\nSort keys: the heal migration rewrites {} groups, {rows} rows",
            self.sort_key_groups.len()
        )?;
        for g in self.sort_key_groups.iter().take(SHOWN_GROUPS) {
            let parent = g
                .parent_id
                .map_or_else(|| "top level".to_owned(), |p| format!("parent {p}"));
            writeln!(
                f,
                "  workspace {} {parent}: {} rows, {}",
                g.workspace_id, g.rows, g.reason
            )?;
        }
        if self.sort_key_groups.len() > SHOWN_GROUPS {
            let more = self.sort_key_groups.len() - SHOWN_GROUPS;
            writeln!(f, "  … {more} more groups")?;
        }

        writeln!(
            f,
            "\nRefresh: the sweep renders {} live pages again",
            self.stale_pages
        )?;
        if !self.renderer_version_column {
            writeln!(
                f,
                "  (no doc_markdown_cache.renderer_version: a database from before the \
                 upgrade, where every live page is stale)"
            )?;
        }

        writeln!(f, "\nThe check fails on {} pages", self.failing_pages.len())?;
        for id in self.failing_pages.iter().take(SHOWN_PAGES) {
            writeln!(f, "  page {id}")?;
        }
        if self.failing_pages.len() > SHOWN_PAGES {
            let more = self.failing_pages.len() - SHOWN_PAGES;
            writeln!(f, "  … {more} more pages")?;
        }
        Ok(())
    }
}

fn write_outcomes(f: &mut fmt::Formatter<'_>, title: &str, outcomes: &Outcomes) -> fmt::Result {
    writeln!(f, "\n{title}:")?;
    for (outcome, count) in &outcomes.counts {
        let fails = if FAILING.contains(outcome) {
            "  <- fails the check"
        } else {
            ""
        };
        writeln!(f, "  {outcome:<28} {count:>8}{fails}")?;
    }
    for (outcome, example) in &outcomes.first {
        writeln!(f, "\nfirst {outcome}:")?;
        for line in example.lines() {
            writeln!(f, "  {line}")?;
        }
    }
    Ok(())
}

/// A pool on `url` whose every transaction is read-only
/// (`default_transaction_read_only`), so the check can't write.
async fn read_only_pool(url: &str) -> PgPool {
    let options = PgConnectOptions::from_str(url)
        .expect("database URL")
        .options([("default_transaction_read_only", "on")]);
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("connect read-only")
}

/// Opt-in: the check against a restored backup or a staging copy (see the
/// module docs).
#[tokio::test]
#[ignore = "reads the database at KNOT_COMPAT_DATABASE_URL; run it with --ignored"]
async fn real_data_compat() {
    let url = std::env::var("KNOT_COMPAT_DATABASE_URL")
        .expect("set KNOT_COMPAT_DATABASE_URL to a restored backup or a staging copy");
    let report = compat_report(&read_only_pool(&url).await).await;
    println!("{report}");
    assert_eq!(
        report.violations(),
        0,
        "pages that would change outside the named classes, or can't be read; see the \
         report above"
    );
}

/// Pages written with plain SQL, which a database from before the upgrade
/// accepts too.
struct Seed {
    pool: PgPool,
    ws: Uuid,
    user: Uuid,
}

impl Seed {
    async fn new(pool: &PgPool) -> Self {
        let ws = sqlx::query_scalar(
            "INSERT INTO workspaces (slug, name) VALUES ('default', 'WS') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let user = sqlx::query_scalar(
            "INSERT INTO users (email, display_name) VALUES ('alice@example.com', 'Alice')
             RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        Seed {
            pool: pool.clone(),
            ws,
            user,
        }
    }

    /// A live page under `parent`, keyed `sort_key`, holding `content`.
    async fn page(
        &self,
        parent: Option<Uuid>,
        sort_key: &str,
        content: Option<&DocHandle>,
    ) -> Uuid {
        let id = self.row(parent, sort_key, false, false).await;
        if let Some(doc) = content {
            self.update(id, YrsEngine.encode_state_as_update(doc, None).unwrap())
                .await;
        }
        id
    }

    /// Stores `bytes` as an update of `doc`.
    async fn update(&self, doc: Uuid, bytes: Vec<u8>) {
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, by_user_id) VALUES ($1, $2, $3)",
        )
        .bind(doc)
        .bind(bytes)
        .bind(self.user)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// An empty top-level template, keyed `sort_key`.
    async fn template(&self, sort_key: &str) -> Uuid {
        self.row(None, sort_key, true, false).await
    }

    /// An empty archived page under `parent`, keyed `sort_key`: the root of
    /// its own archive.
    async fn archived(&self, parent: Option<Uuid>, sort_key: &str) -> Uuid {
        self.row(parent, sort_key, false, true).await
    }

    async fn row(
        &self,
        parent: Option<Uuid>,
        sort_key: &str,
        template: bool,
        archived: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO documents
                 (id, workspace_id, parent_id, sort_key, created_by, is_template,
                  archived_at, archive_root_id)
             VALUES ($1, $2, $3, $4, $5, $6,
                     CASE WHEN $7 THEN now() END, CASE WHEN $7 THEN $1 END)",
        )
        .bind(id)
        .bind(self.ws)
        .bind(parent)
        .bind(sort_key)
        .bind(self.user)
        .bind(template)
        .bind(archived)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    /// A cache row for `doc`, as the release before this one wrote it: no
    /// renderer version.
    async fn legacy_cache_row(&self, doc: Uuid, md: &str) {
        sqlx::query(
            "INSERT INTO doc_markdown_cache (doc_id, rendered_at_seq, markdown_text)
             VALUES ($1, 1, $2)",
        )
        .bind(doc)
        .bind(md)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

/// A healthy page: one task.
const HEALTHY: &str = "- [ ] write the report\n";

/// A task the old importer damaged: it split the line break out of the
/// item's paragraph, which fails every legacy export (spec A1).
const DAMAGED: &str = "- [ ] first  \n  second\n";

/// A healthy page exports and lists its task as before, a damaged page falls
/// in `damaged_shape`, and the report shows how it changes. The check only
/// reads.
#[tokio::test]
async fn real_data_compat_runs_on_a_seeded_db() {
    let db = knot_test_support::fresh_db().await;
    let seed = Seed::new(&db.pool).await;
    let (healthy, _) = knot_markdown::from_markdown::parse(HEALTHY).unwrap();
    let (damaged, _) = legacy::parse(DAMAGED).unwrap();
    assert!(
        legacy::serialise(&YrsEngine, &damaged).is_err(),
        "the old importer's page is not damaged"
    );
    let healthy_id = seed.page(None, "a", Some(&healthy)).await;
    let damaged_id = seed.page(None, "b", Some(&damaged)).await;
    // The healthy page's render is current; the damaged page never rendered.
    sqlx::query(
        "INSERT INTO doc_markdown_cache
             (doc_id, rendered_at_seq, markdown_text, renderer_version)
         VALUES ($1, 1, $2, $3)",
    )
    .bind(healthy_id)
    .bind(HEALTHY)
    .bind(RENDERER_VERSION)
    .execute(&db.pool)
    .await
    .unwrap();

    let pool = read_only_pool(&db.url).await;
    let report = compat_report(&pool).await;
    println!("{report}");

    assert_eq!(report.pages, 2);
    assert_eq!(
        report.export.counts(),
        &BTreeMap::from([(UNCHANGED, 1), ("damaged_shape", 1)])
    );
    let diff = report.export.first("damaged_shape").expect("a diff");
    assert!(
        diff.contains(&damaged_id.to_string()) && diff.contains("first  \\n"),
        "the diff names the page and its healed export:\n{diff}"
    );
    assert_eq!(
        report.tasks.counts(),
        &BTreeMap::from([(UNCHANGED, 1), (NO_LEGACY_LISTING, 1)])
    );
    assert_eq!(report.violations(), 0);
    assert_eq!(report.sort_key_groups, Vec::new());
    assert!(report.renderer_version_column);
    assert_eq!(report.stale_pages, 1, "the damaged page has no render");

    let write = sqlx::query("CREATE TABLE compat_probe ()")
        .execute(&pool)
        .await
        .expect_err("the check's connection writes");
    assert_eq!(
        write.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("25006"),
        "not a read-only transaction: {write}"
    );
}

/// A task with a nested list, which legacy listed with the list's text as
/// its own (A4): a task class.
const NESTED: &str = "- [ ] parent\n  - child\n";

/// A task listed otherwise, in a task class, is counted under the class by
/// the task differential `knot-markdown/tests/tasks_compat.rs` runs, and
/// doesn't fail the check.
#[tokio::test]
async fn a_classed_task_is_counted_under_its_class() {
    let db = knot_test_support::fresh_db().await;
    let seed = Seed::new(&db.pool).await;
    let (nested, _) = knot_markdown::from_markdown::parse(NESTED).unwrap();
    let id = seed.page(None, "a", Some(&nested)).await;

    let report = compat_report(&read_only_pool(&db.url).await).await;
    println!("{report}");
    assert_eq!(
        report.tasks.counts(),
        &BTreeMap::from([("task_with_nested_list", 1)])
    );
    let example = report.tasks.first("task_with_nested_list").expect("a task");
    assert!(
        example.contains(&id.to_string()) && example.contains("text: \"parent\""),
        "names the page and the new listing: {example}"
    );
    assert_eq!(report.violations(), 0);
}

/// A page the check can't read is a page it didn't compare, so it fails the
/// check, as `knot-server export-check` does: whether its stored state
/// doesn't decode, or the database can't be read the way the check reads it
/// (here a column it reads is missing; on staging, say, a role without
/// `SELECT` on `doc_updates`).
#[tokio::test]
async fn a_page_that_does_not_load_fails_the_check() {
    let db = knot_test_support::fresh_db().await;
    let seed = Seed::new(&db.pool).await;
    let (healthy, _) = knot_markdown::from_markdown::parse(HEALTHY).unwrap();
    let healthy_id = seed.page(None, "a", Some(&healthy)).await;
    let broken = seed.page(None, "b", None).await;
    // Bytes no update decodes from.
    seed.update(broken, vec![0xff; 4]).await;

    let pool = read_only_pool(&db.url).await;
    let report = compat_report(&pool).await;
    println!("{report}");
    let counts = BTreeMap::from([(UNCHANGED, 1), (LOAD_FAILED, 1)]);
    assert_eq!(report.export.counts(), &counts);
    assert_eq!(report.tasks.counts(), &counts);
    let why = report.export.first(LOAD_FAILED).expect("the load error");
    assert!(why.contains(&broken.to_string()), "names the page: {why}");
    assert_eq!(report.failing_pages, vec![broken]);
    assert_eq!(report.violations(), 1);

    sqlx::query("ALTER TABLE doc_updates RENAME COLUMN by_user_id TO by_user")
        .execute(&db.pool)
        .await
        .unwrap();
    let report = compat_report(&pool).await;
    println!("{report}");
    assert_eq!(report.export.counts(), &BTreeMap::from([(LOAD_FAILED, 2)]));
    let why = report.export.first(LOAD_FAILED).expect("the load error");
    assert!(
        why.contains("reading stored state"),
        "a storage error: {why}"
    );
    let mut both = vec![healthy_id, broken];
    both.sort();
    assert_eq!(report.failing_pages, both);
    assert_eq!(report.violations(), 2);
}

/// The first migration of this release. A database migrated up to the one
/// before it is a backup from before the upgrade: it has no
/// `renderer_version`, and the old sort-key constraint, which lets top-level
/// keys repeat.
const UPGRADE: i64 = 20261024120000;

/// On a database from before the upgrade, every live page is stale, and the
/// sort-key groups the heal migration rewrites are those P7 names: a group
/// with an invalid, over-long or repeated key, and with a top-level group to
/// rewrite, every group of the workspace's root list.
#[tokio::test]
async fn real_data_compat_reads_a_db_from_before_the_upgrade() {
    let url = knot_test_support::fresh_db_url().await;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let full = sqlx::migrate!("../../migrations");
    assert!(
        full.migrations.iter().any(|m| m.version >= UPGRADE),
        "this release's migrations are not embedded (cargo clean -p knot-server?)"
    );
    let before = Migrator {
        migrations: Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version < UPGRADE)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    before
        .run(&pool)
        .await
        .expect("migrate to the previous release");

    let seed = Seed::new(&pool).await;
    let (healthy, _) = knot_markdown::from_markdown::parse(HEALTHY).unwrap();
    // The top level: a repeated key, a template and an archived page.
    let a = seed.page(None, "m", Some(&healthy)).await;
    let b = seed.page(None, "m", None).await;
    let template = seed.template("t").await;
    seed.archived(None, "n").await;
    // The template's child shows at the top level of the sidebar, so the
    // heal respreads it with the top level, though its own group is healthy.
    seed.page(Some(template), "a", None).await;
    // An invalid key, a healthy group, and a key over 64 characters.
    seed.page(Some(a), "M", None).await;
    let e = seed.page(Some(b), "a", None).await;
    seed.page(Some(e), &"a".repeat(65), None).await;
    // Without a renderer version, a cache row is an old render.
    seed.legacy_cache_row(a, HEALTHY).await;

    let report = compat_report(&read_only_pool(&url).await).await;
    println!("{report}");

    assert_eq!(report.pages, 7);
    assert_eq!(report.export.counts(), &BTreeMap::from([(UNCHANGED, 7)]));
    assert_eq!(
        report.tasks.counts(),
        &BTreeMap::from([(UNCHANGED, 1), (NO_TASKS, 6)])
    );
    assert!(!report.renderer_version_column);
    assert_eq!(report.stale_pages, 7);
    let groups: BTreeMap<Option<Uuid>, (i64, &str)> = report
        .sort_key_groups
        .iter()
        .map(|g| {
            assert_eq!(g.workspace_id, seed.ws);
            (g.parent_id, (g.rows, g.reason.as_str()))
        })
        .collect();
    assert_eq!(
        groups,
        BTreeMap::from([
            (None, (4, "duplicate_key")),
            (Some(template), (1, "root_list")),
            (Some(a), (1, "invalid_key")),
            (Some(e), (1, "long_key")),
        ])
    );
}
