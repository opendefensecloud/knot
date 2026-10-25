//! Workspace import (`POST /api/workspace/import`): every page of an export
//! lands, under the import target or at the top level, with fresh order keys
//! in the export's order. The export's own keys only order siblings, so
//! importing the same export twice keeps every page of both copies.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
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

/// The router plus a logged-in owner's cookie header and CSRF token, and the
/// database, for reading the imported tree back.
struct World {
    app: axum::Router,
    cookie: String,
    csrf: String,
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
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": EMAIL, "password": PASSWORD}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT, "login");
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect();
    let sid = cookies.iter().find(|c| c.starts_with("sid=")).unwrap();
    let csrf = cookies.iter().find(|c| c.starts_with("csrf=")).unwrap();
    World {
        app,
        cookie: format!("{sid}; {csrf}"),
        csrf: csrf.trim_start_matches("csrf=").to_string(),
        pool,
        workspace: workspace.id,
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
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .header("x-csrf-token", &self.csrf)
                    .header("content-type", content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
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

/// Regression guard after Task 9: top-level keys are unique now, so the
/// export's keys would collide at the top level too.
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
/// and with a child listed before its parent.
#[tokio::test(flavor = "multi_thread")]
async fn import_preserves_manifest_order() {
    let w = world().await;
    let target = w.create("Target").await;
    let book = Page::new(None, "Book", "n");
    let pages = vec![
        Page::new(Some(book.id), "Three", "t"),
        Page::new(None, "Afterword", "p"),
        Page::new(Some(book.id), "One", "c"),
        Page::new(Some(book.id), "Two", "m"),
        Page::new(Some(book.id), "Two, continued", "m"),
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
            "  Two",
            "  Two, continued",
            "  Three",
            "Afterword",
        ]
    );
    w.assert_keys_fresh().await;
}
