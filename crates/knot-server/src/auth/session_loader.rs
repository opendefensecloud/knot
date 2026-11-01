//! Resolves the caller to an `AuthContext` and attaches it to request
//! extensions. Two mechanisms, in this order:
//!
//! 1. `Authorization: Bearer knot_…` — a personal API token, on `/api/*`
//!    only. Authoritative when present: a bad token is a 401, we never
//!    fall back to cookies (a client sending both must not get accidental
//!    session powers — and tokens bypass CSRF, so mixing the two would
//!    blur the CSRF boundary).
//! 2. `sid` cookie — a browser session.
//!
//! No credential → no extension, no error: downstream handlers decide
//! whether the route requires auth.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::Response,
};
use knot_auth::SessionToken;
use knot_storage::{ApiTokenStore, SessionStore, WorkspaceStore};

use super::context::{AuthContext, TokenAuth};

pub use crate::auth::cookies::SID_COOKIE;

/// Tokens only ever hit `/api/*`: not the collab sockets (scope-less live
/// editing would bypass the scope model) and not `/auth/*` (sessions there).
const TOKEN_PATH_PREFIX: &str = "/api/";

#[derive(Clone)]
pub struct SessionDeps {
    pub sessions: Arc<dyn SessionStore>,
    pub tokens: Arc<dyn ApiTokenStore>,
    pub workspaces: Arc<dyn WorkspaceStore>,
    pub session_key: Vec<u8>,
}

fn bearer_token(req: &Request) -> Option<String> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let secret = value.strip_prefix("Bearer ")?;
    if secret.starts_with("knot_") {
        Some(secret.to_string())
    } else {
        None
    }
}

pub async fn session_loader_mw(
    State(deps): State<SessionDeps>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    if req.uri().path().starts_with(TOKEN_PATH_PREFIX)
        && let Some(secret) = bearer_token(&req)
    {
        return match token_context(&deps, &secret).await {
            Ok(Some((ctx, token_auth))) => {
                req.extensions_mut().insert(ctx);
                let id = token_auth.id;
                req.extensions_mut().insert(token_auth);
                // Fire-and-forget last_used_at bump, like session touch.
                let tokens = deps.tokens.clone();
                tokio::spawn(async move {
                    if let Err(e) = tokens.touch(id).await {
                        tracing::warn!(error=?e, "api-token touch failed");
                    }
                });
                next.run(req).await
            }
            // Recognised format, wrong/revoked credential — a hard stop,
            // never a cookie fallback (see module docs).
            Ok(None) => crate::http_error::json_err(
                StatusCode::UNAUTHORIZED,
                "auth.token.invalid",
                "invalid or revoked API token",
            ),
            Err(_) => {
                crate::http_error::json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
            }
        };
    }

    if let Some(token) = crate::auth::cookies::find_cookie(&req, SID_COOKIE)
        && let Ok(decoded) = SessionToken::decode(&token)
    {
        let id = knot_auth::csrf::hash_session_id(&deps.session_key, decoded.as_bytes());
        if let Ok(Some(s)) = deps.sessions.find_active(&id).await
            && let Ok(Some(role)) = deps
                .workspaces
                .get_member_role(s.workspace_id, s.user_id)
                .await
        {
            req.extensions_mut().insert(AuthContext {
                user_id: s.user_id,
                workspace_id: s.workspace_id,
                role,
            });
            // Fire-and-forget touch (using the hashed id).
            let sessions = deps.sessions.clone();
            tokio::spawn(async move {
                if let Err(e) = sessions.touch(&id).await {
                    tracing::warn!(error=?e, "session touch failed");
                }
            });
        }
    }
    next.run(req).await
}

/// Token credential → (AuthContext, TokenAuth). `Ok(None)` when the token
/// is unknown, revoked, or its holder is no longer a workspace member.
/// `Err(())` on a storage failure (→ 500).
async fn token_context(
    deps: &SessionDeps,
    secret: &str,
) -> Result<Option<(AuthContext, TokenAuth)>, ()> {
    let hash = knot_storage::api_token_hash(secret);
    let token = match deps.tokens.find_alive(&hash).await {
        Ok(Some(t)) => t,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::warn!(error=?e, "api-token lookup failed");
            return Err(());
        }
    };
    // The member join in `find_alive` guarantees a membership row, but the
    // role still decides whether the token can act at all.
    match deps
        .workspaces
        .get_member_role(token.workspace_id, token.user_id)
        .await
    {
        Ok(Some(role)) => Ok(Some((
            AuthContext {
                user_id: token.user_id,
                workspace_id: token.workspace_id,
                role,
            },
            TokenAuth {
                id: token.id,
                scopes: token.scopes,
            },
        ))),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::warn!(error=?e, "api-token role lookup failed");
            Err(())
        }
    }
}

/// Safe methods need no scope check — kept here (next to the scope
/// middleware, which uses it) because it mirrors CSRF's notion of safe.
pub(crate) fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}
