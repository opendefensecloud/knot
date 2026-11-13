//! Per-request scope check for API-key principals.
//!
//! Runs on every route of `routes::api::router` (layered there between
//! `require_session` and `csrf`). Cookie sessions carry no [`ApiPrincipal`]
//! and are not scope-limited.
//!
//! The health routes (`/api/healthz`, `/api/readyz`, `/api/version`) are merged
//! separately in `lib.rs`, outside this layer, so they need no scope. They are
//! still behind the session loader, so a broken or stale Bearer gets a 401
//! `api_key.invalid` there too.
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
/// (S2). The handlers of `/api/auth/api-keys*` refuse keys as well.
///
/// A public share link is a credential too, an anonymous one that outlives
/// the key, its user's password and membership: no key may mint one or
/// read the existing tokens. Revoking a link (DELETE) stays open to keys.
///
/// Page grants and restriction decide who can reach a page, and those
/// decisions outlive the key that made them: a leaked key of a page's owner
/// could otherwise hand the page to another account, or lift its
/// restriction, for good. Reading grants and access stays open to keys.
pub const SESSION_ONLY: &[(Method, &str)] = &[
    (Method::GET, "/api/auth/api-keys"),
    (Method::POST, "/api/auth/api-keys"),
    (Method::DELETE, "/api/auth/api-keys/{id}"),
    (Method::POST, "/api/workspace/members"),
    (Method::PATCH, "/api/workspace/members/{id}"),
    (Method::DELETE, "/api/workspace/members/{id}"),
    (Method::POST, "/api/docs/{doc_id}/shares"),
    (Method::GET, "/api/docs/{doc_id}/shares"),
    (Method::PUT, "/api/docs/{id}/grants/{principal}"),
    (Method::DELETE, "/api/docs/{id}/grants/{principal}"),
    (Method::PUT, "/api/docs/{id}/restricted"),
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
    // Renders a file; writes nothing but its `doc.export` audit row.
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

/// Whether `method` on the route template `matched` refuses API keys. axum
/// answers HEAD with the GET handler, so a GET row covers HEAD as well.
pub fn session_only(method: &Method, matched: &str) -> bool {
    let method = if method == Method::HEAD {
        &Method::GET
    } else {
        method
    };
    SESSION_ONLY
        .iter()
        .any(|(m, p)| m == method && *p == matched)
}

/// Logged in place of the raw path, which is whatever the client sent.
const UNMATCHED_ROUTE: &str = "<unmatched>";

/// Logs denials and successful writes by key id, so an incident can be
/// narrowed to the key. Audit rows still name only the key's user. Never the
/// secret nor its hash, and no line for reads: too noisy.
pub async fn scope_mw(req: Request<Body>, next: Next) -> Response {
    let Some(principal) = req.extensions().get::<ApiPrincipal>().copied() else {
        return next.run(req).await;
    };
    let key_id = principal.key_id;
    let method = req.method().clone();
    let matched = req.extensions().get::<MatchedPath>().cloned();
    let matched = matched.as_ref().map(MatchedPath::as_str);
    let route = matched.unwrap_or(UNMATCHED_ROUTE);
    if matched.is_some_and(|m| session_only(&method, m)) {
        tracing::info!(
            %key_id,
            %method,
            %route,
            required = %"session",
            "api key refused on a session-only route"
        );
        return json_err(
            StatusCode::FORBIDDEN,
            "api_key.session_required",
            "this route needs a signed-in session, not an API key",
        );
    }
    // Without a MatchedPath (no route matched) the override table cannot
    // apply, so only the method default counts.
    let required = match matched {
        Some(m) => required_scope(&method, m),
        None => method_default(&method),
    };
    if principal.scopes.contains(required) {
        let response = next.run(req).await;
        if !method.is_safe() && response.status().is_success() {
            tracing::info!(%key_id, %method, %route, "api key write");
        }
        return response;
    }
    tracing::info!(
        %key_id,
        %method,
        %route,
        required = %required.as_str(),
        "api key scope denied"
    );
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
    use axum::{
        Router,
        http::Request as HttpRequest,
        middleware,
        routing::{get, post},
    };
    use knot_storage::{Scopes, WorkspaceRole};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::auth::{AuthContext, csrf_mw, log_capture};

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
        assert_eq!(SESSION_ONLY.len(), 11);
    }

    #[test]
    fn changing_who_can_reach_a_page_is_session_only() {
        for (m, p) in [
            (Method::PUT, "/api/docs/{id}/grants/{principal}"),
            (Method::DELETE, "/api/docs/{id}/grants/{principal}"),
            (Method::PUT, "/api/docs/{id}/restricted"),
        ] {
            assert!(session_only(&m, p), "{m} {p}");
        }
        // Seeing who has access stays readable to keys.
        assert!(!session_only(&Method::GET, "/api/docs/{id}/grants"));
        assert!(!session_only(&Method::GET, "/api/docs/{id}/access"));
    }

    /// The page audit log only reads: `read`, like the grants and access
    /// listings, with the user's page role still deciding in the handler.
    #[test]
    fn the_page_audit_log_is_a_read_open_to_keys() {
        let p = "/api/docs/{id}/audit";
        assert_eq!(required_scope(&Method::GET, p), Scope::Read);
        assert!(!session_only(&Method::GET, p));
        assert!(!session_only(&Method::HEAD, p));
    }

    /// axum answers HEAD with the GET handler, so a GET row covers HEAD too.
    #[test]
    fn head_on_a_session_only_get_route_is_session_only() {
        assert!(session_only(&Method::HEAD, "/api/docs/{doc_id}/shares"));
        assert!(session_only(&Method::HEAD, "/api/auth/api-keys"));
        assert!(!session_only(&Method::HEAD, "/api/workspace/members"));
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
    fn short_link_scopes_follow_the_method() {
        // Get-or-create writes a permanent row, so it is a create; the
        // short-link reads need only `read`.
        let p = "/api/docs/{id}/short-link";
        assert_eq!(required_scope(&Method::POST, p), Scope::Create);
        assert_eq!(required_scope(&Method::GET, p), Scope::Read);
        assert_eq!(
            required_scope(&Method::GET, "/api/short-links/{code}"),
            Scope::Read
        );
        assert!(!session_only(&Method::POST, p));
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
        keyed(
            Router::new()
                .route("/api/docs", post(|| async { "created" }))
                .route("/api/docs/{id}/purge", post(|| async { "purged" })),
            scopes,
        )
    }

    fn keyed(routes: Router, scopes: &[&str]) -> Router {
        let scopes =
            Scopes::from_names(&scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        routes
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

    const NIL: &str = "00000000-0000-0000-0000-000000000000";

    /// Routes the log tests need beyond `app`: a read, a write the handler
    /// refuses and a session-only write.
    fn logging_app(scopes: &[&str]) -> Router {
        keyed(
            Router::new()
                .route("/api/docs/{id}", get(|| async { "doc" }))
                .route(
                    "/api/docs/{id}/template",
                    post(|| async { StatusCode::UNPROCESSABLE_ENTITY }),
                )
                .route("/api/workspace/members", post(|| async { "invited" })),
            scopes,
        )
    }

    fn request(method: Method, path: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    /// Catches a scope denial going unlogged, or losing a field. The whole
    /// line is compared, so a new field (a secret, say) fails it too.
    #[tokio::test]
    async fn scope_denial_is_logged_with_key_method_route_and_scope() {
        let (logs, _guard) = log_capture::capture();
        let (status, _) = call(app(&["read"]), "/api/docs/x/purge").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            logs.text(),
            format!(
                " INFO api key scope denied key_id={NIL} method=POST \
                 route=/api/docs/{{id}}/purge required=delete\n"
            )
        );
    }

    /// Catches a denial on an unrouted path going unlogged, or logging the
    /// raw path, which is whatever the client sent.
    #[tokio::test]
    async fn scope_denial_without_a_route_does_not_log_the_raw_path() {
        let (logs, _guard) = log_capture::capture();
        let (status, _) = call(app(&["read"]), "/api/not-a-route").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            logs.text(),
            format!(
                " INFO api key scope denied key_id={NIL} method=POST \
                 route=<unmatched> required=create\n"
            )
        );
    }

    /// Catches the session-only refusal going unlogged.
    #[tokio::test]
    async fn session_only_refusal_is_logged_with_key_method_route_and_scope() {
        let (logs, _guard) = log_capture::capture();
        let (status, v) = call(logging_app(&["create"]), "/api/workspace/members").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "api_key.session_required");
        assert_eq!(
            logs.text(),
            format!(
                " INFO api key refused on a session-only route key_id={NIL} method=POST \
                 route=/api/workspace/members required=session\n"
            )
        );
    }

    /// Catches a successful write by a key going unlogged.
    #[tokio::test]
    async fn successful_write_is_logged_with_key_method_and_route() {
        let (logs, _guard) = log_capture::capture();
        let (status, _) = call(app(&["delete"]), "/api/docs/x/purge").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            logs.text(),
            format!(" INFO api key write key_id={NIL} method=POST route=/api/docs/{{id}}/purge\n")
        );
    }

    /// Catches logging every request by a key: reads are too noisy.
    #[tokio::test]
    async fn successful_read_is_not_logged() {
        let (logs, _guard) = log_capture::capture();
        let resp = logging_app(&["read"])
            .oneshot(request(Method::GET, "/api/docs/x"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(logs.text(), "");
    }

    /// Catches logging a write the handler refused as if it had happened.
    #[tokio::test]
    async fn refused_write_is_not_logged_as_a_write() {
        let (logs, _guard) = log_capture::capture();
        let resp = logging_app(&["update"])
            .oneshot(request(Method::POST, "/api/docs/x/template"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(logs.text(), "");
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
