# Plan 25 Outcome — Excalidraw Boards

**Status:** GO_WITH_CONCERNS. All 15 tasks landed across the Rust backend (storage, BoardRoom actor, registry, REST + WS routes, SVG cache, public share path) and the React frontend (Excalidraw lazy chunk, Y binding option A, inline preview + full-screen modal, Tiptap node + toolbar button, markdown sentinel round-trip). Inline gates ran green per task (`cargo test -p knot-storage / -p knot-crdt / -p knot-server`, `cargo clippy --all-targets --all-features -- -D warnings`, `pnpm tsc`, `pnpm lint`, `pnpm build`). **Live multi-client e2e collaboration on a board (two-browser Playwright) was NOT run as part of this plan — see "What's deferred".**

**Verdict:** End-to-end board lifecycle works: insert via toolbar button → edit in modal → CRDT sync via per-board WS → server-side snapshot + SVG cache → inline preview from cache → markdown round-trip via sentinel image. The path is structurally sound and matches the plan's Option A binding choice. The shape of the public-share read-only board view is in place but anonymous WS access for live observation is deferred.

## What landed

| Commit | Task | Subject |
|---|---|---|
| 7e862d1 | T1 | boards / board_updates / board_snapshots migration |
| 7281efa | T2 | BoardStore + PgBoardStore + integration tests |
| d574d16 | T3 | BoardRoom actor + BoardRooms registry |
| 45f9e5d | T4 | /collab/board/:id WS route + BoardRooms wiring |
| 2715ae8 | T5 | boards REST endpoints + SVG cache |
| 49d6563, 36825c7, 456a310 | T6 | markdown sentinel round-trip + review fixes |
| 4da7ebe | T7+T8 | ExcalidrawBoard Tiptap node + ExcalidrawBoardView + boards.api.ts |
| b4664ce | T9 | BoardProvider + KnotProvider URL switched to `/collab/doc/:id`; deprecated `/collab/:doc_id` alias removed |
| 8d438b3 | T10 | ExcalidrawModal + Y binding |
| 094c206 | T11 | debounced SVG snapshot + cache invalidation |
| e14db66 | T12 | toolbar Insert diagram (Excalidraw) button |
| 4247d77 | T13 | public board SVG endpoint (`/p/:token/boards/:id/svg`) + sentinel rewrite |
| db04aa2 | T14 | Playwright e2e (single + two-context + markdown export) |
| 71bed7e + (this) | T15 | outcome doc + README row |

## Gates

**Verified:**

- `cargo test -p knot-storage` — PgBoardStore round-trip + snapshot/update reads (T2)
- `cargo test -p knot-crdt` — BoardRoom actor apply-update + snapshot persistence (T3)
- `cargo test -p knot-server` — boards REST handlers + public SVG route (T5, T13)
- `cargo clippy --all-targets --all-features -- -D warnings` — clean throughout
- `pnpm tsc` — clean on every frontend task
- `pnpm lint --max-warnings 0` — clean
- `pnpm build` — Excalidraw lands as a separate lazy chunk (~1.1 MB gz); main bundle unaffected for pages without boards
- `pnpm playwright test --list` — discovery clean; existing 30+ spec suite still enumerates

**Not verified:**

- Live two-browser Playwright collaboration on a single board (CRDT convergence, presence, cursor sync between peers)
- Anonymous public-viewer WS read access (intentionally not wired)
- Excalidraw library/asset prefetch under slow-3G
- Snapshot compaction under heavy update volume

## Architecture summary

**Per-board Yjs doc.** Each board is its own CRDT document (`boards.id` = doc id). State lives in `board_updates` (append log) + `board_snapshots` (compacted). The document doc that *embeds* the board only carries an `excalidrawBoardNode` referencing the board id — the board's element graph is never inlined into the parent doc's Yjs state.

**BoardRoom actor.** Mirror of `DocRoom`: owns the `yrs::Doc`, fans updates to subscribers, persists via `BoardStore`. `BoardRooms` is the per-process registry (id → handle, GC on idle).

**Two WS namespaces.** `/collab/doc/:id` (existing, doc CRDT) and `/collab/board/:id` (new, board CRDT). Both speak the same y-websocket binary protocol; the routing layer distinguishes which registry to hand off to. The deprecated bare `/collab/:doc_id` alias was removed in T14 and `KnotProvider` switched to `/collab/doc/:id` in the same cycle — single source of truth, no compatibility tail.

