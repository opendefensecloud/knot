//! The frozen reference for the differential compatibility harness (spec
//! "Compatibility guarantees", 1; planning decision P20).
//!
//! `to_markdown.rs`, `from_markdown.rs` and `tasks.rs` are copies of the
//! serializer, importer and task extractor as they were before the PR #42
//! fixes (commit 3d09071), without their test modules. Never edit them: every
//! change to the real modules is measured against them. `compat` holds the
//! classifiers the harness and the real-data check share.
//!
//! Compiled only with the `legacy-compat` feature, which production builds
//! don't enable.

pub mod compat;
pub mod from_markdown;
pub mod tasks;
pub mod to_markdown;

pub use from_markdown::{ParseError, parse};
pub use tasks::{Task, extract_tasks};
pub use to_markdown::{SerError, serialise};
