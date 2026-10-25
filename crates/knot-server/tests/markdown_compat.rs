//! The real-data compatibility check (spec "Compatibility guarantees", 2):
//! what upgrading a database to this release changes, found without
//! changing anything.
//!
//! [`compat_report`] reads every live page from storage, the way the
//! refresh sweep loads it (P18: no room is booted), and compares:
//! - its Markdown export by the legacy and by the new serializer, classified
//!   with `knot_markdown::legacy::compat`. A healthy page (its legacy export
//!   is a fixed point) outside the named export classes must export byte
//!   for byte as before, and a page legacy exports must still export;
//! - its `/tasks` listing by legacy (the legacy export, then
//!   `extract_tasks`) and by the new Y-walk (`extract_tasks_from_doc`).
//!   The new listing must list one task per task item the room counts (the
//!   lockstep property). Where the legacy listing agrees with the room, a
//!   task outside the named task classes must be listed as before. The rule
//!   is the task differential of `knot_markdown::legacy::compat`, the one
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
//! if a page legacy exports would no longer export, if the new listing
//! breaks the lockstep property, or if a page can't be read, so wasn't
//! compared.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::str::FromStr;

use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::RENDERER_VERSION;
use knot_markdown::legacy;
use knot_markdown::legacy::compat::{self, Listed, TaskVerdict};
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
/// Legacy exports the page, and the new serializer returns an error: the
/// page exported before and no longer does. Fails the check, whatever the
/// page's class or health (serializer rule 2: an error keeps no text).
const NO_LONGER_EXPORTS: &str = "no_longer_exports";
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
/// The new listing lists another number of tasks than the room has task
/// items, which breaks the lockstep property: listing index `k` and toggle
/// index `k` are no longer the same item. Fails the check.
const LOCKSTEP_BROKEN: &str = "lockstep_broken";

/// The outcomes that fail the check.
const FAILING: &[&str] = &[
    HEALTHY_CHANGED,
    NO_LONGER_EXPORTS,
    LISTED_DIFFERENTLY,
    LOCKSTEP_BROKEN,
    LOAD_FAILED,
    PANICKED,
];

/// How many failing pages, sort-key groups, and lines of a diff's side, the
/// report shows.
const SHOWN_PAGES: usize = 20;
const SHOWN_GROUPS: usize = 20;
const SHOWN_LINES: usize = 20;
/// Lines of context around a diff.
const CONTEXT: usize = 2;

