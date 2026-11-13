//! Shared JSON error envelope helper per spec §6.3:
//! `{"error": {"code": "<stable>", "message": "<human>", "details": {}}}`

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
    response::Response,
};
use serde_json::json;

pub fn json_err(status: StatusCode, code: &str, message: &str) -> Response {
    json_err_with_details(status, code, message, json!({}))
}

/// [`json_err`] with `details` filled in (an object, by convention).
pub fn json_err_with_details(
    status: StatusCode,
    code: &str,
    message: &str,
    details: serde_json::Value,
) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "code": code,
                "message": message,
                "details": details,
            }
        })),
    )
        .into_response()
}

/// 429 `rate.limited`: a rate limit refused the request. The wait is in
/// `Retry-After` (for other clients) and in `details.retry_after_secs`
/// (which the web client reads).
pub fn rate_limited(retry_after_secs: u64) -> Response {
    let mut response = json_err_with_details(
        StatusCode::TOO_MANY_REQUESTS,
        "rate.limited",
        "too many requests",
        json!({ "retry_after_secs": retry_after_secs }),
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
    response
}

/// 429 for a browser opening the public share page (`GET /p/{token}`): that
/// route answers navigations with whole HTML documents (its 410 and 503
/// too), so a refusal there is a small page in the same style, with the
/// same `Retry-After` and the wait in its text, rather than raw JSON.
pub fn rate_limited_page(retry_after_secs: u64) -> Response {
    let page = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Too many requests</title></head>\
         <body style=\"font-family:system-ui;padding:40px;text-align:center\"><h1>Too many requests</h1>\
         <p>You're doing that too fast. Try again in {retry_after_secs} s.</p></body></html>"
    );
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page,
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_secs));
    response
}

/// 409 `doc.archived`: the doc (or the parent a doc would go under) is in
/// the archive, where everything is read-only until it is restored.
pub fn doc_archived() -> Response {
    json_err(StatusCode::CONFLICT, "doc.archived", "")
}

/// Marks an [`audit_unavailable`] response, so the export metrics can tell
/// it apart from a `503 export.busy`.
#[derive(Clone, Copy, Debug)]
pub struct AuditUnavailable;

/// 503 `audit.unavailable`: an export whose `doc.export` audit rows couldn't
/// be written. Exports fail closed, so the file is not sent; the client may
/// try again once the audit log takes writes.
pub fn audit_unavailable() -> Response {
    let mut response = json_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "audit.unavailable",
        "the export couldn't be recorded in the audit log",
    );
    response.extensions_mut().insert(AuditUnavailable);
    response
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::header;
    use serde_json::json;

    use super::*;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn json_err_with_details_carries_the_details() {
        let response = json_err_with_details(
            StatusCode::FORBIDDEN,
            "quota.exceeded",
            "quota reached",
            json!({"quota": "pages"}),
        );
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            body_json(response).await,
            json!({"error": {"code": "quota.exceeded", "message": "quota reached", "details": {"quota": "pages"}}})
        );
    }

    #[tokio::test]
    async fn json_err_keeps_empty_details() {
        let response = json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
        assert_eq!(body_json(response).await["error"]["details"], json!({}));
    }

    #[tokio::test]
    async fn rate_limited_is_a_429_with_retry_after_in_header_and_details() {
        let response = rate_limited(7);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "7");
        assert_eq!(
            body_json(response).await,
            json!({"error": {"code": "rate.limited", "message": "too many requests", "details": {"retry_after_secs": 7}}})
        );
    }
}
