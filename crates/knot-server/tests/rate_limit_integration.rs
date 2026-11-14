//! HTTP rate limits through the real router (issue #88, spec tests 3 and 4).
//!
//! The anonymous-route tests (sign-in, public pages, client IP) and the
//! constructor test need no database: the IP buckets run before the
//! handlers, which answer 500/410 without storage. The `/api` tests sign in
//! real users and need Postgres (`knot_test_support::fresh_db`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_config::Config;
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

/// Limits on, every bucket × `scale` (0.1: write 12, expensive 1, export 2,
/// sign-in 3, public 60 per burst).
fn limited_config(scale: f64, trusted_proxies: &str) -> Config {
    Config {
        rate_limit_enabled: true,
        rate_limit_scale: scale,
        trusted_proxies: trusted_proxies.into(),
        ..Default::default()
    }
}

fn in_memory_app(config: Config) -> Router {
    let mut state = AppState::in_memory();
    state.config = Arc::new(config);
    router_with_state(state)
}

/// A request from TCP peer `peer`, with each of `forwarded_lines` as an
/// `X-Forwarded-For` line.
fn request_from(
    method: &str,
    uri: &str,
    peer: &str,
    forwarded_lines: &[&str],
    body: Body,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for line in forwarded_lines {
        builder = builder.header("x-forwarded-for", *line);
    }
    let mut request = builder.body(body).unwrap();
    let peer_address: SocketAddr = format!("{peer}:40000").parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer_address));
    request
}

fn login_body(email: &str, password: &str) -> Body {
    Body::from(serde_json::json!({"email": email, "password": password}).to_string())
}

async fn sign_in_status(app: &Router, peer: &str, forwarded_lines: &[&str]) -> StatusCode {
    app.clone()
        .oneshot(request_from(
            "POST",
            "/auth/login",
            peer,
            forwarded_lines,
            login_body("someone@example.com", "wrong-password"),
        ))
        .await
        .unwrap()
        .status()
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::json!({}))
}

/// Counts every counter increment, by `name{label=value,…}`. Installed per
/// test thread with `metrics::set_default_local_recorder` (the tests run on
/// a current-thread runtime, so the handlers record on that thread too).
#[derive(Default)]
struct CountingRecorder {
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
}

impl CountingRecorder {
    fn count(&self, rendered_key: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(rendered_key)
            .map_or(0, |counter| {
                counter.load(std::sync::atomic::Ordering::Relaxed)
            })
    }
}

fn render_key(key: &Key) -> String {
    let mut labels: Vec<String> = key
        .labels()
        .map(|label| format!("{}={}", label.key(), label.value()))
        .collect();
    labels.sort();
    if labels.is_empty() {
        key.name().to_owned()
    } else {
        format!("{}{{{}}}", key.name(), labels.join(","))
    }
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let counter = self
            .counters
            .lock()
            .unwrap()
            .entry(render_key(key))
            .or_default()
            .clone();
        Counter::from_arc(counter)
    }
    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

// ---------------------------------------------------------------------------
// Constructors (no database)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn constructors_build_state_without_limits() {
    let in_memory = AppState::in_memory();
    assert!(!in_memory.config.rate_limit_enabled);
    assert!(!in_memory.rate_limits.enabled);
    assert!(!in_memory.rate_limits.http.is_enabled());
    assert!(in_memory.trusted_proxies.is_empty());

    // A lazy pool opens no connection, so this needs no database.
    let lazy_pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://knot@127.0.0.1:1/never")
        .unwrap();
    let with_pool = AppState::with_pool(lazy_pool);
    assert!(!with_pool.config.rate_limit_enabled);
    assert!(!with_pool.rate_limits.enabled);
    assert!(!with_pool.rate_limits.http.is_enabled());

    // Everything else in the constructors' config is the default.
    let defaults_with_limits_off = Config {
        rate_limit_enabled: false,
        ..Default::default()
    };
    assert_eq!(
        format!("{:?}", in_memory.config),
        format!("{defaults_with_limits_off:?}")
    );
}

