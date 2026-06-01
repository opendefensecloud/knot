//! Per-request authentication context populated by `SessionLoader`.

use knot_storage::WorkspaceRole;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub role: WorkspaceRole,
}
