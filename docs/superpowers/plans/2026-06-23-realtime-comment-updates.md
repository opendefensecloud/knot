# Realtime Comment Updates Implementation Plan (item C)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** When any comment changes on a document, every user viewing that document sees it within ~1s, via Postgres LISTEN/NOTIFY → collab WebSocket → React Query invalidation.

**Architecture:** Comment mutations `pg_notify('doc_comments', {doc_id})`. A background listener (mirroring the `acl_invalidate` listener) consumes it and, **only if the doc's room is already active**, pushes a `MSG_COMMENTS` frame to its connections. The frontend `KnotProvider` emits a `"comments"` event; `KnotEditor` invalidates `["comments", docId]` so the sidebar refetches.

**Tech Stack:** Rust (axum, sqlx `PgListener`, tokio) + `knot-crdt` room actors; React + TS + @tanstack/react-query. Backend tests: `cargo nextest run` (dev-compose Postgres; never testcontainers). Frontend: `cd web && pnpm test` / `pnpm tsc --noEmit`. E2E: `cd e2e && pnpm playwright test`.

**Spec:** `docs/superpowers/specs/2026-06-23-realtime-comment-updates-design.md`

**Preconditions:** dev-compose Postgres healthy.

---

## File Structure

- Modify: `crates/knot-server/src/protocol.rs` — `MSG_COMMENTS` const.
- Modify: `crates/knot-crdt/src/room.rs` — `Event::ServerFrame(Vec<u8>)` + handler.
- Modify: `crates/knot-crdt/src/registry.rs` — `Rooms::notify_doc_comments` (non-booting).
- Modify: `crates/knot-server/src/routes/api/comments.rs` — `notify_comment_change` + calls.
- Create: `crates/knot-server/src/comments_listener.rs` — LISTEN loop (lives in the **lib** crate alongside `protocol`/`room`).
- Modify: `crates/knot-server/src/lib.rs` — declare `pub mod comments_listener;` (next to `pub mod protocol;`).
- Modify: `crates/knot-server/src/main.rs` — spawn the listener (the bin references it as `knot_server::comments_listener::spawn`).
- Modify: `web/src/features/editor/KnotProvider.ts` — `MSG_COMMENTS` + `"comments"` event.
- Modify: `web/src/features/editor/KnotProvider.test.ts` — frame→event unit test.
- Modify: `web/src/features/editor/KnotEditor.tsx` — subscribe + invalidate.
- Create: `e2e/flows/comments-realtime.spec.ts` — two-client realtime proof.

---

## Task 1: Room broadcast plumbing (knot-crdt + protocol)

**Files:**
- Modify: `crates/knot-server/src/protocol.rs`
- Modify: `crates/knot-crdt/src/room.rs`
- Modify: `crates/knot-crdt/src/registry.rs`

Context: `Event` enum (`room.rs:38`) is the room actor's input; the `AwarenessIn` arm (`room.rs:223-234`) is the fan-out template (iterate `self.conns`, `conn.tx.try_send(bytes.clone())`, collect+remove dead). `ConnHandle { tx: mpsc::Sender<Vec<u8>> }` (`room.rs:33`). `Rooms::revoke_all_for_doc` (`registry.rs:104`) is the non-booting lookup template (`if let Some(h) = self.map.get(&doc_id) { h.tx.send(Event::…).await }`).

- [ ] **Step 1: Add the protocol constant**

In `crates/knot-server/src/protocol.rs`, after `MSG_AWARENESS`:

```rust
/// Server→client push: comments on this doc changed; client should refetch.
/// Payload: <varuint len><JSON bytes> where JSON is `{ "doc_id": "<uuid>" }`.
pub const MSG_COMMENTS: u8 = 5;
```

- [ ] **Step 2: Add the `ServerFrame` event + handler**

In `crates/knot-crdt/src/room.rs`, add a variant to `Event` (before `Shutdown`):

```rust
    /// Broadcast a pre-framed server message to every connection in the room.
    /// Used for out-of-band pushes (e.g. "comments changed") that don't touch
    /// the CRDT. Opaque bytes — the room does not interpret them.
    ServerFrame(Vec<u8>),
```

In the actor's `run` select loop (next to the `AwarenessIn` arm ~`room.rs:223`), add:

```rust
                    Some(Event::ServerFrame(frame)) => {
                        let mut to_close: Vec<ConnId> = Vec::new();
                        for (cid, conn) in &self.conns {
                            match conn.tx.try_send(frame.clone()) {
                                Ok(_) => {}
                                Err(_) => to_close.push(*cid),
                            }
                        }
                        for cid in to_close { self.conns.remove(&cid); }
                    }
```

- [ ] **Step 3: Add the non-booting `notify_doc_comments`**

In `crates/knot-crdt/src/registry.rs`, next to `revoke_all_for_doc`:

