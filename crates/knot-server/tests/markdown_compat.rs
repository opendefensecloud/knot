//! The real-data compatibility check (spec "Compatibility guarantees", 2):
//! what upgrading a database to this release changes, found without
//! changing anything.
//!
//! [`compat_report`] reads every live page from storage, the way the
//! refresh sweep loads it (P18: no room is booted), and compares:
//! - its Markdown export by the legacy and by the new serializer, classified
//!   with `knot_markdown::legacy::compat`. A healthy page (its legacy export
//!   is a fixed point) outside the named export classes must export byte
//!   for byte as before. A page whose export changes, in a class or not, or
//!   that legacy couldn't export, must keep every text character of the page
//!   (serializer rule 2), and the new export of a page must not fail. Such a
//!   page's new export should also be a fixed point, `serialise(parse(new))
//!   == new`: one that isn't, outside `compat::stability_exempt`, is
//!   reported;
//! - its `/tasks` listing by legacy (the legacy export, then
//!   `extract_tasks`) and by the new Y-walk (`extract_tasks_from_doc`).
//!   The new listing must list one task per task item the room counts (the
//!   lockstep property). Where the legacy listing agrees with the room, a
//!   task outside the named task classes must be listed as before. The rule
//!   is the task differential of `knot_markdown::legacy::compat`, the one
//!   `knot-markdown/tests/tasks_compat.rs` proves over the corpus.
//!
//! A page that nests past the writer's depth limit (`compat::depth_limit`)
//! isn't compared with legacy: legacy, and the classes and task rules after
//! it, recurse once per element, and a stack overflow aborts the check
//! without a report. Its export and its listing count under `depth_limit`;
//! its export is still checked for lost text, and its listing for the
//! lockstep property.
//!
//! The report lists every page of each outcome but `unchanged` and
//! `no_tasks`.
//!
//! It also reports the sort-key groups the heal migration would rewrite
//! (P7), how many pages the refresh sweep would render again (P17), and
//! how many pages have a parent in another workspace, which the upgrade
//! leaves as they are (reported only).
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
//! if a page whose export changes would lose text, if the new export of a
//! page fails, if the new listing breaks the lockstep property, or if a page
//! can't be read, so wasn't compared.

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
/// The new export of a page whose export changed, or that legacy couldn't
/// export, is missing a text character of the page (serializer rule 2: no
/// text is ever lost). Fails the check, whatever the page's class or health.
const TEXT_LOST: &str = "text_lost";
/// Neither legacy nor the new serializer exports the page. The tolerant
/// export writes every page, so this fails the check too.
const NEW_EXPORT_FAILS: &str = "new_export_fails";

// Outcomes of a page's task listing, besides `UNCHANGED`, the task classes,
// `LOAD_FAILED` and `PANICKED`.

/// The page nests past the writer's depth limit (`compat::depth_limit`), so
/// it isn't compared with legacy, only checked for the lockstep property.
/// Its export counts under the export class of that name.
const DEPTH_LIMIT: &str = "depth_limit";

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
    TEXT_LOST,
    NEW_EXPORT_FAILS,
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
/// How many pages with a parent in another workspace the report names.
const SHOWN_CROSS_WORKSPACE: usize = 5;
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
    /// How many pages, archived ones included, have a parent in another
    /// workspace. This release no longer writes such a link (create, move,
    /// from-template and import return 404) but doesn't heal the ones there
    /// are, so they are reported and don't fail the check.
    pub cross_workspace_parents: i64,
    /// The first of those pages, in id order, each with its parent.
    pub cross_workspace_examples: Vec<(Uuid, Uuid)>,
    /// The pages whose export changes, or that legacy couldn't export, whose
    /// new export isn't a fixed point, in id order. Reported only: the
    /// corpus harness names the causes (`new_export_is_a_fixed_point`).
    pub not_fixed_point: Vec<Uuid>,
    /// How the first of those changes when imported and exported again.
    pub first_not_fixed_point: Option<String>,
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
        exported: ExportCheck,
        listed: (&'static str, Option<String>),
    ) {
        let page = |example: String| format!("page {id}\n{example}");
        let (listed, task_example) = listed;
        self.pages += 1;
        self.export
            .add(id, exported.outcome, || exported.example.map(page));
        self.tasks.add(id, listed, || task_example.map(page));
        if let Some(unfixed) = exported.unfixed {
            self.not_fixed_point.push(id);
            self.first_not_fixed_point
                .get_or_insert_with(|| page(unfixed));
        }
        if FAILING.contains(&exported.outcome) || FAILING.contains(&listed) {
            self.failing_pages.push(id);
        }
    }
}

