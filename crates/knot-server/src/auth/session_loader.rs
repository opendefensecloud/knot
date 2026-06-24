//! Loads the session named by the `sid` cookie and attaches an
//! `AuthContext` to request extensions. No `sid` cookie → no extension,
//! no error: downstream handlers decide whether the route requires auth.

use std::sync::Arc;

use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use knot_auth::SessionToken;
use knot_storage::{SessionStore, WorkspaceStore};

use super::context::AuthContext;

pub use crate::auth::cookies::SID_COOKIE;

#[derive(Clone)]
pub struct SessionDeps {
    pub sessions: Arc<dyn SessionStore>,
    pub workspaces: Arc<dyn WorkspaceStore>,
    pub session_key: Vec<u8>,
}

pub async fn session_loader_mw(
    axum::extract::State(deps): axum::extract::State<SessionDeps>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
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
