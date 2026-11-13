//! Workspace export + import (Plan 32, v1).
//!
//! Export: GET /api/workspace/export → zip with markdown bodies, an
//! index.json manifest, attachment blob bytes, and cached board SVGs.
//! GET /api/docs/{doc_id}/export?descendants= is the same zip of one page,
//! or of it and the readable pages below it. The zip is built in memory,
//! then recorded per included page as `doc.export`, and only then sent:
//! exports fail closed, so a GET whose rows can't be written is refused
//! with `503 audit.unavailable` and no zip. HEAD sends no zip, so it is
//! neither recorded nor refused.
//!
//! Import: POST /api/workspace/import → multipart zip upload that creates
//! new docs (with remapped ids) in the caller's workspace and seeds each
//! doc's content from markdown.
//!
//! v1 scope intentionally omits per-board Yjs state. Imported boards
//! arrive empty with the exported SVG as their preview; reopening the
//! board modal lets the user keep editing from there.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{Method, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use knot_docs::Visibility;
use knot_storage::Placement;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zip::{ZipWriter, write::SimpleFileOptions};

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::{audit_unavailable, json_err, json_err_with_details};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/workspace/export", get(export_workspace))
        .route("/api/workspace/import", post(import))
        .route("/api/docs/{doc_id}/export", get(export_doc))
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
    /// workspace root. It must be a page of the caller's workspace (else
    /// 404 `doc.not_found`), and the caller still needs editor+ on it;
    /// owner-only on the workspace otherwise applies.
    #[serde(default)]
    parent_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

