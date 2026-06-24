//! Shared cookie helpers for the auth layer.

use std::time::Duration;

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderName, HeaderValue, header},
    response::Response,
};
use knot_auth::SessionToken;

use crate::AppState;

pub const SID_COOKIE: &str = "sid";
pub const CSRF_COOKIE: &str = "csrf";
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 30); // 30 days

/// Find a cookie value from the `Cookie` header by name.
pub fn find_cookie(req: &Request<Body>, name: &str) -> Option<String> {
    let h = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for raw in h.split(';') {
        if let Ok(c) = cookie::Cookie::parse(raw.trim())
            && c.name() == name
        {
            return Some(c.value().to_string());
        }
    }
    None
}

/// Build the two Set-Cookie strings: (sid, csrf). The `Secure` flag comes from
/// `AppState::cookie_secure` (config `KNOT_COOKIE_SECURE`, default true; dev
/// sets false for plain-HTTP localhost).
pub fn build_session_cookies(state: &AppState, token: &SessionToken) -> (String, String) {
    let sec = if state.cookie_secure { "; Secure" } else { "" };
    let sid = format!(
        "{SID_COOKIE}={}; HttpOnly; SameSite=Lax; Path=/{sec}",
        token.encode()
    );
    let csrf_tok = knot_auth::csrf::mint(&state.session_key, token.as_bytes());
    let csrf = format!("{CSRF_COOKIE}={csrf_tok}; SameSite=Lax; Path=/{sec}");
    (sid, csrf)
}

/// Build cookie clearings for sid + csrf (Max-Age=0).
pub fn build_clear_cookies() -> (String, String) {
    let sid = format!("{SID_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
    let csrf = format!("{CSRF_COOKIE}=; SameSite=Lax; Path=/; Max-Age=0");
    (sid, csrf)
}

/// Convenience: append both cookies to a response headers map.
pub fn append_session_cookies(resp: &mut Response, sid: &str, csrf: &str) {
    let set_cookie: HeaderName = header::SET_COOKIE;
    let headers = resp.headers_mut();
    headers.append(set_cookie.clone(), HeaderValue::from_str(sid).expect("sid"));
    headers.append(set_cookie, HeaderValue::from_str(csrf).expect("csrf"));
}

pub const OIDC_FLOW_COOKIE: &str = "oidc_flow";
pub const OIDC_FLOW_TTL_SEC: i64 = 300;

/// Build a Set-Cookie string for the OIDC flow state (base64 of JSON
/// containing state/nonce/pkce). Short-lived (5 min) cookie scoped to the
/// callback path.
pub fn build_flow_cookie(state: &AppState, encoded_payload: &str) -> String {
    let sec = if state.cookie_secure { "; Secure" } else { "" };
    format!(
        "{OIDC_FLOW_COOKIE}={encoded_payload}; HttpOnly; SameSite=Lax; Path=/; Max-Age={OIDC_FLOW_TTL_SEC}{sec}"
    )
}

pub fn build_flow_clear_cookie() -> String {
    format!("{OIDC_FLOW_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}
