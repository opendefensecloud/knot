pub mod api_key_loader;
pub mod context;
pub mod cookies;
pub mod require_session;
pub mod session_loader;

pub use api_key_loader::ApiPrincipal;
pub use context::{AuthContext, SessionHash};
pub use require_session::require_session_mw;
pub use session_loader::{SID_COOKIE, SessionDeps, session_loader_mw};

pub mod csrf;
pub use csrf::{CSRF_COOKIE, CSRF_HEADER, csrf_mw};

pub mod scope;
pub use scope::{required_scope, scope_mw};

pub mod require_doc_role;
pub use require_doc_role::{
    ARCHIVE_PURGE_ROUTE, ARCHIVE_RESTORE_ROUTE, DocArchived, EffectiveDocRole, require_doc_role_mw,
    require_viewer,
};
