//! Local credential authentication: login, logout, session info.

use std::net::SocketAddr;

use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use knot_auth::{Allow, SessionToken};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::auth::AuthContext;
use crate::auth::cookies::{
    SESSION_TTL, SID_COOKIE, append_session_cookies, build_clear_cookies, build_session_cookies,
    find_cookie,
};
use crate::http_error::json_err;

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Serialize)]
struct SessionResponse {
    user_id: String,
    email: String,
    display_name: String,
    workspace_id: String,
    role: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/session", get(session))
        .route("/auth/password", post(change_password))
}

#[tracing::instrument(skip_all, name = "auth.login")]
async fn login(State(state): State<AppState>, req: Request<Body>) -> Response {
    let addr: SocketAddr = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0)
        .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());

    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return invalid_credentials(),
    };
    let body: LoginRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => return invalid_credentials(),
    };

    let Some(users) = state.users.clone() else {
        return internal();
    };
    let Some(workspaces) = state.workspaces.clone() else {
        return internal();
    };
    let Some(sessions) = state.sessions.clone() else {
        return internal();
    };

    let ip_key = format!("ip:{}", addr.ip());
    let email_key = format!("email:{}", body.email.to_lowercase());
    if matches!(state.throttle.check(&ip_key), Allow::No)
        || matches!(state.throttle.check(&email_key), Allow::No)
    {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return invalid_credentials();
    }

    let user = match users.find_by_email(&body.email).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            state.throttle.record_failure(&ip_key);
            state.throttle.record_failure(&email_key);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            return invalid_credentials();
        }
        Err(e) => {
            tracing::error!(error=?e, "login lookup");
            return internal();
        }
    };
    let Some(hash) = user.password_hash.as_deref() else {
        state.throttle.record_failure(&email_key);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return invalid_credentials();
    };

    let ok = state.hasher.verify(hash, &body.password).unwrap_or(false);
    if !ok {
        state.throttle.record_failure(&ip_key);
        state.throttle.record_failure(&email_key);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return invalid_credentials();
    }
    state.throttle.reset(&ip_key);
    state.throttle.reset(&email_key);

    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        _ => return internal(),
    };
    match workspaces.get_member_role(ws.id, user.id).await {
        Ok(Some(_)) => {}
        _ => return invalid_credentials(),
    };

    let token = SessionToken::generate();
    let exp = Utc::now() + chrono::Duration::from_std(SESSION_TTL).unwrap();
    if let Err(e) = sessions
        .create(
            token.as_bytes(),
            user.id,
            ws.id,
            exp,
            user_agent.as_deref(),
            Some(addr.ip()),
        )
        .await
    {
        tracing::error!(error=?e, "create session");
        return internal();
    }

    let (sid, csrf) = build_session_cookies(&state, &token);
    let mut resp = StatusCode::NO_CONTENT.into_response();
    append_session_cookies(&mut resp, &sid, &csrf);
    resp
}

async fn logout(State(state): State<AppState>, req: Request<Body>) -> Response {
    if let Some(sid) = find_cookie(&req, SID_COOKIE)
        && let Ok(token) = SessionToken::decode(&sid)
        && let Some(sessions) = state.sessions.clone()
    {
        let _ = sessions.delete(token.as_bytes()).await;
    }
    let (sid_clear, csrf_clear) = build_clear_cookies();
    let mut resp = StatusCode::NO_CONTENT.into_response();
    append_session_cookies(&mut resp, &sid_clear, &csrf_clear);
    resp
}

async fn session(State(state): State<AppState>, req: Request<Body>) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "no session",
        );
    };
    let Some(users) = state.users.clone() else {
        return internal();
    };
    let user = match users.find_by_id(ctx.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => return invalid_credentials(),
        Err(e) => {
            tracing::error!(error=?e, "session lookup");
            return internal();
        }
    };
    Json(SessionResponse {
        user_id: user.id.to_string(),
        email: user.email,
        display_name: user.display_name,
        workspace_id: ctx.workspace_id.to_string(),
        role: ctx.role.as_str().into(),
    })
    .into_response()
}

#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

#[tracing::instrument(skip_all, name = "auth.change_password")]
async fn change_password(State(state): State<AppState>, req: Request<Body>) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "no session",
        );
    };

    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let ip_key = format!("pw:ip:{ip}");
    let user_key = format!("pw:user:{}", ctx.user_id);

    if matches!(state.throttle.check(&ip_key), Allow::No)
        || matches!(state.throttle.check(&user_key), Allow::No)
    {
        return json_err(
            StatusCode::TOO_MANY_REQUESTS,
            "auth.throttled",
            "too many attempts",
        );
    }

    let bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return internal(),
    };
    let body: PasswordChange = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "auth.bad_request", "invalid body"),
    };

    // Validate new password before any crypto work to avoid timing leaks.
    if body.new == body.current {
        return json_err(
            StatusCode::BAD_REQUEST,
            "auth.password_reuse",
            "new password must differ from current",
        );
    }
    if body.new.chars().count() < 8 {
        return json_err(
            StatusCode::BAD_REQUEST,
            "auth.weak_password",
            "password must be at least 8 characters",
        );
    }

    let Some(users) = state.users.clone() else {
        return internal();
    };
    let user = match users.find_by_id(ctx.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => return invalid_credentials(),
        Err(e) => {
            tracing::error!(error=?e, "change_password lookup");
            return internal();
        }
    };

    let Some(hash) = user.password_hash.as_deref() else {
        // OIDC-only user — no local credential to change.
        state.throttle.record_failure(&ip_key);
        return invalid_credentials();
    };

    let ok = state.hasher.verify(hash, &body.current).unwrap_or(false);
    if !ok {
        state.throttle.record_failure(&ip_key);
        state.throttle.record_failure(&user_key);
        return invalid_credentials();
    }

    let new_hash = match state.hasher.hash(&body.new) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, "change_password hash");
            return internal();
        }
    };

    let Some(pool) = state.pool.clone() else {
        return internal();
    };
    if let Err(e) = sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
        .bind(new_hash)
        .bind(ctx.user_id)
        .execute(&pool)
        .await
    {
        tracing::error!(error=?e, "change_password update");
        return internal();
    }

    state.throttle.reset(&ip_key);
    state.throttle.reset(&user_key);
    StatusCode::NO_CONTENT.into_response()
}

fn invalid_credentials() -> Response {
    json_err(
        StatusCode::UNAUTHORIZED,
        "auth.invalid_credentials",
        "invalid credentials",
    )
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
