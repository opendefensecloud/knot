//! GET /p/:token — anonymous read of a doc via a public share link.
//!
//! Also serves the cached board SVG previews referenced by sentinel image
//! tags in the rendered markdown, gated by the same share token.

use std::collections::HashMap;

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::Response,
    routing::get,
};
use uuid::Uuid;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/p/:token", get(public_doc))
        .route("/p/:token/boards/:id/svg", get(public_board_svg))
        .route("/p/:token/blobs/:id", get(public_blob))
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

    // Resolve any `knot://doc/<uuid>` references in the markdown to share
    // tokens (when the target doc also has an active share). Build the map
    // up-front because the pulldown event mapper is synchronous.
    let referenced_docs = collect_doc_link_targets(&cached.markdown_text);
    let mut doc_link_map: HashMap<Uuid, String> = HashMap::new();
    if !referenced_docs.is_empty() {
        for id in referenced_docs {
            if let Ok(tokens) = shares.list_active(id).await
                && let Some(t) = tokens.into_iter().next()
            {
                doc_link_map.insert(id, t.token);
            }
        }
    }

    let html = render_markdown(&doc.title, &cached.markdown_text, &token, &doc_link_map);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=60")
        .body(Body::from(html))
        .unwrap()
}

fn render_markdown(
    title: &str,
    md: &str,
    token: &str,
    doc_link_map: &HashMap<Uuid, String>,
) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, html};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts).map(|ev| match ev {
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            // Two URL rewrites overlap on Tag::Image: board sentinels and
            // `/api/blobs/<uuid>` references. Boards take precedence (sentinel
            // URLs never start with `/api/blobs/`, but explicit is clearer).
            let rewritten = rewrite_board_url(&dest_url, token)
                .or_else(|| rewrite_blob_url(&dest_url, token))
                .map(Into::into)
                .unwrap_or(dest_url);
            Event::Start(Tag::Image {
                link_type,
                dest_url: rewritten,
                title,
                id,
            })
        }
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            // Internal doc links: rewrite to another share token if the target
            // is also shared; otherwise leave as a dead `#` anchor so the link
            // text is visible but inert.
            let rewritten = if let Some(target) = parse_doc_link(&dest_url) {
                match doc_link_map.get(&target) {
                    Some(t) => format!("/p/{t}").into(),
                    None => "#".into(),
                }
            } else {
                dest_url
            };
            Event::Start(Tag::Link {
                link_type,
                dest_url: rewritten,
                title,
                id,
            })
        }
        other => other,
    });
    let mut raw = String::new();
    html::push_html(&mut raw, parser);
    // pulldown-cmark emits raw HTML (`<script>`, `<img onerror=...>`) and
    // unsafe link schemes (`javascript:`) verbatim. This page is served to
    // ANONYMOUS visitors, so sanitize before embedding. Relative URLs are the
    // rewritten board/blob links (`/p/<token>/...`) and must pass through.
    let body = ammonia::Builder::default()
        .url_relative(ammonia::UrlRelative::PassThrough)
        .clean(&raw)
        .to_string();
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

/// If `url` matches the sentinel `knot://board/<uuid>.svg`, rewrite it to the
/// token-gated public board SVG route `/p/<token>/boards/<uuid>/svg`. Otherwise
/// return `None`.
fn rewrite_board_url(url: &str, token: &str) -> Option<String> {
    use knot_markdown::{BOARD_URL_PREFIX, BOARD_URL_SUFFIX};
    let rest = url.strip_prefix(BOARD_URL_PREFIX)?;
    let id = rest.strip_suffix(BOARD_URL_SUFFIX)?;
    // Validate it's a real UUID so we don't accept arbitrary paths.
    let id = Uuid::parse_str(id).ok()?;
    // Share tokens are URL-safe by construction (see knot-storage::share_tokens).
    Some(format!("/p/{token}/boards/{id}/svg"))
}

/// If `url` matches the sentinel `knot://doc/<uuid>`, return the target
/// doc id. Otherwise return `None`.
fn parse_doc_link(url: &str) -> Option<Uuid> {
    use knot_markdown::DOC_URL_PREFIX;
    let rest = url.strip_prefix(DOC_URL_PREFIX)?;
    // Strip an optional trailing slash / fragment / query.
    let id_part = rest.split(['?', '#', '/']).next().unwrap_or(rest);
    Uuid::parse_str(id_part).ok()
}

