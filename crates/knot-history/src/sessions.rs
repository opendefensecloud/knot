//! Session grouping: turns each row's credited effect into timeline entries.
//!
//! Rows whose source is NULL, `edit`, `task` or `suggestion` join their
//! author's open session when `row.at − session.ended_at ≤ SESSION_GAP`
//! (negative counts as joining: late gap credits carry an older time),
//! else that session seals and a new one opens. Rows with `restore`,
//! `restore_fallback`, `import`, `template` or `copy` seal the same author's
//! open session and become a sealed single-row entry. Gap content touches no
//! session and becomes a sealed author-less `gap` entry. Open sessions seal
//! once idle for `SESSION_GAP + SEAL_MARGIN` ([`Grouper::seal_idle`]).
//!
//! The pass filters contributions whose effect is empty before they reach
//! the grouper, except the page's first `copy` row; [`Grouper::feed`] drops
//! any other empty one too, so an empty effect never touches a session.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use knot_storage::UpdateSource;
use serde_json::{Map, Value, json};
use thiserror::Error;
use uuid::Uuid;
use yrs::IdSet;

/// Idle time after which the next row of the same author opens a new session.
pub const SESSION_GAP: chrono::Duration = chrono::Duration::minutes(20);

/// Extra idle time before a sweep seals a session: a row of the same author
/// may still be committing (an append can take `APPEND_STATEMENT_TIMEOUT`).
pub const SEAL_MARGIN: chrono::Duration = chrono::Duration::seconds(60);

/// `doc_history_entries.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Session,
    Restore,
    Copy,
    Import,
    Template,
    Erase,
    Baseline,
    Gap,
}

impl EntryKind {
    /// The SQL spelling (`doc_history_entries.kind`).
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Session => "session",
            EntryKind::Restore => "restore",
            EntryKind::Copy => "copy",
            EntryKind::Import => "import",
            EntryKind::Template => "template",
            EntryKind::Erase => "erase",
            EntryKind::Baseline => "baseline",
            EntryKind::Gap => "gap",
        }
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `kind` that is none of `EntryKind`'s spellings.
#[derive(Debug, Error)]
#[error("unknown history entry kind {0:?}")]
pub struct UnknownEntryKind(pub String);

impl FromStr for EntryKind {
    type Err = UnknownEntryKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "session" => EntryKind::Session,
            "restore" => EntryKind::Restore,
            "copy" => EntryKind::Copy,
            "import" => EntryKind::Import,
            "template" => EntryKind::Template,
            "erase" => EntryKind::Erase,
            "baseline" => EntryKind::Baseline,
            "gap" => EntryKind::Gap,
            other => return Err(UnknownEntryKind(other.to_string())),
        })
    }
}

/// Where a contribution's effect comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionKind {
    /// A row's own effect, by its `source`; `None` is a 0.9.9-era or
    /// old-replica row, which groups like `edit`.
    Row(Option<UpdateSource>),
    /// A parked row's content that a gap merge drained, credited to that
    /// row's author: groups like `Row(Some(Edit))` and sets the session's
    /// `meta.gap_credit_rev` to its rev.
    GapCredit,
    /// Content a snapshot holds and no row carries, or a gap claim's credit.
    Gap,
}

/// One credited effect, fed to the grouper in rev order.
#[derive(Debug, Clone)]
pub struct Contribution {
    pub rev: i64,
    pub at: DateTime<Utc>,
    pub author: Option<Uuid>,
    pub kind: ContributionKind,
    /// The row's `meta` (`doc_updates.meta`).
    pub meta: Option<Value>,
    pub inserted: IdSet,
    pub deleted: IdSet,
}

/// A `doc_history_entries` row to write: `id` is `None` for an entry this
/// grouper created.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryDraft {
    pub id: Option<i64>,
    pub kind: EntryKind,
    pub author: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub start_rev: Option<i64>,
    pub end_rev: Option<i64>,
    pub sealed: bool,
    pub inserted: IdSet,
    pub deleted: IdSet,
    pub meta: Value,
    pub baseline_id: Option<i64>,
}

