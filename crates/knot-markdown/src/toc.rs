//! The table of contents in Markdown. A `table_of_contents` block is
//! written as GitLab's own marker `[[_TOC_]]` on a line of its own, with
//! `<!-- knot:toc maxLevel=N -->` on the line before it when its depth is
//! not the default 3. In a table cell both sit on the cell's one line.
//!
//! Reading it back, [`mark_toc_markers`] runs before the importer: every
//! paragraph or table cell that holds just the marker (and the comment) is
//! replaced by the sentinel image `knot://toc/N`, which the importer turns
//! into the block the way it turns `knot://board/…` into a board. Working on
//! pulldown-cmark's own events keeps code blocks and inline text out of it,
//! and `_TOC_` is never read as emphasis.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// GitLab's table of contents marker.
pub const TOC_MARKER: &str = "[[_TOC_]]";

/// The depth of a table of contents without one.
pub const TOC_DEFAULT_LEVEL: u8 = 3;

/// The importer's sentinel image for a table of contents of depth N.
pub const TOC_URL_PREFIX: &str = "knot://toc/";

/// `raw` (an attribute: `"2"`, or `2` written by the editor) as a depth
/// 1–6; anything else is the default.
pub fn toc_level(raw: Option<&str>) -> u8 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.fract() == 0.0 && (1.0..=6.0).contains(n))
        .map_or(TOC_DEFAULT_LEVEL, |n| n as u8)
}

/// The comment written before a table of contents of depth `level`, or
/// nothing for the default depth.
pub fn toc_comment(level: u8) -> Option<String> {
    (level != TOC_DEFAULT_LEVEL).then(|| format!("<!-- knot:toc maxLevel={level} -->"))
}

/// The depth a `<!-- knot:toc maxLevel=N -->` comment names (the default
/// when N is not 1–6), or `None` when `html` is not such a comment.
pub fn toc_comment_level(html: &str) -> Option<u8> {
    let inner = html
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim()
        .strip_prefix("knot:toc")?
        .trim();
    let value = inner.strip_prefix("maxLevel=").unwrap_or("");
    Some(toc_level(Some(value)))
}

/// The depth of the sentinel image `url`, or `None` when it is not one.
pub fn toc_sentinel_level(url: &str) -> Option<u8> {
    let rest = url.strip_prefix(TOC_URL_PREFIX)?;
    let level: u8 = rest.parse().ok()?;
    (1..=6).contains(&level).then_some(level)
}

fn sentinel(level: u8) -> String {
    format!("![]({TOC_URL_PREFIX}{level})")
}

/// `src` with each table of contents marker replaced by its sentinel image
/// (see the module doc); `src` itself when there is none.
pub fn mark_toc_markers(src: &str) -> Cow<'_, str> {
    if !src.contains(TOC_MARKER) {
        return Cow::Borrowed(src);
    }
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    // The depth of a `knot:toc` comment block just before, until the next
    // block starts.
    let mut comment_level: Option<u8> = None;
    for (event, range) in Parser::new_ext(src, opts).into_offset_iter() {
        match event {
            Event::Html(html) => {
                if let Some(level) = toc_comment_level(&html) {
                    comment_level = Some(level);
                }
            }
            Event::Start(Tag::HtmlBlock) | Event::End(TagEnd::HtmlBlock) => {}
            Event::Start(Tag::Paragraph) => {
                let level = comment_level.take().unwrap_or(TOC_DEFAULT_LEVEL);
                if let Some(at) = marker_only(src, range.clone()) {
                    edits.push((at, sentinel(level)));
                }
            }
            Event::Start(Tag::TableCell) => {
                comment_level = None;
                if let Some((at, level)) = cell_marker(src, range.clone()) {
                    edits.push((at, sentinel(level)));
                }
            }
            Event::Start(_) => comment_level = None,
            _ => {}
        }
    }
    if edits.is_empty() {
        return Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len());
    let mut at = 0;
    for (range, with) in edits {
        if range.start < at {
            continue;
        }
        out.push_str(&src[at..range.start]);
        out.push_str(&with);
        at = range.end;
    }
    out.push_str(&src[at..]);
    Cow::Owned(out)
}

/// The range of the marker when `range` of `src` holds nothing else.
fn marker_only(src: &str, range: Range<usize>) -> Option<Range<usize>> {
    let slice = src.get(range.clone())?;
    let lead = slice.len() - slice.trim_start().len();
    (slice.trim() == TOC_MARKER).then(|| {
        let start = range.start + lead;
        start..start + TOC_MARKER.len()
    })
}

/// The range to replace and the depth of a marker (maybe after a
/// `knot:toc` comment) at the start or the end of table cell `range`, set
/// off from the cell's other text by whitespace: the serializer joins a
/// cell's blocks onto its one line with a space.
fn cell_marker(src: &str, range: Range<usize>) -> Option<(Range<usize>, u8)> {
    let slice = src.get(range.clone())?;
    let trimmed = slice.trim();
    let start = range.start + (slice.len() - slice.trim_start().len());
    // At the start: `[comment] marker`, then the end or whitespace.
    if let Some((len, level)) = leading_marker(trimmed) {
        return Some((start..start + len, level));
    }
    // At the end: whitespace or the start, then `[comment] marker`.
    let body = trimmed.strip_suffix(TOC_MARKER)?;
    let before = body.trim_end();
    if !before.is_empty() && before.len() == body.len() {
        return None;
    }
    let (at, level) = match before.rfind("<!--") {
        Some(c) if before.ends_with("-->") => match toc_comment_level(&before[c..]) {
            Some(level) if c == 0 || before[..c].ends_with(char::is_whitespace) => (c, level),
            _ => (body.len(), TOC_DEFAULT_LEVEL),
        },
        _ => (body.len(), TOC_DEFAULT_LEVEL),
    };
    if at > 0 && !trimmed[..at].ends_with(char::is_whitespace) {
        return None;
    }
    Some((start + at..start + trimmed.len(), level))
}

/// The length and depth when `text` starts with a marker (maybe after a
/// `knot:toc` comment) followed by the end or whitespace.
fn leading_marker(text: &str) -> Option<(usize, u8)> {
    let (rest, level, used) = if text.starts_with("<!--") {
        let end = text.find("-->")? + 3;
        let level = toc_comment_level(&text[..end])?;
        let after = &text[end..];
        let gap = after.len() - after.trim_start().len();
        (&after[gap..], level, end + gap)
    } else {
        (text, TOC_DEFAULT_LEVEL, 0)
    };
    let after = rest.strip_prefix(TOC_MARKER)?;
    (after.is_empty() || after.starts_with(char::is_whitespace))
        .then_some((used + TOC_MARKER.len(), level))
}
