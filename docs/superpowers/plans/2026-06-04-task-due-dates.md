# Task Due Dates (Plan 35)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Depends on:** Plan 34 (Inline Date/Time Picker). The `datetime` node + `knot://time/` sentinel are prerequisites; this plan reuses them.

**Goal:** Attach a "due by" timestamp to a task item; the `/tasks` page sorts and styles by urgency (overdue, today, this week, later, none).

**Architecture:**
- A task item's due date is the first `datetime` node inside the task that is preceded by an explicit affordance — either the keyword "by" / "due" (case-insensitive) or, in the UI, a dedicated "Set due date" action on the task item that inserts the datetime with a leading "by ".
- Rationale: free-form inference (any datetime ⇒ due) misfires on "meeting at 3pm". Requiring an explicit cue keeps the contract clean and makes the data trustworthy without a separate field on the schema.
- Extractor reads the task item's content and pulls the first such datetime into `tasks.due_at` (new column, nullable).

---

## Tasks

### T1: Schema migration
- `migrations/`: add `due_at TIMESTAMPTZ NULL` to `tasks`. Index on `(assignee_user_id, due_at)` for the sorted query in T4.

### T2: Extractor — pick up due dates
- `crates/knot-server/src/tasks/extract.rs`: after the existing task-item walk produces `text`, re-scan the task's inline content for a `datetime` node whose preceding text (last ~16 chars before it, after trimming) matches `(?i)\b(by|due)\b\s*$`. Use a small hand-rolled scanner — no regex on the hot path; bytewise lowercase check is fine.
- If found, set `due_at = Some(iso)`; otherwise `None`.
- Unit tests: `[ ] @me ship by //2026-06-04T17:00:00Z` → `due_at = Some(...)`; `[ ] @me meeting at //2026-06-04T15:00:00Z` → `due_at = None`.

### T3: API surface
- `GET /api/tasks?include_completed=...` returns `due_at` on each row.
- New optional query: `?sort=due` (default) sorts overdue→today→week→later→none, then by doc title for stability.

### T4: Frontend — sorted/grouped view
- `TasksPage.tsx`: group tasks into buckets (Overdue / Today / This week / Later / No date) instead of grouping by doc. Within a bucket, sub-group by doc.
- Style: Overdue rows use `text-destructive`; Today uses amber accent; the rest neutral.
- Each row shows the due chip on the right (`Jun 4 · 5pm`).

### T5: Task item affordance in editor
- A task item with no due date shows a faded "Add due date" pill (visible on hover/focus of the list item). Click → opens the `DateTimePopover` (Plan 34, T5); on apply, inserts ` by <datetime-node>` at the end of the task's inline content.
- A task item with a due date shows the chip inline; the same hover pill becomes "Change" / "Clear".
- `testid="task-due-pill"`.

### T6: e2e
- `web/tests/task-due-dates.spec.ts`:
  - Add a task with a due date via the pill, navigate to /tasks, assert it lands in the right bucket.
  - Change the due date, reload /tasks, bucket updates.
  - Clear it, row moves to "No date".

### T7: Outcome doc

---
