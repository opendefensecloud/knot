//! Local credential authentication: login, logout, session info.

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use knot_auth::api_key::{self, PrincipalKind};
use knot_auth::{Allow, SessionToken};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::auth::AuthContext;
use crate::auth::cookies::{
    SESSION_TTL, SID_COOKIE, append_session_cookies, build_clear_cookies, build_session_cookies,
    find_cookie,
};
use crate::client_ip::{self, IpKey};
use crate::http_error::json_err;
use crate::rate_limit::{HttpBucket, http::take_user_token};

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
    /// "local" | "oidc" | "bot"; lets the UI hide "never expires" for SSO users.
    auth_kind: &'static str,
    /// Upper bound for API key TTL in days; `null` means unlimited allowed.
    max_api_key_ttl_days: Option<i64>,
}

/// A fixed Argon2 hash so login runs the (expensive) verify even when the user
/// is absent or has no password, removing the existing-user timing oracle.
fn timing_dummy_hash(hasher: &knot_auth::Hasher) -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hasher.hash("knot-timing-dummy").unwrap_or_default())
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
    // The client through any trusted proxy (the TCP peer without one).
    let client = client_ip::from_request(&req, &state.trusted_proxies);

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

    let ip_key = format!("ip:{}", IpKey::of(client.ip));
    let email_key = email_throttle_key(&body.email);
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
            let _ = state
                .hasher
                .verify(timing_dummy_hash(&state.hasher), &body.password);
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
        let _ = state
            .hasher
            .verify(timing_dummy_hash(&state.hasher), &body.password);
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
    let sid_hash = knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes());
    if let Err(e) = sessions
        .create(
            &sid_hash,
            user.id,
            ws.id,
            exp,
            user_agent.as_deref(),
            Some(client.ip),
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
        let hash = knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes());
        let _ = sessions.delete(&hash).await;
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
    let kind = crate::routes::api::api_keys::principal_kind(&user);
    Json(SessionResponse {
        auth_kind: match kind {
            PrincipalKind::Local => "local",
            PrincipalKind::Oidc => "oidc",
            PrincipalKind::Bot => "bot",
        },
        max_api_key_ttl_days: api_key::max_ttl_days(kind),
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

    // `/auth/*` has no CSRF layer, and a same-site page's form can POST here
    // with the session cookie. A form can't send `application/json` (that
    // needs a CORS preflight, which knot never answers), so requiring it
    // before the token is taken keeps such forms from draining the
    // `expensive` bucket that duplicate and import share.
    if !has_json_content_type(&req) {
        return json_err(
            StatusCode::BAD_REQUEST,
            "auth.bad_request",
            "expected Content-Type: application/json",
        );
    }

    // Two Argon2 operations and a session rotation per success: charged to
    // the expensive bucket, after the session and content-type checks (a
    // 401 or a form POST costs nothing), on top of the failure throttle
    // below.
    if let Err(refusal) = take_user_token(&state, HttpBucket::Expensive, ctx.user_id) {
        return refusal;
    }

    let client = client_ip::from_request(&req, &state.trusted_proxies);
    let ip_key = format!("pw:ip:{}", IpKey::of(client.ip));
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
    // One transaction: the new hash (whose trigger revokes the user's API
    // keys, S5) and the end of every existing session (other devices, any
    // stolen cookie). A key mint locks the user row and re-checks its
    // session, so it either commits first and is revoked here, or waits and
    // finds its session gone. Two statements outside a transaction would
    // let a mint slip in between.
    if let Err(e) = replace_password_hash(&pool, ctx.user_id, &new_hash).await {
        tracing::error!(error=?e, "change_password update");
        return internal();
    }

    // Mint a fresh session for the current request.
    if let Some(sessions) = state.sessions.clone() {
        let token = SessionToken::generate();
        let exp = Utc::now() + chrono::Duration::from_std(SESSION_TTL).unwrap();
        let sid_hash = knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes());
        if let Err(e) = sessions
            .create(&sid_hash, ctx.user_id, ctx.workspace_id, exp, None, None)
            .await
        {
            tracing::error!(error=?e, "change_password: re-create session");
            return internal();
        }
        state.throttle.reset(&ip_key);
        state.throttle.reset(&user_key);
        let (sid, csrf) = build_session_cookies(&state, &token);
        let mut resp = StatusCode::NO_CONTENT.into_response();
        append_session_cookies(&mut resp, &sid, &csrf);
        return resp;
    }
    state.throttle.reset(&ip_key);
    state.throttle.reset(&user_key);
    StatusCode::NO_CONTENT.into_response()
}