/// Manifest version 2 (Plan 32, revised): file paths inside the zip
/// are human-readable (slugified titles/labels/names), and the manifest
/// is the sole source of truth for UUID ↔ path mapping. Version 1
/// hard-coded `<uuid>.<ext>` filenames and is intentionally not
/// imported (project guidance: no backwards compat for the export
/// format pre-1.0).
const MANIFEST_VERSION: &str = "2";

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
    /// The doc's place among its siblings. Import keeps the order these
    /// keys give but writes fresh keys.
    sort_key: String,
    /// Path inside the zip that holds this doc's markdown body.
    /// E.g. `docs/Meeting-notes.md`.
    path: String,
    /// Import creates this doc restricted, with the importer as its owner.
    /// Set on restriction boundaries, and on a root of the export that sat
    /// inside a restricted subtree, so the copy is no more visible than the
    /// original was. Absent from older exports.
    #[serde(default)]
    restricted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct AttachmentEntry {
    id: String,
    doc_id: String,
    content_type: String,
    original_name: Option<String>,
    byte_size: i64,
    /// Path inside the zip that holds the attachment bytes.
    /// E.g. `attachments/photo.png`. Derived from `original_name`
    /// when set, otherwise from the attachment id.
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BoardEntry {
    id: String,
    doc_id: String,
    label: Option<String>,
    has_svg: bool,
    /// Path inside the zip that holds the rendered SVG, when
    /// `has_svg`. E.g. `boards/Architecture.svg`. None when no SVG
    /// was cached for the board at export time.
    path: Option<String>,
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

async fn export_workspace(State(state): State<AppState>, req: Request) -> Response {
    let is_get = req.method() == Method::GET;
    let api_key_id = super::audit::api_key_id_of(&req);
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    // Owner-only — exports contain everyone's content; treating it as a
    // member-readable endpoint would leak across grants.
    use knot_storage::WorkspaceRole;
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    match workspaces
        .get_member_role(ctx.workspace_id, ctx.user_id)
        .await
    {
        Ok(Some(WorkspaceRole::Owner)) => {}
        _ => return json_err(StatusCode::FORBIDDEN, "acl.owner_required", ""),
    }
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let all_docs = match docs.list_alive(ctx.workspace_id).await {
        Ok(d) => d,
        Err(_) => return internal(),
    };
    // Owning the workspace doesn't open restricted pages: only the pages the
    // owner can read go into the export.
    let all_ids: Vec<Uuid> = all_docs.iter().map(|d| d.id).collect();
    let visibility = match super::docs::visibility_covering_ids(&state, &ctx, &all_ids).await {
        Ok(visibility) => visibility,
        Err(response) => return response,
    };
    let readable_docs: Vec<_> = all_docs
        .into_iter()
        .filter(|d| visibility.readable(d.id))
        .collect();
    let exported_doc_ids: Vec<Uuid> = readable_docs.iter().map(|doc| doc.id).collect();
    let response = write_export_zip(&state, ctx.workspace_id, readable_docs, &visibility).await;
    if is_get
        && response.status() == StatusCode::OK
        && let Err(refusal) = record_zip_export(
            &state,
            &ctx,
            api_key_id,
            &exported_doc_ids,
            serde_json::json!({"scope": "workspace"}),
        )
        .await
    {
        return refusal;
    }
    response
}

/// Single-doc export. With `descendants=true`, includes every descendant
/// of `doc_id` in the workspace tree that the caller can read. Either way
/// the root doc(s) of the returned zip have their `parent_id` cleared so an
/// import grafts the subtree under whatever target the caller specifies.
async fn export_doc(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ExportDocQuery>,
    req: Request,
) -> Response {
    let is_get = req.method() == Method::GET;
    let api_key_id = super::audit::api_key_id_of(&req);
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(acl) = state.acl.clone() else {
        return internal();
    };
    // Reader on the root is enough to export it; subtree export inherits the
    // same per-doc ACL via filtering below.
    match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(_) => return internal(),
    }
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let all_docs = match docs.list_alive(ctx.workspace_id).await {
        Ok(d) => d,
        Err(_) => return internal(),
    };
    let subtree_ids = if q.descendants {
        collect_subtree_ids(doc_id, &all_docs)
    } else {
        HashSet::from([doc_id])
    };
    let subset_ids: Vec<Uuid> = subtree_ids.iter().copied().collect();
    let visibility = match super::docs::visibility_covering_ids(&state, &ctx, &subset_ids).await {
        Ok(visibility) => visibility,
        Err(response) => return response,
    };
    // Per-descendant ACL re-check: a subpage can be restricted, or the
    // caller's grant may not inherit, and subtree export must not leak it.
    // The root passed the check above; skip descendants the caller can't
    // read.
    let subset: Vec<_> = all_docs
        .into_iter()
        .filter(|d| subtree_ids.contains(&d.id))
        .filter(|d| d.id == doc_id || visibility.readable(d.id))
        .collect();
    if subset.is_empty() {
        return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
    }
    // The pages in the file: `subset`, never `subtree_ids`, which still
    // holds the pages the caller can't read.
    let exported_doc_ids: Vec<Uuid> = subset.iter().map(|doc| doc.id).collect();
    let response = write_export_zip(&state, ctx.workspace_id, subset, &visibility).await;
    if is_get && response.status() == StatusCode::OK {
        let scope_fields = if q.descendants {
            serde_json::json!({"scope": "subtree", "root_id": doc_id})
        } else {
            serde_json::json!({"scope": "page"})
        };
        if let Err(refusal) =
            record_zip_export(&state, &ctx, api_key_id, &exported_doc_ids, scope_fields).await
        {
            return refusal;
        }
    }
    response
}

/// Records a built Markdown zip in the audit log before it is sent: one
/// `doc.export` row per page in it, all sharing one `export_id`.
/// `scope_fields` (`scope`, and `root_id` for a subtree) gain `format`,
/// `export_id`, `doc_count` and, for an export made with an API key,
/// `api_key_id`; the last three are for operators, the audit log never
/// sends them. Fail closed: when the rows can't be written (or there
/// is no database to write them to) the refusal to send instead of the zip
/// is the `Err`, `503 audit.unavailable`. So is an insert that hangs: it is
/// given up on after `audit::EXPORT_AUDIT_WRITE_TIMEOUT`, well inside the
/// request timeout. Only a zip that took nearly that long to build can
/// still end in 408 during the insert; rows that commit just as it fires
/// are for a zip the client never got, which errs on the safe side.
async fn record_zip_export(
    state: &AppState,
    ctx: &AuthContext,
    api_key_id: Option<Uuid>,
    exported_doc_ids: &[Uuid],
    scope_fields: serde_json::Value,
) -> Result<(), Response> {
    let Some(pool) = state.pool.as_ref() else {
        tracing::error!("export refused: no database to record it in");
        return Err(audit_unavailable());
    };
    let mut audit_data = super::audit::with_api_key_id(scope_fields, api_key_id);
    if let Some(fields) = audit_data.as_object_mut() {
        fields.insert("format".into(), "markdown_zip".into());
        fields.insert("export_id".into(), Uuid::new_v4().to_string().into());
        fields.insert("doc_count".into(), exported_doc_ids.len().into());
    }
    knot_storage::audit::record_for_targets(
        pool,
        ctx.workspace_id,
        Some(ctx.user_id),
        "doc.export",
        "doc",
        exported_doc_ids,
        &audit_data,
    )
    .await
    .map_err(|_| audit_unavailable())
}

/// BFS collect of `root` + descendants from the flat doc list. Single O(N)
/// pass via a parent → children index, then a queue starting from `root`.
fn collect_subtree_ids(
    root: Uuid,
    all: &[knot_storage::Document],
) -> std::collections::HashSet<Uuid> {
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

/// The zip of `all_docs`, each of which the caller can read (`visibility`
/// is theirs).
async fn write_export_zip(
    state: &AppState,
    workspace_id: Uuid,
    all_docs: Vec<knot_storage::Document>,
    visibility: &Visibility,
) -> Response {
    let Some(cache) = state.markdown_cache.clone() else {
        return internal();
    };
    let Some(blob_meta) = state.blob_meta.clone() else {
        return internal();
    };
    let Some(blob_store) = state.blob_store.clone() else {
        return internal();
    };
    let Some(boards) = state.boards.clone() else {
        return internal();
    };

    // Manifest parent_ids that don't point at a doc inside the export become
    // None, so import grafts cleanly under the import's target parent, and a
    // parent the caller can't read is never named.
    let included_ids: std::collections::HashSet<Uuid> = all_docs.iter().map(|d| d.id).collect();
    let resolve_parent = |d: &knot_storage::Document| -> Option<Uuid> {
        d.parent_id.filter(|p| included_ids.contains(p))
    };
    // A root of the export that sat inside a restricted subtree is restricted
    // on import too; below a root, the root's flag (or a doc's own) covers it.
    let restricted_on_import = |d: &knot_storage::Document| -> bool {
        let within_restricted = visibility
            .access(d.id)
            .is_some_and(|access| access.restriction_root.is_some());
        d.restricted || (resolve_parent(d).is_none() && within_restricted)
    };

    // index.json — captures tree + lookup tables.
    let mut manifest = Manifest {
        knot_export_version: MANIFEST_VERSION.into(),
        docs: Vec::with_capacity(all_docs.len()),
        attachments: Vec::new(),
        boards: Vec::new(),
    };

    // Pre-pass: assign every doc/board/attachment a human-readable
    // path inside the zip. Collisions are resolved with a numeric
    // suffix. Hold the maps so the markdown rewrite below points its
    // sentinels at the same paths we'll write the files to.
    let mut used_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    used_paths.insert("index.json".to_string());

    let mut doc_path_for: std::collections::HashMap<Uuid, String> =
        std::collections::HashMap::new();
    let mut board_path_for: std::collections::HashMap<Uuid, String> =
        std::collections::HashMap::new();
    let mut blob_path_for: std::collections::HashMap<Uuid, String> =
        std::collections::HashMap::new();

    for d in &all_docs {
        let stem = slugify(&d.title);
        let path = unique_path(&mut used_paths, "docs", &stem, ".md");
        doc_path_for.insert(d.id, path);
    }
    // Resolve board paths up-front — we need them in the map before we
    // start writing markdown so sentinel rewrites land correctly.
    let mut board_lists: HashMap<Uuid, Vec<knot_storage::Board>> = HashMap::new();
    for d in &all_docs {
        if let Ok(list) = boards.list_for_doc(d.id).await {
            for b in &list {
                let stem = slugify(b.label.as_deref().unwrap_or("Diagram"));
                let path = unique_path(&mut used_paths, "boards", &stem, ".svg");
                board_path_for.insert(b.id, path);
            }
            board_lists.insert(d.id, list);
        }
    }
    // Same for attachments — use original_name when present.
    let attachment_metas: Vec<knot_storage::BlobMetadata> = blob_meta
        .list_for_workspace(workspace_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|m| included_ids.contains(&m.doc_id))
        .collect();
    for m in &attachment_metas {
        let (stem, ext) = match m.original_name.as_deref() {
            Some(name) => split_stem_ext(name),
            None => (m.id.to_string(), String::new()),
        };
        let stem = slugify(&stem);
        let path = unique_path(&mut used_paths, "attachments", &stem, &ext);
        blob_path_for.insert(m.id, path);
    }

    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut zip = ZipWriter::new(cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        for d in &all_docs {
            let doc_path = doc_path_for.get(&d.id).cloned().unwrap_or_default();
            manifest.docs.push(DocEntry {
                id: d.id.to_string(),
                parent_id: resolve_parent(d).map(|p| p.to_string()),
                title: d.title.clone(),
                sort_key: d.sort_key.clone(),
                path: doc_path.clone(),
                restricted: restricted_on_import(d),
            });
            // Markdown body — best-effort from the cache. Missing cache entry
            // means the doc has never been exported; we skip the body rather
            // than triggering live re-render to keep the export quick. The
            // user can hit the markdown endpoint once to populate it.
            //
            // Rewrite sentinels to the human-readable zip paths so the
            // exported markdown renders correctly in any plain markdown
            // viewer (Obsidian, VSCode preview, GitHub, etc.). The import
            // step reverses the rewrite back to live sentinels using the
            // manifest's id ↔ path mapping.
            let md_raw = cache
                .get(d.id)
                .await
                .ok()
                .flatten()
                .map(|c| c.markdown_text)
                .unwrap_or_default();
            let md = rewrite_for_export(&md_raw, &doc_path_for, &board_path_for, &blob_path_for);
            if zip.start_file(&doc_path, opts).is_err() || zip.write_all(md.as_bytes()).is_err() {
                return internal();
            }
            if let Some(board_list) = board_lists.get(&d.id) {
                for b in board_list {
                    let svg_bytes = boards.get_svg(b.id).await.ok().flatten();
                    let path = board_path_for.get(&b.id).cloned();
                    manifest.boards.push(BoardEntry {
                        id: b.id.to_string(),
                        doc_id: d.id.to_string(),
                        label: b.label.clone(),
                        has_svg: svg_bytes.is_some(),
                        path: svg_bytes.as_ref().and(path.clone()),
                    });
                    if let (Some(svg), Some(path)) = (svg_bytes, path)
                        && zip.start_file(&path, opts).is_ok()
                    {
                        let _ = zip.write_all(&svg);
                    }
                }
            }
        }

        for m in &attachment_metas {
            let path = blob_path_for.get(&m.id).cloned().unwrap_or_default();
            manifest.attachments.push(AttachmentEntry {
                id: m.id.to_string(),
                doc_id: m.doc_id.to_string(),
                content_type: m.content_type.clone(),
                original_name: m.original_name.clone(),
                byte_size: m.byte_size,
                path: path.clone(),
            });
            if let Ok(bytes) = blob_store.get(m.id).await
                && zip.start_file(&path, opts).is_ok()
            {
                let _ = zip.write_all(&bytes);
            }
        }

        // Manifest last so it can include everything we've captured.
        let manifest_json = match serde_json::to_vec_pretty(&manifest) {
            Ok(v) => v,
            Err(_) => return internal(),
        };
        if zip.start_file("index.json", opts).is_err() || zip.write_all(&manifest_json).is_err() {
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
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    // ACL: with no parent_id, owner on the workspace is required.
    // With a parent_id, editor+ on that parent is sufficient. As for create
    // and move, a parent in another workspace, or one the caller can't
    // read, is refused like an id no page has.
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
            if let Err(response) =
                crate::restricted::require_editable_parent(&state, &ctx, parent).await
            {
                return response;
            }
        }
    }
    // Policy: a bot adds no page inside a restricted subtree, so refuse before
    // anything is created.
    if let Some(response) =
        crate::bots::deny_bot_restricted_destination(&state, &ctx, q.parent_id).await
    {
        return response;
    }
    let Some(blob_meta) = state.blob_meta.clone() else {
        return internal();
    };
    let Some(blob_store) = state.blob_store.clone() else {
        return internal();
    };
    let Some(boards) = state.boards.clone() else {
        return internal();
    };
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };

    // Read body. Hard cap at 50 MB for v1.
    let bytes: Bytes = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let mut zip = match open_import_zip(&bytes) {
        Ok(z) => z,
        Err(code) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, code, ""),
    };
    let manifest = match read_manifest(&mut zip) {
        Ok(m) => m,
        Err(code) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, code, ""),
    };
    // A restricted page is created with an owner grant for the importer.
    if manifest.docs.iter().any(|d| d.restricted) {
        match crate::bots::is_bot(&state, ctx.user_id).await {
            Ok(false) => {}
            Ok(true) => {
                return crate::bots::restricted_forbidden(&ctx, "import restricted page");
            }
            Err(response) => return response,
        }
    }
    // What this import may write, checked before anything is: the body cap
    // bounds compressed bytes only, and the manifest may name one entry any
    // number of times.
    let import_plan = ImportPlan::of(&manifest, &zip);
    if let Err(refusal) = import_plan.enforce_caps() {
        tracing::warn!(
            pages = import_plan.pages,
            bytes = import_plan.total_bytes(),
            references = import_plan.references,
            "import: refused, over the per-import caps"
        );
        return refusal;
    }
    // The quotas, once, for every page and the declared attachment bytes
    // (board SVGs aren't counted): over either, nothing is imported.
    if let Err(response) = crate::quota::enforce_pages(&state, &ctx, import_plan.pages).await {
        return response;
    }
    if let Err(response) =
        crate::quota::enforce_blob_bytes(&state, &ctx, import_plan.blob_bytes).await
    {
        return response;
    }

    // 2. ID remap tables — populated as we create new records below.
    let mut doc_remap: HashMap<String, Uuid> = HashMap::new();
    let mut blob_remap: HashMap<String, Uuid> = HashMap::new();
    let mut board_remap: HashMap<String, Uuid> = HashMap::new();

    // Reverse lookup tables: zip path → original UUID string. Built up
    // front from the manifest so the sentinel rewriter can map markdown
    // references like `docs/Meeting-notes.md` back to a `knot://doc/`
    // URL using the freshly-assigned id.
    let mut path_to_doc: HashMap<String, String> = HashMap::new();
    let mut path_to_blob: HashMap<String, String> = HashMap::new();
    let mut path_to_board: HashMap<String, String> = HashMap::new();
    for d in &manifest.docs {
        path_to_doc.insert(d.path.clone(), d.id.clone());
    }
    for a in &manifest.attachments {
        path_to_blob.insert(a.path.clone(), a.id.clone());
    }
    for b in &manifest.boards {
        if let Some(p) = &b.path {
            path_to_board.insert(p.clone(), b.id.clone());
        }
    }

    // 3. Create doc records, parents before children. Each doc goes last
    //    among its new siblings and gets a fresh key there: the manifest's
    //    keys only set the order (see `creation_order`), because they
    //    collide with the pages of an earlier import of the same export.
    let docs_sorted = creation_order(&manifest.docs);
    let mut imported_root_ids: Vec<Uuid> = Vec::new();
    let mut restricted_subtrees = RestrictedSubtrees::default();
    for d in &docs_sorted {
        if !restricted_subtrees.admit(d) {
            tracing::warn!(old_id=?d.id, "import: doc skipped, its restricted subtree lost a page");
            continue;
        }
        // Reject malformed doc ids before letting them flow through the
        // remap. They'd be harmless (the doc_remap key is never used as a
        // filesystem path on import — only as an interned manifest key),
        // but defense in depth.
        if Uuid::parse_str(&d.id).is_err() {
            restricted_subtrees.failed(d);
            continue;
        }
        // Roots of the import (no parent_id, or parent_id not in the
        // manifest) get grafted under the caller's target parent, if any.
        let new_parent = d
            .parent_id
            .as_ref()
            .and_then(|p| doc_remap.get(p).copied())
            .or(q.parent_id);
        let is_import_root = new_parent == q.parent_id;
        // A restricted page is created restricted, in one transaction, so
        // neither its title nor anything later put below it or into it is
        // ever readable to the whole workspace. That makes the importer its
        // owner.
        let created = if d.restricted {
            docs.create_placed_restricted(
                ctx.workspace_id,
                new_parent,
                &d.title,
                Placement::Last,
                ctx.user_id,
            )
            .await
        } else {
            docs.create_placed(
                ctx.workspace_id,
                new_parent,
                &d.title,
                Placement::Last,
                ctx.user_id,
            )
            .await
        };
        match created {
            Ok(created) => {
                doc_remap.insert(d.id.clone(), created.id);
                if is_import_root {
                    imported_root_ids.push(created.id);
                }
                // This replica reads the new boundary right away, not once
                // the invalidation's NOTIFY comes back.
                if d.restricted {
                    crate::archive::evict_local(&state, &[created.id]).await;
                }
            }
            Err(e) => {
                tracing::warn!(error=?e, old_id=?d.id, "import: create doc failed");
                restricted_subtrees.failed(d);
            }
        }
    }
    // Under a parent the importer edits through a grant that doesn't
    // inherit, they couldn't edit what they imported; an owner grant on
    // each root covers the pages below it.
    for root_id in &imported_root_ids {
        if let Err(response) = crate::restricted::ensure_creator_can_edit(
            &state,
            ctx.workspace_id,
            *root_id,
            ctx.user_id,
        )
        .await
        {
            return response;
        }
    }

    // Entries the loops below skip as too large or unreadable, warned
    // about once at the end.
    let mut skipped_entries = 0usize;

    // 4. Import attachments. New ids, but doc_id remapped to the new doc.
    for a in &manifest.attachments {
        let new_id = Uuid::new_v4();
        let Some(new_doc_id) = doc_remap.get(&a.doc_id).copied() else {
            continue;
        };
        // The manifest id must be a real UUID — anything else means
        // either a malformed zip or a path-confusion attempt. Skip rather
        // than feed unvalidated text into a zip lookup.
        if Uuid::parse_str(&a.id).is_err() {
            continue;
        }
        let Some(bytes_vec) = read_import_entry(&mut zip, &a.path, &mut skipped_entries) else {
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
        if blob_meta.insert(&meta).await.is_err() {
            continue;
        }
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
        let Some(new_doc_id) = doc_remap.get(&b.doc_id).copied() else {
            continue;
        };
        if Uuid::parse_str(&b.id).is_err() {
            continue;
        }
        let created = match boards
            .create(new_doc_id, ctx.user_id, b.label.clone())
            .await
        {
            Ok(c) => c,
            Err(_) => continue,
        };
        board_remap.insert(b.id.clone(), created.id);
        if b.has_svg
            && let Some(path) = &b.path
            && let Some(svg) = read_import_entry(&mut zip, path, &mut skipped_entries)
            && boards.set_svg(created.id, &svg).await.is_err()
        {
            tracing::warn!(old_id=?b.id, "import: set_svg failed");
        }
    }

    // 6. For each doc, read markdown, rewrite knot:// + /api/blobs ids,
    //    parse to a Y-update, push via the room actor.
    for d in &manifest.docs {
        let Some(new_doc_id) = doc_remap.get(&d.id).copied() else {
            continue;
        };
        let Some(md_bytes) = read_import_entry(&mut zip, &d.path, &mut skipped_entries) else {
            continue;
        };
        let md = match std::str::from_utf8(&md_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => continue,
        };
        let rewritten = remap_sentinels(
            &md,
            &path_to_doc,
            &path_to_blob,
            &path_to_board,
            &doc_remap,
            &blob_remap,
            &board_remap,
        );
        if rewritten.trim().is_empty() {
            continue;
        }
        let (page, update_bytes) = match knot_markdown::from_markdown::parse(&rewritten) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        // Imported tasks arrive assigned; nobody is notified about them.
        super::markdown::index_copied_tasks(&state, new_doc_id, &page).await;
        let room = match rooms.acquire(new_doc_id).await {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(error=?e, %new_doc_id, "room acquire failed during import");
                continue;
            }
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = room
            .tx
            .send(knot_crdt::Event::ReplaceWithMarkdown {
                update_bytes,
                // The importer put this content into the new page.
                by_user: Some(ctx.user_id),
                source: knot_storage::UpdateSource::Import,
                meta: None,
                reply: tx,
            })
            .await;
        if matches!(rx.await, Ok(Ok(_))) {
            // Best-effort: kick the indexer so /tasks reflects the imported
            // tree without waiting for someone to hit each markdown export.
            let _ =
                super::markdown::refresh_markdown_and_index(&state, new_doc_id, Some(ctx.user_id))
                    .await;
        }
    }

    if skipped_entries > 0 {
        tracing::warn!(
            skipped_entries,
            "import: zip entries skipped, larger than they declare (or than an entry may be) \
             or unreadable"
        );
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

/// Split a filename into (stem, ext-with-dot). "photo.png" → ("photo",
/// ".png"); "Makefile" → ("Makefile", ""). Only the *last* dot counts.
fn split_stem_ext(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            (stem.to_string(), format!(".{ext}"))
        }
        _ => (name.to_string(), String::new()),
    }
}

