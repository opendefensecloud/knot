# Import a Markdown File as a Page — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a user get an existing Markdown document into knot — by picking a `.md` file on the doc page, or by pasting Markdown source into the editor.

**Architecture:** The backend capability already exists (`POST /api/docs/{id}/markdown`); the web app has never called it. Add a `?mode=replace|append` switch to that endpoint (defaulting to today's append so `from_template` is untouched), then build the two UI entry points on top: a header button that reads a file and POSTs it, and a `handlePaste` branch that converts Markdown text to editor HTML client-side.

**Tech Stack:** Rust (axum 0.8, yrs, tokio), TypeScript/React 19, Tiptap 2, `marked` 18 (new dep), DOMPurify (already present), vitest + @testing-library/react, Playwright.

**Spec:** `docs/superpowers/specs/2026-09-04-import-markdown-page-design.md`

## Global Constraints

- **Wire default stays `append`.** Absent or `mode=append` must keep byte-identical behaviour to today. Only the new UI sends `mode=replace`. `from_template` (`routes/api/docs.rs:534`) is not modified.
- **Rust tests need dev-compose Postgres.** `make compose.up` before running. Tests use `knot_test_support::fresh_db()`, never testcontainers. If the suite flakes on resource pressure, run `make db.cleanup`.
- **Test commands:** Rust `cargo nextest run -p knot-server --test <name>`; web `cd web && pnpm test`; types `cd web && pnpm tsc --noEmit`; lint `cd web && pnpm lint` (must pass with `--max-warnings 0`); e2e `cd e2e && pnpm playwright test flows/<name>.spec.ts`.
- **CI uses `pnpm install --frozen-lockfile`** — `web/pnpm-lock.yaml` must be committed alongside `web/package.json`.
- **No title changes on import.** The page title is never derived from a leading `#`.
- **Error codes are user-visible contract.** Use exactly `markdown.bad_mode`, `markdown.not_utf8`, `markdown.parse`, `markdown.apply`, `acl.editor_required`, `auth.session_required`, `acl.no_grant`.

## File Structure

**Create:**

- `crates/knot-server/tests/markdown_import_integration.rs` — HTTP-level coverage of the import endpoint (both modes, role gating, bad input, attribution).
- `web/src/features/editor/markdownPaste.ts` — the two pure functions behind Markdown paste: `looksLikeMarkdown` and `markdownToHtml`.
- `web/src/features/editor/markdownPaste.test.ts` — heuristic + conversion tests.
- `web/src/features/docs/ImportMarkdownButton.tsx` — self-contained import control (file input, size guard, emptiness check, confirm, POST).
- `web/src/features/docs/ImportMarkdownButton.test.tsx` — confirm-gate and size-guard tests.
- `e2e/flows/import-markdown.spec.ts` — file import and Markdown paste, end to end.

**Modify:**

- `crates/knot-crdt/src/room.rs` — add `by_user` to `Event::ReplaceWithMarkdown`; thread into `PersistJob`.
- `crates/knot-server/src/routes/api/history.rs:173` — pass `by_user: None`.
- `crates/knot-server/src/routes/api/export_import.rs:680` — pass `by_user: None`.
- `crates/knot-server/tests/templates_integration.rs:200` — pass `by_user: None`.
- `crates/knot-server/src/routes/api/markdown.rs` — `?mode=` on `import_inline`.
- `web/package.json` + `web/pnpm-lock.yaml` — add `marked`.
- `web/src/lib/sanitize.ts` — add `sanitizeEditorFragment`.
- `web/src/features/editor/KnotEditor.tsx` — Markdown branch in `handlePaste`, Shift latch in `handleDOMEvents.keydown`.
- `web/src/features/docs/docs.api.ts` — `importMarkdown`.
- `web/src/features/docs/DocPage.tsx` — render `ImportMarkdownButton`.
- `CHANGELOG.md` — Unreleased entry.

---

## Task 1: Attribute `ReplaceWithMarkdown` to a user

`Event::ReplaceWithMarkdown` persists with a hardcoded `by_user_id: None` (`crates/knot-crdt/src/room.rs:355`), so a replace lands in `doc_updates` unattributed. Task 2 uses this event for imports, and an import that wipes and rewrites a page should name its author. Add the field now, with existing callers passing `None` so nothing else changes behaviour.

**Files:**

- Modify: `crates/knot-crdt/src/room.rs` (enum variant ~`:67`, match arm ~`:298`, `PersistJob` ~`:355`, test call sites ~`:827`, `:838`, `:910`, `:943`)
- Modify: `crates/knot-server/src/routes/api/history.rs:173`
- Modify: `crates/knot-server/src/routes/api/export_import.rs:680`
- Modify: `crates/knot-server/tests/templates_integration.rs:200`
- Test: `crates/knot-crdt/src/room.rs` (`mod tests`)

**Interfaces:**

- Consumes: nothing from earlier tasks.
- Produces: `knot_crdt::Event::ReplaceWithMarkdown { update_bytes: Vec<u8>, by_user: Option<Uuid>, reply: oneshot::Sender<Result<i64, String>> }` — Task 2 constructs this with `by_user: Some(ctx.user_id)`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/knot-crdt/src/room.rs`, right after the existing `NoopUpdates` struct:

```rust
    /// Records the `by_user` argument each persist passes through, so
    /// tests can assert attribution reaches the store.
    struct RecordingUpdates {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Option<Uuid>>>>,
    }
    #[async_trait::async_trait]
    impl knot_storage::UpdatesStore for RecordingUpdates {
        async fn insert_batch(
            &self,
            _: Uuid,
            by_user: Option<Uuid>,
            updates: &[Vec<u8>],
        ) -> Result<Vec<i64>, knot_storage::UpdatesStoreError> {
            self.seen.lock().unwrap().push(by_user);
            Ok((1..=updates.len() as i64).collect())
        }
        async fn since(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<Vec<knot_storage::DocUpdate>, knot_storage::UpdatesStoreError> {
            Ok(vec![])
        }
        async fn max_seq(&self, _: Uuid) -> Result<i64, knot_storage::UpdatesStoreError> {
            Ok(0)
        }
        async fn delete_up_to(
            &self,
            _: Uuid,
            _: i64,
        ) -> Result<u64, knot_storage::UpdatesStoreError> {
            Ok(0)
        }
    }
```

Then add this test at the end of `mod tests`:

```rust
    /// A replace carries its author through to the persisted update.
    /// Before this, `ReplaceWithMarkdown` hardcoded `by_user_id: None`, so
    /// a full-document rewrite was the one mutation nobody could be blamed
    /// for.
    #[tokio::test(flavor = "multi_thread")]
    async fn replace_with_markdown_records_by_user() {
        let bus = Arc::new(MemBus::new());
        let doc_id = Uuid::new_v4();
        let sub = bus.subscribe(doc_id).await.unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let updates: Arc<dyn knot_storage::UpdatesStore> =
            Arc::new(RecordingUpdates { seen: seen.clone() });
        let snapshots: Arc<dyn knot_storage::SnapshotStore> = Arc::new(NoopSnapshots);
        let engine: Arc<dyn Engine> = Arc::new(YrsEngine);
        let policy = crate::snapshot::SnapshotPolicy {
            every_n: 1000,
            idle: std::time::Duration::from_secs(60),
        };
        let h = Room::spawn(
            doc_id,
            engine.clone(),
            bus,
            sub,
            updates,
            snapshots,
            policy,
            None,
        )
        .await
        .unwrap();

        let alice = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();
        h.tx.send(Event::ReplaceWithMarkdown {
            update_bytes: make_replace_bytes_raw("Imported"),
            by_user: Some(alice),
            reply: tx,
        })
        .await
        .unwrap();
        rx.await.unwrap().expect("replace");

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![Some(alice)],
            "replace should persist with its author, got {recorded:?}"
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p knot-crdt replace_with_markdown_records_by_user`
Expected: FAIL — compile error, `Event::ReplaceWithMarkdown` has no field `by_user`.

- [ ] **Step 3: Add the field to the event**

In `crates/knot-crdt/src/room.rs`, change the variant (around `:67`):

```rust
    ReplaceWithMarkdown {
        /// Full-state update bytes encoding the replacement content.
        update_bytes: Vec<u8>,
        /// Attribution for the persisted update, mirroring `ApplyUpdate`.
        /// `None` where there is no meaningful actor (workspace import),
        /// or where the caller has not been threaded through yet.
        by_user: Option<Uuid>,
        reply: oneshot::Sender<Result<i64, String>>,
    },
```

- [ ] **Step 4: Thread it into the persist job**

In the match arm (around `:298`), change the pattern and the `PersistJob`:

```rust
                    Some(Event::ReplaceWithMarkdown { update_bytes, by_user, reply }) => {
```

and around `:355`:

```rust
                        let _ = self.persist_tx.send(crate::writer::PersistJob {
                            bytes: effective_bytes.clone(),
                            by_user_id: by_user,
                            persisted: Some(persisted_tx),
                        }).await;
```

- [ ] **Step 5: Update every existing call site**

Add `by_user: None,` to each. In `crates/knot-crdt/src/room.rs` tests: the four sends at roughly `:827`, `:838`, `:910`, `:943`. In `crates/knot-server/src/routes/api/history.rs:173`:

```rust
        .send(knot_crdt::Event::ReplaceWithMarkdown {
            update_bytes,
            by_user: None,
            reply: tx,
        })
```

Same shape in `crates/knot-server/src/routes/api/export_import.rs:680` and `crates/knot-server/tests/templates_integration.rs:200`.

Find any you missed with: `grep -rn "ReplaceWithMarkdown" --include="*.rs" crates`

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo nextest run -p knot-crdt && cargo nextest run -p knot-server --test templates_integration --test history_integration`
Expected: PASS, including `replace_with_markdown_records_by_user` and the pre-existing `replace_with_markdown_swaps_content`.

- [ ] **Step 7: Commit**

```bash
git add crates/knot-crdt/src/room.rs crates/knot-server/src/routes/api/history.rs \
        crates/knot-server/src/routes/api/export_import.rs \
        crates/knot-server/tests/templates_integration.rs
git commit -m "feat(crdt): attribute ReplaceWithMarkdown to a user

The variant hardcoded by_user_id: None, so a full-document replace was
the one mutation that landed in doc_updates unattributed. Existing
callers pass None, so their behaviour is unchanged; the markdown import
endpoint will pass the importing user."
```

---

## Task 2: `?mode=replace` on `POST /api/docs/{id}/markdown`

`import_inline` sends `Event::ApplyUpdate`, which Yjs *merges* into the live fragment — importing into a non-empty page appends to what is there. The only caller today (`from_template`) always targets an empty doc, so this has never been exercised. Add a mode switch; keep append as the wire default.

**Files:**

- Modify: `crates/knot-server/src/routes/api/markdown.rs` (`import_inline`, `:161`)
- Test: `crates/knot-server/tests/markdown_import_integration.rs` (create)

**Interfaces:**

- Consumes: `Event::ReplaceWithMarkdown { update_bytes, by_user, reply }` from Task 1.
- Produces: `POST /api/docs/{id}/markdown?mode=replace|append` → `204`; unknown mode → `400 markdown.bad_mode`. Task 6's `docsApi.importMarkdown` targets this.

- [ ] **Step 1: Write the failing tests**

Create `crates/knot-server/tests/markdown_import_integration.rs`. The harness mirrors `crates/knot-server/tests/history_integration.rs` — read that file first; `wire_rooms` and `login` are copied from it verbatim.

```rust
//! Integration tests for `POST /api/docs/{id}/markdown` — the single-page
//! Markdown import behind the doc page's "Import Markdown…" control.
//!
//! The endpoint predates any caller in the web app. These tests pin the two
//! modes against each other: `append` (the wire default, which Yjs merges
//! into whatever is already there) and `replace` (clear + apply in one
//! transaction). The append-duplicates test is not aspirational — it locks
//! in the default so `from_template` keeps working.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::{Hasher, Throttle};
use knot_crdt::{Rooms, SnapshotPolicy, YrsEngine};
use knot_server::{AppState, router_with_state};
use knot_storage::{PgSnapshotStore, PgUpdatesStore, SnapshotStore, UpdatesStore, WorkspaceRole};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Fixture {
    app: axum::Router,
    pool: PgPool,
    doc_id: Uuid,
    owner_id: Uuid,
}

/// Workspace + owner (alice) + viewer (bob) + one doc, with a real Rooms
/// registry wired up so the import endpoint can acquire a room.
async fn fixture() -> Fixture {
    let db = knot_test_support::fresh_db().await;
    let pool = db.pool.clone();

    let mut state = AppState::with_pool(pool.clone());
    state.hasher = Arc::new(Hasher::fast_for_tests());
    state.throttle = Arc::new(Throttle::new());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();

    let hash = state.hasher.hash("hunter22").unwrap();
    let ws = state
        .workspaces
        .as_ref()
        .unwrap()
        .create("default", "Workspace")
        .await
        .unwrap();
    let owner = state
        .users
        .as_ref()
        .unwrap()
        .create_local("alice@example.com", "Alice", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, owner.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let viewer = state
        .users
        .as_ref()
        .unwrap()
        .create_local("bob@example.com", "Bob", &hash)
        .await
        .unwrap();
    state
        .workspaces
        .as_ref()
        .unwrap()
        .add_member(ws.id, viewer.id, WorkspaceRole::Viewer)
        .await
        .unwrap();
    let doc = state
        .docs
        .as_ref()
        .unwrap()
        .create(ws.id, None, "Import Doc", "m", owner.id)
        .await
        .unwrap();

    wire_rooms(&mut state, &db.url).await;
    let doc_id = doc.id;
    let owner_id = owner.id;
    Fixture {
        app: router_with_state(state),
        pool,
        doc_id,
        owner_id,
    }
}

/// Spin up a real Rooms registry wired to the test DB (copied from
/// `history_integration.rs` — the import endpoint needs `rooms_v2`).
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

async fn login(app: &axum::Router, email: &str) -> (String, String) {
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
    assert_eq!(r.status(), StatusCode::NO_CONTENT, "login failed");
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

/// POST markdown. `mode` is appended as a query param when `Some`.
async fn import(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
    mode: Option<&str>,
    body: impl Into<Body>,
) -> StatusCode {
    let uri = match mode {
        Some(m) => format!("/api/docs/{doc_id}/markdown?mode={m}"),
        None => format!("/api/docs/{doc_id}/markdown"),
    };
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "text/markdown")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(body.into())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// Same as `import` but also returns the parsed error code, for negative cases.
async fn import_err(
    app: &axum::Router,
    sid: &str,
    csrf: &str,
    doc_id: Uuid,
    mode: Option<&str>,
    body: impl Into<Body>,
) -> (StatusCode, String) {
    let uri = match mode {
        Some(m) => format!("/api/docs/{doc_id}/markdown?mode={m}"),
        None => format!("/api/docs/{doc_id}/markdown"),
    };
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "text/markdown")
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .header("x-csrf-token", csrf)
                .body(body.into())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    let code = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_string))
        .unwrap_or_default();
    (status, code)
}

async fn export(app: &axum::Router, sid: &str, csrf: &str, doc_id: Uuid) -> String {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/docs/{doc_id}/markdown"))
                .header("cookie", format!("{sid}; csrf={csrf}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "markdown export failed");
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The wire default merges. `from_template` relies on this (it always
/// targets a brand-new empty doc), so it must not change — but it is also
/// exactly why the UI cannot use the default.
#[tokio::test(flavor = "multi_thread")]
async fn append_into_non_empty_doc_keeps_both() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, None, "# First\n").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("append"), "# Second\n").await,
        StatusCode::NO_CONTENT
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    let md = export(&f.app, &sid, &csrf, f.doc_id).await;
    assert!(md.contains("First"), "append lost the original: {md:?}");
    assert!(md.contains("Second"), "append lost the import: {md:?}");
}

/// What the UI uses: the imported file becomes the page, not an addition
/// to it.
#[tokio::test(flavor = "multi_thread")]
async fn replace_into_non_empty_doc_swaps_content() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, None, "# Original\n").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# Imported\n").await,
        StatusCode::NO_CONTENT
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    let md = export(&f.app, &sid, &csrf, f.doc_id).await;
    assert!(md.contains("Imported"), "replace lost the import: {md:?}");
    assert!(!md.contains("Original"), "replace kept the old body: {md:?}");
}

/// The `len > 0` guard in the replace arm must not trip on a fresh doc —
/// this is the common case for the UI (create a page, import into it).
#[tokio::test(flavor = "multi_thread")]
async fn replace_into_empty_doc_works() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# Fresh\n").await,
        StatusCode::NO_CONTENT
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(export(&f.app, &sid, &csrf, f.doc_id).await.contains("Fresh"));
}

/// The importing user is recorded against the update, so a full-page
/// rewrite is attributable in `doc_updates`.
#[tokio::test(flavor = "multi_thread")]
async fn replace_records_the_importing_user() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    assert_eq!(
        import(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# Attributed\n").await,
        StatusCode::NO_CONTENT
    );
    tokio::time::sleep(Duration::from_millis(100)).await;

    let authors: Vec<Option<Uuid>> =
        sqlx::query_scalar("SELECT by_user_id FROM doc_updates WHERE doc_id = $1 ORDER BY seq")
            .bind(f.doc_id)
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert!(
        authors.contains(&Some(f.owner_id)),
        "expected an update attributed to the importer, got {authors:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_mode_is_rejected() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    let (status, code) =
        import_err(&f.app, &sid, &csrf, f.doc_id, Some("merge"), "# Nope\n").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(code, "markdown.bad_mode");
}

#[tokio::test(flavor = "multi_thread")]
async fn viewer_cannot_import() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "bob@example.com").await;

    let (status, code) =
        import_err(&f.app, &sid, &csrf, f.doc_id, Some("replace"), "# Nope\n").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code, "acl.editor_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_import_is_rejected() {
    let f = fixture().await;

    // No session cookie at all. CSRF middleware sees matching header+cookie
    // values, so the request reaches the handler's auth check.
    let (status, code) = import_err(&f.app, "sid=nope", "nope", f.doc_id, None, "# Nope\n").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code, "auth.session_required");
}

#[tokio::test(flavor = "multi_thread")]
async fn non_utf8_body_is_rejected() {
    let f = fixture().await;
    let (sid, csrf) = login(&f.app, "alice@example.com").await;

    let (status, code) = import_err(
        &f.app,
        &sid,
        &csrf,
        f.doc_id,
        Some("replace"),
        vec![0xffu8, 0xfe, 0xfd],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code, "markdown.not_utf8");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `make compose.up && cargo nextest run -p knot-server --test markdown_import_integration`
Expected: `unknown_mode_is_rejected` FAILS (`?mode=merge` is ignored today, so the import succeeds with 204 and no error code) and `replace_into_non_empty_doc_swaps_content` FAILS (the export still contains "Original"). `replace_records_the_importing_user` fails on attribution. The rest pass.

- [ ] **Step 3: Add the mode parameter**

In `crates/knot-server/src/routes/api/markdown.rs`, extend the `axum::extract` import to include `Query`:

```rust
use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
```

Add the query type just above `import_inline`:

```rust
/// Query for [`import_inline`].
///
/// Deliberately typed as `Option<String>` rather than an enum: a serde
/// enum would make an unknown value fail deserialization, and axum would
/// answer with its own plain-text 400 instead of our JSON error envelope.
#[derive(serde::Deserialize)]
pub(super) struct ImportQuery {
    #[serde(default)]
    mode: Option<String>,
}
```

Change the signature (`Query` is a `FromRequestParts` extractor, so it must come before the body-consuming `Request`):

```rust
pub(super) async fn import_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ImportQuery>,
    req: Request,
) -> Response {
```

- [ ] **Step 4: Validate the mode after the auth checks**

Insert immediately after the `role.0 == Viewer` rejection and before `let Some(rooms) = …`:

```rust
    // `append` is the default because it is what shipped: `from_template`
    // and any existing API client rely on the merge. Only the doc page's
    // import control asks for `replace`.
    let replace = match q.mode.as_deref() {
        None | Some("append") => false,
        Some("replace") => true,
        Some(_) => return json_err(StatusCode::BAD_REQUEST, "markdown.bad_mode", ""),
    };
```

- [ ] **Step 5: Dispatch on the mode**

Replace everything from `let (tx, rx) = tokio::sync::oneshot::channel();` to the end of the function body (the current `room.tx.send(Event::ApplyUpdate…)` block and its `match rx.await`) with:

```rust
    // The two events reply over differently-typed channels (`EngineError`
    // vs `String`), so they cannot share one oneshot — normalise to
    // `Result<i64, String>` here.
    let applied: Result<i64, String> = if replace {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if room
            .tx
            .send(knot_crdt::Event::ReplaceWithMarkdown {
                update_bytes,
                by_user: Some(ctx.user_id),
                reply: tx,
            })
            .await
            .is_err()
        {
            return internal();
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => return internal(),
        }
    } else {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if room
            .tx
            .send(knot_crdt::Event::ApplyUpdate {
                update_bytes,
                by_user: Some(ctx.user_id),
                reply: tx,
            })
            .await
            .is_err()
        {
            return internal();
        }
        match rx.await {
            Ok(Ok(seq)) => Ok(seq),
            Ok(Err(e)) => Err(format!("{e:?}")),
            Err(_) => return internal(),
        }
    };

    match applied {
        Ok(_seq) => {
            let _ = refresh_markdown_and_index(&state, doc_id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, %doc_id, replace, "md import apply");
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.apply", "")
        }
    }
}
```

Also update the module doc comment at the top of the file:

```rust
//! GET  /api/docs/{id}/markdown    → text/markdown export
//! POST /api/docs/{id}/markdown    → cold-import markdown as a y-update
//!
//! Import takes `?mode=append` (default) or `?mode=replace`. Append hands
//! the parsed bytes to `Event::ApplyUpdate`, which Yjs *merges* into the
//! live fragment — right for `from_template`, which always targets a fresh
//! doc, wrong for importing over a page that already has content. Replace
//! goes through `Event::ReplaceWithMarkdown`, which clears the fragment and
//! applies in one transaction.
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo nextest run -p knot-server --test markdown_import_integration`
Expected: PASS, all nine tests.

- [ ] **Step 7: Verify nothing else regressed**

Run: `cargo nextest run --workspace --all-features && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/knot-server/src/routes/api/markdown.rs \
        crates/knot-server/tests/markdown_import_integration.rs
git commit -m "feat(api): add ?mode=replace to POST /api/docs/{id}/markdown

The endpoint only ever merged, because its one caller (from-template)
always targets an empty doc. Importing over a page with content
duplicated it. Replace routes through ReplaceWithMarkdown; append stays
the wire default so existing callers are untouched."
```

---

## Task 3: `marked` dependency and the editor HTML sanitizer

Markdown passes raw HTML straight through, so anything converted client-side has to be sanitized before it reaches the editor. Landing the dependency and the sanitizer together keeps Task 4 focused on the heuristic.

**Files:**

- Modify: `web/package.json`, `web/pnpm-lock.yaml`
- Modify: `web/src/lib/sanitize.ts`
- Test: `web/src/lib/sanitize.test.ts` (exists — add cases)

**Interfaces:**

- Produces: `sanitizeEditorFragment(html: string): DocumentFragment` — Task 4's `markdownToHtml` calls it.

- [ ] **Step 1: Add the dependency**

```bash
cd web && pnpm add marked
```

Verify it resolved to 18.x and that the lockfile changed:

```bash
node -e "console.log(require('./node_modules/marked/package.json').version)"
git diff --stat package.json pnpm-lock.yaml
```

- [ ] **Step 2: Write the failing tests**

In `web/src/lib/sanitize.test.ts`, widen the existing import to
`import { sanitizeEditorFragment, sanitizeSvg } from "./sanitize";` and append:

```ts
describe("sanitizeEditorFragment", () => {
  function html(fragment: DocumentFragment): string {
    const host = document.createElement("div");
    host.appendChild(fragment);
    return host.innerHTML;
  }

  it("keeps the structure knot's schema can represent", () => {
    const out = html(
      sanitizeEditorFragment(
        '<h2>Title</h2><ul><li>one</li></ul><p><a href="https://x.test">link</a></p>',
      ),
    );
    expect(out).toContain("<h2>Title</h2>");
    expect(out).toContain("<li>one</li>");
    expect(out).toContain('href="https://x.test"');
  });

  it("keeps checkbox inputs so task items can be promoted", () => {
    const out = html(
      sanitizeEditorFragment('<ul><li><input type="checkbox" checked>done</li></ul>'),
    );
    expect(out).toContain("<input");
    expect(out).toContain('type="checkbox"');
  });

  it("keeps data-checked", () => {
    const out = html(sanitizeEditorFragment('<ul><li data-checked="true">done</li></ul>'));
    expect(out).toContain('data-checked="true"');
  });

  it("strips scripts and event handlers", () => {
    const out = html(
      sanitizeEditorFragment('<p>hi</p><script>alert(1)</script><img src=x onerror="alert(1)">'),
    );
    expect(out).not.toContain("script");
    expect(out).not.toContain("onerror");
  });

  it("strips javascript: URLs", () => {
    const out = html(sanitizeEditorFragment('<a href="javascript:alert(1)">x</a>'));
    expect(out).not.toContain("javascript:");
  });
});
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd web && pnpm test src/lib/sanitize.test.ts`
Expected: FAIL — `sanitizeEditorFragment` is not exported.

- [ ] **Step 4: Implement the sanitizer**

Append to `web/src/lib/sanitize.ts`:

```ts
/** Tags the knot editor schema can actually represent, plus `input` —
 *  which is not a schema node, but must survive sanitization long enough
 *  for `markdownToHtml` to turn `marked`'s task-list checkboxes into
 *  `data-checked` on the `<li>`. */
const EDITOR_ALLOWED_TAGS = [
  "p", "br", "hr",
  "h1", "h2", "h3", "h4", "h5", "h6",
  "blockquote", "pre", "code",
  "ul", "ol", "li",
  "strong", "em", "u", "s", "del",
  "a", "img",
  "table", "thead", "tbody", "tr", "th", "td",
  "input",
];

const EDITOR_ALLOWED_ATTR = [
  "href", "title", "alt", "src", "class", "start",
  "colspan", "rowspan", "align",
  "type", "checked", "data-checked",
];

/**
 * Sanitize HTML derived from pasted Markdown before it reaches the editor.
 *
 * Markdown passes raw HTML through untouched, so `<script>` or
 * `<img src=x onerror=…>` in a pasted document would otherwise be handed
 * straight to Tiptap's DOM parser.
 *
 * Returns a `DocumentFragment` rather than a string so callers can
 * post-process the result without an `innerHTML` round-trip — reparsing
 * unsanitized markup into a detached element is enough to start an
 * `<img>` load and fire its `onerror`.
 */
export function sanitizeEditorFragment(html: string): DocumentFragment {
  return DOMPurify.sanitize(html, {
    ALLOWED_TAGS: EDITOR_ALLOWED_TAGS,
    ALLOWED_ATTR: EDITOR_ALLOWED_ATTR,
    RETURN_DOM_FRAGMENT: true,
  });
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd web && pnpm test src/lib/sanitize.test.ts && pnpm tsc --noEmit && pnpm lint`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add web/package.json web/pnpm-lock.yaml web/src/lib/sanitize.ts web/src/lib/sanitize.test.ts
git commit -m "feat(web): add marked and an editor HTML sanitizer

Markdown passes raw HTML through, so anything converted client-side must
be sanitized before Tiptap parses it. Returns a fragment so the task-list
rewrite can post-process without reparsing unsanitized markup."
```

---

## Task 4: The Markdown paste module

The heuristic is the risky part of this feature, so it lives in a pure module with its own tests rather than inline in the editor.

**Files:**

- Create: `web/src/features/editor/markdownPaste.ts`
- Test: `web/src/features/editor/markdownPaste.test.ts`

**Interfaces:**

- Consumes: `sanitizeEditorFragment(html: string): DocumentFragment` from Task 3.
- Produces: `looksLikeMarkdown(text: string): boolean` and `markdownToHtml(markdown: string): string` — Task 5's `handlePaste` calls both.

- [ ] **Step 1: Write the failing tests**

Create `web/src/features/editor/markdownPaste.test.ts`:

```ts
import { describe, expect, it } from "vitest";

import { looksLikeMarkdown, markdownToHtml } from "./markdownPaste";

describe("looksLikeMarkdown — converts", () => {
  it("a subheading, the issue's own example", () => {
    expect(looksLikeMarkdown("## Heading\n\nSome text.")).toBe(true);
  });

  it("a fenced code block on its own", () => {
    expect(looksLikeMarkdown("```js\nconst a = 1;\n```")).toBe(true);
  });

  it("a GFM table on its own", () => {
    expect(looksLikeMarkdown("| a | b |\n| --- | --- |\n| 1 | 2 |")).toBe(true);
  });

  it("a bullet list", () => {
    expect(looksLikeMarkdown("- one\n- two\n- three")).toBe(true);
  });

  it("an ordered list", () => {
    expect(looksLikeMarkdown("1. one\n2. two")).toBe(true);
  });

  it("prose carrying two different inline cues", () => {
    expect(looksLikeMarkdown("See [the docs](https://x.test) for **details**.")).toBe(true);
  });

  it("a heading plus a list", () => {
    expect(looksLikeMarkdown("# Release notes\n\n- fixed a thing")).toBe(true);
  });

  it("a task list", () => {
    expect(looksLikeMarkdown("- [ ] todo\n- [x] done")).toBe(true);
  });
});

describe("looksLikeMarkdown — leaves alone", () => {
  it("one line of prose", () => {
    expect(looksLikeMarkdown("Just a sentence I copied from somewhere.")).toBe(false);
  });

  it("prose mentioning C# and F#", () => {
    expect(looksLikeMarkdown("C# is fine and so is F#.")).toBe(false);
  });

  it("a Python snippet whose comments start with #", () => {
    const py = [
      "# fetch the rows",
      "rows = db.query(SQL)",
      "# drop the empty ones",
      "rows = [r for r in rows if r.ok]",
    ].join("\n");
    expect(looksLikeMarkdown(py)).toBe(false);
  });

  it("a shell script", () => {
    expect(looksLikeMarkdown("#!/bin/bash\n# build it\nset -e\nmake all")).toBe(false);
  });

  it("a config file with blank-separated # comments", () => {
    const toml = "# server\n\nport = 8080\n\n# database\n\nurl = \"postgres://\"";
    expect(looksLikeMarkdown(toml)).toBe(false);
  });

  it("empty text", () => {
    expect(looksLikeMarkdown("")).toBe(false);
  });

  it("a lone hyphen", () => {
    expect(looksLikeMarkdown("-")).toBe(false);
  });
});

describe("markdownToHtml", () => {
  it("renders headings and lists", () => {
    const html = markdownToHtml("## Title\n\n- one\n- two\n");
    expect(html).toContain("<h2>Title</h2>");
    expect(html).toContain("<li>one</li>");
  });

  it("promotes task items to data-checked and drops the input", () => {
    const html = markdownToHtml("- [ ] todo\n- [x] done\n");
    expect(html).toContain('data-checked="false"');
    expect(html).toContain('data-checked="true"');
    expect(html).not.toContain("<input");
  });

  it("keeps the code fence language as a class", () => {
    expect(markdownToHtml("```js\nconst a = 1;\n```")).toContain("language-js");
  });

  it("renders a GFM table", () => {
    const html = markdownToHtml("| a | b |\n| --- | --- |\n| 1 | 2 |");
    expect(html).toContain("<table>");
    expect(html).toContain("<td>1</td>");
  });

  it("strips embedded scripts and handlers", () => {
    const html = markdownToHtml("# Hi\n\n<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>");
    expect(html).not.toContain("script");
    expect(html).not.toContain("onerror");
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd web && pnpm test src/features/editor/markdownPaste.test.ts`
Expected: FAIL — cannot resolve `./markdownPaste`.

- [ ] **Step 3: Implement the module**

Create `web/src/features/editor/markdownPaste.ts`:

```ts
/**
 * Markdown-aware paste.
 *
 * `handlePaste` only ever intercepted clipboard *files*; Markdown source
 * pasted as text landed verbatim, so `## Heading` stayed `## Heading`.
 * The two pure pieces of the fix live here so they can be tested without
 * an editor instance.
 *
 * The heuristic is deliberately asymmetric. A false negative pastes plain
 * text — exactly what happened before this existed, so nothing is lost. A
 * false positive silently mangles what the user pasted. So it errs toward
 * not firing: a lone cue only counts when it is unambiguous (a fence, a
 * GFM table, a `##` subheading), and everything else needs corroboration.
 */
import { marked } from "marked";

import { sanitizeEditorFragment } from "../../lib/sanitize";

const HEADING = /^ {0,3}(#{1,6})[ \t]+\S/;
const BULLET = /^ {0,3}[-*+][ \t]+\S/;
const ORDERED = /^ {0,3}\d{1,9}[.)][ \t]+\S/;
const QUOTE = /^ {0,3}>[ \t]?\S/;
const FENCE = /^ {0,3}(?:```|~~~)/;
const RULE = /^ {0,3}(?:-{3,}|\*{3,}|_{3,})[ \t]*$/;
const TABLE_ROW = /^ {0,3}\|.*\|[ \t]*$/;
const TABLE_DELIM = /^ {0,3}\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)+\|?[ \t]*$/;
const LINK = /\[[^\]\n]+\]\([^)\s]+(?:[ \t]+"[^"\n]*")?\)/;
const IMAGE = /!\[[^\]\n]*\]\([^)\s]+\)/;
const EMPHASIS = /(\*\*|__)(?=\S)[\s\S]+?\S\1/;

/**
 * Does this text look like Markdown source rather than plain text?
 *
 * Headings only count at the start of the paste or after a blank line.
 * That single rule is what keeps a pasted Python or shell snippet from
 * being read as a pile of `#` headings, because its comments sit directly
 * above the code they describe.
 *
 * Known limitation: a `#`-commented config file whose comments *are*
 * blank-line separated needs a `##` or a second kind of cue before it
 * converts — a plain `#`-only config stays plain text, which is why
 * repeated level-1 headings alone are not enough.
 */
export function looksLikeMarkdown(text: string): boolean {
  if (text.trim().length === 0) return false;

  const lines = text.split(/\r?\n/);
  let sawFence = false;
  let sawTable = false;
  let sawSubHeading = false;
  let headings = 0;
  let bullets = 0;
  let ordered = 0;
  let quotes = 0;
  let rules = 0;

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (FENCE.test(line)) sawFence = true;
    if (TABLE_ROW.test(line) && i + 1 < lines.length && TABLE_DELIM.test(lines[i + 1])) {
      sawTable = true;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      // Start-of-paste or after a blank line. Real Markdown separates its
      // headings; `#` comments in source code sit against their code.
      const atBlockStart = i === 0 || lines[i - 1].trim().length === 0;
      if (atBlockStart) {
        headings += 1;
        if (heading[1].length >= 2) sawSubHeading = true;
      }
      continue;
    }
    if (BULLET.test(line)) bullets += 1;
    else if (ORDERED.test(line)) ordered += 1;
    else if (QUOTE.test(line)) quotes += 1;
    else if (RULE.test(line)) rules += 1;
  }

  // Unambiguous on their own: no other text format uses these.
  if (sawFence || sawTable || sawSubHeading) return true;

  const kinds =
    (headings > 0 ? 1 : 0) +
    (bullets > 0 ? 1 : 0) +
    (ordered > 0 ? 1 : 0) +
    (quotes > 0 ? 1 : 0) +
    (rules > 0 ? 1 : 0) +
    (LINK.test(text) ? 1 : 0) +
    (IMAGE.test(text) ? 1 : 0) +
    (EMPHASIS.test(text) ? 1 : 0);
  if (kinds >= 2) return true;

  // A repeated block cue is a list or a quote block — structure, not prose.
  // Headings are excluded: repeated `#` is how most config formats comment.
  return bullets >= 2 || ordered >= 2 || quotes >= 2;
}

/**
 * Convert Markdown source to HTML the editor's schema can parse.
 *
 * Sanitize first, rewrite second: reparsing `marked`'s raw output into a
 * detached element would be enough for an `<img src=x onerror=…>` smuggled
 * through the Markdown to start loading.
 */
export function markdownToHtml(markdown: string): string {
  const raw = marked.parse(markdown, { async: false, gfm: true }) as string;
  const fragment = sanitizeEditorFragment(raw);
  promoteTaskItems(fragment);
  const host = document.createElement("div");
  host.appendChild(fragment);
  return host.innerHTML;
}

/**
 * `marked` renders GFM task items as
 * `<li><input type="checkbox" checked disabled>text</li>`, but knot's
 * `list_item` carries its state in `data-checked` (see
 * `TaskListExtension`) and has no `input` in its schema. Without this
 * rewrite `- [x] done` degrades to a plain bullet and disappears from
 * `/tasks`.
 */
function promoteTaskItems(fragment: DocumentFragment): void {
  fragment.querySelectorAll("li > input[type='checkbox']").forEach((input) => {
    const li = input.parentElement;
    if (!li) return;
    li.setAttribute("data-checked", input.hasAttribute("checked") ? "true" : "false");
    input.remove();
  });
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd web && pnpm test src/features/editor/markdownPaste.test.ts`
Expected: PASS, all 20 cases.

If `markdownToHtml`'s fence test fails on the class name, check what `marked` 18 emits (`pnpm exec node -e "import('marked').then(m => console.log(m.marked.parse('\`\`\`js\nx\n\`\`\`')))"`) and align the assertion to the real output rather than loosening the sanitizer.

- [ ] **Step 5: Verify types and lint**

Run: `cd web && pnpm tsc --noEmit && pnpm lint`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add web/src/features/editor/markdownPaste.ts web/src/features/editor/markdownPaste.test.ts
git commit -m "feat(editor): add the Markdown paste heuristic and converter

Pure module so the heuristic — the risky half of Markdown paste — is
testable without an editor. Errs toward not firing: a miss pastes plain
text as before, a false positive would mangle the paste."
```

---

## Task 5: Wire Markdown paste into the editor

**Files:**

- Modify: `web/src/features/editor/KnotEditor.tsx` (`editorProps`, `:229-244`)

**Interfaces:**

- Consumes: `looksLikeMarkdown`, `markdownToHtml` from Task 4.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Add the import and the Shift latch**

In `web/src/features/editor/KnotEditor.tsx`, add to the local imports (after the `EditorToolbar` import):

```tsx
import { looksLikeMarkdown, markdownToHtml } from "./markdownPaste";
```

Inside `EditorBody`, next to the existing `editorRef` declaration (`:104`):

```tsx
  // ⌘⇧V / Ctrl+Shift+V is the conventional "paste without formatting". The
  // paste event carries no modifier state of its own, so latch it from the
  // keydown that triggers the paste.
  const plainPasteRef = useRef(false);
```

- [ ] **Step 2: Replace `editorProps`**

Replace the whole `editorProps` block (`:229-244`) with:

```tsx
      editorProps: {
        handleDOMEvents: {
          keydown: (_view, event) => {
            if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "v") {
              plainPasteRef.current = event.shiftKey;
            }
            return false;
          },
        },
        handleDrop(_view, event, _slice, _moved) {
          const files = Array.from((event as DragEvent).dataTransfer?.files ?? []);
          if (files.length === 0) return false;
          event.preventDefault();
          void uploadAndInsert(files);
          return true;
        },
        handlePaste(_view, event) {
          const clipboard = (event as ClipboardEvent).clipboardData;
          const files = Array.from(clipboard?.files ?? []);
          if (files.length > 0) {
            event.preventDefault();
            void uploadAndInsert(files);
            return true;
          }
          // Consume the latch on every paste, so a plain-text paste never
          // leaks its opt-out into the next one.
          const plainOnly = plainPasteRef.current;
          plainPasteRef.current = false;
          if (plainOnly) return false;
          // A rich-text source already carries structure; ProseMirror's own
          // clipboard parser handles it better than a Markdown round-trip.
          if (clipboard?.types.includes("text/html")) return false;
          const ed = editorRef.current;
          // Inside a code block or a code mark, a paste must stay verbatim.
          if (!ed || ed.isActive("code_block") || ed.isActive("code")) return false;
          const text = clipboard?.getData("text/plain") ?? "";
          if (!looksLikeMarkdown(text)) return false;
          event.preventDefault();
          ed.chain().focus().insertContent(markdownToHtml(text)).run();
          notify("info", "Pasted as Markdown");
          return true;
        },
      },
```

- [ ] **Step 3: Verify types, lint, and the existing suite**

Run: `cd web && pnpm tsc --noEmit && pnpm lint && pnpm test`
Expected: PASS. If `pnpm lint` flags `notify` as a missing dependency of the `useEditor` deps array (`:267`), add `notify` to that array — it is a stable zustand selector, so it will not cause re-creation churn.

- [ ] **Step 4: Commit**

```bash
git add web/src/features/editor/KnotEditor.tsx
git commit -m "feat(editor): paste Markdown source as formatted content

Pasted Markdown landed as literal text. handlePaste now converts it when
it unambiguously looks like Markdown, with three opt-outs: a rich-text
clipboard, a code block, and ⌘⇧V."
```

---

## Task 6: The import control

**Files:**

- Modify: `web/src/features/docs/docs.api.ts`
- Create: `web/src/features/docs/ImportMarkdownButton.tsx`
- Test: `web/src/features/docs/ImportMarkdownButton.test.tsx`

**Interfaces:**

- Consumes: `POST /api/docs/{id}/markdown?mode=replace` from Task 2.
- Produces: `docsApi.importMarkdown(id: string, markdown: string, mode?: "replace" | "append")`, and `<ImportMarkdownButton docId={string} docTitle={string} />` — Task 7 renders it.

- [ ] **Step 1: Add the API method**

In `web/src/features/docs/docs.api.ts`, add to the `docsApi` object after `createFromTemplate`:

```ts
  /**
   * Import Markdown into an existing doc. `replace` swaps the body;
   * `append` is the server's default and merges into whatever is there.
   * Returns 204 with no body, so `ok` is `undefined`.
   */
  importMarkdown(id: string, markdown: string, mode: "replace" | "append" = "replace") {
    return apiFetch<void>(
      `/api/docs/${encodeURIComponent(id)}/markdown?mode=${mode}`,
      { method: "POST", body: markdown, contentType: "text/markdown; charset=utf-8" },
    );
  },
```

- [ ] **Step 2: Write the failing tests**

Create `web/src/features/docs/ImportMarkdownButton.test.tsx`:

```tsx
import { render, screen, cleanup, fireEvent, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ImportMarkdownButton } from "./ImportMarkdownButton";

const importMarkdown = vi.fn();
const exportMarkdown = vi.fn();

vi.mock("./docs.api", () => ({
  docsApi: { importMarkdown: (...a: unknown[]) => importMarkdown(...a) },
}));
vi.mock("../../lib/history.api", () => ({
  historyApi: { exportMarkdown: (...a: unknown[]) => exportMarkdown(...a) },
}));

function pick(name = "notes.md", body = "# Imported\n") {
  const file = new File([body], name, { type: "text/markdown" });
  fireEvent.change(screen.getByTestId("doc-import-md-input"), { target: { files: [file] } });
  return file;
}

beforeEach(() => {
  importMarkdown.mockReset().mockResolvedValue({ ok: undefined });
  exportMarkdown.mockReset().mockResolvedValue({ ok: "" });
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ImportMarkdownButton", () => {
  it("imports without prompting when the page is empty", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() => expect(importMarkdown).toHaveBeenCalledWith("d1", "# Imported\n", "replace"));
    expect(confirm).not.toHaveBeenCalled();
  });

  it("prompts before replacing a page that has content", async () => {
    exportMarkdown.mockResolvedValue({ ok: "# Existing\n" });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() => expect(importMarkdown).toHaveBeenCalled());
    expect(confirm).toHaveBeenCalledOnce();
    expect(confirm.mock.calls[0][0]).toContain("Notes");
    expect(confirm.mock.calls[0][0]).toContain("notes.md");
  });

  it("imports nothing when the prompt is declined", async () => {
    exportMarkdown.mockResolvedValue({ ok: "# Existing\n" });
    vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() => expect(exportMarkdown).toHaveBeenCalled());
    expect(importMarkdown).not.toHaveBeenCalled();
  });

  it("prompts when the page's current content cannot be read", async () => {
    exportMarkdown.mockResolvedValue({
      error: { code: "internal", message: "boom", details: {}, status: 500 },
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() => expect(confirm).toHaveBeenCalled());
    expect(importMarkdown).not.toHaveBeenCalled();
  });

  it("rejects an oversized file before any network call", async () => {
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    const big = new File(["x".repeat(1024 * 1024 + 1)], "big.md", { type: "text/markdown" });
    fireEvent.change(screen.getByTestId("doc-import-md-input"), { target: { files: [big] } });
    await waitFor(() => expect(screen.getByTestId("doc-import-md")).toBeInTheDocument());
    expect(exportMarkdown).not.toHaveBeenCalled();
    expect(importMarkdown).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd web && pnpm test src/features/docs/ImportMarkdownButton.test.tsx`
Expected: FAIL — cannot resolve `./ImportMarkdownButton`.

- [ ] **Step 4: Implement the component**

Create `web/src/features/docs/ImportMarkdownButton.tsx`:

```tsx
import { FileUp } from "lucide-react";
import { useRef, useState } from "react";

import { IconButton } from "../../components/ui/IconButton";
import { historyApi } from "../../lib/history.api";
import { useUi } from "../../stores/ui";

import { docsApi } from "./docs.api";

/** The server caps the import body at 1 MB (`markdown.rs`, `to_bytes`).
 *  Anything larger is refused by axum before our handler runs, producing a
 *  bare `400 bad_request` — check it here so the user gets a real reason. */
const MAX_IMPORT_BYTES = 1024 * 1024;

function messageFor(code: string): string {
  switch (code) {
    case "acl.editor_required":
    case "acl.no_grant":
      return "You don't have permission to import into this page.";
    case "markdown.not_utf8":
      return "That file isn't valid UTF-8 text.";
    case "markdown.parse":
      return "Couldn't read that file as Markdown.";
    default:
      return "Import failed.";
  }
}

/**
 * "Import Markdown…" — reads a local `.md` file and replaces this page's
 * body with it.
 *
 * Import replaces rather than appends, because the endpoint's default
 * merges into the existing content and would duplicate a page you have
 * already used. The old body is still reachable from History, which is what
 * the confirm says.
 */
export function ImportMarkdownButton({
  docId,
  docTitle,
}: {
  docId: string;
  docTitle: string;
}) {
  const inputRef = useRef<HTMLInputElement | null>(null);
  const notify = useUi((s) => s.notify);
  const [busy, setBusy] = useState(false);

  async function importFile(file: File) {
    if (file.size > MAX_IMPORT_BYTES) {
      notify("error", "That file is larger than the 1 MB import limit.");
      return;
    }
    const markdown = await file.text();

    // Only warn when there is something to lose. If the current body can't
    // be read, ask anyway rather than assume the page is empty.
    const current = await historyApi.exportMarkdown(docId);
    const isEmpty = "ok" in current && current.ok.trim().length === 0;
    if (!isEmpty) {
      const proceed = window.confirm(
        `Replace the contents of "${docTitle}" with ${file.name}? ` +
          "The current version stays in History.",
      );
      if (!proceed) return;
    }

    setBusy(true);
    const r = await docsApi.importMarkdown(docId, markdown, "replace");
    setBusy(false);
    if ("error" in r) {
      notify("error", messageFor(r.error.code));
      return;
    }
    // No refetch needed: the room actor fans the replace out over the same
    // WebSocket the editor is already on, exactly as a history restore does.
    notify("info", `Imported "${file.name}"`);
  }

  return (
    <>
      <IconButton
        data-testid="doc-import-md"
        label="Import Markdown…"
        disabled={busy}
        onClick={() => inputRef.current?.click()}
      >
        <FileUp size={16} aria-hidden />
      </IconButton>
      <input
        ref={inputRef}
        type="file"
        accept=".md,.markdown,text/markdown,text/plain"
        className="hidden"
        data-testid="doc-import-md-input"
        onChange={(e) => {
          const file = e.target.files?.[0];
          // Reset first so picking the same file again still fires onChange.
          e.target.value = "";
          if (file) void importFile(file);
        }}
      />
    </>
  );
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd web && pnpm test src/features/docs/ImportMarkdownButton.test.tsx`
Expected: PASS, all five cases.

- [ ] **Step 6: Verify types and lint**

Run: `cd web && pnpm tsc --noEmit && pnpm lint`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add web/src/features/docs/docs.api.ts \
        web/src/features/docs/ImportMarkdownButton.tsx \
        web/src/features/docs/ImportMarkdownButton.test.tsx
git commit -m "feat(docs): add an Import Markdown control

Reads a local .md file and replaces the page body via
POST /api/docs/{id}/markdown?mode=replace, confirming first when the page
already has content."
```

---

## Task 7: Render the control and prove the whole flow

**Files:**

- Modify: `web/src/features/docs/DocPage.tsx` (`:182-197`, just before the export button)
- Create: `e2e/flows/import-markdown.spec.ts`

**Interfaces:**

- Consumes: `<ImportMarkdownButton />` from Task 6; `looksLikeMarkdown`/`markdownToHtml` wiring from Task 5.

- [ ] **Step 1: Render the button**

In `web/src/features/docs/DocPage.tsx`, add to the local imports:

```tsx
import { ImportMarkdownButton } from "./ImportMarkdownButton";
```

Insert immediately **before** the `doc-export` `IconButton` (`:182`):

```tsx
          {(effRole === "owner" || effRole === "editor") && (
            <ImportMarkdownButton docId={id} docTitle={meta.title} />
          )}
```

- [ ] **Step 2: Write the e2e spec**

Create `e2e/flows/import-markdown.spec.ts`:

```ts
import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations", "audit_events", "doc_markdown_cache", "doc_tasks",
    "doc_snapshots", "doc_updates", "document_grants", "documents",
    "sessions", "workspace_members", "users", "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

// Each test bootstraps through /setup, and POST /auth/setup returns 410 once a
// user exists — so reset per test, not once per file.
test.beforeEach(reset);

async function newDoc(page: import("@playwright/test").Page) {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("i@example.com");
  await page.getByTestId("setup-display-name").fill("I");
  await page.getByTestId("setup-password").fill("hunter22!hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

test("import a .md file into an empty page, then replace it", async ({ page }) => {
  await newDoc(page);
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");

  await page.getByTestId("doc-import-md-input").setInputFiles({
    name: "notes.md",
    mimeType: "text/markdown",
    buffer: Buffer.from("# Imported Heading\n\n- alpha\n- beta\n"),
  });

  await expect(editor.locator("h1")).toHaveText("Imported Heading", { timeout: 10_000 });
  await expect(editor.locator("li")).toHaveCount(2);

  // Second import into the now-non-empty page: accept the confirm and check
  // the old body is gone rather than appended to.
  page.once("dialog", (d) => void d.accept());
  await page.getByTestId("doc-import-md-input").setInputFiles({
    name: "replacement.md",
    mimeType: "text/markdown",
    buffer: Buffer.from("# Replacement Heading\n\nOnly this.\n"),
  });

  await expect(editor.locator("h1")).toHaveText("Replacement Heading", { timeout: 10_000 });
  await expect(editor).not.toContainText("Imported Heading");
  await expect(editor).not.toContainText("alpha");
});

test("paste Markdown source into the editor", async ({ page }) => {
  await newDoc(page);
  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();

  await page.evaluate(() => {
    const dt = new DataTransfer();
    dt.setData("text/plain", "## Pasted Heading\n\n- one\n- two\n");
    const pm = document.querySelector("[data-testid='editor-host'] .ProseMirror")!;
    pm.dispatchEvent(
      new ClipboardEvent("paste", { bubbles: true, cancelable: true, clipboardData: dt }),
    );
  });

  await expect(editor.locator("h2")).toHaveText("Pasted Heading", { timeout: 10_000 });
  await expect(editor.locator("li")).toHaveCount(2);
});
```

- [ ] **Step 3: Run the unit suites**

Run: `cd web && pnpm tsc --noEmit && pnpm lint && pnpm test`
Expected: PASS.

- [ ] **Step 4: Run the e2e spec**

Run: `make compose.up && cd e2e && pnpm playwright test flows/import-markdown.spec.ts`
Expected: PASS, 2 tests.

If the second import's `h1` assertion times out, check whether the confirm dialog fired before `setInputFiles` resolved — `page.once("dialog", …)` must be registered before the call, as written.

- [ ] **Step 5: Run the whole e2e suite for regressions**

Run: `cd e2e && pnpm playwright test`
Expected: PASS with no new failures (the suite was at `42 passed` before this change; expect `44 passed`).

- [ ] **Step 6: Commit**

```bash
git add web/src/features/docs/DocPage.tsx e2e/flows/import-markdown.spec.ts
git commit -m "feat(docs): surface Import Markdown on the doc page

Adds the header control next to Export, plus e2e coverage for importing
into an empty page, replacing a non-empty one, and pasting Markdown
source."
```

---

## Task 8: Changelog

**Files:**

- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add the entry**

Under `## [Unreleased]`, add an `### Added` section **above** the existing `### Changed` (Keep a Changelog orders Added before Changed):

```markdown
### Added
- **Import a Markdown file as a page.** The doc page has an "Import Markdown…"
  control next to Export: pick a `.md` file and it becomes the page body. The
  editor also understands Markdown on paste, so pasting a document's source
  now produces real headings, lists, tables and task items instead of literal
  `## Heading` text. Pasting is left alone when the clipboard carries rich
  text, when the cursor is in a code block, and on ⌘⇧V.
- `POST /api/docs/{id}/markdown` takes `?mode=replace`, which clears the page
  before applying instead of merging into it. The default is unchanged
  (`append`), so existing callers — including create-from-template — behave
  exactly as before. Importing over a page that already had content used to
  duplicate it; that is what the new mode fixes, and what the import control
  uses.
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog entry for Markdown page import"
```

---

## Final verification

- [ ] `make compose.up`
- [ ] `cargo nextest run --workspace --all-features` → PASS
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` → clean
- [ ] `cargo fmt --check` → clean
- [ ] `cd web && pnpm tsc --noEmit && pnpm lint && pnpm test` → PASS
- [ ] `cd e2e && pnpm playwright test` → PASS
- [ ] Manual smoke: create a page, import a `.md` with a heading, a table and a `- [x]` task; confirm the task appears on `/tasks`; import a second file and confirm the replace prompt; paste Markdown source and confirm it formats; paste the same source inside a code block and confirm it stays literal.
