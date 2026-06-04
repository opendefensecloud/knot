# Document Templates (Plan 36)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** A doc can be flagged as a template. "New document" presents a template gallery; picking a template creates a fresh doc with a clean copy of the template's content.

**Architecture:**
- `docs.is_template BOOLEAN NOT NULL DEFAULT false`. Templates are normal docs that participate in the existing ACL system; the only special behavior is that they're filtered out of the main tree by default and listed in the "New document" gallery.
- Cloning happens through markdown round-trip — *not* a Y.Doc state copy. This intentionally drops the source template's comments, edit history, and CRDT lineage; the new doc starts fresh. Templates exist to be instantiated; carrying the source's collaborative ghost into every copy would be wrong.
- Per-workspace templates only in v1. Built-in catalog (RFC, meeting notes, weekly review) is a follow-up if there's demand.

---

## Tasks

### T1: Schema migration
- `migrations/`: add `is_template BOOLEAN NOT NULL DEFAULT false` on `docs`. Partial index `(workspace_id) WHERE is_template` for the gallery query.

### T2: API — flag & list
- `PATCH /api/docs/:id`: accept `is_template`. Owners only.
- `GET /api/workspace/templates`: returns `[{id, title, preview}]` for templates the user can read. Preview is the first paragraph (≤ 200 chars) from the doc's plain text.

### T3: API — create from template
- `POST /api/docs/from-template/:template_id { title, parent_id }`: enforces read access on the template + write access on the parent. Implementation:
  1. Export template to markdown via existing `doc_to_markdown` (Plan 33 T2 shared helper).
  2. Create a new empty doc with the given title + parent.
  3. Import the markdown into the new doc (reuse the importer from Plan 32 with `parent_id` pre-set, but only the single-doc path).
- The new doc is *not* a template (`is_template = false`).
- Returns the new `doc_id`.

### T4: Storage layer
- `PgDocStore`: `set_template(doc_id, bool)`, `list_templates(workspace_id, user_id)`. Integration tests via `knot_test_support::fresh_db`.

### T5: Frontend — "Save as template" action
- `DocPage` overflow menu: when `effRole === "owner"`, show "Save as template" / "Remove from templates" toggle. Toast on success.

### T6: Frontend — New Document picker
- Replace the current "+ New" instant-create with a small modal:
  - Top option: **Blank document** (current behavior).
  - Grid of template cards (title + preview snippet, ~3 columns on desktop).
  - Click → calls `POST /api/docs/from-template/:id`; on success, navigate to the new doc.
  - Empty state when no templates exist: "Save any doc as a template from its menu."
- testIds `new-doc-modal`, `new-doc-blank`, `template-card-<id>`.

### T7: Templates section in sidebar
- A collapsed "Templates" section in the workspace sidebar lists template docs (so owners can open/edit them). Templates do *not* appear in the main doc tree.

### T8: e2e
- `web/tests/templates.spec.ts`:
  - Create doc, write content, mark as template via menu.
  - Open New Document modal, see template card, click → new doc opens with the template's content.
  - Edit the original template; verify the previously-created doc is unchanged (clone, not link).
  - Unmark template; gallery card disappears.

### T9: Outcome doc

---
