//! The entries list: one page of a page's history, newest first, for
//! `GET /api/docs/{id}/history/entries` (architecture, READ API).
//!
//! The list has two sections. The log section holds every entry built from
//! the log (`kind <> 'baseline'`), newest `end_rev` first; the baseline
//! section ("Earlier versions") holds the pre-rollout `baseline` entries,
//! newest `ended_at` first, after the whole log section. A [`Cursor`] is a
//! keyset position in one of them, opaque to clients.
//!
//! Stored `meta` is never returned as is: [`render_page`] rebuilds each
//! entry's `meta` from its kind's allowlist, shows authors through
//! [`history_author_view`], and hides a copy's or a template's source page
//! unless the reader may read it.

use std::collections::{HashMap, HashSet};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

use crate::people::{HistoryPerson, history_author_view};
use crate::sessions::EntryKind;

/// Entries per page when the request names no `limit`.
pub const DEFAULT_LIMIT: i64 = 50;
/// The largest `limit` a request may name.
pub const MAX_LIMIT: i64 = 200;
/// How far (in revs) the built history may trail the page's head before
/// the list reports `catching_up`.
pub const CATCHING_UP_REVS: i64 = 500;

/// A position in the list: after an entry of the log section, at the start
/// of the baseline section, or after an entry of the baseline section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    Log { end_rev: i64, id: i64 },
    BaselineStart,
    Baseline { ended_at: DateTime<Utc>, id: i64 },
}

/// A cursor string that is not one [`Cursor::encode`] wrote.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("not a history cursor")]
pub struct CursorError;

impl Cursor {
    /// URL-safe base64 (no padding) of `L.<end_rev>.<id>`, `B.start` or
    /// `B.<unix micros>.<id>`.
    pub fn encode(&self) -> String {
        let plain = match self {
            Cursor::Log { end_rev, id } => format!("L.{end_rev}.{id}"),
            Cursor::BaselineStart => "B.start".to_string(),
            Cursor::Baseline { ended_at, id } => {
                format!("B.{}.{id}", ended_at.timestamp_micros())
            }
        };
        URL_SAFE_NO_PAD.encode(plain)
    }

    pub fn decode(s: &str) -> Result<Cursor, CursorError> {
        let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| CursorError)?;
        let plain = String::from_utf8(bytes).map_err(|_| CursorError)?;
        let parts: Vec<&str> = plain.split('.').collect();
        let int = |s: &str| s.parse::<i64>().map_err(|_| CursorError);
        match parts.as_slice() {
            ["L", end_rev, id] => Ok(Cursor::Log {
                end_rev: int(end_rev)?,
                id: int(id)?,
            }),
            ["B", "start"] => Ok(Cursor::BaselineStart),
            ["B", micros, id] => Ok(Cursor::Baseline {
                ended_at: DateTime::from_timestamp_micros(int(micros)?).ok_or(CursorError)?,
                id: int(id)?,
            }),
            _ => Err(CursorError),
        }
    }
}

/// One `doc_history_entries` row as the list reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryRow {
    pub id: i64,
    pub kind: EntryKind,
    pub author_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// Builds the `Log` cursor; not rendered.
    pub end_rev: Option<i64>,
    pub sealed: bool,
    pub words_added: Option<i32>,
    pub words_removed: Option<i32>,
    pub meta: Value,
}

/// One page of the list, before rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryPage {
    pub history_since: DateTime<Utc>,
    /// The page was created at or after `history_since`: its history has
    /// no pre-rollout part.
    pub history_complete: bool,
    /// The builder has not caught up with the page: the list may be
    /// incomplete (genesis not built yet, also during a rebuild, or the
    /// built history trails the head by more than [`CATCHING_UP_REVS`]).
    pub catching_up: bool,
    /// The builder gave the page up: the list stays as it is.
    pub unavailable: bool,
    pub entries: Vec<EntryRow>,
    pub next_cursor: Option<Cursor>,
}

