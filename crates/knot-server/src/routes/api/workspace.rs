//! GET    /api/workspace
//! GET    /api/workspace/members
//! POST   /api/workspace/members        body: {email, role, password?, display_name?, kind?}
//! PATCH  /api/workspace/members/{id}    body: {role}
//! DELETE /api/workspace/members/{id}

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use knot_storage::WorkspaceRole;
use knot_storage::audit;
use knot_storage::user_store::UserKind;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/workspace", get(get_workspace))
        .route(
            "/api/workspace/members",
            get(list_members).post(invite_member),
        )
        .route(
            "/api/workspace/members/{id}",
            patch(change_role).delete(remove_member),
        )
}

#[derive(Serialize)]
struct WorkspaceResponse {
    id: String,
    slug: String,
    name: String,
    role: String,
    /// Days an archived doc stays restorable; the UI quotes it.
    archive_retention_days: u32,
}

async fn get_workspace(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = ctx(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        _ => return internal(),
    };
    Json(WorkspaceResponse {
        id: ws.id.to_string(),
        slug: ws.slug,
        name: ws.name,
        role: ctx.role.as_str().into(),
        archive_retention_days: state.config.archive_retention_days,
    })
    .into_response()
}

#[derive(Serialize)]
struct MemberResponse {
    user_id: String,
    email: String,
    display_name: String,
    role: String,
    kind: &'static str,
}

async fn list_members(State(state): State<AppState>, req: Request) -> Response {
    if ctx(&req).is_none() {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    }
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        _ => return internal(),
    };
    match workspaces.list_members(ws.id).await {
        Ok(members) => Json(
            members
                .into_iter()
                .map(|m| MemberResponse {
                    user_id: m.user_id.to_string(),
                    email: m.email,
                    display_name: m.display_name,
                    role: m.role.as_str().into(),
                    kind: m.kind.as_str(),
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list_members");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct InviteRequest {
    email: String,
    role: String,
    password: Option<String>,
    display_name: Option<String>,
    /// `"human"` (default) or `"bot"`.
    kind: Option<String>,
}

async fn invite_member(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = ctx(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if ctx.role != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Ok(body) = read_json::<InviteRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(role) = WorkspaceRole::parse(&body.role) else {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "workspace.invalid_role",
            "",
        );
    };
    let kind = match body.kind.as_deref() {
        None => UserKind::Human,
        Some(k) => match UserKind::parse(k) {
            Some(k) => k,
            None => {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "member.kind_invalid", "");
            }
        },
    };
    // S4: answer cleanly before the KB001 trigger would.
    if kind == UserKind::Bot && role == WorkspaceRole::Owner {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bot.owner_forbidden",
            "a bot can only be editor or viewer",
        );
    }
    let Some(users) = state.users.clone() else {
        return internal();
    };
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    let display_name = body
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| body.email.split('@').next().unwrap_or(&body.email));

    // Before any user is created: a failure here must not burn the email.
    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        Ok(None) => {
            tracing::error!("invite: no workspace");
            return internal();
        }
        Err(e) => {
            tracing::error!(error=?e, "invite workspace lookup");
            return internal();
        }
    };

    let user = if kind == UserKind::Bot {
        // A bot is always a fresh, credentialless user; any password is
        // ignored. An existing email is refused rather than reused, so a
        // human can never be turned into a bot. Removing a member revokes
        // its keys in the DB, so nothing old comes back either way. The user
        // and its membership commit together: an account no workspace lists
        // would hold the email for good.
        match users
            .create_bot_member(ws.id, &body.email, display_name, role)
            .await
        {
            Ok(u) => u,
            Err(knot_storage::UserStoreError::EmailExists) => {
                return json_err(
                    StatusCode::CONFLICT,
                    "workspace.email_taken",
                    "a bot needs an email no user has",
                );
            }
            Err(e) => {
                tracing::error!(error=?e, "invite create_bot_member");
                return internal();
            }
        }
    } else {
        match users.find_by_email(&body.email).await {
            // A removed bot is not re-added as a human; invite a new bot.
            Ok(Some(u)) if u.kind == UserKind::Bot => {
                return json_err(
                    StatusCode::CONFLICT,
                    "workspace.email_taken",
                    "that email belongs to a bot",
                );
            }
            Ok(Some(u)) => u,
            Ok(None) => match body.password.as_deref() {
                Some(pw) if pw.chars().count() >= 8 => {
                    let hash = match state.hasher.hash(pw) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::error!(error=?e, "invite hash");
                            return internal();
                        }
                    };
                    match users.create_local(&body.email, display_name, &hash).await {
                        Ok(u) => u,
                        Err(e) => {
                            tracing::error!(error=?e, "invite create_local");
                            return internal();
                        }
                    }
                }
                Some(_) => {
                    return json_err(
                        StatusCode::BAD_REQUEST,
                        "auth.weak_password",
                        "password too short",
                    );
                }
                None => {
                    return json_err(
                        StatusCode::NOT_FOUND,
                        "workspace.user_not_found",
                        "user must exist or include a password",
                    );
                }
            },
            Err(e) => {
                tracing::error!(error=?e, "invite lookup");
                return internal();
            }
        }
    };
    if kind != UserKind::Bot
        && let Err(e) = workspaces.add_member(ws.id, user.id, role).await
    {
        match e {
            knot_storage::WorkspaceStoreError::Sqlx(ref s) if is_unique_violation(s) => {
                return json_err(StatusCode::CONFLICT, "workspace.already_member", "");
            }
            _ => {
                tracing::error!(error=?e, "invite add_member");
                return internal();
            }
        }
    }
    if let Some(pool) = state.pool.as_ref() {
        let data = serde_json::json!({ "kind": user.kind.as_str() });
        audit::record_with_data(
            pool,
            ws.id,
            Some(ctx.user_id),
            "workspace.member.invite",
            "user",
            user.id,
            &data,
        )
        .await;
    }
    StatusCode::CREATED.into_response()
}