```rust
    /// Push a pre-framed server message to all clients in the doc's room, IF a
    /// room is currently active. Never boots a room — a comment on an unviewed
    /// document is simply dropped.
    pub async fn notify_doc_comments(&self, doc_id: Uuid, frame: Vec<u8>) {
        if let Some(h) = self.map.get(&doc_id) {
            let _ = h.tx.send(crate::room::Event::ServerFrame(frame)).await;
        }
    }
```

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p knot-crdt -p knot-server && cargo clippy -p knot-crdt -p knot-server --all-targets -- -D warnings`
Expected: compiles, no warnings. (No behavior yet — wired in Task 2.)

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server/src/protocol.rs crates/knot-crdt/src/room.rs crates/knot-crdt/src/registry.rs
git commit -m "feat(crdt): ServerFrame broadcast + non-booting notify_doc_comments"
```

---

## Task 2: Emit notify on comment mutations + listener + spawn

**Files:**
- Modify: `crates/knot-server/src/routes/api/comments.rs`
- Create: `crates/knot-server/src/comments_listener.rs`
- Modify: `crates/knot-server/src/lib.rs` (declare the module) and `crates/knot-server/src/main.rs` (spawn it)

**Crate structure (verified):** `protocol`, `room`, `routes` are `pub mod` in the **lib** crate `knot_server`; `main.rs` is the bin and references lib items as `knot_server::…`. So `comments_listener.rs` goes in the lib crate, declared `pub mod comments_listener;` in `lib.rs` — it can then use `crate::protocol::{MSG_COMMENTS, append_var_uint}` (both `pub`). `main.rs` spawns it via `knot_server::comments_listener::spawn(pool, rooms)`.

Context: `broadcast_mentions` (`comments.rs:140`) shows pool access: `let Some(ctx) = state.pool.as_ref() else { return };` then `let pool = ctx.clone(); tokio::spawn(async move { sqlx::query("SELECT pg_notify(...)").bind(...).execute(&pool).await; });`. The mutating handlers are `create_thread`, `create_reply`, `edit_comment`, `resolve_thread`, `unresolve_thread`, `add_reaction`, `remove_reaction`, `delete_comment`; each has the `doc_id` (path param) and `State(state)`. The acl-listener spawn in `main.rs:255-270` shows the `if let (Some(pool), …) = (state.pool.clone(), …)` pattern. `knot_docs::listener` (`crates/knot-docs/src/listener.rs`) shows the `PgListener::connect_with(pool)` + `listen(CHANNEL)` + `recv()` reconnect loop.

- [ ] **Step 1: Add the notify helper in comments.rs**

After `broadcast_mentions`, add:

```rust
/// Fire-and-forget: tell any active room for `doc_id` that its comments changed,
/// so connected clients refetch. Mirrors `broadcast_mentions`' pool access.
fn notify_comment_change(state: &AppState, doc_id: Uuid) {
    let Some(pool) = state.pool.as_ref().cloned() else {
        return;
    };
    let payload = serde_json::json!({ "doc_id": doc_id.to_string() }).to_string();
    tokio::spawn(async move {
        let _ = sqlx::query("SELECT pg_notify('doc_comments', $1)")
            .bind(&payload)
            .execute(&pool)
            .await;
    });
}
```

- [ ] **Step 2: Call it after every successful mutation**

In each of the eight handlers, after the store mutation succeeds (and before/after building the response — anywhere on the success path), add:

```rust
    notify_comment_change(&state, doc_id);
```

Handlers: `create_thread`, `create_reply`, `edit_comment`, `resolve_thread`, `unresolve_thread`, `add_reaction`, `remove_reaction`, `delete_comment`. Each already has `doc_id` in scope (the `broadcast_mentions` callers prove it for create/reply/edit; the others take `Path` params — confirm by reading each signature). If any handler genuinely lacks `doc_id` and only has a comment/thread id, derive it from the comment via the comments store (e.g. a `get`/`thread_doc_id` lookup) before notifying; do not guess.

- [ ] **Step 3: Create the listener**

Create `crates/knot-server/src/comments_listener.rs`:

```rust
//! Background task: forward `doc_comments` Postgres notifications to active
//! collab rooms so connected clients refetch their comments. Mirrors the
//! `acl_invalidate` listener. Never boots a room (see Rooms::notify_doc_comments).

use std::sync::Arc;

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::protocol::{MSG_COMMENTS, append_var_uint};

const CHANNEL: &str = "doc_comments";

pub fn spawn(pool: PgPool, rooms: Arc<knot_crdt::Rooms>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_once(&pool, &rooms).await {
                tracing::warn!(error=?e, "comments listener error; reconnecting in 5s");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    })
}

async fn run_once(pool: &PgPool, rooms: &Arc<knot_crdt::Rooms>) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    tracing::info!("comments listener subscribed to {CHANNEL}");
    loop {
        let note = listener.recv().await?;
        let payload = note.payload();
        let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        let Some(doc_id) = json
            .get("doc_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        // Frame: [MSG_COMMENTS][varuint len][json bytes] — matches the frontend
        // readVarBytes reader in KnotProvider.
        let body = payload.as_bytes();
        let mut frame = Vec::with_capacity(body.len() + 6);
        frame.push(MSG_COMMENTS);
        append_var_uint(&mut frame, body.len() as u64);
        frame.extend_from_slice(body);
        rooms.notify_doc_comments(doc_id, frame).await;
    }
}
```