type Row = (
    i64,
    String,
    Option<Uuid>,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<i64>,
    bool,
    Option<i32>,
    Option<i32>,
    Value,
);

const COLUMNS: &str = "id, kind, author_id, started_at, ended_at, end_rev, sealed, \
                       words_added, words_removed, meta";

fn entry_row(r: Row) -> Result<EntryRow, sqlx::Error> {
    let kind =
        r.1.parse::<EntryKind>()
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(EntryRow {
        id: r.0,
        kind,
        author_id: r.2,
        started_at: r.3,
        ended_at: r.4,
        end_rev: r.5,
        sealed: r.6,
        words_added: r.7,
        words_removed: r.8,
        meta: r.9,
    })
}

/// One page of `doc_id`'s list after `cursor` (from the top when `None`),
/// at most `limit` entries; `None` when the page row is gone.
pub async fn list_entries(
    pool: &PgPool,
    doc_id: Uuid,
    cursor: Option<Cursor>,
    limit: i64,
) -> Result<Option<HistoryPage>, sqlx::Error> {
    // Never decodes `retry_after` (Global Constraints: 'infinity' panics
    // chrono); a page with neither a heads nor a state row is not catching
    // up.
    let status: Option<(DateTime<Utc>, bool, bool, bool)> = sqlx::query_as(
        "SELECT d.history_since,
                d.created_at >= d.history_since AS history_complete,
                coalesce(s.retry_after = 'infinity', false) AS unavailable,
                (NOT coalesce(s.retry_after = 'infinity', false)
                 AND ((s.doc_id IS NULL AND coalesce(h.dirty, false))
                      OR (s.doc_id IS NOT NULL AND s.genesis_rev IS NULL)
                      OR coalesce(s.built_rev, 0) < coalesce(h.head_rev, 0) - $2)) AS catching_up
           FROM documents d
           LEFT JOIN doc_history_state s ON s.doc_id = d.id
           LEFT JOIN doc_history_heads h ON h.doc_id = d.id
          WHERE d.id = $1",
    )
    .bind(doc_id)
    .bind(CATCHING_UP_REVS)
    .fetch_optional(pool)
    .await?;
    let Some((history_since, history_complete, unavailable, catching_up)) = status else {
        return Ok(None);
    };

    let limit = limit.max(1);
    let mut entries = Vec::new();
    let mut next_cursor = None;

    let log_section = match &cursor {
        None => Some((i64::MAX, i64::MAX)),
        Some(Cursor::Log { end_rev, id }) => Some((*end_rev, *id)),
        Some(Cursor::BaselineStart | Cursor::Baseline { .. }) => None,
    };
    if let Some((c_rev, c_id)) = log_section {
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM doc_history_entries
              WHERE doc_id = $1 AND kind <> 'baseline' AND (end_rev, id) < ($2, $3)
              ORDER BY end_rev DESC, id DESC
              LIMIT $4"
        )))
        .bind(doc_id)
        .bind(c_rev)
        .bind(c_id)
        .bind(limit + 1)
        .fetch_all(pool)
        .await?;
        for r in rows {
            entries.push(entry_row(r)?);
        }
        if entries.len() as i64 > limit {
            entries.truncate(limit as usize);
            let last = entries.last().expect("limit is at least 1");
            next_cursor = Some(Cursor::Log {
                // The log section holds only entries with an `end_rev`
                // (the table's CHECK).
                end_rev: last.end_rev.unwrap_or(i64::MIN),
                id: last.id,
            });
        }
    }

    if next_cursor.is_none() {
        // The log section is exhausted: the baseline section fills the
        // rest of the page.
        let need = limit - entries.len() as i64;
        let (c_at, c_id) = match &cursor {
            Some(Cursor::Baseline { ended_at, id }) => (Some(*ended_at), *id),
            _ => (None, i64::MAX),
        };
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM doc_history_entries
              WHERE doc_id = $1 AND kind = 'baseline'
                AND ($2::timestamptz IS NULL OR (ended_at, id) < ($2, $3))
              ORDER BY ended_at DESC, id DESC
              LIMIT $4"
        )))
        .bind(doc_id)
        .bind(c_at)
        .bind(c_id)
        .bind(need + 1)
        .fetch_all(pool)
        .await?;
        let more = rows.len() as i64 > need;
        for r in rows.into_iter().take(need as usize) {
            entries.push(entry_row(r)?);
        }
        if more {
            next_cursor = Some(if need == 0 {
                // The log section ended exactly at the page boundary: the
                // extra row is not on this page.
                Cursor::BaselineStart
            } else {
                let last = entries.last().expect("need is at least 1");
                Cursor::Baseline {
                    ended_at: last.ended_at,
                    id: last.id,
                }
            });
        }
    }

    Ok(Some(HistoryPage {
        history_since,
        history_complete,
        catching_up,
        unavailable,
        entries,
        next_cursor,
    }))
}

