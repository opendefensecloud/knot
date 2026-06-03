//! knot server library — exports `router()` for tests + state for main.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use tower_http::services::{ServeDir, ServeFile};
use knot_auth::{Hasher, Throttle};
use knot_config::Config;
use knot_docs::AclCache;
use knot_storage::{
    BlobMeta, BlobStore, DocStore, GrantStore, MarkdownCacheStore, PgBytesStore, PgDocStore,
    PgGrantStore, PgMarkdownCache, PgSessionStore, PgUserStore, PgWorkspaceStore, Pool,
    SessionStore, UserStore, WorkspaceStore,
};
use uuid::Uuid;

pub mod auth;
pub mod http_error;
pub mod metrics;
pub mod protocol;
pub mod room;
pub mod routes;

use auth::SessionDeps;

#[derive(Clone)]
pub struct AppState {
    pub pool: Option<Pool>,
    pub users: Option<Arc<dyn UserStore>>,
    pub workspaces: Option<Arc<dyn WorkspaceStore>>,
    pub sessions: Option<Arc<dyn SessionStore>>,
    pub docs: Option<Arc<dyn DocStore>>,
    pub grants: Option<Arc<dyn GrantStore>>,
    pub acl: Option<Arc<AclCache>>,
    pub markdown_cache: Option<Arc<dyn MarkdownCacheStore>>,
    pub blob_store: Option<Arc<dyn BlobStore>>,
    pub blob_meta: Option<Arc<BlobMeta>>,
    pub rooms_v2: Option<Arc<knot_crdt::Rooms>>,
    pub bus: Option<Arc<dyn knot_crdt::Bus>>,
    pub hasher: Arc<Hasher>,
    pub throttle: Arc<Throttle>,
    pub session_key: Vec<u8>,
    pub base_url: String,
    pub oidc_enabled: bool,
    pub oidc: Option<Arc<knot_auth::oidc::OidcClient>>,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn in_memory() -> Self {
        Self {
            pool: None,
            users: None,
            workspaces: None,
            sessions: None,
            docs: None,
            grants: None,
            acl: None,
            markdown_cache: None,
            blob_store: None,
            blob_meta: None,
            rooms_v2: None,
            bus: None,
            hasher: Arc::new(Hasher::new()),
            throttle: Arc::new(Throttle::new()),
            session_key: Vec::new(),
            base_url: "http://localhost:3000".into(),
            oidc_enabled: false,
            oidc: None,
            config: Arc::new(Config::default()),
        }
    }

    /// Constructor used by `main` + integration tests when a real Postgres
    /// pool is available. Wires every storage trait to the corresponding
    /// `Pg*` impl so callers don't have to assemble them by hand. Caller is
    /// still responsible for setting `session_key`, `base_url`, and
    /// `oidc_enabled` from configuration.
    pub fn with_pool(pool: Pool) -> Self {
        let users: Arc<dyn UserStore> = Arc::new(PgUserStore::new(pool.clone()));
        let workspaces: Arc<dyn WorkspaceStore> = Arc::new(PgWorkspaceStore::new(pool.clone()));
        let sessions: Arc<dyn SessionStore> = Arc::new(PgSessionStore::new(pool.clone()));
        let docs: Arc<dyn DocStore> = Arc::new(PgDocStore::new(pool.clone()));
        let grants: Arc<dyn GrantStore> = Arc::new(PgGrantStore::new(pool.clone()));
        let acl = Arc::new(AclCache::new(workspaces.clone(), grants.clone()));
        let markdown_cache: Arc<dyn MarkdownCacheStore> =
            Arc::new(PgMarkdownCache::new(pool.clone()));
        let blob_store: Arc<dyn BlobStore> = Arc::new(PgBytesStore::new(pool.clone()));
        let blob_meta = Arc::new(BlobMeta::new(pool.clone()));
        Self {
            pool: Some(pool),
            users: Some(users),
            workspaces: Some(workspaces),
            sessions: Some(sessions),
            docs: Some(docs),
            grants: Some(grants),
            acl: Some(acl),
            markdown_cache: Some(markdown_cache),
            blob_store: Some(blob_store),
            blob_meta: Some(blob_meta),
            rooms_v2: None,
            bus: None,
            hasher: Arc::new(Hasher::new()),
            throttle: Arc::new(Throttle::new()),
            session_key: Vec::new(),
            base_url: "http://localhost:3000".into(),
            oidc_enabled: false,
            oidc: None,
            config: Arc::new(Config::default()),
        }
    }

    pub fn session_deps(&self) -> Option<SessionDeps> {
        Some(SessionDeps {
            sessions: self.sessions.clone()?,
            workspaces: self.workspaces.clone()?,
        })
    }
}

/// In-memory router (used by tests + the spike main without DB).
pub fn router() -> Router {
    router_with_state(AppState::in_memory())
}

pub fn router_with_state(state: AppState) -> Router {
    let web_dist =
        std::env::var("KNOT_WEB_DIST").unwrap_or_else(|_| "/web/dist".into());
    let index_path = format!("{web_dist}/index.html");
    let spa = ServeDir::new(&web_dist)
        .append_index_html_on_directories(true)
        .not_found_service(ServeFile::new(&index_path));

    let mut r = Router::new()
        .route("/collab/:doc_id", get(collab_upgrade))
        .merge(routes::health::router())
        .merge(routes::auth::router())
        .merge(routes::api::router(state.clone()))
        .fallback_service(spa);

    if let Some(deps) = state.session_deps() {
        r = r.layer(axum::middleware::from_fn_with_state(
            deps,
            auth::session_loader_mw,
        ));
    }

    r.layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn(crate::metrics::record))
        .with_state(state)
}

#[tracing::instrument(skip_all, name = "collab.upgrade", fields(doc_id = %doc_id))]
async fn collab_upgrade(
    Path(doc_id): Path<Uuid>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
    req: axum::extract::Request,
) -> axum::response::Response {
    let Some(ctx) = req.extensions().get::<crate::auth::AuthContext>().cloned() else {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            "auth.session_required",
        )
            .into_response();
    };
    let acl = match state.acl.as_ref() {
        Some(a) => a.clone(),
        None => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    };
    match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        Ok(Some(_role)) => {}
        Ok(None) => return (axum::http::StatusCode::FORBIDDEN, "acl.no_grant").into_response(),
        Err(_) => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    }
    let rooms = match state.rooms_v2.as_ref() {
        Some(r) => r.clone(),
        None => {
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response();
        }
    };
    ws.on_upgrade(move |socket| async move {
        crate::room::serve(rooms, doc_id, socket).await;
    })
    .into_response()
}
