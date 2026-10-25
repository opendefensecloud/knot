//! Duplicate a page, and with `include_subpages` its live subpages
//! (`POST /api/docs/{id}/duplicate`, spec D).
//!
//! Everything is prepared before the one transaction that stores the copy
//! (`knot_storage::duplicate::apply_duplicate`):
//! - the content of every page, copied from its current state without
//!   booting a room (P18), with its references to the copied pages, boards
//!   and files pointed at their copies (P14);
//! - the boards and files those pages show, when the user can read the page
//!   they belong to (P19). Each gets a copy of its own, so the copy never
//!   depends on the original. Other references stay as they are;
//! - the objects of files kept outside the database, copied before the
//!   transaction, once their ids are recorded in `blob_copy_pending` (P16).
//!   Nothing is deleted on the request path: if the transaction doesn't
//!   commit, the hourly purge job removes them (`archive::run_purge_once`).
//!
//! The route runs [`run`] on a task of its own, so the request timeout or a
//! client going away can't stop it halfway.

use std::collections::{HashMap, HashSet};

use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::remap::{Refs, Remap, references, remap_refs};
use knot_storage::duplicate::{
    BlobCopy, BoardCopy, DuplicatePlan, PageCopy, apply_duplicate, record_pending_copies,
};
use knot_storage::{
    BlobStoreError, Board, BoardStore, BoardStoreError, DocStoreError, Document, WorkspaceRole,
};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::routes::api::markdown::{current_snapshot, refresh_markdown_from};

/// Pages one duplicate may copy, the top page included.
pub const MAX_DUPLICATE_PAGES: usize = 200;
/// Bytes of files one duplicate may copy, by their recorded sizes.
pub const MAX_DUPLICATE_BLOB_BYTES: i64 = 200 * 1024 * 1024;

/// Why [`run`] made no copy.
#[derive(Debug, thiserror::Error)]
pub enum DuplicateError {
    /// The user can't edit the page, or can't create pages (P15).
    #[error("editor required")]
    EditorRequired,
    /// No such page in the user's workspace.
    #[error("not found")]
    NotFound,
    #[error("the page is a template")]
    IsTemplate,
    #[error("the page is in the archive")]
    Archived,
    /// Over [`MAX_DUPLICATE_PAGES`] or [`MAX_DUPLICATE_BLOB_BYTES`].
    #[error("too large")]
    TooLarge,
    /// While it was copied, the page was moved or archived, or a file it
    /// shows was purged or is missing from the store. Trying again can
    /// succeed once the page is back or the file is fixed.
    #[error("the page changed while it was copied")]
    Conflict,
    /// The copy's order key was taken by a writer outside placement, such
    /// as an older release during an upgrade. Trying again can succeed.
    #[error("sort key conflict")]
    SortKeyConflict,
    #[error("internal: {0}")]
    Internal(String),
}

fn internal(e: impl std::fmt::Display) -> DuplicateError {
    DuplicateError::Internal(e.to_string())
}

/// One page of the copy while it is prepared.
struct Page {
    new_id: Uuid,
    parent: Option<Uuid>,
    title: String,
    icon: Option<String>,
    /// Its current content, transient. Remapped in place before it is
    /// stored.
    content: DocHandle,
    refs: Refs,
}

/// One board of the copy while it is prepared.
struct PlannedBoard {
    source: Board,
    new_id: Uuid,
    new_doc_id: Uuid,
}

