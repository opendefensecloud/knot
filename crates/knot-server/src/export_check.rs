//! `knot-server export-check`: run every page through both export writers
//! offline and report what fails or degrades. Meant for a staging copy of the
//! database. It never migrates and never writes.

use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::Context as _;
use knot_crdt::DocHandle;
use knot_export::{Assets, Fallbacks, Meta};
use knot_storage::{DocStore, PgDocStore, PgWorkspaceStore, WorkspaceStore};
use sqlx::PgPool;
use uuid::Uuid;

use crate::routes::api::markdown::stored_snapshot;

#[derive(Debug)]
pub struct CheckReport {
    pub pages: usize,
    pub failed: Vec<(Uuid, String)>,
    pub fallbacks: Fallbacks,
}

/// The page as the room would load it: latest snapshot, then the updates
/// after it. Errors are returned, not swallowed. This is the markdown
/// refresh's load (`routes::api::markdown::stored_snapshot`), without its
/// seq.
pub async fn hydrate(pool: &PgPool, doc_id: Uuid) -> anyhow::Result<DocHandle> {
    let (doc, _seq) = stored_snapshot(pool, doc_id).await?;
    Ok(doc)
}

fn add(total: &mut Fallbacks, f: &Fallbacks) {
    total.diagram += f.diagram;
    total.image_unavailable += f.image_unavailable;
    total.image_over_budget += f.image_over_budget;
    total.unknown_element += f.unknown_element;
}

/// Walk and write one hydrated page in both formats. Returns the fallbacks
/// summed over the two writers.
fn check_page(handle: &DocHandle, base_url: &str) -> anyhow::Result<Fallbacks> {
    let walked = knot_export::walk(handle.inner());
    let meta = Meta {
        title: "export-check".to_owned(),
        exported_on: String::new(),
        base_url: base_url.to_owned(),
    };
    // Offline: no image or diagram is resolved, so they fall back.
    let assets = Assets::default();
    let unknown = u32::try_from(walked.unknown_elements.len()).unwrap_or(u32::MAX);
    let mut total = Fallbacks::default();
    for mut output in [
        knot_export::write_docx(&walked.doc, &meta, &assets)?,
        knot_export::write_odt(&walked.doc, &meta, &assets)?,
    ] {
        output.fallbacks.unknown_element += unknown;
        add(&mut total, &output.fallbacks);
    }
    Ok(total)
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
}

pub async fn check_all(pool: &PgPool, base_url: &str) -> anyhow::Result<CheckReport> {
    let mut report = CheckReport {
        pages: 0,
        failed: Vec::new(),
        fallbacks: Fallbacks::default(),
    };
    let Some(ws) = PgWorkspaceStore::new(pool.clone()).get_singleton().await? else {
        return Ok(report);
    };
    let docs = PgDocStore::new(pool.clone());
    let mut pages = docs.list_alive(ws.id).await?;
    pages.extend(docs.list_templates(ws.id).await?);

    for page in pages {
        report.pages += 1;
        let outcome = match hydrate(pool, page.id).await {
            Ok(handle) => catch_unwind(AssertUnwindSafe(|| check_page(&handle, base_url)))
                .unwrap_or_else(|p| Err(anyhow::anyhow!("panic: {}", panic_message(&*p)))),
            Err(e) => Err(e.context("hydrate")),
        };
        match outcome {
            Ok(f) => add(&mut report.fallbacks, &f),
            Err(e) => report.failed.push((page.id, format!("{e:#}"))),
        }
    }
    Ok(report)
}

/// Run the check against `cfg.database_url`; fails when any page fails.
pub async fn run(cfg: knot_config::Config) -> anyhow::Result<()> {
    if cfg.database_url.is_empty() {
        anyhow::bail!("KNOT_DATABASE_URL must be set");
    }
    // Not knot_storage::connect: that runs migrations.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&cfg.database_url)
        .await
        .context("database connect")?;
    let report = check_all(&pool, &cfg.base_url).await?;
    println!(
        "checked {} pages, {} failed",
        report.pages,
        report.failed.len()
    );
    let f = &report.fallbacks;
    println!("diagram: {}", f.diagram);
    println!("image_unavailable: {}", f.image_unavailable);
    println!("image_over_budget: {}", f.image_over_budget);
    println!("unknown_element: {}", f.unknown_element);
    for (id, error) in &report.failed {
        println!("failed {id}: {error}");
    }
    if !report.failed.is_empty() {
        anyhow::bail!("{} pages failed", report.failed.len());
    }
    Ok(())
}
