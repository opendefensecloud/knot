//! HTTP rate limits (spec "HTTP buckets").
//!
//! Each limited request takes one token from exactly one bucket, chosen by
//! [`classify`] from its method (HEAD counts as GET) and route template
//! (`MatchedPath`). The `/api` buckets are keyed by user and run in
//! [`api_mw`], after the session and CSRF checks and before the doc-role
//! check; `/auth/*` and `/p/*` sit outside the api router and get
//! [`anonymous_mw`], keyed by client IP. `POST /auth/password` is charged
//! to [`HttpBucket::Expensive`] in its handler, after its session check
//! ([`take_user_token`]).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::Response;
use tokio::time::Instant;
use uuid::Uuid;

use super::bucket::{KeyedBuckets, Rate, retry_after_secs};
use crate::AppState;
use crate::auth::AuthContext;
use crate::client_ip::{self, IpKey};
use crate::http_error::{rate_limited, rate_limited_page};

/// One HTTP bucket. Every bucket has its own tokens per key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HttpBucket {
    /// Duplicate, workspace import, password change; per user.
    Expensive,
    /// Zip and DOCX/ODT exports; per user. GETs, so a cross-site navigation
    /// can drain this bucket but no other.
    Export,
    /// File uploads; per user.
    Upload,
    /// The board SVG preview, sent after every board change; per user.
    BoardPreview,
    /// Every other mutating `/api` route; per user.
    Write,
    /// Password sign-in and first-run setup; per client IP.
    SignIn,
    /// Public share pages and their files; per client IP.
    Public,
}

impl HttpBucket {
    pub const ALL: [HttpBucket; 7] = [
        HttpBucket::Expensive,
        HttpBucket::Export,
        HttpBucket::Upload,
        HttpBucket::BoardPreview,
        HttpBucket::Write,
        HttpBucket::SignIn,
        HttpBucket::Public,
    ];

    /// The `bucket` label of `knot_rate_limited_total`.
    pub fn label(self) -> &'static str {
        match self {
            HttpBucket::Expensive => "expensive",
            HttpBucket::Export => "export",
            HttpBucket::Upload => "upload",
            HttpBucket::BoardPreview => "board_preview",
            HttpBucket::Write => "write",
            HttpBucket::SignIn => "sign_in",
            HttpBucket::Public => "public",
        }
    }

    /// The bucket's burst and refill before `rate_limit_scale`.
    pub fn rate(self) -> Rate {
        match self {
            HttpBucket::Expensive => Rate::per_minute(10, 10),
            HttpBucket::Export => Rate::per_minute(20, 20),
            HttpBucket::Upload => Rate::per_minute(60, 60),
            HttpBucket::BoardPreview => Rate::per_minute(300, 300),
            HttpBucket::Write => Rate::per_minute(120, 120),
            HttpBucket::SignIn => Rate::per_minute(30, 30),
            HttpBucket::Public => Rate::per_minute(600, 600),
        }
    }

    /// Keyed by client IP (anonymous routes) rather than by user.
    pub fn keyed_by_client_ip(self) -> bool {
        matches!(self, HttpBucket::SignIn | HttpBucket::Public)
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Routes with a bucket of their own: (method, route template, bucket).
/// Every other mutating `/api` route is [`HttpBucket::Write`]. Each entry is
/// pinned to a real route by a test.
pub const NAMED_ROUTES: &[(&str, &str, HttpBucket)] = &[
    ("POST", "/api/docs/{id}/duplicate", HttpBucket::Expensive),
    ("POST", "/api/workspace/import", HttpBucket::Expensive),
    ("GET", "/api/workspace/export", HttpBucket::Export),
    ("GET", "/api/docs/{doc_id}/export", HttpBucket::Export),
    ("POST", "/api/docs/{id}/export/{format}", HttpBucket::Export),
    ("POST", "/api/docs/{doc_id}/blobs", HttpBucket::Upload),
    ("PUT", "/api/boards/{id}/svg", HttpBucket::BoardPreview),
    ("POST", "/auth/login", HttpBucket::SignIn),
    ("POST", "/auth/setup", HttpBucket::SignIn),
    ("GET", "/p/{token}", HttpBucket::Public),
    ("GET", "/p/{token}/blobs/{id}", HttpBucket::Public),
    ("GET", "/p/{token}/boards/{id}/svg", HttpBucket::Public),
];

/// Routes their handler charges, after a check the middleware can't make
/// (`/auth/password` has no CSRF layer and checks its own session).
pub const HANDLER_CHARGED_ROUTES: &[(&str, &str, HttpBucket)] =
    &[("POST", "/auth/password", HttpBucket::Expensive)];

/// The bucket a request takes its token from, or `None` when it isn't
/// limited (other GETs, health, static files, `/auth/session`, OIDC, …).
pub fn classify(method: &Method, matched_path: &str) -> Option<HttpBucket> {
    // axum answers HEAD with the GET handler, which builds the whole body.
    let method_name = if method == Method::HEAD {
        Method::GET.as_str()
    } else {
        method.as_str()
    };
    let named_bucket = NAMED_ROUTES
        .iter()
        .find(|(route_method, template, _)| {
            *route_method == method_name && *template == matched_path
        })
        .map(|(_, _, bucket)| *bucket);
    if named_bucket.is_some() {
        return named_bucket;
    }
    let mutating = matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    // A new mutating route is limited without anyone listing it.
    (mutating && matched_path.starts_with("/api/")).then_some(HttpBucket::Write)
}

/// What a bucket is keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HttpKey {
    User(Uuid),
    ClientIp(IpKey),
}

