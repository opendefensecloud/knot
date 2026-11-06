//! Per-request scope check for API-key principals.
//!
//! Runs on every `/api/*` request (layered between `require_session` and
//! `csrf` in `routes::api::router`). Cookie sessions carry no
//! [`ApiPrincipal`] and are not scope-limited.
//!
//! The required scope comes from the HTTP method, except for the routes in
//! [`OVERRIDES`]: that table is the single auditable place for exceptions.
//! Routes in [`SESSION_ONLY`] refuse every key, whatever its scopes.

use axum::{
    Json,
    body::Body,
    extract::{MatchedPath, Request},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use knot_storage::Scope;
use serde_json::json;

use super::api_key_loader::ApiPrincipal;
use crate::http_error::json_err;

/// Routes no API key may call, keyed by the exact axum `MatchedPath`: they
/// mint credentials or change who is a member and with which role, so a
/// scoped key reaching them could turn itself into an unscoped owner
/// (S2, S9). The handlers of `/api/auth/api-keys*` refuse keys as well.
///
/// A public share link is a credential too, an anonymous one that outlives
/// the key, its user's password and membership: no key may mint one or
/// read the existing tokens. Revoking a link (DELETE) stays open to keys.
pub const SESSION_ONLY: &[(Method, &str)] = &[
    (Method::GET, "/api/auth/api-keys"),
    (Method::POST, "/api/auth/api-keys"),
    (Method::DELETE, "/api/auth/api-keys/{id}"),
    (Method::POST, "/api/workspace/members"),
    (Method::PATCH, "/api/workspace/members/{id}"),
    (Method::DELETE, "/api/workspace/members/{id}"),
    (Method::POST, "/api/docs/{doc_id}/shares"),
    (Method::GET, "/api/docs/{doc_id}/shares"),
];

/// POST routes that are not creates, keyed by the exact axum `MatchedPath`.
/// The ONLY place scope exceptions live.
pub const OVERRIDES: &[(Method, &str, Scope)] = &[
    (Method::POST, "/api/docs/{id}/move", Scope::Update),
    (Method::POST, "/api/docs/{id}/markdown", Scope::Update),
    (Method::POST, "/api/docs/{id}/restore", Scope::Update),
    (
        Method::POST,
        "/api/docs/{id}/history/{seq}/restore",
        Scope::Update,
    ),
    (Method::POST, "/api/docs/{id}/template", Scope::Update),
    (
        Method::POST,
        "/api/docs/{id}/comments/{thread_id}/resolve",
        Scope::Update,
    ),
    (
        Method::POST,
        "/api/docs/{id}/comments/{thread_id}/unresolve",
        Scope::Update,
    ),
    (Method::POST, "/api/notifications/read", Scope::Update),
    (Method::POST, "/api/whats-new/seen", Scope::Update),
    (
        Method::POST,
        "/api/docs/{id}/suggestions/{suggestion_id}/accept",
        Scope::Update,
    ),
    (
        Method::POST,
        "/api/docs/{id}/suggestions/{suggestion_id}/reject",
        Scope::Update,
    ),
    (
        Method::POST,
        "/api/docs/{id}/suggestions/{suggestion_id}/withdraw",
        Scope::Update,
    ),
    // Renders a file; writes nothing.
    (Method::POST, "/api/docs/{id}/export/{format}", Scope::Read),
    (Method::POST, "/api/docs/{id}/purge", Scope::Delete),
];

/// Scope implied by the method alone. Unknown methods need `update`
/// (conservative: never `read`).
fn method_default(method: &Method) -> Scope {
    match *method {
        Method::GET | Method::HEAD | Method::OPTIONS => Scope::Read,
        Method::POST => Scope::Create,
        Method::PATCH | Method::PUT => Scope::Update,
        Method::DELETE => Scope::Delete,
        _ => Scope::Update,
    }
}

/// The scope an API key needs for `method` on the axum route template
/// `matched`: the override table first, then the method default.
pub fn required_scope(method: &Method, matched: &str) -> Scope {
    OVERRIDES
        .iter()
        .find(|(m, p, _)| m == method && *p == matched)
        .map(|(_, _, s)| *s)
        .unwrap_or_else(|| method_default(method))
}

/// Whether `method` on the route template `matched` refuses API keys.
pub fn session_only(method: &Method, matched: &str) -> bool {
    SESSION_ONLY
        .iter()
        .any(|(m, p)| m == method && *p == matched)
}

pub async fn scope_mw(req: Request<Body>, next: Next) -> Response {
    let Some(principal) = req.extensions().get::<ApiPrincipal>().copied() else {
        return next.run(req).await;
    };
    let matched = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    if matched.is_some_and(|m| session_only(req.method(), m)) {
        return json_err(
            StatusCode::FORBIDDEN,
            "api_key.session_required",
            "this route needs a signed-in session, not an API key",
        );
    }
    // Without a MatchedPath (no route matched) the override table cannot
    // apply, so only the method default counts.
    let required = match matched {
        Some(m) => required_scope(req.method(), m),
        None => method_default(req.method()),
    };
    if principal.scopes.contains(required) {
        return next.run(req).await;
    }
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": {
                "code": "api_key.scope_denied",
                "message": "",
                "details": { "scope": required.as_str() },
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::{Router, http::Request as HttpRequest, middleware, routing::post};
    use knot_storage::{Scopes, WorkspaceRole};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::auth::{AuthContext, csrf_mw};

    fn up(p: &str) -> Scope {
        required_scope(&Method::POST, p)
    }

    #[test]
    fn move_requires_update() {
        assert_eq!(up("/api/docs/{id}/move"), Scope::Update);
    }
    #[test]
    fn markdown_import_requires_update() {
        assert_eq!(up("/api/docs/{id}/markdown"), Scope::Update);
    }
    #[test]
    fn archive_restore_requires_update() {
        assert_eq!(up("/api/docs/{id}/restore"), Scope::Update);
    }
    #[test]
    fn history_restore_requires_update() {
        assert_eq!(up("/api/docs/{id}/history/{seq}/restore"), Scope::Update);
    }
    #[test]
    fn set_template_requires_update() {
        assert_eq!(up("/api/docs/{id}/template"), Scope::Update);
    }
    #[test]
    fn comment_resolve_requires_update() {
        assert_eq!(
            up("/api/docs/{id}/comments/{thread_id}/resolve"),
            Scope::Update
        );
    }
    #[test]
    fn comment_unresolve_requires_update() {
        assert_eq!(
            up("/api/docs/{id}/comments/{thread_id}/unresolve"),
            Scope::Update
        );
    }
    #[test]
    fn notifications_read_requires_update() {
        assert_eq!(up("/api/notifications/read"), Scope::Update);
    }
    #[test]
    fn whats_new_seen_requires_update() {
        assert_eq!(up("/api/whats-new/seen"), Scope::Update);
    }
    #[test]
    fn suggestion_accept_requires_update() {
        assert_eq!(
            up("/api/docs/{id}/suggestions/{suggestion_id}/accept"),
            Scope::Update
        );
    }
    #[test]
    fn suggestion_reject_requires_update() {
        assert_eq!(
            up("/api/docs/{id}/suggestions/{suggestion_id}/reject"),
            Scope::Update
        );
    }
    #[test]
    fn suggestion_withdraw_requires_update() {
        assert_eq!(
            up("/api/docs/{id}/suggestions/{suggestion_id}/withdraw"),
            Scope::Update
        );
    }
    #[test]
    fn export_file_requires_read() {
        assert_eq!(up("/api/docs/{id}/export/{format}"), Scope::Read);
    }
    #[test]
    fn purge_requires_delete() {
        assert_eq!(up("/api/docs/{id}/purge"), Scope::Delete);
    }

    #[test]
    fn member_management_and_key_management_are_session_only() {
        for (m, p) in [
            (Method::POST, "/api/workspace/members"),
            (Method::PATCH, "/api/workspace/members/{id}"),
            (Method::DELETE, "/api/workspace/members/{id}"),
            (Method::GET, "/api/auth/api-keys"),
            (Method::POST, "/api/auth/api-keys"),
            (Method::DELETE, "/api/auth/api-keys/{id}"),
        ] {
            assert!(session_only(&m, p), "{m} {p}");
        }
        // Listing members stays readable to keys (agents look up assignees).
        assert!(!session_only(&Method::GET, "/api/workspace/members"));
        assert_eq!(SESSION_ONLY.len(), 8);
    }

    #[test]
    fn share_links_cannot_be_minted_or_read_with_a_key() {
        assert!(session_only(&Method::POST, "/api/docs/{doc_id}/shares"));
        assert!(session_only(&Method::GET, "/api/docs/{doc_id}/shares"));
        // Revoking a public link only narrows access.
        assert!(!session_only(
            &Method::DELETE,
            "/api/docs/{doc_id}/shares/{share_id}"
        ));
    }

    #[test]
    fn every_override_row_is_covered_by_a_test_above() {
        // Guards against adding a row without its dedicated test.
        assert_eq!(OVERRIDES.len(), 14);
    }

    #[test]
    fn method_defaults_apply_outside_the_table() {
        let p = "/api/docs/{id}";
        assert_eq!(required_scope(&Method::GET, p), Scope::Read);
        assert_eq!(required_scope(&Method::HEAD, p), Scope::Read);
        assert_eq!(required_scope(&Method::OPTIONS, p), Scope::Read);
        assert_eq!(required_scope(&Method::POST, "/api/docs"), Scope::Create);
        assert_eq!(required_scope(&Method::PATCH, p), Scope::Update);
        assert_eq!(required_scope(&Method::PUT, p), Scope::Update);
        assert_eq!(required_scope(&Method::DELETE, p), Scope::Delete);
        // Overrides are per method: GET on an overridden path stays read.
        assert_eq!(
            required_scope(&Method::GET, "/api/docs/{id}/markdown"),
            Scope::Read
        );
    }

    #[test]
    fn unknown_method_requires_update_never_read() {
        let m = Method::from_bytes(b"PROPFIND").unwrap();
        assert_eq!(required_scope(&m, "/api/docs/{id}"), Scope::Update);
        assert_eq!(required_scope(&Method::TRACE, "/api/x"), Scope::Update);
    }

    /// Mirrors the layer order of `routes::api::router` (scope outside csrf)
    /// with a key principal injected by an outer layer, like
    /// `require_session` sees it after the API-key loader.
    fn app(scopes: &[&str]) -> Router {
        let scopes =
            Scopes::from_names(&scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        Router::new()
            .route("/api/docs", post(|| async { "created" }))
            .route("/api/docs/{id}/purge", post(|| async { "purged" }))
            .layer(middleware::from_fn(csrf_mw))
            .layer(middleware::from_fn(scope_mw))
            .layer(middleware::from_fn(
                move |mut req: Request<Body>, next: Next| async move {
                    req.extensions_mut().insert(AuthContext {
                        user_id: Uuid::nil(),
                        workspace_id: Uuid::nil(),
                        role: WorkspaceRole::Editor,
                    });
                    req.extensions_mut().insert(ApiPrincipal {
                        key_id: Uuid::nil(),
                        scopes,
                    });
                    next.run(req).await
                },
            ))
    }

    async fn call(app: Router, path: &str) -> (StatusCode, serde_json::Value) {
        let req = HttpRequest::post(path).body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn key_with_scope_passes_scope_and_skips_csrf_without_token() {
        let (status, _) = call(app(&["create"]), "/api/docs").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn key_lacking_scope_gets_scope_denied_with_required_scope() {
        let (status, v) = call(app(&["read", "create"]), "/api/docs/x/purge").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "api_key.scope_denied");
        assert_eq!(v["error"]["details"]["scope"], "delete");
    }

    #[tokio::test]
    async fn cookie_session_without_principal_is_not_scope_limited_but_csrf_checked() {
        let app = Router::new()
            .route("/api/docs", post(|| async { "created" }))
            .layer(middleware::from_fn(csrf_mw))
            .layer(middleware::from_fn(scope_mw))
            .layer(middleware::from_fn(
                |mut req: Request<Body>, next: Next| async move {
                    req.extensions_mut().insert(AuthContext {
                        user_id: Uuid::nil(),
                        workspace_id: Uuid::nil(),
                        role: WorkspaceRole::Editor,
                    });
                    next.run(req).await
                },
            ));
        let (status, v) = call(app, "/api/docs").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "auth.csrf");
    }
}
