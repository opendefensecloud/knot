//! knot spike server library — exports `router()` for tests.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use knot_crdt::YrsEngine;
use knot_storage::Pool;

pub mod protocol;
pub mod room;
pub mod routes;

use room::Rooms;

#[derive(Clone)]
pub struct AppState {
    pub rooms: Arc<Rooms>,
    pub pool: Option<Pool>,
}

impl AppState {
    pub fn in_memory() -> Self {
        Self {
            rooms: Arc::new(Rooms::new(YrsEngine)),
            pool: None,
        }
    }

    pub fn with_pool(pool: Pool) -> Self {
        Self {
            rooms: Arc::new(Rooms::new(YrsEngine)),
            pool: Some(pool),
        }
    }
}

/// In-memory router (used by tests + the spike main without DB).
pub fn router() -> Router {
    router_with_state(AppState::in_memory())
}

pub fn router_with_state(state: AppState) -> Router {
    Router::new()
        .route("/collab/:doc_id", get(collab_upgrade))
        .merge(routes::health::router())
        .with_state(state)
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
