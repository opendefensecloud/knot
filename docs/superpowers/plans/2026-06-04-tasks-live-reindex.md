# Tasks Live Reindex (Plan 33)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** When a user adds, edits, or removes a `[ ] @assignee` task in a doc, the `/tasks` page reflects it without a manual Refresh click.

**Root cause today:** task extraction only runs when something hits `/api/docs/:id/markdown`. The tasks page's `refetchOnMount: "always"` fetches a *stale* index. We need the extractor to run on durable doc updates.

**Architecture:**
- Server-side: extend the snapshot/flush path in `BoardRoom`'s sibling — `DocRoom` — to re-extract task rows whenever a writer-job persists.
- Debounce so we don't re-parse on every keystroke: extract at most once per N seconds per doc (use the existing snapshot tick, not a new timer).
- Extraction is cheap (single pulldown pass, replaces rows for that doc inside a transaction). No new channel needed; the client's `refetchOnMount` already runs on visit.
- Optional follow-up: workspace WS push so an *open* `/tasks` page updates live. Out of scope for this plan; flagged as Plan 34.

---

## Tasks

### T1: Extractor entry point
- `crates/knot-server/src/crdt/room.rs`: after a writer-job's `persisted` oneshot fires for a doc update, schedule a `reindex_tasks(doc_id)` job on the actor's mailbox if not already pending. Coalesce: a single pending flag per doc, cleared when the job runs.
- Reuse existing `extract_tasks(&markdown)` + `TaskStore::replace_for_doc(doc_id, rows)`. Trigger is the snapshot tick (every N seconds), not every update — bound the work.
- Add a `tracing::debug!` at job start/end with `doc_id` and row count.

### T2: Markdown source for extraction
- Reuse the same path that `/api/docs/:id/markdown` uses today (`yrs → to_markdown`). Factor the common code into `knot_markdown::doc_to_markdown(&Doc)` if not already shared, so the room actor and the HTTP handler call the same function.

### T3: Integration test
- `crates/knot-server/tests/tasks_live_reindex.rs`: open a doc via WS, apply a Y update that inserts a task item with an `@user` mention, await snapshot tick, assert `task_store.list_for_user(user_id)` returns the new row without any HTTP roundtrip.
- Uses `knot_test_support::fresh_db`. Spawn the full server harness; no manual `testcontainers`.

### T4: Frontend cleanup
- `web/src/features/tasks/TasksPage.tsx`: `refetchOnMount: "always"` stays; remove the `Refresh` button's now-redundant "tickle every doc's markdown export" loop. Keep the button for a manual hard refresh that calls a single `/api/tasks/reindex` (optional admin route; otherwise just `invalidateQueries`).

### T5: e2e
- `web/tests/tasks-live-reindex.spec.ts`: in doc A, type `[ ] @me thing`, wait for snapshot interval + small buffer, navigate to `/tasks`, assert "thing" visible without clicking Refresh.

### T6: Outcome doc

---
