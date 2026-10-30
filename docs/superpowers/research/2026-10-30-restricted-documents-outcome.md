# Outcome: restricted documents (private subtrees)

**Date:** 2026-10-30
**Spec:** [specs/2026-10-30-restricted-documents-design.md](../specs/2026-10-30-restricted-documents-design.md)
**Plan:** [plans/2026-10-30-restricted-documents.md](../plans/2026-10-30-restricted-documents.md)
**Goal:** A page owner can restrict a page so that only people with an explicit grant on it (or below it) can see it and its subpages. Workspace roles, the owner's included, give nothing inside. Ancestor grants above the boundary don't flow in.

## What landed

- **Rule** (`knot-docs/src/rule.rs`). One top-down fold computes a doc's access. A restricted node resets the state: the workspace role stops applying and inherited grants are dropped. Two evaluators share it:
  - a per-doc one, `access_along_path` over `chain_for_principal`;
  - a batched one, `compute_access` over `acl_nodes`, cached per (workspace, user) as `Visibility`.

  A parity test keeps the two in step. Non-members get nothing, whatever grants they hold.
- **Uniform no-access.** An unreadable page answers like an unknown id:
  - `403 acl.no_grant` on `/api/docs/{id}…`;
  - `404` as a body `parent_id`;
  - `422 doc.bad_anchor` as an anchor;
  - left out of every listing: sidebar, search, templates, archive, My tasks, export.

  Every single-doc `DocResponse` is built by one helper that nulls a parent the caller can't read.
- **Sidebar.** Readable pages under a hidden parent come back as `parent_hidden` and are hoisted into "Shared with you".
- **Guards.**
  - Last-owner: `409 grant.last_owner`. Only current members count as owners.
  - Grants for non-members: `422 grant.not_member`, checked inside the grant transaction under a membership row lock.
  - Boundary-crossing moves need Owner on the moved doc: `403 acl.restricted_move`.
  - Archive or purge over subpages the actor can't read: `409 doc.hidden_descendants`.
  - Restore out of an archived restricted parent: `409 doc.restore_restricted_parent_archived`.
  - Public links inside a restricted subtree: `409 share.restricted`. Restricting a page revokes the links of its whole subtree, and `/p` answers such a link exactly like a dead one.
- **Membership.** Removing a member deletes their grants. The migration also deletes grants already orphaned by earlier removals. A re-invite or OIDC re-provision therefore never revives old access.
- **Invalidation and realtime.** Every access change records an invalidation in the same transaction.
  - Role changes and member removal record a workspace-wide row (`doc_id IS NULL`, payload `workspace:<id>`).
  - The listener uses `PgListener::try_recv()`, so a silent sqlx reconnect is detected. On any gap or fallback it evicts the whole cache and restarts every doc and board room with a retryable close (1012), so every socket's access is checked again.
  - A per-doc revoke still closes with 4403.
  - An ACL epoch closes the race between the upgrade's access check and the room join.
- **Duplicate, import and export.**
  - Duplicate prunes hidden subpages. It re-homes a board or file copy only when neither side is in a restricted subtree, and fails closed otherwise.
  - Import creates restricted entries already restricted, in the same transaction. When a restricted entry fails to create, its manifest subtree is skipped.
  - Export carries the flag.
- **Admin CLI.** `knot-server admin restricted-list` and `admin grant` are the break-glass recovery path. The grant and its `admin.grant` audit row are written in one transaction.
- **Web.** Covered:
  - PermissionsDialog: the Restrict switch, "People with access" with sources, public links disabled inside.
  - Sidebar: locks, "Shared with you", per-doc `effective_role` gating, boundary-crossing confirms (between two subtrees too).
  - DocPage: chip, no-access copy, lost-access redirect.
  - The link picker inserts "Restricted page" for any target inside a restricted subtree.
  - The archive list sends `parent_hidden`, `effective_role` and `subpage_ids`.

## How it was built

- 17 plan tasks were run by a workflow. Each task had an implementer subagent and an independent reviewer. Every task was approved in its first review round.
- A whole-branch review followed. Six dimension finders (HTTP leaks, realtime/side channels, cache consistency, storage/rule, web, spec) produced 40 findings. Each was checked by three adversarial verifiers. 36 were confirmed, about 25 distinct after merging overlaps, including 2 critical:
  - Write responses (rename, template, restrict) leaked the hidden parent's id.
  - sqlx's silent listener reconnect skipped eviction.

  All were fixed in six reviewed groups.
- Final gates:
  - `cargo test --workspace`: 1069 passed.
  - Clippy and fmt: clean.
  - Web: vitest 849 passed; tsc and eslint clean.
  - Playwright: 21 passed (`restricted.spec.ts` plus role-gating, docs, duplicate, archive, share-link, tree-moves, templates, search, collab).

## Non-obvious things

- `PgListener::recv()` reconnects and re-LISTENs by itself when the connection drops. Notifications sent during the gap are lost and no error is returned. Only `try_recv()` reports the gap (`Ok(None)`).
- `Visibility` snapshots are cached for 60 s. Listings must use `visibility_covering(ids)`, or a just-created doc reads as hidden. Routes that change access evict the caller's subtree locally after commit, so the caller reads their own writes.
- For archived docs, "readable" means Editor+. A viewer's grant on an archived subpage counts as hidden for the purge guard and the archive counts.
- The e2e `reset.ts` truncates through `docker compose exec`. Run it with `COMPOSE_PROJECT_NAME` pointing at a throwaway Postgres, never the dev one.

## Known limits and carryforward

- **Ancestor flags aren't locked.** The boundary check on move locks the moved row and the destination, but not the `restricted` flags of their ancestors. Concurrent interleavings serialize to some legal order, so this doesn't open a path to declassify a page, but it isn't strictly race-free.
- **Hidden-subpage check before archive/purge.** It runs in the route, before the storage transaction. A hidden child created in that window could be archived with its parent.
- **Creator-grant race.** A creator who loses edit access to the parent while their create is in flight still gets the page created, but receives no grant. The page then needs an owner to share it.
- **Reconnect after a workspace-wide restart.** A user who really lost access gets HTTP 403 on the upgrade. The browser reports that as 1006, so the editor shows "offline" and retries instead of redirecting. This fails closed, but the experience could be better: re-check access in DocPage while offline.
- **Redundant per-doc invalidations.** `remove_member` still writes them alongside the workspace-wide one. They could be dropped.
- **Link text.** Text already stored in content is never rewritten when a page is restricted later.
