//! Background task: forward `doc_comments` Postgres notifications to active
//! collab rooms so connected clients refetch their comments. Mirrors the
//! `acl_invalidate` listener. Never boots a room.

use std::sync::Arc;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::protocol::{MSG_COMMENTS, append_var_uint};

const CHANNEL: &str = "doc_comments";

pub fn spawn(pool: PgPool, rooms: Arc<knot_crdt::Rooms>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_once(&pool, &rooms).await {
                tracing::warn!(error=?e, "comments listener error; reconnecting in 5s");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    })
}

async fn run_once(pool: &PgPool, rooms: &Arc<knot_crdt::Rooms>) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    tracing::info!("comments listener subscribed to {CHANNEL}");
    loop {
        let note = listener.recv().await?;
        let payload = note.payload();
        let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        let Some(doc_id) = json
            .get("doc_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        let body = payload.as_bytes();
        let mut frame = Vec::with_capacity(body.len() + 6);
        frame.push(MSG_COMMENTS);
        append_var_uint(&mut frame, body.len() as u64);
        frame.extend_from_slice(body);
        rooms.notify_doc_comments(doc_id, frame).await;
    }
}
