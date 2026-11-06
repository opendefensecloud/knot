//! GET    /api/auth/api-keys            ?user_id=<uuid> (owner, bot target only)
//! POST   /api/auth/api-keys            body: {name, scopes, expires_in_days?, user_id?}
//! DELETE /api/auth/api-keys/{id}       ?user_id=<uuid> (owner, bot target only)
//!
//! Session principals only (S2): an API key can never mint, list or revoke
//! keys, so no key can escalate its own scopes. The plaintext key exists
//! exactly once, in the 201 response (S7); no handler here logs or traces it.

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
};
use chrono::{DateTime, Duration, Utc};
use knot_auth::api_key::{self, ApiKeySecret, PrincipalKind, TtlError};
use knot_storage::WorkspaceRole;
use knot_storage::api_key_store::{
    ApiKey, ApiKeyStoreError, MAX_LIVE_KEYS_PER_USER, NewApiKey, Scopes,
};
use knot_storage::audit;
use knot_storage::user_store::{User, UserKind};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{ApiPrincipal, AuthContext, SessionHash};
use crate::http_error::json_err;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/api-keys", get(list_keys).post(create_key))
        .route("/api/auth/api-keys/{id}", delete(revoke_key))
}

#[derive(Serialize)]
struct KeyResponse {
    id: Uuid,
    name: String,
    scopes: Vec<String>,
    /// Who minted the key, so owners can tell who holds a bot's keys.
    created_by: Uuid,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

impl From<ApiKey> for KeyResponse {
    fn from(k: ApiKey) -> Self {
        Self {
            id: k.id,
            name: k.name,
            scopes: k.scopes.names(),
            created_by: k.created_by,
            created_at: k.created_at,
            expires_at: k.expires_at,
            last_used_at: k.last_used_at,
            revoked_at: k.revoked_at,
        }
    }
}

#[derive(Deserialize)]
struct TargetQuery {
    user_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct CreateRequest {
    name: String,
    scopes: Vec<String>,
    #[serde(default)]
    expires_in_days: Option<i64>,
    #[serde(default)]
    user_id: Option<Uuid>,
}

/// The single authorization gate for key management. Returns the caller's
/// context and the target user.
///
/// 1. An API-key principal is refused (S2).
/// 2. No `user_id` targets the caller; another user only when the caller is
///    workspace owner and the target is a bot member of the caller's
///    workspace.
/// 3. A session whose own user is a bot is refused: bots have no
///    interactive credentials, and their keys are minted by owners only.
fn authorize<'a>(
    state: &'a AppState,
    req: &Request,
    user_id: Option<Uuid>,
) -> impl Future<Output = Result<(AuthContext, User), Response>> + Send + 'a {
    // Read the extensions synchronously: holding `&Request` across an await
    // would make the handler future `!Send` (`Body` is not `Sync`).
    let is_key = req.extensions().get::<ApiPrincipal>().is_some();
    let ctx = req.extensions().get::<AuthContext>().cloned();
    authorize_target(state, is_key, ctx, user_id)
}

async fn authorize_target(
    state: &AppState,
    is_key: bool,
    ctx: Option<AuthContext>,
    user_id: Option<Uuid>,
) -> Result<(AuthContext, User), Response> {
    if is_key {
        return Err(session_required());
    }
    let Some(ctx) = ctx else {
        return Err(json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "",
        ));
    };
    let Some(users) = state.users.as_ref() else {
        return Err(internal());
    };
    let target_id = user_id.unwrap_or(ctx.user_id);
    let target = match users.find_by_id(target_id).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = ?e, "api key target lookup");
            return Err(internal());
        }
    };
    if target_id == ctx.user_id {
        return match target {
            Some(u) if u.kind == UserKind::Bot => {
                Err(json_err(StatusCode::FORBIDDEN, "bot.forbidden", ""))
            }
            Some(u) => Ok((ctx, u)),
            None => Err(internal()),
        };
    }
    let Some(u) = target.filter(|u| ctx.role == WorkspaceRole::Owner && u.kind == UserKind::Bot)
    else {
        return Err(foreign_forbidden());
    };
    // A bot of another workspace is not this owner's to manage.
    let Some(workspaces) = state.workspaces.as_ref() else {
        return Err(internal());
    };
    match workspaces.get_member_role(ctx.workspace_id, u.id).await {
        Ok(Some(_)) => Ok((ctx, u)),
        Ok(None) => Err(foreign_forbidden()),
        Err(e) => {
            tracing::error!(error = ?e, "api key target membership");
            Err(internal())
        }
    }
}

