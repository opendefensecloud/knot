//! How the page audit log shows its rows: each event's actor, actor kind
//! and whitelisted `details`, and the stats. Pure functions over the rows
//! the handler read, the people it loaded and the export roots the reader
//! may open.
//!
//! Raw `data` is never sent: each action's `details` copy only the keys
//! listed for it, so the operator fields (`doc_count`, `export_id`,
//! `fallbacks`, `source_id`) and any future key stay on the server. Every
//! person is shown through `history_author_view`; an id with no `users`
//! row is a former member, its id not echoed.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use knot_history::{AuthorView, HistoryPerson, history_author_view};
use knot_storage::audit::{DocAuditRow, DocAuditStats};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{EXPORT_ACTION, EventTokens, category_of};

/// The exporters `stats.exports.by_person` lists, most recent first.
pub const MAX_LISTED_EXPORTERS: usize = 50;

/// Grant actions, whose `data.principal` may name a person.
const GRANT_ACTIONS: &[&str] = &["doc.grant", "doc.grant.delete", "admin.grant"];

const KNOWN_FORMATS: &[&str] = &["docx", "odt", "markdown_zip"];
const KNOWN_SCOPES: &[&str] = &["page", "subtree", "workspace"];

/// A former member, a deleted user, or an anonymized person: no key, no
/// name.
fn former() -> AuthorView {
    AuthorView {
        key: None,
        name: None,
        former: true,
    }
}

/// How `person_id` is shown: by `history_author_view`, or as a former
/// member when there is no `users` row for it.
fn person_view(person_id: Uuid, people: &HashMap<Uuid, HistoryPerson>) -> AuthorView {
    people
        .get(&person_id)
        .map_or_else(former, history_author_view)
}

/// Who did it, as a label for the reader: `user` when a person did; for a
/// row without an actor, `admin_cli` (the break-glass grant), `retention`
/// (the archive's purge job) or `system`.
pub fn actor_kind(action: &str, actor_id: Option<Uuid>) -> &'static str {
    match (actor_id, action) {
        (Some(_), _) => "user",
        (None, "admin.grant") => "admin_cli",
        (None, "doc.purge") => "retention",
        (None, _) => "system",
    }
}

/// A grant's principal as stored (`user:<uuid>` or `group:<name>`).
enum Principal<'text> {
    User(Uuid),
    Group(&'text str),
}

fn parse_principal(text: &str) -> Option<Principal<'_>> {
    if let Some(user) = text.strip_prefix("user:") {
        return Uuid::parse_str(user).ok().map(Principal::User);
    }
    text.strip_prefix("group:")
        .filter(|name| !name.is_empty())
        .map(Principal::Group)
}

fn principal_of(row: &DocAuditRow) -> Option<Principal<'_>> {
    row.data
        .get("principal")
        .and_then(Value::as_str)
        .and_then(parse_principal)
}

/// The root a subtree export row names, when it is not the page itself.
pub fn subtree_root(row: &DocAuditRow, doc_id: Uuid) -> Option<Uuid> {
    if row.action != EXPORT_ACTION || string_field(&row.data, "scope") != Some("subtree") {
        return None;
    }
    string_field(&row.data, "root_id")
        .and_then(|text| Uuid::parse_str(text).ok())
        .filter(|root_id| *root_id != doc_id)
}

fn string_field<'data>(data: &'data Value, key: &str) -> Option<&'data str> {
    data.get(key).and_then(Value::as_str)
}

/// Every person the response may show: the events' actors, the people the
/// grant rows name, the exporters and the last exporter.
pub fn referenced_people(rows: &[DocAuditRow], stats: Option<&DocAuditStats>) -> Vec<Uuid> {
    let mut person_ids: Vec<Uuid> = Vec::new();
    for row in rows {
        person_ids.extend(row.actor_id);
        if GRANT_ACTIONS.contains(&row.action.as_str())
            && let Some(Principal::User(user_id)) = principal_of(row)
        {
            person_ids.push(user_id);
        }
    }
    if let Some(stats) = stats {
        person_ids.extend(stats.exporters.iter().map(|exporter| exporter.actor_id));
        person_ids.extend(
            stats
                .last_export
                .as_ref()
                .and_then(|last_export| last_export.actor_id),
        );
    }
    person_ids.sort();
    person_ids.dedup();
    person_ids
}