#[tokio::test]
async fn limits_off_never_answer_429_on_sign_in() {
    let app = router_with_state(AppState::in_memory());
    for _ in 0..100 {
        assert_ne!(
            sign_in_status(&app, "192.0.2.1", &[]).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}

// ---------------------------------------------------------------------------
// Anonymous routes and client IP (no database)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sign_in_is_limited_per_client_ip() {
    let app = in_memory_app(limited_config(0.1, ""));
    for _ in 0..3 {
        assert_ne!(
            sign_in_status(&app, "192.0.2.1", &[]).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let refused = app
        .clone()
        .oneshot(request_from(
            "POST",
            "/auth/login",
            "192.0.2.1",
            &[],
            login_body("someone@example.com", "x"),
        ))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    // 3 per minute: one token every 20 s.
    assert_eq!(refused.headers()[header::RETRY_AFTER], "20");
    let body = json_body(refused).await;
    assert_eq!(body["error"]["code"], "rate.limited");
    assert_eq!(body["error"]["details"]["retry_after_secs"], 20);

    // Setup shares the sign-in bucket.
    let setup = app
        .clone()
        .oneshot(request_from(
            "POST",
            "/auth/setup",
            "192.0.2.1",
            &[],
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(setup.status(), StatusCode::TOO_MANY_REQUESTS);

    // Another client is unaffected.
    assert_ne!(
        sign_in_status(&app, "192.0.2.2", &[]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn ipv6_clients_share_a_bucket_per_64() {
    let app = in_memory_app(limited_config(0.1, ""));
    for host in ["2001:db8:1:2::1", "2001:db8:1:2::2", "2001:db8:1:2::3"] {
        assert_ne!(
            sign_in_status(&app, &format!("[{host}]"), &[]).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        sign_in_status(&app, "[2001:db8:1:2::ffff]", &[]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_ne!(
        sign_in_status(&app, "[2001:db8:1:3::1]", &[]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn session_config_and_logout_are_not_limited() {
    let app = in_memory_app(limited_config(0.01, ""));
    for (method, uri) in [
        ("GET", "/auth/session"),
        ("GET", "/auth/config"),
        ("POST", "/auth/logout"),
        ("GET", "/auth/oidc/login"),
        ("GET", "/api/healthz"),
        ("GET", "/api/version"),
    ] {
        for _ in 0..20 {
            let response = app
                .clone()
                .oneshot(request_from(method, uri, "192.0.2.1", &[], Body::empty()))
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "{method} {uri}"
            );
        }
    }
}

#[tokio::test]
async fn public_pages_are_limited_per_client_ip_head_included() {
    // Public at scale 0.01: a burst of six.
    let app = in_memory_app(limited_config(0.01, ""));
    let public_uris = [
        "/p/sometoken",
        "/p/sometoken/blobs/0193e0c0-0000-7000-8000-000000000001",
        "/p/sometoken/boards/0193e0c0-0000-7000-8000-000000000001/svg",
    ];
    for request_number in 0..6 {
        let uri = public_uris[request_number % public_uris.len()];
        let method = if request_number % 2 == 0 {
            "GET"
        } else {
            "HEAD"
        };
        let response = app
            .clone()
            .oneshot(request_from(method, uri, "192.0.2.1", &[], Body::empty()))
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{uri}");
    }
    for method in ["GET", "HEAD"] {
        let response = app
            .clone()
            .oneshot(request_from(
                method,
                "/p/sometoken",
                "192.0.2.1",
                &[],
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{method}");
    }
    let other_client = app
        .clone()
        .oneshot(request_from(
            "GET",
            "/p/sometoken",
            "192.0.2.2",
            &[],
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_ne!(other_client.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// A public request from `peer` sending `accept` as its `Accept` header.
fn public_request_accepting(method: &str, uri: &str, peer: &str, accept: &str) -> Request<Body> {
    let mut request = request_from(method, uri, peer, &[], Body::empty());
    request
        .headers_mut()
        .insert(header::ACCEPT, accept.parse().unwrap());
    request
}

#[tokio::test]
async fn a_refused_share_page_navigation_gets_an_html_page_with_the_wait() {
    // Public at scale 0.01: a burst of six.
    let app = in_memory_app(limited_config(0.01, ""));
    let browser_accept = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
    for _ in 0..6 {
        app.clone()
            .oneshot(public_request_accepting(
                "GET",
                "/p/sometoken",
                "192.0.2.1",
                browser_accept,
            ))
            .await
            .unwrap();
    }

    let navigation = app
        .clone()
        .oneshot(public_request_accepting(
            "GET",
            "/p/sometoken",
            "192.0.2.1",
            browser_accept,
        ))
        .await
        .unwrap();
    assert_eq!(navigation.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        navigation.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    let retry_after: u64 = navigation.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=10).contains(&retry_after), "{retry_after}");
    let page = String::from_utf8(
        navigation
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(page.starts_with("<!doctype html>"), "{page}");
    assert!(page.contains("Too many requests"), "{page}");
    assert!(
        page.contains(&format!("Try again in {retry_after} s.")),
        "{page}"
    );

    // The SPA's fetch (no text/html) and the page's files keep the envelope.
    for (uri, accept) in [
        ("/p/sometoken", "*/*"),
        (
            "/p/sometoken/blobs/0193e0c0-0000-7000-8000-000000000001",
            browser_accept,
        ),
    ] {
        let refused = app
            .clone()
            .oneshot(public_request_accepting("GET", uri, "192.0.2.1", accept))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS, "{uri}");
        assert_eq!(
            refused.headers()[header::CONTENT_TYPE],
            "application/json",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn two_clients_behind_a_trusted_proxy_get_separate_buckets() {
    let app = in_memory_app(limited_config(0.1, "10.0.0.0/8"));
    for _ in 0..3 {
        assert_ne!(
            sign_in_status(&app, "10.1.2.3", &["203.0.113.1"]).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        sign_in_status(&app, "10.1.2.3", &["203.0.113.1"]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // Through another trusted hop, the same client is the same bucket.
    assert_eq!(
        sign_in_status(&app, "10.9.9.9", &["203.0.113.1, 10.4.4.4"]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A spoofed left-most entry doesn't move the client to another bucket.
    assert_eq!(
        sign_in_status(&app, "10.1.2.3", &["198.51.100.1, 203.0.113.1"]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_ne!(
        sign_in_status(&app, "10.1.2.3", &["203.0.113.2"]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn untrusted_peer_with_forwarded_header_skips_ip_buckets() {
    let recorder = CountingRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let app = in_memory_app(limited_config(0.1, ""));
    // No proxy is trusted, yet the header is there: an unknown proxy in
    // front, behind which every client has its address. Never a shared
    // bucket: these requests skip the IP buckets.
    for client_number in 0..20 {
        let forwarded = format!("203.0.113.{client_number}");
        assert_ne!(
            sign_in_status(&app, "10.1.2.3", &[forwarded.as_str()]).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        recorder.count("knot_client_ip_unresolved_total{reason=no_trusted_proxies}"),
        20
    );
    // The same peer without the header is limited as itself.
    for _ in 0..3 {
        assert_ne!(
            sign_in_status(&app, "10.1.2.3", &[]).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        sign_in_status(&app, "10.1.2.3", &[]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        recorder.count("knot_client_ip_unresolved_total{reason=no_trusted_proxies}"),
        20
    );
    assert_eq!(recorder.count("knot_rate_limited_total{bucket=sign_in}"), 1);
}

#[tokio::test]
async fn an_untrusted_peer_is_limited_by_its_address_when_proxies_are_trusted() {
    let recorder = CountingRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let app = in_memory_app(limited_config(0.1, "10.0.0.0/8"));
    // A direct client outside the trusted proxies can't pick its bucket by
    // rotating the header.
    for client_number in 0..3 {
        let forwarded = format!("203.0.113.{client_number}");
        assert_ne!(
            sign_in_status(&app, "198.51.100.7", &[forwarded.as_str()]).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        sign_in_status(&app, "198.51.100.7", &["203.0.113.99"]).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        recorder.count("knot_client_ip_unresolved_total{reason=untrusted_peer}"),
        4
    );
    assert_eq!(
        recorder.count("knot_client_ip_unresolved_total{reason=no_trusted_proxies}"),
        0,
        "proxies are configured: no sign of a missing setting"
    );
}

#[tokio::test]
async fn a_malformed_forwarded_entry_is_counted() {
    let recorder = CountingRecorder::default();
    let _recorder_guard = metrics::set_default_local_recorder(&recorder);
    let app = in_memory_app(limited_config(1.0, "10.0.0.0/8"));
    for forwarded in ["203.0.113.1, not-an-ip", "203.0.113.1,", ""] {
        sign_in_status(&app, "10.1.2.3", &[forwarded]).await;
    }
    sign_in_status(&app, "10.1.2.3", &["203.0.113.1"]).await;
    assert_eq!(
        recorder.count("knot_client_ip_malformed_forwarded_total"),
        3
    );
}

// ---------------------------------------------------------------------------
// `/api` buckets and the login throttle (Postgres)
// ---------------------------------------------------------------------------

struct SeededWorkspace {
    state: AppState,
    doc_id: Uuid,
}

const PASSWORD: &str = "hunter22";

/// A workspace with alice (owner) and bob (editor), and one doc by alice.
async fn seeded_workspace(config: Config) -> SeededWorkspace {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut state = AppState::with_pool(pool);
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    state.config = Arc::new(config);

    let hash = state.hasher.hash(PASSWORD).unwrap();
    let workspaces = state.workspaces.clone().unwrap();
    let users = state.users.clone().unwrap();
    let workspace = workspaces.create("default", "Workspace").await.unwrap();
    let alice = users
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    let bob = users
        .create_local("bob@example.com", "Bob", &hash)
        .await
        .unwrap();
    workspaces
        .add_member(workspace.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    workspaces
        .add_member(workspace.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(workspace.id, None, "Limited", "m", alice.id)
        .await
        .unwrap();
    SeededWorkspace {
        state,
        doc_id: doc.id,
    }
}

/// A signed-in session: the `sid=…` cookie pair and the CSRF token.
struct Session {
    sid_cookie: String,
    csrf_token: String,
}

/// Sign `email` in from `peer` (each user its own address, so sign-ins
/// don't spend each other's sign-in tokens).
async fn sign_in(app: &Router, email: &str, peer: &str) -> Session {
    let response = app
        .clone()
        .oneshot(request_from(
            "POST",
            "/auth/login",
            peer,
            &[],
            login_body(email, PASSWORD),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect();
    let cookie_pair = |name: &str| {
        cookies
            .iter()
            .find(|cookie| cookie.starts_with(&format!("{name}=")))
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    };
    let csrf_pair = cookie_pair("csrf");
    Session {
        sid_cookie: cookie_pair("sid"),
        csrf_token: csrf_pair.split_once('=').unwrap().1.to_string(),
    }
}

/// Send `method uri` as `session` (none: anonymous), with the CSRF header
/// when `with_csrf`.
async fn send_as(
    app: &Router,
    session: Option<&Session>,
    with_csrf: bool,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(session) = session {
        builder = builder.header(
            "cookie",
            format!("{}; csrf={}", session.sid_cookie, session.csrf_token),
        );
        if with_csrf {
            builder = builder.header("x-csrf-token", &session.csrf_token);
        }
    }
    app.clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn rename(app: &Router, session: &Session, doc_id: Uuid) -> StatusCode {
    send_as(
        app,
        Some(session),
        true,
        "PATCH",
        &format!("/api/docs/{doc_id}"),
        serde_json::json!({"title": "Renamed"}),
    )
    .await
    .status()
}

#[tokio::test]
async fn writes_past_the_burst_get_429_with_retry_after() {
    let seeded = seeded_workspace(limited_config(0.1, "")).await;
    let app = router_with_state(seeded.state);
    let alice = sign_in(&app, "alice@example.com", "192.0.2.1").await;
    for _ in 0..12 {
        assert_eq!(rename(&app, &alice, seeded.doc_id).await, StatusCode::OK);
    }
    let refused = send_as(
        &app,
        Some(&alice),
        true,
        "PATCH",
        &format!("/api/docs/{}", seeded.doc_id),
        serde_json::json!({"title": "Once more"}),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    // 12 per minute: one token every 5 s, less the real time the 12
    // renames took (the bucket refills meanwhile). The exact rounding is
    // pinned with injected time by the unit tests in `rate_limit::http`.
    let retry_after: u64 = refused.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=5).contains(&retry_after), "Retry-After {retry_after}");
    let body = json_body(refused).await;
    assert_eq!(body["error"]["code"], "rate.limited");
    assert_eq!(
        body["error"]["details"]["retry_after_secs"], retry_after,
        "the header and the details agree"
    );
}

#[tokio::test]
async fn another_user_and_reads_are_unaffected_by_a_limited_user() {
    let seeded = seeded_workspace(limited_config(0.1, "")).await;
    let app = router_with_state(seeded.state);
    let alice = sign_in(&app, "alice@example.com", "192.0.2.1").await;
    let bob = sign_in(&app, "bob@example.com", "192.0.2.2").await;
    for _ in 0..12 {
        rename(&app, &alice, seeded.doc_id).await;
    }
    assert_eq!(
        rename(&app, &alice, seeded.doc_id).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(rename(&app, &bob, seeded.doc_id).await, StatusCode::OK);
    for uri in [
        "/api/docs".to_string(),
        format!("/api/docs/{}", seeded.doc_id),
    ] {
        for _ in 0..30 {
            let response =
                send_as(&app, Some(&alice), true, "GET", &uri, serde_json::json!({})).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
    }
}

#[tokio::test]
async fn head_on_export_is_limited_like_get() {
    let seeded = seeded_workspace(limited_config(0.1, "")).await;
    let app = router_with_state(seeded.state);
    let alice = sign_in(&app, "alice@example.com", "192.0.2.1").await;
    // Export at scale 0.1: a burst of two.
    for _ in 0..2 {
        let response = send_as(
            &app,
            Some(&alice),
            true,
            "HEAD",
            "/api/workspace/export",
            serde_json::json!({}),
        )
        .await;
        assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
    for (method, uri) in [
        ("HEAD", "/api/workspace/export".to_string()),
        ("GET", "/api/workspace/export".to_string()),
        ("HEAD", format!("/api/docs/{}/export", seeded.doc_id)),
    ] {
        let response = send_as(
            &app,
            Some(&alice),
            true,
            method,
            &uri,
            serde_json::json!({}),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "{method} {uri}"
        );
    }
}

#[tokio::test]
async fn each_named_bucket_is_separate() {
    let seeded = seeded_workspace(limited_config(0.1, "")).await;
    let app = router_with_state(seeded.state);
    let alice = sign_in(&app, "alice@example.com", "192.0.2.1").await;
    for _ in 0..12 {
        rename(&app, &alice, seeded.doc_id).await;
    }
    assert_eq!(
        rename(&app, &alice, seeded.doc_id).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let duplicate_uri = format!("/api/docs/{}/duplicate", seeded.doc_id);
    let duplicate = send_as(
        &app,
        Some(&alice),
        true,
        "POST",
        &duplicate_uri,
        serde_json::json!({}),
    )
    .await;
    assert_ne!(duplicate.status(), StatusCode::TOO_MANY_REQUESTS);
    let export = send_as(
        &app,
        Some(&alice),
        true,
        "GET",
        &format!("/api/docs/{}/export", seeded.doc_id),
        serde_json::json!({}),
    )
    .await;
    assert_ne!(export.status(), StatusCode::TOO_MANY_REQUESTS);
    // Expensive at scale 0.1: a burst of one, now spent.
    let second_duplicate = send_as(
        &app,
        Some(&alice),
        true,
        "POST",
        &duplicate_uri,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(second_duplicate.status(), StatusCode::TOO_MANY_REQUESTS);
    let second_export = send_as(
        &app,
        Some(&alice),
        true,
        "GET",
        &format!("/api/docs/{}/export", seeded.doc_id),
        serde_json::json!({}),
    )
    .await;
    assert_ne!(second_export.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_401_or_a_csrf_failure_costs_nothing() {
    let seeded = seeded_workspace(limited_config(0.1, "")).await;
    let app = router_with_state(seeded.state);
    let alice = sign_in(&app, "alice@example.com", "192.0.2.1").await;
    let rename_uri = format!("/api/docs/{}", seeded.doc_id);
    for _ in 0..20 {
        let without_csrf = send_as(
            &app,
            Some(&alice),
            false,
            "PATCH",
            &rename_uri,
            serde_json::json!({"title": "x"}),
        )
        .await;
        assert_eq!(without_csrf.status(), StatusCode::FORBIDDEN);
        let without_session = send_as(
            &app,
            None,
            false,
            "PATCH",
            &rename_uri,
            serde_json::json!({"title": "x"}),
        )
        .await;
        assert_eq!(without_session.status(), StatusCode::UNAUTHORIZED);
    }
    for _ in 0..12 {
        assert_eq!(rename(&app, &alice, seeded.doc_id).await, StatusCode::OK);
    }
}

#[tokio::test]
async fn limits_off_never_429() {
    let seeded = seeded_workspace(Config {
        rate_limit_enabled: false,
        ..Default::default()
    })
    .await;
    let app = router_with_state(seeded.state);
    let alice = sign_in(&app, "alice@example.com", "192.0.2.1").await;
    for _ in 0..130 {
        assert_eq!(rename(&app, &alice, seeded.doc_id).await, StatusCode::OK);
    }
}

#[tokio::test]
async fn password_change_takes_an_expensive_token() {
    let seeded = seeded_workspace(limited_config(0.1, "")).await;
    let app = router_with_state(seeded.state);
    let alice = sign_in(&app, "alice@example.com", "192.0.2.1").await;
    let wrong_current = serde_json::json!({"current": "not-it-at-all", "new": "brand-new-pass"});
    let first = send_as(
        &app,
        Some(&alice),
        false,
        "POST",
        "/auth/password",
        wrong_current.clone(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::UNAUTHORIZED);
    let second = send_as(
        &app,
        Some(&alice),
        false,
        "POST",
        "/auth/password",
        wrong_current,
    )
    .await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json_body(second).await["error"]["code"], "rate.limited");
}

/// `POST /auth/password` with `content_type`, as a signed-in user (the
/// session's `AuthContext` attached directly: no database needed).
fn password_change_request(user_id: Uuid, content_type: &str, body: &str) -> Request<Body> {
    let mut request = request_from(
        "POST",
        "/auth/password",
        "192.0.2.1",
        &[],
        Body::from(body.to_string()),
    );
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    request
        .extensions_mut()
        .insert(knot_server::auth::AuthContext {
            user_id,
            workspace_id: Uuid::new_v4(),
            role: WorkspaceRole::Owner,
        });
    request
}

#[tokio::test]
async fn a_password_change_a_plain_form_could_send_takes_no_token() {
    // `/auth/password` has no CSRF layer: a same-site page's form POSTs
    // (text/plain, urlencoded, multipart) must not drain the `expensive`
    // bucket that duplicate and import share. Expensive at scale 0.1: 1.
    let app = in_memory_app(limited_config(0.1, ""));
    let user_id = Uuid::new_v4();
    let json_shaped_body = r#"{"current":"not-it-at-all","new":"brand-new-pass"}"#;
    for form_content_type in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
    ] {
        let response = app
            .clone()
            .oneshot(password_change_request(
                user_id,
                form_content_type,
                json_shaped_body,
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{form_content_type}"
        );
    }
    let first_json = app
        .clone()
        .oneshot(password_change_request(
            user_id,
            "application/json; charset=utf-8",
            json_shaped_body,
        ))
        .await
        .unwrap();
    assert_ne!(
        first_json.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the forms took no token"
    );
    let second_json = app
        .clone()
        .oneshot(password_change_request(
            user_id,
            "application/json",
            json_shaped_body,
        ))
        .await
        .unwrap();
    assert_eq!(
        second_json.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a JSON request takes the token"
    );
}

#[tokio::test]
async fn login_throttle_keys_on_the_client_behind_a_trusted_proxy() {
    // Limits off: only the failure throttle is at work.
    let seeded = seeded_workspace(Config {
        rate_limit_enabled: false,
        trusted_proxies: "10.0.0.0/8".into(),
        ..Default::default()
    })
    .await;
    let pool = seeded.state.pool.clone().unwrap();
    let app = router_with_state(seeded.state);
    // Five failures from one client behind the ingress (unknown emails, so
    // only its IP key fills) throttle that client …
    for attempt in 0..5 {
        let response = app
            .clone()
            .oneshot(request_from(
                "POST",
                "/auth/login",
                "10.1.2.3",
                &["203.0.113.1"],
                login_body(&format!("nobody{attempt}@example.com"), "wrong"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let throttled = app
        .clone()
        .oneshot(request_from(
            "POST",
            "/auth/login",
            "10.1.2.3",
            &["203.0.113.1"],
            login_body("alice@example.com", PASSWORD),
        ))
        .await
        .unwrap();
    assert_eq!(throttled.status(), StatusCode::UNAUTHORIZED);
    // … and not everyone else behind the same ingress.
    let other_client = app
        .clone()
        .oneshot(request_from(
            "POST",
            "/auth/login",
            "10.1.2.3",
            &["203.0.113.2"],
            login_body("alice@example.com", PASSWORD),
        ))
        .await
        .unwrap();
    assert_eq!(other_client.status(), StatusCode::NO_CONTENT);
    // The session records the client, not the proxy.
    let stored_ip: Option<String> =
        sqlx::query_scalar("SELECT host(ip) FROM sessions ORDER BY created_at DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_ip.as_deref(), Some("203.0.113.2"));
}
