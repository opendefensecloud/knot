//! Workspace export + import (Plan 32, v1).
//!
//! Export: GET /api/workspace/export → zip with markdown bodies, an
//! index.json manifest, attachment blob bytes, and cached board SVGs.
//!
//! Import: POST /api/workspace/import → multipart zip upload that creates
//! new docs (with remapped ids) in the caller's workspace and seeds each
//! doc's content from markdown.
//!
//! v1 scope intentionally omits per-board Yjs state. Imported boards
//! arrive empty with the exported SVG as their preview; reopening the
//! board modal lets the user keep editing from there.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zip::{ZipWriter, write::SimpleFileOptions};

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/workspace/export", get(export_workspace))
        .route("/api/workspace/import", post(import))
        .route("/api/docs/:doc_id/export", get(export_doc))
}

#[derive(Debug, Deserialize)]
struct ExportDocQuery {
    /// When true, include all descendants of `doc_id` (subtree export).
    /// When false (default), export only the doc itself.
    #[serde(default)]
    descendants: bool,
}

#[derive(Debug, Deserialize)]
struct ImportQuery {
    /// Optional target parent. Imported root docs (manifest entries with
    /// `parent_id = None`) are placed under this parent instead of the
    /// workspace root. The caller still needs editor+ on that parent;
    /// owner-only on the workspace otherwise applies.
    #[serde(default)]
    parent_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

const MANIFEST_VERSION: &str = "1";

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    knot_export_version: String,
    docs: Vec<DocEntry>,
    attachments: Vec<AttachmentEntry>,
    boards: Vec<BoardEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DocEntry {
    id: String,
    parent_id: Option<String>,
    title: String,
    sort_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AttachmentEntry {
    id: String,
    doc_id: String,
    content_type: String,
    original_name: Option<String>,
    byte_size: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct BoardEntry {
    id: String,
    doc_id: String,
    label: Option<String>,
    has_svg: bool,
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

async fn export_workspace(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    // Owner-only — exports contain everyone's content; treating it as a
    // member-readable endpoint would leak across grants.
    use knot_storage::WorkspaceRole;
    let Some(workspaces) = state.workspaces.clone() else { return internal(); };
    match workspaces.get_member_role(ctx.workspace_id, ctx.user_id).await {
        Ok(Some(WorkspaceRole::Owner)) => {}
        _ => return json_err(StatusCode::FORBIDDEN, "acl.owner_required", ""),
    }
    let Some(docs) = state.docs.clone() else { return internal(); };
    let all_docs = match docs.list_alive(ctx.workspace_id).await {
        Ok(d) => d,
        Err(_) => return internal(),
    };
    write_export_zip(&state, ctx.workspace_id, all_docs, /*reparent_roots=*/ false).await
}

/// Single-doc export. With `descendants=true`, includes every descendant
/// of `doc_id` in the workspace tree. Either way the root doc(s) of the
/// returned zip have their `parent_id` cleared so an import grafts the
/// subtree under whatever target the caller specifies.
async fn export_doc(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ExportDocQuery>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(acl) = state.acl.clone() else { return internal(); };
    // Reader on the root is enough to export it; subtree export inherits the
    // same per-doc ACL via filtering below.
    match acl.effective_role(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(_) => return internal(),
    }
    let Some(docs) = state.docs.clone() else { return internal(); };
    let all_docs = match docs.list_alive(ctx.workspace_id).await {
        Ok(d) => d,
        Err(_) => return internal(),
    };
    let subset: Vec<_> = if q.descendants {
        let ids = collect_subtree_ids(doc_id, &all_docs);
        // Per-descendant ACL re-check. Parent-readable + child-private is a
        // legal grant in knot today, and we don't want subtree export to
        // leak the child. Skip descendants the caller can't read; the
        // tree shape gets pruned at the lowest readable ancestor.
        let mut keep: Vec<knot_storage::Document> = Vec::new();
        for d in all_docs.into_iter().filter(|d| ids.contains(&d.id)) {
            if d.id == doc_id {
                keep.push(d);
                continue;
            }
            match acl.effective_role(ctx.workspace_id, d.id, ctx.user_id).await {
                Ok(Some(_)) => keep.push(d),
                _ => continue,
            }
        }
        keep
    } else {
        all_docs.into_iter().filter(|d| d.id == doc_id).collect()
    };
    if subset.is_empty() {
        return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
    }
    write_export_zip(&state, ctx.workspace_id, subset, /*reparent_roots=*/ true).await
}

/// BFS collect of `root` + descendants from the flat doc list. Single O(N)
/// pass via a parent → children index, then a queue starting from `root`.
fn collect_subtree_ids(root: Uuid, all: &[knot_storage::Document]) -> std::collections::HashSet<Uuid> {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut children: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for d in all {
        if let Some(p) = d.parent_id {
            children.entry(p).or_default().push(d.id);
        }
    }
    let mut out = HashSet::<Uuid>::new();
    let mut q = VecDeque::<Uuid>::new();
    out.insert(root);
    q.push_back(root);
    while let Some(id) = q.pop_front() {
        if let Some(kids) = children.get(&id) {
            for k in kids {
                if out.insert(*k) {
                    q.push_back(*k);
                }
            }
        }
    }
    out
}

async fn write_export_zip(
    state: &AppState,
    workspace_id: Uuid,
    all_docs: Vec<knot_storage::Document>,
    reparent_roots: bool,
) -> Response {
    let Some(cache) = state.markdown_cache.clone() else { return internal(); };
    let Some(blob_meta) = state.blob_meta.clone() else { return internal(); };
    let Some(blob_store) = state.blob_store.clone() else { return internal(); };
    let Some(boards) = state.boards.clone() else { return internal(); };

    // For subtree/single exports, manifest parent_ids that don't point at a
    // doc inside the subset become None so import grafts cleanly under the
    // import's target parent.
    let included_ids: std::collections::HashSet<Uuid> =
        all_docs.iter().map(|d| d.id).collect();
    let resolve_parent = |d: &knot_storage::Document| -> Option<String> {
        match d.parent_id {
            Some(p) if !reparent_roots || included_ids.contains(&p) => Some(p.to_string()),
            _ => None,
        }
    };

    // index.json — captures tree + lookup tables.
    let mut manifest = Manifest {
        knot_export_version: MANIFEST_VERSION.into(),
        docs: Vec::with_capacity(all_docs.len()),
        attachments: Vec::new(),
        boards: Vec::new(),
    };

    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut zip = ZipWriter::new(cursor);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        for d in &all_docs {
            manifest.docs.push(DocEntry {
                id: d.id.to_string(),
                parent_id: resolve_parent(d),
                title: d.title.clone(),
                sort_key: d.sort_key.clone(),
            });
            // Markdown body — best-effort from the cache. Missing cache entry
            // means the doc has never been exported; we skip the body rather
            // than triggering live re-render to keep the export quick. The
            // user can hit the markdown endpoint once to populate it.
            //
            // Rewrite sentinels to local zip paths so the exported markdown
            // renders correctly in any plain markdown viewer (Obsidian,
            // VSCode preview, GitHub, etc.). The import step reverses the
            // rewrite back to live sentinels with the new ids.
            let md_raw = cache
                .get(d.id)
                .await
                .ok()
                .flatten()
                .map(|c| c.markdown_text)
                .unwrap_or_default();
            let md = rewrite_for_export(&md_raw);
            if zip.start_file(format!("docs/{}.md", d.id), opts).is_err()
                || zip.write_all(md.as_bytes()).is_err()
            {
                return internal();
            }
            // Board metadata + SVG per doc.
            if let Ok(board_list) = boards.list_for_doc(d.id).await {
                for b in board_list {
                    let svg_bytes = boards.get_svg(b.id).await.ok().flatten();
                    manifest.boards.push(BoardEntry {
                        id: b.id.to_string(),
                        doc_id: d.id.to_string(),
                        label: b.label.clone(),
                        has_svg: svg_bytes.is_some(),
                    });
                    if let Some(svg) = svg_bytes
                        && zip.start_file(format!("boards/{}.svg", b.id), opts).is_ok()
                    {
                        let _ = zip.write_all(&svg);
                    }
                }
            }
        }

        // Attachments — list every blob in the workspace and filter to
        // those that belong to docs included in the export. Empty workspace
        // is fine: the loop just produces an empty list and we end up with
        // a zip containing only `index.json`.
        if let Ok(metas) = blob_meta.list_for_workspace(workspace_id).await {
            for m in metas.into_iter().filter(|m| included_ids.contains(&m.doc_id)) {
                manifest.attachments.push(AttachmentEntry {
                    id: m.id.to_string(),
                    doc_id: m.doc_id.to_string(),
                    content_type: m.content_type.clone(),
                    original_name: m.original_name.clone(),
                    byte_size: m.byte_size,
                });
                if let Ok(bytes) = blob_store.get(m.id).await
                    && zip.start_file(format!("attachments/{}", m.id), opts).is_ok()
                {
                    let _ = zip.write_all(&bytes);
                }
            }
        }

        // Manifest last so it can include everything we've captured.
        let manifest_json = match serde_json::to_vec_pretty(&manifest) {
            Ok(v) => v,
            Err(_) => return internal(),
        };
        if zip.start_file("index.json", opts).is_err()
            || zip.write_all(&manifest_json).is_err()
        {
            return internal();
        }
        if zip.finish().is_err() {
            return internal();
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            r#"attachment; filename="knot-workspace-export.zip""#,
        )
        .body(Body::from(buf))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

async fn import(
    State(state): State<AppState>,
    Query(q): Query<ImportQuery>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    use knot_storage::WorkspaceRole;
    let Some(workspaces) = state.workspaces.clone() else { return internal(); };
    let Some(acl) = state.acl.clone() else { return internal(); };
    // ACL: with no parent_id, owner on the workspace is required.
    // With a parent_id, editor+ on that parent is sufficient.
    match q.parent_id {
        None => {
            match workspaces
                .get_member_role(ctx.workspace_id, ctx.user_id)
                .await
            {
                Ok(Some(WorkspaceRole::Owner)) => {}
                _ => return json_err(StatusCode::FORBIDDEN, "acl.owner_required", ""),
            }
        }
        Some(parent) => {
            match acl.effective_role(ctx.workspace_id, parent, ctx.user_id).await {
                Ok(Some(WorkspaceRole::Owner | WorkspaceRole::Editor)) => {}
                _ => return json_err(StatusCode::FORBIDDEN, "acl.editor_required", ""),
            }
        }
    }
    let Some(docs) = state.docs.clone() else { return internal(); };
    let Some(blob_meta) = state.blob_meta.clone() else { return internal(); };
    let Some(blob_store) = state.blob_store.clone() else { return internal(); };
    let Some(boards) = state.boards.clone() else { return internal(); };
    let Some(rooms) = state.rooms_v2.clone() else { return internal(); };

    // Read body. Hard cap at 50 MB for v1.
    let bytes: Bytes = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let cursor = Cursor::new(bytes.as_ref());
    let mut zip = match zip::ZipArchive::new(cursor) {
        Ok(z) => z,
        Err(_) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, "import.not_zip", ""),
    };

    // 1. Read manifest.
    let manifest: Manifest = match read_zip_entry(&mut zip, "index.json") {
        Some(bytes) => match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(_) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, "import.bad_manifest", ""),
        },
        None => return json_err(StatusCode::UNPROCESSABLE_ENTITY, "import.no_manifest", ""),
    };
    if manifest.knot_export_version != MANIFEST_VERSION {
        return json_err(StatusCode::UNPROCESSABLE_ENTITY, "import.version_mismatch", "");
    }

    // 2. ID remap tables — populated as we create new records below.
    let mut doc_remap: HashMap<String, Uuid> = HashMap::new();
    let mut blob_remap: HashMap<String, Uuid> = HashMap::new();
    let mut board_remap: HashMap<String, Uuid> = HashMap::new();

    // 3. Create doc records in tree order (parent before child).
    //    Topological sort by parent_id chain.
    let docs_sorted = sort_docs_by_depth(&manifest.docs);
    for d in &docs_sorted {
        // Reject malformed doc ids before letting them flow through the
        // remap. They'd be harmless (the doc_remap key is never used as a
        // filesystem path on import — only as an interned manifest key),
        // but defense in depth.
        if Uuid::parse_str(&d.id).is_err() { continue; }
        // Roots of the import (no parent_id, or parent_id not in the
        // manifest) get grafted under the caller's target parent, if any.
        let new_parent = d
            .parent_id
            .as_ref()
            .and_then(|p| doc_remap.get(p).copied())
            .or(q.parent_id);
        match docs
            .create(
                ctx.workspace_id,
                new_parent,
                &d.title,
                &d.sort_key,
                ctx.user_id,
            )
            .await
        {
            Ok(created) => {
                doc_remap.insert(d.id.clone(), created.id);
            }
            Err(e) => {
                tracing::warn!(error=?e, old_id=?d.id, "import: create doc failed");
            }
        }
    }

    // 4. Import attachments. New ids, but doc_id remapped to the new doc.
    for a in &manifest.attachments {
        let new_id = Uuid::new_v4();
        let Some(new_doc_id) = doc_remap.get(&a.doc_id).copied() else { continue };
        // The manifest id must be a real UUID — anything else means
        // either a malformed zip or a path-confusion attempt. Skip rather
        // than feed unvalidated text into a zip lookup.
        if Uuid::parse_str(&a.id).is_err() { continue; }
        let Some(bytes_vec) = read_zip_entry(&mut zip, &format!("attachments/{}", a.id)) else {
            continue;
        };
        // Trust the BYTES, not the manifest. Recompute sha + size from the
        // unzipped payload, and pick a content-type that respects an
        // allowlist (defaults to application/octet-stream for anything
        // unfamiliar — prevents the manifest from declaring text/html for
        // a JS file, etc.).
        use sha2::{Digest, Sha256};
        let sha = Sha256::digest(&bytes_vec).to_vec();
        let content_type = sanitize_content_type(&a.content_type);
        let meta = knot_storage::BlobMetadata {
            id: new_id,
            workspace_id: ctx.workspace_id,
            doc_id: new_doc_id,
            content_type: content_type.clone(),
            byte_size: bytes_vec.len() as i64,
            sha256: sha,
            original_name: a.original_name.clone(),
            created_by: ctx.user_id,
            created_at: chrono::Utc::now(),
        };
        if blob_meta.insert(&meta).await.is_err() { continue; }
        if blob_store
            .put(new_id, &bytes_vec, &content_type)
            .await
            .is_err()
        {
            let _ = blob_meta.delete(new_id).await;
            continue;
        }
        blob_remap.insert(a.id.clone(), new_id);
    }

    // 5. Import boards. New ids; SVG preserved if present; Yjs state is
    //    NOT seeded in v1, so the board's content history is fresh.
    for b in &manifest.boards {
        let Some(new_doc_id) = doc_remap.get(&b.doc_id).copied() else { continue };
        if Uuid::parse_str(&b.id).is_err() { continue; }
        let created = match boards
            .create(new_doc_id, ctx.user_id, b.label.clone())
            .await
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        board_remap.insert(b.id.clone(), created.id);
        if b.has_svg
            && let Some(svg) = read_zip_entry(&mut zip, &format!("boards/{}.svg", b.id))
            && boards.set_svg(created.id, &svg).await.is_err()
        {
            tracing::warn!(old_id=?b.id, "import: set_svg failed");
        }
    }

    // 6. For each doc, read markdown, rewrite knot:// + /api/blobs ids,
    //    parse to a Y-update, push via the room actor.
    for d in &manifest.docs {
        let Some(new_doc_id) = doc_remap.get(&d.id).copied() else { continue };
        let Some(md_bytes) = read_zip_entry(&mut zip, &format!("docs/{}.md", d.id)) else { continue };
        let md = match std::str::from_utf8(&md_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => continue,
        };
        let rewritten = remap_sentinels(&md, &doc_remap, &blob_remap, &board_remap);
        if rewritten.trim().is_empty() {
            continue;
        }
        let update_bytes = match knot_markdown::from_markdown::parse(&rewritten) {
            Ok((_doc, bytes)) => bytes,
            Err(_) => continue,
        };
        let room = rooms.acquire(new_doc_id).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = room
            .tx
            .send(knot_crdt::Event::ReplaceWithMarkdown {
                update_bytes,
                reply: tx,
            })
            .await;
        if matches!(rx.await, Ok(Ok(_))) {
            // Best-effort: kick the indexer so /tasks reflects the imported
            // tree without waiting for someone to hit each markdown export.
            let _ = super::markdown::refresh_markdown_and_index(&state, new_doc_id).await;
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                // Actual count of docs that landed, not what the manifest
                // claimed — partial failures are surfaced rather than
                // hidden behind a cheerful number.
                "imported_docs": doc_remap.len(),
                "imported_attachments": blob_remap.len(),
                "imported_boards": board_remap.len(),
            }))
                .unwrap_or_default(),
        ))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Hard per-entry decompression cap to defend against zip-bombs. The