fn uuid_at(meta: &Value, key: &str) -> Option<Uuid> {
    meta.get(key)?.as_str()?.parse().ok()
}

fn string_at(meta: &Value, key: &str) -> Value {
    match meta.get(key) {
        Some(Value::String(s)) => Value::String(s.clone()),
        _ => Value::Null,
    }
}

/// The ids in a session's `meta.accepted_by` (unparsable ids as `None`).
fn accepted_by(meta: &Value) -> Vec<Option<Uuid>> {
    match meta.get("accepted_by") {
        Some(Value::Array(ids)) => ids
            .iter()
            .map(|v| v.as_str().and_then(|s| s.parse().ok()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The pages the entries name as their source: `meta.source_doc_id` of
/// copy entries and `meta.template_doc_id` of template entries,
/// deduplicated, in order of first appearance.
pub fn referenced_docs(entries: &[EntryRow]) -> Vec<Uuid> {
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter_map(|e| match e.kind {
            EntryKind::Copy => uuid_at(&e.meta, "source_doc_id"),
            EntryKind::Template => uuid_at(&e.meta, "template_doc_id"),
            _ => None,
        })
        .filter(|id| seen.insert(*id))
        .collect()
}

/// The people the rendered entries show: every author and every session's
/// `accepted_by`, deduplicated.
pub fn referenced_people(entries: &[EntryRow]) -> Vec<Uuid> {
    let mut seen = HashSet::new();
    entries
        .iter()
        .flat_map(|e| {
            let accepted = if e.kind == EntryKind::Session {
                accepted_by(&e.meta)
            } else {
                Vec::new()
            };
            e.author_id
                .into_iter()
                .chain(accepted.into_iter().flatten())
        })
        .filter(|id| seen.insert(*id))
        .collect()
}

fn author_json(id: Option<Uuid>, people: &HashMap<Uuid, HistoryPerson>) -> Value {
    match id.and_then(|id| people.get(&id)) {
        Some(p) => serde_json::to_value(history_author_view(p)).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// An entry's `meta` as readers see it: only its kind's allowlisted keys,
/// each rebuilt from the stored value (Global Constraints).
fn render_meta(
    e: &EntryRow,
    people: &HashMap<Uuid, HistoryPerson>,
    readable: &HashSet<Uuid>,
) -> Value {
    let m = &e.meta;
    match e.kind {
        EntryKind::Session => {
            let ids = accepted_by(m);
            if ids.is_empty() {
                json!({})
            } else {
                let views: Vec<Value> = ids.into_iter().map(|id| author_json(id, people)).collect();
                json!({ "accepted_by": views })
            }
        }
        EntryKind::Restore => json!({
            "target_ended_at": string_at(m, "target_ended_at"),
            "target_entry_id": m.get("target_entry_id").and_then(Value::as_i64),
            "fallback": m.get("fallback").and_then(Value::as_bool),
        }),
        EntryKind::Copy => match uuid_at(m, "source_doc_id") {
            Some(id) if readable.contains(&id) => json!({
                "source_doc_id": id,
                "source_title": string_at(m, "source_title"),
                "version_at": string_at(m, "version_at"),
            }),
            _ => json!({ "source_hidden": true }),
        },
        EntryKind::Template => match uuid_at(m, "template_doc_id") {
            Some(id) if readable.contains(&id) => json!({
                "template_doc_id": id,
                "template_title": string_at(m, "template_title"),
            }),
            _ => json!({ "template_hidden": true }),
        },
        EntryKind::Baseline => json!({
            "rollout": m.get("rollout").and_then(Value::as_bool).unwrap_or(false),
        }),
        EntryKind::Import => {
            let mode = match m.get("mode").and_then(Value::as_str) {
                Some(mode @ ("append" | "replace")) => Value::String(mode.to_string()),
                _ => Value::Null,
            };
            json!({ "mode": mode })
        }
        EntryKind::Gap | EntryKind::Erase => Value::Object(Map::new()),
    }
}

/// The response body of `GET /api/docs/{id}/history/entries`: `people`
/// holds the authors and accepters to show ([`referenced_people`]),
/// `readable` the referenced pages the reader may read
/// ([`referenced_docs`]).
pub fn render_page(
    page: &HistoryPage,
    people: &HashMap<Uuid, HistoryPerson>,
    readable: &HashSet<Uuid>,
) -> Value {
    let entries: Vec<Value> = page
        .entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "kind": e.kind.as_str(),
                "author": author_json(e.author_id, people),
                "started_at": e.started_at,
                "ended_at": e.ended_at,
                "open": !e.sealed,
                "words_added": e.words_added,
                "words_removed": e.words_removed,
                "meta": render_meta(e, people, readable),
            })
        })
        .collect();
    json!({
        "history_since": page.history_since,
        "history_complete": page.history_complete,
        "catching_up": page.catching_up,
        "unavailable": page.unavailable,
        "entries": entries,
        "next_cursor": page.next_cursor.as_ref().map(Cursor::encode),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        URL_SAFE_NO_PAD.encode(s)
    }

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let at = DateTime::from_timestamp_micros(1_762_600_000_123_456).unwrap();
        for c in [
            Cursor::Log { end_rev: 42, id: 7 },
            Cursor::BaselineStart,
            Cursor::Baseline {
                ended_at: at,
                id: 9,
            },
        ] {
            assert_eq!(Cursor::decode(&c.encode()), Ok(c.clone()), "{c:?}");
        }
        assert_eq!(Cursor::BaselineStart.encode(), b64("B.start"));
        assert_eq!(Cursor::Log { end_rev: 3, id: 4 }.encode(), b64("L.3.4"));
        assert!(!Cursor::Log { end_rev: 3, id: 4 }.encode().contains('='));

        let mut bad = vec!["!!".to_string(), String::new()];
        for plain in [
            "L.1",
            "Q.1.2",
            "L.a.1",
            "B.x.1",
            "B.start.1",
            "B.9223372036854775807.1",
            "B.-9223372036854775808.1",
            "L.1.2.3",
        ] {
            bad.push(b64(plain));
        }
        for s in bad {
            assert_eq!(Cursor::decode(&s), Err(CursorError), "{s:?}");
        }
    }

    fn uid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn t(minute: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_762_600_000 + minute * 60, 0).unwrap()
    }

    fn row(id: i64, kind: EntryKind, author: Option<Uuid>, meta: Value) -> EntryRow {
        EntryRow {
            id,
            kind,
            author_id: author,
            started_at: t(id),
            ended_at: t(id + 1),
            end_rev: Some(id),
            sealed: true,
            words_added: Some(1),
            words_removed: Some(0),
            meta,
        }
    }

    fn person(id: Uuid, name: &str, anonymized: bool) -> (Uuid, HistoryPerson) {
        (
            id,
            HistoryPerson {
                id,
                display_name: name.to_string(),
                anonymized,
            },
        )
    }

    #[test]
    fn render_page_rebuilds_meta_per_kind() {
        let (ann, u2, missing) = (uid(1), uid(2), uid(3));
        let (src, tpl, hidden_src, hidden_tpl) = (uid(10), uid(11), uid(12), uid(13));
        let at = "2026-11-08T10:00:00+00:00";
        let people: HashMap<Uuid, HistoryPerson> =
            [person(ann, "Ann", false), person(u2, "Zed", true)].into();
        let readable: HashSet<Uuid> = [src, tpl].into();
        let entries = vec![
            row(
                1,
                EntryKind::Session,
                Some(ann),
                json!({"accepted_by": [ann, u2], "suggestion_ids": [uid(99)],
                       "gap_credit_rev": 3, "secret": "x"}),
            ),
            row(2, EntryKind::Session, Some(ann), json!({"secret": "x"})),
            row(
                3,
                EntryKind::Restore,
                Some(ann),
                json!({"target_snapshot_seq": 7, "target_ended_at": at, "secret": "x"}),
            ),
            row(
                4,
                EntryKind::Restore,
                Some(ann),
                json!({"target_entry_id": 5, "target_rev": 9, "target_ended_at": at}),
            ),
            row(
                5,
                EntryKind::Restore,
                Some(ann),
                json!({"target_snapshot_seq": 7, "target_ended_at": at, "fallback": true}),
            ),
            row(
                6,
                EntryKind::Baseline,
                None,
                json!({"rollout": true, "secret": "x"}),
            ),
            row(7, EntryKind::Baseline, None, json!({})),
            row(
                8,
                EntryKind::Import,
                Some(ann),
                json!({"mode": "append", "secret": "x"}),
            ),
            row(9, EntryKind::Import, Some(ann), json!({})),
            row(10, EntryKind::Import, Some(ann), json!({"mode": "drop"})),
            row(11, EntryKind::Gap, None, json!({"secret": "x"})),
            row(12, EntryKind::Erase, Some(ann), json!({"secret": "x"})),
            row(
                13,
                EntryKind::Copy,
                Some(ann),
                json!({"source_doc_id": src, "source_title": "Src", "secret": "x"}),
            ),
            row(
                14,
                EntryKind::Copy,
                Some(ann),
                json!({"source_doc_id": hidden_src, "source_title": "Hidden source"}),
            ),
            row(
                15,
                EntryKind::Template,
                Some(ann),
                json!({"template_doc_id": tpl, "template_title": "Tpl", "secret": "x"}),
            ),
            row(
                16,
                EntryKind::Template,
                Some(ann),
                json!({"template_doc_id": hidden_tpl, "template_title": "Hidden template"}),
            ),
            row(17, EntryKind::Session, Some(missing), json!({})),
        ];
        let page = HistoryPage {
            history_since: t(0),
            history_complete: true,
            catching_up: false,
            unavailable: false,
            entries,
            next_cursor: Some(Cursor::Log {
                end_rev: 17,
                id: 17,
            }),
        };
        let out = render_page(&page, &people, &readable);
        let listed = out["entries"].as_array().unwrap();
        let meta = |i: usize| listed[i]["meta"].clone();
        let ann_view = json!({"key": ann, "name": "Ann", "former": false});

        assert_eq!(
            meta(0),
            json!({"accepted_by": [ann_view, {"key": null, "name": null, "former": true}]})
        );
        assert_eq!(meta(1), json!({}));
        assert_eq!(
            meta(2),
            json!({"target_ended_at": at, "target_entry_id": null, "fallback": null})
        );
        assert_eq!(
            meta(3),
            json!({"target_ended_at": at, "target_entry_id": 5, "fallback": null})
        );
        assert_eq!(meta(4)["fallback"], json!(true));
        assert_eq!(meta(5), json!({"rollout": true}));
        assert_eq!(meta(6), json!({"rollout": false}));
        assert_eq!(meta(7), json!({"mode": "append"}));
        assert_eq!(meta(8), json!({"mode": null}));
        assert_eq!(meta(9), json!({"mode": null}));
        assert_eq!(meta(10), json!({}));
        assert_eq!(meta(11), json!({}));
        assert_eq!(
            meta(12),
            json!({"source_doc_id": src, "source_title": "Src", "version_at": null})
        );
        assert_eq!(meta(13), json!({"source_hidden": true}));
        assert_eq!(
            meta(14),
            json!({"template_doc_id": tpl, "template_title": "Tpl"})
        );
        assert_eq!(meta(15), json!({"template_hidden": true}));

        // Authors: JSON null without an author or a loaded person.
        assert_eq!(listed[0]["author"], ann_view);
        assert_eq!(listed[5]["author"], Value::Null);
        assert_eq!(listed[10]["author"], Value::Null);
        assert_eq!(listed[16]["author"], Value::Null);

        // The entry shape, exactly.
        let keys: Vec<&str> = listed[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut want = vec![
            "author",
            "ended_at",
            "id",
            "kind",
            "meta",
            "open",
            "started_at",
            "words_added",
            "words_removed",
        ];
        want.sort_unstable();
        let mut got = keys.clone();
        got.sort_unstable();
        assert_eq!(got, want);
        assert_eq!(listed[0]["open"], json!(false));
        assert_eq!(
            out["next_cursor"],
            json!(
                Cursor::Log {
                    end_rev: 17,
                    id: 17
                }
                .encode()
            )
        );

        // No key outside its kind's allowlist.
        for e in listed {
            let allowed: &[&str] = match e["kind"].as_str().unwrap() {
                "session" => &["accepted_by"],
                "restore" => &["target_ended_at", "target_entry_id", "fallback"],
                "copy" => &[
                    "source_doc_id",
                    "source_title",
                    "version_at",
                    "source_hidden",
                ],
                "template" => &["template_doc_id", "template_title", "template_hidden"],
                "baseline" => &["rollout"],
                "import" => &["mode"],
                _ => &[],
            };
            for k in e["meta"].as_object().unwrap().keys() {
                assert!(allowed.contains(&k.as_str()), "{k} in {e}");
            }
        }
        let text = out.to_string();
        for leak in [
            "secret",
            "suggestion_ids",
            "gap_credit_rev",
            "target_snapshot_seq",
            "target_rev",
            "Hidden source",
            "Hidden template",
            "Zed",
        ] {
            assert!(!text.contains(leak), "{leak} leaked: {text}");
        }
        for id in [u2, hidden_src, hidden_tpl, missing, uid(99)] {
            assert!(!text.contains(&id.to_string()), "{id} leaked: {text}");
        }
    }

    #[test]
    fn referenced_docs_and_people_are_deduplicated() {
        let (a, b, s, t) = (uid(1), uid(2), uid(10), uid(11));
        let entries = vec![
            row(1, EntryKind::Copy, Some(a), json!({"source_doc_id": s})),
            row(
                2,
                EntryKind::Template,
                Some(a),
                json!({"template_doc_id": t}),
            ),
            row(3, EntryKind::Copy, Some(b), json!({"source_doc_id": s})),
            // A restore's or an import's ids are not sources.
            row(
                4,
                EntryKind::Restore,
                Some(b),
                json!({"source_doc_id": uid(12)}),
            ),
            row(
                5,
                EntryKind::Session,
                Some(b),
                json!({"accepted_by": [a, uid(3), "x"]}),
            ),
            row(6, EntryKind::Gap, None, json!({})),
        ];
        assert_eq!(referenced_docs(&entries), vec![s, t]);
        assert_eq!(referenced_people(&entries), vec![a, b, uid(3)]);
    }
}