/// What a contribution does to the sessions.
enum Class {
    /// Joins its author's session; `gap_credit` marks the session's
    /// `gap_credit_rev`.
    Session { gap_credit: bool },
    /// Seals its author's session and becomes a sealed entry of this kind.
    Special { kind: EntryKind, fallback: bool },
    /// Becomes a sealed gap entry and touches no session.
    Gap,
}

fn classify(kind: ContributionKind) -> Class {
    use UpdateSource as S;
    match kind {
        ContributionKind::Row(None | Some(S::Edit | S::Task | S::Suggestion)) => {
            Class::Session { gap_credit: false }
        }
        ContributionKind::GapCredit => Class::Session { gap_credit: true },
        ContributionKind::Row(Some(S::Restore)) => Class::Special {
            kind: EntryKind::Restore,
            fallback: false,
        },
        ContributionKind::Row(Some(S::RestoreFallback)) => Class::Special {
            kind: EntryKind::Restore,
            fallback: true,
        },
        ContributionKind::Row(Some(S::Import)) => Class::Special {
            kind: EntryKind::Import,
            fallback: false,
        },
        ContributionKind::Row(Some(S::Template)) => Class::Special {
            kind: EntryKind::Template,
            fallback: false,
        },
        ContributionKind::Row(Some(S::Copy)) => Class::Special {
            kind: EntryKind::Copy,
            fallback: false,
        },
        ContributionKind::Gap => Class::Gap,
    }
}

/// One entry the grouper holds, with whether it changed.
struct Slot {
    entry: EntryDraft,
    changed: bool,
}

/// Groups contributions into entries, continuing the page's open sessions.
pub struct Grouper {
    /// The open sessions it was given first, then every entry it created, in
    /// creation order.
    slots: Vec<Slot>,
    /// Each author's open session, as an index into `slots`.
    open: HashMap<Option<Uuid>, usize>,
}

impl Grouper {
    /// Starts from the page's open sessions (as of `built_rev`). Should two
    /// be open for one author, the one that ended last is continued.
    pub fn new(open: Vec<EntryDraft>) -> Grouper {
        let mut by_author: HashMap<Option<Uuid>, usize> = HashMap::new();
        let slots: Vec<Slot> = open
            .into_iter()
            .map(|entry| Slot {
                entry,
                changed: false,
            })
            .collect();
        for (i, slot) in slots.iter().enumerate() {
            let e = &slot.entry;
            if e.kind != EntryKind::Session || e.sealed {
                continue;
            }
            match by_author.get(&e.author) {
                Some(&j) if slots[j].entry.ended_at >= e.ended_at => {}
                _ => {
                    by_author.insert(e.author, i);
                }
            }
        }
        Grouper {
            slots,
            open: by_author,
        }
    }

    /// Feeds one contribution; contributions come in rev order.
    pub fn feed(&mut self, c: Contribution) {
        let class = classify(c.kind);
        let empty = c.inserted.is_empty() && c.deleted.is_empty();
        match class {
            Class::Session { gap_credit } => {
                if !empty {
                    self.join(c, gap_credit);
                }
            }
            // An empty special row reaches the grouper only as the page's
            // first `copy` row, which still gets its "Created from" entry.
            Class::Special { kind, fallback } => {
                self.seal_open(c.author);
                let mut meta = object_meta(c.meta.as_ref());
                if fallback {
                    meta.insert("fallback".into(), Value::Bool(true));
                }
                self.push(single(&c, kind, c.author, Value::Object(meta)));
            }
            Class::Gap => {
                if !empty {
                    self.push(single(&c, EntryKind::Gap, None, json!({})));
                }
            }
        }
    }

