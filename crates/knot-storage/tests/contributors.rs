//! Doc byline: creator + everyone who changed the page content.

use chrono::{DateTime, TimeZone, Utc};
use knot_storage::{
    Contributor, ContributorStore, DocStore, PgContributorStore, PgDocStore, PgUpdatesStore,
    PgUserStore, PgWorkspaceStore, UpdatesStore, UserStore, WorkspaceRole, WorkspaceStore,
};
use uuid::Uuid;

struct Fx {
    pool: sqlx::PgPool,
    ws: Uuid,
    doc: knot_storage::Document,
    /// (id, display_name) of the doc's creator.
    alice: (Uuid, String),
}

async fn fixture() -> Fx {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let alice = add_user(&pool, ws.id, "alice@x.test", "Alice").await;
    let doc = PgDocStore::new(pool.clone())
        .create(ws.id, None, "D", "m", alice)
        .await
        .unwrap();
    Fx {
        pool,
        ws: ws.id,
        doc,
        alice: (alice, "Alice".into()),
    }
}

async fn add_user(pool: &sqlx::PgPool, ws: Uuid, email: &str, name: &str) -> Uuid {
    let u = PgUserStore::new(pool.clone())
        .create_local(email, name, "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws, u.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    u.id
}

/// Record a content change by `user` the way the room writer does.
async fn edit(pool: &sqlx::PgPool, doc: Uuid, user: Uuid) {
    PgUpdatesStore::new(pool.clone())
        .insert_batch(doc, &[(Some(user), vec![0u8])])
        .await
        .unwrap();
}

/// Pin a contributor's timestamps so ordering is deterministic.
async fn set_times(
    pool: &sqlx::PgPool,
    doc: Uuid,
    user: Uuid,
    first: DateTime<Utc>,
    last: DateTime<Utc>,
) {
    let n = sqlx::query(
        "UPDATE doc_contributors SET first_edited_at = $3, last_edited_at = $4
         WHERE doc_id = $1 AND user_id = $2",
    )
    .bind(doc)
    .bind(user)
    .bind(first)
    .bind(last)
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1, "fixture: {user} is not a contributor");
}

fn at(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 3, h, 0, 0).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn byline_lists_creator_and_contributors_most_recent_first() {
    let fx = fixture().await;
    let bob = add_user(&fx.pool, fx.ws, "bob@x.test", "Bob").await;
    let carol = add_user(&fx.pool, fx.ws, "carol@x.test", "Carol").await;
    let dave = add_user(&fx.pool, fx.ws, "dave@x.test", "Dave").await;
    for u in [dave, bob, carol] {
        edit(&fx.pool, fx.doc.id, u).await;
    }
    // Carol edited last; Bob and Dave tie, so display_name breaks it.
    set_times(&fx.pool, fx.doc.id, bob, at(9), at(11)).await;
    set_times(&fx.pool, fx.doc.id, carol, at(10), at(12)).await;
    set_times(&fx.pool, fx.doc.id, dave, at(8), at(11)).await;

    let b = PgContributorStore::new(fx.pool.clone())
        .byline(fx.doc.id)
        .await
        .unwrap()
        .expect("doc exists");

    assert_eq!((b.created_by.id, b.created_by.display_name), fx.alice);
    assert_eq!(b.created_at, fx.doc.created_at);
    assert_eq!(
        b.contributors_since, fx.doc.created_at,
        "a doc created after tracking began is tracked from its creation"
    );
    let c = |user_id: Uuid, name: &str, first, last| Contributor {
        user_id,
        display_name: name.into(),
        first_edited_at: first,
        last_edited_at: last,
    };
    assert_eq!(
        b.contributors,
        vec![
            c(carol, "Carol", at(10), at(12)),
            c(bob, "Bob", at(9), at(11)),
            c(dave, "Dave", at(8), at(11)),
        ],
        "ordered by last_edited_at DESC, then display_name ASC"
    );
}

/// Credit outlives membership: someone removed from the workspace still
/// wrote what they wrote, and the byline still names them.
#[tokio::test(flavor = "multi_thread")]
async fn byline_resolves_a_removed_member_by_name() {
    let fx = fixture().await;
    let bob = add_user(&fx.pool, fx.ws, "bob@x.test", "Bob").await;
    edit(&fx.pool, fx.doc.id, bob).await;
    PgWorkspaceStore::new(fx.pool.clone())
        .remove_member(fx.ws, bob)
        .await
        .unwrap();

    let b = PgContributorStore::new(fx.pool.clone())
        .byline(fx.doc.id)
        .await
        .unwrap()
        .expect("doc exists");
    let names: Vec<_> = b
        .contributors
        .iter()
        .map(|c| (c.user_id, c.display_name.as_str()))
        .collect();
    assert_eq!(names, vec![(bob, "Bob")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn byline_of_a_doc_nobody_edited_has_no_contributors() {
    let fx = fixture().await;
    let b = PgContributorStore::new(fx.pool.clone())
        .byline(fx.doc.id)
        .await
        .unwrap()
        .expect("doc exists");
    assert!(b.contributors.is_empty());
    assert_eq!(b.created_by.id, fx.alice.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn byline_of_unknown_doc_is_none() {
    let fx = fixture().await;
    let b = PgContributorStore::new(fx.pool.clone())
        .byline(Uuid::new_v4())
        .await
        .unwrap();
    assert!(b.is_none());
}