/// Pages by outcome: how many had each, how the first page of each that
/// changes changes, and every page of each but [`UNCHANGED`] and
/// [`NO_TASKS`], in id order.
#[derive(Debug, Default)]
pub struct Outcomes {
    counts: BTreeMap<&'static str, usize>,
    first: BTreeMap<&'static str, String>,
    pages: BTreeMap<&'static str, Vec<Uuid>>,
}

impl Outcomes {
    /// Counts page `id` under `outcome`. `example` shows how the page
    /// changes (`None` when it doesn't), and is kept for the outcome's first
    /// page.
    fn add(&mut self, id: Uuid, outcome: &'static str, example: impl FnOnce() -> Option<String>) {
        *self.counts.entry(outcome).or_default() += 1;
        if !matches!(outcome, UNCHANGED | NO_TASKS) {
            self.pages.entry(outcome).or_default().push(id);
        }
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

    /// The pages with `outcome`, in id order: none for [`UNCHANGED`] and
    /// [`NO_TASKS`].
    pub fn pages(&self, outcome: &str) -> &[Uuid] {
        self.pages.get(outcome).map_or(&[], Vec::as_slice)
    }
}

/// How a page's Markdown export changes.
#[derive(Debug)]
struct ExportCheck {
    /// An export class, or one of the outcomes at the top of this file.
    outcome: &'static str,
    /// How the export changes: the diff, or why an export fails.
    example: Option<String>,
    /// When the page's export changes, or legacy couldn't export it, and its
    /// new export isn't a fixed point: how that export changes when
    /// imported and exported again.
    unfixed: Option<String>,
}

impl ExportCheck {
    /// The check of a page that a writer, or the check, panicked on.
    fn panicked(why: String) -> Self {
        Self {
            outcome: PANICKED,
            example: Some(why),
            unfixed: None,
        }
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
    compat_report_with(pool, new_export).await
}

/// A page's export by this release's serializer, or why it failed.
type Export = fn(&DocHandle) -> Result<String, String>;

/// This release's export of `doc`.
fn new_export(doc: &DocHandle) -> Result<String, String> {
    knot_markdown::serialise(&YrsEngine, doc).map_err(|e| e.to_string())
}

/// [`compat_report`], with `serialise` as this release's serializer: the
/// tests check the check against a lossy one.
async fn compat_report_with(pool: &PgPool, serialise: Export) -> CompatReport {
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM documents WHERE archived_at IS NULL ORDER BY id")
            .fetch_all(pool)
            .await
            .expect("list the live pages");
    let mut report = CompatReport::default();
    for id in ids {
        match knot_server::export_check::hydrate(pool, id).await {
            Ok(doc) => {
                let exported = guarded(|| export_outcome(&doc, serialise), ExportCheck::panicked);
                let listed = guarded(|| task_outcome(&doc), |why| (PANICKED, Some(why)));
                report.add_page(id, exported, listed);
            }
            Err(e) => {
                let why = format!("{e:#}");
                let exported = ExportCheck {
                    outcome: LOAD_FAILED,
                    example: Some(why.clone()),
                    unfixed: None,
                };
                report.add_page(id, exported, (LOAD_FAILED, Some(why)));
            }
        }
    }
    report.renderer_version_column = has_renderer_version(pool).await;
    report.sort_key_groups = sort_key_groups(pool).await;
    report.stale_pages = stale_pages(pool, report.renderer_version_column).await;
    (
        report.cross_workspace_parents,
        report.cross_workspace_examples,
    ) = cross_workspace_parents(pool).await;
    report
}

/// What `check` returns, or, when it panics, what `panicked` makes of the
/// panic's message. A stack overflow aborts instead, which is why legacy
/// isn't run on a page past the depth limit.
fn guarded<T>(check: impl FnOnce() -> T, panicked: impl FnOnce(String) -> T) -> T {
    catch_unwind(AssertUnwindSafe(check)).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".to_owned());
        panicked(message)
    })
}

