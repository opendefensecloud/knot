//! Workspace import of restricted pages (`POST /api/workspace/import`).
//!
//! A manifest entry marked `restricted` is created restricted: the flag, the
//! importer's owner grant, the audit entries and the ACL invalidation are
//! written in the transaction that inserts the page, so its title is never
//! listed to the whole workspace. When a page in a restricted subtree of the
//! manifest fails to import, its subpages are dropped rather than grafted,
//! unrestricted, onto the import target.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const EMAIL: &str = "owner@restricted-import.test";
const PASSWORD: &str = "hunter22hunter22";

/// A title Postgres refuses to store (text can't hold a NUL byte), so the
/// create of a page carrying it fails.
const UNSTORABLE_TITLE: &str = "Broken\u{0}page";

/// A logged-in member: cookie header and CSRF token.
struct Session {
    cookie: String,
    csrf: String,
}

/// The router plus the logged-in owner, and the database, for reading the
/// imported tree back.
struct World {
    app: axum::Router,
    owner: Session,
    owner_id: Uuid,
    pool: PgPool,
    workspace: Uuid,
}

/// A workspace with one owner, and a real room registry: the import seeds
/// each page's content through its room.
async fn world() -> World {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();
    let mut state = AppState::with_pool(pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let workspace = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "W")
        .await
        .unwrap();
    let hash = state.hasher.hash(PASSWORD).unwrap();
    let owner = state
        .users
        .as_ref()
        .unwrap()
        .create_local(EMAIL, "Olive Owner", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    wire_rooms(&mut state, &db.url).await;
    let app = router_with_state(state);
    let session = login(&app, EMAIL, PASSWORD).await;
    World {
        app,
        owner: session,
        owner_id: owner.id,
        pool,
        workspace: workspace.id,
    }
}

/// Logs `email` in and returns their session.
async fn login(app: &axum::Router, email: &str, password: &str) -> Session {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "login {email}");
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    let sid = cookies.iter().find(|c| c.starts_with("sid=")).unwrap();
    let csrf = cookies.iter().find(|c| c.starts_with("csrf=")).unwrap();
    Session {
        cookie: format!("{sid}; {csrf}"),
        csrf: csrf.trim_start_matches("csrf=").to_string(),
    }
}

/// A room registry on the test database (as in
/// `workspace_import_integration.rs`).
async fn wire_rooms(state: &mut AppState, db_url: &str) {
    let pool = state.pool.as_ref().unwrap().clone();
    let bus = Arc::new(knot_crdt::PgBus::connect(db_url).await.unwrap());
    let updates: Arc<dyn UpdatesStore> = Arc::new(PgUpdatesStore::new(pool.clone()));
    let snaps: Arc<dyn SnapshotStore> = Arc::new(PgSnapshotStore::new(pool.clone()));
    let policy = SnapshotPolicy {
        every_n: 1000,
        idle: Duration::from_secs(3600),
    };
    let rooms = Arc::new(Rooms::new(
        Arc::new(YrsEngine),
        bus.clone(),
        updates,
        snaps,
        policy,
        Duration::from_secs(3600),
    ));
    state.bus = Some(bus);
    state.rooms_v2 = Some(rooms);
}

impl World {
    /// Imports `zip` at the top level as the owner, asserting 200; returns
    /// the number of pages the import reports.
    async fn import(&self, zip: Vec<u8>) -> u64 {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/workspace/import")
                    .header("cookie", &self.owner.cookie)
                    .header("x-csrf-token", &self.owner.csrf)
                    .header("content-type", "application/zip")
                    .body(Body::from(zip))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        assert_eq!(status, StatusCode::OK, "import: {body}");
        body["imported_docs"].as_u64().unwrap()
    }

    /// The live page titled `title`, with its parent and restricted flag.
    async fn page(&self, title: &str) -> Option<(Uuid, Option<Uuid>, bool)> {
        sqlx::query_as(
            "SELECT id, parent_id, restricted FROM documents
              WHERE workspace_id = $1 AND title = $2 AND archived_at IS NULL",
        )
        .bind(self.workspace)
        .bind(title)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
    }