impl fmt::Display for HttpKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpKey::User(user_id) => write!(formatter, "user {user_id}"),
            HttpKey::ClientIp(ip_key) => write!(formatter, "client {ip_key}"),
        }
    }
}

/// The HTTP buckets of one replica.
pub struct HttpLimits {
    /// One per [`HttpBucket`], by [`HttpBucket::index`]; `None` when off.
    buckets: Option<Vec<KeyedBuckets<HttpKey>>>,
}

impl HttpLimits {
    /// Every bucket at its rate × `scale`, never below one token and one
    /// per minute.
    pub fn new(scale: f64) -> Self {
        let buckets = HttpBucket::ALL
            .iter()
            .map(|bucket| KeyedBuckets::new(bucket.rate().scaled(scale, Rate::MINIMUM)))
            .collect();
        Self {
            buckets: Some(buckets),
        }
    }

    /// No HTTP limit at all.
    pub fn disabled() -> Self {
        Self { buckets: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.buckets.is_some()
    }

    /// `bucket`'s scaled rate; `None` when off.
    pub fn rate(&self, bucket: HttpBucket) -> Option<Rate> {
        self.buckets_of(bucket).map(KeyedBuckets::rate)
    }

    fn buckets_of(&self, bucket: HttpBucket) -> Option<&KeyedBuckets<HttpKey>> {
        self.buckets
            .as_ref()
            .map(|all_buckets| &all_buckets[bucket.index()])
    }

    /// Take a token from `key`'s `bucket`, or say how long until one is
    /// back. A refusal counts `knot_rate_limited_total{bucket}` and, when the
    /// key starts being limited, logs once.
    pub fn check(&self, bucket: HttpBucket, key: &HttpKey, now: Instant) -> Result<(), Duration> {
        let Some(buckets) = self.buckets_of(bucket) else {
            return Ok(());
        };
        buckets
            .try_take_noting_start(key, 1.0, now)
            .map_err(|refusal| {
                metrics::counter!("knot_rate_limited_total", "bucket" => bucket.label())
                    .increment(1);
                if refusal.newly_limited {
                    tracing::warn!(
                        bucket = bucket.label(),
                        key = %key,
                        retry_after_secs = retry_after_secs(refusal.wait),
                        "rate limit reached; refusing with 429 until the bucket refills"
                    );
                }
                refusal.wait
            })
    }

    /// [`HttpLimits::check`] as a response: `Err` holds the 429.
    #[allow(
        clippy::result_large_err,
        reason = "the Err is the ready axum Response, returned unboxed like the async handlers do"
    )]
    fn check_response(
        &self,
        bucket: HttpBucket,
        key: &HttpKey,
        now: Instant,
    ) -> Result<(), Response> {
        self.check(bucket, key, now)
            .map_err(|wait| rate_limited(retry_after_secs(wait)))
    }

    /// [`HttpLimits::check_response`], with the 429 as an HTML page.
    #[allow(
        clippy::result_large_err,
        reason = "the Err is the ready axum Response, returned unboxed like the async handlers do"
    )]
    fn check_page_response(
        &self,
        bucket: HttpBucket,
        key: &HttpKey,
        now: Instant,
    ) -> Result<(), Response> {
        self.check(bucket, key, now)
            .map_err(|wait| rate_limited_page(retry_after_secs(wait)))
    }
}