/// How `doc`'s Markdown export by `serialise`, this release's serializer,
/// changes.
fn export_outcome(doc: &DocHandle, serialise: Export) -> ExportCheck {
    let new = serialise(doc);
    let old = if compat::depth_limit(doc) {
        Err("not compared: the page nests past the depth limit".to_owned())
    } else {
        legacy::serialise(&YrsEngine, doc).map_err(|e| e.to_string())
    };
    let text = compat::doc_text(doc);
    let lost = |md: &str| compat::first_lost_char(&text, md);
    let outcome = classify_export(
        &old,
        &new,
        || compat::export_class(doc),
        || compat::healthy(doc).is_some(),
        |md| lost(md).is_some(),
    );
    if outcome == UNCHANGED {
        return ExportCheck {
            outcome,
            example: None,
            unfixed: None,
        };
    }
    let mut example = match (&old, &new) {
        (Ok(old), Ok(new)) => line_diff(("legacy", old), ("new", new)),
        _ => format!("legacy: {}\nnew:    {}", shown(&old), shown(&new)),
    };
    if outcome == TEXT_LOST
        && let Ok(md) = &new
        && let Some((i, c)) = lost(md)
    {
        example = format!("lost {c:?}, character {i} of the page's text\n{example}");
    }
    ExportCheck {
        outcome,
        example: Some(example),
        unfixed: new
            .as_deref()
            .ok()
            .and_then(|md| not_fixed_point(md, serialise)),
    }
}

/// How a page's Markdown export changes, from its legacy export `old`, its
/// new export `new`, its export class (`compat::export_class`), whether it
/// is healthy (`compat::healthy`), and whether a new export loses text of
/// the page (`compat::first_lost_char`). The last three are asked for only
/// when they decide.
fn classify_export(
    old: &Result<String, String>,
    new: &Result<String, String>,
    class: impl FnOnce() -> Option<&'static str>,
    healthy: impl FnOnce() -> bool,
    loses_text: impl FnOnce(&str) -> bool,
) -> &'static str {
    if old.is_ok() && old == new {
        return UNCHANGED;
    }
    let new = match new {
        Ok(new) => new,
        Err(_) if old.is_ok() => return NO_LONGER_EXPORTS,
        Err(_) => return NEW_EXPORT_FAILS,
    };
    if loses_text(new) {
        return TEXT_LOST;
    }
    class().unwrap_or_else(|| {
        if healthy() {
            HEALTHY_CHANGED
        } else {
            NOT_HEALTHY
        }
    })
}

/// How `md`, an export by `serialise`, changes when imported and exported
/// again, when it does: it isn't a fixed point (serializer rule 2).
/// Markdown with text the writer can't write back yet
/// (`compat::stability_exempt`, a follow-up) is left out.
fn not_fixed_point(md: &str, serialise: Export) -> Option<String> {
    if compat::stability_exempt(md).is_some() {
        return None;
    }
    let again = knot_markdown::from_markdown::parse(md)
        .map_err(|e| e.to_string())
        .and_then(|(doc, _)| serialise(&doc));
    match again {
        Ok(again) if again == md => None,
        Ok(again) => Some(line_diff(("export", md), ("re-export", &again))),
        Err(e) => Some(format!("export: {md:?}\nre-export: error: {e}")),
    }
}

/// An export, or why it failed.
fn shown(export: &Result<String, String>) -> String {
    match export {
        Ok(md) => format!("{md:?}"),
        Err(e) => format!("error: {e}"),
    }
}

/// The lines that differ between `old` and `new`, each with its name,
/// between the lines both start and end with, with up to [`CONTEXT`] lines
/// around them. Each line is quoted, so trailing spaces and blank lines
/// show.
fn line_diff((old_name, old): (&str, &str), (new_name, new): (&str, &str)) -> String {
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
    let mut out = format!("--- {old_name}\n+++ {new_name}\n");
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
        compat::depth_limit(doc),
        || compat::legacy_listing(doc),
        |old| compat::agrees_with_room(doc, old),
        |old| compat::task_verdicts(doc, old),
    )
}

