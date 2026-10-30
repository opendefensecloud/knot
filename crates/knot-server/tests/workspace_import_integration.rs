//! Workspace import (`POST /api/workspace/import`): every page of an export
//! lands, under the import target or at the top level, with fresh order keys
//! in the export's order. The export's own keys only order siblings, so
//! importing the same export twice keeps every page of both copies.
//!
//! Also the export side's handling of restricted pages: an export holds only
//! the pages its author can read, and carries the restriction so that an
//! import of it is restricted again.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{
    PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole, lexorank,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const EMAIL: &str = "owner@import.test";
const PASSWORD: &str = "hunter22hunter22";

/// A logged-in member: cookie header and CSRF token.
struct Session {
    cookie: String,
    csrf: String,
}

/// The router plus the logged-in owner's session, and the database, for
/// reading the imported tree back.
struct World {
    app: axum::Router,
    owner: Session,
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
    let owner = login(&app, EMAIL, PASSWORD).await;
    World {
        app,
        owner,
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
/// `markdown_import_integration.rs`).
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
    /// One request as the owner; the JSON body, or `Null` when there is none.
    async fn send(
        &self,
        method: &str,
        uri: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let (status, bytes) = self
            .request(&self.owner, method, uri, content_type, body)
            .await;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// One request as `session`; the raw body.
    async fn request(
        &self,
        session: &Session,
        method: &str,
        uri: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Bytes) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("cookie", &session.cookie)
                    .header("x-csrf-token", &session.csrf)
                    .header("content-type", content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes)
    }

    /// One JSON request as `session`; the JSON body, or `Null`.
    async fn send_json(
        &self,
        session: &Session,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let (status, bytes) = self
            .request(
                session,
                method,
                uri,
                "application/json",
                body.to_string().into_bytes(),
            )
            .await;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// Invites `email` with workspace `role`, logs them in, and returns their
    /// session and user id.
    async fn invite(&self, email: &str, role: &str) -> (Session, Uuid) {
        let (status, body) = self
            .send_json(
                &self.owner,
                "POST",
                "/api/workspace/members",
                json!({"email": email, "role": role, "password": PASSWORD}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "invite {email}: {body}");
        let user_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
            .bind(email)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        (login(&self.app, email, PASSWORD).await, user_id)
    }

    /// `POST /api/docs` as `session`, under `parent` when given, asserting
    /// 201; returns the id.
    async fn create_as(&self, session: &Session, title: &str, parent: Option<Uuid>) -> Uuid {
        let (status, doc) = self
            .send_json(
                session,
                "POST",
                "/api/docs",
                json!({"title": title, "parent_id": parent}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "create {title}: {doc}");
        Uuid::parse_str(doc["id"].as_str().unwrap()).unwrap()
    }

    /// Restricts `doc` as `session`, asserting 200.
    async fn restrict(&self, session: &Session, doc: Uuid) {
        let (status, body) = self
            .send_json(
                session,
                "PUT",
                &format!("/api/docs/{doc}/restricted"),
                json!({"restricted": true}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "restrict {doc}: {body}");
    }

    /// Grants `user` `role` on `doc` as `session`, asserting success.
    async fn grant(&self, session: &Session, doc: Uuid, user: Uuid, role: &str, inherit: bool) {
        let (status, body) = self
            .send_json(
                session,
                "PUT",
                &format!("/api/docs/{doc}/grants/user:{user}"),
                json!({"role": role, "inherit": inherit}),
            )
            .await;
        assert!(
            status.is_success(),
            "grant {role} on {doc}: {status} {body}"
        );
    }

    /// `GET uri` as `session`, asserting 200; returns the zip bytes.
    async fn export_as(&self, session: &Session, uri: &str) -> Vec<u8> {
        let (status, bytes) = self
            .request(session, "GET", uri, "application/json", Vec::new())
            .await;
        assert_eq!(status, StatusCode::OK, "export {uri}");
        bytes.to_vec()
    }

    /// `POST /api/docs` at the top level, asserting 201; returns the id.
    async fn create(&self, title: &str) -> Uuid {
        let body = json!({ "title": title }).to_string().into_bytes();
        let (status, doc) = self
            .send("POST", "/api/docs", "application/json", body)
            .await;
        assert_eq!(status, StatusCode::CREATED, "create {title}: {doc}");
        Uuid::parse_str(doc["id"].as_str().unwrap()).unwrap()
    }

    /// Imports `zip` under `parent` (`None`: the top level), asserting 200
    /// and that every page of the export was imported.
    async fn import(&self, parent: Option<Uuid>, zip: Vec<u8>, pages: usize) {
        let uri = match parent {
            Some(p) => format!("/api/workspace/import?parent_id={p}"),
            None => "/api/workspace/import".to_string(),
        };
        let (status, body) = self.send("POST", &uri, "application/zip", zip).await;
        assert_eq!(status, StatusCode::OK, "import: {body}");
        assert_eq!(body["imported_docs"], json!(pages), "import: {body}");
    }

    /// The live pages under `parent` (`None`: the top level), depth first,
    /// each as its title indented two spaces per level, siblings in the
    /// sidebar's order: by key, byte by byte.
    async fn outline(&self, parent: Option<Uuid>) -> Vec<String> {
        let rows: Vec<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
            "SELECT id, parent_id, title FROM documents
              WHERE workspace_id = $1 AND archived_at IS NULL
              ORDER BY sort_key COLLATE \"C\", id",
        )
        .bind(self.workspace)
        .fetch_all(&self.pool)
        .await
        .unwrap();
        fn walk(
            rows: &[(Uuid, Option<Uuid>, String)],
            parent: Option<Uuid>,
            depth: usize,
            out: &mut Vec<String>,
        ) {
            for (id, _, title) in rows.iter().filter(|(_, p, _)| *p == parent) {
                out.push(format!("{}{title}", "  ".repeat(depth)));
                walk(rows, Some(*id), depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(&rows, parent, 0, &mut out);
        out
    }

    /// Asserts that every key in the workspace is valid and unique among its
    /// siblings: the import wrote fresh keys, not the export's.
    async fn assert_keys_fresh(&self) {
        let groups: Vec<(Option<Uuid>, Vec<String>)> = sqlx::query_as(
            "SELECT parent_id, array_agg(sort_key ORDER BY sort_key COLLATE \"C\")
               FROM documents WHERE workspace_id = $1 GROUP BY parent_id",
        )
        .bind(self.workspace)
        .fetch_all(&self.pool)
        .await
        .unwrap();
        for (parent, keys) in groups {
            assert!(
                keys.iter().all(|k| lexorank::is_valid(k) && k.len() <= 64),
                "bad keys under {parent:?}: {keys:?}"
            );
            assert!(
                keys.windows(2).all(|w| w[0] < w[1]),
                "duplicate keys under {parent:?}: {keys:?}"
            );
        }
    }
}

/// One page of an export: its id, its parent's id, title and order key.
struct Page {
    id: Uuid,
    parent: Option<Uuid>,
    title: &'static str,
    key: &'static str,
}

impl Page {
    fn new(parent: Option<Uuid>, title: &'static str, key: &'static str) -> Self {
        Self {
            id: Uuid::new_v4(),
            parent,
            title,
            key,
        }
    }
}

/// A workspace export of `pages`, in this manifest order, built the way
/// `contributors_integration.rs` builds one: an `index.json` manifest plus
/// one Markdown body per page.
fn export(pages: &[Page]) -> Vec<u8> {
    let docs: Vec<Value> = pages
        .iter()
        .enumerate()
        .map(|(i, p)| {
            json!({
                "id": p.id.to_string(),
                "parent_id": p.parent.map(|id| id.to_string()),
                "title": p.title,
                "sort_key": p.key,
                "path": format!("docs/page-{i}.md"),
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
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("index.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    for (i, p) in pages.iter().enumerate() {
        zip.start_file(format!("docs/page-{i}.md"), opts).unwrap();
        zip.write_all(format!("# {}\n\nBody.\n", p.title).as_bytes())
            .unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// Two top-level pages with children, as exported: `Guide` (`Install`,
/// `Usage`) and `Reference` (`API`).
fn guide_and_reference() -> Vec<Page> {
    let guide = Page::new(None, "Guide", "g");
    let reference = Page::new(None, "Reference", "r");
    let install = Page::new(Some(guide.id), "Install", "i");
    let usage = Page::new(Some(guide.id), "Usage", "u");
    let api = Page::new(Some(reference.id), "API", "a");
    vec![guide, install, usage, reference, api]
}

/// The outline one import of [`guide_and_reference`] adds.
const GUIDE_AND_REFERENCE: [&str; 5] = ["Guide", "  Install", "  Usage", "Reference", "  API"];

/// The second import's roots used to take the same keys as the first's under
/// the same parent: each collided, was skipped, and its children were grafted
/// onto the import target instead.
#[tokio::test(flavor = "multi_thread")]
async fn importing_the_same_export_twice_under_a_parent_keeps_every_doc() {
    let w = world().await;
    let target = w.create("Target").await;
    let zip = export(&guide_and_reference());

    w.import(Some(target), zip.clone(), 5).await;
    w.import(Some(target), zip, 5).await;

    let twice: Vec<&str> = GUIDE_AND_REFERENCE.repeat(2);
    assert_eq!(w.outline(Some(target)).await, twice);
    w.assert_keys_fresh().await;
}

/// The same at the top level. Top-level keys used to be allowed to repeat
/// (NULL parents counted as distinct), so both copies kept the export's keys:
/// duplicate keys at the top level, and the two copies interleaved by id.
/// Now that they are unique, reusing the export's keys would collide there
/// as they do under a parent.
#[tokio::test(flavor = "multi_thread")]
async fn importing_the_same_export_twice_at_top_level_keeps_every_doc() {
    let w = world().await;
    w.create("Home").await;
    let zip = export(&guide_and_reference());

    w.import(None, zip.clone(), 5).await;
    w.import(None, zip, 5).await;

    let mut expected = vec!["Home"];
    expected.extend(GUIDE_AND_REFERENCE.repeat(2));
    assert_eq!(w.outline(None).await, expected);
    w.assert_keys_fresh().await;
}

/// A page that is its own parent in the manifest: the walk that orders pages
/// parents first recursed through it without end, so the stack overflowed and
/// aborted the whole server process. It imports as a root under the import
/// target (here the top level), as the sidebar shows a page in a parent
/// cycle, and its child stays under it.
#[tokio::test(flavor = "multi_thread")]
async fn import_with_self_parent_does_not_crash() {
    let w = world().await;
    w.create("Home").await;
    let mut ouroboros = Page::new(None, "Ouroboros", "m");
    ouroboros.parent = Some(ouroboros.id);
    let tail = Page::new(Some(ouroboros.id), "Tail", "m");

    w.import(None, export(&[tail, ouroboros]), 2).await;

    assert_eq!(w.outline(None).await, ["Home", "Ouroboros", "  Tail"]);
    w.assert_keys_fresh().await;
}

/// Two pages that are each other's parent crashed the server the same way.
/// Both import as roots under the import target, ordered by key among its
/// other new children, and a child of one of them stays under it.
#[tokio::test(flavor = "multi_thread")]
async fn import_with_parent_cycle_does_not_crash() {
    let w = world().await;
    let target = w.create("Target").await;
    let mut ping = Page::new(None, "Ping", "a");
    let pong = Page::new(Some(ping.id), "Pong", "b");
    ping.parent = Some(pong.id);
    let echo = Page::new(Some(pong.id), "Echo", "m");
    let after = Page::new(None, "After", "c");

    w.import(Some(target), export(&[echo, pong, ping, after]), 4)
        .await;

    assert_eq!(
        w.outline(Some(target)).await,
        ["Ping", "Pong", "  Echo", "After"]
    );
    w.assert_keys_fresh().await;
}

/// Siblings keep the export's order: by key, byte by byte (`"M"` before
/// `"c"`), ties in manifest order, whatever order the manifest lists them in,
/// and with a child listed before its parent. The tied pair is listed against
/// title order, so a tie broken by title would fail.
#[tokio::test(flavor = "multi_thread")]
async fn import_preserves_manifest_order() {
    let w = world().await;
    let target = w.create("Target").await;
    let book = Page::new(None, "Book", "n");
    let pages = vec![
        Page::new(Some(book.id), "Three", "t"),
        Page::new(None, "Afterword", "p"),
        Page::new(Some(book.id), "One", "c"),
        Page::new(Some(book.id), "Two, continued", "m"),
        Page::new(Some(book.id), "Two", "m"),
        Page::new(Some(book.id), "Zero", "M"),
        book,
    ];

    w.import(Some(target), export(&pages), 7).await;

    assert_eq!(
        w.outline(Some(target)).await,
        [
            "Book",
            "  Zero",
            "  One",
            "  Two, continued",
            "  Two",
            "  Three",
            "Afterword",
        ]
    );
    w.assert_keys_fresh().await;
}

/// The names of every file in `zip`.
fn zip_names(zip: &[u8]) -> Vec<String> {
    let archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    archive.file_names().map(str::to_string).collect()
}

/// The text of `index.json` in `zip`.
fn manifest_text(zip: &[u8]) -> String {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let mut text = String::new();
    archive
        .by_name("index.json")
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

/// The manifest entry for the page titled `title`.
fn manifest_entry(zip: &[u8], title: &str) -> Value {
    let manifest: Value = serde_json::from_str(&manifest_text(zip)).unwrap();
    manifest["docs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["title"] == title)
        .unwrap_or_else(|| panic!("no manifest entry titled {title}: {manifest}"))
        .clone()
}

/// Even the workspace owner's export holds only the pages they can read. A
/// page granted to them below a boundary they can't read comes out as a
/// root, marked restricted, and neither the boundary's id nor its title is
/// anywhere in the zip.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_export_excludes_restricted_docs_owner_cannot_read() {
    let w = world().await;
    let (bob, bob_id) = w.invite("bob@import.test", "editor").await;
    let owner_id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
        .bind(EMAIL)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    w.create("Handbook").await;
    let vault = w.create("Bobs vault").await;
    let diary = w.create_as(&w.owner, "Bobs diary", Some(vault)).await;
    let shared = w.create_as(&w.owner, "Shared plan", Some(vault)).await;
    // Bob takes the vault over and shuts the owner out, but shares one page.
    w.grant(&w.owner, vault, bob_id, "owner", true).await;
    w.restrict(&bob, vault).await;
    w.grant(&bob, shared, owner_id, "viewer", false).await;

    let zip = w.export_as(&w.owner, "/api/workspace/export").await;

    let manifest = manifest_text(&zip);
    for hidden in [vault, diary] {
        assert!(
            !manifest.contains(&hidden.to_string()),
            "{hidden} leaked into index.json: {manifest}"
        );
    }
    for hidden_title in ["Bobs vault", "Bobs diary"] {
        assert!(
            !manifest.contains(hidden_title),
            "{hidden_title} leaked into index.json"
        );
    }
    let names = zip_names(&zip);
    assert!(
        names
            .iter()
            .all(|name| !name.contains("vault") && !name.contains("diary")),
        "a hidden page's file is in the zip: {names:?}"
    );
    assert!(names.contains(&"docs/Handbook.md".to_string()), "{names:?}");

    let shared_entry = manifest_entry(&zip, "Shared plan");
    assert_eq!(
        shared_entry["parent_id"],
        Value::Null,
        "hidden parent dropped"
    );
    assert_eq!(
        shared_entry["restricted"], true,
        "still restricted on import"
    );
    assert_eq!(manifest_entry(&zip, "Handbook")["restricted"], false);
}

/// A grantee exports a restricted subtree and imports it at the top level:
/// the imported boundary is restricted again, the importer owns it, and a
/// member who could read neither the original nor the copy still can't.
/// Exporting a page inside the subtree alone marks it restricted too, since
/// it was restricted where it came from.
#[tokio::test(flavor = "multi_thread")]
async fn export_import_round_trip_keeps_restriction() {
    let w = world().await;
    let (bob, _) = w.invite("bob@import.test", "editor").await;
    let (carol, carol_id) = w.invite("carol@import.test", "owner").await;
    let vault = w.create("Vault").await;
    let plans = w.create_as(&w.owner, "Plans", Some(vault)).await;
    w.restrict(&w.owner, vault).await;
    w.grant(&w.owner, vault, carol_id, "editor", true).await;

    let subtree = w
        .export_as(
            &carol,
            &format!("/api/docs/{vault}/export?descendants=true"),
        )
        .await;
    assert_eq!(manifest_entry(&subtree, "Vault")["restricted"], true);
    assert_eq!(manifest_entry(&subtree, "Plans")["restricted"], false);
    let (status, body) = w
        .request(
            &carol,
            "POST",
            "/api/workspace/import",
            "application/zip",
            subtree,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "import: {body:?}");

    let imported_vault: Uuid = sqlx::query_scalar(
        "SELECT id FROM documents WHERE title = 'Vault' AND id <> $1 AND parent_id IS NULL",
    )
    .bind(vault)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    let imported_plans: Uuid =
        sqlx::query_scalar("SELECT id FROM documents WHERE title = 'Plans' AND parent_id = $1")
            .bind(imported_vault)
            .fetch_one(&w.pool)
            .await
            .unwrap();

    let (status, carol_view) = w
        .send_json(
            &carol,
            "GET",
            &format!("/api/docs/{imported_vault}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(carol_view["restricted"], true, "{carol_view}");
    assert_eq!(carol_view["effective_role"], "owner", "{carol_view}");
    for copy in [imported_vault, imported_plans] {
        let (status, _) = w
            .send_json(&bob, "GET", &format!("/api/docs/{copy}"), Value::Null)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "bob reads the copy {copy}");
    }

    // A page from inside the subtree, exported on its own.
    let page = w
        .export_as(&carol, &format!("/api/docs/{plans}/export"))
        .await;
    assert_eq!(manifest_entry(&page, "Plans")["restricted"], true);
    let (status, _) = w
        .request(
            &carol,
            "POST",
            "/api/workspace/import",
            "application/zip",
            page,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let imported_page: Uuid =
        sqlx::query_scalar("SELECT id FROM documents WHERE title = 'Plans' AND parent_id IS NULL")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    let (status, _) = w
        .send_json(
            &bob,
            "GET",
            &format!("/api/docs/{imported_page}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "bob reads the lone copy");
}

/// A subtree export leaves out a restricted subpage the exporter can't read,
/// and everything below it.
#[tokio::test(flavor = "multi_thread")]
async fn subtree_export_prunes_restricted_child() {
    let w = world().await;
    let (bob, bob_id) = w.invite("bob@import.test", "editor").await;
    let handbook = w.create("Handbook").await;
    w.create_as(&w.owner, "Public chapter", Some(handbook))
        .await;
    let secret = w
        .create_as(&w.owner, "Secret chapter", Some(handbook))
        .await;
    let appendix = w.create_as(&w.owner, "Secret appendix", Some(secret)).await;
    w.grant(&w.owner, secret, bob_id, "owner", true).await;
    w.restrict(&bob, secret).await;

    let zip = w
        .export_as(
            &w.owner,
            &format!("/api/docs/{handbook}/export?descendants=true"),
        )
        .await;

    let manifest = manifest_text(&zip);
    for (hidden, hidden_title) in [(secret, "Secret chapter"), (appendix, "Secret appendix")] {
        assert!(!manifest.contains(&hidden.to_string()), "{manifest}");
        assert!(!manifest.contains(hidden_title), "{manifest}");
    }
    let names = zip_names(&zip);
    assert!(
        names.iter().all(|name| !name.contains("Secret")),
        "{names:?}"
    );
    assert_eq!(manifest_entry(&zip, "Public chapter")["restricted"], false);
}