/// Which TTL policy applies to `u`'s keys. The pure-OIDC rule (human, no
/// password, linked identity) lives only here.
pub(crate) fn principal_kind(u: &User) -> PrincipalKind {
    if u.kind == UserKind::Bot {
        PrincipalKind::Bot
    } else if u.password_hash.is_none() && u.oidc_subject.is_some() {
        PrincipalKind::Oidc
    } else {
        PrincipalKind::Local
    }
}

#[tracing::instrument(skip_all, name = "api_keys.list")]
async fn list_keys(
    State(state): State<AppState>,
    Query(q): Query<TargetQuery>,
    req: Request,
) -> Response {
    let (ctx, target) = match authorize(&state, &req, q.user_id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(store) = state.api_keys.as_ref() else {
        return internal();
    };
    match store.list_for_user(ctx.workspace_id, target.id).await {
        Ok(keys) => {
            Json(keys.into_iter().map(KeyResponse::from).collect::<Vec<_>>()).into_response()
        }
        Err(e) => {
            tracing::error!(error = ?e, "api key list");
            internal()
        }
    }
}

#[derive(Serialize)]
struct CreatedResponse<'a> {
    id: Uuid,
    /// The plaintext key: returned here and nowhere else.
    key: &'a str,
    name: String,
    scopes: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
}

#[tracing::instrument(skip_all, name = "api_keys.create")]
async fn create_key(State(state): State<AppState>, req: Request) -> Response {
    // Authorization needs the user_id from the body; refuse key principals
    // before reading it.
    if req.extensions().get::<ApiPrincipal>().is_some() {
        return session_required();
    }
    // The store re-checks this session inside the insert's transaction, so
    // a password change or logout that ends it can't race the mint (S5).
    let Some(SessionHash(session_hash)) = req.extensions().get::<SessionHash>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Ok(body) = serde_json::from_slice::<CreateRequest>(&bytes) else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let req = Request::from_parts(parts, axum::body::Body::empty());
    let (ctx, target) = match authorize(&state, &req, body.user_id).await {
        Ok(v) => v,
        Err(r) => return r,
    };

    let Ok(scopes) = Scopes::from_names(&body.scopes) else {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "api_key.scopes_invalid",
            "scopes must be a non-empty subset of read, create, update, delete",
        );
    };
    let name = body.name.trim();
    if !(1..=100).contains(&name.chars().count()) {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "api_key.name_invalid",
            "name must be 1 to 100 characters",
        );
    }
    let days = match api_key::validate_ttl_days(principal_kind(&target), body.expires_in_days) {
        Ok(d) => d,
        Err(TtlError::TooShort) => return ttl_too_short(),
        Err(TtlError::TooLong) => return ttl_too_long(),
    };
    let expires_at = match days {
        None => None,
        Some(d) => {
            let now = match db_now(&state).await {
                Ok(t) => t,
                Err(r) => return r,
            };
            match Duration::try_days(d).and_then(|d| now.checked_add_signed(d)) {
                Some(t) => Some(t),
                None => {
                    return json_err(
                        StatusCode::BAD_REQUEST,
                        "api_key.ttl_too_long",
                        "expiry too far in the future",
                    );
                }
            }
        }
    };

    let Some(store) = state.api_keys.as_ref() else {
        return internal();
    };
    let secret = ApiKeySecret::generate();
    let key_hash = api_key::hash(secret.as_str(), &state.session_key);
    let key = match store
        .create(NewApiKey {
            workspace_id: ctx.workspace_id,
            user_id: target.id,
            name,
            key_hash: &key_hash,
            scopes: &scopes,
            created_by: ctx.user_id,
            expires_at,
            session_hash: Some(&session_hash),
        })
        .await
    {
        Ok(k) => k,
        // The KB003 trigger is the backstop for the OIDC cap (S6).
        Err(ApiKeyStoreError::TtlRejected) => return ttl_too_long(),
        Err(ApiKeyStoreError::LimitReached) => {
            return json_err(
                StatusCode::CONFLICT,
                "api_key.limit_reached",
                &format!(
                    "a user can hold at most {MAX_LIVE_KEYS_PER_USER} live keys; revoke one first"
                ),
            );
        }
        // The session, or the membership, this request was admitted with
        // ended while it ran (password change, logout, removal).
        Err(ApiKeyStoreError::SessionEnded) => {
            return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
        }
        // Lost the owner role, or the bot left, while the request ran.
        Err(ApiKeyStoreError::NotPermitted) => return foreign_forbidden(),
        Err(e) => {
            tracing::error!(error = ?e, "api key create");
            return internal();
        }
    };

    if let Some(pool) = state.pool.as_ref() {
        // Never the secret nor its hash (S7).
        let data = serde_json::json!({
            "key_id": key.id,
            "scopes": key.scopes.names(),
            "expires_at": key.expires_at,
        });
        audit::record_with_data(
            pool,
            ctx.workspace_id,
            Some(ctx.user_id),
            "api-key.create",
            "user",
            target.id,
            &data,
        )
        .await;
    }

    (
        StatusCode::CREATED,
        Json(CreatedResponse {
            id: key.id,
            key: secret.as_str(),
            name: key.name,
            scopes: key.scopes.names(),
            expires_at: key.expires_at,
        }),
    )
        .into_response()
}