    /// Seals every open session that ended before
    /// `now − SESSION_GAP − SEAL_MARGIN`. Called only by a pass that
    /// processed every row up to the page's head.
    pub fn seal_idle(&mut self, now: DateTime<Utc>) {
        let cutoff = now - SESSION_GAP - SEAL_MARGIN;
        let idle: Vec<Option<Uuid>> = self
            .open
            .iter()
            .filter(|&(_, &i)| self.slots[i].entry.ended_at < cutoff)
            .map(|(author, _)| *author)
            .collect();
        for author in idle {
            self.seal_open(author);
        }
    }

    /// Every entry it changed or created, sealed or open, in creation order.
    pub fn finish(self) -> Vec<EntryDraft> {
        self.slots
            .into_iter()
            .filter(|s| s.changed)
            .map(|s| s.entry)
            .collect()
    }

    fn join(&mut self, c: Contribution, gap_credit: bool) {
        if let Some(&i) = self.open.get(&c.author)
            && c.at - self.slots[i].entry.ended_at <= SESSION_GAP
        {
            let slot = &mut self.slots[i];
            slot.changed = true;
            let e = &mut slot.entry;
            e.started_at = e.started_at.min(c.at);
            e.ended_at = e.ended_at.max(c.at);
            e.start_rev = Some(e.start_rev.map_or(c.rev, |r| r.min(c.rev)));
            e.end_rev = Some(e.end_rev.map_or(c.rev, |r| r.max(c.rev)));
            e.inserted.merge_with(c.inserted.clone());
            e.deleted.merge_with(c.deleted.clone());
            note_session_meta(&mut e.meta, &c, gap_credit);
            return;
        }
        self.seal_open(c.author);
        let mut meta = json!({});
        note_session_meta(&mut meta, &c, gap_credit);
        let i = self.push(EntryDraft {
            id: None,
            kind: EntryKind::Session,
            author: c.author,
            started_at: c.at,
            ended_at: c.at,
            start_rev: Some(c.rev),
            end_rev: Some(c.rev),
            sealed: false,
            inserted: c.inserted,
            deleted: c.deleted,
            meta,
            baseline_id: None,
        });
        self.open.insert(c.author, i);
    }

    /// Seals `author`'s open session, if any.
    fn seal_open(&mut self, author: Option<Uuid>) {
        if let Some(i) = self.open.remove(&author) {
            let slot = &mut self.slots[i];
            slot.entry.sealed = true;
            slot.changed = true;
        }
    }

    fn push(&mut self, entry: EntryDraft) -> usize {
        self.slots.push(Slot {
            entry,
            changed: true,
        });
        self.slots.len() - 1
    }
}

/// A sealed single-rev entry for `c`.
fn single(c: &Contribution, kind: EntryKind, author: Option<Uuid>, meta: Value) -> EntryDraft {
    EntryDraft {
        id: None,
        kind,
        author,
        started_at: c.at,
        ended_at: c.at,
        start_rev: Some(c.rev),
        end_rev: Some(c.rev),
        sealed: true,
        inserted: c.inserted.clone(),
        deleted: c.deleted.clone(),
        meta,
        baseline_id: None,
    }
}