#[derive(Deserialize)]
struct ChangeRoleRequest {
    role: String,
}

async fn change_role(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = ctx(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if ctx.role != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Ok(body) = read_json::<ChangeRoleRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(new_role) = WorkspaceRole::parse(&body.role) else {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "workspace.invalid_role",
            "",
        );
    };
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        _ => return internal(),
    };

    // S4: a bot can never be promoted to owner; answer before KB001 does.
    if new_role == WorkspaceRole::Owner {
        let Some(users) = state.users.clone() else {
            return internal();
        };
        match users.find_by_id(user_id).await {
            Ok(Some(u)) if u.kind == UserKind::Bot => {
                return json_err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "bot.owner_forbidden",
                    "a bot can only be editor or viewer",
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error=?e, "change_role lookup");
                return internal();
            }
        }
    }

    if new_role != WorkspaceRole::Owner {
        let current = workspaces
            .get_member_role(ws.id, user_id)
            .await
            .ok()
            .flatten();
        if current == Some(WorkspaceRole::Owner) {
            let owners = workspaces.count_owners(ws.id).await.unwrap_or(0);
            if owners <= 1 {
                return json_err(
                    StatusCode::CONFLICT,
                    "workspace.last_owner",
                    "cannot demote the last owner",
                );
            }
        }
    }

    if let Err(e) = workspaces.update_role(ws.id, user_id, new_role).await {
        tracing::error!(error=?e, "update_role");
        return internal();
    }
    // The workspace role is the member's base role in every doc, so drop
    // this replica's cache wholesale (role changes are rare), at once: a
    // demoted owner must not restrict a page, and so take it over, on a
    // cached owner role. Every replica's listener follows the workspace-wide
    // `member-role` invalidation, evicting too and closing open sockets for
    // a re-check.
    if let Some(acl) = state.acl.as_ref() {
        acl.evict_all();
        acl.run_pending_tasks().await;
    }
    if let Some(pool) = state.pool.as_ref() {
        audit::record(
            pool,
            ws.id,
            Some(ctx.user_id),
            "workspace.member.role",
            "user",
            user_id,
        )
        .await;
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn remove_member(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = ctx(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if ctx.role != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        _ => return internal(),
    };
    let current = workspaces
        .get_member_role(ws.id, user_id)
        .await
        .ok()
        .flatten();
    if current == Some(WorkspaceRole::Owner) {
        let owners = workspaces.count_owners(ws.id).await.unwrap_or(0);
        if owners <= 1 {
            return json_err(
                StatusCode::CONFLICT,
                "workspace.last_owner",
                "cannot remove the last owner",
            );
        }
    }
    if let Err(e) = workspaces.remove_member(ws.id, user_id).await {
        tracing::error!(error=?e, "remove_member");
        return internal();
    }
    // The member's grants are gone and inherited grants reach whole
    // subtrees, so drop this replica's cache wholesale (member removal is
    // rare) rather than let a cached access outlive a re-invite. Every
    // replica's listener follows the `member-remove` invalidations, the
    // workspace-wide one closing open sockets for a re-check.
    if let Some(acl) = state.acl.as_ref() {
        acl.evict_all();
        acl.run_pending_tasks().await;
    }
    if let Some(pool) = state.pool.as_ref() {
        audit::record(
            pool,
            ws.id,
            Some(ctx.user_id),
            "workspace.member.remove",
            "user",
            user_id,
        )
        .await;
    }
    StatusCode::NO_CONTENT.into_response()
}

fn ctx(req: &Request) -> Option<AuthContext> {
    req.extensions().get::<AuthContext>().cloned()
}

async fn read_json<T: serde::de::DeserializeOwned>(req: Request) -> Result<T, ()> {
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use axum::body::Body;

    use super::*;
    use crate::auth::log_capture;

    fn invite(body: serde_json::Value) -> Request {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/workspace/members")
            .body(Body::from(body.to_string()))
            .unwrap();
        req.extensions_mut().insert(AuthContext {
            user_id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            role: WorkspaceRole::Owner,
        });
        req
    }

    fn bodies() -> [serde_json::Value; 2] {
        [
            serde_json::json!({"email": "b@x.test", "role": "editor", "kind": "bot"}),
            serde_json::json!({"email": "h@x.test", "role": "editor", "password": "longenough1"}),
        ]
    }

    /// Catches the workspace being fetched after the user is created: the
    /// failed invite would leave a user behind, its email burned on retry.
    #[tokio::test]
    async fn invite_without_a_workspace_creates_no_user_and_says_why() {
        let state = AppState::with_pool(knot_test_support::fresh_db().await.pool);
        let (logs, _guard) = log_capture::capture();
        for body in bodies() {
            let r = invite_member(State(state.clone()), invite(body)).await;
            assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        assert_eq!(state.users.as_ref().unwrap().count().await.unwrap(), 0);
        assert_eq!(logs.text().matches("invite: no workspace").count(), 2);
    }

    /// Catches a workspace lookup failure answering 500 with no log.
    #[tokio::test]
    async fn invite_logs_a_failed_workspace_lookup() {
        let pool = knot_test_support::fresh_db().await.pool;
        let state = AppState::with_pool(pool.clone());
        pool.close().await;
        let (logs, _guard) = log_capture::capture();
        for body in bodies() {
            let r = invite_member(State(state.clone()), invite(body)).await;
            assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        assert_eq!(logs.text().matches("invite workspace lookup").count(), 2);
    }
}
