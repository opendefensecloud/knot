//! GET /p/{token} — anonymous read of a doc via a public share link.
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
use knot_storage::ShareToken;
use uuid::Uuid;

use crate::AppState;
use crate::routes::api::blobs::safe_inline_content_type;

/// `Cache-Control` of every successful `/p/*` response. A link stops working
/// when it is revoked, expires, or its doc moves into a restricted subtree,
/// so no shared cache may keep a copy, and browsers revalidate each time.
const PUBLIC_CACHE_CONTROL: &str = "private, no-cache";

/// What a dead page link says. A page link whose doc is in a restricted
/// subtree says the same (see [`alive_share`]).
const DEAD_LINK_MESSAGE: &str = "This link has expired or been revoked.";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/p/{token}", get(public_doc))
        .route("/p/{token}/boards/{id}/svg", get(public_board_svg))
        .route("/p/{token}/blobs/{id}", get(public_blob))
}

/// The alive share behind `token`, or the response refusing it: the
/// `dead_link` response when the token is unknown, revoked, expired or its
/// doc archived, and when its doc is in a restricted subtree. Each route
/// thus answers a restricted doc exactly like a dead link, so it doesn't
/// reveal that the doc still exists. The restriction is checked on every
/// request rather than only when the link is created, because a shared doc
/// can be moved into a restricted subtree.
async fn alive_share(
    state: &AppState,
    token: &str,
    dead_link: fn() -> Response,
) -> Result<ShareToken, Response> {
    let Some(shares) = state.shares.clone() else {
        return Err(gone("internal"));
    };
    let Some(docs) = state.docs.clone() else {
        return Err(gone("internal"));
    };
    let Some(share) = shares.find_alive(token).await.ok().flatten() else {
        return Err(dead_link());
    };
    match docs
        .restriction_root(share.workspace_id, share.doc_id)
        .await
    {
        Ok(None) => Ok(share),
        Ok(Some(_)) => Err(dead_link()),
        Err(_) => Err(gone("internal")),
    }
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

    let share = match alive_share(&state, &token, || gone(DEAD_LINK_MESSAGE)).await {
        Ok(share) => share,
        Err(refusal) => return refusal,
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
    // up-front because the pulldown event mapper is synchronous. A target in
    // a restricted subtree is left out, so its link renders as `#`: its
    // share (made before it was moved in) is refused anyway, and the token
    // must not leak. A failed check leaves it out too.
    let referenced_docs = collect_doc_link_targets(&cached.markdown_text);
    let mut doc_link_map: HashMap<Uuid, String> = HashMap::new();
    if !referenced_docs.is_empty() {
        for id in referenced_docs {
            if let Ok(tokens) = shares.list_active(id).await
                && let Some(t) = tokens.into_iter().next()
                && let Ok(None) = docs.restriction_root(t.workspace_id, t.doc_id).await
            {
                doc_link_map.insert(id, t.token);
            }
        }
    }

    let html = render_markdown(&doc.title, &cached.markdown_text, &token, &doc_link_map);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, PUBLIC_CACHE_CONTROL)
        .body(Body::from(html))
        .unwrap()
}

/// The classes of a table of contents entry, by its level below the
/// shallowest heading shown (deeper ones stay at 4).
const TOC_LEVEL_CLASSES: [&str; 5] = [
    "knot-box__l0",
    "knot-box__l1",
    "knot-box__l2",
    "knot-box__l3",
    "knot-box__l4",
];

/// A heading of the page: its level, text and anchor id.
struct HeadingRef {
    level: u8,
    text: String,
    slug: String,
}

/// Put before every heading slug, as the editor does (`headingAnchors.ts`):
/// keeps heading ids clear of other ids on the page.
const HEADING_ID_PREFIX: &str = "h-";

/// The headings of `events`, in order, with the ids the editor gives them
/// (`h-` and the slug of `knot_markdown::slug`).
fn collect_headings(events: &[pulldown_cmark::Event<'_>]) -> Vec<HeadingRef> {
    use pulldown_cmark::{Event, Tag, TagEnd};
    let mut slugger = knot_markdown::slug::Slugger::default();
    let mut out = Vec::new();
    let mut open: Option<(u8, String)> = None;
    for event in events {
        match event {
            Event::Start(Tag::Heading { level, .. }) => open = Some((*level as u8, String::new())),
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, text)) = open.as_mut() {
                    text.push_str(t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text)) = open.take() {
                    let slug = format!("{HEADING_ID_PREFIX}{}", slugger.slug(&text));
                    out.push(HeadingRef { level, text, slug });
                }
            }
            _ => {}
        }
    }
    out
}

