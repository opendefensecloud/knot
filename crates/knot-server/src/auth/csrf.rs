//! Verify the `X-CSRF-Token` header against the `csrf` cookie for unsafe
//! methods on routes mounted ABOVE this layer. Safe methods (GET/HEAD/
//! OPTIONS) and routes mounted OUTSIDE this layer (e.g. all of `/auth/*`,
//! which establishes the session in the first place) are unaffected.
//!
//! Behaviour: only enforce CSRF when a session is already present
//! (`AuthContext` extension exists). Anonymous unsafe POSTs cannot be
//! CSRF'd because there is no session cookie to ride.

use axum::{
    body::Body,
    extract::Request,
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};

use super::{api_key_loader::ApiPrincipal, context::AuthContext};
use crate::http_error::json_err;

pub use crate::auth::cookies::CSRF_COOKIE;
pub const CSRF_HEADER: &str = "x-csrf-token";

pub async fn csrf_mw(req: Request<Body>, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if safe {
        return next.run(req).await;
    }
    if req.extensions().get::<AuthContext>().is_none() {
        return next.run(req).await;
    }
    // API keys travel in a header, never a cookie: nothing to forge.
    if req.extensions().get::<ApiPrincipal>().is_some() {
        return next.run(req).await;
    }

    let csrf_cookie = crate::auth::cookies::find_cookie(&req, CSRF_COOKIE);
    let csrf_header = req
        .headers()
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if let (Some(c), Some(h)) = (csrf_cookie, csrf_header)
        && c == h
    {
        return next.run(req).await;
    }

    json_err(
        StatusCode::FORBIDDEN,
        "auth.csrf",
        "missing or mismatched CSRF token",
    )
}