fn principal_view(principal: Principal<'_>, people: &HashMap<Uuid, HistoryPerson>) -> Value {
    match principal {
        Principal::User(user_id) => json!({"kind": "user", "person": person_view(user_id, people)}),
        Principal::Group(name) => json!({"kind": "group", "name": name}),
    }
}

/// Copies `key` from `data` into `details` when it holds a value `keep`
/// accepts.
fn copy_field(details: &mut Map<String, Value>, data: &Value, key: &str, keep: fn(&Value) -> bool) {
    if let Some(value) = data.get(key).filter(|value| keep(value)) {
        details.insert(key.to_owned(), value.clone());
    }
}

/// The `details` of `row`, whitelisted per action. `roots` holds the titles
/// of the subtree export roots the reader may open.
pub fn details(
    row: &DocAuditRow,
    doc_id: Uuid,
    people: &HashMap<Uuid, HistoryPerson>,
    roots: &HashMap<Uuid, String>,
) -> Value {
    let data = &row.data;
    let mut details = Map::new();
    match row.action.as_str() {
        EXPORT_ACTION => {
            copy_field(&mut details, data, "format", Value::is_string);
            copy_field(&mut details, data, "scope", Value::is_string);
            copy_field(&mut details, data, "diagram_pictures", Value::is_u64);
            if string_field(data, "scope") == Some("subtree") {
                let root_id =
                    string_field(data, "root_id").and_then(|text| Uuid::parse_str(text).ok());
                // On the root itself the row says nothing more; elsewhere it
                // names the root when the reader may open it.
                if root_id != Some(doc_id) {
                    let shown_root = root_id
                        .and_then(|root_id| roots.get(&root_id).map(|title| (root_id, title)));
                    match shown_root {
                        Some((root_id, title)) => {
                            details.insert("root".into(), json!({"id": root_id, "title": title}));
                            details.insert("root_hidden".into(), json!(false));
                        }
                        None => {
                            details.insert("root".into(), Value::Null);
                            details.insert("root_hidden".into(), json!(true));
                        }
                    }
                }
            }
        }
        "doc.grant" | "admin.grant" => {
            if let Some(principal) = principal_of(row) {
                details.insert("principal".into(), principal_view(principal, people));
            }
            copy_field(&mut details, data, "role", Value::is_string);
            copy_field(&mut details, data, "inherit", Value::is_boolean);
            if row.action == "admin.grant" {
                details.insert("break_glass".into(), json!(true));
            }
        }
        "doc.grant.delete" => {
            if let Some(principal) = principal_of(row) {
                details.insert("principal".into(), principal_view(principal, people));
            }
        }
        _ => {}
    }
    Value::Object(details)
}

/// One event as the log sends it, its id sealed by `tokens`.
pub fn event_view(
    row: &DocAuditRow,
    doc_id: Uuid,
    tokens: &EventTokens,
    people: &HashMap<Uuid, HistoryPerson>,
    roots: &HashMap<Uuid, String>,
) -> Value {
    json!({
        "id": tokens.seal(row.id),
        "at": row.created_at,
        "action": row.action,
        "category": category_of(&row.action),
        "actor_kind": actor_kind(&row.action, row.actor_id),
        "actor": row.actor_id.map(|actor_id| person_view(actor_id, people)),
        "details": details(row, doc_id, people, roots),
    })
}

/// One `by_person` row.
struct ExporterView {
    actor: AuthorView,
    count: i64,
    last_at: DateTime<Utc>,
}