/// Walk the markdown source for `knot://doc/<uuid>` link targets so we can
/// resolve them via the share-token store before rendering. Image URLs that
/// happen to match the sentinel shape are ignored (board sentinels use a
/// different prefix; doc images would not).
fn collect_doc_link_targets(md: &str) -> Vec<Uuid> {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut out: Vec<Uuid> = Vec::new();
    let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for ev in Parser::new_ext(md, opts) {
        if let Event::Start(Tag::Link { dest_url, .. }) = ev
            && let Some(id) = parse_doc_link(&dest_url)
            && seen.insert(id)
        {
            out.push(id);
        }
    }
    out
}

/// If `url` matches `/api/blobs/<uuid>`, rewrite it to the token-gated public
/// blob route `/p/<token>/blobs/<uuid>`. Otherwise return `None`.
fn rewrite_blob_url(url: &str, token: &str) -> Option<String> {
    let rest = url.strip_prefix("/api/blobs/")?;
    // Strip an optional trailing slash or query — keep it simple, validate UUID.
    let id_part = rest.split(['?', '#', '/']).next().unwrap_or(rest);
    let id = Uuid::parse_str(id_part).ok()?;
    Some(format!("/p/{token}/blobs/{id}"))
}

async fn public_blob(
    State(state): State<AppState>,
    Path((token, blob_id)): Path<(String, Uuid)>,
) -> Response {
    let Some(shares) = state.shares.clone() else {
        return gone("internal");
    };
    let Some(blob_meta) = state.blob_meta.clone() else {
        return gone("internal");
    };
    let Some(store) = state.blob_store.clone() else {
        return gone("internal");
    };

    let Some(share) = shares.find_alive(&token).await.ok().flatten() else {
        return not_found();
    };
    let meta = match blob_meta.find(blob_id).await {
        Ok(Some(m)) => m,
        _ => return not_found(),
    };
    // The blob must belong to the shared document.
    if meta.doc_id != share.doc_id {
        return not_found();
    }
    let bytes = match store.get(blob_id).await {
        Ok(b) => b,
        Err(knot_storage::BlobStoreError::NotFound) => return not_found(),
        Err(_) => return gone("internal"),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, meta.content_type)
        .header(header::CACHE_CONTROL, "public, max-age=60")
        .header(header::CONTENT_LENGTH, meta.byte_size)
        .body(Body::from(bytes))
        .unwrap()
}

