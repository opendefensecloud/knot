//! Page history (M1.3): the builder that replays each page's log into
//! timeline entries, and the read API over them.
//!
//! - [`replay`]: the replay engine, a yrs doc that applies a page's rows and
//!   reports what each one changed and parked.
//! - [`isolate`]: the thread every replay runs on, which turns a panic or a
//!   hang into an error.

pub mod isolate;
pub mod replay;

pub use isolate::{IsolateError, run_isolated};
pub use replay::{ReplayError, ReplayMode, Replayer, RowEffect, RowOutcome};