/// The `stats` object of the first page.
pub fn stats_view(stats: &DocAuditStats, people: &HashMap<Uuid, HistoryPerson>) -> Value {
    let total: i64 = stats.by_action.iter().map(|action| action.count).sum();
    let by_action: Vec<Value> = stats
        .by_action
        .iter()
        .map(|action| {
            json!({"action": action.action, "count": action.count, "last_at": action.last_at})
        })
        .collect();

    let mut exports_total = 0;
    let mut exports_last_30_days = 0;
    let mut by_format = Map::new();
    let mut by_scope = Map::new();
    for group in &stats.export_groups {
        exports_total += group.count;
        exports_last_30_days += group.last_30_days;
        add_count(
            &mut by_format,
            group.format.as_deref(),
            KNOWN_FORMATS,
            group.count,
        );
        add_count(
            &mut by_scope,
            group.scope.as_deref(),
            KNOWN_SCOPES,
            group.count,
        );
    }

    // Every former, anonymized or missing person in one row, so the number
    // of them doesn't show.
    let mut named_exporters: Vec<ExporterView> = Vec::new();
    let mut former_exporters: Option<ExporterView> = None;
    for exporter in &stats.exporters {
        let actor = person_view(exporter.actor_id, people);
        if actor.former {
            let merged = former_exporters.get_or_insert(ExporterView {
                actor,
                count: 0,
                last_at: exporter.last_at,
            });
            merged.count += exporter.count;
            merged.last_at = merged.last_at.max(exporter.last_at);
        } else {
            named_exporters.push(ExporterView {
                actor,
                count: exporter.count,
                last_at: exporter.last_at,
            });
        }
    }
    let people_count = named_exporters.len() as i64 + i64::from(former_exporters.is_some());
    let mut by_person = named_exporters;
    by_person.extend(former_exporters);
    by_person.sort_by_key(|exporter| std::cmp::Reverse(exporter.last_at));
    let by_person_truncated = by_person.len() > MAX_LISTED_EXPORTERS;
    by_person.truncate(MAX_LISTED_EXPORTERS);
    let by_person: Vec<Value> = by_person
        .into_iter()
        .map(|exporter| {
            json!({"actor": exporter.actor, "count": exporter.count, "last_at": exporter.last_at})
        })
        .collect();

    let last = stats.last_export.as_ref().map(|last_export| {
        json!({
            "at": last_export.created_at,
            "actor_kind": actor_kind(&last_export.action, last_export.actor_id),
            "actor": last_export.actor_id.map(|actor_id| person_view(actor_id, people)),
            "format": string_field(&last_export.data, "format"),
            "scope": string_field(&last_export.data, "scope"),
        })
    });

    json!({
        "total": total,
        "by_action": by_action,
        "exports": {
            "total": exports_total,
            "people": people_count,
            "last_30_days": exports_last_30_days,
            "last": last,
            "by_format": by_format,
            "by_scope": by_scope,
            "by_person": by_person,
            "by_person_truncated": by_person_truncated,
        },
    })
}