async fn public_board_svg(
    State(state): State<AppState>,
    Path((token, board_id)): Path<(String, Uuid)>,
) -> Response {
    let Some(shares) = state.shares.clone() else {
        return gone("internal");
    };
    let Some(boards) = state.boards.clone() else {
        return gone("internal");
    };

    let Some(share) = shares.find_alive(&token).await.ok().flatten() else {
        return not_found();
    };
    let board = match boards.get(board_id).await {
        Ok(b) => b,
        Err(_) => return not_found(),
    };
    // The board must belong to the shared document.
    if board.doc_id != share.doc_id {
        return not_found();
    }
    let svg = match boards.get_svg(board_id).await {
        Ok(Some(b)) => b,
        Ok(None) => return not_found(),
        Err(_) => return gone("internal"),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=60")
        // Defense-in-depth: an SVG opened as a top-level document can run
        // embedded <script>. nosniff + a script-free CSP neutralize that even
        // if the stored SVG is malicious.
        .header("X-Content-Type-Options", "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox",
        )
        .body(Body::from(svg))
        .unwrap()
}

fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("not found"))
        .unwrap()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_board_url_matches_sentinel() {
        let id = Uuid::new_v4();
        let url = format!("knot://board/{id}.svg");
        let got = rewrite_board_url(&url, "tok123").unwrap();
        assert_eq!(got, format!("/p/tok123/boards/{id}/svg"));
    }

    #[test]
    fn rewrite_board_url_ignores_non_sentinel() {
        assert!(rewrite_board_url("https://example.com/img.png", "t").is_none());
        assert!(rewrite_board_url("knot://board/not-a-uuid.svg", "t").is_none());
        assert!(rewrite_board_url("knot://board/foo.png", "t").is_none());
    }

    #[test]
    fn render_markdown_rewrites_board_sentinel_images() {
        let id = Uuid::new_v4();
        let md = format!("![Diagram](knot://board/{id}.svg)\n");
        let html = render_markdown("Hello", &md, "tok123", &HashMap::new());
        assert!(
            html.contains(&format!("/p/tok123/boards/{id}/svg")),
            "expected rewritten URL in: {html}"
        );
        assert!(
            !html.contains("knot://board/"),
            "expected sentinel to be removed: {html}"
        );
    }

    #[test]
    fn render_markdown_leaves_normal_images_alone() {
        let md = "![alt](https://example.com/cat.png)\n";
        let html = render_markdown("Hello", md, "tok123", &HashMap::new());
        assert!(html.contains("https://example.com/cat.png"));
    }

    #[test]
    fn render_markdown_strips_raw_html_script() {
        // Stored XSS regression: raw <script> in the doc markdown must not
        // survive to the anonymous share page.
        let md =
            "hello\n\n<script>alert(document.cookie)</script>\n\n<img src=x onerror=alert(1)>\n";
        let html = render_markdown("T", md, "tok", &HashMap::new());
        assert!(!html.contains("<script"), "script tag survived: {html}");
        assert!(!html.contains("onerror"), "event handler survived: {html}");
        assert!(
            !html.contains("alert(1)"),
            "inline handler survived: {html}"
        );
    }

    #[test]
    fn render_markdown_strips_javascript_link_scheme() {
        let md = "[click me](javascript:alert(1))\n";
        let html = render_markdown("T", md, "tok", &HashMap::new());
        assert!(!html.contains("javascript:"), "js scheme survived: {html}");
        // Link text is preserved even though the unsafe href is dropped.
        assert!(html.contains("click me"));
    }

    #[test]
    fn render_markdown_keeps_relative_share_links() {
        // The board/blob rewrites produce relative `/p/<token>/...` URLs that
        // sanitization must NOT strip.
        let id = Uuid::new_v4();
        let md = format!("![diagram](knot://board/{id}.svg)\n");
        let html = render_markdown("T", &md, "tok", &HashMap::new());
        assert!(
            html.contains(&format!("/p/tok/boards/{id}/svg")),
            "relative board link was stripped by sanitizer: {html}"
        );
    }

    #[test]
    fn rewrite_blob_url_matches_api_path() {
        let id = Uuid::new_v4();
        let url = format!("/api/blobs/{id}");
        let got = rewrite_blob_url(&url, "tok").unwrap();
        assert_eq!(got, format!("/p/tok/blobs/{id}"));
    }

    #[test]
    fn rewrite_blob_url_ignores_other_urls() {
        assert!(rewrite_blob_url("https://example.com/img.png", "t").is_none());
        assert!(rewrite_blob_url("/api/docs/123", "t").is_none());
        assert!(rewrite_blob_url("/api/blobs/not-a-uuid", "t").is_none());
    }

    #[test]
    fn render_markdown_rewrites_blob_image_urls() {
        let id = Uuid::new_v4();
        let md = format!("![cat](/api/blobs/{id})\n");
        let html = render_markdown("Hello", &md, "tok", &HashMap::new());
        assert!(
            html.contains(&format!("/p/tok/blobs/{id}")),
            "expected rewritten URL in: {html}"
        );
    }

    #[test]
    fn parse_doc_link_matches_sentinel() {
        let id = Uuid::new_v4();
        let url = format!("knot://doc/{id}");
        assert_eq!(parse_doc_link(&url), Some(id));
    }

    #[test]
    fn parse_doc_link_ignores_other_urls() {
        assert!(parse_doc_link("https://example.com").is_none());
        assert!(parse_doc_link("knot://doc/not-a-uuid").is_none());
        assert!(parse_doc_link("knot://board/aaaa").is_none());
    }

    #[test]
    fn collect_doc_link_targets_dedupes() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let md = format!(
            "[a](knot://doc/{a})\n[a-dup](knot://doc/{a})\n[b](knot://doc/{b})\n[ext](https://example.com)\n"
        );
        let mut got = collect_doc_link_targets(&md);
        got.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn render_markdown_rewrites_internal_link_to_share_when_present() {
        let id = Uuid::new_v4();
        let md = format!("[See doc B](knot://doc/{id})\n");
        let mut map = HashMap::new();
        map.insert(id, "tok-b".to_string());
        let html = render_markdown("A", &md, "tok-a", &map);
        assert!(html.contains("/p/tok-b"), "got: {html}");
        assert!(!html.contains("knot://doc/"));
    }

    #[test]
    fn render_markdown_strips_internal_link_when_target_not_shared() {
        let id = Uuid::new_v4();
        let md = format!("[See doc B](knot://doc/{id})\n");
        let html = render_markdown("A", &md, "tok-a", &HashMap::new());
        // Dead-anchor — link text preserved, href is just "#".
        assert!(html.contains("See doc B"));
        assert!(html.contains("href=\"#\""), "got: {html}");
        assert!(!html.contains("knot://doc/"));
    }
}
