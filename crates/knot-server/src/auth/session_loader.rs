//! Loads the session named by the `sid` cookie and attaches an
//! `AuthContext` to request extensions. No `sid` cookie → no extension,
//! no error: downstream handlers decide whether the route requires auth.

use std::sync::Arc;

use axum::{body::Body, extract::Request, http::header, middleware::Next, response::Response};
use knot_auth::SessionToken;
use knot_storage::{SessionStore, WorkspaceStore};

use super::context::AuthContext;

pub const SID_COOKIE: &str = "sid";

#[derive(Clone)]
pub struct SessionDeps {
    pub sessions: Arc<dyn SessionStore>,
    pub workspaces: Arc<dyn WorkspaceStore>,
}

pub async fn session_loader_mw(
    axum::extract::State(deps): axum::extract::State<SessionDeps>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(token) = extract_sid(&req)
        && let Ok(decoded) = SessionToken::decode(&token)
        && let Ok(Some(s)) = deps.sessions.find_active(decoded.as_bytes()).await
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
        // Fire-and-forget touch.
        let sessions = deps.sessions.clone();
        let id = s.id.clone();
        tokio::spawn(async move {
            let _ = sessions.touch(&id).await;
        });
    }
    next.run(req).await
}

fn extract_sid(req: &Request<Body>) -> Option<String> {
    let header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for raw in header.split(';') {
        if let Ok(c) = cookie::Cookie::parse(raw.trim())
            && c.name() == SID_COOKIE
        {
            return Some(c.value().to_string());
        }
    }
    None
}
