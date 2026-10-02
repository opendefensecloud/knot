//! Scrape /metrics after generating some HTTP traffic.
//!
//! NB: `/metrics` is served on a SEPARATE port (KNOT_METRICS_ADDR /
//! default :9090) — it is NOT on the main axum router. We install the
//! exporter on a random port, drive the app via oneshot, then scrape
//! over HTTP.
//!
//! The Prometheus recorder is a global singleton. This file is structured
//! so all tests share a single recorder install via std::sync::OnceLock,
//! which keeps the suite robust when cargo runs multiple tests in one
//! process.

use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use knot_test_support::fresh_db;
use tower::ServiceExt;

static EXPORTER_PORT: OnceLock<u16> = OnceLock::new();

fn install_exporter() -> u16 {
    *EXPORTER_PORT.get_or_init(|| {
        let port = pick_free_port();
        let addr = format!("127.0.0.1:{port}");
        // The exporter's HTTP listener is a task on the runtime that installs
        // it. Each #[tokio::test] runtime dies with its test, so the listener
        // gets a runtime of its own that outlives every test in this file.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    knot_obs::metrics::init(&addr).expect("install exporter");
                    ready_tx.send(()).unwrap();
                    std::future::pending::<()>().await;
                });
        });
        ready_rx.recv().unwrap();
        port
    })
}

fn pick_free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_endpoint_lists_described_names_after_traffic() {
    let port = install_exporter();
    let db = fresh_db().await;
    let mut state = knot_server::AppState::with_pool(db.pool.clone());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let app = knot_server::router_with_state(state);

    // Generate traffic on a couple of routes.
    for _ in 0..3 {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let body = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Described families appear in the output even with zero samples;
    // ones we actually drove also include sample lines.
    assert!(
        body.contains("knot_http_requests_total"),
        "missing knot_http_requests_total\n{body}"
    );
    assert!(
        body.contains("knot_http_request_duration_seconds"),
        "missing knot_http_request_duration_seconds\n{body}"
    );
    // knot_room_active is a gauge that only appears after a sample is recorded;
    // skip asserting on it here since no room traffic is generated in this test.
}

async fn scrape(port: u16) -> String {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// Sum of every sample line of `series` (e.g. a histogram's `_count`).
fn sample_total(body: &str, series: &str) -> f64 {
    body.lines()
        .filter(|line| line.starts_with(series))
        .filter_map(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
        .sum()
}

/// The dashboard, the SLO doc and the PrometheusRule all query
/// `histogram_quantile(..., ..._bucket)`: a summary exports no buckets, so
/// every latency panel and alert would silently show nothing.
#[tokio::test(flavor = "multi_thread")]
async fn http_latency_is_exported_as_a_bucketed_histogram() {
    let port = install_exporter();
    let db = fresh_db().await;
    let mut state = knot_server::AppState::with_pool(db.pool.clone());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let app = knot_server::router_with_state(state);
    let r = app
        .oneshot(
            Request::builder()
                .uri("/api/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let body = scrape(port).await;
    assert!(
        body.contains("# TYPE knot_http_request_duration_seconds histogram"),
        "not a histogram\n{body}"
    );
    assert!(
        body.contains("knot_http_request_duration_seconds_bucket{"),
        "no buckets\n{body}"
    );
}

/// Opening a document over the collab socket is where the editor waits for
/// content, but the HTTP histogram stops at the 101 upgrade. These two
/// series cover the rest: the DB hydrate of a cold room, and upgrade →
/// initial SyncStep2 queued for every join.
#[tokio::test(flavor = "multi_thread")]
async fn opening_a_document_records_hydrate_and_initial_sync_latency() {
    use futures_util::StreamExt;
    use std::sync::Arc;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let port = install_exporter();
    let db = fresh_db().await;
    let mut state = knot_server::AppState::with_pool(db.pool.clone());
    state.hasher = Arc::new(knot_auth::Hasher::fast_for_tests());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    state.cookie_secure = false;
    state.rooms_v2 = Some(Arc::new(knot_crdt::Rooms::new(
        Arc::new(knot_crdt::YrsEngine),
        Arc::new(knot_crdt::MemBus::new()),
        Arc::new(knot_storage::PgUpdatesStore::new(db.pool.clone())),
        Arc::new(knot_storage::PgSnapshotStore::new(db.pool.clone())),
        knot_crdt::SnapshotPolicy {
            every_n: 100,
            idle: std::time::Duration::from_secs(60),
        },
        std::time::Duration::from_secs(300),
    )));
    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let hash = state.hasher.hash("metricspass").unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local("metrics@ws.test", "M", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace.id, user.id, knot_storage::WorkspaceRole::Owner)
        .await
        .unwrap();
    let doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(workspace.id, None, "Doc", "m", user.id)
        .await
        .unwrap();

    let app = knot_server::router_with_state(state);
    let login = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": "metrics@ws.test", "password": "metricspass"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::NO_CONTENT);
    let sid_cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .find(|cookie| cookie.starts_with("sid="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let before = scrape(port).await;
    let mut request = format!("ws://{addr}/collab/doc/{}", doc.id)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("cookie", sid_cookie.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let first_frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("initial sync frame")
        .unwrap()
        .unwrap();
    assert_eq!(
        &first_frame.into_data()[..2],
        &[0u8, 1u8],
        "SyncStep2 first"
    );

    let after = scrape(port).await;
    for series in [
        "knot_room_hydrate_seconds_count",
        "knot_collab_initial_sync_seconds_count",
    ] {
        assert!(
            sample_total(&after, series) >= sample_total(&before, series) + 1.0,
            "{series} did not grow\n{after}"
        );
        let buckets = series.replace("_count", "_bucket{");
        assert!(after.contains(&buckets), "{buckets} missing\n{after}");
    }
}
