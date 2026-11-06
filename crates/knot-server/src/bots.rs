//! Application-level guards that keep bot accounts from taking ownership.
//!
//! The DB triggers (`KB001` on `workspace_members`, `KB002` on
//! `document_grants`) are the last line: they refuse a bot owner whatever
//! the code path. These guards answer the same refusal as a clean 4xx
//! before the store call, instead of a trigger error turned 500.

use axum::{http::StatusCode, response::Response};
use knot_storage::user_store::UserKind;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

/// Whether `user_id` is a bot (`users.kind = 'bot'`). An unknown user is
/// not a bot. 500 when the users store is missing or fails.
pub async fn is_bot(state: &AppState, user_id: Uuid) -> Result<bool, Response> {
    let Some(users) = state.users.as_ref() else {
        return Err(internal());
    };
    match users.find_by_id(user_id).await {
        Ok(user) => Ok(user.is_some_and(|u| u.kind == UserKind::Bot)),
        Err(e) => {
            tracing::error!(error=?e, %user_id, "bot kind lookup");
            Err(internal())
        }
    }
}

/// 403 `bot.forbidden` when the actor is a bot.
pub async fn deny_bot_actor(state: &AppState, ctx: &AuthContext, action: &str) -> Option<Response> {
    match is_bot(state, ctx.user_id).await {
        Ok(false) => None,
        Ok(true) => {
            tracing::info!(action, user_id = %ctx.user_id, "bot actor denied");
            Some(json_err(StatusCode::FORBIDDEN, "bot.forbidden", ""))
        }
        Err(response) => Some(response),
    }
}

/// 403 `bot.restricted_forbidden` when the actor is a bot and `parent_id`
/// resolves inside a restricted subtree (creating there mints an owner
/// grant).
pub async fn deny_bot_restricted_destination(
    state: &AppState,
    ctx: &AuthContext,
    parent_id: Option<Uuid>,
) -> Option<Response> {
    let parent_id = parent_id?;
    match is_bot(state, ctx.user_id).await {
        Ok(false) => return None,
        Ok(true) => {}
        Err(response) => return Some(response),
    }
    let Some(docs) = state.docs.as_ref() else {
        return Some(internal());
    };
    match docs.restriction_root(ctx.workspace_id, parent_id).await {
        Ok(None) => None,
        Ok(Some(_)) => Some(restricted_forbidden(ctx, "create in restricted subtree")),
        Err(e) => {
            tracing::error!(error=?e, %parent_id, "bot destination restriction root");
            Some(internal())
        }
    }
}

/// 403 `bot.restricted_forbidden`, logged with the action name.
pub(crate) fn restricted_forbidden(ctx: &AuthContext, action: &str) -> Response {
    tracing::info!(action, user_id = %ctx.user_id, "bot restricted action denied");
    json_err(StatusCode::FORBIDDEN, "bot.restricted_forbidden", "")
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
