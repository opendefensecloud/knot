//! The frozen reference for the differential compatibility harness (spec
//! "Compatibility guarantees", 1; planning decision P20).
//!
//! `to_markdown.rs`, `from_markdown.rs` and `tasks.rs` are copies of the
//! serializer, importer and task extractor as they were before the PR #42
//! fixes (commit 3d09071), without their test modules. Never edit them: every
//! change to the real modules is measured against them. `compat` holds the
//! classifiers and the task differential the harness and the real-data
//! check share.
//!
//! The reference is fully frozen: the copies call no live code of this
//! crate. The helpers they used from it are snapshotted here as they were at
//! 3d09071: the mark table in `schema.rs`, and below the board sentinel
//! constants and URL builder that `lib.rs` defines.
//!
//! Compiled only with the `legacy-compat` feature, which production builds
//! don't enable.

pub mod compat;
pub mod from_markdown;
mod schema;
pub mod tasks;
pub mod to_markdown;

pub use from_markdown::{ParseError, parse};
pub use tasks::{Task, extract_tasks};
pub use to_markdown::{SerError, serialise};

/// Snapshot of `crate::BOARD_URL_PREFIX`.
const BOARD_URL_PREFIX: &str = "knot://board/";
/// Snapshot of `crate::BOARD_URL_SUFFIX`.
const BOARD_URL_SUFFIX: &str = ".svg";
/// Snapshot of `crate::DEFAULT_BOARD_LABEL`.
const DEFAULT_BOARD_LABEL: &str = "Diagram";

/// Snapshot of `crate::board_sentinel_url`: `knot://board/<id>.svg`.
fn board_sentinel_url(id: &str) -> String {
    format!("{BOARD_URL_PREFIX}{id}{BOARD_URL_SUFFIX}")
}
