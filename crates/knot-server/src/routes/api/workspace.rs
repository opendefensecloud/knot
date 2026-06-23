//! GET    /api/workspace
//! GET    /api/workspace/members
//! POST   /api/workspace/members        body: {email, role}
//! PATCH  /api/workspace/members/:id    body: {role}
//! DELETE /api/workspace/members/:id

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use knot_storage::WorkspaceRole;
use knot_storage::audit;
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
            "/api/workspace/members/:id",
            patch(change_role).delete(remove_member),
        )
}

#[derive(Serialize)]
struct WorkspaceResponse {
    id: String,
    slug: String,
    name: String,
    role: String,
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
    })
    .into_response()
}

#[derive(Serialize)]
struct MemberResponse {
    user_id: String,
    email: String,
    display_name: String,
    role: String,
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
    let Some(users) = state.users.clone() else {
        return internal();
    };
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };

    let user = match users.find_by_email(&body.email).await {
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
                let display = body
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| body.email.split('@').next().unwrap_or(&body.email));
                match users.create_local(&body.email, display, &hash).await {
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
    };
    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        _ => return internal(),
    };
    if let Err(e) = workspaces.add_member(ws.id, user.id, role).await {
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
        audit::record(
            pool,
            ws.id,
            Some(ctx.user_id),
            "workspace.member.invite",
            "user",
            user.id,
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