/// Lower-case, ASCII-safe slug for a zip filename. Replaces anything
/// outside `[A-Za-z0-9._-]` with `-`, collapses runs, trims leading/
/// trailing punctuation, caps length. Empty input → "untitled".
///
/// We accept any path inside the zip on import (per the manifest), so
/// the goal here is purely human-readability — not security.
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = true; // suppress leading dash
    for ch in input.chars() {
        let safe = ch.is_ascii_alphanumeric() || ch == '_' || ch == '.';
        if safe {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with(['-', '.']) {
        out.pop();
    }
    while out.starts_with(['-', '.']) {
        out.remove(0);
    }
    if out.len() > 80 {
        out.truncate(80);
        while out.ends_with(['-', '.']) {
            out.pop();
        }
    }
    if out.is_empty() {
        return "untitled".to_string();
    }
    out
}

/// Resolves "<dir>/<stem><ext>" against an in-progress collision set,
/// appending `-2`, `-3`, ... as needed until the path is unique within
/// the zip. Reserves the chosen path so the next call sees it taken.
fn unique_path(
    used: &mut std::collections::HashSet<String>,
    dir: &str,
    stem: &str,
    ext: &str,
) -> String {
    let mut candidate = format!("{dir}/{stem}{ext}");
    if !used.contains(&candidate) {
        used.insert(candidate.clone());
        return candidate;
    }
    let mut n: u32 = 2;
    loop {
        candidate = format!("{dir}/{stem}-{n}{ext}");
        if !used.contains(&candidate) {
            used.insert(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

/// Hard per-entry decompression cap to defend against zip-bombs. The
/// compressed body is already capped at 50 MiB (see import handler), so
/// any individual entry above this limit is almost certainly malicious.
const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;

/// Most pages one import may create.
const MAX_IMPORT_PAGES: u64 = 10_000;

/// Most attachment and board SVG bytes one import may write, together.
const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024;

/// Most page Markdown bytes one import may read and write into its pages.
/// Each page parses its entry and stores it as CRDT updates, a Markdown
/// cache and search text, so an entry named by many pages costs its size
/// once per page.
const MAX_IMPORT_MARKDOWN_BYTES: u64 = 256 * 1024 * 1024;

/// Most attachment and board references one import may hold, together.
/// Each one costs a zip lookup when planned and a row (and a copy) when
/// imported, whatever bytes it names.
const MAX_IMPORT_REFERENCES: u64 = 20_000;

/// What an import would write, from the manifest and the zip entries'
/// declared sizes, known before the first page is created. The per-import
/// caps are checked against it, and so are the quotas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImportPlan {
    /// Pages in the manifest.
    pub(crate) pages: u64,
    /// Page Markdown bytes: the declared size of the entry behind every
    /// page, summed per page.
    pub(crate) markdown_bytes: u64,
    /// Attachment bytes: the declared size of the entry behind every
    /// attachment reference, summed per reference.
    pub(crate) blob_bytes: u64,
    /// Board SVG bytes, counted the same way.
    pub(crate) board_svg_bytes: u64,
    /// Attachment and board references in the manifest, boards without an
    /// SVG included.
    pub(crate) references: u64,
}

impl ImportPlan {
    /// Counts every page of `manifest`, and for each page's Markdown and each
    /// attachment and board SVG reference the uncompressed size its zip
    /// entry declares, at most `MAX_ENTRY_BYTES` (a larger one is skipped
    /// when read). An entry named twice counts twice, since each reference
    /// writes its own copy. A missing entry counts zero; it is skipped when
    /// read. The manifest's own `byte_size` is never trusted.
    fn of<R: Read + std::io::Seek>(manifest: &Manifest, zip: &zip::ZipArchive<R>) -> Self {
        let markdown_bytes = manifest
            .docs
            .iter()
            .map(|doc| declared_entry_size(zip, &doc.path))
            .fold(0u64, u64::saturating_add);
        let blob_bytes = manifest
            .attachments
            .iter()
            .map(|attachment| declared_entry_size(zip, &attachment.path))
            .fold(0u64, u64::saturating_add);
        let board_svg_bytes = manifest
            .boards
            .iter()
            .filter(|board| board.has_svg)
            .filter_map(|board| board.path.as_deref())
            .map(|path| declared_entry_size(zip, path))
            .fold(0u64, u64::saturating_add);
        Self {
            pages: manifest.docs.len() as u64,
            markdown_bytes,
            blob_bytes,
            board_svg_bytes,
            references: (manifest.attachments.len() as u64)
                .saturating_add(manifest.boards.len() as u64),
        }
    }

    /// Attachment and board SVG bytes together.
    fn total_bytes(&self) -> u64 {
        self.blob_bytes.saturating_add(self.board_svg_bytes)
    }

    /// `413 import.too_large` when the import would create more than
    /// `MAX_IMPORT_PAGES` pages, write more than `MAX_IMPORT_MARKDOWN_BYTES`
    /// of page Markdown or `MAX_IMPORT_BYTES` of files and board images, or
    /// hold more than `MAX_IMPORT_REFERENCES` attachment and board
    /// references.
    #[allow(
        clippy::result_large_err,
        reason = "the Err is the ready axum Response, returned unboxed like the async handlers do"
    )]
    fn enforce_caps(&self) -> Result<(), Response> {
        if self.pages <= MAX_IMPORT_PAGES
            && self.markdown_bytes <= MAX_IMPORT_MARKDOWN_BYTES
            && self.total_bytes() <= MAX_IMPORT_BYTES
            && self.references <= MAX_IMPORT_REFERENCES
        {
            return Ok(());
        }
        Err(json_err_with_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "import.too_large",
            "this import is too large",
            serde_json::json!({
                "pages": self.pages,
                "bytes": self.total_bytes(),
                "markdown_bytes": self.markdown_bytes,
                "max_pages": MAX_IMPORT_PAGES,
                "max_bytes": MAX_IMPORT_BYTES,
                "max_markdown_bytes": MAX_IMPORT_MARKDOWN_BYTES,
                "references": self.references,
                "max_references": MAX_IMPORT_REFERENCES,
            }),
        ))
    }
}

/// The index of the zip entry named exactly `name`. Unlike
/// `ZipArchive::index_for_name`, a miss never falls back to scanning every
/// entry's decoded name (a manifest naming many missing entries would cost
/// references × entries); it only matters for CP437 names, and export
/// paths are ASCII.
fn zip_entry_index<R: Read + std::io::Seek>(zip: &zip::ZipArchive<R>, name: &str) -> Option<usize> {
    zip.index_for_name_raw(name.as_bytes())
}

/// The uncompressed size the zip entry `name` declares, at most
/// `MAX_ENTRY_BYTES`; zero when there is no such entry. Reads only the
/// central directory.
fn declared_entry_size<R: Read + std::io::Seek>(zip: &zip::ZipArchive<R>, name: &str) -> u64 {
    zip_entry_index(zip, name)
        .and_then(|index| zip.by_index_data(index).ok())
        .map_or(0, |entry| entry.size().min(MAX_ENTRY_BYTES))
}

/// Why [`read_zip_entry`] has no bytes for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnreadEntry {
    /// No entry of that name.
    Missing,
    /// It declares more than `MAX_ENTRY_BYTES`.
    DeclaredTooLarge,
    /// It holds more than it declares, or doesn't decompress.
    LargerThanDeclaredOrUnreadable,
}