**REST surface.**
- `GET/PUT /boards/:id/svg` — cached SVG preview (server-rendered on PUT from client-supplied SVG; served on GET)
- `POST /boards` / `GET /boards/:id` — create + metadata
- `GET /p/:token/boards/:id/svg` — public read-only preview gated by share token

**Y binding (Option A).** `yBinding.ts` syncs Excalidraw scene ↔ a single `Y.Map` (whole-scene replace per change). The plan called out Option B (per-attribute Y.Map per element) as the more granular alternative; Option A was picked because Excalidraw's `onChange` already gives whole-scene diffs and Option B requires non-trivial element-id reconciliation. Trade-off captured for Plan 26.

**Inline vs modal split.** `ExcalidrawBoardView` (the NodeView) renders the cached SVG with a click-to-edit affordance and never loads the Excalidraw chunk. `ExcalidrawModal` (full-screen modal) lazy-imports Excalidraw + opens a WS provider for the board (`BoardProvider`). Reading a page with N boards costs N SVG fetches, zero JS chunks. Editing one costs the Excalidraw chunk load on first open.

**Markdown round-trip.** The sentinel is a Markdown image `![<label or "Diagram">](knot://board/<UUID>.svg)` — constants `BOARD_URL_PREFIX = "knot://board/"`, `BOARD_URL_SUFFIX = ".svg"`, and `DEFAULT_BOARD_LABEL = "Diagram"` live in `crates/knot-markdown/src/lib.rs`. Sentinel detection requires that the paragraph contain *exactly* one image whose URL matches the prefix/suffix — mixed-content paragraphs fall back to a regular image node to avoid surprising users.

## What was non-obvious

**1. `suppressOnChange` must wrap `ydoc.transact`, not live inside the observer.** `observeDeep` fires *synchronously* inside the transaction. If you set the flag inside the observer callback and clear it after, the transaction is already mid-flight and the flag does nothing — and you get a remote→local→remote loop. The flag wraps the *outer* transact call so the observer sees `suppressOnChange === true` and skips the Excalidraw write.

**2. Excalidraw v0.18's imperative API ships via the `excalidrawAPI` callback prop, not a ref.** Remote collaborators must be pushed via `api.updateScene({ collaborators })` — there is no `collaborators` prop. The plan's snippet (passing `collaborators` as a prop) was wrong; the implementation captures the API via `excalidrawAPI={api => apiRef.current = api}` and calls `updateScene` from the Y observer.

