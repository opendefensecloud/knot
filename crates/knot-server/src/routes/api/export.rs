//! Page export to Word and OpenDocument:
//! - GET  /api/docs/{id}/export/diagrams      the page's diagrams, for the
//!   browser to render, and the upload limits
//! - POST /api/docs/{id}/export/{docx|odt}    multipart, one PNG per diagram
//!   key (an empty body means none) → the file
//!
//! Both sit behind `require_doc_role_mw`, so viewers and up may export, and
//! archived pages are refused. The export itself lives in [`crate::export`].

use std::collections::HashMap;
use std::time::Instant;

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use knot_export::{DiagramRef, Fallbacks};
use multer::{Constraints, Multipart, SizeLimit};
use serde::Serialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, DocArchived, require_viewer};
use crate::export::{self, DIAGRAM_SCALE, Format};
use crate::http_error::{doc_archived, json_err};

/// Largest request body: 10 MiB, as for blob uploads, so a proxy that lets
/// image uploads through lets exports through too.
pub const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;
/// Largest diagram picture. A bigger part is dropped and falls back.
pub const MAX_PART_BYTES: usize = 2 * 1024 * 1024;
/// Parts read from one request. Later parts are dropped and fall back.
pub const MAX_PARTS: usize = 100;
/// `Retry-After` seconds on `503 export.busy`.
const RETRY_AFTER_SECS: HeaderValue = HeaderValue::from_static("5");
const FALLBACKS_HEADER: HeaderName = HeaderName::from_static("x-knot-export-fallbacks");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/docs/{id}/export/diagrams", get(diagrams))
        .route("/api/docs/{id}/export/{format}", post(export_file))
}

#[derive(Serialize)]
struct DiagramsResponse {
    limits: Limits,
    diagrams: Vec<Diagram>,
}

/// What the browser has to fit its pictures into.
#[derive(Serialize)]
struct Limits {
    max_body_bytes: u64,
    max_part_bytes: usize,
    max_parts: usize,
    scale: u32,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Diagram {
    Mermaid {
        key: String,
        source: String,
    },
    Board {
        key: String,
        board_id: String,
        label: String,
    },
}

impl From<DiagramRef> for Diagram {
    fn from(diagram: DiagramRef) -> Self {
        match diagram {
            DiagramRef::Mermaid { key, source } => Self::Mermaid { key, source },
            DiagramRef::Board {
                key,
                board_id,
                label,
            } => Self::Board {
                key,
                board_id,
                label,
            },
        }
    }
}

/// The diagrams on the page's live state, under the keys the writers look
/// pictures up by.
async fn diagrams(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if let Some(refusal) = require_viewer(&req) {
        return refusal;
    }
    // The role layer lets GETs through on archived pages; export refuses
    // them on both routes.
    if req.extensions().get::<DocArchived>().is_some() {
        return doc_archived();
    }
    let handle = match export::snapshot(&state, doc_id).await {
        Ok(handle) => handle,
        Err(e) => {
            tracing::error!(error = %e, %doc_id, "export diagrams: snapshot");
            return internal();
        }
    };
    let listed = tokio::task::spawn_blocking(move || {
        knot_export::diagrams(&knot_export::walk(handle.inner()).doc)
    })
    .await;
    let diagrams = match listed {
        Ok(diagrams) => diagrams,
        Err(e) => {
            tracing::error!(error = %e, %doc_id, "export diagrams: walk");
            return internal();
        }
    };
    Json(DiagramsResponse {
        limits: Limits {
            max_body_bytes: MAX_BODY_BYTES,
            max_part_bytes: MAX_PART_BYTES,
            max_parts: MAX_PARTS,
            scale: DIAGRAM_SCALE,
        },
        diagrams: diagrams.into_iter().map(Diagram::from).collect(),
    })
    .into_response()
}

/// Export the page as `format`, counting the outcome and timing it.
#[tracing::instrument(skip_all, name = "docs.export", fields(%doc_id))]
async fn export_file(
    State(state): State<AppState>,
    Path((doc_id, format)): Path<(Uuid, String)>,
    req: Request,
) -> Response {
    let started = Instant::now();
    let format = Format::parse(&format);
    let response = export_response(&state, doc_id, format, req).await;
    let labels = [
        ("format", format.map_or("unknown", Format::extension)),
        ("outcome", outcome(response.status())),
    ];
    metrics::counter!("knot_export_total", &labels).increment(1);
    metrics::histogram!("knot_export_duration_seconds", &labels)
        .record(started.elapsed().as_secs_f64());
    response
}

async fn export_response(
    state: &AppState,
    doc_id: Uuid,
    format: Option<Format>,
    req: Request,
) -> Response {
    if let Some(refusal) = require_viewer(&req) {
        return refusal;
    }
    let Some(user_id) = req.extensions().get::<AuthContext>().map(|ctx| ctx.user_id) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(format) = format else {
        return json_err(
            StatusCode::BAD_REQUEST,
            "export.bad_format",
            "format must be docx or odt",
        );
    };
    // Taken before the upload is read, so only the exports holding a permit
    // keep pictures, images and output in memory.
    let permit = tokio::time::timeout(
        state.export_busy_wait,
        state.export_permits.clone().acquire_owned(),
    )
    .await;
    let _permit = match permit {
        Ok(Ok(permit)) => permit,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "export permits closed");
            return internal();
        }
        Err(_) => return busy(),
    };
    let Ok(pictures) = read_pictures(req).await else {
        return json_err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "export.too_large",
            "10 MiB cap",
        );
    };
    let built = match export::build(state, user_id, doc_id, format, pictures).await {
        Ok(built) => built,
        Err(e) => {
            tracing::error!(error = %e, "export");
            return internal();
        }
    };
    let file_name = export::file_name(&built.title, format);
    (
        [
            (header::CONTENT_TYPE, format.content_type().to_owned()),
            (
                header::CONTENT_DISPOSITION,
                export::content_disposition(&file_name),
            ),
            (FALLBACKS_HEADER, fallbacks_header(&built.fallbacks)),
        ],
        built.bytes,
    )
        .into_response()
}