/// The bytes of the zip entry `name`, or why there are none: there is no
/// such entry, it declares more than `MAX_ENTRY_BYTES`, or it holds more
/// than it declares. The last makes the declared sizes `ImportPlan` checks
/// an upper bound on what import writes.
fn read_zip_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, UnreadEntry> {
    let index = zip_entry_index(zip, name).ok_or(UnreadEntry::Missing)?;
    let entry = zip.by_index(index).map_err(|_| UnreadEntry::Missing)?;
    // Reject entries that DECLARE absurd decompressed sizes outright —
    // their `size()` is attacker-controlled but lets us short-circuit
    // before allocating anything. The take() limit below catches lies.
    let declared_size = entry.size();
    if declared_size > MAX_ENTRY_BYTES {
        return Err(UnreadEntry::DeclaredTooLarge);
    }
    let mut out = Vec::with_capacity(declared_size as usize);
    // One byte past the declared size is enough to tell that it lied.
    let mut limited = entry.take(declared_size + 1);
    if limited.read_to_end(&mut out).is_err() || out.len() as u64 > declared_size {
        return Err(UnreadEntry::LargerThanDeclaredOrUnreadable);
    }
    Ok(out)
}

/// [`read_zip_entry`] for the import loops: `None` for an entry without
/// bytes. One that is too large or unreadable is logged at debug and
/// counted in `skipped_entries`, for one warning per import; a missing one
/// is skipped silently, as before.
fn read_import_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
    skipped_entries: &mut usize,
) -> Option<Vec<u8>> {
    match read_zip_entry(zip, name) {
        Ok(bytes) => Some(bytes),
        Err(UnreadEntry::Missing) => None,
        Err(reason) => {
            tracing::debug!(entry = name, ?reason, "import: zip entry skipped");
            *skipped_entries += 1;
            None
        }
    }
}

/// Which docs of the manifest import may still create, as it goes through
/// them in `creation_order`. A doc whose parent wasn't created is grafted
/// onto the import target, except in a restricted subtree of the manifest:
/// only the boundary carries the flag, so a subpage grafted there would be
/// readable to the whole workspace. Such a subpage is skipped, with
/// everything below it.
#[derive(Default)]
struct RestrictedSubtrees {
    /// Manifest ids of the docs seen so far that are restricted or have a
    /// restricted ancestor in the manifest.
    within_restricted: HashSet<String>,
    /// Manifest ids of the docs in `within_restricted` that weren't created,
    /// and of everything below them.
    dropped: HashSet<String>,
}

impl RestrictedSubtrees {
    /// Notes `entry`, after its parent (if in the manifest) was noted;
    /// `false` when it must be skipped because its parent was dropped.
    fn admit(&mut self, entry: &DocEntry) -> bool {
        let parent = entry.parent_id.as_deref();
        if entry.restricted || parent.is_some_and(|p| self.within_restricted.contains(p)) {
            self.within_restricted.insert(entry.id.clone());
        }
        if parent.is_some_and(|p| self.dropped.contains(p)) {
            self.dropped.insert(entry.id.clone());
            return false;
        }
        true
    }

    /// `entry`, admitted, wasn't created: in a restricted subtree, the
    /// docs below it are skipped.
    fn failed(&mut self, entry: &DocEntry) {
        if self.within_restricted.contains(&entry.id) {
            self.dropped.insert(entry.id.clone());
        }
    }
}