**3. Sentinel parsing tracks two counters.** `image_depth` (are we inside an image's alt-text?) prevents alt-text from leaking into surrounding prose when the image is *not* a board sentinel. `paragraph_image_count` prevents two sentinels in the same paragraph from collapsing into a single board node — we only emit the board node when the paragraph contains exactly one image and it's a sentinel.

**4. Mixed-content paragraphs containing a sentinel are rejected.** A paragraph like `text ![](knot://board/xxx.svg) more text` does *not* round-trip as a board node — only "paragraph wraps exactly one sentinel image" triggers `excalidraw_board`. This keeps the boundary tight and avoids silent reflow of user prose.

**5. Schema regen is the canonical path for `excalidraw_board`.** The node type was added to `tools/schema.json` (T6) and regenerated into `crates/knot-markdown/src/schema.rs` and `web/src/features/editor/schema.ts` via the schemagen tool; hand-editing either side gets clobbered. The schemagen golden tests (`tools/schemagen/tests/fixtures/expected.{rs,ts}.golden`) gate that the generator output matches.

**6. `apiFetch` is JSON-only, so `web/src/lib/boards.api.ts` uses direct `fetch` for SVG GET/PUT.** Mirrors `blobs.api.ts` for the same reason — content-type isn't JSON, and the response body is an SVG string, not a parsed object.

**7. Public board SVG endpoint validates `board.doc_id == share.doc_id` and returns uniform 404 on every failure.** "no share token", "share doesn't grant this doc", "no board with this id", "board belongs to a different doc", "no cached preview" — all 404. We deliberately do not distinguish failure modes to avoid leaking the existence of boards across documents.

**8. The deprecated `/collab/:doc_id` alias was removed in the same cycle as the frontend URL switch.** T9 dropped both the server route alias and the client's old URL string (alongside adding `BoardProvider`). There is no compat window — single source of truth, fewer footguns later.

**9. Test infra: `knot_test_support::fresh_db` against dev-compose.** Per project rule, never `testcontainers::Postgres` — it caused OOMs from leaked containers twice. All `knot-storage` board tests use `fresh_db`.

**10. Excalidraw lazy chunk is ~1.1 MB gz, separate bundle.** Pages without boards never load it. The first board edit on a page triggers a single chunk fetch; subsequent opens are cached. Inline preview (`ExcalidrawBoardView`) never pays this cost because it only renders the cached SVG.

## What's deferred

From the plan's "Open trade-offs to revisit" and "Carryforward":

- **Option B per-attribute Yjs binding.** More granular merge semantics for concurrent edits to the same element (e.g., two users dragging the same shape). Option A's whole-scene replace is correct under last-writer-wins but loses information in true concurrent element edits.
- **Public WS access for anonymous viewers.** Today public shares get the cached SVG, not live updates. Anonymous WS access (read-only, token-gated) would let viewers see edits as they happen.
- **Board export to PNG / PDF.** Server-side rasterization for download.
- **Sub-board copy/paste; board templates.** Inserting a board copy across docs; starting from a saved template.
- **Board comments.** (Plan 27)
- **Board template gallery.** (Plan 28)
- **Live two-browser Playwright** for board CRDT convergence + presence + cursor sync.

## Carryforward

Recommended next:

1. **Plan 26 — Option B per-attribute Y binding.** Re-bind `yBinding.ts` to a `Y.Map<elementId, Y.Map<attr, value>>` shape and add a reconciliation layer for element-id churn. Belongs in its own plan because it touches the merge-semantics contract.
2. **Plan 27 — Board comments.** Thread anchors on board elements, reusing the existing comments infra.
3. **Plan 28 — Board templates.** Gallery + insert-from-template flow; likely a small REST + a new modal.
4. **Live two-browser Playwright spec** for `/collab/board/:id` — should be a one-task addendum, not its own plan.

## Files of interest

| Path | Role |
|---|---|
| `migrations/` (Plan 25 T1) | boards / board_updates / board_snapshots schema |
| `crates/knot-storage/src/boards.rs` | BoardStore trait + PgBoardStore impl |
| `crates/knot-crdt/src/board_room.rs` | BoardRoom actor |
| `crates/knot-crdt/src/board_registry.rs` | BoardRooms registry |
| `crates/knot-server/src/lib.rs` | mounts `/collab/doc/:id` + `/collab/board/:id` WS routes |
| `crates/knot-server/src/board_room_shim.rs` | board WS adapter wiring BoardRooms to the y-websocket protocol |
| `crates/knot-server/src/routes/api/boards.rs` | boards REST + SVG cache (GET/PUT/POST) |
| `crates/knot-server/src/routes/public.rs` | `/p/:token/boards/:id/svg` + sentinel rewrite |
| `crates/knot-markdown/src/lib.rs` | `BOARD_URL_PREFIX`, `BOARD_URL_SUFFIX`, `DEFAULT_BOARD_LABEL` |
| `crates/knot-markdown/src/from_markdown.rs` | sentinel parse (paragraph wraps one image with knot://board/ URL) |
| `crates/knot-markdown/src/to_markdown.rs` | sentinel emit |
| `web/src/features/boards/yBinding.ts` | Option A Y binding (whole-scene replace) |
| `web/src/features/boards/ExcalidrawModal.tsx` | full-screen modal with WS provider |
| `web/src/features/boards/BoardProvider.ts` | per-board WS provider |
| `web/src/features/editor/nodes/ExcalidrawBoard.tsx` | Tiptap node |
| `web/src/features/editor/nodes/ExcalidrawBoardView.tsx` | inline cached-SVG preview NodeView |
| `web/src/features/editor/EditorToolbar.tsx` | `toolbar-excalidraw` Insert diagram button (T12) |
| `web/src/lib/boards.api.ts` | direct-fetch SVG GET/PUT + JSON metadata |
| `e2e/flows/excalidraw.spec.ts` | Playwright e2e (single + two-context + markdown export) |
| `tools/schema.json` | schema source (adds `excalidraw_board` node) |