/// Copy `source_id` as `ctx`'s user, whose role on the page is `doc_role`,
/// right after it, with its live subpages when `include_subpages`. Returns
/// the copy of the page.
pub async fn run(
    state: AppState,
    ctx: AuthContext,
    doc_role: WorkspaceRole,
    source_id: Uuid,
    include_subpages: bool,
) -> Result<Document, DuplicateError> {
    // P15: the copy is a new page, so the workspace role counts too.
    if doc_role == WorkspaceRole::Viewer || ctx.role == WorkspaceRole::Viewer {
        return Err(DuplicateError::EditorRequired);
    }
    let not_configured = |what: &str| internal(format!("{what} not configured"));
    let docs = state.docs.as_ref().ok_or_else(|| not_configured("docs"))?;
    let pool = state.pool.as_ref().ok_or_else(|| not_configured("pool"))?;
    let blob_store = state
        .blob_store
        .as_ref()
        .ok_or_else(|| not_configured("blob_store"))?;
    let board_store = state
        .boards
        .as_deref()
        .ok_or_else(|| not_configured("boards"))?;
    let ws = ctx.workspace_id;

    let source = match docs.get(source_id).await.map_err(internal)? {
        Some(doc) if doc.workspace_id == ws => doc,
        _ => return Err(DuplicateError::NotFound),
    };
    if source.is_template {
        return Err(DuplicateError::IsTemplate);
    }
    if source.archived_at.is_some() {
        return Err(DuplicateError::Archived);
    }
    let subpages = if include_subpages {
        docs.live_subtree(ws, source_id).await.map_err(internal)?
    } else {
        Vec::new()
    };
    if 1 + subpages.len() > MAX_DUPLICATE_PAGES {
        return Err(DuplicateError::TooLarge);
    }

    let page_ids: HashMap<Uuid, Uuid> = std::iter::once(source_id)
        .chain(subpages.iter().map(|row| row.id))
        .map(|id| (id, Uuid::new_v4()))
        .collect();
    // The top page goes under the source's parent; every subpage under the
    // copy of its parent, which comes before it.
    let outline = std::iter::once((
        source_id,
        source.parent_id,
        format!("{} (copy)", source.title),
        source.icon.clone(),
    ))
    .chain(
        subpages
            .into_iter()
            .map(|row| (row.id, Some(page_ids[&row.parent_id]), row.title, row.icon)),
    );
    let mut pages = Vec::with_capacity(page_ids.len());
    for (id, parent, title, icon) in outline {
        let (content, _seq) = current_snapshot(&state, id).await.map_err(internal)?;
        let refs = references(content.inner());
        pages.push(Page {
            new_id: page_ids[&id],
            parent,
            title,
            icon,
            content,
            refs,
        });
    }

    let boards = choose_boards(&state, board_store, &ctx, &pages, &page_ids).await?;
    let bytes_in_db = blob_store.bytes_deleted_with_metadata();
    let blobs = choose_blobs(&state, &ctx, &pages, &page_ids, bytes_in_db).await?;
    let bytes = blobs
        .iter()
        .fold(0i64, |sum, blob| sum.saturating_add(blob.source.byte_size));
    if bytes > MAX_DUPLICATE_BLOB_BYTES {
        return Err(DuplicateError::TooLarge);
    }

    // Objects outside the database: recorded first, so that a copy whose
    // answer was lost is still cleaned up (P16).
    if !bytes_in_db && !blobs.is_empty() {
        let new_ids: Vec<Uuid> = blobs.iter().map(|blob| blob.new_id).collect();
        record_pending_copies(pool, &new_ids)
            .await
            .map_err(internal)?;
        for blob in &blobs {
            blob_store
                .copy(blob.source.id, blob.new_id, &blob.source.content_type)
                .await
                .map_err(|e| match e {
                    // The file's object is gone. Keeping the reference would
                    // leave the copy depending on the original's file.
                    BlobStoreError::NotFound => DuplicateError::Conflict,
                    e => internal(e),
                })?;
        }
    }

    let remap = Remap {
        docs: page_ids,
        boards: boards.iter().map(|b| (b.source.id, b.new_id)).collect(),
        blobs: blobs.iter().map(|b| (b.source.id, b.new_id)).collect(),
    };
    let engine = YrsEngine;
    let mut copies = Vec::with_capacity(pages.len());
    for page in &pages {
        remap_refs(page.content.inner(), &remap);
        copies.push(PageCopy {
            new_id: page.new_id,
            parent: page.parent,
            title: page.title.clone(),
            icon: page.icon.clone(),
            state: engine
                .encode_state_as_update(&page.content, None)
                .map_err(internal)?,
        });
    }
    let mut board_copies = Vec::with_capacity(boards.len());
    for board in boards {
        board_copies.push(BoardCopy {
            new_id: board.new_id,
            new_doc_id: board.new_doc_id,
            state: board_state(board_store, board.source.id).await?,
            svg: board_store
                .get_svg(board.source.id)
                .await
                .map_err(internal)?,
            label: board.source.label,
        });
    }
    let mut copies = copies.into_iter();
    let plan = DuplicatePlan {
        ws,
        actor: ctx.user_id,
        source_id,
        top: copies.next().expect("the source is the first page"),
        pages: copies.collect(),
        boards: board_copies,
        blobs,
    };
    // On failure nothing is deleted here: the purge job removes the copied
    // objects (P16).
    let outcome = apply_duplicate(pool, &plan).await.map_err(|e| match e {
        DocStoreError::DuplicateConflict => DuplicateError::Conflict,
        DocStoreError::Conflict => DuplicateError::SortKeyConflict,
        e => internal(e),
    })?;
    tracing::info!(
        %source_id,
        copy_id = %outcome.top.id,
        pages = outcome.pages.len(),
        boards = plan.boards.len(),
        files = plan.blobs.len(),
        "duplicate"
    );

    // Best effort: a copy whose refresh fails has no cached Markdown, and
    // the refresh sweep renders it and indexes its tasks later.
    let contents: HashMap<Uuid, &DocHandle> = pages
        .iter()
        .map(|page| (page.new_id, &page.content))
        .collect();
    for (doc_id, seq) in &outcome.pages {
        let Some(content) = contents.get(doc_id) else {
            continue;
        };
        if let Err(e) = refresh_markdown_from(&state, *doc_id, content, *seq).await {
            tracing::warn!(error = ?e, %doc_id, "duplicate: refresh of the copy failed");
        }
    }
    Ok(outcome.top)
}