/// The order import creates docs in: by depth in the manifest's tree, so
/// parents come before children, then by `sort_key` byte by byte (the
/// sidebar's order, `COLLATE "C"`), then by manifest index. Each doc is
/// appended to its new siblings, so this order is the order they keep.
/// Inputs without a known parent (or with a non-workspace parent) end up as
/// roots.
///
/// The manifest is untrusted, so its parents may form a cycle. A doc that is
/// its own ancestor comes back without its parent and imports as a root
/// under the import target, the way the sidebar shows such a doc
/// (`buildTree`) and the way `heal_doc_cycles` repaired them. A doc below a
/// cycle keeps its parent.
fn creation_order(entries: &[DocEntry]) -> Vec<DocEntry> {
    #[derive(Clone, Copy)]
    enum Mark {
        /// On the current climb, at this index.
        Climbing(usize),
        /// Depth in the manifest's tree; 0 for a root.
        Depth(usize),
    }
    let mut by_id: HashMap<&str, &DocEntry> = HashMap::new();
    for e in entries {
        by_id.insert(e.id.as_str(), e);
    }
    // The parent of `id`, when it is in the manifest too.
    let parent = |id: &str| {
        by_id
            .get(id)
            .and_then(|e| e.parent_id.as_deref())
            .filter(|p| by_id.contains_key(p))
    };
    let mut marks: HashMap<&str, Mark> = HashMap::new();
    let mut in_cycle: HashSet<&str> = HashSet::new();
    for e in entries {
        // Climb from `e` through its ancestors, marking each, until a doc
        // whose depth is known, a root, or a doc already on this climb (a
        // cycle). A loop, not a recursion: the chain can be deeper than the
        // stack.
        let mut climb: Vec<&str> = Vec::new();
        let mut id = e.id.as_str();
        // The depth of the last doc left on `climb`.
        let top = loop {
            match marks.get(id).copied() {
                Some(Mark::Depth(d)) => break d + 1,
                Some(Mark::Climbing(i)) => {
                    for member in climb.drain(i..) {
                        marks.insert(member, Mark::Depth(0));
                        in_cycle.insert(member);
                    }
                    break 1;
                }
                None => {}
            }
            marks.insert(id, Mark::Climbing(climb.len()));
            climb.push(id);
            match parent(id) {
                Some(p) => id = p,
                None => break 0,
            }
        };
        for (above, id) in climb.into_iter().rev().enumerate() {
            marks.insert(id, Mark::Depth(top + above));
        }
    }
    let depth_of = |e: &DocEntry| match marks.get(e.id.as_str()) {
        Some(Mark::Depth(d)) => *d,
        _ => 0,
    };
    let mut out: Vec<DocEntry> = entries
        .iter()
        .map(|e| DocEntry {
            id: e.id.clone(),
            parent_id: if in_cycle.contains(e.id.as_str()) {
                None
            } else {
                e.parent_id.clone()
            },
            title: e.title.clone(),
            sort_key: e.sort_key.clone(),
            path: e.path.clone(),
            restricted: e.restricted,
        })
        .collect();
    // Stable, so equal keys keep their manifest order.
    out.sort_by(|a, b| {
        depth_of(a)
            .cmp(&depth_of(b))
            .then_with(|| a.sort_key.cmp(&b.sort_key))
    });
    out
}

/// Map a sentinel URL (one of `knot://doc/<uuid>`, `knot://board/<uuid>.svg`,
/// `/api/blobs/<uuid>`) to its human-readable zip path using the export's
/// pre-computed id → path tables. Returns `None` for URLs that aren't a
/// recognized sentinel, or whose id isn't in the export.
fn url_to_local_path(
    url: &str,
    doc_paths: &std::collections::HashMap<Uuid, String>,
    board_paths: &std::collections::HashMap<Uuid, String>,
    blob_paths: &std::collections::HashMap<Uuid, String>,
) -> Option<String> {
    if let Some(rest) = url.strip_prefix("knot://doc/") {
        let id_str = rest.split(['?', '#', '/']).next().unwrap_or(rest);
        if let Ok(id) = Uuid::parse_str(id_str)
            && let Some(p) = doc_paths.get(&id)
        {
            return Some(p.clone());
        }
    }
    if let Some(rest) = url.strip_prefix("knot://board/")
        && let Some(id_str) = rest.strip_suffix(".svg")
        && let Ok(id) = Uuid::parse_str(id_str)
        && let Some(p) = board_paths.get(&id)
    {
        return Some(p.clone());
    }
    if let Some(rest) = url.strip_prefix("/api/blobs/") {
        let id_str = rest.split(['?', '#', '/']).next().unwrap_or(rest);
        if let Ok(id) = Uuid::parse_str(id_str)
            && let Some(p) = blob_paths.get(&id)
        {
            return Some(p.clone());
        }
    }
    None
}

/// Inverse mapping used by import. Takes a zip path (from the manifest's
/// path↔id tables) and the id-remap tables; returns the live URL using
/// the freshly-assigned id, or `None` when the path isn't tracked or
/// the old id isn't in the remap.
fn local_path_to_url(
    path: &str,
    path_to_doc: &HashMap<String, String>,
    path_to_blob: &HashMap<String, String>,
    path_to_board: &HashMap<String, String>,
    doc_remap: &HashMap<String, Uuid>,
    blob_remap: &HashMap<String, Uuid>,
    board_remap: &HashMap<String, Uuid>,
) -> Option<String> {
    if let Some(old) = path_to_doc.get(path)
        && let Some(new) = doc_remap.get(old)
    {
        return Some(format!("knot://doc/{new}"));
    }
    if let Some(old) = path_to_board.get(path)
        && let Some(new) = board_remap.get(old)
    {
        return Some(format!("knot://board/{new}.svg"));
    }
    if let Some(old) = path_to_blob.get(path)
        && let Some(new) = blob_remap.get(old)
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
        let Some(replacement) = map_url(url) else {
            continue;
        };
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
    if edits.is_empty() {
        return md.to_string();
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = md.to_string();
    for (start, end, repl) in edits {
        out.replace_range(start..end, &repl);
    }
    out
}

/// Rewrite live sentinel + blob URLs to the human-readable zip paths so
/// the exported markdown renders cleanly in any plain markdown viewer.
///
///   knot://doc/<uuid>          → docs/<slug>.md
///   knot://board/<uuid>.svg    → boards/<slug>.svg
///   /api/blobs/<uuid>          → attachments/<slug>
fn rewrite_for_export(
    md: &str,
    doc_paths: &std::collections::HashMap<Uuid, String>,
    board_paths: &std::collections::HashMap<Uuid, String>,
    blob_paths: &std::collections::HashMap<Uuid, String>,
) -> String {
    rewrite_link_urls(md, |u| {
        url_to_local_path(u, doc_paths, board_paths, blob_paths)
    })
}

/// Inverse of `rewrite_for_export`, applying the id remap so imported
/// references point at the freshly-created records.
fn remap_sentinels(
    md: &str,
    path_to_doc: &HashMap<String, String>,
    path_to_blob: &HashMap<String, String>,
    path_to_board: &HashMap<String, String>,
    doc_remap: &HashMap<String, Uuid>,
    blob_remap: &HashMap<String, Uuid>,
    board_remap: &HashMap<String, Uuid>,
) -> String {
    rewrite_link_urls(md, |u| {
        local_path_to_url(
            u,
            path_to_doc,
            path_to_blob,
            path_to_board,
            doc_remap,
            blob_remap,
            board_remap,
        )
    })
}

/// Conservative content-type allowlist for imported attachments. Any
/// type not on this list collapses to `application/octet-stream` so a
/// manifest can't trick the server into serving `text/html` (XSS) or
/// `image/svg+xml` (script-in-SVG) for arbitrary uploaded bytes. The
/// real defense for SVG is the public-share path (which is fine — it's
/// just bytes through CORS-friendly headers); this clamp protects the
/// authenticated `/api/blobs/{id}` path.
fn sanitize_content_type(declared: &str) -> String {
    // Strip parameters (`text/plain; charset=utf-8` → `text/plain`).
    let base = declared
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    const ALLOWED: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "application/pdf",
        "text/plain",
        "text/markdown",
        "text/csv",
        "application/json",
        "application/zip",
    ];
    if ALLOWED.contains(&base.as_str()) {
        base
    } else {
        "application/octet-stream".to_string()
    }
}

/// Validate the upload body is a recognisable zip archive. Pure for
/// testing — the error codes here flow through to the import handler's
/// HTTP responses verbatim.
fn open_import_zip(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, &'static str> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| "import.not_zip")
}

