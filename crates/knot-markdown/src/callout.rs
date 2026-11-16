//! Callout boxes: a `blockquote` with a `kind` (one of GitHub's and
//! GitLab's five alert kinds) and an optional plain-text `title`.

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
