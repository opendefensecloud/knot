use std::collections::HashSet;
use std::sync::Arc;

use knot_storage::{
    DocStore, PgDocStore, PgShortLinkStore, PgUserStore, PgWorkspaceStore, ShortLinkStore,
    UserStore, WorkspaceRole, WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

struct Fixture {
    links: PgShortLinkStore,
    docs: PgDocStore,
    pool: sqlx::PgPool,
    ws: Uuid,
    user: Uuid,
    doc: Uuid,
}

async fn setup() -> Fixture {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let u = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let docs = PgDocStore::new(pool.clone());
    let doc = docs
        .create(ws.id, None, "Doc", &sort_key_between(None, None), u.id)
        .await
        .unwrap();
    Fixture {
        links: PgShortLinkStore::new(pool.clone()),
        docs,
        pool,
        ws: ws.id,
        user: u.id,
        doc: doc.id,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_or_create_is_idempotent() {
    let f = setup().await;
    let a = f.links.get_or_create(f.doc, f.user).await.unwrap();
    let b = f.links.get_or_create(f.doc, f.user).await.unwrap();
    assert_eq!(a.code, b.code);
    assert_eq!(a.doc_id, f.doc);
    assert_eq!(
        f.links.find_for_doc(f.doc).await.unwrap().unwrap().code,
        a.code
    );
    assert_eq!(
        f.links.resolve(&a.code).await.unwrap().unwrap().doc_id,
        f.doc
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_get_or_create_returns_one_code() {
    let f = setup().await;
    let links = Arc::new(PgShortLinkStore::new(f.pool.clone()));
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let links = links.clone();
            let (doc, user) = (f.doc, f.user);
            tokio::spawn(async move { links.get_or_create(doc, user).await.unwrap().code })
        })
        .collect();
    let mut codes = HashSet::new();
    for t in tasks {
        codes.insert(t.await.unwrap());
    }
    assert_eq!(codes.len(), 1, "{codes:?}");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM doc_short_links WHERE doc_id = $1")
        .bind(f.doc)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn code_survives_rename_and_move() {
    let f = setup().await;
    let code = f.links.get_or_create(f.doc, f.user).await.unwrap().code;
    f.docs
        .rename(f.ws, f.doc, f.user, "Renamed", None)
        .await
        .unwrap();
    let parent = f
        .docs
        .create(f.ws, None, "Parent", "z", f.user)
        .await
        .unwrap();
    f.docs
        .move_to(
            f.ws,
            f.doc,
            f.user,
            Some(parent.id),
            &sort_key_between(None, None),
        )
        .await
        .unwrap();
    assert_eq!(f.links.resolve(&code).await.unwrap().unwrap().doc_id, f.doc);
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_removes_code() {
    let f = setup().await;
    let code = f.links.get_or_create(f.doc, f.user).await.unwrap().code;
    f.docs.archive(f.ws, f.doc, f.user).await.unwrap();
    f.docs.purge(f.ws, f.doc, Some(f.user)).await.unwrap();
    assert!(f.links.resolve(&code).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_code_resolves_none() {
    let f = setup().await;
    assert!(f.links.resolve("Zz9Zz9Z").await.unwrap().is_none());
    assert!(f.links.find_for_doc(f.doc).await.unwrap().is_none());
}
