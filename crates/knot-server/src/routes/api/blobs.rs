//! Blob upload / download / delete.
//!
//! POST   /api/docs/:doc_id/blobs           multipart, returns BlobMetadata
//! GET    /api/blobs/:id                    streams bytes, ACL-checked
//! DELETE /api/blobs/:id                    editor+ on parent doc

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use multer::{Constraints, Multipart, SizeLimit};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

const MAX_BLOB_BYTES: u64 = 10 * 1024 * 1024;
const BLOCKED_PREFIXES: &[&str] = &[
    "application/x-executable",
    "application/x-msdownload",
    "application/x-msdos-program",
    "application/x-mach-binary",
];

#[derive(serde::Serialize)]
struct BlobResponse {
    id: String,
    doc_id: String,
    content_type: String,
    byte_size: i64,
    url: String,
    original_name: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/docs/:doc_id/blobs", post(upload))
        .route("/api/blobs/:id", get(download).delete(delete_blob))
}

async fn upload(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };

    // ACL: must have editor or owner on the doc.
    let Some(acl) = state.acl.clone() else {
        return internal();
    };
    match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        Ok(Some(knot_storage::WorkspaceRole::Owner | knot_storage::WorkspaceRole::Editor)) => {}
        Ok(_) => {
            return json_err(
                StatusCode::FORBIDDEN,
                "acl.no_grant",
                "editor role required",
            );
        }
        Err(_) => return internal(),
    }

    let Some(boundary) = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| multer::parse_boundary(s).ok())
    else {
        return json_err(StatusCode::BAD_REQUEST, "blob.bad_multipart", "");
    };

    let stream = req.into_body().into_data_stream();
    let constraints = Constraints::new().size_limit(SizeLimit::new().whole_stream(MAX_BLOB_BYTES));
    let mut mp = Multipart::with_constraints(stream, boundary, constraints);

    let field = match mp.next_field().await {
        Ok(Some(f)) => f,
        Ok(None) => return json_err(StatusCode::BAD_REQUEST, "blob.missing_file", ""),
        Err(e) if is_size_exceeded(&e) => {
            return json_err(StatusCode::PAYLOAD_TOO_LARGE, "blob.too_large", "10 MB cap");
        }
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "blob.bad_multipart", ""),
    };
    let original_name = field.file_name().map(|s| s.to_string());
    let field_ct = field
        .content_type()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());

    if BLOCKED_PREFIXES.iter().any(|p| field_ct.starts_with(p)) {
        return json_err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "blob.blocked_type",
            &field_ct,
        );
    }

    let bytes = match field.bytes().await {
        Ok(b) => b,
        Err(e) if is_size_exceeded(&e) => {
            return json_err(StatusCode::PAYLOAD_TOO_LARGE, "blob.too_large", "10 MB cap");
        }
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "blob.read_error", ""),
    };
    if bytes.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "blob.empty", "");
    }

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha256 = hasher.finalize().to_vec();

    let blob_id = Uuid::new_v4();
    let meta = knot_storage::BlobMetadata {
        id: blob_id,
        workspace_id: ctx.workspace_id,
        doc_id,
        content_type: field_ct.clone(),
        byte_size: bytes.len() as i64,
        sha256,
        original_name: original_name.clone(),
        created_by: ctx.user_id,
        created_at: chrono::Utc::now(),
    };

    let Some(store) = state.blob_store.clone() else {
        return internal();
    };
    let Some(blobs) = state.blob_meta.clone() else {
        return internal();
    };

    // Insert metadata first so the FK from blob_bytes → blobs is satisfied.
    if let Err(e) = blobs.insert(&meta).await {
        tracing::error!(error=?e, "blob meta insert");
        return internal();
    }
    if let Err(e) = store.put(blob_id, &bytes, &field_ct).await {
        let _ = blobs.delete(blob_id).await;
        tracing::error!(error=?e, "blob put");
        return internal();
    }

    (
        StatusCode::CREATED,
        Json(BlobResponse {
            id: meta.id.to_string(),
            doc_id: meta.doc_id.to_string(),
            content_type: meta.content_type,
            byte_size: meta.byte_size,
            url: format!("/api/blobs/{}", meta.id),
            original_name: meta.original_name,
        }),
    )
        .into_response()
}

async fn download(State(state): State<AppState>, Path(id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(blobs) = state.blob_meta.clone() else {
        return internal();
    };
    let Some(store) = state.blob_store.clone() else {
        return internal();
    };
    let Some(acl) = state.acl.clone() else {
        return internal();
    };

    let meta = match blobs.find(id).await {
        Ok(Some(m)) => m,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "blob.not_found", ""),
        Err(_) => return internal(),
    };
    match acl
        .effective_role(meta.workspace_id, meta.doc_id, ctx.user_id)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(_) => return internal(),
    }

    let bytes = match store.get(id).await {
        Ok(b) => b,
        Err(knot_storage::BlobStoreError::NotFound) => {
            return json_err(StatusCode::NOT_FOUND, "blob.not_found", "");
        }
        Err(_) => return internal(),
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, meta.content_type)
        .header(header::CACHE_CONTROL, "private, max-age=60")
        .header(header::CONTENT_LENGTH, meta.byte_size)
        .body(Body::from(bytes))
        .unwrap()
}

async fn delete_blob(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(blobs) = state.blob_meta.clone() else {
        return internal();
    };
    let Some(store) = state.blob_store.clone() else {
        return internal();
    };
    let Some(acl) = state.acl.clone() else {
        return internal();
    };

    let meta = match blobs.find(id).await {
        Ok(Some(m)) => m,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "blob.not_found", ""),
        Err(_) => return internal(),
    };
    match acl
        .effective_role(meta.workspace_id, meta.doc_id, ctx.user_id)
        .await
    {
        Ok(Some(knot_storage::WorkspaceRole::Owner | knot_storage::WorkspaceRole::Editor)) => {}
        _ => {
            return json_err(
                StatusCode::FORBIDDEN,
                "acl.no_grant",
                "editor role required",
            );
        }
    }

    let _ = store.delete(id).await;
    let _ = blobs.delete(id).await;
    StatusCode::NO_CONTENT.into_response()
}

fn is_size_exceeded(e: &multer::Error) -> bool {
    matches!(
        e,
        multer::Error::StreamSizeExceeded { .. } | multer::Error::FieldSizeExceeded { .. }
    )
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