/// The per-user buckets of `/api`. Runs inside the session and CSRF layers
/// (a 401 or a CSRF failure costs nothing) and outside the doc-role layer
/// and the handler (a refused request costs no ACL lookup).
pub async fn api_mw(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let limits = &state.rate_limits.http;
    if !limits.is_enabled() {
        return next.run(request).await;
    }
    let Some(user_id) = request
        .extensions()
        .get::<AuthContext>()
        .map(|context| context.user_id)
    else {
        return next.run(request).await;
    };
    let bucket = request
        .extensions()
        .get::<MatchedPath>()
        .and_then(|matched_path| classify(request.method(), matched_path.as_str()))
        .filter(|bucket| !bucket.keyed_by_client_ip());
    let Some(bucket) = bucket else {
        return next.run(request).await;
    };
    match limits.check_response(bucket, &HttpKey::User(user_id), Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(refusal) => refusal,
    }
}

/// The per-client-IP buckets of `/auth/*` and `/p/*`. When `trusted_proxies`
/// is empty and a request carries `X-Forwarded-For`, knot is behind a proxy
/// it wasn't told about and every client looks alike: the bucket is skipped
/// for that request (never worse than without limits), counted as
/// `knot_client_ip_unresolved_total{reason="no_trusted_proxies"}` and warned
/// about once per process, so the missing setting shows up. With proxies
/// trusted, a peer outside them is the client whatever its header says, and
/// is limited by its own address; it is counted as
/// `{reason="untrusted_peer"}` (a direct client can send the header too, so
/// this one is no sure sign of a proxy left out of the list) and warned
/// about once per process as well. Both counters count only while rate
/// limits are on: with them off this layer returns first.
pub async fn anonymous_mw(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let limits = &state.rate_limits.http;
    if !limits.is_enabled() {
        return next.run(request).await;
    }
    let bucket = request
        .extensions()
        .get::<MatchedPath>()
        .and_then(|matched_path| classify(request.method(), matched_path.as_str()))
        .filter(|bucket| bucket.keyed_by_client_ip());
    let Some(bucket) = bucket else {
        return next.run(request).await;
    };
    let is_page_navigation = is_share_page_navigation(&request);
    let client = client_ip::from_request(&request, &state.trusted_proxies);
    if client.malformed_forwarded_entry {
        metrics::counter!("knot_client_ip_malformed_forwarded_total").increment(1);
    }
    if !client.resolved {
        let reason = UnresolvedReason::of(&state.trusted_proxies);
        metrics::counter!("knot_client_ip_unresolved_total", "reason" => reason.label())
            .increment(1);
        reason.warn_once(client.ip);
        if reason == UnresolvedReason::NoTrustedProxies {
            return next.run(request).await;
        }
    }
    let key = HttpKey::ClientIp(IpKey::of(client.ip));
    let checked = if is_page_navigation {
        limits.check_page_response(bucket, &key, Instant::now())
    } else {
        limits.check_response(bucket, &key, Instant::now())
    };
    match checked {
        Ok(()) => next.run(request).await,
        Err(refusal) => refusal,
    }
}

/// The public share page itself (not its files), asked for as HTML: a
/// browser opening a share link. Its refusal is a page, not JSON. The SPA's
/// own fetch of the page (no `text/html` in `Accept`) keeps the envelope.
fn is_share_page_navigation(request: &Request) -> bool {
    let is_share_page = request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|matched_path| matched_path.as_str() == "/p/{token}");
    let accepts_html = request
        .headers()
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|accept| accept.to_str().ok())
        .any(|accept| accept.contains("text/html"));
    is_share_page && accepts_html
}

/// Why a request's client couldn't be read from `X-Forwarded-For`: the
/// `reason` label of `knot_client_ip_unresolved_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// No proxy is trusted, yet the header came: knot is likely behind a
    /// proxy it wasn't told about. The per-IP buckets are skipped.
    NoTrustedProxies,
    /// Proxies are trusted, but this peer isn't one of them: a proxy left
    /// out of the list, or a direct client sending the header itself. The
    /// peer is limited as the client.
    UntrustedPeer,
}

