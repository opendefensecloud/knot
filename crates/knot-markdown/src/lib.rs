//! Round-trip serializer between the canonical ProseMirror schema and Markdown.

pub mod from_markdown;
pub mod schema;
pub mod to_markdown;

pub use to_markdown::{SerError, serialise};

/// Sentinel URL prefix for embedded Excalidraw boards in Markdown.
pub const BOARD_URL_PREFIX: &str = "knot://board/";
/// Sentinel URL suffix for embedded Excalidraw boards in Markdown.
pub const BOARD_URL_SUFFIX: &str = ".svg";
/// Default alt-text label used when serialising a board with no explicit label,
/// and recognised as "no label" when parsing.
pub(crate) const DEFAULT_BOARD_LABEL: &str = "Diagram";

/// Build a sentinel URL for the given board id: `knot://board/<id>.svg`.
pub(crate) fn board_sentinel_url(id: &str) -> String {
    format!("{BOARD_URL_PREFIX}{id}{BOARD_URL_SUFFIX}")
}
