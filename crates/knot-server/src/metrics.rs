//! HTTP request/response metrics middleware.
//!
//! Emits two metrics with bounded labels:
//!   knot_http_requests_total{method,route,status_class}
//!   knot_http_request_duration_seconds{method,route,status_class}
//!
//! `route` is the MatchedPath template (e.g. "/api/docs/{id}"), NOT the
//! raw URI — keeps cardinality bounded.

use std::time::Instant;

use axum::{
    body::Body,
    extract::MatchedPath,
    http::{Request, Response},
    middleware::Next,
};
use metrics::{counter, histogram};

pub async fn record(req: Request<Body>, next: Next) -> Response<Body> {
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());

    let start = Instant::now();
    let resp = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();

    let status = resp.status().as_u16();
    let status_class = match status / 100 {
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    };

    let labels = [
        ("method", method.to_string()),
        ("route", route),
        ("status_class", status_class.to_string()),
    ];
    counter!("knot_http_requests_total", &labels).increment(1);
    histogram!("knot_http_request_duration_seconds", &labels).record(elapsed);

    resp
}
