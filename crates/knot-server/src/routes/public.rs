//! GET /p/:token — anonymous read of a doc via a public share link.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::Response,
    routing::get,
    Router,
};

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/p/:token", get(public_doc))
}

async fn public_doc(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some(shares) = state.shares.clone() else {
        return gone("internal");
    };
    let Some(docs) = state.docs.clone() else {
        return gone("internal");
    };
    let Some(cache) = state.markdown_cache.clone() else {
        return gone("internal");
    };

    let Some(share) = shares.find_alive(&token).await.ok().flatten() else {
        return gone("This link has expired or been revoked.");
    };
    let doc = match docs.get(share.doc_id).await {
        Ok(Some(d)) => d,
        _ => return gone("This document is no longer available."),
    };
    let cached = match cache.get(share.doc_id).await {
        Ok(Some(c)) => c,
        _ => return placeholder(&doc.title),
    };

    let html = render_markdown(&doc.title, &cached.markdown_text);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=60")
        .body(Body::from(html))
        .unwrap()
}

fn render_markdown(title: &str, md: &str) -> String {
    use pulldown_cmark::{Options, Parser, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut body = String::new();
    html::push_html(&mut body, parser);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title>\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <style>body{{max-width:720px;margin:40px auto;padding:0 24px;\
         font-family:system-ui,sans-serif;line-height:1.5;color:#222}}\
         pre,code{{font-family:ui-monospace,monospace;background:#f5f5f5;padding:2px 4px;border-radius:3px}}\
         pre{{padding:12px;overflow:auto}}\
         h1,h2,h3{{margin-top:1.5em}}\
         img{{max-width:100%}}</style></head>\
         <body><article>{}</article></body></html>",
        html_escape(title),
        body,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn gone(msg: &str) -> Response {
    Response::builder()
        .status(StatusCode::GONE)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(format!(
            "<!doctype html><html><body style=\"font-family:system-ui;padding:40px;text-align:center\"><h1>410 Gone</h1><p>{}</p></body></html>",
            html_escape(msg),
        )))
        .unwrap()
}

fn placeholder(title: &str) -> Response {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::RETRY_AFTER, "30")
        .body(Body::from(format!(
            "<!doctype html><html><body style=\"font-family:system-ui;padding:40px;text-align:center\"><h1>{}</h1><p>This document is still rendering. Try again in a moment.</p></body></html>",
            html_escape(title),
        )))
        .unwrap()
}