/// compressed body is already capped at 50 MiB (see import handler), so
/// any individual entry above this limit is almost certainly malicious.
const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;

fn read_zip_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    let entry = zip.by_name(name).ok()?;
    // Reject entries that DECLARE absurd decompressed sizes outright —
    // their `size()` is attacker-controlled but lets us short-circuit
    // before allocating anything. The take() limit below catches lies.
    if entry.size() > MAX_ENTRY_BYTES {
        return None;
    }
    let cap = (entry.size() as usize).min(MAX_ENTRY_BYTES as usize);
    let mut out = Vec::with_capacity(cap);
    let mut limited = entry.take(MAX_ENTRY_BYTES);
    limited.read_to_end(&mut out).ok()?;
    Some(out)
}

/// Topological sort by parent_id. Inputs without a known parent (or with a
/// non-workspace parent) end up as roots.
fn sort_docs_by_depth(entries: &[DocEntry]) -> Vec<DocEntry> {
    let mut by_id: HashMap<&str, &DocEntry> = HashMap::new();
    for e in entries {
        by_id.insert(e.id.as_str(), e);
    }
    let mut depth = HashMap::<String, usize>::new();
    fn compute<'a>(
        id: &str,
        by_id: &HashMap<&'a str, &'a DocEntry>,
        depth: &mut HashMap<String, usize>,
    ) -> usize {
        if let Some(d) = depth.get(id) { return *d; }
        let d = match by_id
            .get(id)
            .and_then(|e| e.parent_id.as_ref())
            .filter(|p| by_id.contains_key(p.as_str()))
        {
            Some(p) => compute(p.as_str(), by_id, depth) + 1,
            None => 0,
        };
        depth.insert(id.to_string(), d);
        d
    }
    for e in entries {
        compute(&e.id, &by_id, &mut depth);
    }
    let mut out: Vec<DocEntry> = entries
        .iter()
        .map(|e| DocEntry {
            id: e.id.clone(),
            parent_id: e.parent_id.clone(),
            title: e.title.clone(),
            sort_key: e.sort_key.clone(),
        })
        .collect();
    out.sort_by_key(|e| *depth.get(&e.id).unwrap_or(&0));
    out
}