/// Read and version-check `index.json` from an already-opened import zip.
fn read_manifest<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
) -> Result<Manifest, &'static str> {
    let bytes = read_zip_entry(zip, "index.json").map_err(|_| "import.no_manifest")?;
    let m: Manifest = serde_json::from_slice(&bytes).map_err(|_| "import.bad_manifest")?;
    if m.knot_export_version != MANIFEST_VERSION {
        return Err("import.version_mismatch");
    }
    Ok(m)
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest entry with `id` under `parent_id`.
    fn manifest_doc(id: &str, parent_id: Option<&str>, restricted: bool) -> DocEntry {
        DocEntry {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            title: id.to_string(),
            sort_key: "a".to_string(),
            path: format!("docs/{id}.md"),
            restricted,
        }
    }

    #[test]
    fn restricted_subtrees_drop_below_a_lost_page_in_a_restricted_subtree() {
        let vault = manifest_doc("vault", None, true);
        let plans = manifest_doc("plans", Some("vault"), false);
        let notes = manifest_doc("notes", Some("plans"), false);
        let open = manifest_doc("open", None, false);
        let leaf = manifest_doc("leaf", Some("open"), false);

        // Every page created: nothing is skipped.
        let mut all_created = RestrictedSubtrees::default();
        for entry in [&vault, &plans, &notes, &open, &leaf] {
            assert!(all_created.admit(entry), "{} admitted", entry.id);
        }

        // The boundary is lost: its whole subtree goes.
        let mut boundary_lost = RestrictedSubtrees::default();
        assert!(boundary_lost.admit(&vault));
        boundary_lost.failed(&vault);
        assert!(!boundary_lost.admit(&plans));
        assert!(!boundary_lost.admit(&notes));
        assert!(boundary_lost.admit(&open));

        // A subpage of the boundary is lost: only what is below it goes.
        let mut subpage_lost = RestrictedSubtrees::default();
        assert!(subpage_lost.admit(&vault));
        assert!(subpage_lost.admit(&plans));
        subpage_lost.failed(&plans);
        assert!(!subpage_lost.admit(&notes));

        // Outside every restricted subtree, children of a lost page are
        // still admitted, to be grafted onto the import target.
        let mut unrestricted_lost = RestrictedSubtrees::default();
        assert!(unrestricted_lost.admit(&open));
        unrestricted_lost.failed(&open);
        assert!(unrestricted_lost.admit(&leaf));
    }

    /// Build the three uuid → path tables used by rewrite_for_export
    /// for a single doc/board/blob trio.
    fn single_path_maps(
        doc: Uuid,
        board: Uuid,
        blob: Uuid,
    ) -> (
        std::collections::HashMap<Uuid, String>,
        std::collections::HashMap<Uuid, String>,
        std::collections::HashMap<Uuid, String>,
    ) {
        let mut d = std::collections::HashMap::new();
        d.insert(doc, "docs/Hello.md".to_string());
        let mut b = std::collections::HashMap::new();
        b.insert(board, "boards/Diagram.svg".to_string());
        let mut a = std::collections::HashMap::new();
        a.insert(blob, "attachments/photo.png".to_string());
        (d, b, a)
    }

    #[test]
    fn export_rewrite_handles_all_three_sentinel_shapes() {
        let doc = Uuid::new_v4();
        let board = Uuid::new_v4();
        let blob = Uuid::new_v4();
        let (d, b, a) = single_path_maps(doc, board, blob);
        let md = format!(
            "See [other](knot://doc/{doc}) and ![diag](knot://board/{board}.svg) and ![pic](/api/blobs/{blob})\n"
        );
        let out = rewrite_for_export(&md, &d, &b, &a);
        assert!(out.contains("docs/Hello.md"));
        assert!(out.contains("boards/Diagram.svg"));
        assert!(out.contains("attachments/photo.png"));
        assert!(!out.contains("knot://"));
        assert!(!out.contains("/api/blobs/"));
    }

    #[test]
    fn import_remap_local_paths_to_new_sentinels() {
        let old_doc = Uuid::new_v4();
        let new_doc = Uuid::new_v4();
        let mut doc_remap = HashMap::new();
        doc_remap.insert(old_doc.to_string(), new_doc);
        let mut path_to_doc = HashMap::new();
        path_to_doc.insert("docs/Hello.md".to_string(), old_doc.to_string());
        let md = "[link](docs/Hello.md)\n";
        let out = remap_sentinels(
            md,
            &path_to_doc,
            &HashMap::new(),
            &HashMap::new(),
            &doc_remap,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(out.contains(&format!("knot://doc/{new_doc}")));
        assert!(!out.contains("docs/Hello.md"));
    }

    #[test]
    fn import_leaves_unknown_local_paths_untouched() {
        // A local-path URL the manifest doesn't know about means the
        // referenced resource wasn't bundled with this zip. The walker
        // should leave it as-is rather than producing a broken sentinel.
        let md = "[ghost](docs/missing.md)\n";
        let out = remap_sentinels(
            md,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(out, md);
    }

    #[test]
    fn slugify_handles_unsafe_chars_and_empty_input() {
        assert_eq!(slugify("Hello World"), "Hello-World");
        assert_eq!(slugify("../etc/passwd"), "etc-passwd");
        assert_eq!(slugify("  "), "untitled");
        assert_eq!(slugify("multi    spaces"), "multi-spaces");
        assert_eq!(slugify("Über naïve résumé"), "ber-na-ve-r-sum");
    }

    #[test]
    fn unique_path_disambiguates_collisions() {
        let mut used = std::collections::HashSet::new();
        used.insert("index.json".to_string());
        let a = unique_path(&mut used, "docs", "Notes", ".md");
        let b = unique_path(&mut used, "docs", "Notes", ".md");
        let c = unique_path(&mut used, "docs", "Notes", ".md");
        assert_eq!(a, "docs/Notes.md");
        assert_eq!(b, "docs/Notes-2.md");
        assert_eq!(c, "docs/Notes-3.md");
    }

    #[test]
    fn split_stem_ext_handles_dotfiles_and_extensionless() {
        assert_eq!(split_stem_ext("photo.png"), ("photo".into(), ".png".into()));
        assert_eq!(split_stem_ext("Makefile"), ("Makefile".into(), "".into()));
        assert_eq!(
            split_stem_ext(".gitignore"),
            (".gitignore".into(), "".into())
        );
        assert_eq!(split_stem_ext("a.b.c"), ("a.b".into(), ".c".into()));
    }

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut z = ZipWriter::new(cursor);
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in entries {
                z.start_file(*name, opts).unwrap();
                z.write_all(body).unwrap();
            }
            z.finish().unwrap();
        }
        buf
    }

    #[test]
    fn import_open_zip_rejects_non_zip_bytes() {
        let err = open_import_zip(b"hello world").unwrap_err();
        assert_eq!(err, "import.not_zip");
    }

    #[test]
    fn import_read_manifest_missing_index_json() {
        let zip = build_zip(&[("docs/x.md", b"hi")]);
        let mut z = open_import_zip(&zip).unwrap();
        let err = read_manifest(&mut z).unwrap_err();
        assert_eq!(err, "import.no_manifest");
    }

    #[test]
    fn import_read_manifest_rejects_malformed_json() {
        let zip = build_zip(&[("index.json", b"{not json")]);
        let mut z = open_import_zip(&zip).unwrap();
        let err = read_manifest(&mut z).unwrap_err();
        assert_eq!(err, "import.bad_manifest");
    }

    #[test]
    fn import_read_manifest_rejects_version_mismatch() {
        let body = br#"{"knot_export_version":"99","docs":[],"attachments":[],"boards":[]}"#;
        let zip = build_zip(&[("index.json", body)]);
        let mut z = open_import_zip(&zip).unwrap();
        let err = read_manifest(&mut z).unwrap_err();
        assert_eq!(err, "import.version_mismatch");
    }

    #[test]
    fn import_read_manifest_accepts_well_formed_v1_index() {
        let body = format!(
            r#"{{"knot_export_version":"{MANIFEST_VERSION}","docs":[],"attachments":[],"boards":[]}}"#
        );
        let zip = build_zip(&[("index.json", body.as_bytes())]);
        let mut z = open_import_zip(&zip).unwrap();
        let m = read_manifest(&mut z).unwrap();
        assert!(m.docs.is_empty());
    }

    #[test]
    fn sanitize_content_type_strips_parameters_and_clamps_unknown_types() {
        assert_eq!(sanitize_content_type("image/png"), "image/png");
        assert_eq!(
            sanitize_content_type("text/plain; charset=utf-8"),
            "text/plain"
        );
        assert_eq!(
            sanitize_content_type("image/svg+xml"),
            "application/octet-stream"
        );
        assert_eq!(
            sanitize_content_type("text/html"),
            "application/octet-stream"
        );
        assert_eq!(
            sanitize_content_type("application/javascript"),
            "application/octet-stream"
        );
    }

    #[test]
    fn rewrite_link_urls_handles_link_inside_table_cell() {
        let doc = Uuid::new_v4();
        let (d, b, a) = single_path_maps(doc, Uuid::nil(), Uuid::nil());
        let md = format!("| col |\n| --- |\n| [other](knot://doc/{doc}) |\n");
        let out = rewrite_for_export(&md, &d, &b, &a);
        assert!(out.contains("docs/Hello.md"));
        assert!(!out.contains("knot://"));
    }

    #[test]
    fn rewrite_link_urls_handles_link_inside_nested_list() {
        let doc = Uuid::new_v4();
        let (d, b, a) = single_path_maps(doc, Uuid::nil(), Uuid::nil());
        let md = format!("- outer\n  - inner [link](knot://doc/{doc})\n    - deeper\n");
        let out = rewrite_for_export(&md, &d, &b, &a);
        assert!(out.contains("docs/Hello.md"));
    }

    #[test]
    fn rewrite_link_urls_handles_link_inside_blockquote() {
        let doc = Uuid::new_v4();
        let (d, b, a) = single_path_maps(doc, Uuid::nil(), Uuid::nil());
        let md = format!("> see [other](knot://doc/{doc})\n");
        let out = rewrite_for_export(&md, &d, &b, &a);
        assert!(out.contains("docs/Hello.md"));
    }

    #[test]
    fn rewrite_leaves_user_mentions_alone() {
        // `knot://user/<uuid>` is the mention sentinel — not a doc/board/
        // blob ref. The export's rewrite must pass it through unchanged so
        // the import can re-link assignees by user_id.
        let uid = Uuid::new_v4();
        let md = format!("- [ ] [@Alice](knot://user/{uid}) Buy milk\n");
        let (d, b, a) = single_path_maps(Uuid::nil(), Uuid::nil(), Uuid::nil());
        let out = rewrite_for_export(&md, &d, &b, &a);
        assert_eq!(out, md);
    }

    #[test]
    fn rewrite_skips_reference_style_links_silently() {
        let doc = Uuid::new_v4();
        let (d, b, a) = single_path_maps(doc, Uuid::nil(), Uuid::nil());
        let md = format!("[link][ref]\n\n[ref]: knot://doc/{doc}\n");
        let out = rewrite_for_export(&md, &d, &b, &a);
        assert!(out.contains(&format!("knot://doc/{doc}")));
        assert!(!out.contains("docs/Hello.md"));
    }

    #[test]
    fn rewrite_ignores_uuid_lookalikes_in_prose() {
        let id = Uuid::new_v4();
        let (d, b, a) = single_path_maps(id, Uuid::nil(), Uuid::nil());
        let md = format!(
            "Discussing the value `knot://doc/{id}` inline.\n\n```\nknot://doc/{id}\n```\n"
        );
        let out = rewrite_for_export(&md, &d, &b, &a);
        assert_eq!(out, md);
    }

    #[test]
    fn export_then_import_roundtrips_through_remap() {
        let old_doc = Uuid::new_v4();
        let new_doc = Uuid::new_v4();
        let (d, b, a) = single_path_maps(old_doc, Uuid::nil(), Uuid::nil());
        let mut doc_remap = HashMap::new();
        doc_remap.insert(old_doc.to_string(), new_doc);
        let mut path_to_doc = HashMap::new();
        path_to_doc.insert("docs/Hello.md".to_string(), old_doc.to_string());

        let original = format!("[other](knot://doc/{old_doc})\n");
        let exported = rewrite_for_export(&original, &d, &b, &a);
        let imported = remap_sentinels(
            &exported,
            &path_to_doc,
            &HashMap::new(),
            &HashMap::new(),
            &doc_remap,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(imported, format!("[other](knot://doc/{new_doc})\n"));
    }

    /// A manifest page with `id`, its parent's id and its order key.
    fn entry(id: &str, parent: Option<&str>, key: &str) -> DocEntry {
        DocEntry {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            title: id.to_string(),
            sort_key: key.to_string(),
            path: format!("docs/{id}.md"),
            restricted: false,
        }
    }

    /// `(id, parent_id)` of each entry `creation_order` returns, in order.
    fn ordered(entries: &[DocEntry]) -> Vec<(String, Option<String>)> {
        creation_order(entries)
            .into_iter()
            .map(|e| (e.id, e.parent_id))
            .collect()
    }

    /// Pages that are their own ancestor, `a` and `b` each other's parent
    /// and `s` its own, lose their parent and are ordered as roots, by key
    /// among the other roots. A page under a cycle member keeps it as its
    /// parent. The walk used to recurse through the cycle without end.
    #[test]
    fn creation_order_makes_cycle_members_roots() {
        let entries = [
            entry("c", Some("a"), "c"),
            entry("a", Some("b"), "a"),
            entry("b", Some("a"), "b"),
            entry("s", Some("s"), "s"),
            entry("r", None, "r"),
        ];
        let some = |p: &str| Some(p.to_string());
        assert_eq!(
            ordered(&entries),
            [
                ("a".to_string(), None),
                ("b".to_string(), None),
                ("r".to_string(), None),
                ("s".to_string(), None),
                ("c".to_string(), some("a")),
            ]
        );
    }

    /// A chain of parents far deeper than a thread's stack, listed deepest
    /// first so the walk from the first entry climbs all of it. Recursing
    /// once per level overflowed the stack and aborted the process.
    #[test]
    fn creation_order_walks_a_chain_deeper_than_the_stack() {
        let n = 200_000;
        let entries: Vec<DocEntry> = (0..n)
            .rev()
            .map(|i| {
                let parent = (i > 0).then(|| (i - 1).to_string());
                entry(&i.to_string(), parent.as_deref(), "m")
            })
            .collect();
        let ids: Vec<usize> = creation_order(&entries)
            .iter()
            .map(|e| e.id.parse().unwrap())
            .collect();
        assert!(ids.into_iter().eq(0..n), "parents before children");
    }

    // -----------------------------------------------------------------------
    // Import caps
    // -----------------------------------------------------------------------

    /// A manifest attachment of the page `doc_id`, stored at `path`. Its
    /// `byte_size` claims nothing: the caps must not trust it.
    fn manifest_attachment(doc_id: &str, path: &str) -> AttachmentEntry {
        AttachmentEntry {
            id: Uuid::new_v4().to_string(),
            doc_id: doc_id.to_string(),
            content_type: "application/octet-stream".to_string(),
            original_name: None,
            byte_size: 0,
            path: path.to_string(),
        }
    }

    /// A manifest board of the page `doc_id`, with its SVG at `path`.
    fn manifest_board(doc_id: &str, path: Option<&str>) -> BoardEntry {
        BoardEntry {
            id: Uuid::new_v4().to_string(),
            doc_id: doc_id.to_string(),
            label: None,
            has_svg: path.is_some(),
            path: path.map(str::to_string),
        }
    }

    /// A version-2 manifest of `docs`, `attachments` and `boards`.
    fn manifest_of(
        docs: Vec<DocEntry>,
        attachments: Vec<AttachmentEntry>,
        boards: Vec<BoardEntry>,
    ) -> Manifest {
        Manifest {
            knot_export_version: MANIFEST_VERSION.to_string(),
            docs,
            attachments,
            boards,
        }
    }

    /// `zip` with the uncompressed size of `entry_name` rewritten to
    /// `declared_size`, in its local header and in the central directory
    /// alike, so the archive is consistent but lies about that entry.
    fn with_declared_size(zip: &[u8], entry_name: &str, declared_size: u32) -> Vec<u8> {
        const LOCAL_HEADER: &[u8] = b"PK\x03\x04";
        const CENTRAL_HEADER: &[u8] = b"PK\x01\x02";
        let mut patched = zip.to_vec();
        let name = entry_name.as_bytes();
        let name_at =
            |bytes: &[u8], offset: usize| bytes.get(offset..offset + name.len()) == Some(name);
        let mut patched_headers = 0;
        for offset in 0..patched.len().saturating_sub(4) {
            let size_offset = match &patched[offset..offset + 4] {
                signature if signature == LOCAL_HEADER && name_at(&patched, offset + 30) => 22,
                signature if signature == CENTRAL_HEADER && name_at(&patched, offset + 46) => 24,
                _ => continue,
            };
            patched[offset + size_offset..offset + size_offset + 4]
                .copy_from_slice(&declared_size.to_le_bytes());
            patched_headers += 1;
        }
        assert_eq!(
            patched_headers, 2,
            "local and central header of {entry_name}"
        );
        patched
    }

    /// The plan counts every page, and the declared size of the zip entry
    /// behind every attachment and board SVG reference: an entry named twice
    /// counts twice, the manifest's `byte_size` counts for nothing, a missing
    /// entry or a board without an SVG counts zero.
    #[test]
    fn import_plan_counts_every_reference_by_declared_size() {
        let photo = vec![7u8; 1000];
        let svg = b"<svg/>".to_vec();
        let zip_bytes = build_zip(&[
            ("attachments/photo.png", &photo),
            ("boards/sketch.svg", &svg),
        ]);
        let zip = open_import_zip(&zip_bytes).unwrap();
        let manifest = manifest_of(
            vec![
                manifest_doc("a", None, false),
                manifest_doc("b", Some("a"), false),
                manifest_doc("c", None, true),
            ],
            vec![
                manifest_attachment("a", "attachments/photo.png"),
                manifest_attachment("b", "attachments/photo.png"),
                manifest_attachment("c", "attachments/missing.png"),
            ],
            vec![
                manifest_board("a", Some("boards/sketch.svg")),
                manifest_board("b", None),
            ],
        );

        let plan = ImportPlan::of(&manifest, &zip);

        assert_eq!(
            plan,
            ImportPlan {
                pages: 3,
                markdown_bytes: 0,
                blob_bytes: 2000,
                board_svg_bytes: svg.len() as u64,
                references: 5,
            }
        );
        assert_eq!(plan.total_bytes(), 2000 + svg.len() as u64);
    }

    /// An entry that declares more than one entry may hold is skipped when
    /// read, so it counts as at most `MAX_ENTRY_BYTES` per reference.
    #[test]
    fn import_plan_counts_an_oversized_declaration_at_the_entry_cap() {
        let zip_bytes = with_declared_size(
            &build_zip(&[("attachments/huge.bin", b"tiny")]),
            "attachments/huge.bin",
            u32::MAX,
        );
        let zip = open_import_zip(&zip_bytes).unwrap();
        let manifest = manifest_of(
            vec![manifest_doc("a", None, false)],
            vec![manifest_attachment("a", "attachments/huge.bin")],
            vec![],
        );

        assert_eq!(ImportPlan::of(&manifest, &zip).blob_bytes, MAX_ENTRY_BYTES);
    }

    /// The body of a refusal, as JSON.
    async fn refusal_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn import_caps_refuse_one_page_too_many() {
        let at_the_cap = ImportPlan {
            pages: MAX_IMPORT_PAGES,
            markdown_bytes: 0,
            blob_bytes: 0,
            board_svg_bytes: 0,
            references: 0,
        };
        assert!(at_the_cap.enforce_caps().is_ok());

        let over_the_cap = ImportPlan {
            pages: MAX_IMPORT_PAGES + 1,
            ..at_the_cap
        };
        let refusal = over_the_cap.enforce_caps().unwrap_err();
        assert_eq!(refusal.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            refusal_json(refusal).await["error"],
            serde_json::json!({
                "code": "import.too_large",
                "message": "this import is too large",
                "details": {
                    "pages": MAX_IMPORT_PAGES + 1,
                    "bytes": 0,
                    "markdown_bytes": 0,
                    "max_pages": MAX_IMPORT_PAGES,
                    "max_bytes": MAX_IMPORT_BYTES,
                    "max_markdown_bytes": MAX_IMPORT_MARKDOWN_BYTES,
                    "references": 0,
                    "max_references": MAX_IMPORT_REFERENCES,
                },
            })
        );
    }

    /// Attachment and board SVG bytes share one cap.
    #[tokio::test]
    async fn import_caps_refuse_one_byte_too_many() {
        let at_the_cap = ImportPlan {
            pages: 1,
            markdown_bytes: 0,
            blob_bytes: MAX_IMPORT_BYTES - 10,
            board_svg_bytes: 10,
            references: 2,
        };
        assert!(at_the_cap.enforce_caps().is_ok());

        let over_the_cap = ImportPlan {
            board_svg_bytes: 11,
            ..at_the_cap
        };
        let refusal = over_the_cap.enforce_caps().unwrap_err();
        assert_eq!(refusal.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let details = refusal_json(refusal).await["error"]["details"].clone();
        assert_eq!(details["bytes"], MAX_IMPORT_BYTES + 1);
        assert_eq!(details["max_bytes"], MAX_IMPORT_BYTES);
    }

    /// Every page's Markdown is read and written once per page, so the plan
    /// counts the declared size of the entry behind every doc, per
    /// reference: many pages naming one large entry count it each time.
    #[test]
    fn import_plan_counts_page_markdown_per_reference() {
        let shared_markdown = vec![b'x'; 5000];
        let zip_bytes = build_zip(&[("docs/shared.md", &shared_markdown)]);
        let zip = open_import_zip(&zip_bytes).unwrap();
        let docs: Vec<DocEntry> = (0..3)
            .map(|index| DocEntry {
                path: "docs/shared.md".to_string(),
                ..manifest_doc(&format!("page-{index}"), None, false)
            })
            .chain([manifest_doc("missing", None, false)])
            .collect();
        let manifest = manifest_of(docs, vec![], vec![]);

        let plan = ImportPlan::of(&manifest, &zip);

        assert_eq!(plan.pages, 4);
        assert_eq!(plan.markdown_bytes, 3 * 5000);
        assert_eq!(plan.total_bytes(), 0, "files and board images only");
    }

    /// Page Markdown has a cap of its own, refused like the others.
    #[tokio::test]
    async fn import_caps_refuse_one_markdown_byte_too_many() {
        let at_the_cap = ImportPlan {
            pages: 1,
            markdown_bytes: MAX_IMPORT_MARKDOWN_BYTES,
            blob_bytes: 0,
            board_svg_bytes: 0,
            references: 0,
        };
        assert!(at_the_cap.enforce_caps().is_ok());

        let over_the_cap = ImportPlan {
            markdown_bytes: MAX_IMPORT_MARKDOWN_BYTES + 1,
            ..at_the_cap
        };
        let refusal = over_the_cap.enforce_caps().unwrap_err();
        assert_eq!(refusal.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let details = refusal_json(refusal).await["error"]["details"].clone();
        assert_eq!(details["markdown_bytes"], MAX_IMPORT_MARKDOWN_BYTES + 1);
        assert_eq!(details["max_markdown_bytes"], MAX_IMPORT_MARKDOWN_BYTES);
    }

    /// Attachment and board references share one cap, checked like the
    /// others: one too many is refused with every count in the details.
    #[tokio::test]
    async fn import_caps_refuse_one_reference_too_many() {
        let at_the_cap = ImportPlan {
            pages: 1,
            markdown_bytes: 0,
            blob_bytes: 0,
            board_svg_bytes: 0,
            references: MAX_IMPORT_REFERENCES,
        };
        assert!(at_the_cap.enforce_caps().is_ok());

        let over_the_cap = ImportPlan {
            references: MAX_IMPORT_REFERENCES + 1,
            ..at_the_cap
        };
        let refusal = over_the_cap.enforce_caps().unwrap_err();
        assert_eq!(refusal.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let error = refusal_json(refusal).await["error"].clone();
        assert_eq!(error["code"], "import.too_large");
        assert_eq!(
            error["details"],
            serde_json::json!({
                "pages": 1,
                "bytes": 0,
                "markdown_bytes": 0,
                "max_pages": MAX_IMPORT_PAGES,
                "max_bytes": MAX_IMPORT_BYTES,
                "max_markdown_bytes": MAX_IMPORT_MARKDOWN_BYTES,
                "references": MAX_IMPORT_REFERENCES + 1,
                "max_references": MAX_IMPORT_REFERENCES,
            })
        );
    }

    /// Missing entries cost no bytes, but every reference counts against the
    /// reference cap: boards with and without an SVG, attachments whose
    /// entry isn't there.
    #[test]
    fn import_caps_count_references_to_missing_entries() {
        let zip_bytes = build_zip(&[("docs/a.md", b"# A\n")]);
        let zip = open_import_zip(&zip_bytes).unwrap();
        let attachment_count = MAX_IMPORT_REFERENCES as usize / 2;
        let board_count = MAX_IMPORT_REFERENCES as usize - attachment_count;
        let attachments = |count: usize| {
            (0..count)
                .map(|index| manifest_attachment("a", &format!("attachments/missing-{index}.bin")))
                .collect::<Vec<_>>()
        };
        let boards = |count: usize| {
            (0..count)
                .map(|index| {
                    let svg_path = format!("boards/missing-{index}.svg");
                    manifest_board("a", (index % 2 == 0).then_some(svg_path.as_str()))
                })
                .collect::<Vec<_>>()
        };

        let at_the_cap = ImportPlan::of(
            &manifest_of(
                vec![manifest_doc("a", None, false)],
                attachments(attachment_count),
                boards(board_count),
            ),
            &zip,
        );
        assert_eq!(at_the_cap.references, MAX_IMPORT_REFERENCES);
        assert_eq!(at_the_cap.total_bytes(), 0);
        assert!(at_the_cap.enforce_caps().is_ok());

        let over_the_cap = ImportPlan::of(
            &manifest_of(
                vec![manifest_doc("a", None, false)],
                attachments(attachment_count + 1),
                boards(board_count),
            ),
            &zip,
        );
        assert_eq!(over_the_cap.references, MAX_IMPORT_REFERENCES + 1);
        assert!(over_the_cap.enforce_caps().is_err());
    }

    /// A small, highly compressible entry named by enough attachments goes
    /// over the byte cap, though the zip itself is tiny.
    #[test]
    fn import_caps_count_an_entry_referenced_many_times() {
        let one_mebibyte = vec![0u8; 1024 * 1024];
        let zip_bytes = build_zip(&[("attachments/zeros.bin", &one_mebibyte)]);
        assert!(zip_bytes.len() < 64 * 1024, "the zip stays small");
        let zip = open_import_zip(&zip_bytes).unwrap();
        let references = (MAX_IMPORT_BYTES / one_mebibyte.len() as u64) as usize + 1;
        assert!(
            (references as u64) <= MAX_IMPORT_REFERENCES,
            "the byte cap, not the reference cap, refuses this"
        );
        let manifest = manifest_of(
            vec![manifest_doc("a", None, false)],
            (0..references)
                .map(|_| manifest_attachment("a", "attachments/zeros.bin"))
                .collect(),
            vec![],
        );

        let plan = ImportPlan::of(&manifest, &zip);

        assert_eq!(plan.blob_bytes, references as u64 * 1024 * 1024);
        assert!(plan.enforce_caps().is_err());
    }

    #[test]
    fn read_zip_entry_reads_an_entry_of_its_declared_size() {
        let zip_bytes = build_zip(&[("docs/a.md", b"# Hello\n")]);
        let mut zip = open_import_zip(&zip_bytes).unwrap();
        assert_eq!(
            read_zip_entry(&mut zip, "docs/a.md").as_deref(),
            Ok(&b"# Hello\n"[..])
        );
        assert_eq!(
            read_zip_entry(&mut zip, "docs/missing.md"),
            Err(UnreadEntry::Missing)
        );
    }

    /// An entry that holds more than it declares is skipped, so an import
    /// never writes more bytes than its caps checked. This also pins the zip
    /// crate's own guard: its reader fails an entry that runs past its
    /// declared size, and either that error or our one-byte-past check
    /// must skip the entry.
    #[test]
    fn read_zip_entry_skips_an_entry_larger_than_declared() {
        let zip_bytes = with_declared_size(
            &build_zip(&[("attachments/liar.bin", &[1u8; 4096])]),
            "attachments/liar.bin",
            16,
        );
        let mut zip = open_import_zip(&zip_bytes).unwrap();
        assert_eq!(
            read_zip_entry(&mut zip, "attachments/liar.bin"),
            Err(UnreadEntry::LargerThanDeclaredOrUnreadable)
        );
    }

    /// The import loops count entries skipped as too large or unreadable,
    /// for one warning per import, but not missing ones.
    #[test]
    fn read_import_entry_counts_skipped_entries_but_not_missing_ones() {
        let zip_bytes = with_declared_size(
            &build_zip(&[
                ("attachments/honest.bin", &[2u8; 64]),
                ("attachments/liar.bin", &[1u8; 4096]),
            ]),
            "attachments/liar.bin",
            16,
        );
        let mut zip = open_import_zip(&zip_bytes).unwrap();
        let mut skipped_entries = 0;

        assert_eq!(
            read_import_entry(&mut zip, "attachments/honest.bin", &mut skipped_entries),
            Some(vec![2u8; 64])
        );
        assert_eq!(
            read_import_entry(&mut zip, "attachments/missing.bin", &mut skipped_entries),
            None
        );
        assert_eq!(skipped_entries, 0);
        for _ in 0..2 {
            assert_eq!(
                read_import_entry(&mut zip, "attachments/liar.bin", &mut skipped_entries),
                None
            );
        }
        assert_eq!(skipped_entries, 2);
    }

    /// Entries are found by their exact name only.
    #[test]
    fn zip_entries_are_looked_up_by_exact_name() {
        let zip_bytes = build_zip(&[("attachments/photo.png", b"png")]);
        let zip = open_import_zip(&zip_bytes).unwrap();
        assert!(zip_entry_index(&zip, "attachments/photo.png").is_some());
        assert_eq!(zip_entry_index(&zip, "attachments/Photo.png"), None);
        assert_eq!(declared_entry_size(&zip, "attachments/photo.png"), 3);
        assert_eq!(declared_entry_size(&zip, "attachments/none.png"), 0);
    }
}
