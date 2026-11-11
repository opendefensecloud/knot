//! Who a history author is, as readers may see them: the one place that
//! turns a stored user id into the name and key a history route returns
//! (architecture, `former_member` → READ PATH).
//!
//! History tables keep user ids only and never rewrite them. Whether a
//! person is shown is decided here at read time, by one SQL expression,
//! [`ANONYMIZED_SQL`], used by every history read (the entries list and the
//! contributors API). An anonymized person ("Former member", M4) comes back
//! with no id and no name, so a reader cannot re-identify them by joining on
//! the id that comments and suggestions still carry. Until M4 adds
//! `users.history_anonymized_at`, nobody is anonymized.

use std::collections::HashMap;

use knot_storage::DocByline;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// Whether the person in `users u` is anonymized in history. M4 changes it
/// to `u.history_anonymized_at IS NOT NULL`; the column does not exist
/// before M4.
pub const ANONYMIZED_SQL: &str = "false";

/// A history author as loaded from `users`. Never sent as is: it carries
/// the id and the name also when the person is anonymized.
#[derive(Debug, Clone)]
pub struct HistoryPerson {
    pub id: Uuid,
    pub display_name: String,
    pub anonymized: bool,
}

/// How a history route shows an author: `key` (the user id, which the web
/// client uses for the avatar's colour) and `name`, both `null` for a
/// former member. An entry without an author has no `AuthorView` at all;
/// it is JSON `null`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuthorView {
    pub key: Option<Uuid>,
    pub name: Option<String>,
    pub former: bool,
}

/// How `person` is shown on every history route: by id and name, or, when
/// anonymized, as a former member with neither.
pub fn history_author_view(person: &HistoryPerson) -> AuthorView {
    if person.anonymized {
        AuthorView {
            key: None,
            name: None,
            former: true,
        }
    } else {
        AuthorView {
            key: Some(person.id),
            name: Some(person.display_name.clone()),
            former: false,
        }
    }
}

/// The people named by `ids`, keyed by id. An id with no `users` row is
/// absent from the map.
pub async fn load_people(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, HistoryPerson>, sqlx::Error> {
    let sql = format!(
        "SELECT u.id, u.display_name, {ANONYMIZED_SQL} AS anonymized
         FROM users u WHERE u.id = ANY($1)"
    );
    let rows = sqlx::query_as::<_, (Uuid, String, bool)>(sqlx::AssertSqlSafe(sql))
        .bind(ids)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, display_name, anonymized)| {
            (
                id,
                HistoryPerson {
                    id,
                    display_name,
                    anonymized,
                },
            )
        })
        .collect())
}

/// The `GET /api/docs/{id}/contributors` body for `b`: the byline's order
/// and times, with every person (the creator and each contributor) shown
/// through [`history_author_view`]. A person missing from `people` (their
/// user row was deleted between the two reads) keeps the byline's own id
/// and name, `former: false`.
pub fn byline_view(b: &DocByline, people: &HashMap<Uuid, HistoryPerson>) -> serde_json::Value {
    let creator = shown(b.created_by.id, &b.created_by.display_name, people);
    let contributors: Vec<serde_json::Value> = b
        .contributors
        .iter()
        .map(|c| {
            let v = shown(c.user_id, &c.display_name, people);
            serde_json::json!({
                "user_id": v.key,
                "display_name": v.name,
                "former": v.former,
                "first_edited_at": c.first_edited_at,
                "last_edited_at": c.last_edited_at,
            })
        })
        .collect();
    serde_json::json!({
        "created_by": {
            "id": creator.key,
            "display_name": creator.name,
            "former": creator.former,
        },
        "created_at": b.created_at,
        "contributors_since": b.contributors_since,
        "contributors": contributors,
    })
}

