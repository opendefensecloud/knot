//! SPA static serving: cache headers and compression for `web/dist`.

use std::io::Read;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

const INDEX_HTML: &str = "<!doctype html><script src=\"/assets/index-Abc123.js\"></script>";

/// A bundle big and repetitive enough that any real compressor shrinks it.
fn bundle_source() -> String {
    "export const greeting = 'hello from the knot editor bundle';\n".repeat(2_000)
}

fn dist_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), INDEX_HTML).unwrap();
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/index-Abc123.js"), bundle_source()).unwrap();
    dir
}

async fn get(
    dist: &tempfile::TempDir,
    path: &str,
    accept_encoding: Option<&str>,
) -> axum::http::Response<Body> {
    let app: axum::Router = knot_server::static_files::router(dist.path().to_str().unwrap());
    let mut request = Request::builder().uri(path);
    if let Some(encodings) = accept_encoding {
        request = request.header(header::ACCEPT_ENCODING, encodings);
    }
    app.oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn cache_control(response: &axum::http::Response<Body>) -> Option<&str> {
    response
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|value| value.to_str().unwrap())
}

async fn body_bytes(response: axum::http::Response<Body>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn hashed_asset_is_cacheable_forever() {
    let dist = dist_dir();
    let response = get(&dist, "/assets/index-Abc123.js", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        cache_control(&response),
        Some("public, max-age=31536000, immutable")
    );
}

#[tokio::test]
async fn index_html_must_be_revalidated() {
    let dist = dist_dir();
    let response = get(&dist, "/", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(cache_control(&response), Some("no-cache"));
    assert_eq!(body_bytes(response).await, INDEX_HTML.as_bytes());
}

#[tokio::test]
async fn client_side_route_serves_index_html_that_must_be_revalidated() {
    let dist = dist_dir();
    let response = get(&dist, "/docs/0b6f3a56-0000-0000-0000-000000000000", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(cache_control(&response), Some("no-cache"));
    assert_eq!(body_bytes(response).await, INDEX_HTML.as_bytes());
}

#[tokio::test]
async fn missing_asset_is_404_not_a_cached_index_html() {
    let dist = dist_dir();
    let response = get(&dist, "/assets/KnotEditor-Gone999.js", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let cache = cache_control(&response).unwrap_or_default().to_owned();
    assert!(!cache.contains("immutable"), "cache-control was {cache:?}");
    assert_ne!(body_bytes(response).await, INDEX_HTML.as_bytes());
}

#[tokio::test]
async fn asset_is_brotli_compressed_when_the_client_accepts_it() {
    let dist = dist_dir();
    let response = get(&dist, "/assets/index-Abc123.js", Some("gzip, deflate, br")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .map(|value| value.to_str().unwrap()),
        Some("br")
    );
    let compressed = body_bytes(response).await;
    assert!(compressed.len() < bundle_source().len() / 10);
    let mut decompressed = String::new();
    brotli::Decompressor::new(compressed.as_slice(), 4096)
        .read_to_string(&mut decompressed)
        .unwrap();
    assert_eq!(decompressed, bundle_source());
}

#[tokio::test]
async fn asset_is_sent_uncompressed_when_the_client_does_not_ask() {
    let dist = dist_dir();
    let response = get(&dist, "/assets/index-Abc123.js", None).await;
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(body_bytes(response).await, bundle_source().as_bytes());
}
