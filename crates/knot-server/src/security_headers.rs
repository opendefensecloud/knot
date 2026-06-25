//! Sets security headers on every response. CSP is enforced; it is intentionally
//! permissive only where the SPA requires it (inline styles from Tiptap/Excalidraw).
//! HSTS is emitted only when cookies are Secure (i.e. served over TLS).

use axum::{
    extract::{Request, State},
    http::{HeaderValue, header},
    middleware::Next,
    response::Response,
};

// `connect-src` includes ws/wss for the same-origin collab socket; `img-src`
// allows data:/blob: for editor/board images; `worker-src blob:` for Yjs/Excalidraw.
pub const CSP: &str = "default-src 'self'; \
base-uri 'self'; object-src 'none'; frame-ancestors 'none'; \
img-src 'self' data: blob:; font-src 'self' data:; \
style-src 'self' 'unsafe-inline'; script-src 'self'; \
connect-src 'self' ws: wss:; worker-src 'self' blob:";

pub async fn set_security_headers(
    State(cookie_secure): State<bool>,
    req: Request,
    next: Next,
) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    if cookie_secure {
        h.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    res
}
