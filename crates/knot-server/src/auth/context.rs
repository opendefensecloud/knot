//! Per-request authentication context populated by `SessionLoader`.

use knot_storage::WorkspaceRole;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub role: WorkspaceRole,
    /// S10: the API key's scope root; `None` for cookie sessions and
    /// unscoped keys. Pass it to every `AclCache` call made for this request.
    pub root_doc_id: Option<Uuid>,
}

/// The hashed id (the `sessions` primary key) of the cookie session that
/// authenticated this request; absent for API-key principals. Lets a
/// handler re-check, inside its own transaction, that the session still
/// exists (key minting does, so a password change can't race it).
#[derive(Clone)]
pub struct SessionHash(pub Vec<u8>);

impl std::fmt::Debug for SessionHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionHash(<redacted>)")
    }
}
