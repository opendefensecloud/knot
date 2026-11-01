//! Duplicate a page, and with `include_subpages` its live subpages
//! (`POST /api/docs/{id}/duplicate`, spec D).
//!
//! Everything is prepared before the one transaction that stores the copy
//! (`knot_storage::duplicate::apply_duplicate`):
//! - the content of every page, copied from its current state without
//!   booting a room (P18), with its references to the copied pages, boards
//!   and files pointed at their copies (P14);
//! - only the subpages the user can read, and none below one they can't
//!   (restricted documents). A copy of a restricted page is restricted too,
//!   and the user gets `owner` on it;
//! - the boards and files those pages show, when the user can read the page
//!   they belong to (P19). Each gets a copy of its own, so the copy never
//!   depends on the original, unless its page is not copied and lies in
//!   another restriction boundary than the copy: a copy would let other
//!   people see it. Other references stay as they are;
//! - the objects of files kept outside the database, copied before the
//!   transaction, once their ids are recorded in `blob_copy_pending` (P16).
//!   Nothing is deleted on the request path: if the transaction doesn't
//!   commit, the hourly purge job removes them (`archive::run_purge_once`).
//!
//! The route runs a duplicate through [`start`], on a task of its own, so
//! the request timeout or a client going away can't stop it halfway. The
//! answer comes as soon as the copy is stored; the refresh of the copy's
//! derived data ([`Refresh`]) runs after it, on the same task. The task
//! holds one of [`MAX_CONCURRENT_DUPLICATES`] permits until both end, since
//! it keeps every copied page in memory until then.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use axum::response::Response;
use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_docs::{DocAccess, Visibility};
use knot_markdown::remap::{Refs, Remap, references, remap_refs};
use knot_storage::duplicate::{
    BlobCopy, BoardCopy, DuplicatePlan, PageCopy, apply_duplicate, record_pending_copies,
};
use knot_storage::{
    BlobStoreError, Board, BoardStore, BoardStoreError, DocStoreError, Document, SubtreeRow,
    WorkspaceRole,
};
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tracing::Instrument;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::routes::api::markdown::{current_snapshot, refresh_markdown_from};

/// Pages one duplicate may copy, the top page included.
pub const MAX_DUPLICATE_PAGES: usize = 200;
/// Bytes of files one duplicate may copy, by their recorded sizes.
pub const MAX_DUPLICATE_BLOB_BYTES: i64 = 200 * 1024 * 1024;
/// Duplicates one server process runs at a time. Each holds every page it
/// copies in memory until the refresh of its copy ends.
pub const MAX_CONCURRENT_DUPLICATES: usize = 2;
/// How long a duplicate waits for a free slot before it is refused as busy.
pub const BUSY_WAIT: Duration = Duration::from_secs(5);

/// Why [`run`] made no copy.
#[derive(Debug, thiserror::Error)]
pub enum DuplicateError {
    /// The user can't edit the page, or can't create pages (P15).
    #[error("editor required")]
    EditorRequired,
    /// No such page in the user's workspace.
    #[error("not found")]
    NotFound,
    /// The user can't put a page under the source's parent, where the copy
    /// goes: the answer of `restricted::require_editable_parent` (404, 403
    /// or 409), passed on as it is.
    #[error("the source's parent refuses the copy")]
    Destination(Response),
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
    /// Its page is a restriction boundary, and so is the copy.
    restricted: bool,
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

/// A stored copy.
#[derive(Debug)]
pub struct Copied {
    /// The copy of the page.
    pub top: Document,
    /// What is left to do for it.
    pub refresh: Refresh,
}

/// The refresh of a stored copy's derived data: the cached Markdown and the
/// task rows of every page of the copy.
pub struct Refresh {
    /// Each page of the copy, its content and the seq of the update that
    /// holds it.
    pages: Vec<(Uuid, DocHandle, i64)>,
}

impl Refresh {
    /// Best effort: a copy whose refresh fails has no cached Markdown, and
    /// the refresh sweep renders it and indexes its tasks later.
    pub async fn run(self, state: &AppState) {
        for (doc_id, content, seq) in self.pages {
            if let Err(e) = refresh_markdown_from(state, doc_id, &content, seq).await {
                tracing::warn!(error = ?e, %doc_id, "duplicate: refresh of the copy failed");
            }
        }
    }
}

impl fmt::Debug for Refresh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Refresh")
            .field("pages", &self.pages.len())
            .finish()
    }
}

