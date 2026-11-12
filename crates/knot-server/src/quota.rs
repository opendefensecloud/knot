//! Optional page and file-byte quotas (issue #88), per user and per
//! workspace, all off (0) by default.
//!
//! The handlers check them before they write anything: `POST /api/docs`
//! and from-template (+1 page), duplicate (+1 page before the permit wait,
//! then the copy's pages and file bytes once planned), import (its pages
//! and declared file bytes, once) and blob upload (the file's size). Usage
//! is counted in Postgres (`knot_storage::quota`), so a quota holds across
//! replicas; a quota that is off costs no query.
//!
//! **Best effort:** concurrent writes can both pass a check, so a quota can
//! be exceeded by what runs at once. A strict check would serialise every
//! create behind one lock.
//!
//! A refusal is `403 quota.exceeded`, counted in
//! `knot_quota_exceeded_total{quota,scope}` and audited as
//! `quota.exceeded`, at most once per user, quota and scope per
//! [`AUDIT_WINDOW`], so a script at its quota can't turn refusals into
//! audit rows.

use std::future::Future;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::{json_err, json_err_with_details};
use crate::rate_limit::{KeyedBuckets, Rate};

/// The page quota's label, in metrics, audit data and the refusal.
pub const PAGES: &str = "pages";
/// The file-byte quota's label.
pub const BLOB_BYTES: &str = "blob_bytes";
/// A quota on what one user created.
pub const USER: &str = "user";
/// A quota on the whole workspace.
pub const WORKSPACE: &str = "workspace";

/// At most one `quota.exceeded` audit row per user, quota and scope in this
/// long.
pub const AUDIT_WINDOW: Duration = Duration::from_secs(10 * 60);

/// A write a quota refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRefusal {
    /// [`PAGES`] or [`BLOB_BYTES`].
    pub quota: &'static str,
    /// [`USER`] or [`WORKSPACE`].
    pub scope: &'static str,
    pub limit: u64,
    /// What the scope used before the write.
    pub used: u64,
    /// What the write would add.
    pub requested: u64,
}

/// Why a quota check didn't pass.
#[derive(Debug)]
pub enum QuotaCheckError {
    /// A quota refused the write.
    Exceeded(QuotaRefusal),
    /// The usage couldn't be counted.
    Internal(String),
}

/// Whether `requested` more pages may be created by `ctx`'s user: the user
/// quota first, then the workspace quota. A quota that is off is never
/// counted.
pub async fn check_pages(
    state: &AppState,
    ctx: &AuthContext,
    requested: u64,
) -> Result<(), QuotaCheckError> {
    let workspace_id = ctx.workspace_id;
    let user_id = ctx.user_id;
    check_against(
        PAGES,
        state.config.quota_pages_per_user,
        state.config.quota_pages_per_workspace,
        requested,
        |scope| {
            let pool = state.pool.clone();
            async move {
                let pool = pool.ok_or_else(|| "pool not configured".to_string())?;
                let counted = if scope == USER {
                    knot_storage::quota::pages_created_by(&pool, workspace_id, user_id).await
                } else {
                    knot_storage::quota::pages_in_workspace(&pool, workspace_id).await
                };
                counted.map_err(|e| e.to_string())
            }
        },
    )
    .await
}

/// Whether `requested` more file bytes may be stored by `ctx`'s user, as
/// [`check_pages`] does for pages.
pub async fn check_blob_bytes(
    state: &AppState,
    ctx: &AuthContext,
    requested: u64,
) -> Result<(), QuotaCheckError> {
    let workspace_id = ctx.workspace_id;
    let user_id = ctx.user_id;
    check_against(
        BLOB_BYTES,
        state.config.quota_blob_bytes_per_user,
        state.config.quota_blob_bytes_per_workspace,
        requested,
        |scope| {
            let pool = state.pool.clone();
            async move {
                let pool = pool.ok_or_else(|| "pool not configured".to_string())?;
                let counted = if scope == USER {
                    knot_storage::quota::blob_bytes_created_by(&pool, workspace_id, user_id).await
                } else {
                    knot_storage::quota::blob_bytes_in_workspace(&pool, workspace_id).await
                };
                counted.map_err(|e| e.to_string())
            }
        },
    )
    .await
}

/// [`check_pages`], with a refusal recorded ([`record_refusal`]) and
/// answered ([`refusal_response`]), and a count that failed answered 500.
pub async fn enforce_pages(
    state: &AppState,
    ctx: &AuthContext,
    requested: u64,
) -> Result<(), Response> {
    let outcome = check_pages(state, ctx, requested).await;
    answer(state, ctx, outcome).await
}