fn shown(id: Uuid, byline_name: &str, people: &HashMap<Uuid, HistoryPerson>) -> AuthorView {
    match people.get(&id) {
        Some(p) => history_author_view(p),
        None => AuthorView {
            key: Some(id),
            name: Some(byline_name.to_owned()),
            former: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use knot_storage::{Contributor, PersonRef};
    use serde_json::json;

    use super::*;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 3, h, 0, 0).unwrap()
    }

    fn person(id: Uuid, name: &str, anonymized: bool) -> HistoryPerson {
        HistoryPerson {
            id,
            display_name: name.into(),
            anonymized,
        }
    }

    fn contributor(id: Uuid, name: &str, first: u32, last: u32) -> Contributor {
        Contributor {
            user_id: id,
            display_name: name.into(),
            first_edited_at: at(first),
            last_edited_at: at(last),
        }
    }

    #[test]
    fn a_former_member_has_no_id_or_name() {
        let p = person(Uuid::new_v4(), "Ann", true);
        assert_eq!(
            history_author_view(&p),
            AuthorView {
                key: None,
                name: None,
                former: true
            }
        );
        assert_eq!(
            serde_json::to_value(history_author_view(&p)).unwrap(),
            json!({"key": null, "name": null, "former": true})
        );
    }

    #[test]
    fn a_member_is_shown_by_id_and_name() {
        let id = Uuid::new_v4();
        let p = person(id, "Ann", false);
        assert_eq!(
            history_author_view(&p),
            AuthorView {
                key: Some(id),
                name: Some("Ann".into()),
                former: false
            }
        );
        assert_eq!(
            serde_json::to_value(history_author_view(&p)).unwrap(),
            json!({"key": id, "name": "Ann", "former": false})
        );
    }

    #[test]
    fn the_byline_hides_former_members() {
        let (c, d, m) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let byline = DocByline {
            created_by: PersonRef {
                id: c,
                display_name: "Ann".into(),
            },
            created_at: at(8),
            contributors_since: at(8),
            contributors: vec![
                contributor(c, "Ann", 9, 13),
                contributor(d, "Dan", 10, 12),
                contributor(m, "Max", 9, 11),
            ],
        };
        let people: HashMap<Uuid, HistoryPerson> = [
            person(c, "Ann", true),
            person(d, "Dan", true),
            person(m, "Max", false),
        ]
        .into_iter()
        .map(|p| (p.id, p))
        .collect();

        let v = byline_view(&byline, &people);
        assert_eq!(
            v["created_by"],
            json!({"id": null, "display_name": null, "former": true})
        );
        assert_eq!(v["created_at"], json!(at(8)));
        assert_eq!(v["contributors_since"], json!(at(8)));
        for i in [0, 1] {
            assert_eq!(v["contributors"][i]["user_id"], json!(null));
            assert_eq!(v["contributors"][i]["display_name"], json!(null));
            assert_eq!(v["contributors"][i]["former"], json!(true));
        }
        // Order and times are the byline's: a former member keeps their
        // place and their edit times.
        assert_eq!(v["contributors"][0]["first_edited_at"], json!(at(9)));
        assert_eq!(v["contributors"][0]["last_edited_at"], json!(at(13)));
        assert_eq!(v["contributors"][1]["last_edited_at"], json!(at(12)));
        assert_eq!(
            v["contributors"][2],
            json!({
                "user_id": m,
                "display_name": "Max",
                "former": false,
                "first_edited_at": at(9),
                "last_edited_at": at(11),
            })
        );
        assert_eq!(v["contributors"].as_array().unwrap().len(), 3);
        let s = v.to_string();
        assert!(
            !s.contains(&c.to_string()) && !s.contains(&d.to_string()),
            "a former member's id leaked: {s}"
        );
        assert!(
            !s.contains("Ann") && !s.contains("Dan"),
            "a former member's name leaked: {s}"
        );
    }

    /// A user row deleted between the byline read and `load_people` is not
    /// in `people`; the byline still has the id and name it read.
    #[test]
    fn a_person_missing_from_people_keeps_the_bylines_name() {
        let (c, m) = (Uuid::new_v4(), Uuid::new_v4());
        let byline = DocByline {
            created_by: PersonRef {
                id: c,
                display_name: "Cleo".into(),
            },
            created_at: at(8),
            contributors_since: at(8),
            contributors: vec![contributor(m, "Max", 9, 11)],
        };
        let v = byline_view(&byline, &HashMap::new());
        assert_eq!(
            v,
            json!({
                "created_by": {"id": c, "display_name": "Cleo", "former": false},
                "created_at": at(8),
                "contributors_since": at(8),
                "contributors": [{
                    "user_id": m,
                    "display_name": "Max",
                    "former": false,
                    "first_edited_at": at(9),
                    "last_edited_at": at(11),
                }],
            })
        );
    }

    #[tokio::test]
    async fn load_people_reads_names_and_skips_unknown_ids() {
        let db = knot_test_support::fresh_db().await;
        let mut ids = Vec::new();
        for (email, name) in [("ann@people.test", "Ann"), ("max@people.test", "Max")] {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO users (email, display_name) VALUES ($1, $2) RETURNING id",
            )
            .bind(email)
            .bind(name)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            ids.push(id);
        }
        let unknown = Uuid::new_v4();

        let people = load_people(&db.pool, &[ids[0], ids[1], unknown, ids[0]])
            .await
            .unwrap();
        assert_eq!(people.len(), 2);
        assert!(!people.contains_key(&unknown));
        for (id, name) in [(ids[0], "Ann"), (ids[1], "Max")] {
            let p = &people[&id];
            assert_eq!(
                (p.id, p.display_name.as_str(), p.anonymized),
                (id, name, false)
            );
        }
        assert!(load_people(&db.pool, &[]).await.unwrap().is_empty());
    }
}