/// Whether `doc_id` is a page of `ctx`'s workspace that its user can read:
/// the rule the page export applies to the files it embeds (P19).
async fn readable(
    state: &AppState,
    ctx: &AuthContext,
    doc_id: Uuid,
) -> Result<bool, DuplicateError> {
    let acl = state
        .acl
        .as_ref()
        .ok_or_else(|| internal("acl not configured"))?;
    Ok(acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
        .map_err(internal)?
        .is_some())
}

/// The boards the pages show that `ctx`'s user can read, once each. Each
/// copy belongs to the copy of the board's own page when that page is
/// copied too, else to the copy of the first page that shows it.
async fn choose_boards(
    state: &AppState,
    store: &dyn BoardStore,
    ctx: &AuthContext,
    pages: &[Page],
    page_ids: &HashMap<Uuid, Uuid>,
) -> Result<Vec<PlannedBoard>, DuplicateError> {
    let mut seen = HashSet::new();
    let mut chosen = Vec::new();
    for page in pages {
        for id in sorted(&page.refs.boards) {
            if !seen.insert(id) {
                continue;
            }
            let board = match store.get(id).await {
                Ok(board) => board,
                // Deleted, or never was: the reference stays.
                Err(BoardStoreError::NotFound) => continue,
                Err(e) => return Err(internal(e)),
            };
            if !readable(state, ctx, board.doc_id).await? {
                continue;
            }
            chosen.push(PlannedBoard {
                new_id: Uuid::new_v4(),
                new_doc_id: page_ids.get(&board.doc_id).copied().unwrap_or(page.new_id),
                source: board,
            });
        }
    }
    Ok(chosen)
}

/// The files the pages show that belong to `ctx`'s workspace and whose page
/// the user can read, once each, given to pages as in [`choose_boards`].
async fn choose_blobs(
    state: &AppState,
    ctx: &AuthContext,
    pages: &[Page],
    page_ids: &HashMap<Uuid, Uuid>,
    bytes_in_db: bool,
) -> Result<Vec<BlobCopy>, DuplicateError> {
    let meta = state
        .blob_meta
        .as_ref()
        .ok_or_else(|| internal("blob_meta not configured"))?;
    let mut seen = HashSet::new();
    let mut chosen = Vec::new();
    for page in pages {
        for id in sorted(&page.refs.blobs) {
            if !seen.insert(id) {
                continue;
            }
            let Some(source) = meta.find(id).await.map_err(internal)? else {
                continue;
            };
            if source.workspace_id != ctx.workspace_id
                || !readable(state, ctx, source.doc_id).await?
            {
                continue;
            }
            chosen.push(BlobCopy {
                new_id: Uuid::new_v4(),
                new_doc_id: page_ids.get(&source.doc_id).copied().unwrap_or(page.new_id),
                source,
                bytes_in_db,
            });
        }
    }
    Ok(chosen)
}

/// `ids` in a fixed order, so that a page's copies are planned alike every
/// time.
fn sorted(ids: &HashSet<Uuid>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = ids.iter().copied().collect();
    ids.sort();
    ids
}

/// A board's content as one update: its latest snapshot and every update,
/// merged as the board room loads them.
async fn board_state(store: &dyn BoardStore, board_id: Uuid) -> Result<Vec<u8>, DuplicateError> {
    let engine = YrsEngine;
    let board = engine.new_doc();
    if let Some((_seq, snapshot)) = store.latest_snapshot(board_id).await.map_err(internal)? {
        engine.apply_update(&board, &snapshot).map_err(internal)?;
    }
    for update in store.load_updates(board_id).await.map_err(internal)? {
        // The board room leaves out an update it can't apply, so the
        // original shows the board without it; so does the copy.
        if let Err(e) = engine.apply_update(&board, &update) {
            tracing::warn!(error = %e, %board_id, "duplicate: board update left out");
        }
    }
    engine
        .encode_state_as_update(&board, None)
        .map_err(internal)
}
