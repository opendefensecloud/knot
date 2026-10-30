//! Postgres LISTEN consumer for `acl_invalidate`.
//!
//! On each notification (`knot_storage::invalidations::parse_payload`):
//! - A doc: evict the cache entries of the doc and every descendant, and
//!   revoke their sockets (`on_invalidate`). If the subtree can't be
//!   walked, fall back to invalidating everything (below).
//! - A workspace (a member's role change or removal): invalidate
//!   everything.
//! - Anything else (malformed): invalidate everything.
//!
//! Then best-effort delete the consumed outbox rows.
//!
//! "Everything" means: evict the whole cache, then have every open socket
//! re-check its access (`on_invalidate_all`).
//!
//! NOTIFYs sent while the listener has no connection are lost. sqlx
//! reconnects a dropped connection by itself; `try_recv` reports it, and
//! the listener invalidates everything. Every (re)subscription after an
//! error (with a 5s backoff) does the same.

use std::sync::Arc;
use std::time::Duration;

use knot_storage::invalidations::{self, Invalidated};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::AclCache;

/// What the listener does to open sockets when access changes.
#[derive(Clone)]
pub struct InvalidationHooks {
    /// The access to this doc changed: revoke its sockets. Called for the
    /// doc and each of its descendants.
    pub on_invalidate: Arc<dyn Fn(Uuid) + Send + Sync>,
    /// Access may have changed anywhere (a lost connection, a failed
    /// subtree walk, a malformed payload, or a member's role change or
    /// removal): have every open socket re-check its access. Called after
    /// the whole cache was evicted, so the re-check reads fresh access.
    pub on_invalidate_all: Arc<dyn Fn() + Send + Sync>,
}

/// Only `on_invalidate`, for a listener with no sockets to re-check.
impl<F> From<Arc<F>> for InvalidationHooks
where
    F: Fn(Uuid) + Send + Sync + 'static,
{
    fn from(on_invalidate: Arc<F>) -> Self {
        Self {
            on_invalidate,
            on_invalidate_all: Arc::new(|| {}),
        }
    }
}

pub fn spawn_listener(
    pool: PgPool,
    cache: Arc<AclCache>,
    docs: Arc<dyn knot_storage::DocStore>,
    hooks: impl Into<InvalidationHooks>,
) -> JoinHandle<()> {
    let hooks = hooks.into();
    tokio::spawn(async move {
        loop {
            match run_once(&pool, &cache, &docs, &hooks).await {
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
    hooks: &InvalidationHooks,
) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(invalidations::CHANNEL).await?;
    tracing::info!("acl listener subscribed to {}", invalidations::CHANNEL);
    // Anything cached, and any socket opened, before this point may predate
    // a NOTIFY that arrived while no listener was subscribed.
    invalidate_everything(cache, hooks).await;
    loop {
        // Not `recv`, which reconnects without a word and so hides the
        // NOTIFYs lost meanwhile.
        let Some(notification) = listener.try_recv().await? else {
            // sqlx has already subscribed again (eager reconnect), so a
            // change after this point is heard.
            tracing::warn!("acl listener lost its connection; invalidating everything");
            invalidate_everything(cache, hooks).await;
            continue;
        };
        let payload = notification.payload();
        match invalidations::parse_payload(payload) {
            Some(Invalidated::Doc(doc_id)) => {
                tracing::debug!(%doc_id, "acl evict");
                cache.evict_doc(doc_id);
                (hooks.on_invalidate)(doc_id);
                // Walk descendants and evict each so inherit=true grants on
                // ancestors don't leave stale cache entries on children.
                match docs.descendant_ids(doc_id).await {
                    Ok(ids) => {
                        for child in ids {
                            cache.evict_doc(child);
                            (hooks.on_invalidate)(child);
                        }
                        cache.run_pending_tasks().await;
                    }
                    Err(e) => {
                        // Without the subtree a restrict would leave its
                        // descendants readable from the cache and their
                        // sockets open; invalidate everything instead.
                        tracing::warn!(error=?e, "descendant_ids failed; invalidating everything");
                        invalidate_everything(cache, hooks).await;
                    }
                }
                // GC the outbox row.
                let _ = sqlx::query("DELETE FROM acl_invalidations WHERE doc_id = $1")
                    .bind(doc_id)
                    .execute(pool)
                    .await;
            }
            Some(Invalidated::Workspace(workspace_id)) => {
                tracing::debug!(%workspace_id, "acl evict all");
                invalidate_everything(cache, hooks).await;
                let _ = sqlx::query(
                    "DELETE FROM acl_invalidations WHERE workspace_id = $1 AND doc_id IS NULL",
                )
                .bind(workspace_id)
                .execute(pool)
                .await;
            }
            None => {
                tracing::warn!(
                    payload,
                    "malformed acl_invalidate payload; invalidating everything"
                );
                invalidate_everything(cache, hooks).await;
            }
        }
    }
}

/// Evict the whole cache, then have every open socket re-check its access,
/// which then reads fresh access.
async fn invalidate_everything(cache: &AclCache, hooks: &InvalidationHooks) {
    cache.evict_all();
    cache.run_pending_tasks().await;
    (hooks.on_invalidate_all)();
}