/// The request body was over [`MAX_BODY_BYTES`].
struct TooLarge;

/// The diagram pictures in the request, by part name. An empty body, one
/// that isn't multipart, or one that turns out malformed gives what was read
/// so far: a missing picture falls back, it never fails the export. Parts
/// over [`MAX_PART_BYTES`] or past [`MAX_PARTS`] are read and dropped, since
/// multer can't continue after its own per-field limit.
async fn read_pictures(req: Request) -> Result<HashMap<String, Vec<u8>>, TooLarge> {
    let mut pictures = HashMap::new();
    let Some(boundary) = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| multer::parse_boundary(ct).ok())
    else {
        return Ok(pictures);
    };
    let constraints = Constraints::new().size_limit(SizeLimit::new().whole_stream(MAX_BODY_BYTES));
    let mut multipart =
        Multipart::with_constraints(req.into_body().into_data_stream(), boundary, constraints);
    let mut parts = 0;
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => return Ok(pictures),
            Err(e) => return stop_reading(e, pictures),
        };
        parts += 1;
        let name = field.name().map(str::to_owned);
        let mut kept = (parts <= MAX_PARTS && name.is_some()).then(Vec::new);
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    if kept
                        .as_ref()
                        .is_some_and(|bytes| bytes.len() + chunk.len() > MAX_PART_BYTES)
                    {
                        kept = None;
                    }
                    if let Some(bytes) = kept.as_mut() {
                        bytes.extend_from_slice(&chunk);
                    }
                }
                Ok(None) => break,
                Err(e) => return stop_reading(e, pictures),
            }
        }
        if let (Some(name), Some(bytes)) = (name, kept) {
            pictures.insert(name, bytes);
        }
    }
}

fn stop_reading(
    e: multer::Error,
    pictures: HashMap<String, Vec<u8>>,
) -> Result<HashMap<String, Vec<u8>>, TooLarge> {
    if matches!(e, multer::Error::StreamSizeExceeded { .. }) {
        return Err(TooLarge);
    }
    tracing::debug!(error = %e, "export pictures: unreadable multipart body");
    Ok(pictures)
}

/// `X-Knot-Export-Fallbacks`. The client shows `diagrams` and `images`.
fn fallbacks_header(fallbacks: &Fallbacks) -> String {
    format!(
        "diagrams={}, images={}, unknown={}",
        fallbacks.diagram,
        fallbacks.image_unavailable + fallbacks.image_over_budget,
        fallbacks.unknown_element
    )
}

/// The `outcome` label of `knot_export_total` for a response status.
fn outcome(status: StatusCode) -> &'static str {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        "busy"
    } else if status.is_server_error() {
        "server_error"
    } else if status.is_client_error() {
        "client_error"
    } else {
        "ok"
    }
}

fn busy() -> Response {
    let mut response = json_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "export.busy",
        "too many exports at once",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, RETRY_AFTER_SECS);
    response
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallbacks_header_sums_the_image_kinds() {
        let fallbacks = Fallbacks {
            diagram: 2,
            image_unavailable: 1,
            image_over_budget: 3,
            unknown_element: 4,
        };
        assert_eq!(
            fallbacks_header(&fallbacks),
            "diagrams=2, images=4, unknown=4"
        );
    }

    #[test]
    fn outcome_follows_the_status() {
        for (status, label) in [
            (StatusCode::OK, "ok"),
            (StatusCode::BAD_REQUEST, "client_error"),
            (StatusCode::PAYLOAD_TOO_LARGE, "client_error"),
            (StatusCode::NOT_FOUND, "client_error"),
            (StatusCode::SERVICE_UNAVAILABLE, "busy"),
            (StatusCode::INTERNAL_SERVER_ERROR, "server_error"),
        ] {
            assert_eq!(outcome(status), label, "{status}");
        }
    }
}