/// Map a URL classified by pulldown as a link/image destination to a
/// zip-relative path. Returns `None` for URLs that aren't one of our three
/// sentinel shapes (external links pass through unchanged).
fn url_to_local_path(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("knot://doc/") {
        let id = rest.split(['?', '#', '/']).next().unwrap_or(rest);
        if Uuid::parse_str(id).is_ok() {
            return Some(format!("docs/{id}.md"));
        }
    }
    if let Some(rest) = url.strip_prefix("knot://board/")
        && let Some(id) = rest.strip_suffix(".svg")
        && Uuid::parse_str(id).is_ok()
    {
        return Some(format!("boards/{id}.svg"));
    }
    if let Some(rest) = url.strip_prefix("/api/blobs/") {
        let id = rest.split(['?', '#', '/']).next().unwrap_or(rest);
        if Uuid::parse_str(id).is_ok() {
            return Some(format!("attachments/{id}"));
        }
    }
    None
}

/// Inverse mapping used by import. Takes a path the export wrote and the
/// id-remap tables; returns the live URL using the freshly-assigned id, or
/// `None` when the path isn't a known shape or the old id isn't in the
/// remap (meaning the referenced resource wasn't in this zip).
fn local_path_to_url(
    url: &str,
    doc_remap: &HashMap<String, Uuid>,
    blob_remap: &HashMap<String, Uuid>,
    board_remap: &HashMap<String, Uuid>,
) -> Option<String> {
    if let Some(rest) = url.strip_prefix("docs/")
        && let Some(old) = rest.strip_suffix(".md")
        && let Some(new) = doc_remap.get(old)
    {
        return Some(format!("knot://doc/{new}"));
    }
    if let Some(rest) = url.strip_prefix("boards/")
        && let Some(old) = rest.strip_suffix(".svg")
        && let Some(new) = board_remap.get(old)
    {
        return Some(format!("knot://board/{new}.svg"));
    }
    if let Some(rest) = url.strip_prefix("attachments/")
        && let Some(new) = blob_remap.get(rest)
    {
        return Some(format!("/api/blobs/{new}"));
    }
    None
}

