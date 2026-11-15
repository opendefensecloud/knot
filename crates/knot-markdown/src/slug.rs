//! Heading anchor ids, the way GitHub makes them: lower case, letters,
//! digits, spaces and `-` kept, whitespace runs become `-`, repeats are
//! numbered `x`, `x-1`, `x-2`, and an empty result is `section`. The web
//! client has the same rule (`web/src/features/editor/headingSlugs.ts`);
//! `tests/fixtures/heading_slugs.json` holds both to it. Rule after
//! dendronotes (`web/src/lib/outline.ts`, Apache-2.0).

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Hands out unique slugs for the headings of one page, in order.
#[derive(Debug, Default)]
pub struct Slugger {
    seen: HashMap<String, usize>,
}

impl Slugger {
    /// The slug for the next heading titled `title`.
    pub fn slug(&mut self, title: &str) -> String {
        let base = base_slug(title);
        let n = self.seen.entry(base.clone()).or_insert(0);
        let out = if *n == 0 { base } else { format!("{base}-{n}") };
        *n += 1;
        out
    }
}

fn base_slug(title: &str) -> String {
    let kept: String = title
        .nfkd()
        .flat_map(char::to_lowercase)
        .filter(|&c| {
            (c.is_alphabetic() && !is_combining_mark(c))
                || c.is_numeric()
                || c.is_whitespace()
                || c == '-'
        })
        .collect();
    let joined = kept.split_whitespace().collect::<Vec<_>>().join("-");
    if joined.is_empty() {
        "section".to_owned()
    } else {
        joined
    }
}
