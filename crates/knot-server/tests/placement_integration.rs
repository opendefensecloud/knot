//! Where new and moved pages land among their siblings, over HTTP against a
//! fresh database per test.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_server::{AppState, router_with_state};
use knot_storage::{WorkspaceRole, lexorank};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const EMAIL: &str = "owner@placement.test";
const PASSWORD: &str = "hunter22hunter22";

/// The router plus a logged-in owner's cookie header and CSRF token, and the
/// database, for seeding rows the API would never write.
#[derive(Clone)]
struct World {
    app: axum::Router,
    cookie: String,
    csrf: String,
    pool: PgPool,
    workspace: Uuid,
    owner: Uuid,
}

async fn world() -> World {
    let pool = knot_test_support::fresh_db().await.pool;
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
        owner: owner.id,
    }
}

impl World {
    /// One request as the owner; the JSON body, or `Null` when there is none.
    async fn send(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("cookie", &self.cookie)
                    .header("x-csrf-token", &self.csrf)
                    .header("content-type", "application/json")
                    .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// `POST /api/docs`, asserting 201; returns the new page's id.
    async fn create(&self, body: Value) -> String {
        let (status, doc) = self.send("POST", "/api/docs", Some(body.clone())).await;
        assert_eq!(status, StatusCode::CREATED, "create {body}: {doc}");
        doc["id"].as_str().unwrap().to_string()
    }

    /// The live children of `parent` (`None`: the top level) as
    /// `(title, sort_key)`, in the sidebar's order: by key, code unit by
    /// code unit.
    async fn level(&self, parent: Option<&str>) -> Vec<(String, String)> {
        let (status, docs) = self.send("GET", "/api/docs", None).await;
        assert_eq!(status, StatusCode::OK, "list: {docs}");
        let mut level: Vec<(String, String)> = docs
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["parent_id"].as_str() == parent)
            .map(|d| {
                let title = d["title"].as_str().unwrap().to_string();
                (title, d["sort_key"].as_str().unwrap().to_string())
            })
            .collect();
        level.sort_by(|x, y| x.1.cmp(&y.1));
        level
    }

    /// A page inserted with raw SQL, so its key can be one the generator
    /// never writes (as an old release or the database before the heal could).
    async fn seed(&self, parent: Option<&str>, title: &str, key: &str) -> String {
        let parent = parent.map(|p| Uuid::parse_str(p).unwrap());
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(self.workspace)
        .bind(parent)
        .bind(title)
        .bind(key)
        .bind(self.owner)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        id.to_string()
    }

    /// `POST /api/docs/{id}/move` with `body`.
    async fn move_doc(&self, id: &str, body: Value) -> (StatusCode, Value) {
        self.send("POST", &format!("/api/docs/{id}/move"), Some(body))
            .await
    }

