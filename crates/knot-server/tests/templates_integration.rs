//! Integration: POST /api/docs/from-template/{id} clones the template's
//! markdown into a brand-new doc. The clone must be a fresh CRDT
//! lineage — comments/history don't carry over — but the content
//! must match the template's exported markdown.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Engine, Event, Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use tower::ServiceExt;
use uuid::Uuid;

async fn seeded() -> (AppState, Uuid, Uuid, Uuid, String) {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = s.hasher.hash("hunter22").unwrap();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "WS")
        .await
        .unwrap();
    let owner = s
        .users
        .as_ref()
        .unwrap()
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let tpl_doc = s
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Meeting notes", "m", owner.id)
        .await
        .unwrap();

    // Wire Rooms against the dev DB so ApplyUpdate persists.
    let bus: Arc<dyn knot_crdt::Bus> = Arc::new(knot_crdt::PgBus::connect(&db.url).await.unwrap());
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
    s.bus = Some(bus);
    s.rooms_v2 = Some(rooms.clone());

    // Seed the template's content via ApplyUpdate.
    let md = "# Agenda\n\n- topic one\n- topic two\n";
    let (_doc, update_bytes) = knot_markdown::from_markdown::parse(md).unwrap();
    let room = rooms.acquire(tpl_doc.id).await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(Event::ApplyUpdate {
            update_bytes,
            by_user: Some(owner.id),
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().expect("apply");

    // Flip the template flag.
    s.docs
        .as_ref()
        .unwrap()
        .set_template(ws.id, tpl_doc.id, owner.id, true)
        .await
        .unwrap();

    (s, ws.id, tpl_doc.id, owner.id, md.to_string())
}

async fn login(app: &axum::Router) -> (String, String) {
    login_as(app, "alice@example.com", "hunter22").await
}

/// Log in as `email` and return `(sid=…, csrf token)`.
async fn login_as(app: &axum::Router, email: &str, password: &str) -> (String, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let sid = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf = cookies
        .iter()
        .find(|c| c.starts_with("csrf="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .split('=')
        .nth(1)
        .unwrap()
        .to_string();
    (sid, csrf)
}

#[tokio::test(flavor = "multi_thread")]
async fn from_template_clones_markdown_into_new_doc() {
    let (state, ws_id, tpl_id, _user, src_md) = seeded().await;
    let app = router_with_state(state.clone());
    let (sid, csrf) = login(&app).await;

    // POST /api/docs/from-template/{id} with no title — server should
    // fall back to "<template title> copy".
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/docs/from-template/{tpl_id}"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", &csrf)
                .header("content-type", "application/json")
                .body(Body::from("{}".to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "from-template failed");
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let new_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new_id, tpl_id);
    assert_eq!(
        body["title"].as_str().unwrap(),
        "Meeting notes copy",
        "default title should be source + ' copy'"
    );
    assert!(!body["is_template"].as_bool().unwrap());

    // Pull the new doc's markdown back out and confirm it matches the
    // template's source. Round-trip is exact for this fixture.
    let rooms = state.rooms_v2.as_ref().unwrap();
    let room = rooms.acquire(new_id).await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx.send(Event::ExportState(tx)).await.unwrap();
    let (state_bytes, _seq) = rx.await.unwrap().unwrap();
    let engine = YrsEngine;
    let transient = engine.new_doc();
    engine.apply_update(&transient, &state_bytes).unwrap();
    let md = knot_markdown::to_markdown::serialise(&engine, &transient).unwrap();
    assert_eq!(md, src_md);

    // Mutating the new doc must not bleed into the template.
    let _ = ws_id; // unused — kept for context if extending the test.
    let mutate = "# Different content now\n";
    let (_d, mutate_bytes) = knot_markdown::from_markdown::parse(mutate).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    room.tx
        .send(Event::ReplaceWithMarkdown {
            update_bytes: mutate_bytes,
            by_user: None,
            reply: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().expect("replace");

    // Template content still matches its original markdown.
    let tpl_room = rooms.acquire(tpl_id).await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tpl_room.tx.send(Event::ExportState(tx)).await.unwrap();
    let (state_bytes, _seq) = rx.await.unwrap().unwrap();
    let transient = engine.new_doc();
    engine.apply_update(&transient, &state_bytes).unwrap();
    let tpl_md = knot_markdown::to_markdown::serialise(&engine, &transient).unwrap();
    assert_eq!(tpl_md, src_md, "mutating the clone changed the template");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_templates_endpoint_returns_marked_doc() {
    let (state, _ws, tpl_id, _user, _md) = seeded().await;
    let app = router_with_state(state);
    let (sid, csrf) = login(&app).await;
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/workspace/templates")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", &csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"].as_str().unwrap(), tpl_id.to_string());
    assert!(list[0]["is_template"].as_bool().unwrap());
}

/// `GET /api/workspace/templates` as the given session.
async fn list_templates(app: &axum::Router, sid: &str, csrf: &str) -> Vec<serde_json::Value> {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/workspace/templates")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn restricted_template_not_listed_for_non_grantee() {
    let (state, ws_id, tpl_id, owner_id, _md) = seeded().await;
    let bob_hash = state.hasher.hash("bobpass1").unwrap();
    let bob = state
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", &bob_hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws_id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let open_template = state
        .docs
        .as_ref()
        .unwrap()
        .create(ws_id, None, "Open template", "n", owner_id)
        .await
        .unwrap();
    state
        .docs
        .as_ref()
        .unwrap()
        .set_template(ws_id, open_template.id, owner_id, true)
        .await
        .unwrap();
    state
        .docs
        .as_ref()
        .unwrap()
        .set_restricted(ws_id, tpl_id, owner_id, true)
        .await
        .unwrap();
    let app = router_with_state(state);

    let (bob_sid, bob_csrf) = login_as(&app, "bob@example.com", "bobpass1").await;
    let bob_ids: Vec<String> = list_templates(&app, &bob_sid, &bob_csrf)
        .await
        .iter()
        .map(|template| template["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(bob_ids, vec![open_template.id.to_string()]);

    // The owner, who holds the restricted template's owner grant, sees both.
    let (alice_sid, alice_csrf) = login(&app).await;
    let alice_templates = list_templates(&app, &alice_sid, &alice_csrf).await;
    assert_eq!(alice_templates.len(), 2);
    let restricted_entry = alice_templates
        .iter()
        .find(|template| template["id"] == tpl_id.to_string().as_str())
        .expect("restricted template listed for its owner");
    assert_eq!(restricted_entry["restricted"], true);
}