/// Runs [`run`] on a task of its own, and then the [`Refresh`] of its copy,
/// holding `permit` until both end. The returned receiver gets the answer
/// as soon as the copy is stored or refused; dropping it (the request timed
/// out, or the client went away) stops neither.
pub fn start(
    state: AppState,
    ctx: AuthContext,
    doc_role: WorkspaceRole,
    source_id: Uuid,
    include_subpages: bool,
    permit: OwnedSemaphorePermit,
) -> oneshot::Receiver<Result<Document, DuplicateError>> {
    let (answer, answered) = oneshot::channel();
    tokio::spawn(
        async move {
            match run(state.clone(), ctx, doc_role, source_id, include_subpages).await {
                Ok(Copied { top, refresh }) => {
                    let _ = answer.send(Ok(top));
                    refresh.run(&state).await;
                }
                Err(e) => {
                    let _ = answer.send(Err(e));
                }
            }
            drop(permit);
        }
        .in_current_span(),
    );
    answered
}

/// Whether `ctx`'s user, whose role on the page is `doc_role`, may copy
/// `source_id`: the user's roles (P15), and a live page of their workspace
/// that is no template. Returns the page. None of it needs a permit, so the
/// route checks it before it waits for one: a viewer or a template gets its
/// own answer while the server is busy. [`run`] checks it again.
pub async fn check(
    state: &AppState,
    ctx: &AuthContext,
    doc_role: WorkspaceRole,
    source_id: Uuid,
) -> Result<Document, DuplicateError> {
    if doc_role == WorkspaceRole::Viewer {
        return Err(DuplicateError::EditorRequired);
    }
    let docs = state
        .docs
        .as_ref()
        .ok_or_else(|| internal("docs not configured"))?;
    let source = match docs.get(source_id).await.map_err(internal)? {
        Some(doc) if doc.workspace_id == ctx.workspace_id => doc,
        _ => return Err(DuplicateError::NotFound),
    };
    // P15: the copy is a new page next to the source. Under a parent, the
    // user must be able to edit there; at the top level, the workspace role
    // counts.
    match source.parent_id {
        Some(parent_id) => crate::restricted::require_editable_parent(state, ctx, parent_id)
            .await
            .map_err(DuplicateError::Destination)?,
        None if ctx.role == WorkspaceRole::Viewer => return Err(DuplicateError::EditorRequired),
        None => {}
    }
    if source.is_template {
        return Err(DuplicateError::IsTemplate);
    }
    if source.archived_at.is_some() {
        return Err(DuplicateError::Archived);
    }
    Ok(source)
}

/// Copy `source_id` as `ctx`'s user, whose role on the page is `doc_role`,
/// right after it, with its live subpages when `include_subpages`. Returns
/// once the copy is stored, with the refresh of its derived data still to
/// run.
pub async fn run(
    state: AppState,
    ctx: AuthContext,
    doc_role: WorkspaceRole,
    source_id: Uuid,
    include_subpages: bool,
) -> Result<Copied, DuplicateError> {
    let source = check(&state, &ctx, doc_role, source_id).await?;
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
    let acl = state.acl.as_ref().ok_or_else(|| not_configured("acl"))?;
    let ws = ctx.workspace_id;

    let live_subpages = if include_subpages {
        docs.live_subtree(ws, source_id).await.map_err(internal)?
    } else {
        Vec::new()
    };
    let listed_ids: Vec<Uuid> = source
        .parent_id
        .into_iter()
        .chain(live_subpages.iter().map(|row| row.id))
        .collect();
    let visibility = acl
        .visibility_covering(ws, ctx.user_id, &listed_ids)
        .await
        .map_err(internal)?;
    let subpages = readable_subpages(live_subpages, &visibility);
    // The restriction boundary the copy goes into, as its parent's.
    let destination_root = source
        .parent_id
        .and_then(|parent_id| visibility.access(parent_id))
        .and_then(|parent_access| parent_access.restriction_root);
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
        source.restricted,
    ))
    .chain(subpages.into_iter().map(|row| {
        (
            row.id,
            Some(page_ids[&row.parent_id]),
            row.title,
            row.icon,
            row.restricted,
        )
    }));
    let mut pages = Vec::with_capacity(page_ids.len());
    for (id, parent, title, icon, restricted) in outline {
        let (content, _seq) = current_snapshot(&state, id).await.map_err(internal)?;
        let refs = references(content.inner());
        pages.push(Page {
            new_id: page_ids[&id],
            parent,
            title,
            icon,
            restricted,
            content,
            refs,
        });
    }

    let copy = CopyTargets {
        pages: &pages,
        page_ids: &page_ids,
        destination_root,
    };
    let boards = choose_boards(&state, board_store, &ctx, &copy).await?;
    let bytes_in_db = blob_store.bytes_deleted_with_metadata();
    let blobs = choose_blobs(&state, &ctx, &copy, bytes_in_db).await?;
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
            restricted: page.restricted,
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
    // Copies of restricted pages already carry the user's `owner` grant.
    // The top copy can still be out of their reach when their access to its
    // parent came from a grant that doesn't inherit.
    if crate::restricted::ensure_creator_can_edit(&state, ws, outcome.top.id, ctx.user_id)
        .await
        .is_err()
    {
        return Err(internal("creator owner grant"));
    }

    let mut contents: HashMap<Uuid, DocHandle> = pages
        .into_iter()
        .map(|page| (page.new_id, page.content))
        .collect();
    let refresh = Refresh {
        pages: outcome
            .pages
            .iter()
            .filter_map(|(doc_id, seq)| {
                let content = contents.remove(doc_id)?;
                Some((*doc_id, content, *seq))
            })
            .collect(),
    };
    Ok(Copied {
        top: outcome.top,
        refresh,
    })
}

