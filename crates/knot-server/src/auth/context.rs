//! Per-request authentication context populated by `SessionLoader`.

use knot_storage::WorkspaceRole;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub role: WorkspaceRole,
}

/// Marker extension set when the caller authenticated with an API token
/// (Bearer). Its presence skips CSRF (no cookies ride along) and turns on
/// the scope check in `token_scopes`.
#[derive(Debug, Clone)]
pub struct TokenAuth {
    pub id: Uuid,
    pub scopes: Vec<String>,
}

impl TokenAuth {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}