    /// Every key under `parent` (`None`: the top level), archived pages and
    /// templates included, by code unit.
    async fn keys(&self, parent: Option<&str>) -> Vec<String> {
        let parent = parent.map(|p| Uuid::parse_str(p).unwrap());
        sqlx::query_scalar(
            "SELECT sort_key FROM documents
              WHERE workspace_id = $1 AND parent_id IS NOT DISTINCT FROM $2
              ORDER BY sort_key COLLATE \"C\"",
        )
        .bind(self.workspace)
        .bind(parent)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

/// Asserts that every key under `parent`, archived pages included, is valid,
/// at most 64 characters long, and unique.
async fn assert_keys_healthy(w: &World, parent: Option<&str>) {
    let keys = w.keys(parent).await;
    assert!(
        keys.iter().all(|k| lexorank::is_valid(k) && k.len() <= 64),
        "bad keys under {parent:?}: {keys:?}"
    );
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "duplicate keys under {parent:?}: {keys:?}"
    );
}

/// The `error.code` of a JSON error body.
fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// Titles of `level`, after asserting its keys are pairwise distinct.
fn distinct_titles(level: &[(String, String)]) -> Vec<&str> {
    assert!(
        level.windows(2).all(|w| w[0].1 < w[1].1),
        "duplicate sort keys: {level:?}"
    );
    level.iter().map(|(title, _)| title.as_str()).collect()
}

/// "New page" under a parent puts each page first. The old generator's 8th
/// key duplicated the 7th, so the 8th create failed with 409.
#[tokio::test(flavor = "multi_thread")]
async fn ten_new_pages_under_a_parent_all_succeed_in_order() {
    let w = world().await;
    let parent = w.create(json!({"title": "Parent"})).await;
    for i in 1..=10 {
        w.create(json!({"title": format!("Page {i}"), "parent_id": parent}))
            .await;
    }
    let level = w.level(Some(&parent)).await;
    let expected: Vec<String> = (1..=10).rev().map(|i| format!("Page {i}")).collect();
    assert_eq!(distinct_titles(&level), expected);
}

/// The same at the top level, where the database accepted the duplicate key:
/// from the 7th page on, every new page got the same "0m" key.
#[tokio::test(flavor = "multi_thread")]
async fn ten_new_top_level_pages_get_distinct_ordered_keys() {
    let w = world().await;
    for i in 1..=10 {
        w.create(json!({"title": format!("Page {i}")})).await;
    }
    let level = w.level(None).await;
    let expected: Vec<String> = (1..=10).rev().map(|i| format!("Page {i}")).collect();
    assert_eq!(distinct_titles(&level), expected);
}

/// "Insert after" the first page, ten times: each new page goes right after
/// it. The old generator returned the neighbour's own key on the 7th try, so
/// that create failed with 409.
#[tokio::test(flavor = "multi_thread")]
async fn insert_after_first_ten_times() {
    let w = world().await;
    let parent = w.create(json!({"title": "Parent"})).await;
    let first = w
        .create(json!({"title": "First", "parent_id": parent}))
        .await;
    for i in 1..=10 {
        w.create(json!({
            "title": format!("After {i}"),
            "parent_id": parent,
            "after_id": first,
        }))
        .await;
    }
    let level = w.level(Some(&parent)).await;
    let mut expected = vec!["First".to_string()];
    expected.extend((1..=10).rev().map(|i| format!("After {i}")));
    assert_eq!(distinct_titles(&level), expected);
}

/// The parent of `id` as the API reports it.
async fn parent_of(w: &World, id: &str) -> Option<String> {
    let (status, doc) = w.send("GET", &format!("/api/docs/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "get {id}: {doc}");
    doc["parent_id"].as_str().map(str::to_string)
}

/// Keys from before the heal or from an old release, in the sidebar's order:
/// below the alphabet (`"0"`), a trailing '0', upper case, and over 64
/// characters. `between` rejects the first four as neighbours, and the next
/// key after the last would be 71 characters long.
fn bad_keys() -> [(&'static str, String); 5] {
    [
        ("zero", "0".to_string()),
        ("double zero", "00".to_string()),
        ("upper M", "M".to_string()),
        ("m zero", "m0".to_string()),
        ("long", "z".repeat(70)),
    ]
}

/// "New page" and moves next to keys the generator rejects. The request used
/// to panic (`sort_key_between` on `"0"`); now the group is respread first,
/// the request succeeds, every key ends up valid, and the order is kept.
#[tokio::test(flavor = "multi_thread")]
async fn placement_rebalances_invalid_neighbours() {
    enum Op {
        /// `POST /api/docs` under the group, after this page if any.
        Create(Option<&'static str>),
        /// Move a top-level page into the group, before this page.
        MoveBefore(&'static str),
        /// Move a top-level page into the group, after this page.
        MoveAfter(&'static str),
    }
    let cases = [
        (
            Op::Create(None),
            ["new", "zero", "double zero", "upper M", "m zero", "long"],
        ),
        (
            Op::Create(Some("zero")),
            ["zero", "new", "double zero", "upper M", "m zero", "long"],
        ),
        (
            Op::Create(Some("long")),
            ["zero", "double zero", "upper M", "m zero", "long", "new"],
        ),
        (
            Op::MoveBefore("upper M"),
            ["zero", "double zero", "new", "upper M", "m zero", "long"],
        ),
        (
            Op::MoveAfter("m zero"),
            ["zero", "double zero", "upper M", "m zero", "new", "long"],
        ),
    ];
    let w = world().await;
    for (op, expected) in cases {
        let parent = w.create(json!({"title": "Parent"})).await;
        let mut ids = HashMap::new();
        for (title, key) in bad_keys() {
            ids.insert(title, w.seed(Some(&parent), title, &key).await);
        }
        let (status, body) = match op {
            Op::Create(after) => {
                let mut body = json!({"title": "new", "parent_id": parent});
                if let Some(after) = after {
                    body["after_id"] = json!(ids[after]);
                }
                w.send("POST", "/api/docs", Some(body)).await
            }
            Op::MoveBefore(anchor) => {
                let mover = w.create(json!({"title": "new"})).await;
                w.move_doc(
                    &mover,
                    json!({"parent_id": parent, "before_id": ids[anchor]}),
                )
                .await
            }
            Op::MoveAfter(anchor) => {
                let mover = w.create(json!({"title": "new"})).await;
                w.move_doc(
                    &mover,
                    json!({"parent_id": parent, "after_id": ids[anchor]}),
                )
                .await
            }
        };
        assert!(status.is_success(), "{expected:?}: {status} {body}");
        assert_eq!(distinct_titles(&w.level(Some(&parent)).await), expected);
        assert_keys_healthy(&w, Some(&parent)).await;
    }
}

/// An archived page keeps its key and still counts towards uniqueness, so it
/// bounds the key of a page placed next to it. After "A", with "X" archived
/// between "A" and "B", the new key goes between "A" and "X"; between "A"
/// and "B" it would be "X"'s key. A regression guard: the routes' sibling
/// read (`siblings()`) already included archived pages.
#[tokio::test(flavor = "multi_thread")]
async fn archived_sibling_between_neighbours_does_not_collide() {
    let w = world().await;
    let parent = w.create(json!({"title": "Parent"})).await;
    let a = w.seed(Some(&parent), "A", "c").await;
    let x = w.seed(Some(&parent), "X", "i").await;
    w.seed(Some(&parent), "B", "o").await;
    let (status, body) = w.send("DELETE", &format!("/api/docs/{x}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "archive X: {body}");

    let (status, created) = w
        .send(
            "POST",
            "/api/docs",
            Some(json!({"title": "New", "parent_id": parent, "after_id": a})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "create after A: {created}");
    let mover = w.create(json!({"title": "Mover"})).await;
    let (status, moved) = w
        .move_doc(&mover, json!({"parent_id": parent, "after_id": a}))
        .await;
    assert_eq!(status, StatusCode::OK, "move after A: {moved}");

    let new_key = created["sort_key"].as_str().unwrap();
    let moved_key = moved["sort_key"].as_str().unwrap();
    assert!(
        "c" < moved_key && moved_key < new_key && new_key < "i",
        "A c < Mover {moved_key} < New {new_key} < X i"
    );
    assert_eq!(
        distinct_titles(&w.level(Some(&parent)).await),
        ["A", "Mover", "New", "B"]
    );
    assert_keys_healthy(&w, Some(&parent)).await;
}

/// Twenty "New page" requests at once, under a parent and at the top level.
/// Each read the first key outside any transaction, so several computed the
/// same key and failed with 409 `doc.sort_key_conflict`: under a parent
/// always, and at the top level since the heal made those keys unique.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_creates_get_distinct_keys() {
    let w = world().await;
    let parent = w.create(json!({"title": "Parent"})).await;
    for group in [Some(parent.as_str()), None] {
        let requests: Vec<_> = (0..20)
            .map(|i| {
                let w = w.clone();
                let mut body = json!({"title": format!("Page {i}")});
                if let Some(parent) = group {
                    body["parent_id"] = json!(parent);
                }
                tokio::spawn(async move { w.send("POST", "/api/docs", Some(body)).await })
            })
            .collect();
        for request in requests {
            let (status, body) = request.await.unwrap();
            assert_eq!(status, StatusCode::CREATED, "{group:?}: {body}");
        }
        let level = w.level(group).await;
        let pages = distinct_titles(&level)
            .into_iter()
            .filter(|t| t.starts_with("Page "))
            .count();
        assert_eq!(pages, 20, "{group:?}: {level:?}");
        assert_keys_healthy(&w, group).await;
    }
}

/// A move whose own group must be respread: `"1"` and `"0"` in front of
/// `spread(11)`, then the last page moves to the top. No key fits before
/// `"0"`, so the other twelve pages get `spread(12)`, whose last key is the
/// moving page's current one. The moving page takes a provisional key in the
/// same statement, so the respread can't collide with it.
#[tokio::test(flavor = "multi_thread")]
async fn rebalance_with_the_moved_page_in_its_group() {
    let w = world().await;
    let parent = w.create(json!({"title": "Parent"})).await;
    let mut keys = vec!["0".to_string(), "1".to_string()];
    keys.extend(lexorank::spread(11));
    assert_eq!(
        lexorank::spread(12).last(),
        keys.last(),
        "the respread hands the moving page's key to its neighbour"
    );
    let mut ids = Vec::new();
    for key in &keys {
        ids.push(w.seed(Some(&parent), key, key).await);
    }

    let (status, body) = w
        .move_doc(
            ids.last().unwrap(),
            json!({"parent_id": parent, "before_id": ids[0]}),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "move the last page to the top: {body}"
    );

    let mut expected: Vec<&str> = vec![keys.last().unwrap()];
    expected.extend(keys[..keys.len() - 1].iter().map(String::as_str));
    assert_eq!(distinct_titles(&w.level(Some(&parent)).await), expected);
    assert_keys_healthy(&w, Some(&parent)).await;
}

/// Two users swap nesting at once: D into P and P into D. One move wins and
/// the other is refused with 409 `doc.move_cycle`. Never a 500 from a
/// deadlock, and never both, which made D and P each other's parent and hid
/// both from the sidebar. A few rounds, each releasing both requests
/// together.
#[tokio::test(flavor = "multi_thread")]
async fn swapping_nesting_moves_do_not_deadlock() {
    let w = world().await;
    for round in 0..5 {
        let d = w.create(json!({"title": format!("D{round}")})).await;
        let p = w.create(json!({"title": format!("P{round}")})).await;
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let swap = |id: &str, parent: &str| {
            let (w, start) = (w.clone(), start.clone());
            let (id, body) = (id.to_string(), json!({ "parent_id": parent }));
            tokio::spawn(async move {
                start.wait().await;
                w.move_doc(&id, body).await
            })
        };
        let (d_into_p, p_into_d) = (swap(&d, &p), swap(&p, &d));
        let mut results = [d_into_p.await.unwrap(), p_into_d.await.unwrap()];
        results.sort_by_key(|(status, _)| status.as_u16());
        let [(won, _), (lost, refusal)] = &results;
        assert_eq!(*won, StatusCode::OK, "round {round}: {results:?}");
        assert_eq!(
            (*lost, code(refusal)),
            (StatusCode::CONFLICT, "doc.move_cycle"),
            "round {round}: {results:?}"
        );
        let nested = [
            parent_of(&w, &d).await == Some(p.clone()),
            parent_of(&w, &p).await == Some(d.clone()),
        ];
        assert!(
            nested == [true, false] || nested == [false, true],
            "round {round}: exactly one nests in the other, got {nested:?}"
        );
    }
}

/// A move anchored to a page outside the target group is refused instead of
/// landing at the end: a page under another parent, an archived sibling, and
/// a template.
#[tokio::test(flavor = "multi_thread")]
async fn move_with_non_sibling_anchor_is_422() {
    let w = world().await;
    let page = w.create(json!({"title": "Page"})).await;
    let folder = w.create(json!({"title": "Folder"})).await;
    let nested = w
        .create(json!({"title": "Nested", "parent_id": folder}))
        .await;
    let archived = w.create(json!({"title": "Archived"})).await;
    let (status, body) = w
        .send("DELETE", &format!("/api/docs/{archived}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "archive: {body}");
    let template = w.create(json!({"title": "Template"})).await;
    let (status, body) = w
        .send(
            "POST",
            &format!("/api/docs/{template}/template"),
            Some(json!({"is_template": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "mark template: {body}");
    let before = w.keys(None).await;

    for anchor in [&nested, &archived, &template] {
        for side in ["after_id", "before_id"] {
            let mut request = json!({"parent_id": null});
            request[side] = json!(anchor);
            let (status, body) = w.move_doc(&page, request).await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::UNPROCESSABLE_ENTITY, "doc.bad_anchor"),
                "{side} {anchor}: {body}"
            );
        }
    }
    assert_eq!(w.keys(None).await, before, "nothing moved");
}

/// A page can't be placed next to itself.
#[tokio::test(flavor = "multi_thread")]
async fn move_with_self_anchor_is_422() {
    let w = world().await;
    let page = w.create(json!({"title": "Page"})).await;
    w.create(json!({"title": "Other"})).await;
    for side in ["after_id", "before_id"] {
        let mut request = json!({"parent_id": null});
        request[side] = json!(page);
        let (status, body) = w.move_doc(&page, request).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNPROCESSABLE_ENTITY, "doc.bad_anchor"),
            "{side}: {body}"
        );
    }
}

/// "New page" after a page that isn't under the requested parent is refused
/// instead of landing at the end.
#[tokio::test(flavor = "multi_thread")]
async fn create_with_non_sibling_anchor_is_422() {
    let w = world().await;
    let top = w.create(json!({"title": "Top"})).await;
    let folder = w.create(json!({"title": "Folder"})).await;
    let (status, body) = w
        .send(
            "POST",
            "/api/docs",
            Some(json!({"title": "New", "parent_id": folder, "after_id": top})),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "doc.bad_anchor"),
        "{body}"
    );
    assert!(w.level(Some(&folder)).await.is_empty(), "nothing created");
}

/// Waits until `n` sessions on this test's database are waiting for a lock.
async fn wait_for_lock_waits(pool: &PgPool, n: i64) {
    for _ in 0..500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{n} sessions never waited for a lock together");
}

/// `DELETE /api/docs/{id}`, asserting 204: `id` goes to the archive.
async fn archive(w: &World, id: &str) {
    let (status, body) = w.send("DELETE", &format!("/api/docs/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "archive {id}: {body}");
}

/// Whether `id` is in the archive, and its parent, read from the database.
async fn archived_under(w: &World, id: &str) -> (bool, Option<Uuid>) {
    sqlx::query_as("SELECT archived_at IS NOT NULL, parent_id FROM documents WHERE id = $1")
        .bind(Uuid::parse_str(id).unwrap())
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// Inserts a page under `parent` with `key` in a transaction it leaves open,
/// as a writer that skips the group's lock (an old release during a rolling
/// upgrade) would. A placement's read of the group doesn't see the row, and
/// its write waits for this transaction.
async fn uncommitted_page(
    w: &World,
    parent: Option<&str>,
    key: &str,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut old_pod = w.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, $2, 'Old pod', $3, $4)",
    )
    .bind(w.workspace)
    .bind(parent.map(|p| Uuid::parse_str(p).unwrap()))
    .bind(key)
    .bind(w.owner)
    .execute(&mut *old_pod)
    .await
    .unwrap();
    old_pod
}

/// A writer that skips the group's lock takes the key restore picked, between
/// restore's read of the group and its write: a page whose parent is
/// archived goes last at the top level, and the writer takes that key first.
/// The route sent the unique violation to a 500; it is 409
/// `doc.sort_key_conflict` now, which the client can retry, and nothing is
/// restored.
#[tokio::test(flavor = "multi_thread")]
async fn restore_racing_an_unlocked_writer_for_its_key_is_409() {
    let w = world().await;
    let parent = w.seed(None, "Parent", "m").await;
    let child = w.seed(Some(&parent), "Child", "m").await;
    archive(&w, &child).await;
    archive(&w, &parent).await;
    // The key restore picks: last at the top level, after "m".
    let key = lexorank::between(Some("m"), None).unwrap();
    let old_pod = uncommitted_page(&w, None, &key).await;

    let request = {
        let (w, uri) = (w.clone(), format!("/api/docs/{child}/restore"));
        tokio::spawn(async move { w.send("POST", &uri, None).await })
    };
    // Restore's write waits for the old pod's row with the same key.
    wait_for_lock_waits(&w.pool, 1).await;
    old_pod.commit().await.unwrap();

    let (status, body) = request.await.unwrap();
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "doc.sort_key_conflict"),
        "{body}"
    );
    let parent = Uuid::parse_str(&parent).unwrap();
    assert_eq!(
        archived_under(&w, &child).await,
        (true, Some(parent)),
        "nothing was restored"
    );
}

/// Restore reads the page's parent, then locks the parent's row, then the
/// page's. When the page was restored, moved and archived again in between,
/// it is no longer where restore looked, and restore stops. The route sent
/// that to a 500 as well; it is 409 `doc.restore_conflict` now, a code of its
/// own because no key collided, and the page stays where the move put it.
#[tokio::test(flavor = "multi_thread")]
async fn restore_racing_a_move_of_the_page_is_409() {
    let w = world().await;
    let parent = w.seed(None, "Parent", "m").await;
    let elsewhere = w.seed(None, "Elsewhere", "t").await;
    let page = w.seed(Some(&parent), "Page", "m").await;
    archive(&w, &page).await;
    // Hold the parent's row, so restore waits for it after reading where
    // the page is.
    let mut other = w.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM documents WHERE id = $1 FOR UPDATE")
        .bind(Uuid::parse_str(&parent).unwrap())
        .execute(&mut *other)
        .await
        .unwrap();

    let request = {
        let (w, uri) = (w.clone(), format!("/api/docs/{page}/restore"));
        tokio::spawn(async move { w.send("POST", &uri, None).await })
    };
    wait_for_lock_waits(&w.pool, 1).await;
    // What restoring the page, moving it under Elsewhere and archiving it
    // again leaves behind: an archive root under another parent.
    let elsewhere = Uuid::parse_str(&elsewhere).unwrap();
    sqlx::query("UPDATE documents SET parent_id = $2 WHERE id = $1")
        .bind(Uuid::parse_str(&page).unwrap())
        .bind(elsewhere)
        .execute(&mut *other)
        .await
        .unwrap();
    other.commit().await.unwrap();

    let (status, body) = request.await.unwrap();
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "doc.restore_conflict"),
        "{body}"
    );
    assert_eq!(archived_under(&w, &page).await, (true, Some(elsewhere)));
}

/// A writer that skips the group's lock takes the key a move picked, between
/// the move's read of the group and its write. The unique violation is 409
/// `doc.sort_key_conflict`, as for restore, and the page stays where it was.
#[tokio::test(flavor = "multi_thread")]
async fn move_racing_an_unlocked_writer_for_its_key_is_409() {
    let w = world().await;
    let folder = w.seed(None, "Folder", "m").await;
    w.seed(Some(&folder), "Inside", "m").await;
    let page = w.seed(None, "Page", "t").await;
    // The key the move picks: last under Folder, after "m".
    let key = lexorank::between(Some("m"), None).unwrap();
    let old_pod = uncommitted_page(&w, Some(&folder), &key).await;

    let request = {
        let (w, page, body) = (w.clone(), page.clone(), json!({ "parent_id": folder }));
        tokio::spawn(async move { w.move_doc(&page, body).await })
    };
    // The move's write waits for the old pod's row with the same key.
    wait_for_lock_waits(&w.pool, 1).await;
    old_pod.commit().await.unwrap();

    let (status, body) = request.await.unwrap();
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "doc.sort_key_conflict"),
        "{body}"
    );
    assert_eq!(parent_of(&w, &page).await, None, "the page didn't move");
}