/// Walk `md` via pulldown's offset iterator and produce a copy with every
/// Link/Image destination URL rewritten via `map_url`. URLs that `map_url`
/// returns `None` for are left untouched. Other markdown text is preserved
/// byte-for-byte — we only edit the byte ranges pulldown attributed to
/// link/image destinations.
fn rewrite_link_urls<F: FnMut(&str) -> Option<String>>(md: &str, mut map_url: F) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);

    // Collect (byte_start, byte_end, replacement) for each URL we touch.
    // Apply in reverse so earlier indices stay valid.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (event, range) in Parser::new_ext(md, opts).into_offset_iter() {
        let url: Option<&str> = match &event {
            Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.as_ref()),
            Event::Start(Tag::Image { dest_url, .. }) => Some(dest_url.as_ref()),
            _ => None,
        };
        let Some(url) = url else { continue };
        let Some(replacement) = map_url(url) else { continue };
        // Locate the URL substring inside the event's source range. Search
        // from the range's start to anchor; pulldown's reported URL is the
        // exact string we expect to find inside this span.
        let span = &md[range.clone()];
        if let Some(off) = span.find(url) {
            let start = range.start + off;
            let end = start + url.len();
            edits.push((start, end, replacement));
        }
    }
    if edits.is_empty() { return md.to_string(); }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = md.to_string();
    for (start, end, repl) in edits {
        out.replace_range(start..end, &repl);
    }
    out
}