/// Set once each reason's warning was logged by this process.
static NO_TRUSTED_PROXIES_WARNED: AtomicBool = AtomicBool::new(false);
static UNTRUSTED_PEER_WARNED: AtomicBool = AtomicBool::new(false);

impl UnresolvedReason {
    pub fn of(trusted_proxies: &client_ip::TrustedProxies) -> Self {
        if trusted_proxies.is_empty() {
            UnresolvedReason::NoTrustedProxies
        } else {
            UnresolvedReason::UntrustedPeer
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UnresolvedReason::NoTrustedProxies => "no_trusted_proxies",
            UnresolvedReason::UntrustedPeer => "untrusted_peer",
        }
    }

    /// Log this reason's warning, the first time only in this process.
    fn warn_once(self, peer_ip: std::net::IpAddr) {
        let already_warned = match self {
            UnresolvedReason::NoTrustedProxies => &NO_TRUSTED_PROXIES_WARNED,
            UnresolvedReason::UntrustedPeer => &UNTRUSTED_PEER_WARNED,
        };
        if already_warned.swap(true, Ordering::Relaxed) {
            return;
        }
        match self {
            UnresolvedReason::NoTrustedProxies => tracing::warn!(
                peer = %peer_ip,
                "a request carried X-Forwarded-For but KNOT_TRUSTED_PROXIES is empty: \
                 knot seems to be behind a proxy it was not told about, so per-IP rate \
                 limits are skipped for such requests (see \
                 knot_client_ip_unresolved_total{{reason=\"no_trusted_proxies\"}}). \
                 Set KNOT_TRUSTED_PROXIES to the proxy's addresses"
            ),
            UnresolvedReason::UntrustedPeer => tracing::warn!(
                peer = %peer_ip,
                "a request carried X-Forwarded-For from a peer not in \
                 KNOT_TRUSTED_PROXIES; the header is ignored and the peer is limited \
                 as the client (see \
                 knot_client_ip_unresolved_total{{reason=\"untrusted_peer\"}}). If the \
                 peer is a proxy of yours, add it to KNOT_TRUSTED_PROXIES"
            ),
        }
    }
}

