//! Integration tests for `PgFavoriteStore`. Uses
//! `knot_test_support::fresh_db` against the dev compose Postgres.

use knot_storage::{
    AddOutcome, DocStore, FAVORITES_LIMIT, FavoriteStore, PgDocStore, PgFavoriteStore, PgUserStore,
    PgWorkspaceStore, UserStore, WorkspaceRole, WorkspaceStore, sort_key_between,
};
use sqlx::PgPool;
use uuid::Uuid;

struct World {
    pool: PgPool,
    store: PgFavoriteStore,
    docs: PgDocStore,
    ws: Uuid,
    alice: Uuid,
    bob: Uuid,
    d1: Uuid,
    d2: Uuid,
}

/// A workspace with alice (owner) and bob (editor), and two top-level
/// pages d1, d2 created by alice.
async fn setup() -> World {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws_store = PgWorkspaceStore::new(pool.clone());
    let ws = ws_store.create("default", "W").await.unwrap();
    let users = PgUserStore::new(pool.clone());
    let alice = users
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = users
        .create_local("bob@x.test", "Bob Smith", "$h$")
        .await
        .unwrap();
    ws_store
        .add_member(ws.id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws_store
        .add_member(ws.id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let docs = PgDocStore::new(pool.clone());
    let k1 = sort_key_between(None, None);
    let k2 = sort_key_between(Some(&k1), None);
    let d1 = docs
        .create(ws.id, None, "One", &k1, alice.id)
        .await
        .unwrap();
    let d2 = docs
        .create(ws.id, None, "Two", &k2, alice.id)
        .await
        .unwrap();
    World {
        store: PgFavoriteStore::new(pool.clone()),
        docs,
        pool,
        ws: ws.id,
        alice: alice.id,
        bob: bob.id,
        d1: d1.id,
        d2: d2.id,
    }
}

async fn ids(w: &World, user: Uuid, ws: Uuid, hidden: &[Uuid]) -> Vec<Uuid> {
    w.store
        .list(user, ws, hidden)
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.doc_id)
        .collect()
}

async fn row_count(pool: &PgPool, user: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM user_favorites WHERE user_id = $1")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn sort_key_of(pool: &PgPool, user: Uuid, doc: Uuid) -> String {
    sqlx::query_scalar("SELECT sort_key FROM user_favorites WHERE user_id = $1 AND doc_id = $2")
        .bind(user)
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn add_appends_in_star_order() {
    let w = setup().await;
    assert_eq!(
        w.store.add(w.alice, w.ws, w.d2).await.unwrap(),
        AddOutcome::Added
    );
    assert_eq!(
        w.store.add(w.alice, w.ws, w.d1).await.unwrap(),
        AddOutcome::Added
    );
    assert_eq!(ids(&w, w.alice, w.ws, &[]).await, vec![w.d2, w.d1]);

    let list = w.store.list(w.alice, w.ws, &[]).await.unwrap();
    assert_eq!(list[0].title, "Two");
    assert_eq!(list[1].title, "One");
    assert!(list[0].sort_key < list[1].sort_key);
}

#[tokio::test(flavor = "multi_thread")]
async fn add_is_idempotent() {
    let w = setup().await;
    assert_eq!(
        w.store.add(w.alice, w.ws, w.d1).await.unwrap(),
        AddOutcome::Added
    );
    let key = sort_key_of(&w.pool, w.alice, w.d1).await;
    assert_eq!(
        w.store.add(w.alice, w.ws, w.d1).await.unwrap(),
        AddOutcome::AlreadyThere
    );
    assert_eq!(row_count(&w.pool, w.alice).await, 1);
    assert_eq!(sort_key_of(&w.pool, w.alice, w.d1).await, key);
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_is_idempotent() {
    let w = setup().await;
    w.store.add(w.alice, w.ws, w.d1).await.unwrap();
    assert!(w.store.remove(w.alice, w.ws, w.d1).await.unwrap());
    assert!(!w.store.remove(w.alice, w.ws, w.d1).await.unwrap());
    assert!(!w.store.remove(w.alice, w.ws, w.d2).await.unwrap());
    assert!(ids(&w, w.alice, w.ws, &[]).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_only_touches_the_callers_row() {
    let w = setup().await;
    w.store.add(w.alice, w.ws, w.d1).await.unwrap();
    w.store.add(w.bob, w.ws, w.d1).await.unwrap();
    assert!(w.store.remove(w.bob, w.ws, w.d1).await.unwrap());
    assert_eq!(ids(&w, w.alice, w.ws, &[]).await, vec![w.d1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_is_per_user_and_workspace() {
    let w = setup().await;
    w.store.add(w.alice, w.ws, w.d1).await.unwrap();
    w.store.add(w.bob, w.ws, w.d2).await.unwrap();
    assert_eq!(ids(&w, w.alice, w.ws, &[]).await, vec![w.d1]);
    assert_eq!(ids(&w, w.bob, w.ws, &[]).await, vec![w.d2]);

    let other = PgWorkspaceStore::new(w.pool.clone())
        .create("other", "O")
        .await
        .unwrap();
    assert!(ids(&w, w.alice, other.id, &[]).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn list_skips_hidden_archived_and_templates() {
    let w = setup().await;
    let k3 = sort_key_between(Some("z"), None);
    let tpl = w
        .docs
        .create(w.ws, None, "Tpl", &k3, w.alice)
        .await
        .unwrap();
    w.store.add(w.alice, w.ws, w.d1).await.unwrap();
    w.store.add(w.alice, w.ws, w.d2).await.unwrap();
    w.store.add(w.alice, w.ws, tpl.id).await.unwrap();
    w.docs
        .set_template(w.ws, tpl.id, w.alice, true)
        .await
        .unwrap();

    assert_eq!(ids(&w, w.alice, w.ws, &[]).await, vec![w.d1, w.d2]);
    assert_eq!(ids(&w, w.alice, w.ws, &[w.d1]).await, vec![w.d2]);

    w.docs.archive(w.ws, w.d2, w.alice).await.unwrap();
    assert_eq!(ids(&w, w.alice, w.ws, &[]).await, vec![w.d1]);

    w.docs.restore(w.ws, w.d2, w.alice).await.unwrap();
    assert_eq!(ids(&w, w.alice, w.ws, &[]).await, vec![w.d1, w.d2]);
    // Hidden and archived rows stay stored.
    assert_eq!(row_count(&w.pool, w.alice).await, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_cascades() {
    let w = setup().await;
    w.store.add(w.alice, w.ws, w.d1).await.unwrap();
    w.store.add(w.bob, w.ws, w.d1).await.unwrap();
    w.docs.archive(w.ws, w.d1, w.alice).await.unwrap();
    w.docs.purge(w.ws, w.d1, Some(w.alice)).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM user_favorites WHERE doc_id = $1")
        .bind(w.d1)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn limit_reached_at_200() {
    let w = setup().await;
    let n = usize::try_from(FAVORITES_LIMIT).unwrap() + 1;
    let keys = knot_storage::lexorank::spread(n);
    let mut pages = Vec::with_capacity(n);
    for (i, k) in keys.iter().enumerate() {
        // Children of d1, so their keys cannot collide with d1/d2's.
        let d = w
            .docs
            .create(w.ws, Some(w.d1), &format!("P{i}"), k, w.alice)
            .await
            .unwrap();
        pages.push(d.id);
    }
    for &p in &pages[..n - 1] {
        assert_eq!(
            w.store.add(w.alice, w.ws, p).await.unwrap(),
            AddOutcome::Added
        );
    }
    assert_eq!(
        w.store.add(w.alice, w.ws, pages[n - 1]).await.unwrap(),
        AddOutcome::LimitReached
    );
    assert_eq!(
        w.store.add(w.alice, w.ws, pages[0]).await.unwrap(),
        AddOutcome::AlreadyThere
    );
    assert_eq!(row_count(&w.pool, w.alice).await, FAVORITES_LIMIT);

    // Hidden rows still count toward the cap.
    w.store.remove(w.alice, w.ws, pages[0]).await.unwrap();
    w.docs.archive(w.ws, pages[1], w.alice).await.unwrap();
    assert_eq!(
        w.store.add(w.alice, w.ws, pages[0]).await.unwrap(),
        AddOutcome::Added
    );
    assert_eq!(
        w.store.add(w.alice, w.ws, pages[n - 1]).await.unwrap(),
        AddOutcome::LimitReached
    );

    // The cap is per user: bob is unaffected.
    assert_eq!(
        w.store.add(w.bob, w.ws, pages[n - 1]).await.unwrap(),
        AddOutcome::Added
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_adds_of_same_doc_make_one_row() {
    let w = setup().await;
    for _ in 0..10 {
        let (a, b) = tokio::join!(
            w.store.add(w.alice, w.ws, w.d1),
            w.store.add(w.alice, w.ws, w.d1)
        );
        let mut outcomes = vec![a.unwrap(), b.unwrap()];
        outcomes.sort_by_key(|o| *o != AddOutcome::Added);
        assert_eq!(outcomes, vec![AddOutcome::Added, AddOutcome::AlreadyThere]);
        assert_eq!(row_count(&w.pool, w.alice).await, 1);
        assert!(w.store.remove(w.alice, w.ws, w.d1).await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_adds_get_distinct_sort_keys() {
    let w = setup().await;
    for _ in 0..10 {
        let (a, b) = tokio::join!(
            w.store.add(w.alice, w.ws, w.d1),
            w.store.add(w.alice, w.ws, w.d2)
        );
        assert_eq!(a.unwrap(), AddOutcome::Added);
        assert_eq!(b.unwrap(), AddOutcome::Added);
        let list = w.store.list(w.alice, w.ws, &[]).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_ne!(list[0].sort_key, list[1].sort_key);
        w.store.remove(w.alice, w.ws, w.d1).await.unwrap();
        w.store.remove(w.alice, w.ws, w.d2).await.unwrap();
    }
}

/// The cascade from `documents` deletes `WHERE doc_id = ?` once per purged
/// page; without an index leading with `doc_id` each one scans the table.
#[tokio::test(flavor = "multi_thread")]
async fn doc_id_has_a_leading_index_for_the_purge_cascade() {
    let w = setup().await;
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes \
         WHERE schemaname = current_schema() AND tablename = 'user_favorites'",
    )
    .fetch_all(&w.pool)
    .await
    .unwrap();
    assert!(
        defs.iter()
            .any(|d| d.contains("(doc_id)") || d.contains("(doc_id,")),
        "no index on user_favorites leads with doc_id: {defs:?}"
    );
}

/// `add` runs inside an axum handler that is dropped when the client
/// disconnects; a raw `pool.begin()` can then strand an open transaction on
/// a pooled connection (see `knot_storage::begin`). Pin the safe helper.
#[test]
fn add_opens_its_transaction_with_the_cancellation_safe_begin() {
    let src = include_str!("../src/favorites.rs");
    assert!(
        !src.contains(".begin()"),
        "favorites.rs must use crate::begin, not pool.begin()"
    );
    assert!(src.contains("crate::begin(&self.pool)"));
}
