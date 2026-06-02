//! Postgres LISTEN consumer for `acl_invalidate`.
//!
//! On each notification:
//! 1. Parse payload as Uuid (doc_id).
//! 2. Evict cache entries keyed on that doc.
//! 3. Best-effort delete of consumed outbox rows.
//!
//! Reconnects with 5s backoff on listener errors.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::AclCache;

const CHANNEL: &str = "acl_invalidate";

pub fn spawn_listener(
    pool: PgPool,
    cache: Arc<AclCache>,
    docs: Arc<dyn knot_storage::DocStore>,
    on_invalidate: Arc<dyn Fn(Uuid) + Send + Sync>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match run_once(&pool, &cache, &docs, &on_invalidate).await {
                Ok(()) => {
                    tracing::warn!("acl listener exited cleanly; reconnecting");
                }
                Err(e) => {
                    tracing::warn!(error=?e, "acl listener error; reconnecting in 5s");
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    })
}

async fn run_once(
    pool: &PgPool,
    cache: &AclCache,
    docs: &Arc<dyn knot_storage::DocStore>,
    on_invalidate: &Arc<dyn Fn(Uuid) + Send + Sync>,
) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    tracing::info!("acl listener subscribed to {CHANNEL}");
    loop {
        let n = listener.recv().await?;
        let payload = n.payload();
        match payload.parse::<Uuid>() {
            Ok(doc_id) => {
                tracing::debug!(%doc_id, "acl evict");
                cache.evict_doc(doc_id);
                on_invalidate(doc_id);
                // Walk descendants and evict each so inherit=true grants on
                // ancestors don't leave stale cache entries on children.
                match docs.descendant_ids(doc_id).await {
                    Ok(ids) => {
                        for child in ids {
                            cache.evict_doc(child);
                            on_invalidate(child);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error=?e, "descendant_ids; subtree may stale until TTL");
                    }
                }
                cache.run_pending_tasks().await;
                // GC the outbox row.
                let _ = sqlx::query("DELETE FROM acl_invalidations WHERE doc_id = $1")
                    .bind(doc_id)
                    .execute(pool)
                    .await;
            }
            Err(_) => {
                tracing::warn!(payload, "malformed acl_invalidate payload; evicting all");
                cache.evict_all();
                cache.run_pending_tasks().await;
            }
        }
    }
}
