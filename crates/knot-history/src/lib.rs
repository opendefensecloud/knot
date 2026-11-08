//! Page history (M1.3): the builder that replays each page's log into
//! timeline entries, and the read API over them.
//!
//! - [`replay`]: the replay engine, a yrs doc that applies a page's rows and
//!   reports what each one changed and parked.
//! - [`isolate`]: the thread every replay runs on, which turns a panic or a
//!   hang into an error.
//! - [`claims`]: content a row parked, credited to that row's author when a
//!   later apply integrates it.
//! - [`sessions`]: groups credited effects into per-author sessions and the
//!   other timeline entries.

pub mod claims;
pub mod isolate;
pub mod replay;
pub mod sessions;

pub use claims::{Claims, ClaimsError, Credit, ParkedClaim};
pub use isolate::{IsolateError, run_isolated};
pub use replay::{ReplayError, ReplayMode, Replayer, RowEffect, RowOutcome};
pub use sessions::{
    Contribution, ContributionKind, EntryDraft, EntryKind, Grouper, SEAL_MARGIN, SESSION_GAP,
    UnknownEntryKind,
};
