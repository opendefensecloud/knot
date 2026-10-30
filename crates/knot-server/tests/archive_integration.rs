//! Document archive (#37): who may archive, what an archived doc still
//! allows, restore, purge, and the archive list. HTTP-level, against a
//! fresh database per test.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::WorkspaceRole;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// A logged-in user: cookie header, CSRF token, user id.
#[derive(Clone)]
struct Session {
    cookie: String,
    csrf: String,
    user_id: Uuid,
}

struct World {
    state: AppState,
    app: axum::Router,
    owner: Session,
}

const PASSWORD: &str = "hunter22hunter22";

async fn world() -> World {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut state = AppState::with_pool(pool);
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
        .create_local("owner@archive.test", "Olive Owner", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let app = router_with_state(state.clone());
    let owner = login(&app, "owner@archive.test", owner.id).await;
    World { state, app, owner }
}

async fn login(app: &axum::Router, email: &str, user_id: Uuid) -> Session {
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
        user_id,
    }
}

/// One request through `app` as `who`.
async fn send_via(
    app: &axum::Router,
    who: &Session,
    method: &str,
    uri: &str,
    content_type: &str,
    body: String,
) -> (StatusCode, Value) {
    let response = app
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
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

impl World {
    /// Invite a member with `role` and log them in.
    async fn member(&self, email: &str, name: &str, role: &str) -> Session {
        let (status, _) = self
            .send(
                &self.owner,
                "POST",
                "/api/workspace/members",
                Some(json!({"email": email, "role": role, "password": PASSWORD, "display_name": name})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "invite {email}");
        let user = self
            .state
            .users
            .as_ref()
            .unwrap()
            .find_by_email(email)
            .await
            .unwrap()
            .unwrap();
        login(&self.app, email, user.id).await
    }

    async fn send(
        &self,
        who: &Session,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        self.send_raw(who, method, uri, "application/json", body)
            .await
    }

    async fn send_raw(
        &self,
        who: &Session,
        method: &str,
        uri: &str,
        content_type: &str,
        body: String,
    ) -> (StatusCode, Value) {
        send_via(&self.app, who, method, uri, content_type, body).await
    }

    async fn create_doc(&self, who: &Session, title: &str, parent: Option<&str>) -> String {
        let (status, body) = self
            .send(
                who,
                "POST",
                "/api/docs",
                Some(json!({"title": title, "parent_id": parent})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "create {title}: {body}");
        body["id"].as_str().unwrap().to_string()
    }

    async fn archive(&self, who: &Session, doc_id: &str) -> (StatusCode, Value) {
        self.send(who, "DELETE", &format!("/api/docs/{doc_id}"), None)
            .await
    }
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_can_archive() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    let folder = world.create_doc(&editor, "Folder", None).await;
    let child = world.create_doc(&editor, "Child", Some(&folder)).await;

    let (status, body) = world.archive(&editor, &folder).await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, list) = world.send(&editor, "GET", "/api/docs", None).await;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&folder.as_str()) && !ids.contains(&child.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_archive() {
    let world = world().await;
    let viewer = world.member("vi@archive.test", "Vi", "viewer").await;
    let doc = world.create_doc(&world.owner, "Doc", None).await;

    let (status, body) = world.archive(&viewer, &doc).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "acl.editor_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_gets_404_on_archived_doc() {
    let world = world().await;
    let viewer = world.member("vi@archive.test", "Vi", "viewer").await;
    let doc = world.create_doc(&world.owner, "Doc", None).await;
    assert_eq!(
        world.archive(&world.owner, &doc).await.0,
        StatusCode::NO_CONTENT
    );

    for uri in [
        format!("/api/docs/{doc}"),
        format!("/api/docs/{doc}/markdown"),
        format!("/api/docs/{doc}/comments"),
    ] {
        let (status, body) = world.send(&viewer, "GET", &uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert_eq!(error_code(&body), "doc.not_found", "{uri}");
    }
    let (status, _) = world
        .send(&world.owner, "GET", &format!("/api/docs/{doc}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "owners can still preview it");
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_to_archived_doc_are_rejected() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    let doc = world.create_doc(&world.owner, "Doc", None).await;
    let other = world.create_doc(&world.owner, "Other", None).await;
    // Prime the ACL cache with the live state, as a session that had the
    // doc open would have; the archive must evict it.
    let (status, _) = world
        .send(&editor, "GET", &format!("/api/docs/{doc}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(world.archive(&editor, &doc).await.0, StatusCode::NO_CONTENT);

    let owner = &world.owner;
    let json_writes: Vec<(&Session, &str, String, Value)> = vec![
        (
            &editor,
            "PATCH",
            format!("/api/docs/{doc}"),
            json!({"title": "X"}),
        ),
        (&editor, "DELETE", format!("/api/docs/{doc}"), json!(null)),
        (
            &editor,
            "POST",
            format!("/api/docs/{doc}/move"),
            json!({"parent_id": null}),
        ),
        (
            &editor,
            "POST",
            format!("/api/docs/{other}/move"),
            json!({"parent_id": doc}),
        ),
        (
            &editor,
            "POST",
            "/api/docs".to_string(),
            json!({"title": "Late", "parent_id": doc}),
        ),
        (
            owner,
            "PUT",
            format!("/api/docs/{doc}/grants/user:{}", editor.user_id),
            json!({"role": "owner", "inherit": true}),
        ),
        (
            &editor,
            "POST",
            format!("/api/docs/{doc}/comments"),
            json!({"body": "hi", "anchor": null}),
        ),
        (
            owner,
            "POST",
            format!("/api/docs/{doc}/template"),
            json!({"is_template": true}),
        ),
        (
            &editor,
            "POST",
            format!("/api/docs/{doc}/history/1/restore"),
            json!(null),
        ),
        (
            &editor,
            "POST",
            format!("/api/docs/from-template/{doc}"),
            json!({"title": "Copy"}),
        ),
        (owner, "POST", format!("/api/docs/{doc}/shares"), json!({})),
        (
            &editor,
            "PATCH",
            format!("/api/docs/{doc}/tasks/0"),
            json!({"checked": true}),
        ),
        (
            &editor,
            "POST",
            format!("/api/docs/{doc}/boards"),
            json!({}),
        ),
    ];
    for (who, method, uri, body) in json_writes {
        let body = (!body.is_null()).then_some(body);
        let (status, response) = world.send(who, method, &uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}: {response}");
        assert_eq!(error_code(&response), "doc.archived", "{method} {uri}");
    }
    let (status, response) = world
        .send_raw(
            &editor,
            "POST",
            &format!("/api/docs/{doc}/markdown"),
            "text/markdown",
            "# hi".into(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "markdown import: {response}");
    let (status, response) = world
        .send_raw(
            &editor,
            "POST",
            &format!("/api/docs/{doc}/blobs"),
            "multipart/form-data; boundary=x",
            "--x--\r\n".into(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "blob upload: {response}");
    let (status, response) = world
        .send_raw(
            &editor,
            "POST",
            &format!("/api/workspace/import?parent_id={doc}"),
            "application/zip",
            String::new(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "import into it: {response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn share_link_to_archived_doc_is_gone() {
    let world = world().await;
    let doc = world.create_doc(&world.owner, "Shared", None).await;
    let (status, share) = world
        .send(
            &world.owner,
            "POST",
            &format!("/api/docs/{doc}/shares"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{share}");
    let token = share["token"].as_str().unwrap().to_string();
    assert_eq!(
        world.archive(&world.owner, &doc).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, _) = world
        .send(&world.owner, "GET", &format!("/p/{token}"), None)
        .await;

    assert_eq!(
        status,
        StatusCode::GONE,
        "an archived doc's link acts revoked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn my_tasks_skip_archived_docs() {
    let world = world().await;
    let live = world.create_doc(&world.owner, "Live", None).await;
    let gone = world.create_doc(&world.owner, "Gone", None).await;
    let pool = world.state.pool.clone().unwrap();
    let workspace_id: Uuid = sqlx::query_scalar("SELECT id FROM workspaces LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    for doc in [&live, &gone] {
        let doc_id: Uuid = doc.parse().unwrap();
        sqlx::query(
            "INSERT INTO doc_tasks (id, workspace_id, doc_id, item_index, text, assignee_user_id)
             VALUES ($1, $2, $3, 0, 'do it', $4)",
        )
        .bind(format!("{doc_id}:0"))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(world.owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    }
    assert_eq!(
        world.archive(&world.owner, &gone).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, tasks) = world
        .send(&world.owner, "GET", "/api/workspace/tasks", None)
        .await;

    assert_eq!(status, StatusCode::OK);
    let docs: Vec<&str> = tasks
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["doc_id"].as_str().unwrap())
        .collect();
    assert_eq!(docs, vec![live.as_str()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_by_editor_brings_the_folder_back() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    let folder = world.create_doc(&world.owner, "Folder", None).await;
    let child = world.create_doc(&world.owner, "Child", Some(&folder)).await;
    assert_eq!(
        world.archive(&world.owner, &folder).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, body) = world
        .send(
            &editor,
            "POST",
            &format!("/api/docs/{folder}/restore"),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = world
        .send(
            &editor,
            "PATCH",
            &format!("/api/docs/{child}"),
            Some(json!({"title": "Writable again"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "restored docs are writable at once");
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_errors() {
    let world = world().await;
    let viewer = world.member("vi@archive.test", "Vi", "viewer").await;
    let folder = world.create_doc(&world.owner, "Folder", None).await;
    let child = world.create_doc(&world.owner, "Child", Some(&folder)).await;
    let live = world.create_doc(&world.owner, "Live", None).await;
    assert_eq!(
        world.archive(&world.owner, &folder).await.0,
        StatusCode::NO_CONTENT
    );
    let restore = |doc: &str| format!("/api/docs/{doc}/restore");

    let (status, body) = world
        .send(&world.owner, "POST", &restore(&live), None)
        .await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::NOT_FOUND, "doc.not_found")
    );
    let (status, body) = world
        .send(&world.owner, "POST", &restore(&child), None)
        .await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.not_archive_root")
    );
    let (status, body) = world.send(&viewer, "POST", &restore(&folder), None).await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::NOT_FOUND, "doc.not_found")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_is_owner_only_and_needs_the_archive() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    let folder = world.create_doc(&world.owner, "Folder", None).await;
    let child = world.create_doc(&world.owner, "Child", Some(&folder)).await;
    let purge = |doc: &str| format!("/api/docs/{doc}/purge");

    let (status, body) = world
        .send(&world.owner, "POST", &purge(&folder), None)
        .await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.not_archived")
    );
    assert_eq!(
        world.archive(&world.owner, &folder).await.0,
        StatusCode::NO_CONTENT
    );
    let (status, body) = world.send(&editor, "POST", &purge(&folder), None).await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::FORBIDDEN, "acl.owner_required")
    );
    let (status, body) = world.send(&world.owner, "POST", &purge(&child), None).await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.not_archive_root")
    );

    let (status, body) = world
        .send(&world.owner, "POST", &purge(&folder), None)
        .await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let docs = world.state.docs.as_ref().unwrap();
    for doc in [&folder, &child] {
        assert!(
            docs.get(doc.parse().unwrap()).await.unwrap().is_none(),
            "{doc} kept"
        );
    }
    let (_, list) = world
        .send(&world.owner, "GET", "/api/workspace/archive", None)
        .await;
    assert_eq!(list, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_list_contents_and_gating() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed Itor", "editor").await;
    let viewer = world.member("vi@archive.test", "Vi", "viewer").await;
    let folder = world.create_doc(&editor, "Folder", None).await;
    world.create_doc(&editor, "Child", Some(&folder)).await;
    assert_eq!(
        world.archive(&editor, &folder).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, list) = world
        .send(&editor, "GET", "/api/workspace/archive", None)
        .await;

    assert_eq!(status, StatusCode::OK, "{list}");
    let entries = list.as_array().unwrap();
    assert_eq!(entries.len(), 1, "roots only: {list}");
    let entry = &entries[0];
    assert_eq!(entry["id"], json!(folder));
    assert_eq!(entry["title"], json!("Folder"));
    assert_eq!(entry["parent_id"], Value::Null);
    assert_eq!(entry["parent_title"], Value::Null);
    assert_eq!(entry["subpage_count"], json!(1));
    assert_eq!(entry["archived_by"]["display_name"], json!("Ed Itor"));
    let archived_at =
        chrono::DateTime::parse_from_rfc3339(entry["archived_at"].as_str().unwrap()).unwrap();
    let purge_at =
        chrono::DateTime::parse_from_rfc3339(entry["purge_at"].as_str().unwrap()).unwrap();
    assert_eq!(purge_at - archived_at, chrono::Duration::days(30));

    let (status, body) = world
        .send(&viewer, "GET", "/api/workspace/archive", None)
        .await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::FORBIDDEN, "acl.editor_required")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_reports_archive_fields() {
    let world = world().await;
    let folder = world.create_doc(&world.owner, "Folder", None).await;
    let child = world.create_doc(&world.owner, "Child", Some(&folder)).await;
    let live = world.create_doc(&world.owner, "Live", None).await;
    assert_eq!(
        world.archive(&world.owner, &folder).await.0,
        StatusCode::NO_CONTENT
    );
    let get = |doc: &str| format!("/api/docs/{doc}");

    let (_, root) = world.send(&world.owner, "GET", &get(&folder), None).await;
    let (_, member) = world.send(&world.owner, "GET", &get(&child), None).await;
    let (_, alive) = world.send(&world.owner, "GET", &get(&live), None).await;

    assert!(root["archived_at"].is_string(), "{root}");
    assert!(root["purge_at"].is_string());
    assert_eq!(
        root["archived_by"]["id"],
        json!(world.owner.user_id.to_string())
    );
    assert_eq!(root["archived_by"]["display_name"], json!("Olive Owner"));
    assert_eq!(root["archive_root_id"], json!(folder));
    assert_eq!(member["archive_root_id"], json!(folder));
    assert_eq!(member["archive_root_title"], json!("Folder"));
    for field in [
        "archived_at",
        "archived_by",
        "purge_at",
        "archive_root_id",
        "archive_root_title",
    ] {
        assert_eq!(alive[field], Value::Null, "{field} on a live doc");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_reports_retention() {
    let world = world().await;
    let (status, body) = world
        .send(&world.owner, "GET", "/api/workspace", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["archive_retention_days"], json!(30));
}

// ---------------------------------------------------------------------------
// Retention job
// ---------------------------------------------------------------------------

/// Blob backend double: remembers deletes, optionally fails them (an S3
/// outage, say), and can claim its bytes live in the database (Postgres).
struct ScriptedBlobStore {
    fail_deletes: bool,
    bytes_in_database: bool,
    deleted: std::sync::Mutex<Vec<Uuid>>,
}

#[async_trait::async_trait]
impl knot_storage::BlobStore for ScriptedBlobStore {
    async fn put(&self, _id: Uuid, _bytes: &[u8], _ct: &str) -> knot_storage::blobs::Result<()> {
        Ok(())
    }
    async fn get(&self, _id: Uuid) -> knot_storage::blobs::Result<Vec<u8>> {
        Err(knot_storage::BlobStoreError::NotFound)
    }
    async fn delete(&self, id: Uuid) -> knot_storage::blobs::Result<()> {
        if self.fail_deletes {
            return Err(knot_storage::BlobStoreError::Backend(
                "bucket unavailable".into(),
            ));
        }
        self.deleted.lock().unwrap().push(id);
        Ok(())
    }
    fn bytes_deleted_with_metadata(&self) -> bool {
        self.bytes_in_database
    }
}

fn with_blob_store(state: &AppState, fail_deletes: bool) -> (AppState, Arc<ScriptedBlobStore>) {
    with_scripted_store(state, fail_deletes, false)
}

fn with_scripted_store(
    state: &AppState,
    fail_deletes: bool,
    bytes_in_database: bool,
) -> (AppState, Arc<ScriptedBlobStore>) {
    let store = Arc::new(ScriptedBlobStore {
        fail_deletes,
        bytes_in_database,
        deleted: std::sync::Mutex::new(Vec::new()),
    });
    let mut state = state.clone();
    state.blob_store = Some(store.clone());
    (state, store)
}

/// Archive `doc` and backdate the whole archive set by `days`.
async fn archive_days_ago(world: &World, doc: &str, days: i32) {
    assert_eq!(
        world.archive(&world.owner, doc).await.0,
        StatusCode::NO_CONTENT
    );
    sqlx::query(
        "UPDATE documents SET archived_at = now() - make_interval(days => $2)
          WHERE archive_root_id = $1",
    )
    .bind(doc.parse::<Uuid>().unwrap())
    .bind(days)
    .execute(world.state.pool.as_ref().unwrap())
    .await
    .unwrap();
}

async fn attach_blob(world: &World, doc: &str) -> Uuid {
    let meta = knot_storage::BlobMetadata {
        id: Uuid::new_v4(),
        workspace_id: sqlx::query_scalar("SELECT workspace_id FROM documents WHERE id = $1")
            .bind(doc.parse::<Uuid>().unwrap())
            .fetch_one(world.state.pool.as_ref().unwrap())
            .await
            .unwrap(),
        doc_id: doc.parse().unwrap(),
        content_type: "image/png".into(),
        byte_size: 1,
        sha256: vec![0; 32],
        original_name: None,
        created_by: world.owner.user_id,
        created_at: chrono::Utc::now(),
    };
    world
        .state
        .blob_meta
        .as_ref()
        .unwrap()
        .insert(&meta)
        .await
        .unwrap();
    meta.id
}

async fn doc_exists(world: &World, doc: &str) -> bool {
    world
        .state
        .docs
        .as_ref()
        .unwrap()
        .get(doc.parse().unwrap())
        .await
        .unwrap()
        .is_some()
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_job_removes_due_roots_and_their_blobs() {
    use knot_server::archive::{PurgeRun, run_purge_once};
    let world = world().await;
    let old = world.create_doc(&world.owner, "Old", None).await;
    let old_child = world
        .create_doc(&world.owner, "Old child", Some(&old))
        .await;
    let recent = world.create_doc(&world.owner, "Recent", None).await;
    let live = world.create_doc(&world.owner, "Live", None).await;
    let child_blob = attach_blob(&world, &old_child).await;
    attach_blob(&world, &recent).await;
    archive_days_ago(&world, &old, 31).await;
    archive_days_ago(&world, &recent, 29).await;
    let (state, blobs) = with_blob_store(&world.state, false);

    let run = run_purge_once(&state, chrono::Duration::days(30))
        .await
        .unwrap();

    assert_eq!(
        run,
        PurgeRun::Done {
            purged: 1,
            failed: 0
        }
    );
    assert!(!doc_exists(&world, &old).await && !doc_exists(&world, &old_child).await);
    assert!(doc_exists(&world, &recent).await && doc_exists(&world, &live).await);
    assert_eq!(
        *blobs.deleted.lock().unwrap(),
        vec![child_blob],
        "bytes deleted first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_keeps_rows_when_blob_delete_fails() {
    use knot_server::archive::{PurgeRun, run_purge_once};
    let world = world().await;
    let doc = world.create_doc(&world.owner, "Doc", None).await;
    let blob = attach_blob(&world, &doc).await;
    archive_days_ago(&world, &doc, 31).await;
    let (state, _) = with_blob_store(&world.state, true);

    let run = run_purge_once(&state, chrono::Duration::days(30))
        .await
        .unwrap();

    assert_eq!(
        run,
        PurgeRun::Done {
            purged: 0,
            failed: 1
        }
    );
    assert!(doc_exists(&world, &doc).await, "retried on the next run");
    let blob_row = world
        .state
        .blob_meta
        .as_ref()
        .unwrap()
        .find(blob)
        .await
        .unwrap();
    assert!(
        blob_row.is_some(),
        "metadata kept so the next run finds the bytes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_job_skips_when_another_replica_holds_the_lock() {
    use knot_server::archive::{PURGE_LOCK_KEY, PurgeRun, run_purge_once};
    let world = world().await;
    let doc = world.create_doc(&world.owner, "Doc", None).await;
    archive_days_ago(&world, &doc, 31).await;
    let mut other_replica = world.state.pool.as_ref().unwrap().acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(PURGE_LOCK_KEY)
        .execute(&mut *other_replica)
        .await
        .unwrap();

    let run = run_purge_once(&world.state, chrono::Duration::days(30))
        .await
        .unwrap();

    assert_eq!(run, PurgeRun::Skipped);
    assert!(doc_exists(&world, &doc).await);
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PURGE_LOCK_KEY)
        .execute(&mut *other_replica)
        .await
        .unwrap();
    let run = run_purge_once(&world.state, chrono::Duration::days(30))
        .await
        .unwrap();
    assert_eq!(
        run,
        PurgeRun::Done {
            purged: 1,
            failed: 0
        },
        "lock released, runs again"
    );
}

/// A manual purge whose S3 deletes fail part-way leaves the doc fenced: it
/// can't be restored (its attachments may be partly gone), the archive says
/// so, and the next job run finishes the purge without waiting out the
/// retention period.
#[tokio::test(flavor = "multi_thread")]
async fn failed_manual_purge_is_fenced_and_finished_by_the_job() {
    use knot_server::archive::{PurgeRun, run_purge_once};
    let world = world().await;
    let doc = world.create_doc(&world.owner, "Secret", None).await;
    attach_blob(&world, &doc).await;
    assert_eq!(
        world.archive(&world.owner, &doc).await.0,
        StatusCode::NO_CONTENT
    );
    let (failing, _) = with_blob_store(&world.state, true);
    let failing_app = router_with_state(failing);

    let (status, _) = send_via(
        &failing_app,
        &world.owner,
        "POST",
        &format!("/api/docs/{doc}/purge"),
        "application/json",
        String::new(),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, body) = world
        .send(
            &world.owner,
            "POST",
            &format!("/api/docs/{doc}/restore"),
            None,
        )
        .await;
    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.purging")
    );
    let (_, list) = world
        .send(&world.owner, "GET", "/api/workspace/archive", None)
        .await;
    assert_eq!(list[0]["purging"], json!(true), "{list}");
    let (working, _) = with_blob_store(&world.state, false);
    let run = run_purge_once(&working, chrono::Duration::days(30))
        .await
        .unwrap();
    assert_eq!(
        run,
        PurgeRun::Done {
            purged: 1,
            failed: 0
        }
    );
    assert!(!doc_exists(&world, &doc).await);
}

/// With the Postgres backend the bytes are rows that cascade inside the
/// purge transaction; deleting them first, outside it, would only add a
/// window where a doc exists without its attachments.
#[tokio::test(flavor = "multi_thread")]
async fn purge_leaves_database_held_bytes_to_the_cascade() {
    use knot_server::archive::run_purge_once;
    let world = world().await;
    let doc = world.create_doc(&world.owner, "Doc", None).await;
    let blob = attach_blob(&world, &doc).await;
    archive_days_ago(&world, &doc, 31).await;
    let (state, blobs) = with_scripted_store(&world.state, false, true);

    run_purge_once(&state, chrono::Duration::days(30))
        .await
        .unwrap();

    assert!(
        blobs.deleted.lock().unwrap().is_empty(),
        "no separate deletes"
    );
    assert!(!doc_exists(&world, &doc).await);
    let blob_row = world
        .state
        .blob_meta
        .as_ref()
        .unwrap()
        .find(blob)
        .await
        .unwrap();
    assert!(
        blob_row.is_none(),
        "metadata (and with it the bytes) cascaded"
    );
}

// ---- Restricted documents: archive, purge, restore and the archive list
// must neither act on nor name pages the caller can't see.

impl World {
    /// Mark `doc_id` restricted as `who`; the actor becomes its owner.
    async fn restrict(&self, who: &Session, doc_id: &str) {
        let (status, body) = self
            .send(
                who,
                "PUT",
                &format!("/api/docs/{doc_id}/restricted"),
                Some(json!({"restricted": true})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "restrict {doc_id}: {body}");
    }

    /// Give `grantee` `role` on `doc_id`, inheriting to its subpages.
    async fn grant(&self, who: &Session, doc_id: &str, grantee: &Session, role: &str) {
        let (status, body) = self
            .send(
                who,
                "PUT",
                &format!("/api/docs/{doc_id}/grants/user:{}", grantee.user_id),
                Some(json!({"role": role, "inherit": true})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "grant on {doc_id}: {body}");
    }

    async fn is_archived(&self, doc_id: &str) -> bool {
        self.state
            .docs
            .as_ref()
            .unwrap()
            .get(doc_id.parse().unwrap())
            .await
            .unwrap()
            .unwrap()
            .archived_at
            .is_some()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_with_hidden_restricted_descendant_is_409() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    let folder = world.create_doc(&world.owner, "Folder", None).await;
    let secret = world
        .create_doc(&world.owner, "Secret", Some(&folder))
        .await;
    world.restrict(&world.owner, &secret).await;

    let (status, body) = world.archive(&editor, &folder).await;

    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.hidden_descendants"),
        "{body}"
    );
    assert!(!world.is_archived(&folder).await);
    assert!(!world.is_archived(&secret).await);
    assert!(
        !body.to_string().contains(&secret),
        "the hidden page is not named: {body}"
    );
    let (status, body) = world.archive(&world.owner, &folder).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the owner of every page below may: {body}"
    );
    assert!(world.is_archived(&secret).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_with_hidden_restricted_descendant_is_409() {
    let world = world().await;
    let co_owner = world.member("co@archive.test", "Co", "owner").await;
    let folder = world.create_doc(&world.owner, "Folder", None).await;
    let secret = world
        .create_doc(&world.owner, "Secret", Some(&folder))
        .await;
    world.restrict(&world.owner, &secret).await;
    assert_eq!(
        world.archive(&world.owner, &folder).await.0,
        StatusCode::NO_CONTENT
    );
    let purge = format!("/api/docs/{folder}/purge");

    let (status, body) = world.send(&co_owner, "POST", &purge, None).await;

    assert_eq!(
        (status, error_code(&body)),
        (StatusCode::CONFLICT, "doc.hidden_descendants"),
        "{body}"
    );
    assert!(doc_exists(&world, &secret).await);
    let (status, body) = world.send(&world.owner, "POST", &purge, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!doc_exists(&world, &secret).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn restore_refuses_to_reparent_out_of_restricted_subtree() {
    let world = world().await;
    let boundary = world.create_doc(&world.owner, "Boundary", None).await;
    world.restrict(&world.owner, &boundary).await;
    let parent = world
        .create_doc(&world.owner, "Parent", Some(&boundary))
        .await;
    let child = world.create_doc(&world.owner, "Child", Some(&parent)).await;
    assert_eq!(
        world.archive(&world.owner, &child).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        world.archive(&world.owner, &parent).await.0,
        StatusCode::NO_CONTENT
    );
    let restore = |doc: &str| format!("/api/docs/{doc}/restore");

    let (status, body) = world
        .send(&world.owner, "POST", &restore(&child), None)
        .await;

    assert_eq!(
        (status, error_code(&body)),
        (
            StatusCode::CONFLICT,
            "doc.restore_restricted_parent_archived"
        ),
        "{body}"
    );
    assert!(world.is_archived(&child).await, "still in the archive");
    let docs = world.state.docs.as_ref().unwrap();
    let child_doc = docs.get(child.parse().unwrap()).await.unwrap().unwrap();
    assert_eq!(
        child_doc.parent_id.map(|id| id.to_string()),
        Some(parent.clone()),
        "not moved to the top level"
    );
    // Restoring the archived ancestor first makes room for it.
    for doc in [&parent, &child] {
        let (status, body) = world.send(&world.owner, "POST", &restore(doc), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "restore {doc}: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_list_hides_restricted_roots_and_parent_titles() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    // A hidden folder: Ed may edit one page in it, and only see another.
    let boundary = world.create_doc(&world.owner, "Hidden Folder", None).await;
    world.restrict(&world.owner, &boundary).await;
    let shared = world
        .create_doc(&world.owner, "Shared Page", Some(&boundary))
        .await;
    world.grant(&world.owner, &shared, &editor, "editor").await;
    let seen = world
        .create_doc(&world.owner, "Seen Page", Some(&boundary))
        .await;
    world.grant(&world.owner, &seen, &editor, "viewer").await;
    let private = world
        .create_doc(&world.owner, "Private Page", Some(&boundary))
        .await;
    // An open folder with one hidden subpage.
    let folder = world.create_doc(&world.owner, "Open Folder", None).await;
    world.create_doc(&world.owner, "Plain", Some(&folder)).await;
    let inner = world
        .create_doc(&world.owner, "Inner Secret", Some(&folder))
        .await;
    world.restrict(&world.owner, &inner).await;
    for doc in [&shared, &seen, &private, &folder] {
        assert_eq!(
            world.archive(&world.owner, doc).await.0,
            StatusCode::NO_CONTENT
        );
    }

    let (status, list) = world
        .send(&editor, "GET", "/api/workspace/archive", None)
        .await;

    assert_eq!(status, StatusCode::OK, "{list}");
    let entries = list.as_array().unwrap();
    let ids: Vec<&str> = entries
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{list}");
    assert!(ids.contains(&shared.as_str()) && ids.contains(&folder.as_str()));
    assert_eq!(entry_of(entries, &shared)["parent_id"], Value::Null);
    assert_eq!(entry_of(entries, &shared)["parent_title"], Value::Null);
    assert_eq!(entry_of(entries, &folder)["subpage_count"], json!(1));
    for hidden in [&boundary, &private, &inner] {
        assert!(
            !list.to_string().contains(hidden.as_str()),
            "{hidden} named"
        );
    }
    for title in ["Hidden Folder", "Private Page", "Inner Secret", "Seen Page"] {
        assert!(!list.to_string().contains(title), "{title} named: {list}");
    }
    // The owner, who holds every grant, still sees it all.
    let (_, list) = world
        .send(&world.owner, "GET", "/api/workspace/archive", None)
        .await;
    let entries = list.as_array().unwrap();
    assert_eq!(entries.len(), 4, "{list}");
    assert_eq!(
        entry_of(entries, &shared)["parent_title"],
        json!("Hidden Folder")
    );
    assert_eq!(entry_of(entries, &folder)["subpage_count"], json!(2));
}

fn entry_of<'a>(entries: &'a [Value], doc_id: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| entry["id"] == json!(doc_id))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_banner_does_not_name_unreadable_root() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    let boundary = world.create_doc(&world.owner, "Hidden Folder", None).await;
    world.restrict(&world.owner, &boundary).await;
    let child = world
        .create_doc(&world.owner, "Shared Page", Some(&boundary))
        .await;
    world.grant(&world.owner, &child, &editor, "editor").await;
    assert_eq!(
        world.archive(&world.owner, &boundary).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, body) = world
        .send(&editor, "GET", &format!("/api/docs/{child}"), None)
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["archived_at"].is_string(), "{body}");
    assert_eq!(body["archive_root_id"], Value::Null);
    assert_eq!(body["archive_root_title"], Value::Null);
    assert!(!body.to_string().contains(&boundary), "{body}");
    assert!(!body.to_string().contains("Hidden Folder"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn my_tasks_skip_restricted_docs() {
    let world = world().await;
    let editor = world.member("ed@archive.test", "Ed", "editor").await;
    let live = world.create_doc(&world.owner, "Live", None).await;
    let secret = world.create_doc(&world.owner, "Secret", None).await;
    let secret_child = world
        .create_doc(&world.owner, "Secret child", Some(&secret))
        .await;
    let pool = world.state.pool.clone().unwrap();
    let workspace_id: Uuid = sqlx::query_scalar("SELECT id FROM workspaces LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    for doc in [&live, &secret, &secret_child] {
        let doc_id: Uuid = doc.parse().unwrap();
        sqlx::query(
            "INSERT INTO doc_tasks (id, workspace_id, doc_id, item_index, text, assignee_user_id)
             VALUES ($1, $2, $3, 0, 'do it', $4)",
        )
        .bind(format!("{doc_id}:0"))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(editor.user_id)
        .execute(&pool)
        .await
        .unwrap();
    }
    world.restrict(&world.owner, &secret).await;

    let (status, tasks) = world
        .send(&editor, "GET", "/api/workspace/tasks", None)
        .await;

    assert_eq!(status, StatusCode::OK);
    let listed: Vec<(&str, &str)> = tasks
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["doc_id"].as_str().unwrap(),
                t["doc_title"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(listed, vec![(live.as_str(), "Live")]);
}