/// Rewrite live sentinel + blob URLs to zip-relative paths so the exported
/// markdown renders cleanly in any plain markdown viewer.
///
///   knot://doc/<uuid>          → docs/<uuid>.md
///   knot://board/<uuid>.svg    → boards/<uuid>.svg
///   /api/blobs/<uuid>          → attachments/<uuid>
fn rewrite_for_export(md: &str) -> String {
    rewrite_link_urls(md, url_to_local_path)
}

/// Inverse of `rewrite_for_export`, applying the id remap so imported
/// references point at the freshly-created records.
fn remap_sentinels(
    md: &str,
    doc_remap: &HashMap<String, Uuid>,
    blob_remap: &HashMap<String, Uuid>,
    board_remap: &HashMap<String, Uuid>,
) -> String {
    rewrite_link_urls(md, |u| local_path_to_url(u, doc_remap, blob_remap, board_remap))
}

/// Conservative content-type allowlist for imported attachments. Any
/// type not on this list collapses to `application/octet-stream` so a
/// manifest can't trick the server into serving `text/html` (XSS) or
/// `image/svg+xml` (script-in-SVG) for arbitrary uploaded bytes. The
/// real defense for SVG is the public-share path (which is fine — it's
/// just bytes through CORS-friendly headers); this clamp protects the
/// authenticated `/api/blobs/:id` path.
fn sanitize_content_type(declared: &str) -> String {
    // Strip parameters (`text/plain; charset=utf-8` → `text/plain`).
    let base = declared.split(';').next().unwrap_or("").trim().to_lowercase();
    const ALLOWED: &[&str] = &[
        "image/png", "image/jpeg", "image/gif", "image/webp",
        "application/pdf",
        "text/plain", "text/markdown", "text/csv",
        "application/json",
        "application/zip",
    ];
    if ALLOWED.contains(&base.as_str()) {
        base
    } else {
        "application/octet-stream".to_string()
    }
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_rewrite_handles_all_three_sentinel_shapes() {
        let doc = Uuid::new_v4();
        let board = Uuid::new_v4();
        let blob = Uuid::new_v4();
        let md = format!(
            "See [other](knot://doc/{doc}) and ![diag](knot://board/{board}.svg) and ![pic](/api/blobs/{blob})\n"
        );
        let out = rewrite_for_export(&md);
        assert!(out.contains(&format!("docs/{doc}.md")));
        assert!(out.contains(&format!("boards/{board}.svg")));
        assert!(out.contains(&format!("attachments/{blob}")));
        assert!(!out.contains("knot://"));
        assert!(!out.contains("/api/blobs/"));
    }

    #[test]
    fn import_remap_local_paths_to_new_sentinels() {
        let old_doc = Uuid::new_v4();
        let new_doc = Uuid::new_v4();
        let mut doc_remap = HashMap::new();
        doc_remap.insert(old_doc.to_string(), new_doc);
        let md = format!("[link](docs/{old_doc}.md)\n");
        let out = remap_sentinels(&md, &doc_remap, &HashMap::new(), &HashMap::new());
        assert!(out.contains(&format!("knot://doc/{new_doc}")));
        assert!(!out.contains(&format!("docs/{old_doc}.md")));
    }

    #[test]
    fn import_leaves_unknown_local_paths_untouched() {
        // A local-path URL for an id that isn't in the remap means the
        // referenced resource wasn't bundled with this zip. The walker
        // should leave it as-is rather than producing a broken sentinel.
        let md = "[ghost](docs/00000000-0000-0000-0000-000000000000.md)\n";
        let out = remap_sentinels(md, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(out, md);
    }

    #[test]
    fn rewrite_link_urls_handles_link_inside_table_cell() {
        let doc = Uuid::new_v4();
        let md = format!(
            "| col |\n| --- |\n| [other](knot://doc/{doc}) |\n"
        );
        let out = rewrite_for_export(&md);
        assert!(out.contains(&format!("docs/{doc}.md")));
        assert!(!out.contains("knot://"));
    }

    #[test]
    fn rewrite_link_urls_handles_link_inside_nested_list() {
        let doc = Uuid::new_v4();
        let md = format!(
            "- outer\n  - inner [link](knot://doc/{doc})\n    - deeper\n"
        );
        let out = rewrite_for_export(&md);
        assert!(out.contains(&format!("docs/{doc}.md")));
    }

    #[test]
    fn rewrite_link_urls_handles_link_inside_blockquote() {
        let doc = Uuid::new_v4();
        let md = format!("> see [other](knot://doc/{doc})\n");
        let out = rewrite_for_export(&md);
        assert!(out.contains(&format!("docs/{doc}.md")));
    }

    #[test]
    fn rewrite_leaves_user_mentions_alone() {
        // `knot://user/<uuid>` is the mention sentinel — not a doc/board/
        // blob ref. The export's rewrite must pass it through unchanged so
        // the import can re-link assignees by user_id.
        let uid = Uuid::new_v4();
        let md = format!("- [ ] [@Alice](knot://user/{uid}) Buy milk\n");
        let out = rewrite_for_export(&md);
        assert_eq!(out, md);
    }

    #[test]
    fn rewrite_skips_reference_style_links_silently() {
        // Reference-style links have the URL defined elsewhere — pulldown
        // reports the resolved dest_url to our mapper, but the byte range
        // for the EVENT itself doesn't contain the URL text, so the
        // `find()` in rewrite_link_urls misses and the link is left
        // unchanged. Pin this behaviour so future readers know.
        let doc = Uuid::new_v4();
        let md = format!("[link][ref]\n\n[ref]: knot://doc/{doc}\n");
        let out = rewrite_for_export(&md);
        // The URL is NOT rewritten — would need a parser-level approach
        // that walks ref definitions too. Document the limitation here.
        assert!(out.contains(&format!("knot://doc/{doc}")));
        assert!(!out.contains(&format!("docs/{doc}.md")));
    }

    #[test]
    fn rewrite_ignores_uuid_lookalikes_in_prose() {
        // A literal sentinel string outside a link/image context (e.g. in
        // prose or code) must NOT be rewritten — only URLs pulldown
        // classifies as link/image destinations are touched.
        let id = Uuid::new_v4();
        let md = format!(
            "Discussing the value `knot://doc/{id}` inline.\n\n```\nknot://doc/{id}\n```\n"
        );
        let out = rewrite_for_export(&md);
        assert_eq!(out, md);
    }

    #[test]
    fn export_then_import_roundtrips_through_remap() {
        let old_doc = Uuid::new_v4();
        let new_doc = Uuid::new_v4();
        let mut doc_remap = HashMap::new();
        doc_remap.insert(old_doc.to_string(), new_doc);

        let original = format!("[other](knot://doc/{old_doc})\n");
        let exported = rewrite_for_export(&original);
        let imported = remap_sentinels(&exported, &doc_remap, &HashMap::new(), &HashMap::new());
        assert_eq!(imported, format!("[other](knot://doc/{new_doc})\n"));
    }
}