/// The table of contents box for `headings` down to `max_level`, which
/// `index` sorts by depth. It lists them while they fit in what is left of
/// the page's `budget`, taking them from it, and then says
/// [`knot_export::TOC_TOO_MANY_HEADINGS`].
fn toc_html(
    headings: &[HeadingRef],
    index: &knot_export::TocIndex,
    max_level: u8,
    budget: &mut knot_export::TocBudget,
) -> String {
    let shown = index.shown(max_level);
    let mut html = String::from(
        r#"<nav class="knot-box" aria-label="Contents"><p class="knot-box__title">Contents</p>"#,
    );
    if shown.is_empty() {
        html.push_str("<p>No headings yet</p>");
    } else {
        let listed = budget.take(index, max_level);
        if listed > 0 {
            let top = shown.iter().map(|&i| headings[i].level).min().unwrap_or(1);
            html.push_str(r#"<ul class="knot-box__list">"#);
            for h in shown[..listed].iter().map(|&i| &headings[i]) {
                let class = TOC_LEVEL_CLASSES[usize::from((h.level - top).min(4))];
                html.push_str(&toc_entry_html(class, h));
            }
            html.push_str("</ul>");
        }
        if listed < shown.len() {
            html.push_str(&format!("<p>{}</p>", knot_export::TOC_TOO_MANY_HEADINGS));
        }
    }
    html.push_str("</nav>");
    html
}

/// The line of a table of contents that links to `h`.
fn toc_entry_html(class: &str, h: &HeadingRef) -> String {
    format!(
        r##"<li class="{class}"><a href="#{}">{}</a></li>"##,
        h.slug,
        html_escape(&h.text)
    )
}

/// What [`toc_entry_html`] writes for `h`, counted without writing it, for
/// the page's [`knot_export::TocBudget`]. Every level class is as long.
fn toc_entry_bytes(h: &HeadingRef) -> usize {
    const MARKUP: usize = r##"<li class=""><a href="#"></a></li>"##.len();
    const CLASS: usize = TOC_LEVEL_CLASSES[0].len();
    const _: () = {
        let mut i = 1;
        while i < TOC_LEVEL_CLASSES.len() {
            assert!(TOC_LEVEL_CLASSES[i].len() == CLASS);
            i += 1;
        }
    };
    MARKUP + CLASS + h.slug.len() + html_escape_len(&h.text)
}

/// The callout box classes the sanitizer lets through: the box, one per
/// kind, and the title line.
const CALLOUT_CLASSES: [&str; 6] = [
    "knot-callout",
    "knot-callout--note",
    "knot-callout--tip",
    "knot-callout--important",
    "knot-callout--warning",
    "knot-callout--caution",
];

/// `events` with every alert blockquote replaced by a callout box: a
/// `div.knot-callout` whose first line is its title (from `titles`, in
/// order, or the kind's label), escaped.
fn with_callouts(
    events: Vec<pulldown_cmark::Event<'_>>,
    titles: Vec<Option<String>>,
) -> Vec<pulldown_cmark::Event<'_>> {
    use knot_markdown::callout::CalloutKind;
    use pulldown_cmark::{BlockQuoteKind, Event, Tag, TagEnd};
    let mut titles = titles.into_iter();
    let kind_of = |k: BlockQuoteKind| match k {
        BlockQuoteKind::Note => CalloutKind::Note,
        BlockQuoteKind::Tip => CalloutKind::Tip,
        BlockQuoteKind::Important => CalloutKind::Important,
        BlockQuoteKind::Warning => CalloutKind::Warning,
        BlockQuoteKind::Caution => CalloutKind::Caution,
    };
    events
        .into_iter()
        .map(|event| match event {
            Event::Start(Tag::BlockQuote(Some(k))) => {
                let kind = kind_of(k);
                let title = titles.next().flatten();
                Event::Html(
                    format!(
                        r#"<div class="knot-callout knot-callout--{}"><p class="knot-callout__title">{}</p>"#,
                        kind.as_str(),
                        html_escape(title.as_deref().unwrap_or(kind.label()))
                    )
                    .into(),
                )
            }
            Event::End(TagEnd::BlockQuote(Some(_))) => Event::Html("</div>".into()),
            other => other,
        })
        .collect()
}

