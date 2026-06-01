//! Round-trip serializer between the canonical ProseMirror schema and Markdown.

pub mod from_markdown;
pub mod schema;
pub mod to_markdown;

pub use to_markdown::{SerError, serialise};
