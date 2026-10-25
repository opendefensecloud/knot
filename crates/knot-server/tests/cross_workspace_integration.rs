//! A page from another workspace is never a parent: create, move,
//! from-template and import under one fail with `404 doc.not_found`, the
//! same response as an unknown id, and write nothing in either workspace.
//! The foreign key on `parent_id` only proves the row exists somewhere.
//!
//! Two workspaces: `A`, where both users sign in, and `B`. One user is a
//! member of both, the other of `A` only.

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
    PgSnapshotStore, PgUpdatesStore, Placement, SnapshotStore, UpdatesStore, WorkspaceRole,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "hunter22hunter22";

/// A signed-in user's cookie header and CSRF token.
struct Session {
    cookie: String,
    csrf: String,
}

struct World {
    app: axum::Router,
    pool: PgPool,
    /// Owner of `A` and of `B`.
    both: Session,
    /// Editor of `A` only.
    only_a: Session,
    /// A top-level page in `A`.
    mover: Uuid,
    /// A template in `A`.
    template: Uuid,
    /// A live page in `B`.
    foreign: Uuid,
    /// A page in `B`'s archive.
    foreign_archived: Uuid,
}

async fn world() -> World {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();
    let mut state = AppState::with_pool(pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let workspaces = state.workspaces.clone().unwrap();
    let users = state.users.clone().unwrap();
    let docs = state.docs.clone().unwrap();

    let a = workspaces.create("default", "A").await.unwrap().id;
    let b = workspaces.create("other", "B").await.unwrap().id;
    // Sign-in goes to the oldest workspace; make sure that is `A`.
    sqlx::query("UPDATE workspaces SET created_at = created_at + interval '1 day' WHERE id = $1")
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    let hash = state.hasher.hash(PASSWORD).unwrap();
    let both = users
        .create_local("both@ws.test", "Bo Both", &hash)
        .await
        .unwrap()
        .id;
    let only_a = users
        .create_local("only-a@ws.test", "Ann Only", &hash)
        .await
        .unwrap()
        .id;
    workspaces
        .add_member(a, both, WorkspaceRole::Owner)
        .await
        .unwrap();
    workspaces
        .add_member(b, both, WorkspaceRole::Owner)
        .await
        .unwrap();
    workspaces
        .add_member(a, only_a, WorkspaceRole::Editor)
        .await
        .unwrap();

    let page = async |ws, title| {
        docs.create_placed(ws, None, title, Placement::Last, both)
            .await
            .unwrap()
            .id
    };
    let mover = page(a, "Mover").await;
    let template = page(a, "Template").await;
    docs.set_template(a, template, both, true).await.unwrap();
    let foreign = page(b, "Elsewhere").await;
    let foreign_archived = page(b, "Archived elsewhere").await;
    docs.archive(b, foreign_archived, both).await.unwrap();

    wire_rooms(&mut state, &db.url).await;
    let app = router_with_state(state);
    World {
        both: login(&app, "both@ws.test").await,
        only_a: login(&app, "only-a@ws.test").await,
        app,
        pool,
        mover,
        template,
        foreign,
        foreign_archived,
    }
}

/// A room registry on the test database (as in
/// `workspace_import_integration.rs`): from-template and import seed the new
/// page's content through its room.
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

async fn login(app: &axum::Router, email: &str) -> Session {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": email, "password": PASSWORD}).to_string(),
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

impl World {
    /// One request as `who`; the JSON body, or `Null` when there is none.
    async fn send(
        &self,
        who: &Session,
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
                    .header("cookie", &who.cookie)
                    .header("x-csrf-token", &who.csrf)
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

    /// `send` with a JSON body.
    async fn send_json(
        &self,
        who: &Session,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        self.send(
            who,
            method,
            uri,
            "application/json",
            body.to_string().into_bytes(),
        )
        .await
    }

    /// Every page of both workspaces, with what a write would change.
    async fn documents(&self) -> Vec<(Uuid, Uuid, Option<Uuid>, String, String, String)> {
        sqlx::query_as(
            "SELECT id, workspace_id, parent_id, title, sort_key, updated_at::text
               FROM documents ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Sends `request(parent)` as each user, for each page of `B` and for an
    /// id no page has, and asserts that each answer is `404 doc.not_found`,
    /// the same as the unknown id's, and that no page changed or appeared.
    async fn assert_foreign_parents_not_found(
        &self,
        what: &str,
        request: impl AsyncFn(&Session, Uuid) -> (StatusCode, Value),
    ) {
        let before = self.documents().await;
        let mut wrong = Vec::new();
        for (user, who) in [("both", &self.both), ("only A", &self.only_a)] {
            let unknown = request(who, Uuid::new_v4()).await;
            if (unknown.0, code(&unknown.1)) != (StatusCode::NOT_FOUND, "doc.not_found") {
                wrong.push(format!("as {user} under an unknown id: {unknown:?}"));
            }
            for (parent, name) in [
                (self.foreign, "a live page of B"),
                (self.foreign_archived, "an archived page of B"),
            ] {
                let answer = request(who, parent).await;
                if answer != unknown {
                    wrong.push(format!("as {user} under {name}: {answer:?}"));
                }
            }
        }
        let after = self.documents().await;
        let written: Vec<_> = after.iter().filter(|row| !before.contains(row)).collect();
        assert!(
            wrong.is_empty() && written.is_empty(),
            "{what}:\n{}\nwritten: {written:#?}",
            wrong.join("\n")
        );
    }
}

/// The `error.code` of a JSON error body.
fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// A workspace export with one page, as `workspace_import_integration.rs`
/// builds one.
fn one_page_export() -> Vec<u8> {
    let manifest = json!({
        "knot_export_version": "2",
        "docs": [{
            "id": Uuid::new_v4().to_string(),
            "parent_id": null,
            "title": "Imported",
            "sort_key": "m",
            "path": "docs/imported.md",
        }],
        "attachments": [],
        "boards": [],
    });
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("index.json", opts).unwrap();
    zip.write_all(manifest.to_string().as_bytes()).unwrap();
    zip.start_file("docs/imported.md", opts).unwrap();
    zip.write_all(b"# Imported\n\nBody.\n").unwrap();
    zip.finish().unwrap().into_inner()
}

/// `POST /api/docs` under a page of `B` used to create the page in `A` with
/// that parent; an unknown parent failed on the foreign key with a 500.
#[tokio::test(flavor = "multi_thread")]
async fn create_under_a_parent_in_another_workspace_is_404() {
    let w = world().await;
    w.assert_foreign_parents_not_found("create", async |who, parent| {
        w.send_json(
            who,
            "POST",
            "/api/docs",
            json!({"title": "Stray", "parent_id": parent}),
        )
        .await
    })
    .await;
}

/// `POST /api/docs/{id}/move` into a page of `B` used to move the page of
/// `A` under it.
#[tokio::test(flavor = "multi_thread")]
async fn move_into_a_parent_in_another_workspace_is_404() {
    let w = world().await;
    let uri = format!("/api/docs/{}/move", w.mover);
    w.assert_foreign_parents_not_found("move", async |who, parent| {
        w.send_json(who, "POST", &uri, json!({"parent_id": parent}))
            .await
    })
    .await;
}

/// `POST /api/docs/from-template/{id}` under a page of `B` used to create
/// the copy in `A` with that parent.
#[tokio::test(flavor = "multi_thread")]
async fn from_template_under_a_foreign_parent_is_404() {
    let w = world().await;
    let uri = format!("/api/docs/from-template/{}", w.template);
    w.assert_foreign_parents_not_found("from-template", async |who, parent| {
        w.send_json(
            who,
            "POST",
            &uri,
            json!({"title": "Stray", "parent_id": parent}),
        )
        .await
    })
    .await;
}

/// `POST /api/workspace/import?parent_id=` with a page of `B` was refused
/// as `403 acl.editor_required`, as an unknown id was; it is now refused as
/// unknown, like the other three.
#[tokio::test(flavor = "multi_thread")]
async fn import_under_a_foreign_parent_is_404() {
    let w = world().await;
    w.assert_foreign_parents_not_found("import", async |who, parent| {
        w.send(
            who,
            "POST",
            &format!("/api/workspace/import?parent_id={parent}"),
            "application/zip",
            one_page_export(),
        )
        .await
    })
    .await;
}