/// The depth of the table of contents sentinel image starting at
/// `events[i]` (`Start(Image)` then `End(Image)`), if it is one.
fn toc_sentinel_at(events: &[pulldown_cmark::Event<'_>], i: usize) -> Option<u8> {
    use pulldown_cmark::{Event, Tag, TagEnd};
    match (events.get(i), events.get(i + 1)) {
        (Some(Event::Start(Tag::Image { dest_url, .. })), Some(Event::End(TagEnd::Image))) => {
            knot_markdown::toc::toc_sentinel_level(dest_url)
        }
        _ => None,
    }
}

/// `events` with ids on the headings and every table of contents sentinel
/// (alone in a paragraph, or in a table cell) replaced by its box, all
/// boxes together within one [`knot_export::TocBudget`].
fn with_headings(events: Vec<pulldown_cmark::Event<'_>>) -> Vec<pulldown_cmark::Event<'_>> {
    use pulldown_cmark::{Event, Tag, TagEnd};
    let headings = collect_headings(&events);
    let mut next_heading = headings.iter();
    // Built at the first box, so a page without one never pays for it.
    let mut index: Option<knot_export::TocIndex> = None;
    let index_of = |headings: &[HeadingRef]| {
        // Empty headings get an id but no line in a table of contents.
        knot_export::TocIndex::new(headings.iter().map(|h| {
            let level = if h.text.trim().is_empty() { 0 } else { h.level };
            (level, toc_entry_bytes(h))
        }))
    };
    let mut budget = knot_export::TocBudget::default();
    let mut out = Vec::with_capacity(events.len());
    let mut i = 0;
    while i < events.len() {
        if matches!(events[i], Event::Start(Tag::Paragraph))
            && let Some(level) = toc_sentinel_at(&events, i + 1)
            && matches!(events.get(i + 3), Some(Event::End(TagEnd::Paragraph)))
        {
            let index = index.get_or_insert_with(|| index_of(&headings));
            out.push(Event::Html(
                toc_html(&headings, index, level, &mut budget).into(),
            ));
            i += 4;
            continue;
        }
        if let Some(level) = toc_sentinel_at(&events, i) {
            let index = index.get_or_insert_with(|| index_of(&headings));
            out.push(Event::InlineHtml(
                toc_html(&headings, index, level, &mut budget).into(),
            ));
            i += 2;
            continue;
        }
        let event = match &events[i] {
            Event::Start(Tag::Heading {
                level,
                classes,
                attrs,
                ..
            }) => Event::Start(Tag::Heading {
                level: *level,
                id: next_heading.next().map(|h| h.slug.clone().into()),
                classes: classes.clone(),
                attrs: attrs.clone(),
            }),
            other => other.clone(),
        };
        out.push(event);
        i += 1;
    }
    out
}

fn render_markdown(
    title: &str,
    md: &str,
    token: &str,
    doc_link_map: &HashMap<Uuid, String>,
) -> String {
    use pulldown_cmark::{Event, Parser, Tag, html};
    // The importer's options, so alerts read as callout boxes here too.
    let opts = knot_markdown::callout::parser_options();
    // Table of contents markers become sentinel images, the same pre-pass
    // the importer uses; `with_headings` turns them into the box and gives
    // every heading the id the editor gives it.
    let marked = knot_markdown::toc::mark_toc_markers(md);
    // A titled callout marker becomes a bare alert, its title kept aside.
    let callouts = knot_markdown::callout::mark_callouts(&marked);
    let events: Vec<Event<'_>> = Parser::new_ext(&callouts.src, opts).collect();
    let events = with_callouts(events, callouts.titles);
    let parser = with_headings(events).into_iter().map(|ev| match ev {
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
    // On top of the defaults: the heading ids and the table of contents
    // box's own classes, nothing else (see `with_headings`).
    let mut sanitizer = ammonia::Builder::default();
    sanitizer.url_relative(ammonia::UrlRelative::PassThrough);
    for heading in ["h1", "h2", "h3", "h4", "h5", "h6"] {
        sanitizer.add_tag_attributes(heading, &["id"]);
    }
    sanitizer
        .add_tag_attributes("nav", &["aria-label"])
        .add_allowed_classes("nav", &["knot-box"])
        .add_allowed_classes("p", &["knot-box__title"])
        .add_allowed_classes("ul", &["knot-box__list"])
        .add_allowed_classes("li", &TOC_LEVEL_CLASSES)
        .add_allowed_classes("div", &CALLOUT_CLASSES)
        .add_allowed_classes("p", &["knot-callout__title"]);
    let body = sanitizer.clean(&raw).to_string();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title>\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\
         <style>body{{max-width:720px;margin:40px auto;padding:0 24px;\
         font-family:system-ui,sans-serif;line-height:1.5;color:#222}}\
         pre,code{{font-family:ui-monospace,monospace;background:#f5f5f5;padding:2px 4px;border-radius:3px}}\
         pre{{padding:12px;overflow:auto}}\
         h1,h2,h3{{margin-top:1.5em}}\
         img{{max-width:100%}}\
.knot-box{{background:#f4f5f7;border:1px solid #d9dde3;border-radius:6px;padding:12px 16px;margin:1em 0}}\
.knot-box__title{{margin:0 0 6px;font-size:.85em;font-weight:600;color:#666}}\
.knot-box__list{{list-style:none;margin:0;padding:0}}\
.knot-box__l1{{padding-left:1em}}.knot-box__l2{{padding-left:2em}}\
.knot-box__l3{{padding-left:3em}}.knot-box__l4{{padding-left:4em}}\
.knot-callout{{--h:#0969DA;border:1px solid color-mix(in oklab,var(--h) 45%,#fff);border-radius:6px;margin:1em 0;overflow:hidden}}\
.knot-callout--tip{{--h:#1A7F37}}.knot-callout--important{{--h:#8250DF}}\
.knot-callout--warning{{--h:#9A6700}}.knot-callout--caution{{--h:#CF222E}}\
.knot-callout__title{{margin:0;padding:6px 12px;font-weight:600;font-size:.9em;\
background:color-mix(in oklab,var(--h) 14%,#fff);color:color-mix(in oklab,var(--h) 80%,#222)}}\
.knot-callout>:not(.knot-callout__title){{margin-left:12px;margin-right:12px}}</style></head>\
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
    let Some(blob_meta) = state.blob_meta.clone() else {
        return gone("internal");
    };
    let Some(store) = state.blob_store.clone() else {
        return gone("internal");
    };

    let share = match alive_share(&state, &token, not_found).await {
        Ok(share) => share,
        Err(refusal) => return refusal,
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
    let (ct, disposition) = match safe_inline_content_type(&meta.content_type) {
        Some(ct) => (ct, "inline"),
        None => ("application/octet-stream", "attachment"),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CACHE_CONTROL, PUBLIC_CACHE_CONTROL)
        .header(header::CONTENT_LENGTH, meta.byte_size)
        .body(Body::from(bytes))
        .unwrap()
}

async fn public_board_svg(
    State(state): State<AppState>,
    Path((token, board_id)): Path<(String, Uuid)>,
) -> Response {
    let Some(boards) = state.boards.clone() else {
        return gone("internal");
    };

    let share = match alive_share(&state, &token, not_found).await {
        Ok(share) => share,
        Err(refusal) => return refusal,
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
        .header(header::CACHE_CONTROL, PUBLIC_CACHE_CONTROL)
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

/// Bytes [`html_escape`] makes of `s`, without making them.
fn html_escape_len(s: &str) -> usize {
    s.bytes()
        .map(|b| match b {
            b'&' => "&amp;".len(),
            b'<' | b'>' => "&lt;".len(),
            b'"' => "&quot;".len(),
            _ => 1,
        })
        .sum()
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
    fn render_markdown_links_the_favicon() {
        let html = render_markdown("Shared doc", "hello", "tok", &HashMap::new());
        // A shared link is often someone's first sight of knot; the tab needs
        // the same mark the SPA shows rather than a blank placeholder.
        assert!(
            html.contains("<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">"),
            "public doc head should link the favicon, got: {html}"
        );
    }

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

    fn render(md: &str) -> String {
        render_markdown("T", md, "tok", &HashMap::new())
    }

    #[test]
    fn headings_get_slug_ids() {
        let html = render("# Plan\n\n## Tasks\n\n## Tasks\n");
        assert!(html.contains("<h1 id=\"h-plan\">Plan</h1>"), "{html}");
        assert!(html.contains("<h2 id=\"h-tasks\">Tasks</h2>"), "{html}");
        assert!(html.contains("<h2 id=\"h-tasks-1\">Tasks</h2>"), "{html}");
    }

    #[test]
    fn expands_toc_marker_into_box() {
        let html = render("[[_TOC_]]\n\n# Plan\n\n## Tasks\n\n### Details\n");
        assert!(html.contains("class=\"knot-box\""), "{html}");
        assert!(html.contains("class=\"knot-box__title\""), "{html}");
        for slug in ["h-plan", "h-tasks", "h-details"] {
            assert!(
                html.contains(&format!("href=\"#{slug}\"")),
                "{slug} in {html}"
            );
        }
        assert!(!html.contains("[[_TOC_]]"), "{html}");
        assert!(!html.contains("knot://toc"), "{html}");
        // The box is not inside a paragraph.
        assert!(!html.contains("<p><nav"), "{html}");
    }

    #[test]
    fn renders_callout_box_with_label() {
        let html = render("> [!WARNING]\n> x\n");
        assert!(
            html.contains("<div class=\"knot-callout knot-callout--warning\">"),
            "{html}"
        );
        assert!(
            html.contains("<p class=\"knot-callout__title\">Warning</p>"),
            "{html}"
        );
        assert!(html.contains(">x<"), "{html}");
        assert!(!html.contains("<blockquote"), "{html}");
    }

    #[test]
    fn renders_custom_title() {
        let html = render("> [!tip] Wartung am Freitag\n> x\n");
        assert!(html.contains("knot-callout--tip"), "{html}");
        assert!(
            html.contains("<p class=\"knot-callout__title\">Wartung am Freitag</p>"),
            "{html}"
        );
    }

    #[test]
    fn a_marker_pulldown_does_not_read_gives_no_title_to_the_next_box() {
        let html = render(">  [!note] Lost\n> a\n\n> [!WARNING]\n> b\n");
        assert!(
            html.contains("<p class=\"knot-callout__title\">Warning</p>"),
            "{html}"
        );
        assert!(html.contains("[!note] Lost"), "{html}");
    }

    #[test]
    fn callout_title_is_escaped() {
        let html = render("> [!note] <img src=x onerror=alert(1)> & \\*x\\*\n> body\n");
        assert!(!html.contains("<img"), "{html}");
        assert!(
            html.contains(
                "<p class=\"knot-callout__title\">&lt;img src=x onerror=alert(1)&gt; &amp; *x*</p>"
            ),
            "{html}"
        );
        assert!(html.contains(">body<"), "{html}");
    }

    #[test]
    fn plain_quote_stays_blockquote() {
        let html = render("> just a quote\n");
        assert!(html.contains("<blockquote>"), "{html}");
        assert!(!html.contains("class=\"knot-callout"), "{html}");
    }

    #[test]
    fn raw_html_cannot_forge_callout_classes_beyond_allowed() {
        let html = render(
            "<div class=\"knot-callout evil\" onclick=\"alert(1)\" style=\"x\">hi</div>\n\n<p class=\"knot-callout__title other\">t</p>\n",
        );
        assert!(!html.contains("evil"), "{html}");
        assert!(!html.contains("other"), "{html}");
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("style=\"x\""), "{html}");
    }

    #[test]
    fn toc_box_is_a_navigation_landmark() {
        for md in [
            "[[_TOC_]]\n\n# Plan\n",
            "| [[_TOC_]] | b |\n| --- | --- |\n| c | d |\n\n# Plan\n",
        ] {
            let html = render(md);
            assert!(
                html.contains("<nav class=\"knot-box\" aria-label=\"Contents\">"),
                "{html}"
            );
        }
    }

    #[test]
    fn toc_respects_max_level_comment() {
        let html = render("<!-- knot:toc maxLevel=1 -->\n[[_TOC_]]\n\n# Plan\n\n## Tasks\n");
        assert!(html.contains("href=\"#h-plan\""), "{html}");
        assert!(!html.contains("href=\"#h-tasks\""), "{html}");
    }

    #[test]
    fn tables_of_contents_stop_listing_at_the_page_entry_budget() {
        use knot_export::{MAX_PAGE_TOC_ENTRIES, TOC_TOO_MANY_HEADINGS};
        // Each box lists every heading, so two boxes fit the budget and the
        // third lists what is left, one short, and then the notice.
        let headings = MAX_PAGE_TOC_ENTRIES / 3 + 1;
        let mut md = String::from("[[_TOC_]]\n\n[[_TOC_]]\n\n[[_TOC_]]\n\n");
        for i in 0..headings {
            md.push_str(&format!("## H{i}\n\n"));
        }
        let html = render(&md);
        assert_eq!(
            html.matches("href=\"#h-h").count(),
            MAX_PAGE_TOC_ENTRIES,
            "entries"
        );
        assert_eq!(html.matches(TOC_TOO_MANY_HEADINGS).count(), 1, "notice");
    }

    #[test]
    fn one_box_past_the_entry_budget_lists_what_fits_then_says_so() {
        use knot_export::{MAX_PAGE_TOC_ENTRIES, TOC_TOO_MANY_HEADINGS};
        let mut md = String::from("[[_TOC_]]\n\n");
        for i in 0..=MAX_PAGE_TOC_ENTRIES {
            md.push_str(&format!("## H{i}\n\n"));
        }
        let html = render(&md);
        assert_eq!(
            html.matches("href=\"#h-h").count(),
            MAX_PAGE_TOC_ENTRIES,
            "entries"
        );
        assert!(
            html.contains(&format!("</ul><p>{TOC_TOO_MANY_HEADINGS}</p></nav>")),
            "notice after the list"
        );
    }

    #[test]
    fn the_page_byte_budget_counts_escaped_text_not_raw_text() {
        use knot_export::{MAX_PAGE_TOC_BYTES, TOC_TOO_MANY_HEADINGS};
        // An eighth of the budget in `&`, which each escape to five bytes:
        // the first box writes more than half the budget, so a second one
        // finds too little left.
        let amps = "&".repeat(MAX_PAGE_TOC_BYTES / 8);
        let html = render(&format!("[[_TOC_]]\n\n[[_TOC_]]\n\n# {amps}\n"));
        assert_eq!(html.matches("href=\"#h-section\"").count(), 1, "entries");
        assert_eq!(html.matches(TOC_TOO_MANY_HEADINGS).count(), 1, "notice");
    }

    #[test]
    fn toc_entry_bytes_is_what_the_entry_writes() {
        let mut slugger = knot_markdown::slug::Slugger::default();
        for text in ["Plan", "a \"quoted\" <b> & c", "\u{FDFA}\u{FDFA}", ""] {
            let h = HeadingRef {
                level: 2,
                text: text.into(),
                slug: format!("{HEADING_ID_PREFIX}{}", slugger.slug(text)),
            };
            for class in TOC_LEVEL_CLASSES {
                assert_eq!(
                    toc_entry_bytes(&h),
                    toc_entry_html(class, &h).len(),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn toc_leaves_out_empty_headings_but_keeps_their_ids() {
        let html = render("#\n\n# Plan\n\n[[_TOC_]]\n");
        assert_eq!(html.matches("href=\"#h-").count(), 1, "{html}");
        assert!(html.contains("href=\"#h-plan\""), "{html}");
        assert!(html.contains("<h1 id=\"h-section\"></h1>"), "{html}");
    }

    #[test]
    fn a_box_without_headings_says_so_even_past_the_budget() {
        use knot_export::{MAX_PAGE_TOC_ENTRIES, TOC_TOO_MANY_HEADINGS};
        // The first box takes the whole budget; a depth-1 box on a page of
        // only H2 headings has nothing to list, which is not too many.
        let mut md = String::from("<!-- knot:toc maxLevel=1 -->\n[[_TOC_]]\n\n[[_TOC_]]\n\n");
        md.insert_str(0, "[[_TOC_]]\n\n");
        for i in 0..MAX_PAGE_TOC_ENTRIES {
            md.push_str(&format!("## H{i}\n\n"));
        }
        let html = render(&md);
        assert_eq!(html.matches("No headings yet").count(), 1, "empty box");
        assert_eq!(html.matches(TOC_TOO_MANY_HEADINGS).count(), 1, "notice");
    }

    #[test]
    fn expands_toc_in_a_table_cell() {
        let html = render("| [[_TOC_]] | b |\n| --- | --- |\n| c | d |\n\n# Plan\n");
        assert!(html.contains("class=\"knot-box\""), "{html}");
        assert!(html.contains("href=\"#h-plan\""), "{html}");
    }

    #[test]
    fn public_toc_keeps_html_sanitised() {
        let md = "[[_TOC_]]\n\n<div class=\"evil\" id=\"x\" onclick=\"alert(1)\">hi</div>\n\n<nav class=\"knot-box evil\" onclick=\"alert(3)\" style=\"x\">n</nav>\n\n<script>alert(2)</script>\n\n# Plan\n";
        let html = render(md);
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("evil"), "{html}");
        assert!(!html.contains("id=\"x\""), "{html}");
        assert!(!html.contains("style="), "{html}");
        assert!(html.contains("<h1 id=\"h-plan\">"), "{html}");
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
