//! GET /p/:token — anonymous read of a doc via a public share link.
//!
//! Also serves the cached board SVG previews referenced by sentinel image
//! tags in the rendered markdown, gated by the same share token.

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use uuid::Uuid;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/p/:token", get(public_doc))
        .route("/p/:token/boards/:id/svg", get(public_board_svg))
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

    let html = render_markdown(&doc.title, &cached.markdown_text, &token);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=60")
        .body(Body::from(html))
        .unwrap()
}

fn render_markdown(title: &str, md: &str, token: &str) -> String {
    use pulldown_cmark::{html, Event, Options, Parser, Tag};
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
            let rewritten = rewrite_board_url(&dest_url, token)
                .map(Into::into)
                .unwrap_or(dest_url);
            Event::Start(Tag::Image {
                link_type,
                dest_url: rewritten,
                title,
                id,
            })
        }
        other => other,
    });
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
        let html = render_markdown("Hello", &md, "tok123");
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
        let html = render_markdown("Hello", md, "tok123");
        assert!(html.contains("https://example.com/cat.png"));
    }
}
