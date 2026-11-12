//! Word counts of an entry: whitespace tokens in the `Added` and `Removed`
//! chunks of exactly the diff "Show changes" renders ([`crate::diff`]).
//!
//! yrs 0.27.3's diff starts a new chunk at every changed item and at every
//! format item (`DiffAssembler::process`, `types/text.rs:569-680`), so one
//! word typed in two items that were not squashed, or bolded mid-word, comes
//! back as several chunks. A word is therefore counted over each maximal run
//! of consecutive chunks of one kind within one segment, which is what the
//! view renders as one highlighted stretch.

use yrs::{Doc, IdSet};

use crate::diff::{ChunkKind, DiffError, TextChunk, entry_diff, text_chunks};

/// Words an entry added and removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordCounts {
    pub added: i32,
    pub removed: i32,
}

/// Counts the words of `chunks`: `split_whitespace().count()` over each
/// maximal run of consecutive `Added` (or `Removed`) chunks within one
/// segment.
pub fn count_words(chunks: &[TextChunk]) -> WordCounts {
    let mut counts = WordCounts {
        added: 0,
        removed: 0,
    };
    let runs = chunks.chunk_by(|a, b| a.segment == b.segment && a.kind == b.kind);
    for run in runs {
        let slot = match run[0].kind {
            ChunkKind::Plain => continue,
            ChunkKind::Added => &mut counts.added,
            ChunkKind::Removed => &mut counts.removed,
        };
        let text: String = run.iter().map(|c| c.text.as_str()).collect();
        let words = i32::try_from(text.split_whitespace().count()).unwrap_or(i32::MAX);
        *slot = slot.saturating_add(words);
    }
    counts
}

/// The word counts of the entry whose insert set is `inserted` and whose
/// delete set is `deleted`, on `h`: a skip-GC doc replayed to the entry's
/// `end_rev`.
pub fn entry_word_counts(
    h: &Doc,
    inserted: &IdSet,
    deleted: &IdSet,
) -> Result<WordCounts, DiffError> {
    let d = entry_diff(h, inserted, deleted)?;
    Ok(count_words(&text_chunks(&d)))
}
