//! Scope enforcement for API-token requests. Session-authed requests pass
//! through untouched (a logged-in human has their workspace role, no
//! scopes); a token-authed request must carry the scope its method+route
//! requires.
//!
//! Mapping (see [`required_scope`]): safe methods need `read`, `DELETE`
//! needs `delete`, the doc-creating/duplicating POSTs need `create`,
//! purge needs `delete`, every other unsafe request needs `write`.
//! Unmapped unsafe endpoints therefore fall on the conservative side: a
//! write-scope token can use them, a narrower one cannot.
//!
//! Token management (`/api/tokens/*`) is off-limits to tokens entirely —
//! a leaked secret must not be able to mint its own successor or revoke
//! evidence of use.

use axum::{
    body::Body,
    extract::Request,
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};

use super::context::TokenAuth;
use super::session_loader::is_safe;
use crate::http_error::json_err;

/// Scope demanded of a token for `method` + `path`.
pub(crate) fn required_scope(method: &Method, path: &str) -> &'static str {
    if is_safe(method) {
        return "read";
    }
    if *method == Method::DELETE {
        return "delete";
    }
    // POST that creates pages → create; POST that purges → delete.
    if *method == Method::POST {
        if path == "/api/docs" || path.starts_with("/api/docs/from-template/") {
            return "create";
        }
        if path.ends_with("/duplicate") {
            return "create";
        }
        if path.ends_with("/purge") {
            return "delete";
        }
    }
    "write"
}

pub async fn token_scope_mw(req: Request<Body>, next: Next) -> Response {
    let Some(auth) = req.extensions().get::<TokenAuth>().cloned() else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    if path == "/api/tokens" || path.starts_with("/api/tokens/") {
        return json_err(
            StatusCode::FORBIDDEN,
            "auth.token.forbidden",
            "API tokens cannot manage API tokens",
        );
    }
    let required = required_scope(req.method(), path);
    if auth.has(required) {
        next.run(req).await
    } else {
        json_err(
            StatusCode::FORBIDDEN,
            "auth.token.insufficient_scope",
            required,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::required_scope;
    use axum::http::Method;

    fn scope(method: &Method, path: &str) -> &'static str {
        required_scope(method, path)
    }

    #[test]
    fn safe_methods_read() {
        assert_eq!(scope(&Method::GET, "/api/docs"), "read");
        assert_eq!(scope(&Method::HEAD, "/api/docs/xyz"), "read");
    }

    #[test]
    fn creating_posts_create() {
        assert_eq!(scope(&Method::POST, "/api/docs"), "create");
        assert_eq!(
            scope(&Method::POST, "/api/docs/from-template/abc"),
            "create"
        );
        assert_eq!(scope(&Method::POST, "/api/docs/abc/duplicate"), "create");
    }

    #[test]
    fn purge_and_delete_delete() {
        assert_eq!(scope(&Method::POST, "/api/docs/abc/purge"), "delete");
        assert_eq!(scope(&Method::DELETE, "/api/docs/abc"), "delete");
        assert_eq!(scope(&Method::DELETE, "/api/docs/abc/grants/u:1"), "delete");
    }

    #[test]
    fn other_unsafe_write() {
        assert_eq!(scope(&Method::PATCH, "/api/docs/abc"), "write");
        assert_eq!(scope(&Method::POST, "/api/docs/abc/move"), "write");
        assert_eq!(scope(&Method::POST, "/api/docs/abc/markdown"), "write");
        assert_eq!(scope(&Method::POST, "/api/docs/abc/restore"), "write");
        assert_eq!(scope(&Method::PUT, "/api/docs/abc/restricted"), "write");
        assert_eq!(scope(&Method::POST, "/api/docs/abc/comments"), "write");
    }
}
