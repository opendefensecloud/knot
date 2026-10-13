//! Shared JSON error envelope helper per spec §6.3:
//! `{"error": {"code": "<stable>", "message": "<human>", "details": {}}}`

use axum::{Json, http::StatusCode, response::IntoResponse, response::Response};
use serde_json::json;

pub fn json_err(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "code": code,
                "message": message,
                "details": {},
            }
        })),
    )
        .into_response()
}

/// 409 `doc.archived`: the doc (or the parent a doc would go under) is in
/// the archive, where everything is read-only until it is restored.
pub fn doc_archived() -> Response {
    json_err(StatusCode::CONFLICT, "doc.archived", "")
}