/// How a page's task listing changes, and the first task that does, from
/// its new listing `new`, the number of task items the room counts in it,
/// whether it nests past the depth limit (`deep`: then it isn't compared
/// with legacy), its legacy listing (`None` when legacy can't export the
/// page), whether that agrees with the room, and the verdict on each task.
/// The last three are asked for only when they decide.
fn classify_tasks(
    new: &[Listed],
    items: usize,
    deep: bool,
    old: impl FnOnce() -> Option<Vec<Listed>>,
    agrees: impl FnOnce(&[Listed]) -> bool,
    verdicts: impl FnOnce(Vec<Listed>) -> Vec<(Listed, Listed, TaskVerdict)>,
) -> (&'static str, Option<String>) {
    if new.len() != items {
        let why = format!("the room counts {items} task items\nnew: {new:?}");
        return (LOCKSTEP_BROKEN, Some(why));
    }
    if deep {
        return (DEPTH_LIMIT, None);
    }
    let old = old();
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
///
/// The heal is `migrations/20261024120000_heal_sort_keys.sql`. Keep the two
/// in step: when they disagree, the migration follows P7 and this follows it.
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

/// How many pages have a parent in another workspace, and the first
/// [`SHOWN_CROSS_WORKSPACE`] of them, in id order, each with its parent.
async fn cross_workspace_parents(pool: &PgPool) -> (i64, Vec<(Uuid, Uuid)>) {
    let count = sqlx::query_scalar(
        "SELECT count(*) FROM documents c JOIN documents p ON p.id = c.parent_id
          WHERE p.workspace_id <> c.workspace_id",
    )
    .fetch_one(pool)
    .await
    .expect("count the pages with a parent in another workspace");
    let examples = sqlx::query_as(
        "SELECT c.id, p.id FROM documents c JOIN documents p ON p.id = c.parent_id
          WHERE p.workspace_id <> c.workspace_id
          ORDER BY c.id
          LIMIT $1",
    )
    .bind(i64::try_from(SHOWN_CROSS_WORKSPACE).expect("a limit"))
    .fetch_all(pool)
    .await
    .expect("the pages with a parent in another workspace");
    (count, examples)
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

        writeln!(
            f,
            "\nNot a fixed point: {} pages whose export changes (reported only; the \
             check doesn't fail on them)",
            self.not_fixed_point.len()
        )?;
        if let Some(example) = &self.first_not_fixed_point {
            writeln!(f, "\nfirst not a fixed point:")?;
            for line in example.lines() {
                writeln!(f, "  {line}")?;
            }
        }
        for id in &self.not_fixed_point {
            writeln!(f, "  page {id}")?;
        }

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

        writeln!(
            f,
            "\nParents in another workspace: {} pages (reported only; the check \
             doesn't fail on them, and the upgrade doesn't change them)",
            self.cross_workspace_parents
        )?;
        for (page, parent) in &self.cross_workspace_examples {
            writeln!(f, "  page {page} under {parent}")?;
        }
        let shown = i64::try_from(self.cross_workspace_examples.len()).expect("a count");
        if self.cross_workspace_parents > shown {
            let more = self.cross_workspace_parents - shown;
            writeln!(f, "  … {more} more pages")?;
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
    for (outcome, pages) in &outcomes.pages {
        writeln!(f, "\nevery {outcome} page:")?;
        for id in pages {
            writeln!(f, "  page {id}")?;
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
    let exported = ExportCheck {
        outcome: exported,
        example: None,
        unfixed: None,
    };
    report.add_page(Uuid::nil(), exported, (listed, None));
    report.violations()
}

/// The export decision table: how a page's export changes, by its legacy
/// and new export, its export class, whether it is healthy and whether the
/// new export loses text of the page, and whether that fails the check.
#[test]
fn export_outcomes_and_the_ones_that_fail_the_check() {
    let md = |text: &str| Ok::<_, String>(text.to_owned());
    let error = || Err::<String, _>("unknown node".to_owned());
    let class = Some("damaged_shape");
    // What, the legacy and the new export, the class, healthy, whether text
    // is lost, the outcome, and whether it fails the check.
    let rows = [
        (
            "as before",
            md("a\n"),
            md("a\n"),
            None,
            true,
            false,
            UNCHANGED,
            false,
        ),
        // Lost by legacy already: nothing changes.
        (
            "as before, losing text as legacy did",
            md("a\n"),
            md("a\n"),
            None,
            true,
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
            false,
            HEALTHY_CHANGED,
            true,
        ),
        (
            "healthy, text lost",
            md("a\n"),
            md("b\n"),
            None,
            true,
            true,
            TEXT_LOST,
            true,
        ),
        (
            "classed, changed",
            md("a\n"),
            md("b\n"),
            class,
            true,
            false,
            "damaged_shape",
            false,
        ),
        (
            "classed, text lost",
            md("a\n"),
            md("b\n"),
            class,
            true,
            true,
            TEXT_LOST,
            true,
        ),
        (
            "not healthy, changed",
            md("a\n"),
            md("b\n"),
            None,
            false,
            false,
            NOT_HEALTHY,
            false,
        ),
        (
            "not healthy, text lost",
            md("a\n"),
            md("b\n"),
            None,
            false,
            true,
            TEXT_LOST,
            true,
        ),
        (
            "legacy fails",
            error(),
            md("b\n"),
            None,
            false,
            false,
            NOT_HEALTHY,
            false,
        ),
        (
            "legacy fails, text lost",
            error(),
            md("b\n"),
            class,
            false,
            true,
            TEXT_LOST,
            true,
        ),
        (
            "both fail",
            error(),
            error(),
            None,
            false,
            false,
            NEW_EXPORT_FAILS,
            true,
        ),
        (
            "new fails, healthy",
            md("a\n"),
            error(),
            None,
            true,
            false,
            NO_LONGER_EXPORTS,
            true,
        ),
        (
            "new fails, not healthy",
            md("a\n"),
            error(),
            None,
            false,
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
            false,
            NO_LONGER_EXPORTS,
            true,
        ),
    ];
    let wrong: Vec<String> = rows
        .into_iter()
        .filter_map(|(what, old, new, class, healthy, loses, want, fails)| {
            let got = classify_export(&old, &new, || class, || healthy, |_| loses);
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
        classify_tasks(&new, items, false, || old, |_| agrees, |_| verdicts).0
    };
    // A page past the depth limit: legacy isn't asked.
    let deep = |new: Vec<Listed>, items: usize| {
        let legacy =
            || -> Option<Vec<Listed>> { panic!("legacy asked about a page past the depth limit") };
        classify_tasks(&new, items, true, legacy, |_| true, |_| Vec::new()).0
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
            "past the depth limit",
            deep(vec![a()], 1),
            DEPTH_LIMIT,
            false,
        ),
        (
            "past the depth limit, a task fewer than the room's items",
            deep(vec![a()], 2),
            LOCKSTEP_BROKEN,
            true,
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
    let ExportCheck {
        outcome,
        example: why,
        unfixed,
    } = guarded(|| panic!("a writer panicked"), ExportCheck::panicked);
    assert_eq!(outcome, PANICKED);
    assert_eq!(why.as_deref(), Some("a writer panicked"));
    assert_eq!(unfixed, None);
    assert_eq!(violations_of(outcome, UNCHANGED), 1);
    assert_eq!(violations_of(UNCHANGED, outcome), 1);
    // A page counts once, however many of its outcomes fail.
    assert_eq!(violations_of(PANICKED, PANICKED), 1);
}

/// The other half of serializer rule 2, reported only: a new export that
/// reads back as another export isn't a fixed point. Markdown with text the
/// writer can't write back yet (`compat::stability_exempt`) isn't reported.
#[test]
fn an_export_that_reads_back_otherwise_is_reported_as_not_a_fixed_point() {
    assert_eq!(not_fixed_point("a\n", new_export), None);
    assert_eq!(
        not_fixed_point("\\*a\n", new_export),
        None,
        "an escape: exempt"
    );
    // A line break in a heading reads back as a paragraph below it.
    let unfixed = not_fixed_point("# a  \nb\n", new_export).expect("not a fixed point");
    assert!(
        unfixed.contains("--- export") && unfixed.contains("+++ re-export"),
        "{unfixed}"
    );

    let mut report = CompatReport::default();
    let page = Uuid::new_v4();
    let exported = ExportCheck {
        outcome: "nested_tasklist",
        example: None,
        unfixed: Some(unfixed),
    };
    report.add_page(page, exported, (UNCHANGED, None));
    assert_eq!(report.not_fixed_point, [page]);
    assert_eq!(report.violations(), 0);
    let shown = report.to_string();
    assert!(
        shown.contains("Not a fixed point: 1 pages") && shown.contains(&format!("page {page}")),
        "{shown}"
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
    // Every page whose export changes is named, under its outcome.
    assert_eq!(report.export.pages("damaged_shape"), [damaged_id]);
    assert!(report.export.pages(UNCHANGED).is_empty());
    assert_eq!(report.tasks.pages(NO_LEGACY_LISTING), [damaged_id]);
    assert!(
        report
            .to_string()
            .contains(&format!("every damaged_shape page:\n  page {damaged_id}")),
        "{report}"
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
    assert_eq!(report.cross_workspace_parents, 0);

    // A page whose parent is in another workspace, which a create, move or
    // import before this release could write: counted and named, and it
    // doesn't fail the check.
    let other = seed.workspace("other").await;
    let foreign_child = other.page(Some(healthy_id), "m", None).await;
    let report = compat_report(&pool).await;
    println!("{report}");
    assert_eq!(report.cross_workspace_parents, 1);
    assert_eq!(
        report.cross_workspace_examples,
        vec![(foreign_child, healthy_id)]
    );
    let shown = report.to_string();
    assert!(
        shown.contains("Parents in another workspace: 1 pages")
            && shown.contains(&format!("page {foreign_child} under {healthy_id}")),
        "the report shows the count and the page:\n{shown}"
    );
    assert_eq!(report.violations(), 0);
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

/// A page nested far deeper than the editor makes one, as an import of `>`
/// repeated stores it, is checked without legacy: legacy, and the classes
/// after `depth_limit`, recurse once per element, and a stack overflow
/// would abort the check without a report. Its export and its listing count
/// under `depth_limit`; its export keeps its text, so the check passes.
#[tokio::test]
async fn real_data_compat_reads_a_deeply_nested_page() {
    let db = knot_test_support::fresh_db().await;
    let seed = Seed::new(&db.pool).await;
    let (deep, _) =
        knot_markdown::from_markdown::parse(&format!("{} x\n", ">".repeat(100_000))).unwrap();
    let (healthy, _) = knot_markdown::from_markdown::parse(HEALTHY).unwrap();
    let deep_id = seed.page(None, "a", Some(&deep)).await;
    seed.page(None, "b", Some(&healthy)).await;

    let report = compat_report(&read_only_pool(&db.url).await).await;
    println!("{report}");
    assert_eq!(
        report.export.counts(),
        &BTreeMap::from([(UNCHANGED, 1), ("depth_limit", 1)])
    );
    assert_eq!(
        report.tasks.counts(),
        &BTreeMap::from([(UNCHANGED, 1), (DEPTH_LIMIT, 1)])
    );
    assert_eq!(report.export.pages("depth_limit"), [deep_id]);
    assert!(report.not_fixed_point.is_empty());
    assert_eq!(report.violations(), 0);
}

/// A page in an export class: a task with a nested checklist.
const CLASSED: &str = "- [ ] parent\n  - [ ] child task\n";

/// This release's export without its lines after the first: a writer that
/// drops a task's nested checklist, as the final review's mutant did.
fn lossy_export(doc: &DocHandle) -> Result<String, String> {
    let md = new_export(doc)?;
    Ok(format!("{}\n", md.lines().next().unwrap_or_default()))
}

/// A page whose export a class lets change may change only as serializer
/// rule 2 allows: no text of the page is lost. A writer that drops a block
/// fails the check on it (final review md-2), as it fails the corpus
/// harness's `no_text_is_lost`.
#[tokio::test]
async fn a_classed_page_that_loses_text_fails_the_check() {
    let db = knot_test_support::fresh_db().await;
    let seed = Seed::new(&db.pool).await;
    let (classed, _) = knot_markdown::from_markdown::parse(CLASSED).unwrap();
    assert_eq!(compat::export_class(&classed), Some("nested_tasklist"));
    let id = seed.page(None, "a", Some(&classed)).await;
    let pool = read_only_pool(&db.url).await;

    let report = compat_report(&pool).await;
    assert_eq!(
        report.export.counts(),
        &BTreeMap::from([("nested_tasklist", 1)])
    );
    assert_eq!(report.violations(), 0);

    let report = compat_report_with(&pool, lossy_export).await;
    println!("{report}");
    assert_eq!(report.export.counts(), &BTreeMap::from([(TEXT_LOST, 1)]));
    assert_eq!(report.failing_pages, vec![id]);
    let why = report.export.first(TEXT_LOST).expect("the lost text");
    assert!(
        why.contains(&id.to_string()) && why.contains("child task"),
        "names the page and shows the diff: {why}"
    );
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