    /// When each audit entry `action` on `doc` was written.
    async fn audit_times(&self, doc: Uuid, action: &str) -> Vec<DateTime<Utc>> {
        sqlx::query_scalar(
            "SELECT created_at FROM audit_events
              WHERE target_id = $1 AND action = $2 ORDER BY id",
        )
        .bind(doc)
        .bind(action)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

/// One page of an export: its id, its parent's id, title and restricted flag.
struct Page {
    id: Uuid,
    parent: Option<Uuid>,
    title: &'static str,
    restricted: bool,
}

impl Page {
    fn new(parent: Option<Uuid>, title: &'static str, restricted: bool) -> Self {
        Self {
            id: Uuid::new_v4(),
            parent,
            title,
            restricted,
        }
    }
}

/// A workspace export of `pages`, in this manifest order: an `index.json`
/// manifest plus one Markdown body per page.
fn export(pages: &[Page]) -> Vec<u8> {
    let docs: Vec<Value> = pages
        .iter()
        .enumerate()
        .map(|(index, page)| {
            json!({
                "id": page.id.to_string(),
                "parent_id": page.parent.map(|id| id.to_string()),
                "title": page.title,
                "sort_key": format!("a{index}"),
                "path": format!("docs/page-{index}.md"),
                "restricted": page.restricted,
            })
        })
        .collect();
    let manifest = json!({
        "knot_export_version": "2",
        "docs": docs,
        "attachments": [],
        "boards": [],
    });
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("index.json", options).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    for (index, page) in pages.iter().enumerate() {
        zip.start_file(format!("docs/page-{index}.md"), options)
            .unwrap();
        zip.write_all(format!("# {}\n\nSecret body.\n", page.title).as_bytes())
            .unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// The page is inserted restricted: its `doc.create` and `doc.restrict`
/// entries share one transaction's timestamp, so no other transaction ever
/// saw it unrestricted. The importer owns it through an inheriting grant.
#[tokio::test(flavor = "multi_thread")]
async fn restricted_entry_is_created_restricted_in_one_transaction() {
    let w = world().await;
    let vault = Page::new(None, "Vault", true);
    let plans = Page::new(Some(vault.id), "Plans", false);
    assert_eq!(w.import(export(&[vault, plans])).await, 2);

    let (vault_id, _, vault_restricted) = w.page("Vault").await.expect("Vault imported");
    assert!(vault_restricted, "Vault is restricted");
    let (_, plans_parent, plans_restricted) = w.page("Plans").await.expect("Plans imported");
    assert_eq!(plans_parent, Some(vault_id), "Plans stays under Vault");
    assert!(!plans_restricted, "the boundary's flag covers Plans");

    let created = w.audit_times(vault_id, "doc.create").await;
    let restricted = w.audit_times(vault_id, "doc.restrict").await;
    assert_eq!(created.len(), 1, "one doc.create entry");
    assert_eq!(
        restricted, created,
        "restricted in the create's transaction"
    );

    let owner_grant: Option<(String, bool)> = sqlx::query_as(
        "SELECT role, inherit FROM document_grants WHERE doc_id = $1 AND principal = $2",
    )
    .bind(vault_id)
    .bind(format!("user:{}", w.owner_id))
    .fetch_optional(&w.pool)
    .await
    .unwrap();
    assert_eq!(owner_grant, Some(("owner".to_string(), true)));

    let invalidation_reasons: Vec<String> =
        sqlx::query_scalar("SELECT reason FROM acl_invalidations WHERE doc_id = $1 ORDER BY id")
            .bind(vault_id)
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert!(
        invalidation_reasons
            .iter()
            .any(|reason| reason == "restrict"),
        "restrict invalidation: {invalidation_reasons:?}"
    );
}

/// A restricted page that fails to import takes its subtree with it: its
/// subpages carry no flag of their own, so grafted onto the import target
/// they would be readable to the whole workspace.
#[tokio::test(flavor = "multi_thread")]
async fn failed_restricted_page_drops_its_subtree() {
    let w = world().await;
    let vault = Page::new(None, UNSTORABLE_TITLE, true);
    let plans = Page::new(Some(vault.id), "Plans", false);
    let notes = Page::new(Some(plans.id), "Notes", false);
    let sealed = Page::new(Some(plans.id), "Sealed", true);
    let public = Page::new(None, "Public", false);
    assert_eq!(
        w.import(export(&[vault, plans, notes, sealed, public]))
            .await,
        1
    );

    for dropped in ["Plans", "Notes", "Sealed"] {
        assert_eq!(w.page(dropped).await, None, "{dropped} is not imported");
    }
    assert!(w.page("Public").await.is_some(), "Public is imported");
}

/// The same for an unrestricted page inside a restricted subtree of the
/// manifest: its subpages are dropped, not grafted to the top level.
#[tokio::test(flavor = "multi_thread")]
async fn failed_page_below_a_restricted_page_drops_its_subtree() {
    let w = world().await;
    let vault = Page::new(None, "Vault", true);
    let broken = Page::new(Some(vault.id), UNSTORABLE_TITLE, false);
    let notes = Page::new(Some(broken.id), "Notes", false);
    let sibling = Page::new(Some(vault.id), "Sibling", false);
    assert_eq!(w.import(export(&[vault, broken, notes, sibling])).await, 2);

    assert_eq!(w.page("Notes").await, None, "Notes is not imported");
    let (vault_id, _, _) = w.page("Vault").await.expect("Vault imported");
    let (_, sibling_parent, _) = w.page("Sibling").await.expect("Sibling imported");
    assert_eq!(sibling_parent, Some(vault_id));
}

/// Outside every restricted subtree, a page that fails to import still has
/// its subpages grafted onto the import target, as before.
#[tokio::test(flavor = "multi_thread")]
async fn failed_unrestricted_page_still_grafts_its_children() {
    let w = world().await;
    let broken = Page::new(None, UNSTORABLE_TITLE, false);
    let leaf = Page::new(Some(broken.id), "Leaf", false);
    let vault = Page::new(Some(broken.id), "Vault", true);
    assert_eq!(w.import(export(&[broken, leaf, vault])).await, 2);

    let (_, leaf_parent, leaf_restricted) = w.page("Leaf").await.expect("Leaf imported");
    assert_eq!(leaf_parent, None, "Leaf is grafted to the top level");
    assert!(!leaf_restricted);
    let (_, vault_parent, vault_restricted) = w.page("Vault").await.expect("Vault imported");
    assert_eq!(vault_parent, None);
    assert!(vault_restricted);
}