#[tracing::instrument(skip_all, name = "api_keys.revoke")]
async fn revoke_key(
    State(state): State<AppState>,
    Path(key_id): Path<Uuid>,
    Query(q): Query<TargetQuery>,
    req: Request,
) -> Response {
    let (ctx, target) = match authorize(&state, &req, q.user_id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(store) = state.api_keys.as_ref() else {
        return internal();
    };
    let owner = target.id;
    match store.revoke(ctx.workspace_id, owner, key_id).await {
        Ok(true) => {}
        // Not the target's key, unknown, or already revoked: one answer.
        Ok(false) => {
            return json_err(StatusCode::NOT_FOUND, "api_key.not_found", "");
        }
        Err(e) => {
            tracing::error!(error = ?e, "api key revoke");
            return internal();
        }
    }
    if let Some(pool) = state.pool.as_ref() {
        audit::record(
            pool,
            ctx.workspace_id,
            Some(ctx.user_id),
            "api-key.revoke",
            "user",
            owner,
        )
        .await;
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The database clock. The KB003 trigger measures the 90-day cap against
/// its own `now()`, which is never earlier than this, so a key asking for
/// exactly the cap is not refused because the app clock runs ahead.
async fn db_now(state: &AppState) -> Result<DateTime<Utc>, Response> {
    let Some(pool) = state.pool.as_ref() else {
        return Ok(Utc::now());
    };
    sqlx::query_scalar::<_, DateTime<Utc>>("SELECT now()")
        .fetch_one(pool)
        .await
        .map_err(|e| {
            tracing::error!(error = ?e, "db clock");
            internal()
        })
}

fn session_required() -> Response {
    json_err(
        StatusCode::FORBIDDEN,
        "api_key.session_required",
        "API keys can only be managed from a session",
    )
}

fn foreign_forbidden() -> Response {
    json_err(
        StatusCode::FORBIDDEN,
        "api_key.foreign_forbidden",
        "only a workspace owner can manage a bot's keys",
    )
}

fn ttl_too_short() -> Response {
    json_err(
        StatusCode::BAD_REQUEST,
        "api_key.ttl_too_short",
        "keys must live at least 30 days",
    )
}

fn ttl_too_long() -> Response {
    json_err(
        StatusCode::BAD_REQUEST,
        "api_key.ttl_too_long",
        "single-sign-on users' keys expire within 90 days",
    )
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use knot_storage::Scopes;

    use super::*;
    use crate::auth::SessionHash;

    async fn code_of(r: Response) -> (StatusCode, serde_json::Value) {
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (status, v["error"]["code"].clone())
    }

    fn ctx() -> AuthContext {
        AuthContext {
            user_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            role: WorkspaceRole::Owner,
        }
    }

    /// S2 behind SESSION_ONLY: should a route rename slip past the scope
    /// table, the handlers still refuse every key.
    #[tokio::test]
    async fn authorize_target_refuses_a_key_principal() {
        let state = AppState::with_pool(knot_test_support::fresh_db().await.pool);
        for user_id in [None, Some(Uuid::new_v4())] {
            let r = authorize_target(&state, true, Some(ctx()), user_id)
                .await
                .expect_err("a key principal is refused");
            assert_eq!(
                code_of(r).await,
                (
                    StatusCode::FORBIDDEN,
                    serde_json::json!("api_key.session_required")
                )
            );
        }
    }

    /// The refusal comes before the body is read: an unreadable body still
    /// answers 403, not 400.
    #[tokio::test]
    async fn create_key_refuses_a_key_principal_before_reading_the_body() {
        let state = AppState::with_pool(knot_test_support::fresh_db().await.pool);
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/auth/api-keys")
            .body(Body::from("not json"))
            .unwrap();
        req.extensions_mut().insert(ctx());
        req.extensions_mut().insert(SessionHash(vec![1; 32]));
        req.extensions_mut().insert(ApiPrincipal {
            key_id: Uuid::new_v4(),
            scopes: Scopes::from_names(&["read".to_string()]).unwrap(),
        });
        let r = create_key(State(state), req).await;
        assert_eq!(
            code_of(r).await,
            (
                StatusCode::FORBIDDEN,
                serde_json::json!("api_key.session_required")
            )
        );
    }
}