async fn replace_password_hash(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    new_hash: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = knot_storage::begin(pool).await?;
    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
        .bind(new_hash)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// The request's `Content-Type` is `application/json` (parameters such as
/// `charset` allowed): something a plain HTML form can't send.
fn has_json_content_type(req: &Request<Body>) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|content_type| content_type.to_str().ok())
        .and_then(|content_type| content_type.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

/// The failure throttle's key for `email`: lowercased, at most
/// [`knot_auth::throttle::MAX_KEY_BYTES`] long, so a huge email in a login
/// body can't grow the throttle's memory (the throttle cuts keys too).
fn email_throttle_key(email: &str) -> String {
    const PREFIX: &str = "email:";
    let max_key_bytes = knot_auth::throttle::MAX_KEY_BYTES;
    let email_prefix = cut_on_char_boundary(email, max_key_bytes - PREFIX.len());
    // Lowercasing can lengthen some characters; cut again after.
    let key = format!("{PREFIX}{}", email_prefix.to_lowercase());
    cut_on_char_boundary(&key, max_key_bytes).to_owned()
}

/// `text` cut to at most `max_bytes`, on a char boundary.
fn cut_on_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::to_bytes;
    use axum::middleware::Next;
    use knot_storage::WorkspaceRole;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::rate_limit::RateLimits;

    #[test]
    fn the_email_throttle_key_is_lowercased_and_capped() {
        assert_eq!(
            email_throttle_key("Alice@Example.COM"),
            "email:alice@example.com"
        );
        let huge_email = format!("{}@example.com", "Ä".repeat(40_000));
        let key = email_throttle_key(&huge_email);
        assert!(
            key.len() <= knot_auth::throttle::MAX_KEY_BYTES,
            "{}",
            key.len()
        );
        assert!(key.starts_with("email:ää"));
    }

    #[tokio::test]
    async fn a_password_change_takes_an_expensive_token_after_the_session_and_content_type_checks()
    {
        let mut state = AppState::in_memory();
        let config = knot_config::Config {
            rate_limit_enabled: true,
            rate_limit_scale: 0.1,
            ..Default::default()
        };
        state.rate_limits = Arc::new(RateLimits::from_config(&config));
        let user_id = Uuid::new_v4();
        let app = router()
            .layer(axum::middleware::from_fn(
                move |mut request: Request<Body>, next: Next| async move {
                    if request.headers().contains_key("x-test-session") {
                        request.extensions_mut().insert(AuthContext {
                            user_id,
                            workspace_id: Uuid::nil(),
                            role: WorkspaceRole::Editor,
                        });
                    }
                    next.run(request).await
                },
            ))
            .with_state(state);
        let password_change_as = |with_session: bool, content_type: Option<&str>| {
            let mut builder = Request::builder().method("POST").uri("/auth/password");
            if with_session {
                builder = builder.header("x-test-session", "1");
            }
            if let Some(content_type) = content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            builder.body(Body::from("not json")).unwrap()
        };
        let password_change =
            |with_session: bool| password_change_as(with_session, Some("application/json"));
        // Without a session: 401, and no token is taken.
        for _ in 0..3 {
            let response = app.clone().oneshot(password_change(false)).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        // Without a JSON content type (what a plain form sends): 400, and no
        // token is taken either.
        for form_content_type in [None, Some("text/plain")] {
            let response = app
                .clone()
                .oneshot(password_change_as(true, form_content_type))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        // The expensive bucket at scale 0.1 holds one token.
        let first = app.clone().oneshot(password_change(true)).await.unwrap();
        assert_eq!(first.status(), StatusCode::BAD_REQUEST);
        let second = app.clone().oneshot(password_change(true)).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(second.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "rate.limited");
    }
}