/// What upgrading a database to this release changes.
#[derive(Debug, Default)]
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

    /// Counts page `id` under its export and its task outcome, each with
    /// how the page changes, and as failing when either outcome fails the
    /// check.
    fn add_page(
        &mut self,
        id: Uuid,
        (exported, export_example): (&'static str, Option<String>),
        (listed, task_example): (&'static str, Option<String>),
    ) {
        let page = |example: String| format!("page {id}\n{example}");
        self.pages += 1;
        self.export.add(exported, || export_example.map(page));
        self.tasks.add(listed, || task_example.map(page));
        if FAILING.contains(&exported) || FAILING.contains(&listed) {
            self.failing_pages.push(id);
        }
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
    let mut report = CompatReport::default();
    for id in ids {
        match knot_server::export_check::hydrate(pool, id).await {
            Ok(doc) => {
                let exported = guarded(|| export_outcome(&doc));
                let listed = guarded(|| task_outcome(&doc));
                report.add_page(id, exported, listed);
            }
            Err(e) => {
                let why = format!("{e:#}");
                report.add_page(
                    id,
                    (LOAD_FAILED, Some(why.clone())),
                    (LOAD_FAILED, Some(why)),
                );
            }
        }
    }
    report.renderer_version_column = has_renderer_version(pool).await;
    report.sort_key_groups = sort_key_groups(pool).await;
    report.stale_pages = stale_pages(pool, report.renderer_version_column).await;
    report
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
    let outcome = classify_export(
        &old,
        &new,
        || compat::export_class(doc),
        || compat::healthy(doc).is_some(),
    );
    if outcome == UNCHANGED {
        return (UNCHANGED, None);
    }
    let diff = match (&old, &new) {
        (Ok(old), Ok(new)) => line_diff(old, new),
        _ => format!("legacy: {}\nnew:    {}", shown(&old), shown(&new)),
    };
    (outcome, Some(diff))
}

/// How a page's Markdown export changes, from its legacy export `old`, its
/// new export `new`, its export class (`compat::export_class`) and whether
/// it is healthy (`compat::healthy`). The last two are asked for only when
/// they decide.
fn classify_export(
    old: &Result<String, String>,
    new: &Result<String, String>,
    class: impl FnOnce() -> Option<&'static str>,
    healthy: impl FnOnce() -> bool,
) -> &'static str {
    if old.is_ok() && old == new {
        return UNCHANGED;
    }
    if old.is_ok() && new.is_err() {
        return NO_LONGER_EXPORTS;
    }
    class().unwrap_or_else(|| {
        if healthy() {
            HEALTHY_CHANGED
        } else {
            NOT_HEALTHY
        }
    })
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
/// corpus (`compat::task_verdicts`). A new listing with another number of
/// tasks than the room has task items is [`LOCKSTEP_BROKEN`], before
/// anything else is compared.
fn task_outcome(doc: &DocHandle) -> (&'static str, Option<String>) {
    classify_tasks(
        &compat::listing(doc),
        compat::item_count(doc),
        compat::legacy_listing(doc),
        |old| compat::agrees_with_room(doc, old),
        |old| compat::task_verdicts(doc, old),
    )
}

/// How a page's task listing changes, and the first task that does, from
/// its new listing `new`, the number of task items the room counts in it,
/// its legacy listing `old` (`None` when legacy can't export the page),
/// whether that agrees with the room, and the verdict on each task. The
/// last two are asked for only when they decide.
fn classify_tasks(
    new: &[Listed],
    items: usize,
    old: Option<Vec<Listed>>,
    agrees: impl FnOnce(&[Listed]) -> bool,
    verdicts: impl FnOnce(Vec<Listed>) -> Vec<(Listed, Listed, TaskVerdict)>,
) -> (&'static str, Option<String>) {
    if new.len() != items {
        let why = format!("the room counts {items} task items\nnew: {new:?}");
        return (LOCKSTEP_BROKEN, Some(why));
    }
    if new.is_empty() && old.as_ref().is_none_or(Vec::is_empty) {
        return (NO_TASKS, None);
    }
    let Some(old) = old else {
        return (NO_LEGACY_LISTING, Some(format!("new: {new:?}")));
    };
    if !agrees(&old) {
        let listings = format!("legacy: {old:?}\nnew:    {new:?}");
        return (LEGACY_DISAGREES, Some(listings));
    }
    let mut outcome = (UNCHANGED, None);
    for (k, (old, new, verdict)) in verdicts(old).into_iter().enumerate() {
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

/// The violations of a report on one page with these outcomes.
fn violations_of(exported: &'static str, listed: &'static str) -> usize {
    let mut report = CompatReport::default();
    report.add_page(Uuid::nil(), (exported, None), (listed, None));
    report.violations()
}

/// The export decision table: how a page's export changes, by its legacy
/// and new export, its export class and whether it is healthy, and whether
/// that fails the check.
#[test]
fn export_outcomes_and_the_ones_that_fail_the_check() {
    let md = |text: &str| Ok::<_, String>(text.to_owned());
    let error = || Err::<String, _>("unknown node".to_owned());
    let class = Some("damaged_shape");
    // What, the legacy and the new export, the class, healthy, the outcome,
    // and whether it fails the check.
    let rows = [
        (
            "as before",
            md("a\n"),
            md("a\n"),
            None,
            true,
            UNCHANGED,
            false,
        ),
        (
            "healthy, changed",
            md("a\n"),
            md("b\n"),
            None,
            true,
            HEALTHY_CHANGED,
            true,
        ),
        (
            "classed, changed",
            md("a\n"),
            md("b\n"),
            class,
            true,
            "damaged_shape",
            false,
        ),
        (
            "not healthy, changed",
            md("a\n"),
            md("b\n"),
            None,
            false,
            NOT_HEALTHY,
            false,
        ),
        (
            "legacy fails",
            error(),
            md("b\n"),
            None,
            false,
            NOT_HEALTHY,
            false,
        ),
        (
            "both fail",
            error(),
            error(),
            None,
            false,
            NOT_HEALTHY,
            false,
        ),
        (
            "new fails, healthy",
            md("a\n"),
            error(),
            None,
            true,
            NO_LONGER_EXPORTS,
            true,
        ),
        (
            "new fails, not healthy",
            md("a\n"),
            error(),
            None,
            false,
            NO_LONGER_EXPORTS,
            true,
        ),
        (
            "new fails, classed",
            md("a\n"),
            error(),
            class,
            false,
            NO_LONGER_EXPORTS,
            true,
        ),
    ];
    let wrong: Vec<String> = rows
        .into_iter()
        .filter_map(|(what, old, new, class, healthy, want, fails)| {
            let got = classify_export(&old, &new, || class, || healthy);
            let failed = violations_of(got, UNCHANGED) > 0;
            (got != want || failed != fails)
                .then(|| format!("{what}: {got} (fails: {failed}), want {want} (fails: {fails})"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "rows decided wrong:\n{}",
        wrong.join("\n")
    );
}

/// A task as a listing holds it.
fn listed(index: i32, text: &str) -> Listed {
    Listed {
        index,
        text: text.to_owned(),
        assignee: None,
        checked: false,
        due: None,
    }
}

/// The task decision table: how a page's task listing changes, by its new
/// listing, the task items the room counts, its legacy listing, whether
/// that agrees with the room, and the verdict on each task, and whether
/// that fails the check.
#[test]
fn task_outcomes_and_the_ones_that_fail_the_check() {
    let a = || listed(0, "a");
    let b = || listed(1, "b");
    let changed = || listed(0, "a, changed");
    let class = "task_with_nested_list";
    let outcome = |new: Vec<Listed>,
                   items: usize,
                   old: Option<Vec<Listed>>,
                   agrees: bool,
                   verdicts: Vec<(Listed, Listed, TaskVerdict)>| {
        classify_tasks(&new, items, old, |_| agrees, |_| verdicts).0
    };
    // What, the outcome, the one wanted, and whether it fails the check.
    let rows = [
        (
            "no tasks",
            outcome(vec![], 0, Some(vec![]), true, vec![]),
            NO_TASKS,
            false,
        ),
        (
            "no tasks, legacy can't export",
            outcome(vec![], 0, None, true, vec![]),
            NO_TASKS,
            false,
        ),
        (
            "legacy can't export",
            outcome(vec![a()], 1, None, true, vec![]),
            NO_LEGACY_LISTING,
            false,
        ),
        (
            "legacy disagrees with the room",
            outcome(vec![a()], 1, Some(vec![a()]), false, vec![]),
            LEGACY_DISAGREES,
            false,
        ),
        (
            "as before",
            outcome(
                vec![a()],
                1,
                Some(vec![a()]),
                true,
                vec![(a(), a(), TaskVerdict::Same)],
            ),
            UNCHANGED,
            false,
        ),
        (
            "classed, as before",
            outcome(
                vec![a()],
                1,
                Some(vec![a()]),
                true,
                vec![(a(), a(), TaskVerdict::Exempt(class))],
            ),
            UNCHANGED,
            false,
        ),
        (
            "classed, changed",
            outcome(
                vec![changed()],
                1,
                Some(vec![a()]),
                true,
                vec![(a(), changed(), TaskVerdict::Exempt(class))],
            ),
            class,
            false,
        ),
        (
            "changed",
            outcome(
                vec![changed()],
                1,
                Some(vec![a()]),
                true,
                vec![(a(), changed(), TaskVerdict::Differs)],
            ),
            LISTED_DIFFERENTLY,
            true,
        ),
        (
            "classed, then changed",
            outcome(
                vec![changed(), listed(1, "b, changed")],
                2,
                Some(vec![a(), b()]),
                true,
                vec![
                    (a(), changed(), TaskVerdict::Exempt(class)),
                    (b(), listed(1, "b, changed"), TaskVerdict::Differs),
                ],
            ),
            LISTED_DIFFERENTLY,
            true,
        ),
        (
            "a task fewer than the room's items",
            outcome(
                vec![a()],
                2,
                Some(vec![a(), b()]),
                true,
                vec![(a(), a(), TaskVerdict::Same)],
            ),
            LOCKSTEP_BROKEN,
            true,
        ),
        (
            "every task lost, legacy listed none",
            outcome(vec![], 1, Some(vec![]), true, vec![]),
            LOCKSTEP_BROKEN,
            true,
        ),
        (
            "every task lost, legacy can't export",
            outcome(vec![], 1, None, true, vec![]),
            LOCKSTEP_BROKEN,
            true,
        ),
        (
            "a task more than the room's items, legacy can't export",
            outcome(vec![a(), b()], 1, None, true, vec![]),
            LOCKSTEP_BROKEN,
            true,
        ),
    ];
    let wrong: Vec<String> = rows
        .into_iter()
        .filter_map(|(what, got, want, fails)| {
            let failed = violations_of(UNCHANGED, got) > 0;
            (got != want || failed != fails)
                .then(|| format!("{what}: {got} (fails: {failed}), want {want} (fails: {fails})"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "rows decided wrong:\n{}",
        wrong.join("\n")
    );
}

/// A writer, or the check itself, that panics on a page fails the check,
/// and the report keeps the panic's message.
#[test]
fn a_panic_fails_the_check() {
    let (outcome, why) = guarded(|| panic!("a writer panicked"));
    assert_eq!(outcome, PANICKED);
    assert_eq!(why.as_deref(), Some("a writer panicked"));
    assert_eq!(violations_of(outcome, UNCHANGED), 1);
    assert_eq!(violations_of(UNCHANGED, outcome), 1);
    // A page counts once, however many of its outcomes fail.
    assert_eq!(violations_of(PANICKED, PANICKED), 1);
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

    /// A second workspace, `slug`, with the same user.
    async fn workspace(&self, slug: &str) -> Seed {
        let ws = sqlx::query_scalar(
            "INSERT INTO workspaces (slug, name) VALUES ($1, 'Other WS') RETURNING id",
        )
        .bind(slug)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        Seed {
            pool: self.pool.clone(),
            ws,
            user: self.user,
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

/// A fresh database migrated up to the release before this one, its URL and a
/// pool on it.
async fn db_before_the_upgrade() -> (String, PgPool) {
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
    (url, pool)
}

/// On a database from before the upgrade, every live page is stale, and the
/// sort-key groups the heal migration rewrites are those P7 names: a group
/// with an invalid, over-long or repeated key, and with a top-level group to
/// rewrite, every group of the workspace's root list.
#[tokio::test]
async fn real_data_compat_reads_a_db_from_before_the_upgrade() {
    let (url, pool) = db_before_the_upgrade().await;
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

/// The sort-key `SELECT` on more of the keys and groups the heal migration
/// meets (P7), on a database from before the upgrade:
/// - keys ending in `0` (`0`, `00`, `m0`) and an empty key are invalid;
/// - a template's children are a group of their own: a bad one is rewritten
///   for its own key, and the rest of the root list with it;
/// - a group's archived rows are rewritten with it, in a group outside the
///   root list too;
/// - another workspace's root list is its own: its healthy groups stay as
///   they are, however bad this workspace's root list is.
#[tokio::test]
async fn sort_key_groups_flag_each_bad_key_and_only_this_workspace() {
    let (url, pool) = db_before_the_upgrade().await;
    let seed = Seed::new(&pool).await;
    // The top level, healthy on its own, and the root list with `template`.
    let a = seed.page(None, "a", None).await;
    let b = seed.page(None, "b", None).await;
    let c = seed.page(None, "c", None).await;
    let template = seed.template("t").await;
    // A bad group under the template.
    seed.page(Some(template), "0", None).await;
    // A bad group with an archived row, outside the root list.
    let a1 = seed.page(Some(a), "a", None).await;
    seed.page(Some(a), "00", None).await;
    seed.archived(Some(a), "c").await;
    seed.page(Some(b), "m0", None).await;
    seed.page(Some(c), "", None).await;
    // A healthy group outside the root list.
    seed.page(Some(a1), "m", None).await;
    // Another workspace whose groups are all healthy, archived rows and a
    // template's children included.
    let other = seed.workspace("other").await;
    let x = other.page(None, "m", None).await;
    other.archived(None, "n").await;
    let other_template = other.template("t").await;
    other.page(Some(other_template), "a", None).await;
    other.page(Some(x), "a", None).await;
    other.archived(Some(x), "b").await;

    let groups: BTreeMap<(Uuid, Option<Uuid>), (i64, String)> =
        sort_key_groups(&read_only_pool(&url).await)
            .await
            .into_iter()
            .map(|g| ((g.workspace_id, g.parent_id), (g.rows, g.reason)))
            .collect();
    let ws = seed.ws;
    let group = |rows: i64, reason: &str| (rows, reason.to_owned());
    assert_eq!(
        groups,
        BTreeMap::from([
            ((ws, None), group(4, "root_list")),
            ((ws, Some(template)), group(1, "invalid_key")),
            ((ws, Some(a)), group(3, "invalid_key")),
            ((ws, Some(b)), group(1, "invalid_key")),
            ((ws, Some(c)), group(1, "invalid_key")),
        ])
    );
}