- [ ] **Step 4: Declare the module + spawn it**

- In `crates/knot-server/src/lib.rs`, add `pub mod comments_listener;` next to `pub mod protocol;`.
- In `main.rs`, after the acl-listener `if let` block (~`main.rs:270`), add:

```rust
    if let (Some(pool), Some(rooms)) = (state.pool.clone(), state.rooms_v2.clone()) {
        let _handle = knot_server::comments_listener::spawn(pool, rooms);
        tracing::info!("comments listener spawned");
    }
```

- [ ] **Step 5: Build, test, clippy**

Run: `cargo build -p knot-server && cargo nextest run -p knot-server && cargo clippy -p knot-server --all-targets -- -D warnings`
Expected: builds, existing tests pass, no warnings. (The notify path is fire-and-forget; covered E2E in Task 5.)

- [ ] **Step 6: Commit**

```bash
git add crates/knot-server/src/routes/api/comments.rs crates/knot-server/src/comments_listener.rs crates/knot-server/src/main.rs crates/knot-server/src/lib.rs
git commit -m "feat(comments): notify active rooms on comment changes via LISTEN/NOTIFY"
```

---

## Task 3: Frontend provider — MSG_COMMENTS event + unit test

**Files:**
- Modify: `web/src/features/editor/KnotProvider.ts`
- Test: `web/src/features/editor/KnotProvider.test.ts`

Context (read the file first): `KnotProvider` defines `MSG_*` constants (`MSG_SYNC=0`, `MSG_AWARENESS=1`, `MSG_MENTION=4`), a `ProviderEvents` type, a `listeners` record initialized per event, `on/off`, and a `handleMessage(buf, start)` switch returning the next offset; `MSG_MENTION` reads `readVarBytes(buf, start+1)`, decodes JSON, and calls `this.listeners.mention.forEach(fn => fn(msg))`. `handleFrame` (private) loops over concatenated messages.

- [ ] **Step 1: Write the failing test**

Add to `web/src/features/editor/KnotProvider.test.ts` (mirror the existing `handleFrame`-based test and its varuint frame helper):

```ts
it("dispatches a MSG_COMMENTS frame to 'comments' listeners", () => {
  const p = new KnotProvider({ url: "ws://127.0.0.1:1/never", doc: new Y.Doc() });
  const seen: string[] = [];
  p.on("comments", (m) => seen.push(m.doc_id));

  const json = new TextEncoder().encode(JSON.stringify({ doc_id: "doc-123" }));
  const head = [5]; // MSG_COMMENTS
  // varuint length (same LEB128 encoding as the server's append_var_uint)
  let len = json.length;
  while (len >= 0x80) { head.push((len & 0x7f) | 0x80); len >>= 7; }
  head.push(len);
  const frame = new Uint8Array([...head, ...json]);

  (p as unknown as { handleFrame(b: Uint8Array): void }).handleFrame(frame);

  expect(seen).toEqual(["doc-123"]);
  p.destroy();
});
```

Run `cd web && pnpm test KnotProvider` → confirm FAIL (no `comments` event).

- [ ] **Step 2: Implement in KnotProvider.ts**

- Add constant near the others: `const MSG_COMMENTS = 5;`
- Add a payload type:
```ts
export type CommentChangeMsg = { doc_id: string };
```
- Extend `ProviderEvents`:
```ts
  comments: (msg: CommentChangeMsg) => void;
```
- Initialize its listener bucket where `listeners` is built (add `comments: []` to the initializer, matching how `mention: []` is set).
- In `handleMessage`, add a branch mirroring the `MSG_MENTION` one:
```ts
    } else if (type === MSG_COMMENTS) {
      const [payload, next] = readVarBytes(buf, start + 1);
      if (!payload) return null;
      try {
        const msg = JSON.parse(new TextDecoder().decode(payload)) as CommentChangeMsg;
        if (msg.doc_id) this.listeners.comments.forEach((fn) => fn(msg));
      } catch {
        /* malformed — ignore */
      }
      return next;
    }
```
(Match the exact local names used by the existing `MSG_MENTION` branch — `readVarBytes` return shape, `next` offset.)

- [ ] **Step 3: Run test + typecheck**

