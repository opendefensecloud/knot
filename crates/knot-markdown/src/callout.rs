//! Callout boxes: a `blockquote` with a `kind` (one of GitHub's and
//! GitLab's five alert kinds) and an optional plain-text `title`.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag};
use yrs::{ReadTxn, Xml, XmlElementRef};

/// The five kinds of callout box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalloutKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl CalloutKind {
    /// All kinds, in GitHub's order.
    pub const ALL: [CalloutKind; 5] = [
        CalloutKind::Note,
        CalloutKind::Tip,
        CalloutKind::Important,
        CalloutKind::Warning,
        CalloutKind::Caution,
    ];

    /// The kind named `raw`, in any case; `None` for anything else.
    pub fn parse(raw: &str) -> Option<CalloutKind> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(raw.trim()))
    }

    /// The name stored in the document and written in Markdown (lower case).
    pub fn as_str(self) -> &'static str {
        match self {
            CalloutKind::Note => "note",
            CalloutKind::Tip => "tip",
            CalloutKind::Important => "important",
            CalloutKind::Warning => "warning",
            CalloutKind::Caution => "caution",
        }
    }

    /// The name shown when a box has no title of its own.
    pub fn label(self) -> &'static str {
        match self {
            CalloutKind::Note => "Note",
            CalloutKind::Tip => "Tip",
            CalloutKind::Important => "Important",
            CalloutKind::Warning => "Warning",
            CalloutKind::Caution => "Caution",
        }
    }
}

/// `raw` as a title: one line, trimmed; `None` when nothing is left.
pub fn normalise_title(raw: &str) -> Option<String> {
    let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!line.is_empty()).then_some(line)
}

/// The kind and title of `el` when it is a callout box (a blockquote with
/// a known `kind`); `None` for a plain quote or any other element.
pub fn callout_of<T: ReadTxn>(
    txn: &T,
    el: &XmlElementRef,
) -> Option<(CalloutKind, Option<String>)> {
    if el.tag().as_ref() != "blockquote" {
        return None;
    }
    let kind = CalloutKind::parse(&el.get_attribute(txn, "kind")?.to_string(txn))?;
    let title = el
        .get_attribute(txn, "title")
        .and_then(|t| normalise_title(&t.to_string(txn)));
    Some((kind, title))
}

/// The first line of a box in Markdown, after the quote's `> `: GitHub's
/// `[!WARNING]` without a title, GitLab's `[!warning] Title` with one. The
/// title is escaped so that it reads back as the same plain text.
pub fn marker_line(kind: CalloutKind, title: Option<&str>) -> String {
    match title {
        None => format!("[!{}]", kind.as_str().to_ascii_uppercase()),
        Some(t) => format!("[!{}] {}", kind.as_str(), escape_title(t)),
    }
}

/// Characters that could start Markdown inside a title line.
const TITLE_SPECIALS: &[char] = &[
    '\\', '*', '_', '`', '[', ']', '<', '>', '&', '!', '~', '#', '|',
];