/// Take one token from `user_id`'s `bucket`, from a handler. Success spends
/// the token; `Err` holds the 429 and spends nothing.
#[allow(
    clippy::result_large_err,
    reason = "the Err is the ready axum Response, returned unboxed like the async handlers do"
)]
pub fn take_user_token(
    state: &AppState,
    bucket: HttpBucket,
    user_id: Uuid,
) -> Result<(), Response> {
    state
        .rate_limits
        .http
        .check_response(bucket, &HttpKey::User(user_id), Instant::now())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::MatchedPath;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::routing::{get, patch, post};
    use knot_storage::WorkspaceRole;
    use tower::ServiceExt;

    use super::*;
    use crate::rate_limit::RateLimits;

    #[test]
    fn named_routes_go_to_their_bucket() {
        for (method_name, template, bucket) in NAMED_ROUTES {
            let method = Method::from_bytes(method_name.as_bytes()).unwrap();
            assert_eq!(
                classify(&method, template),
                Some(*bucket),
                "{method_name} {template}"
            );
        }
    }

    #[test]
    fn head_counts_as_get() {
        assert_eq!(
            classify(&Method::HEAD, "/api/workspace/export"),
            Some(HttpBucket::Export)
        );
        assert_eq!(
            classify(&Method::HEAD, "/api/docs/{doc_id}/export"),
            Some(HttpBucket::Export)
        );
        assert_eq!(
            classify(&Method::HEAD, "/p/{token}/blobs/{id}"),
            Some(HttpBucket::Public)
        );
        assert_eq!(classify(&Method::HEAD, "/api/docs"), None);
    }

    #[test]
    fn other_mutating_api_routes_are_writes() {
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                classify(&method, "/api/docs/{id}"),
                Some(HttpBucket::Write),
                "{method}"
            );
        }
        assert_eq!(
            classify(&Method::POST, "/api/some/route/added/later"),
            Some(HttpBucket::Write)
        );
        // The named template with another method is an ordinary write.
        assert_eq!(
            classify(&Method::DELETE, "/api/docs/{id}/duplicate"),
            Some(HttpBucket::Write)
        );
    }

    #[test]
    fn reads_and_unlisted_routes_are_not_limited() {
        let unlimited = [
            (Method::GET, "/api/docs"),
            (Method::GET, "/api/docs/{id}"),
            (Method::GET, "/api/search"),
            (Method::GET, "/api/short-links/{code}"),
            (Method::GET, "/api/healthz"),
            (Method::GET, "/api/readyz"),
            (Method::GET, "/api/version"),
            (Method::OPTIONS, "/api/docs"),
            (Method::GET, "/auth/session"),
            (Method::GET, "/auth/config"),
            (Method::POST, "/auth/logout"),
            (Method::GET, "/auth/oidc/login"),
            (Method::GET, "/auth/oidc/callback"),
            // Charged in its handler, not by a middleware.
            (Method::POST, "/auth/password"),
            (Method::GET, "/collab/doc/{doc_id}"),
            (Method::POST, "/p/{token}"),
            (Method::GET, "/"),
        ];
        for (method, template) in unlimited {
            assert_eq!(classify(&method, template), None, "{method} {template}");
        }
    }

    #[test]
    fn every_bucket_has_its_own_slot() {
        for (position, bucket) in HttpBucket::ALL.iter().enumerate() {
            assert_eq!(bucket.index(), position, "{bucket:?}");
        }
    }

    #[test]
    fn labels_and_rates_match_the_spec() {
        let table: Vec<(&str, Rate, bool)> = HttpBucket::ALL
            .iter()
            .map(|bucket| (bucket.label(), bucket.rate(), bucket.keyed_by_client_ip()))
            .collect();
        assert_eq!(
            table,
            vec![
                ("expensive", Rate::per_minute(10, 10), false),
                ("export", Rate::per_minute(20, 20), false),
                ("upload", Rate::per_minute(60, 60), false),
                ("board_preview", Rate::per_minute(300, 300), false),
                ("write", Rate::per_minute(120, 120), false),
                ("sign_in", Rate::per_minute(30, 30), true),
                ("public", Rate::per_minute(600, 600), true),
            ]
        );
    }

    /// The template a request to `uri` matched in the real router (`None`
    /// for the fallback), read by an outer layer.
    async fn matched_template(app: &Router, method: &str, uri: &str) -> Option<String> {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        response
            .headers()
            .get("x-test-matched-path")
            .map(|value| value.to_str().unwrap().to_owned())
    }

    /// `template` with every `{param}` filled in.
    fn concrete_uri(template: &str) -> String {
        let example_id = "0193e0c0-0000-7000-8000-000000000001";
        template
            .split('/')
            .map(|segment| match segment {
                "{format}" => "docx",
                "{token}" | "{code}" => "sometoken",
                segment if segment.starts_with('{') => example_id,
                segment => segment,
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    #[tokio::test]
    async fn every_named_template_is_a_real_route() {
        let app = crate::router().layer(axum::middleware::from_fn(
            |request: Request, next: Next| async move {
                let template = request
                    .extensions()
                    .get::<MatchedPath>()
                    .map(|matched| matched.as_str().to_owned());
                let mut response = next.run(request).await;
                if let Some(template) = template {
                    response.headers_mut().insert(
                        "x-test-matched-path",
                        HeaderValue::from_str(&template).unwrap(),
                    );
                }
                response
            },
        ));
        for (method_name, template, _) in NAMED_ROUTES.iter().chain(HANDLER_CHARGED_ROUTES) {
            let uri = concrete_uri(template);
            assert_eq!(
                matched_template(&app, method_name, &uri).await.as_deref(),
                Some(*template),
                "{method_name} {uri}"
            );
        }
        // The probe itself works: an unknown path matches no template.
        assert_eq!(
            matched_template(&app, "POST", "/api/no/such/route").await,
            None
        );
    }

    #[test]
    fn scale_multiplies_every_bucket_down_to_the_floor() {
        let limits = HttpLimits::new(0.1);
        assert_eq!(
            limits.rate(HttpBucket::Write),
            Some(Rate::per_minute(120, 120).scaled(0.1, Rate::MINIMUM))
        );
        let tiny = HttpLimits::new(0.01);
        assert_eq!(tiny.rate(HttpBucket::Expensive), Some(Rate::MINIMUM));
        assert_eq!(HttpLimits::disabled().rate(HttpBucket::Write), None);
    }

    #[test]
    fn check_refuses_after_the_burst_with_the_wait_for_one_token() {
        let limits = HttpLimits::new(1.0);
        let user = HttpKey::User(Uuid::new_v4());
        let start = Instant::now();
        for _ in 0..10 {
            assert_eq!(limits.check(HttpBucket::Expensive, &user, start), Ok(()));
        }
        let wait = limits
            .check(HttpBucket::Expensive, &user, start)
            .unwrap_err();
        // 10 per minute: one token every 6 s.
        assert_eq!(retry_after_secs(wait), 6);
        let six_seconds_later = start + Duration::from_secs(6);
        assert_eq!(
            limits.check(HttpBucket::Expensive, &user, six_seconds_later),
            Ok(())
        );
    }

    #[test]
    fn buckets_and_keys_are_independent() {
        let limits = HttpLimits::new(0.1);
        let first_user = HttpKey::User(Uuid::new_v4());
        let second_user = HttpKey::User(Uuid::new_v4());
        let now = Instant::now();
        // Expensive at scale 0.1: a burst of one.
        assert_eq!(
            limits.check(HttpBucket::Expensive, &first_user, now),
            Ok(())
        );
        assert!(
            limits
                .check(HttpBucket::Expensive, &first_user, now)
                .is_err()
        );
        assert_eq!(
            limits.check(HttpBucket::Expensive, &second_user, now),
            Ok(())
        );
        for bucket in HttpBucket::ALL
            .into_iter()
            .filter(|bucket| *bucket != HttpBucket::Expensive)
        {
            assert_eq!(limits.check(bucket, &first_user, now), Ok(()), "{bucket:?}");
        }
    }

    #[test]
    fn disabled_limits_never_refuse() {
        let limits = HttpLimits::disabled();
        let user = HttpKey::User(Uuid::new_v4());
        let now = Instant::now();
        for _ in 0..1000 {
            assert_eq!(limits.check(HttpBucket::Expensive, &user, now), Ok(()));
        }
    }

    fn state_with_scale(scale: f64) -> AppState {
        let mut state = AppState::in_memory();
        let config = knot_config::Config {
            rate_limit_enabled: true,
            rate_limit_scale: scale,
            ..Default::default()
        };
        state.rate_limits = Arc::new(RateLimits::from_config(&config));
        state.config = Arc::new(config);
        state
    }

    async fn ok() -> &'static str {
        "ok"
    }

    /// `/api`-shaped routes behind [`api_mw`], with the user from the
    /// `x-test-user` header as the session (none without it).
    fn api_app(state: AppState) -> Router {
        Router::new()
            .route("/api/docs/{id}", get(ok).patch(ok))
            .route("/api/workspace/export", get(ok))
            .route("/api/docs/{id}/duplicate", post(ok))
            .route("/api/notifications/read", patch(ok))
            .layer(axum::middleware::from_fn_with_state(state.clone(), api_mw))
            .layer(axum::middleware::from_fn(
                |mut request: Request, next: Next| async move {
                    let user_id = request
                        .headers()
                        .get("x-test-user")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<Uuid>().ok());
                    if let Some(user_id) = user_id {
                        request.extensions_mut().insert(AuthContext {
                            user_id,
                            workspace_id: Uuid::nil(),
                            role: WorkspaceRole::Editor,
                        });
                    }
                    next.run(request).await
                },
            ))
            .with_state(state)
    }

    async fn send(app: &Router, method: &str, uri: &str, user_id: Option<Uuid>) -> Response {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if let Some(user_id) = user_id {
            builder = builder.header("x-test-user", user_id.to_string());
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    const DOC: &str = "/api/docs/0193e0c0-0000-7000-8000-000000000001";

    #[tokio::test]
    async fn api_writes_past_the_burst_get_429_with_retry_after() {
        let app = api_app(state_with_scale(0.1));
        let user_id = Uuid::new_v4();
        // Write at scale 0.1: a burst of 12, 12 per minute.
        for _ in 0..12 {
            assert_eq!(
                send(&app, "PATCH", DOC, Some(user_id)).await.status(),
                StatusCode::OK
            );
        }
        let refused = send(&app, "PATCH", DOC, Some(user_id)).await;
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.headers()[header::RETRY_AFTER], "5");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(refused.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "rate.limited");
        assert_eq!(body["error"]["details"]["retry_after_secs"], 5);
        // Any other write shares the bucket.
        assert_eq!(
            send(&app, "PATCH", "/api/notifications/read", Some(user_id))
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn api_limits_are_per_user_and_leave_reads_alone() {
        let app = api_app(state_with_scale(0.1));
        let limited_user = Uuid::new_v4();
        for _ in 0..13 {
            send(&app, "PATCH", DOC, Some(limited_user)).await;
        }
        assert_eq!(
            send(&app, "PATCH", DOC, Some(limited_user)).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        for _ in 0..50 {
            assert_eq!(
                send(&app, "GET", DOC, Some(limited_user)).await.status(),
                StatusCode::OK
            );
        }
        assert_eq!(
            send(&app, "PATCH", DOC, Some(Uuid::new_v4()))
                .await
                .status(),
            StatusCode::OK
        );
        // The expensive bucket is its own.
        assert_eq!(
            send(
                &app,
                "POST",
                &format!("{DOC}/duplicate"),
                Some(limited_user)
            )
            .await
            .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn api_head_on_an_export_is_limited_like_get() {
        let app = api_app(state_with_scale(0.1));
        let user_id = Uuid::new_v4();
        // Export at scale 0.1: a burst of two.
        assert_eq!(
            send(&app, "HEAD", "/api/workspace/export", Some(user_id))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            send(&app, "GET", "/api/workspace/export", Some(user_id))
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            send(&app, "HEAD", "/api/workspace/export", Some(user_id))
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            send(&app, "GET", "/api/workspace/export", Some(user_id))
                .await
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn api_without_a_session_or_with_limits_off_passes() {
        let app = api_app(state_with_scale(0.1));
        for _ in 0..50 {
            assert_eq!(
                send(&app, "PATCH", DOC, None).await.status(),
                StatusCode::OK
            );
        }
        let unlimited_app = api_app(AppState::in_memory());
        let user_id = Uuid::new_v4();
        for _ in 0..200 {
            assert_eq!(
                send(&unlimited_app, "PATCH", DOC, Some(user_id))
                    .await
                    .status(),
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn take_user_token_charges_the_handler_bucket() {
        let state = state_with_scale(0.1);
        let user_id = Uuid::new_v4();
        assert!(take_user_token(&state, HttpBucket::Expensive, user_id).is_ok());
        let refusal = take_user_token(&state, HttpBucket::Expensive, user_id).unwrap_err();
        assert_eq!(refusal.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(take_user_token(&AppState::in_memory(), HttpBucket::Expensive, user_id).is_ok());
    }

    #[test]
    fn unresolved_reason_says_whether_any_proxy_is_trusted() {
        let nobody = client_ip::TrustedProxies::default();
        let private_proxies = client_ip::TrustedProxies::new(vec!["10.0.0.0/8".parse().unwrap()]);
        assert_eq!(
            UnresolvedReason::of(&nobody),
            UnresolvedReason::NoTrustedProxies
        );
        assert_eq!(
            UnresolvedReason::of(&private_proxies),
            UnresolvedReason::UntrustedPeer
        );
        assert_eq!(
            UnresolvedReason::NoTrustedProxies.label(),
            "no_trusted_proxies"
        );
        assert_eq!(UnresolvedReason::UntrustedPeer.label(), "untrusted_peer");
    }

    /// Keeps the log lines a test emits.
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_key_is_logged_once_when_it_starts_being_limited() {
        let captured = CapturedLogs::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let limits = HttpLimits::new(0.1);
            let user = HttpKey::User(Uuid::new_v4());
            let now = Instant::now();
            limits.check(HttpBucket::Expensive, &user, now).unwrap();
            for _ in 0..5 {
                assert!(limits.check(HttpBucket::Expensive, &user, now).is_err());
            }
        });
        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert_eq!(logs.matches("rate limit reached").count(), 1, "{logs}");
        assert!(logs.contains("bucket=\"expensive\""), "{logs}");
    }
}