/// A row's `meta` as an object; anything else reads as `{}`.
fn object_meta(meta: Option<&Value>) -> Map<String, Value> {
    match meta {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

/// Adds what `c` tells a session's `meta`: who accepted a suggestion and
/// which one (deduplicated, in insertion order), or the rev of a gap credit.
fn note_session_meta(meta: &mut Value, c: &Contribution, gap_credit: bool) {
    if !meta.is_object() {
        *meta = json!({});
    }
    let Some(m) = meta.as_object_mut() else {
        return;
    };
    if gap_credit {
        m.insert("gap_credit_rev".into(), json!(c.rev));
    }
    if c.kind != ContributionKind::Row(Some(UpdateSource::Suggestion)) {
        return;
    }
    let Some(row_meta) = c.meta.as_ref() else {
        return;
    };
    for (from, into) in [
        ("accepted_by", "accepted_by"),
        ("suggestion_id", "suggestion_ids"),
    ] {
        let Some(id) = row_meta
            .get(from)
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        let id = Value::String(id.to_string());
        let list = m.entry(into).or_insert_with(|| json!([]));
        if !list.is_array() {
            *list = json!([]);
        }
        if let Some(list) = list.as_array_mut()
            && !list.contains(&id)
        {
            list.push(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;
    use yrs::ID;
    use yrs::block::ClientID;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 8, h, m, s).unwrap()
    }

    fn ids(client: u64, clock: u32, len: u32) -> IdSet {
        let mut s = IdSet::new();
        s.insert(ID::new(ClientID::new(client), clock), len);
        s
    }

    /// The ids a contribution at `rev` inserts in these tests.
    fn ins_of(rev: i64) -> IdSet {
        ids(1, rev as u32 * 10, 1)
    }

    fn contribution(
        rev: i64,
        when: DateTime<Utc>,
        author: Option<Uuid>,
        kind: ContributionKind,
    ) -> Contribution {
        Contribution {
            rev,
            at: when,
            author,
            kind,
            meta: None,
            inserted: ins_of(rev),
            deleted: IdSet::new(),
        }
    }

    fn row(rev: i64, when: DateTime<Utc>, author: Option<Uuid>, src: UpdateSource) -> Contribution {
        contribution(rev, when, author, ContributionKind::Row(Some(src)))
    }

    fn edit(rev: i64, when: DateTime<Utc>, author: Option<Uuid>) -> Contribution {
        row(rev, when, author, UpdateSource::Edit)
    }

    fn group(open: Vec<EntryDraft>, cs: Vec<Contribution>) -> Vec<EntryDraft> {
        let mut g = Grouper::new(open);
        for c in cs {
            g.feed(c);
        }
        g.finish()
    }

    fn open_session(
        id: i64,
        author: Option<Uuid>,
        revs: (i64, i64),
        times: (DateTime<Utc>, DateTime<Utc>),
    ) -> EntryDraft {
        EntryDraft {
            id: Some(id),
            kind: EntryKind::Session,
            author,
            started_at: times.0,
            ended_at: times.1,
            start_rev: Some(revs.0),
            end_rev: Some(revs.1),
            sealed: false,
            inserted: ins_of(revs.0),
            deleted: IdSet::new(),
            meta: json!({}),
            baseline_id: None,
        }
    }

    #[test]
    fn two_authors_at_once_get_two_sessions() {
        let a = Some(Uuid::new_v4());
        let b = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                edit(1, at(9, 0, 0), a),
                edit(2, at(9, 10, 0), b),
                edit(3, at(9, 15, 0), a),
                edit(4, at(9, 20, 0), b),
                edit(5, at(9, 30, 0), a),
                edit(6, at(9, 40, 0), a),
            ],
        );
        assert_eq!(out.len(), 2);
        let (sa, sb) = (&out[0], &out[1]);
        for s in [sa, sb] {
            assert_eq!(s.kind, EntryKind::Session);
            assert!(!s.sealed);
            assert_eq!(s.id, None);
            assert_eq!(s.baseline_id, None);
            assert_eq!(s.meta, json!({}));
        }
        assert_eq!(sa.author, a);
        assert_eq!((sa.start_rev, sa.end_rev), (Some(1), Some(6)));
        assert_eq!((sa.started_at, sa.ended_at), (at(9, 0, 0), at(9, 40, 0)));
        let a_ins = [1, 3, 5, 6]
            .iter()
            .fold(IdSet::new(), |s, r| s.merge(&ins_of(*r)));
        assert_eq!(sa.inserted, a_ins);
        assert_eq!(sb.author, b);
        assert_eq!((sb.start_rev, sb.end_rev), (Some(2), Some(4)));
        assert_eq!(sb.inserted, ins_of(2).merge(&ins_of(4)));
    }

    #[test]
    fn a_row_more_than_20_minutes_later_opens_a_new_session() {
        let a = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                edit(1, at(9, 0, 0), a),
                // Exactly SESSION_GAP after the last row: joins.
                edit(2, at(9, 20, 0), a),
                // SESSION_GAP + 1 s: seals and opens.
                edit(3, at(9, 40, 1), a),
            ],
        );
        assert_eq!(out.len(), 2);
        assert!(out[0].sealed);
        assert_eq!((out[0].start_rev, out[0].end_rev), (Some(1), Some(2)));
        assert_eq!(out[0].ended_at, at(9, 20, 0));
        assert!(!out[1].sealed);
        assert_eq!((out[1].start_rev, out[1].end_rev), (Some(3), Some(3)));
        assert_eq!(
            (out[1].started_at, out[1].ended_at),
            (at(9, 40, 1), at(9, 40, 1))
        );
        assert_eq!(out[1].inserted, ins_of(3));
    }

    #[test]
    fn a_special_row_seals_its_authors_session_only() {
        let cases = [
            (UpdateSource::Restore, EntryKind::Restore, json!({"k": 1})),
            (
                UpdateSource::RestoreFallback,
                EntryKind::Restore,
                json!({"k": 1, "fallback": true}),
            ),
            (UpdateSource::Import, EntryKind::Import, json!({"k": 1})),
            (UpdateSource::Template, EntryKind::Template, json!({"k": 1})),
            // Not the page's first row: a copy row the pass still feeds.
            (UpdateSource::Copy, EntryKind::Copy, json!({"k": 1})),
        ];
        for (src, kind, meta) in cases {
            let a = Some(Uuid::new_v4());
            let b = Some(Uuid::new_v4());
            let mut special = row(3, at(9, 2, 0), a, src);
            special.meta = Some(json!({"k": 1}));
            let out = group(
                vec![],
                vec![edit(1, at(9, 0, 0), a), edit(2, at(9, 1, 0), b), special],
            );

            assert_eq!(out.len(), 3, "{src:?}");
            assert_eq!(out[0].author, a);
            assert!(out[0].sealed, "{src:?}: the author's session seals");
            assert_eq!(out[0].end_rev, Some(1));
            assert_eq!(out[1].author, b);
            assert!(
                !out[1].sealed,
                "{src:?}: the other author's session stays open"
            );
            let e = &out[2];
            assert_eq!(e.kind, kind, "{src:?}");
            assert_eq!(e.author, a);
            assert!(e.sealed);
            assert_eq!((e.start_rev, e.end_rev), (Some(3), Some(3)));
            assert_eq!((e.started_at, e.ended_at), (at(9, 2, 0), at(9, 2, 0)));
            assert_eq!(e.inserted, ins_of(3));
            assert_eq!(e.meta, meta, "{src:?}");
        }
    }

    #[test]
    fn a_special_row_after_it_opens_a_new_session() {
        let a = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                edit(1, at(9, 0, 0), a),
                row(2, at(9, 1, 0), a, UpdateSource::Import),
                edit(3, at(9, 2, 0), a),
            ],
        );
        let kinds: Vec<_> = out
            .iter()
            .map(|e| (e.kind, e.sealed, e.start_rev))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (EntryKind::Session, true, Some(1)),
                (EntryKind::Import, true, Some(2)),
                (EntryKind::Session, false, Some(3)),
            ]
        );
    }

    #[test]
    fn restore_fallback_is_a_restore_entry_marked_fallback() {
        let a = Some(Uuid::new_v4());
        let mut fallback = row(1, at(9, 0, 0), a, UpdateSource::RestoreFallback);
        fallback.meta = Some(json!({"restored_from": 7}));
        let bare = row(2, at(9, 1, 0), a, UpdateSource::RestoreFallback);
        let restore = row(3, at(9, 2, 0), a, UpdateSource::Restore);
        let out = group(vec![], vec![fallback, bare, restore]);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|e| e.kind == EntryKind::Restore && e.sealed));
        assert_eq!(out[0].meta, json!({"restored_from": 7, "fallback": true}));
        assert_eq!(out[1].meta, json!({"fallback": true}));
        assert_eq!(out[2].meta, json!({}));
    }

    #[test]
    fn an_empty_first_copy_row_still_creates_its_entry() {
        let a = Some(Uuid::new_v4());
        let mut copy = row(1, at(9, 0, 0), a, UpdateSource::Copy);
        copy.inserted = IdSet::new();
        copy.meta = Some(json!({"source_id": "x"}));
        let out = group(vec![], vec![copy]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EntryKind::Copy);
        assert!(out[0].inserted.is_empty() && out[0].deleted.is_empty());
        assert_eq!(out[0].meta, json!({"source_id": "x"}));
    }

    #[test]
    fn an_empty_effect_touches_no_session() {
        let a = Some(Uuid::new_v4());
        let mut empty_edit = edit(2, at(9, 30, 0), a);
        empty_edit.inserted = IdSet::new();
        let mut empty_credit = contribution(3, at(9, 31, 0), a, ContributionKind::GapCredit);
        empty_credit.inserted = IdSet::new();
        let mut empty_gap = contribution(4, at(9, 32, 0), None, ContributionKind::Gap);
        empty_gap.inserted = IdSet::new();
        let out = group(
            vec![],
            vec![edit(1, at(9, 0, 0), a), empty_edit, empty_credit, empty_gap],
        );
        assert_eq!(out.len(), 1);
        assert!(
            !out[0].sealed,
            "an empty row 30 minutes later seals nothing"
        );
        assert_eq!((out[0].end_rev, out[0].ended_at), (Some(1), at(9, 0, 0)));
        assert_eq!(out[0].meta, json!({}));
    }

    #[test]
    fn task_and_legacy_rows_join_the_authors_session() {
        let a = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                edit(1, at(9, 0, 0), a),
                row(2, at(9, 5, 0), a, UpdateSource::Task),
                contribution(3, at(9, 10, 0), a, ContributionKind::Row(None)),
            ],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EntryKind::Session);
        assert!(!out[0].sealed);
        assert_eq!((out[0].start_rev, out[0].end_rev), (Some(1), Some(3)));
    }

    #[test]
    fn a_gap_is_its_own_sealed_entry_even_with_an_author_less_session_open() {
        let gap = contribution(2, at(9, 5, 0), None, ContributionKind::Gap);

        // Up to the gap: the author-less session is untouched by it.
        let out = group(vec![], vec![edit(1, at(9, 0, 0), None), gap.clone()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, EntryKind::Session);
        assert_eq!(out[0].author, None);
        assert!(!out[0].sealed);
        assert_eq!((out[0].end_rev, out[0].ended_at), (Some(1), at(9, 0, 0)));
        assert_eq!(out[0].inserted, ins_of(1));
        let g = &out[1];
        assert_eq!(g.kind, EntryKind::Gap);
        assert_eq!(g.author, None);
        assert!(g.sealed);
        assert_eq!((g.start_rev, g.end_rev), (Some(2), Some(2)));
        assert_eq!((g.started_at, g.ended_at), (at(9, 5, 0), at(9, 5, 0)));
        assert_eq!(g.inserted, ins_of(2));
        assert_eq!(g.meta, json!({}));

        // A further author-less row joins the session the gap left open.
        let out = group(
            vec![],
            vec![edit(1, at(9, 0, 0), None), gap, edit(3, at(9, 10, 0), None)],
        );
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].start_rev, out[0].end_rev), (Some(1), Some(3)));
        assert!(!out[0].sealed);
        assert_eq!(out[0].inserted, ins_of(1).merge(&ins_of(3)));
        assert_eq!(out[1].kind, EntryKind::Gap);
    }

    #[test]
    fn an_earlier_contribution_never_moves_the_end_back() {
        let a = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                edit(1, at(9, 0, 0), a),
                edit(2, at(9, 20, 0), a),
                contribution(3, at(9, 10, 0), a, ContributionKind::GapCredit),
            ],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].started_at, out[0].ended_at),
            (at(9, 0, 0), at(9, 20, 0))
        );
        assert_eq!((out[0].start_rev, out[0].end_rev), (Some(1), Some(3)));
        assert!(!out[0].sealed);
    }

    #[test]
    fn an_earlier_contribution_moves_the_start_back() {
        let a = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                edit(1, at(9, 10, 0), a),
                contribution(2, at(9, 0, 0), a, ContributionKind::GapCredit),
            ],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].started_at, out[0].ended_at),
            (at(9, 0, 0), at(9, 10, 0))
        );
    }

    #[test]
    fn a_gap_credit_joins_the_authors_session_and_marks_its_rev() {
        let a = Some(Uuid::new_v4());
        let mut g = Grouper::new(vec![]);
        g.feed(edit(1, at(9, 0, 0), a));
        g.feed(contribution(3, at(9, 5, 0), a, ContributionKind::GapCredit));
        g.feed(edit(4, at(9, 6, 0), a));
        g.seal_idle(at(10, 0, 0));
        g.feed(contribution(
            9,
            at(10, 0, 0),
            a,
            ContributionKind::GapCredit,
        ));
        let out = g.finish();

        assert_eq!(out.len(), 2);
        assert!(out[0].sealed);
        assert_eq!((out[0].start_rev, out[0].end_rev), (Some(1), Some(4)));
        assert_eq!(out[0].meta, json!({"gap_credit_rev": 3}));
        assert_eq!(
            out[0].inserted,
            ins_of(1).merge(&ins_of(3)).merge(&ins_of(4))
        );
        assert_eq!(out[1].kind, EntryKind::Session);
        assert!(!out[1].sealed);
        assert_eq!((out[1].start_rev, out[1].end_rev), (Some(9), Some(9)));
        assert_eq!(out[1].meta, json!({"gap_credit_rev": 9}));
    }

    #[test]
    fn a_later_gap_credit_overwrites_the_marked_rev() {
        let a = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                contribution(3, at(9, 0, 0), a, ContributionKind::GapCredit),
                contribution(5, at(9, 1, 0), a, ContributionKind::GapCredit),
            ],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].meta, json!({"gap_credit_rev": 5}));
    }

    #[test]
    fn suggestion_rows_join_the_suggesters_session_and_note_who_accepted() {
        let a = Some(Uuid::new_v4());
        let owner = Uuid::new_v4();
        let (s1, s2) = (Uuid::new_v4(), Uuid::new_v4());
        let mut first = row(2, at(9, 1, 0), a, UpdateSource::Suggestion);
        first.meta = Some(json!({"suggestion_id": s1, "accepted_by": owner}));
        let mut second = row(3, at(9, 2, 0), a, UpdateSource::Suggestion);
        second.meta = Some(json!({"suggestion_id": s2, "accepted_by": owner}));
        let mut again = row(4, at(9, 3, 0), a, UpdateSource::Suggestion);
        again.meta = Some(json!({"suggestion_id": s1, "accepted_by": owner}));

        let out = group(vec![], vec![edit(1, at(9, 0, 0), a), first, second, again]);
        assert_eq!(out.len(), 1);
        let meta = &out[0].meta;
        assert_eq!(meta["accepted_by"], json!([owner]));
        assert_eq!(meta["suggestion_ids"], json!([s1, s2]));
        assert_eq!(meta.as_object().map(|m| m.len()), Some(2));
        assert_eq!((out[0].start_rev, out[0].end_rev), (Some(1), Some(4)));
        assert!(!out[0].sealed);
    }

    #[test]
    fn seal_idle_respects_the_margin() {
        let now = at(10, 0, 0);
        let a = Some(Uuid::new_v4());
        let b = Some(Uuid::new_v4());
        let at_margin = now - SESSION_GAP - SEAL_MARGIN;
        let past_margin = at_margin - chrono::Duration::seconds(1);
        let open = vec![
            open_session(1, a, (1, 2), (at(9, 0, 0), at_margin)),
            open_session(2, b, (3, 4), (at(9, 0, 0), past_margin)),
        ];
        let mut g = Grouper::new(open);
        g.seal_idle(now);
        let out = g.finish();
        // a's session is not idle long enough and was never touched: not returned.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, Some(2));
        assert_eq!(out[0].author, b);
        assert!(out[0].sealed);
        assert_eq!(out[0].ended_at, past_margin);

        // Sessions this grouper opened seal the same way.
        let mut g = Grouper::new(vec![]);
        g.feed(edit(1, at_margin, a));
        g.feed(edit(2, past_margin, b));
        g.seal_idle(now);
        let out = g.finish();
        assert_eq!(out.len(), 2);
        assert!(!out[0].sealed);
        assert!(out[1].sealed);
    }

    #[test]
    fn sessions_accumulate_their_rows_effects() {
        let a = Some(Uuid::new_v4());
        let mut first = edit(4, at(9, 0, 0), a);
        first.inserted = ids(1, 0, 3);
        first.deleted = ids(2, 0, 1);
        let mut second = edit(5, at(9, 5, 0), a);
        second.inserted = ids(1, 3, 2);
        second.deleted = ids(2, 5, 1);
        let out = group(vec![], vec![first, second]);
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.inserted, ids(1, 0, 5));
        assert_eq!(s.deleted, ids(2, 0, 1).merge(&ids(2, 5, 1)));
        assert_eq!((s.start_rev, s.end_rev), (Some(4), Some(5)));
        assert_eq!((s.started_at, s.ended_at), (at(9, 0, 0), at(9, 5, 0)));
    }

    #[test]
    fn an_open_session_from_an_earlier_pass_is_continued() {
        let a = Some(Uuid::new_v4());
        let b = Some(Uuid::new_v4());
        let open = vec![
            open_session(7, a, (1, 2), (at(8, 50, 0), at(8, 55, 0))),
            open_session(8, b, (3, 3), (at(8, 56, 0), at(8, 56, 0))),
        ];
        let out = group(open, vec![edit(4, at(9, 0, 0), a)]);
        // b's session was never touched: not returned.
        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.id, Some(7));
        assert!(!s.sealed);
        assert_eq!((s.start_rev, s.end_rev), (Some(1), Some(4)));
        assert_eq!((s.started_at, s.ended_at), (at(8, 50, 0), at(9, 0, 0)));
        assert_eq!(s.inserted, ins_of(1).merge(&ins_of(4)));

        // One that is too old to join is sealed and returned before the new one.
        let open = vec![open_session(7, a, (1, 2), (at(8, 0, 0), at(8, 30, 0)))];
        let out = group(open, vec![edit(4, at(9, 0, 0), a)]);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].id, out[0].sealed), (Some(7), true));
        assert_eq!(out[0].end_rev, Some(2));
        assert_eq!(
            (out[1].id, out[1].sealed, out[1].start_rev),
            (None, false, Some(4))
        );
    }

    #[test]
    fn author_less_rows_group_under_no_author() {
        let a = Some(Uuid::new_v4());
        let out = group(
            vec![],
            vec![
                edit(1, at(9, 0, 0), None),
                edit(2, at(9, 1, 0), a),
                edit(3, at(9, 2, 0), None),
            ],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].author, None);
        assert_eq!((out[0].start_rev, out[0].end_rev), (Some(1), Some(3)));
        assert_eq!(out[0].kind, EntryKind::Session);
        assert_eq!(out[1].author, a);
        assert_eq!((out[1].start_rev, out[1].end_rev), (Some(2), Some(2)));
    }

    #[test]
    fn entry_kinds_use_their_sql_spellings() {
        let all = [
            (EntryKind::Session, "session"),
            (EntryKind::Restore, "restore"),
            (EntryKind::Copy, "copy"),
            (EntryKind::Import, "import"),
            (EntryKind::Template, "template"),
            (EntryKind::Erase, "erase"),
            (EntryKind::Baseline, "baseline"),
            (EntryKind::Gap, "gap"),
        ];
        for (kind, sql) in all {
            assert_eq!(kind.as_str(), sql);
            assert_eq!(sql.parse::<EntryKind>().unwrap(), kind);
        }
        assert!("periodic".parse::<EntryKind>().is_err());
    }
}