/// The live subpages, from `live_subtree`, that the user can read, in their
/// order. One below a page left out is left out too, even when the user can
/// read it: its parent has no copy to go under. The rows are depth first, so
/// a parent is decided before its children.
fn readable_subpages(rows: Vec<SubtreeRow>, visibility: &Visibility) -> Vec<SubtreeRow> {
    let mut left_out: HashSet<Uuid> = HashSet::new();
    rows.into_iter()
        .filter(|row| {
            if left_out.contains(&row.parent_id) || !visibility.readable(row.id) {
                left_out.insert(row.id);
                return false;
            }
            true
        })
        .collect()
}

/// `ctx`'s user's access to `doc_id`, a page of their workspace, when they
/// can read it: the rule the page export applies to the files it embeds
/// (P19).
async fn readable_access(
    state: &AppState,
    ctx: &AuthContext,
    doc_id: Uuid,
) -> Result<Option<DocAccess>, DuplicateError> {
    let acl = state
        .acl
        .as_ref()
        .ok_or_else(|| internal("acl not configured"))?;
    Ok(acl
        .access(ctx.workspace_id, doc_id, ctx.user_id)
        .await
        .map_err(internal)?
        .filter(|access| access.can_read()))
}

/// The pages of a copy, as [`choose_boards`] and [`choose_blobs`] give them
/// boards and files.
struct CopyTargets<'a> {
    pages: &'a [Page],
    /// Each copied page's copy.
    page_ids: &'a HashMap<Uuid, Uuid>,
    /// The restriction boundary the copy goes into (`None`: none).
    destination_root: Option<Uuid>,
}

impl CopyTargets<'_> {
    /// The page of the copy that gets the copy of a board or file of
    /// `owner_doc`, which `showing_page` shows: the copy of `owner_doc`
    /// when it is copied too, else `showing_page`. `None` when its page is
    /// not copied and it or the copy lies in a restricted subtree: a copy
    /// could let people who can't read its page see it. The reference
    /// stays then. Even one shared boundary is not enough, because inside
    /// it readers differ by branch, through their grants.
    fn owner_of_copy(
        &self,
        owner_doc: Uuid,
        owner_access: DocAccess,
        showing_page: &Page,
    ) -> Option<Uuid> {
        match self.page_ids.get(&owner_doc) {
            Some(copied_owner) => Some(*copied_owner),
            None if owner_access.restriction_root.is_some() || self.destination_root.is_some() => {
                None
            }
            None => Some(showing_page.new_id),
        }
    }
}

/// The boards the pages show that `ctx`'s user can read, once each. Each
/// copy belongs to the copy of the board's own page when that page is
/// copied too, else to the copy of the first page that shows it (see
/// [`CopyTargets::owner_of_copy`]).
async fn choose_boards(
    state: &AppState,
    store: &dyn BoardStore,
    ctx: &AuthContext,
    copy: &CopyTargets<'_>,
) -> Result<Vec<PlannedBoard>, DuplicateError> {
    let mut seen = HashSet::new();
    let mut chosen = Vec::new();
    for page in copy.pages {
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
            let Some(owner_access) = readable_access(state, ctx, board.doc_id).await? else {
                continue;
            };
            let Some(new_doc_id) = copy.owner_of_copy(board.doc_id, owner_access, page) else {
                continue;
            };
            chosen.push(PlannedBoard {
                new_id: Uuid::new_v4(),
                new_doc_id,
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
    copy: &CopyTargets<'_>,
    bytes_in_db: bool,
) -> Result<Vec<BlobCopy>, DuplicateError> {
    let meta = state
        .blob_meta
        .as_ref()
        .ok_or_else(|| internal("blob_meta not configured"))?;
    let mut seen = HashSet::new();
    let mut chosen = Vec::new();
    for page in copy.pages {
        for id in sorted(&page.refs.blobs) {
            if !seen.insert(id) {
                continue;
            }
            let Some(source) = meta.find(id).await.map_err(internal)? else {
                continue;
            };
            if source.workspace_id != ctx.workspace_id {
                continue;
            }
            let Some(owner_access) = readable_access(state, ctx, source.doc_id).await? else {
                continue;
            };
            let Some(new_doc_id) = copy.owner_of_copy(source.doc_id, owner_access, page) else {
                continue;
            };
            chosen.push(BlobCopy {
                new_id: Uuid::new_v4(),
                new_doc_id,
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
