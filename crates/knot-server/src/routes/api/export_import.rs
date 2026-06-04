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
    extract::{Request, State},
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
        .route("/api/workspace/export", get(export))
        .route("/api/workspace/import", post(import))
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

async fn export(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(docs) = state.docs.clone() else { return internal(); };
    let Some(cache) = state.markdown_cache.clone() else { return internal(); };
    let Some(blob_meta) = state.blob_meta.clone() else { return internal(); };
    let Some(blob_store) = state.blob_store.clone() else { return internal(); };
    let Some(boards) = state.boards.clone() else { return internal(); };

    let all_docs = match docs.list_alive(ctx.workspace_id).await {
        Ok(d) => d,
        Err(_) => return internal(),
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
                parent_id: d.parent_id.map(|p| p.to_string()),
                title: d.title.clone(),
                sort_key: d.sort_key.clone(),
            });
            // Markdown body — best-effort from the cache. Missing cache entry
            // means the doc has never been exported; we skip the body rather
            // than triggering live re-render to keep the export quick. The
            // user can hit the markdown endpoint once to populate it.
            let md = cache
                .get(d.id)
                .await
                .ok()
                .flatten()
                .map(|c| c.markdown_text)
                .unwrap_or_default();
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

        // Attachments — list every blob in the workspace and bundle bytes.
        if let Ok(metas) = blob_meta.list_for_workspace(ctx.workspace_id).await {
            for m in metas {
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

async fn import(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    // Owner-only — import creates docs in the current workspace.
    use knot_storage::WorkspaceRole;
    let Some(workspaces) = state.workspaces.clone() else { return internal(); };
    match workspaces
        .get_member_role(ctx.workspace_id, ctx.user_id)
        .await
    {
        Ok(Some(WorkspaceRole::Owner)) => {}
        _ => return json_err(StatusCode::FORBIDDEN, "acl.owner_required", ""),
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
        let new_parent = d
            .parent_id
            .as_ref()
            .and_then(|p| doc_remap.get(p).copied());
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
        let Some(bytes_vec) = read_zip_entry(&mut zip, &format!("attachments/{}", a.id)) else {
            continue;
        };
        // sha256 of bytes for the metadata row.
        use sha2::{Digest, Sha256};
        let sha = Sha256::digest(&bytes_vec).to_vec();
        let meta = knot_storage::BlobMetadata {
            id: new_id,
            workspace_id: ctx.workspace_id,
            doc_id: new_doc_id,
            content_type: a.content_type.clone(),
            byte_size: a.byte_size,
            sha256: sha,
            original_name: a.original_name.clone(),
            created_by: ctx.user_id,
            created_at: chrono::Utc::now(),
        };
        if blob_meta.insert(&meta).await.is_err() { continue; }
        if blob_store
            .put(new_id, &bytes_vec, &a.content_type)
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
        let _ = rx.await;
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "imported_docs": manifest.docs.len() }))
                .unwrap_or_default(),
        ))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_zip_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    let mut entry = zip.by_name(name).ok()?;
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut out).ok()?;
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

/// Rewrite every `knot://doc/<old>`, `knot://board/<old>.svg`, and
/// `/api/blobs/<old>` reference in `md` to the new id from the remap tables.
fn remap_sentinels(
    md: &str,
    doc_remap: &HashMap<String, Uuid>,
    blob_remap: &HashMap<String, Uuid>,
    board_remap: &HashMap<String, Uuid>,
) -> String {
    let mut out = md.to_string();
    for (old, new) in doc_remap {
        out = out.replace(
            &format!("knot://doc/{old}"),
            &format!("knot://doc/{new}"),
        );
    }
    for (old, new) in board_remap {
        out = out.replace(
            &format!("knot://board/{old}.svg"),
            &format!("knot://board/{new}.svg"),
        );
    }
    for (old, new) in blob_remap {
        out = out.replace(
            &format!("/api/blobs/{old}"),
            &format!("/api/blobs/{new}"),
        );
    }
    out
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