fn escape_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    for c in title.chars() {
        if TITLE_SPECIALS.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `escaped` with every backslash before ASCII punctuation removed.
fn unescape_title(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut chars = escaped.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(char::is_ascii_punctuation) {
            out.push(chars.next().unwrap_or(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// Markdown made ready for the importer, and the titles of its boxes.
pub struct CalloutMarks<'a> {
    /// The source, with every titled marker line `[!kind] Title` turned
    /// into the bare `[!KIND]` that pulldown-cmark reads as an alert.
    pub src: Cow<'a, str>,
    /// One entry per alert blockquote pulldown-cmark will report
    /// (`Tag::BlockQuote(Some(_))`), in document order: its title, if any.
    pub titles: Vec<Option<String>>,
}

/// The parser options the importer and the public page read Markdown with.
pub fn parser_options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_GFM);
    opts
}

/// Finds the boxes of `src` with pulldown-cmark's own events (so code
/// blocks and inline text are left alone) and prepares it for import: see
/// [`CalloutMarks`].
pub fn mark_callouts(src: &str) -> CalloutMarks<'_> {
    // Each titled marker: the blockquote's start (its `>`), the marker's
    // range, the bare marker that replaces it, and the title.
    let mut edits: Vec<(usize, Range<usize>, String, String)> = Vec::new();
    for (event, range) in Parser::new_ext(src, parser_options()).into_offset_iter() {
        if let Event::Start(Tag::BlockQuote(None)) = event
            && let Some((at, kind, title)) = titled_marker(src, range.start)
        {
            let bare = format!("[!{}]", kind.as_str().to_ascii_uppercase());
            edits.push((range.start, at, bare, title));
        }
    }
    // The titles go to the alerts pulldown-cmark reports in the rewritten
    // source, matched by where each starts; an edit that does not come out
    // as an alert (a case the line check above misjudged) is dropped, so a
    // title can never land on another box.
    loop {
        let (out, starts) = apply(src, &edits);
        let mut titles = Vec::new();
        let mut alerts = std::collections::HashSet::new();
        for (event, range) in Parser::new_ext(&out, parser_options()).into_offset_iter() {
            if let Event::Start(Tag::BlockQuote(Some(_))) = event {
                alerts.insert(range.start);
                titles.push(
                    starts
                        .iter()
                        .position(|&s| s == range.start)
                        .map(|i| edits[i].3.clone()),
                );
            }
        }
        let before = edits.len();
        let mut i = 0;
        edits.retain(|_| {
            let keep = alerts.contains(&starts[i]);
            i += 1;
            keep
        });
        if edits.len() == before {
            let src = if edits.is_empty() {
                Cow::Borrowed(src)
            } else {
                Cow::Owned(out)
            };
            return CalloutMarks { src, titles };
        }
    }
}

/// `src` with each edit's marker replaced, and where each edit's
/// blockquote starts in the result.
fn apply(src: &str, edits: &[(usize, Range<usize>, String, String)]) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(src.len());
    let mut starts = Vec::with_capacity(edits.len());
    let mut last = 0;
    for (quote, at, bare, _) in edits {
        starts.push(out.len() + (quote - last));
        out.push_str(&src[last..at.start]);
        out.push_str(bare);
        last = at.end;
    }
    out.push_str(&src[last..]);
    (out, starts)
}

/// Whether `text`, the first line of a plain quote, would read back as a
/// box marker (`[!kind]` followed by a space or the line's end, after any
/// leading spaces): such a line needs escaping on export. Returns the byte
/// offset of its `[`.
pub fn marker_start(text: &str) -> Option<usize> {
    let lead = text.len() - text.trim_start_matches([' ', '\t']).len();
    let inner = text[lead..].strip_prefix("[!")?;
    let close = inner.find(']')?;
    CalloutKind::parse(&inner[..close])?;
    let tail = &inner[close + 1..];
    (tail.is_empty() || tail.starts_with(char::is_whitespace)).then_some(lead)
}

/// The titled marker `[!kind] Title` on the first line of the blockquote
/// starting at `start` (its `>`): the marker's byte range up to the line's
/// end, the kind and the unescaped title.
fn titled_marker(src: &str, start: usize) -> Option<(Range<usize>, CalloutKind, String)> {
    let line_end = src[start..].find('\n').map_or(src.len(), |n| start + n);
    let line = &src[start..line_end];
    let after = line.strip_prefix('>')?;
    // At most one space or tab, as pulldown-cmark reads an alert; more
    // makes the line plain text or code.
    let lead = 1 + usize::from(after.starts_with([' ', '\t']));
    let rest = &line[lead..];
    let inner = rest.strip_prefix("[!")?;
    let close = inner.find(']')?;
    let kind = CalloutKind::parse(&inner[..close])?;
    let tail = &inner[close + 1..];
    if !tail.starts_with([' ', '\t']) {
        return None;
    }
    let title = normalise_title(&unescape_title(tail.trim_end_matches('\r')))?;
    Some((start + lead..line_end, kind, title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_case_insensitively() {
        assert_eq!(CalloutKind::parse("WARNING"), Some(CalloutKind::Warning));
        assert_eq!(CalloutKind::parse("tip"), Some(CalloutKind::Tip));
        assert_eq!(CalloutKind::parse("Warn"), None);
        assert_eq!(CalloutKind::Important.as_str(), "important");
        assert_eq!(CalloutKind::Caution.label(), "Caution");
    }

    #[test]
    fn titles_are_trimmed_single_line_text() {
        assert_eq!(normalise_title("  a\nb "), Some("a b".to_string()));
        assert_eq!(normalise_title(" "), None);
    }
}