/// Adds `count` under `key` when it is one of `known` and not zero.
fn add_count(counts: &mut Map<String, Value>, key: Option<&str>, known: &[&str], count: i64) {
    let Some(key) = key.filter(|key| known.contains(key)) else {
        return;
    };
    if count == 0 {
        return;
    }
    let previous = counts.get(key).and_then(Value::as_i64).unwrap_or(0);
    counts.insert(key.to_owned(), json!(previous + count));
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use knot_storage::audit::{ActionCount, ExportGroup, ExporterCount};

    use super::*;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, hour, 0, 0).unwrap()
    }

    fn row(action: &str, actor_id: Option<Uuid>, data: Value) -> DocAuditRow {
        DocAuditRow {
            id: 7,
            actor_id,
            action: action.into(),
            data,
            created_at: at(9),
        }
    }

    fn person(id: Uuid, name: &str) -> HistoryPerson {
        HistoryPerson {
            id,
            display_name: name.into(),
            anonymized: false,
        }
    }

    fn people_of(persons: &[HistoryPerson]) -> HashMap<Uuid, HistoryPerson> {
        persons.iter().map(|p| (p.id, p.clone())).collect()
    }

    #[test]
    fn actor_kind_names_who_acted() {
        let someone = Some(Uuid::new_v4());
        assert_eq!(actor_kind("admin.grant", someone), "user");
        assert_eq!(actor_kind("doc.purge", someone), "user");
        assert_eq!(actor_kind("admin.grant", None), "admin_cli");
        assert_eq!(actor_kind("doc.purge", None), "retention");
        assert_eq!(actor_kind("doc.export", None), "system");
        assert_eq!(actor_kind("doc.create", None), "system");
    }

    #[test]
    fn export_details_keep_only_the_listed_keys() {
        let doc_id = Uuid::new_v4();
        let data = json!({
            "format": "docx", "scope": "page", "diagram_pictures": 2,
            "fallbacks": {"diagram": 1}, "export_id": "x", "doc_count": 4,
            "secret": "no",
        });
        let details = details(
            &row("doc.export", None, data),
            doc_id,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(
            details,
            json!({"format": "docx", "scope": "page", "diagram_pictures": 2})
        );
    }

    #[test]
    fn a_subtree_row_names_a_readable_root_and_hides_the_rest() {
        let (doc_id, root_id, hidden_root_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let roots = HashMap::from([(root_id, "Handbook".to_string())]);
        let subtree = |root: &str| {
            row(
                "doc.export",
                None,
                json!({"format": "markdown_zip", "scope": "subtree", "root_id": root,
                       "export_id": "e", "doc_count": 3}),
            )
        };
        assert_eq!(
            details(
                &subtree(&doc_id.to_string()),
                doc_id,
                &HashMap::new(),
                &roots
            ),
            json!({"format": "markdown_zip", "scope": "subtree"}),
            "on the root itself"
        );
        assert_eq!(
            details(
                &subtree(&root_id.to_string()),
                doc_id,
                &HashMap::new(),
                &roots
            ),
            json!({"format": "markdown_zip", "scope": "subtree",
                   "root": {"id": root_id, "title": "Handbook"}, "root_hidden": false})
        );
        for hidden in [hidden_root_id.to_string(), "garbage".to_string()] {
            let shown = details(&subtree(&hidden), doc_id, &HashMap::new(), &roots);
            assert_eq!(
                shown,
                json!({"format": "markdown_zip", "scope": "subtree",
                       "root": null, "root_hidden": true})
            );
            assert!(!shown.to_string().contains(&hidden_root_id.to_string()));
        }
        assert_eq!(
            subtree_root(&subtree(&root_id.to_string()), doc_id),
            Some(root_id)
        );
        assert_eq!(subtree_root(&subtree(&doc_id.to_string()), doc_id), None);
    }

    #[test]
    fn grant_details_name_the_principal() {
        let doc_id = Uuid::new_v4();
        let (cy, gone) = (Uuid::new_v4(), Uuid::new_v4());
        let people = people_of(&[person(cy, "Cy")]);
        let grant = row(
            "doc.grant",
            Some(Uuid::new_v4()),
            json!({"principal": format!("user:{cy}"), "role": "editor", "inherit": true,
                   "extra": 1}),
        );
        assert_eq!(
            details(&grant, doc_id, &people, &HashMap::new()),
            json!({"principal": {"kind": "user",
                                 "person": {"key": cy, "name": "Cy", "former": false}},
                   "role": "editor", "inherit": true})
        );
        // An old row has no data; a principal with no user row is former.
        assert_eq!(
            details(
                &row("doc.grant", None, json!({})),
                doc_id,
                &people,
                &HashMap::new()
            ),
            json!({})
        );
        let removed = details(
            &row(
                "doc.grant.delete",
                None,
                json!({"principal": format!("user:{gone}"), "role": "owner"}),
            ),
            doc_id,
            &people,
            &HashMap::new(),
        );
        assert_eq!(
            removed,
            json!({"principal": {"kind": "user",
                                 "person": {"key": null, "name": null, "former": true}}})
        );
        assert!(!removed.to_string().contains(&gone.to_string()));
        assert_eq!(
            details(
                &row(
                    "admin.grant",
                    None,
                    json!({"break_glass": true, "principal": "group:ops", "role": "owner",
                           "inherit": false}),
                ),
                doc_id,
                &people,
                &HashMap::new()
            ),
            json!({"principal": {"kind": "group", "name": "ops"}, "role": "owner",
                   "inherit": false, "break_glass": true})
        );
        assert_eq!(
            details(
                &row("doc.grant", None, json!({"principal": "user:nope"})),
                doc_id,
                &people,
                &HashMap::new()
            ),
            json!({}),
            "an unparsable principal is left out"
        );
    }

    #[test]
    fn other_actions_have_no_details() {
        let doc_id = Uuid::new_v4();
        for action in ["doc.duplicate", "doc.rename", "doc.someday"] {
            assert_eq!(
                details(
                    &row(action, None, json!({"source_id": "s", "title": "t"})),
                    doc_id,
                    &HashMap::new(),
                    &HashMap::new()
                ),
                json!({}),
                "{action}"
            );
        }
    }

    #[test]
    fn former_exporters_merge_into_one_row() {
        let (ada, gone_one, gone_two) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let people = people_of(&[person(ada, "Ada")]);
        let stats = DocAuditStats {
            by_action: vec![
                ActionCount {
                    action: "doc.export".into(),
                    count: 6,
                    last_at: at(12),
                },
                ActionCount {
                    action: "doc.create".into(),
                    count: 1,
                    last_at: at(1),
                },
            ],
            export_groups: vec![
                ExportGroup {
                    format: Some("docx".into()),
                    scope: Some("page".into()),
                    count: 3,
                    last_30_days: 2,
                },
                ExportGroup {
                    format: Some("markdown_zip".into()),
                    scope: Some("subtree".into()),
                    count: 2,
                    last_30_days: 2,
                },
                ExportGroup {
                    format: None,
                    scope: None,
                    count: 1,
                    last_30_days: 0,
                },
            ],
            exporters: vec![
                ExporterCount {
                    actor_id: gone_one,
                    count: 2,
                    last_at: at(12),
                },
                ExporterCount {
                    actor_id: ada,
                    count: 3,
                    last_at: at(10),
                },
                ExporterCount {
                    actor_id: gone_two,
                    count: 1,
                    last_at: at(8),
                },
            ],
            last_export: Some(row(
                "doc.export",
                Some(gone_one),
                json!({"format": "docx", "scope": "page", "export_id": "e"}),
            )),
        };
        let view = stats_view(&stats, &people);
        assert_eq!(view["total"], 7);
        let exports = &view["exports"];
        assert_eq!(exports["total"], 6);
        assert_eq!(exports["last_30_days"], 4);
        assert_eq!(exports["people"], 2);
        assert_eq!(exports["by_format"], json!({"docx": 3, "markdown_zip": 2}));
        assert_eq!(exports["by_scope"], json!({"page": 3, "subtree": 2}));
        assert_eq!(
            exports["by_person"],
            json!([
                {"actor": {"key": null, "name": null, "former": true}, "count": 3, "last_at": at(12)},
                {"actor": {"key": ada, "name": "Ada", "former": false}, "count": 3, "last_at": at(10)},
            ])
        );
        assert_eq!(exports["by_person_truncated"], false);
        assert_eq!(
            exports["last"],
            json!({"at": at(9), "actor_kind": "user",
                   "actor": {"key": null, "name": null, "former": true},
                   "format": "docx", "scope": "page"})
        );
        let text = view.to_string();
        for hidden in [gone_one, gone_two] {
            assert!(!text.contains(&hidden.to_string()), "{text}");
        }
        assert!(!text.contains("export_id"), "{text}");
    }

    #[test]
    fn by_person_is_capped_at_fifty() {
        let persons: Vec<HistoryPerson> = (0..51)
            .map(|index| person(Uuid::new_v4(), &format!("P{index}")))
            .collect();
        let stats = DocAuditStats {
            exporters: persons
                .iter()
                .enumerate()
                .map(|(index, p)| ExporterCount {
                    actor_id: p.id,
                    count: 1,
                    last_at: at(0) + chrono::Duration::minutes(index as i64),
                })
                .collect(),
            ..DocAuditStats::default()
        };
        let view = stats_view(&stats, &people_of(&persons));
        assert_eq!(view["exports"]["people"], 51);
        assert_eq!(view["exports"]["by_person"].as_array().unwrap().len(), 50);
        assert_eq!(view["exports"]["by_person_truncated"], true);
        assert_eq!(view["exports"]["by_person"][0]["actor"]["name"], "P50");
        assert_eq!(view["exports"]["last"], Value::Null);
    }
}
