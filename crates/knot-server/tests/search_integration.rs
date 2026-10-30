use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::{WorkspaceRole, sort_key_between};
use tower::ServiceExt;
use uuid::Uuid;

// --- helpers ---

async fn state_with_seeded(role: WorkspaceRole) -> (AppState, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool.clone());
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.throttle = Arc::new(Throttle::new());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let hash = s.hasher.hash("hunter22").unwrap();
    let ws = s
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "Workspace")
        .await
        .unwrap();
    let user = s
        .users
        .as_ref()
        .unwrap()
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    s.workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, user.id, role)
        .await
        .unwrap();
    (s, ws.id, user.id)
}

/// A top-level page after the last one, so top-level keys stay distinct.
async fn make_doc(state: &AppState, workspace_id: Uuid, user_id: Uuid, title: &str) -> Uuid {
    let docs = state.docs.as_ref().unwrap();
    let last = docs.siblings(workspace_id, None).await.unwrap().pop();
    let key = sort_key_between(last.as_ref().map(|d| d.sort_key.as_str()), None);
    docs.create(workspace_id, None, title, &key, user_id)
        .await
        .unwrap()
        .id
}

async fn login_owner(app: &axum::Router) -> (String, String) {
    login_as(app, "alice@example.com").await
}

async fn login_as(app: &axum::Router, email: &str) -> (String, String) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"email": email, "password": "hunter22"}).to_string(),
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
    let sid_kv = cookies
        .iter()
        .find(|c| c.starts_with("sid="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let csrf_val = cookies
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
    (sid_kv, csrf_val)
}

/// Add a member with `role`, whose password is the seeded one.
async fn add_member(
    state: &AppState,
    workspace_id: Uuid,
    email: &str,
    name: &str,
    role: WorkspaceRole,
) -> Uuid {
    let hash = state.hasher.hash("hunter22").unwrap();
    let user = state
        .users
        .as_ref()
        .unwrap()
        .create_local(email, name, &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(workspace_id, user.id, role)
        .await
        .unwrap();
    user.id
}

/// A page under `parent_id`, after its last child.
async fn make_child(
    state: &AppState,
    workspace_id: Uuid,
    user_id: Uuid,
    parent_id: Uuid,
    title: &str,
) -> Uuid {
    let docs = state.docs.as_ref().unwrap();
    let last = docs
        .siblings(workspace_id, Some(parent_id))
        .await
        .unwrap()
        .pop();
    let key = sort_key_between(last.as_ref().map(|d| d.sort_key.as_str()), None);
    docs.create(workspace_id, Some(parent_id), title, &key, user_id)
        .await
        .unwrap()
        .id
}

/// Mark `doc_id` restricted; `actor` becomes its owner.
async fn restrict(state: &AppState, workspace_id: Uuid, doc_id: Uuid, actor: Uuid) {
    state
        .docs
        .as_ref()
        .unwrap()
        .set_restricted(workspace_id, doc_id, actor, true)
        .await
        .unwrap();
}

fn hit_ids(body: &serde_json::Value) -> Vec<String> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["doc_id"].as_str().unwrap().to_string())
        .collect()
}

/// Percent-encodes spaces only — sufficient for single-word test queries.
fn encode_q(q: &str) -> String {
    q.replace(' ', "%20")
}

async fn do_search(app: &axum::Router, sid_kv: &str, q: &str) -> (StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/search?q={}", encode_q(q)))
                .header("cookie", sid_kv)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
    (status, json)
}

// --- cases ---

#[tokio::test(flavor = "multi_thread")]
async fn title_match_returns_doc() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Hello World").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    let (status, body) = do_search(&app, &sid, "hello").await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "Hello World");
    // Title-only match → empty snippet.
    assert_eq!(results[0]["snippet"], "");
    assert!(results[0]["rank"].as_f64().unwrap() > 0.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn body_match_returns_doc_with_snippet() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    let doc_id = make_doc(&state, ws, uid, "Plain Title").await;
    // Seed body via the markdown cache directly.
    state
        .markdown_cache
        .as_ref()
        .unwrap()
        .put(doc_id, 1, "alpha beta gamma delta epsilon")
        .await
        .unwrap();
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    let (status, body) = do_search(&app, &sid, "gamma").await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let snippet = results[0]["snippet"].as_str().unwrap();
    assert!(
        snippet.contains("<b>"),
        "snippet missing <b> highlight: {snippet}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_match_returns_empty_list() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Hello World").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    let (status, body) = do_search(&app, &sid, "xyzzy").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn short_query_returns_empty_without_db() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Hello").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    // Single-char query — below MIN_QUERY_LEN=2.
    let (status, body) = do_search(&app, &sid, "h").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn anon_returns_401() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Hello").await;
    let app = router_with_state(state);
    let r = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/search?q=hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn results_capped_at_max_limit() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    for i in 0..25 {
        make_doc(&state, ws, uid, &format!("foo doc {i}")).await;
    }
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    let (status, body) = do_search(&app, &sid, "foo").await;
    assert_eq!(status, StatusCode::OK);
    let len = body["results"].as_array().unwrap().len();
    assert_eq!(
        len, 20,
        "results should be capped at MAX_LIMIT (20), got {len}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prefix_match_finds_doc_by_word_start() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Findable World").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    let (status, body) = do_search(&app, &sid, "find").await;
    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "prefix 'find' should match 'Findable'");
    assert_eq!(results[0]["title"], "Findable World");
}

