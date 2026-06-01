//! knot server library — exports `router()` for tests + state for main.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use knot_auth::{Hasher, Throttle};
use knot_crdt::YrsEngine;
use knot_storage::{Pool, SessionStore, UserStore, WorkspaceStore};

pub mod auth;
pub mod protocol;
pub mod room;
pub mod routes;

use auth::SessionDeps;
use room::Rooms;

#[derive(Clone)]
pub struct AppState {
    pub rooms: Arc<Rooms>,
    pub pool: Option<Pool>,
    pub users: Option<Arc<dyn UserStore>>,
    pub workspaces: Option<Arc<dyn WorkspaceStore>>,
    pub sessions: Option<Arc<dyn SessionStore>>,
    pub hasher: Arc<Hasher>,
    pub throttle: Arc<Throttle>,
    pub session_key: Vec<u8>,
    pub base_url: String,
    pub oidc_enabled: bool,
}

impl AppState {
    pub fn in_memory() -> Self {
        Self {
            rooms: Arc::new(Rooms::new(YrsEngine)),
            pool: None,
            users: None,
            workspaces: None,
            sessions: None,
            hasher: Arc::new(Hasher::new()),
            throttle: Arc::new(Throttle::new()),
            session_key: Vec::new(),
            base_url: "http://localhost:3000".into(),
            oidc_enabled: false,
        }
    }

    pub fn with_pool(pool: Pool) -> Self {
        Self {
            rooms: Arc::new(Rooms::new(YrsEngine)),
            pool: Some(pool),
            users: None,
            workspaces: None,
            sessions: None,
            hasher: Arc::new(Hasher::new()),
            throttle: Arc::new(Throttle::new()),
            session_key: Vec::new(),
            base_url: "http://localhost:3000".into(),
            oidc_enabled: false,
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
    let mut r = Router::new()
        .route("/collab/:doc_id", get(collab_upgrade))
        .merge(routes::health::router());

    if let Some(deps) = state.session_deps() {
        r = r.layer(axum::middleware::from_fn_with_state(
            deps,
            auth::session_loader_mw,
        ));
    }

    r.with_state(state)
}

async fn collab_upgrade(
    Path(doc_id): Path<String>,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        state.rooms.serve(doc_id, socket).await;
    })
}