Run: `cd web && pnpm test KnotProvider && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add web/src/features/editor/KnotProvider.ts web/src/features/editor/KnotProvider.test.ts
git commit -m "feat(editor): KnotProvider emits a 'comments' event on MSG_COMMENTS"
```

---

## Task 4: KnotEditor subscribes + invalidates comments query

**Files:**
- Modify: `web/src/features/editor/KnotEditor.tsx`

Context: `KnotEditor` creates the provider in an effect and stores it in `pair` state; an existing effect (the mention subscription) does `provider.on("mention", fn)` / `off`. It has `docId` in scope.

- [ ] **Step 1: Add the subscription**

- Import `useQueryClient` from `@tanstack/react-query` (merge with existing imports). Add `const qc = useQueryClient();` near the other hooks.
- Add an effect mirroring the mention subscription (depend on `pair` and `docId`):
```tsx
  useEffect(() => {
    const { provider } = pair;
    const onComments = () => {
      void qc.invalidateQueries({ queryKey: ["comments", docId] });
    };
    provider.on("comments", onComments);
    return () => { provider.off("comments", onComments); };
  }, [pair, docId, qc]);
```
(Place it where `pair` is known non-null, alongside the existing mention effect; match that effect's guard for `pair`.)

- [ ] **Step 2: Typecheck + tests**

Run: `cd web && pnpm tsc --noEmit && pnpm test`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add web/src/features/editor/KnotEditor.tsx
git commit -m "feat(editor): refetch comments when a 'comments' event arrives"
```

---

## Task 5: E2E two-client realtime + verification

**Files:**
- Create: `e2e/flows/comments-realtime.spec.ts`

- [ ] **Step 1: Read an existing multi-context / comments e2e**

Run: `ls e2e/flows && sed -n '1,80p' e2e/flows/share-link.spec.ts` (it already uses two contexts: `owner` + anonymous). Reuse its two-context setup and the project's DB-reset (`execSync`) + `/setup` + invite patterns. Find how comments are posted in the UI (open the comment sidebar via `open-comments`, post a thread/reply; inspect `web/src/features/comments/CommentSidebar.tsx` / `CommentThread.tsx` for the testids).

- [ ] **Step 2: Write the spec**

Create `e2e/flows/comments-realtime.spec.ts`. Setup: owner creates a doc and invites an editor (reuse the invite flow with a password so the editor can log in). Both open the SAME doc (editor view, so the collab socket connects). Then:

```ts
// 1. owner posts a comment/reply in the sidebar.
// 2. WITHOUT reloading, assert the editor's sidebar shows that comment/reply
//    within a few seconds (use Playwright web-first assertions with a timeout,
//    e.g. await expect(editorPage.getByText("hello from owner")).toBeVisible({ timeout: 5000 })).
```

The acceptance assertion: the second client sees the new comment text appear **without a reload**. Keep it resilient (poll via `toBeVisible` timeout, not a fixed sleep).

- [ ] **Step 3: Run it**

Run: `cd e2e && pnpm playwright test comments-realtime`
Expected: PASS. If two-context realtime is too flaky after honest iteration (e.g. timing of the WS connect), reduce to asserting one direction (owner→editor) and/or bump the timeout; if still unstable, mark `test.fixme` with a clear comment and note it — but make a real effort, and keep the Task 3 unit test as the guaranteed gate.

- [ ] **Step 4: Commit**

```bash
git add e2e/flows/comments-realtime.spec.ts
git commit -m "test(e2e): replies appear for other viewers in real time"
```

- [ ] **Step 5: Full verification**

Run:
- `cargo nextest run -p knot-crdt -p knot-server` → PASS
- `cargo clippy -p knot-crdt -p knot-server --all-targets -- -D warnings` → clean
- `cd web && pnpm test && pnpm tsc --noEmit` → PASS
- `cd e2e && pnpm playwright test comments-realtime` → PASS (or documented fixme)
- Manual: two browsers on one doc; a reply in one appears in the other within ~1s.

---

## Self-Review notes

- Spec coverage: protocol const + ServerFrame + non-booting notify (Task 1) ✓; notify on all mutations + listener + spawn (Task 2) ✓; provider event + unit test (Task 3) ✓; KnotEditor invalidate (Task 4) ✓; two-client e2e + manual (Task 5) ✓.
- Idle-room safety: `notify_doc_comments` only sends when `map.get` finds a live room — never boots one. The listener does no `acquire`.
- Frame parity: server builds `[MSG_COMMENTS=5][append_var_uint len][json]`; the frontend test encodes the identical LEB128 varuint and the provider reads it with `readVarBytes`.
- Naming consistency: channel `doc_comments`, event `"comments"`, message type `MSG_COMMENTS=5`, payload `{ doc_id }` consistent across backend, frontend, and tests.
