//! OIDC auth-code (PKCE) endpoints.
//!
//! `GET /auth/oidc/login`    — generate authorize URL, stash flow-state
//!                              in a short-lived `oidc_flow` cookie,
//!                              redirect 302 to the IdP.
//! `GET /auth/oidc/callback` — verify state, exchange code, verify
//!                              id_token + nonce, look up or
//!                              auto-provision the user, mint a session,
//!                              redirect to base_url.

use std::collections::HashMap;

use axum::{
    Router,
    body::Body,
    extract::{Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use knot_auth::SessionToken;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::auth::cookies::{
    OIDC_FLOW_COOKIE, SESSION_TTL, append_session_cookies, build_flow_clear_cookie,
    build_flow_cookie, build_session_cookies, find_cookie,
};
use crate::http_error::json_err;

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

#[derive(Serialize, Deserialize)]
struct FlowState {
    state: String,
    nonce: String,
    pkce: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/oidc/login", get(login))
        .route("/auth/oidc/callback", get(callback))
}

async fn login(State(state): State<AppState>) -> Response {
    let Some(oidc) = state.oidc.clone() else {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth.oidc.disabled",
            "OIDC not enabled",
        );
    };
    let start = oidc.build_authorize_url();
    let flow = FlowState {
        state: start.csrf_state.clone(),
        nonce: start.nonce.clone(),
        pkce: start.pkce_verifier.clone(),
    };
    let encoded = match serde_json::to_vec(&flow) {
        Ok(b) => URL_SAFE_NO_PAD.encode(b),
        Err(_) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""),
    };
    let cookie_val = build_flow_cookie(&state, &encoded);

    let mut resp = Redirect::to(start.authorize_url.as_str()).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie_val).expect("cookie"),
    );
    resp
}

async fn callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    req: Request<Body>,
) -> Response {
    let Some(oidc) = state.oidc.clone() else {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth.oidc.disabled",
            "OIDC not enabled",
        );
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

    let flow = match read_flow_cookie(&req) {
        Some(f) => f,
        None => {
            return json_err(
                StatusCode::BAD_REQUEST,
                "auth.oidc.state_mismatch",
                "missing flow",
            );
        }
    };
    if flow.state != q.state {
        return json_err(
            StatusCode::BAD_REQUEST,
            "auth.oidc.state_mismatch",
            "state mismatch",
        );
    }

    let id = match oidc.exchange_code(&q.code, &flow.pkce, &flow.nonce).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error=?e, "oidc exchange");
            return json_err(StatusCode::BAD_REQUEST, "auth.oidc.exchange_failed", "");
        }
    };

    // Resolve existing user by (issuer, subject), then by email, else
    // auto-provision per policy.
    let user = match users.find_by_oidc(oidc.issuer(), &id.subject).await {
        Ok(Some(u)) => u,
        Ok(None) => match users.find_by_email(&id.email).await {
            Ok(Some(u)) => u,
            Ok(None) => match auto_provision(&state, &id, users.as_ref()).await {
                Ok(Some(u)) => u,
                Ok(None) => {
                    return json_err(
                        StatusCode::FORBIDDEN,
                        "auth.oidc.not_provisioned",
                        "user not provisioned",
                    );
                }
                Err(resp) => return resp,
            },
            Err(e) => {
                tracing::error!(error=?e, "oidc lookup");
                return internal();
            }
        },
        Err(e) => {
            tracing::error!(error=?e, "oidc lookup");
            return internal();
        }
    };

    let ws = match workspaces.get_singleton().await {
        Ok(Some(w)) => w,
        _ => return internal(),
    };
    if workspaces
        .get_member_role(ws.id, user.id)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        if state.config.oidc_auto_provision == "off" {
            return json_err(
                StatusCode::FORBIDDEN,
                "auth.oidc.not_provisioned",
                "existing user not auto-provisioned",
            );
        }
        if let Err(e) = workspaces
            .add_member(ws.id, user.id, knot_storage::WorkspaceRole::Viewer)
            .await
        {
            tracing::error!(error=?e, "oidc add_member");
            return internal();
        }
    }

    let token = SessionToken::generate();
    let exp = Utc::now() + chrono::Duration::from_std(SESSION_TTL).unwrap();
    if let Err(e) = sessions
        .create(token.as_bytes(), user.id, ws.id, exp, None, None)
        .await
    {
        tracing::error!(error=?e, "oidc create session");
        return internal();
    }

    let (sid, csrf) = build_session_cookies(&state, &token);
    let flow_clear = build_flow_clear_cookie();
    let mut resp = Redirect::to(&state.base_url).into_response();
    append_session_cookies(&mut resp, &sid, &csrf);
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&flow_clear).expect("flow"),
    );
    resp
}

async fn auto_provision(
    state: &AppState,
    id: &knot_auth::oidc::VerifiedIdentity,
    users: &dyn knot_storage::UserStore,
) -> Result<Option<knot_storage::User>, Response> {
    let policy = state.config.oidc_auto_provision.as_str();
    let allow = match policy {
        "always" => true,
        "domain" => {
            let domains = &state.config.oidc_allowed_domains;
            let user_domain = id.email.split('@').nth(1).unwrap_or("");
            domains
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .any(|d| d == user_domain)
        }
        "group" => {
            let mapping = &state.config.oidc_role_from_groups;
            let parsed: HashMap<String, String> = serde_json::from_str(mapping).unwrap_or_default();
            id.groups.iter().any(|g| parsed.contains_key(g))
        }
        _ => false,
    };
    if !allow {
        return Ok(None);
    }

    let Some(oidc) = state.oidc.as_ref() else {
        return Err(internal());
    };
    let created = users
        .create_oidc(&id.email, &id.display_name, oidc.issuer(), &id.subject)
        .await
        .map_err(|e| {
            tracing::error!(error=?e, "oidc create");
            internal()
        })?;

    // For "group" policy, attach as workspace member with the mapped role.
    if policy == "group" {
        let mapping = &state.config.oidc_role_from_groups;
        let parsed: HashMap<String, String> = serde_json::from_str(mapping).unwrap_or_default();
        let role = id
            .groups
            .iter()
            .find_map(|g| parsed.get(g))
            .and_then(|s| knot_storage::WorkspaceRole::parse(s))
            .unwrap_or(knot_storage::WorkspaceRole::Viewer);
        if let Some(ws_store) = state.workspaces.clone()
            && let Ok(Some(ws)) = ws_store.get_singleton().await
        {
            let _ = ws_store.add_member(ws.id, created.id, role).await;
        }
    }
    Ok(Some(created))
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

fn read_flow_cookie(req: &Request<Body>) -> Option<FlowState> {
    let raw = find_cookie(req, OIDC_FLOW_COOKIE)?;
    let bytes = URL_SAFE_NO_PAD.decode(raw).ok()?;
    serde_json::from_slice(&bytes).ok()
}