/// [`check_blob_bytes`], answered as [`enforce_pages`] does.
pub async fn enforce_blob_bytes(
    state: &AppState,
    ctx: &AuthContext,
    requested: u64,
) -> Result<(), Response> {
    let outcome = check_blob_bytes(state, ctx, requested).await;
    answer(state, ctx, outcome).await
}

async fn answer(
    state: &AppState,
    ctx: &AuthContext,
    outcome: Result<(), QuotaCheckError>,
) -> Result<(), Response> {
    match outcome {
        Ok(()) => Ok(()),
        Err(QuotaCheckError::Exceeded(refusal)) => {
            record_refusal(state, ctx, &refusal).await;
            Err(refusal_response(&refusal))
        }
        Err(QuotaCheckError::Internal(error)) => {
            tracing::error!(%error, "quota: usage count failed");
            Err(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""))
        }
    }
}

/// The quota check itself: nothing is counted when nothing is requested or
/// both quotas are off; otherwise the user quota (`per_user`) is checked
/// before the workspace quota (`per_workspace`), each counted by
/// `count_used` only when it is on. A write passes when what is used plus
/// what it adds stays within the limit.
async fn check_against<Count, Counted>(
    quota: &'static str,
    per_user: u64,
    per_workspace: u64,
    requested: u64,
    mut count_used: Count,
) -> Result<(), QuotaCheckError>
where
    Count: FnMut(&'static str) -> Counted,
    Counted: Future<Output = Result<u64, String>>,
{
    if requested == 0 {
        return Ok(());
    }
    for (scope, limit) in [(USER, per_user), (WORKSPACE, per_workspace)] {
        if limit == 0 {
            continue;
        }
        let used = count_used(scope).await.map_err(QuotaCheckError::Internal)?;
        if used.saturating_add(requested) > limit {
            return Err(QuotaCheckError::Exceeded(QuotaRefusal {
                quota,
                scope,
                limit,
                used,
                requested,
            }));
        }
    }
    Ok(())
}

/// `403 quota.exceeded`, with `details {quota, scope, limit}`.
pub fn refusal_response(refusal: &QuotaRefusal) -> Response {
    let what = if refusal.quota == PAGES {
        "page"
    } else {
        "file storage"
    };
    let whose = if refusal.scope == USER {
        "your"
    } else {
        "this workspace's"
    };
    json_err_with_details(
        StatusCode::FORBIDDEN,
        "quota.exceeded",
        &format!("{whose} {what} quota is reached"),
        json!({
            "quota": refusal.quota,
            "scope": refusal.scope,
            "limit": refusal.limit,
        }),
    )
}

/// Counts `refusal` in `knot_quota_exceeded_total`, and audits it as
/// `quota.exceeded` (`target_kind = "user"`, data `{quota, scope, limit,
/// used, requested}`) unless the same user, quota and scope was audited in
/// the last [`AUDIT_WINDOW`]. Best effort: an audit write that fails is
/// logged, and still uses up the window (the slot is taken before the
/// write), so that refusal has no audit row and the next one is audited
/// only after the window.
pub async fn record_refusal(state: &AppState, ctx: &AuthContext, refusal: &QuotaRefusal) {
    count_refusal(refusal);
    if !state
        .quota_audit
        .should_record(ctx.user_id, refusal.quota, refusal.scope, Instant::now())
    {
        return;
    }
    tracing::warn!(
        user_id = %ctx.user_id,
        quota = refusal.quota,
        scope = refusal.scope,
        limit = refusal.limit,
        used = refusal.used,
        requested = refusal.requested,
        "quota exceeded"
    );
    let Some(pool) = state.pool.as_ref() else {
        return;
    };
    knot_storage::audit::record_with_data(
        pool,
        ctx.workspace_id,
        Some(ctx.user_id),
        "quota.exceeded",
        "user",
        ctx.user_id,
        &json!({
            "quota": refusal.quota,
            "scope": refusal.scope,
            "limit": refusal.limit,
            "used": refusal.used,
            "requested": refusal.requested,
        }),
    )
    .await;
}

/// `knot_quota_exceeded_total{quota,scope}` +1. Labels never carry the user.
fn count_refusal(refusal: &QuotaRefusal) {
    metrics::counter!(
        "knot_quota_exceeded_total",
        "quota" => refusal.quota,
        "scope" => refusal.scope,
    )
    .increment(1);
}

/// Which refusals get an audit row: one per user, quota and scope per
/// [`AUDIT_WINDOW`]. A token bucket of one token that refills in the
/// window, so the map prunes itself and is key-capped like the rate
/// limiters.
pub struct QuotaAuditThrottle {
    audited: KeyedBuckets<(Uuid, &'static str, &'static str)>,
}

impl QuotaAuditThrottle {
    pub fn new() -> Self {
        Self {
            audited: KeyedBuckets::new(Rate {
                burst: 1.0,
                per_second: 1.0 / AUDIT_WINDOW.as_secs_f64(),
            }),
        }
    }

    /// Whether a refusal of `user_id` by `quota` in `scope` at `now` is to be
    /// audited; true at most once per [`AUDIT_WINDOW`] for each.
    pub fn should_record(
        &self,
        user_id: Uuid,
        quota: &'static str,
        scope: &'static str,
        now: Instant,
    ) -> bool {
        self.audited
            .try_take(&(user_id, quota, scope), 1.0, now)
            .is_ok()
    }
}

impl Default for QuotaAuditThrottle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::to_bytes;
    use knot_config::Config;
    use knot_storage::WorkspaceRole;

    use super::*;

    /// A counter that fails the test if it is asked for anything.
    fn never_counted(scope: &'static str) -> std::future::Ready<Result<u64, String>> {
        panic!("{scope} usage was counted");
    }

    /// A counter answering `user_used` and `workspace_used`, noting each
    /// scope it was asked for.
    fn counter(
        user_used: u64,
        workspace_used: u64,
        asked: &Arc<Mutex<Vec<&'static str>>>,
    ) -> impl FnMut(&'static str) -> std::future::Ready<Result<u64, String>> {
        let asked = asked.clone();
        move |scope| {
            asked.lock().unwrap().push(scope);
            std::future::ready(Ok(if scope == USER {
                user_used
            } else {
                workspace_used
            }))
        }
    }

    fn refusal(scope: &'static str, limit: u64, used: u64, requested: u64) -> QuotaRefusal {
        QuotaRefusal {
            quota: PAGES,
            scope,
            limit,
            used,
            requested,
        }
    }

    fn exceeded(outcome: Result<(), QuotaCheckError>) -> QuotaRefusal {
        match outcome {
            Err(QuotaCheckError::Exceeded(refusal)) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn quotas_that_are_off_are_never_counted() {
        assert!(check_against(PAGES, 0, 0, 5, never_counted).await.is_ok());
        assert!(
            check_against(BLOB_BYTES, 0, 0, u64::MAX, never_counted)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_write_adding_nothing_is_never_counted() {
        assert!(check_against(PAGES, 1, 1, 0, never_counted).await.is_ok());
    }

    #[tokio::test]
    async fn a_write_up_to_the_limit_passes_and_one_past_it_is_refused() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        assert!(
            check_against(PAGES, 3, 0, 1, counter(2, 0, &asked))
                .await
                .is_ok()
        );
        assert_eq!(
            exceeded(check_against(PAGES, 3, 0, 2, counter(2, 0, &asked)).await),
            refusal(USER, 3, 2, 2)
        );
        assert_eq!(
            *asked.lock().unwrap(),
            [USER, USER],
            "only the user quota is on"
        );
    }

    #[tokio::test]
    async fn the_user_quota_is_checked_before_the_workspace_quota() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        assert_eq!(
            exceeded(check_against(PAGES, 3, 10, 1, counter(3, 10, &asked)).await),
            refusal(USER, 3, 3, 1)
        );
        assert_eq!(
            *asked.lock().unwrap(),
            [USER],
            "refused before the workspace count"
        );

        let asked = Arc::new(Mutex::new(Vec::new()));
        assert_eq!(
            exceeded(check_against(PAGES, 3, 10, 1, counter(1, 10, &asked)).await),
            refusal(WORKSPACE, 10, 10, 1)
        );
        assert_eq!(*asked.lock().unwrap(), [USER, WORKSPACE]);
    }

    #[tokio::test]
    async fn only_the_workspace_quota_is_counted_when_the_user_quota_is_off() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        assert!(
            check_against(BLOB_BYTES, 0, 100, 40, counter(0, 60, &asked))
                .await
                .is_ok()
        );
        assert_eq!(*asked.lock().unwrap(), [WORKSPACE]);
    }

    #[tokio::test]
    async fn huge_usage_does_not_overflow() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let refused = exceeded(
            check_against(BLOB_BYTES, u64::MAX - 1, 0, 2, counter(u64::MAX, 0, &asked)).await,
        );
        assert_eq!(refused.used, u64::MAX);
    }

    #[tokio::test]
    async fn a_count_that_fails_is_an_internal_error() {
        let outcome = check_against(PAGES, 1, 0, 1, |_| {
            std::future::ready(Err::<u64, String>("db down".into()))
        })
        .await;
        assert!(
            matches!(outcome, Err(QuotaCheckError::Internal(ref e)) if e == "db down"),
            "{outcome:?}"
        );
    }

    fn context() -> AuthContext {
        AuthContext {
            user_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            role: WorkspaceRole::Editor,
        }
    }

    /// The in-memory state has no pool: with the quotas off, the checks pass
    /// because nothing is counted.
    #[tokio::test]
    async fn the_default_config_counts_nothing() {
        let state = AppState::in_memory();
        let ctx = context();
        assert!(check_pages(&state, &ctx, 1).await.is_ok());
        assert!(
            check_blob_bytes(&state, &ctx, 10 * 1024 * 1024)
                .await
                .is_ok()
        );
        assert!(enforce_pages(&state, &ctx, 1).await.is_ok());
        assert!(enforce_blob_bytes(&state, &ctx, 1).await.is_ok());
    }

    #[tokio::test]
    async fn a_quota_on_reads_the_config_and_needs_the_pool() {
        let mut state = AppState::in_memory();
        state.config = Arc::new(Config {
            quota_blob_bytes_per_workspace: 5,
            ..Default::default()
        });
        let ctx = context();
        assert!(check_pages(&state, &ctx, 1).await.is_ok(), "pages are off");
        assert!(matches!(
            check_blob_bytes(&state, &ctx, 1).await,
            Err(QuotaCheckError::Internal(_))
        ));
        let response = enforce_blob_bytes(&state, &ctx, 1).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_refusal_is_a_403_naming_the_quota_scope_and_limit() {
        let response = refusal_response(&refusal(WORKSPACE, 10, 9, 2));
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            body_json(response).await,
            json!({"error": {
                "code": "quota.exceeded",
                "message": "this workspace's page quota is reached",
                "details": {"quota": "pages", "scope": "workspace", "limit": 10},
            }})
        );
        let response = refusal_response(&QuotaRefusal {
            quota: BLOB_BYTES,
            scope: USER,
            limit: 1024,
            used: 1000,
            requested: 100,
        });
        assert_eq!(
            body_json(response).await["error"],
            json!({
                "code": "quota.exceeded",
                "message": "your file storage quota is reached",
                "details": {"quota": "blob_bytes", "scope": "user", "limit": 1024},
            })
        );
    }

    #[test]
    fn audits_once_per_user_quota_and_scope_per_window() {
        let throttle = QuotaAuditThrottle::new();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let start = Instant::now();
        assert!(throttle.should_record(alice, PAGES, USER, start));
        assert!(!throttle.should_record(alice, PAGES, USER, start));
        assert!(!throttle.should_record(
            alice,
            PAGES,
            USER,
            start + AUDIT_WINDOW - Duration::from_secs(1)
        ));
        // Another user, quota or scope has a window of its own.
        assert!(throttle.should_record(bob, PAGES, USER, start));
        assert!(throttle.should_record(alice, BLOB_BYTES, USER, start));
        assert!(throttle.should_record(alice, PAGES, WORKSPACE, start));
        // After the window, the next refusal is audited again, once.
        let later = start + AUDIT_WINDOW + Duration::from_secs(1);
        assert!(throttle.should_record(alice, PAGES, USER, later));
        assert!(!throttle.should_record(alice, PAGES, USER, later));
    }

    /// Remembers the key of every counter increment.
    #[derive(Default)]
    struct Counters(Mutex<Vec<String>>);

    impl metrics::Recorder for Counters {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(
            &self,
            key: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            let labels: Vec<String> = key
                .labels()
                .map(|label| format!("{}={}", label.key(), label.value()))
                .collect();
            self.0
                .lock()
                .unwrap()
                .push(format!("{}{{{}}}", key.name(), labels.join(",")));
            metrics::Counter::noop()
        }
        fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            metrics::Gauge::noop()
        }
        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    #[test]
    fn every_refusal_is_counted_by_quota_and_scope() {
        let counters = Counters::default();
        metrics::with_local_recorder(&counters, || {
            count_refusal(&refusal(USER, 3, 3, 1));
            count_refusal(&QuotaRefusal {
                quota: BLOB_BYTES,
                scope: WORKSPACE,
                limit: 1,
                used: 1,
                requested: 1,
            });
        });
        assert_eq!(
            *counters.0.lock().unwrap(),
            [
                "knot_quota_exceeded_total{quota=pages,scope=user}",
                "knot_quota_exceeded_total{quota=blob_bytes,scope=workspace}",
            ]
        );
    }
}