#[tokio::test(flavor = "multi_thread")]
async fn special_chars_dont_crash_query() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Hello world").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    // to_tsquery would panic on these without sanitization.
    for needle in ["!@#$%", "foo & bar", "'; DROP TABLE", "a:* | b"] {
        let (status, _) = do_search(&app, &sid, needle).await;
        assert_eq!(status, StatusCode::OK, "query {needle:?} should not 500");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn multi_word_prefix_is_and() {
    let (state, ws, uid) = state_with_seeded(WorkspaceRole::Owner).await;
    make_doc(&state, ws, uid, "Alphabet Soup").await;
    make_doc(&state, ws, uid, "Beta Snack").await;
    let app = router_with_state(state);
    let (sid, _csrf) = login_owner(&app).await;
    // Both prefixes must match the same doc.
    let (_, body) = do_search(&app, &sid, "alph soup").await;
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "Alphabet Soup");
}

#[tokio::test(flavor = "multi_thread")]
async fn search_never_returns_restricted_doc_to_non_grantee() {
    let (state, ws, alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_member(&state, ws, "bob@example.com", "Bob", WorkspaceRole::Owner).await;
    let open_doc = make_doc(&state, ws, alice, "Quarterly open").await;
    let secret = make_doc(&state, ws, alice, "Quarterly secret").await;
    let secret_child = make_child(&state, ws, alice, secret, "Child page").await;
    state
        .markdown_cache
        .as_ref()
        .unwrap()
        .put(secret_child, 1, "the quarterly numbers live here")
        .await
        .unwrap();
    restrict(&state, ws, secret, alice).await;
    let app = router_with_state(state);

    let (bob_sid, _) = login_as(&app, "bob@example.com").await;
    let (status, body) = do_search(&app, &bob_sid, "quarterly").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        hit_ids(&body),
        vec![open_doc.to_string()],
        "a workspace owner without a grant sees neither the boundary nor its subpage: {body}"
    );

    let (alice_sid, _) = login_owner(&app).await;
    let (_, body) = do_search(&app, &alice_sid, "quarterly").await;
    let mut alice_hits = hit_ids(&body);
    alice_hits.sort();
    let mut expected = vec![
        open_doc.to_string(),
        secret.to_string(),
        secret_child.to_string(),
    ];
    expected.sort();
    assert_eq!(alice_hits, expected, "the grantee sees all three: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn search_is_not_starved_by_restricted_hits() {
    let (state, ws, alice) = state_with_seeded(WorkspaceRole::Owner).await;
    add_member(&state, ws, "bob@example.com", "Bob", WorkspaceRole::Editor).await;
    for index in 0..45 {
        let restricted_doc = make_doc(&state, ws, alice, &format!("Zebra {index}")).await;
        restrict(&state, ws, restricted_doc, alice).await;
    }
    let readable = make_doc(&state, ws, alice, "Plain").await;
    state
        .markdown_cache
        .as_ref()
        .unwrap()
        .put(readable, 1, "a zebra crossing in the body")
        .await
        .unwrap();
    let app = router_with_state(state);

    let (bob_sid, _) = login_as(&app, "bob@example.com").await;
    let (status, body) = do_search(&app, &bob_sid, "zebra").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(hit_ids(&body), vec![readable.to_string()], "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn deep_grantee_search_hit_has_no_parent_id() {
    let (state, ws, alice) = state_with_seeded(WorkspaceRole::Owner).await;
    let bob = add_member(&state, ws, "bob@example.com", "Bob", WorkspaceRole::Editor).await;
    let vault = make_doc(&state, ws, alice, "Vault").await;
    let deep = make_child(&state, ws, alice, vault, "Deep note").await;
    restrict(&state, ws, vault, alice).await;
    state
        .grants
        .as_ref()
        .unwrap()
        .put(
            ws,
            deep,
            &format!("user:{bob}"),
            WorkspaceRole::Viewer,
            true,
            alice,
        )
        .await
        .unwrap();
    let app = router_with_state(state);

    let (bob_sid, _) = login_as(&app, "bob@example.com").await;
    let (status, body) = do_search(&app, &bob_sid, "deep").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hit_ids(&body), vec![deep.to_string()], "{body}");
    assert!(body["results"][0]["parent_id"].is_null(), "{body}");
    assert!(
        !body.to_string().contains(&vault.to_string()),
        "the hidden boundary's id never reaches the grantee: {body}"
    );

    let (alice_sid, _) = login_owner(&app).await;
    let (_, body) = do_search(&app, &alice_sid, "deep").await;
    assert_eq!(
        body["results"][0]["parent_id"],
        vault.to_string(),
        "a reader of the parent still gets it: {body}"
    );
}
