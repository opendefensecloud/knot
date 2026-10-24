//! Round-trip serializer between the canonical ProseMirror schema and Markdown.

pub mod from_markdown;
#[cfg(feature = "legacy-compat")]
pub mod legacy;
pub mod schema;
pub mod tasks;
pub mod to_markdown;

pub use to_markdown::{SerError, serialise};

/// Sentinel URL prefix for embedded Excalidraw boards in Markdown.
pub const BOARD_URL_PREFIX: &str = "knot://board/";
/// Sentinel URL suffix for embedded Excalidraw boards in Markdown.
pub const BOARD_URL_SUFFIX: &str = ".svg";
/// Sentinel URL prefix for internal document links in Markdown.
/// Form: `knot://doc/<uuid>` (no suffix; links target the doc as a whole).
pub const DOC_URL_PREFIX: &str = "knot://doc/";
/// Sentinel URL prefix for inline datetimes in Markdown.
/// Form: `knot://time/<rfc3339-utc>`. The link text is the human label
/// (rendered in the user's local time); the URL is the source of truth.
pub const TIME_URL_PREFIX: &str = "knot://time/";
/// Default alt-text label used when serialising a board with no explicit label,
/// and recognised as "no label" when parsing.
pub(crate) const DEFAULT_BOARD_LABEL: &str = "Diagram";

/// Build a sentinel URL for the given board id: `knot://board/<id>.svg`.
pub(crate) fn board_sentinel_url(id: &str) -> String {
    format!("{BOARD_URL_PREFIX}{id}{BOARD_URL_SUFFIX}")
}
