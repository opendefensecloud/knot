//! Serves the built SPA (`web/dist`).
//!
//! Vite content-hashes every file under `/assets`, so an asset URL never
//! changes content: those are cached for a year, and a missing one is a real
//! 404 rather than `index.html` served as JavaScript. Everything else —
//! `index.html` for client-side routes, favicons, the manifest — is `no-cache`
//! so a deploy is picked up on the next load instead of a heuristically cached
//! shell pointing at chunks that no longer exist.
//!
//! Compression is applied here and not to the API: these bodies are public,
//! so compressing them cannot leak a secret the way BREACH exploits.

use axum::Router;
use axum::http::{HeaderValue, Response, StatusCode, header};
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

pub fn router<S>(web_dist: &str) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let assets = ServiceBuilder::new()
        .map_response(cache_forever_when_found)
        .service(ServeDir::new(format!("{web_dist}/assets")));
    // `fallback` (not `not_found_service`) so client-side routes answer 200.
    let pages = ServiceBuilder::new()
        .map_response(always_revalidate)
        .service(
            ServeDir::new(web_dist)
                .append_index_html_on_directories(true)
                .fallback(ServeFile::new(format!("{web_dist}/index.html"))),
        );
    Router::new()
        .nest_service("/assets", assets)
        .fallback_service(pages)
        .layer(CompressionLayer::new())
}

fn cache_forever_when_found<B>(mut response: Response<B>) -> Response<B> {
    let status = response.status();
    if status.is_success() || status == StatusCode::NOT_MODIFIED {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    }
    response
}

fn always_revalidate<B>(mut response: Response<B>) -> Response<B> {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}
