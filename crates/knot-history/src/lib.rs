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
//! - [`diff`]: one entry's diff on a union doc, the core of M1.4's "Show
//!   changes".
//! - [`words`]: the words an entry added and removed, counted in that diff.
//! - [`canonical`]: a page's visible content as plain JSON, for comparing
//!   two states by what a reader sees.
//! - [`pass`]: one builder pass as pure compute (attribution, gap check,
//!   checkpoints), and the window replay every other reader uses.
//! - [`genesis`]: a page's history before `history_since`, as one
//!   checkpoint and its pre-rollout entries.

pub mod canonical;
pub mod claims;
pub mod diff;
pub mod genesis;
pub mod isolate;
pub mod pass;
pub mod replay;
pub mod sessions;
pub mod words;

pub use claims::{Claims, ClaimsError, Credit, ParkedClaim};
pub use diff::{
    ChunkKind, DiffError, EntryDiff, TextChunk, entry_diff, strip_zero_clocks, text_chunks,
};
pub use genesis::{BaselineInput, GenesisError, GenesisInput, GenesisOutput, build_genesis};
pub use isolate::{IsolateError, run_isolated};
pub use pass::{
    CHECKPOINT_FLOOR_BYTES, CHECKPOINT_STATE_FRACTION, CheckpointKind, CheckpointWrite, KeptGap,
    PassInput, PassOutput, PassStats, Progress, ReplayHooks, SnapshotProbe, Stage,
    UnknownCheckpointKind, WindowError, WindowInput, checkpoint_threshold, replay_window, run_pass,
};
pub use replay::{ReplayError, ReplayMode, Replayer, RowEffect, RowOutcome};
pub use sessions::{
    Contribution, ContributionKind, EntryDraft, EntryKind, Grouper, SEAL_MARGIN, SESSION_GAP,
    UnknownEntryKind,
};
pub use words::{WordCounts, count_words, entry_word_counts};
