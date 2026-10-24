//! `export-check` over a seeded database: counts pages, failures and fallbacks
//! without touching the schema.

use knot_server::export_check::check_all;
use knot_storage::{
    DocStore, PgDocStore, PgSnapshotStore, PgUserStore, PgWorkspaceStore, SnapshotStore, UserStore,
    WorkspaceStore,
};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Doc, ReadTxn, StateVector, Transact, Update, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim,
};

/// Full-state bytes and state vector for a doc built by `fill`. The doc is
/// rebuilt from a hand-built update, as a stored page would be.
fn page_state(fill: impl FnOnce(&Doc)) -> (Vec<u8>, Vec<u8>) {
    let source = Doc::new();
    fill(&source);
    let update = source
        .transact()
        .encode_state_as_update_v1(&StateVector::default());
    let doc = Doc::new();
    doc.transact_mut()
        .apply_update(Update::decode_v1(&update).unwrap())
        .unwrap();
    let txn = doc.transact();
    (
        txn.encode_state_as_update_v1(&StateVector::default()),
        txn.state_vector().encode_v1(),
    )
}

async fn migration_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn check_all_reports_pages_failures_and_fallbacks() {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "Workspace")
        .await
        .unwrap();
    let hash = knot_auth::Hasher::fast_for_tests()
        .hash("hunter22")
        .unwrap();
    let user = PgUserStore::new(pool.clone())
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    let docs = PgDocStore::new(pool.clone());
    let a = docs.create(ws.id, None, "A", "m", user.id).await.unwrap();
    let b = docs.create(ws.id, None, "B", "n", user.id).await.unwrap();
    let snaps = PgSnapshotStore::new(pool.clone());

    // Page A: a mermaid block and an element the walker does not know.
    let (bytes, sv) = page_state(|doc| {
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        let code = frag.push_back(&mut txn, XmlElementPrelim::empty("code_block"));
        code.insert_attribute(&mut txn, "language", "mermaid");
        code.push_back(&mut txn, XmlTextPrelim::new("graph TD; A-->B"));
        let callout = frag.push_back(&mut txn, XmlElementPrelim::empty("callout"));
        let p = callout.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("careful"));
    });
    snaps.insert(a.id, 1, &bytes, &sv).await.unwrap();

    // Page B: plain text.
    let (bytes, sv) = page_state(|doc| {
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("hello"));
    });
    snaps.insert(b.id, 1, &bytes, &sv).await.unwrap();

    let migrations = migration_count(&pool).await;

    let report = check_all(&pool, "http://localhost").await.unwrap();

    assert_eq!(report.pages, 2);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.fallbacks.diagram, 2, "one per format");
    assert_eq!(report.fallbacks.unknown_element, 2, "one per format");
    assert_eq!(report.fallbacks.image_unavailable, 0);
    assert_eq!(migration_count(&pool).await, migrations);
}
