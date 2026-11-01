# Restricted documents (private subtrees) — design

**Date:** 2026-10-30
**Status:** Approved (brainstorm)

## Context

The per-document permissions dialog suggests that a page can be locked
down, but it can't. `acl::resolve` (`crates/knot-docs/src/acl.rs`) computes

```
effective role = max(workspace role, grants on the doc, inherit=true grants on ancestors)
```

Grants only ever *raise* access. Every workspace member, viewers included,
can read every document, so there is no way to make a page and its subpages
private to a few named people.

This feature adds a **restricted** flag on a document. Inside a restricted
subtree the workspace role stops applying, and only explicit grants give
access.

A mapping pass over the codebase (7 subsystems plus a completeness critic)
found that `resolve` gates every `/api/docs/{id}/*` route, the collab
websockets, blobs, boards and shares, so those inherit the new rule for free.
The leaks are in everything that does *not* go through `resolve`:

- **Listings** that return every title in the workspace: the sidebar
  (`GET /api/docs`), templates, the archive list, `My tasks`, and search's
  `parent_id`.
- **Unchecked destination parents** on create, move, from-template and
  import. A move is also a declassification path: drag a page out of the
  restricted folder and the whole workspace can read it.
- **Subtree copies** that ignore per-doc access: duplicate copies hidden
  subpages, drops the flag, and re-homes restricted boards and images.
  Workspace export does the same.
- **Side doors**: public share tokens; comment-mention broadcasts; removed
  members whose grants survive, and which OIDC auto-provisioning silently
  revives; board websockets that are never revoked; and an ACL-cache
  insert-after-evict race.

## Decisions

| Question | Decision |
| --- | --- |
| Do workspace owners see restricted docs implicitly? | **No.** Strict: only explicit grants count, for everyone. Recovery for orphaned subtrees is a break-glass `knot-server admin` CLI command, not an in-app override. |
| Do ancestor grants above the restricted doc flow in? | **No.** The restricted doc is a hard boundary. Only grants on the boundary doc and on docs below it count. With nested restrictions, the innermost boundary wins. |
| How does the tree show a readable doc whose restricted parent is hidden? | It is hoisted into a **"Shared with you"** sidebar section. The server sends `parent_id: null` and `parent_hidden: true`, so the hidden parent's id never reaches the client. |
| Who can restrict or unrestrict? | Anyone whose *effective* role on the doc is Owner. |
| Lock-out protection | Restricting a doc grants the actor `owner` (inherit=true) on it, in the same transaction. On a restricted doc, you can't delete or downgrade the last `owner` grant (409 `grant.last_owner`). Only `user:` owner grants of current workspace members count. |
| Response for no access | Unchanged and uniform. Doc-id routes answer 403 `acl.no_grant`, the same as for unknown or foreign ids. A parent id in a request body that the caller can't read answers 404 `doc.not_found`, the same as for unknown or foreign parents. There is no existence oracle. |
| Moving across a boundary | Moving a page into or out of a restricted subtree, or between two different ones, needs **Owner** on the moved page (403 `acl.restricted_move`). Every move also needs Editor+ on the destination parent. |
| Creating inside a restricted subtree | Needs Editor+ on the parent. If the creator would not be able to edit the new page (their access came from a grant with inherit=false), they get an `owner` grant on it automatically. The grant is decided from one fresh snapshot of the chain, the creator's grants and their workspace role, and only when they can still edit the parent then. A creator who lost the parent after the route's check (a concurrent restriction, revocation or role change) gets no grant, and the page stays where it was made, as if it had been made just before the change. The same applies to from-template, import and duplicate. |
| Duplicate | Subpages the actor can't read are pruned. Copies of restricted pages stay restricted, and the actor gets `owner` on them. Boards and images owned by a doc that is not copied are re-homed onto the copy only when neither that doc nor the copy's destination is in a restricted subtree. Otherwise they are not re-homed, even within one boundary, where readers differ by branch through their grants. The reference is kept, and it renders as "unavailable", as for unreadable assets today. Other grants are still not copied. |
| Archive and purge of a subtree containing pages the actor can't see | Refused, 409 `doc.hidden_descendants`. Otherwise an editor could hide or destroy other people's private pages. This reveals only that *something* is hidden below. |
| Restore that would re-parent out of a restricted subtree | Refused, 409 `doc.restore_restricted_parent_archived`. Restore the archived ancestor first. |
| Public share links | Can't be created on a page inside a restricted subtree (409 `share.restricted`). Restricting a page revokes the active links in its subtree. Serving `/p/*` re-checks the restriction, which covers pages that are moved into a restricted subtree. `/p/*` responses become `private, no-cache`. |
| Workspace export | Includes only docs the exporting owner can read. The manifest carries `restricted`, so a round trip keeps restrictions, and the importer becomes `owner` of each restricted page. |
| Member removal | Deletes the member's `user:` grants in the same transaction, with ACL invalidation. `GrantStore::put` checks membership inside its own transaction, locking the membership row `FOR KEY SHARE`, so a grant can't slip in beside a concurrent removal. The migration drops `user:` grants that earlier releases left behind for non-members. A re-invite or OIDC re-provision does not revive access. |
| Group principals (`group:`) | Out of scope. v1 is user grants only, as today. |
| Link text | A doc link stores its text in the *linking* doc's content. When the target is within a restricted subtree, the link picker inserts "Restricted page" as the text instead of the target's title, even from inside a restricted subtree, because readership differs by branch (and between boundaries). Stored text in existing content is not rewritten. Inaccessible link chips get a lock style. |

## Design

### 1. Data model (migration `20261030120000_restricted_docs.sql`)

```sql
ALTER TABLE documents ADD COLUMN restricted boolean NOT NULL DEFAULT false;
CREATE INDEX documents_restricted_idx ON documents (workspace_id) WHERE restricted;
CREATE INDEX document_grants_principal_idx ON document_grants (principal, doc_id);
-- Grants never outlive the membership: drop the `user:` grants of people
-- who are not members of the doc's workspace (left by removals before this
-- release, or granted to outsiders by the old grants route).
DELETE FROM document_grants g USING documents d
 WHERE g.doc_id = d.id AND g.principal LIKE 'user:%'
   AND NOT EXISTS (SELECT 1 FROM workspace_members m
                    WHERE m.workspace_id = d.workspace_id
                      AND g.principal = 'user:' || m.user_id::text);
```

`Document`, `DocRow`, `COLS` and `doc_from_row` (`doc_store.rs`) gain
`restricted`. `DocResponse` gains it as well.

### 2. Access rule: one step function, two evaluators

The rule is a pure top-down fold over a root-to-doc path, in a new
`crates/knot-docs/src/rule.rs`:

```
state = { ws_applies: true, inherited: None, boundary: None }
for node in path (root → doc):
    if node.restricted:                 # hard boundary
        state = { ws_applies: false, inherited: None, boundary: Some(node.id) }
    own       = max role of grants for this user on node (any inherit)
    role(node)= max(own, state.inherited, ws_role if state.ws_applies)
    state.inherited = max(state.inherited, max role of this user's inherit=true grants on node)
```

The user must be a workspace member. With no `ws_role`, the result is `None`
regardless of grants (defence in depth for removed members).

- **Per doc**: `acl::resolve`. A new `GrantStore::chain_for_principal(ws, doc,
  principal)` loads the ancestor chain (`id, parent_id, restricted,
  archived_at`) plus the principal's grants on it, in one recursive CTE that
  keeps the workspace filter and the `depth < 10000` cap. It then folds the
  chain with the step function. `DocAccess` gains `restriction_root:
  Option<Uuid>`, the nearest restricted ancestor-or-self, which is `Some`
  exactly when the doc is in a restricted subtree. `can_read` and
  `can_write` are unchanged.
- **Batched**: `acl::compute_access(nodes, grants, ws_role) -> HashMap<Uuid,
  DocAccess>` walks the whole workspace tree top-down with the same step
  function. It is fed by `DocStore::acl_nodes(ws)` (every doc, including
  archived docs and templates) and `GrantStore::list_for_principal(ws,
  principal)`. `AclCache::visibility(ws, user)` wraps it and returns an
  `Arc<Visibility>` with `access(id)`, `hidden_ids()` and `is_visible(id)`.

  **Fast path**: if the workspace has no restricted doc (`EXISTS` on the
  partial index), every doc gets the member's base role from
  `max(ws_role, grants)`. That base still has to be computed, because the
  list carries per-doc `effective_role`.

  A parity test checks that `resolve(doc) == compute_access(...)[doc]` over
  generated trees, so the two evaluators cannot drift.

### 3. Cache and invalidation

- Toggling `restricted` writes `invalidations::record_in_tx(ws, doc,
  "restrict" | "unrestrict")` in its transaction. The existing listener
  already evicts the doc and every descendant for all users, and revokes
  their doc rooms.
- `AclCache` gets a **generation guard**. A global `AtomicU64` is bumped by
  every eviction. `access()` snapshots it before `resolve` and inserts only
  if it is unchanged; otherwise it retries, at most 3 times, then returns
  the fresh value uncached. This closes the race in which a resolve that
  started before an eviction inserts a stale role afterwards.
- `AclCache::visibility` gets its own moka cache keyed by `(ws, user)`, with
  a 60 s TTL. Every listener eviction and every local eviction clears it
  completely, since ACL changes are rare.
- When the listener may have missed or only partly handled a change, it
  invalidates everything: `evict_all()`, then `on_invalidate_all`, which
  ends every doc and board room on the replica (§5). It does so right after
  (re)subscribing; whenever sqlx reports a lost connection (the listener
  uses `try_recv`, since `recv` reconnects silently and drops the NOTIFYs
  sent meanwhile); if `descendant_ids` fails; and on a malformed payload.
- A workspace role change (`PATCH /api/workspace/members/{id}`) and a member
  removal change one member's access in every doc. Each records a
  workspace-wide invalidation in its transaction (`acl_invalidations` row
  with `doc_id NULL`, reason `member-role` or `member-remove`, NOTIFY
  payload `workspace:<id>`), which every listener handles by invalidating
  everything. The route also evicts its own replica's cache at once, so a
  demoted owner can't restrict a page (and take it over) on a cached owner
  role.
- Routes that change access call `archive::evict_local(state, subtree_ids)`
  after commit to read their own writes, as archive does today.

### 4. API

**New**

- `PUT /api/docs/{id}/restricted` with body `{restricted: bool}`. Owner
  only, behind `require_doc_role_mw`. Restricting (in one transaction): set
  the flag, upsert the actor's `owner` inherit=true grant, revoke the active
  share tokens of the doc and its subtree, audit `doc.restrict`, and record
  the invalidation. Unrestricting: clear the flag, audit `doc.unrestrict`,
  and record the invalidation. Both are idempotent. The response is the
  updated `DocResponse`.
- `GET /api/docs/{id}/access` (any reader). Returns `{restricted,
  restriction_root_id, restriction_root_title, people: [{principal, role,
  inherit, source_doc_id, source_title}]}`. `people` lists the grants that
  apply to this doc: its own grants plus inherit=true grants from ancestors,
  stopping at the boundary when the doc is restricted. Root and source
  titles are given only for docs the caller can read; otherwise they are
  `null`. This powers "People with access" in the dialog.

**Changed**

| Endpoint | Change |
| --- | --- |
| `GET /api/docs` | Filtered by `visibility`. Each doc carries `restricted`, `within_restricted` and `effective_role`, plus `parent_hidden`. When the parent is not visible: `parent_id: null, parent_hidden: true`. |
| `GET /api/docs/{id}` | Adds `restricted`, `within_restricted`, and `restriction_root_id` (null unless readable). `parent_id` is nulled if the parent is unreadable. `archive_root_title` and `archive_root_id` are nulled if the root is unreadable. |
| Every other route answering with one doc (`PATCH /api/docs/{id}`, `POST …/template`, `PUT …/restricted`, create, from-template, move, duplicate) | Built by the same helper as `GET /api/docs/{id}`: `parent_id` is nulled if the parent is unreadable, so a deep grantee who renames their page doesn't learn the hidden boundary's id. |
| `POST /api/docs`, `POST /api/docs/from-template/{id}`, `POST /api/workspace/import?parent_id=` | Parent must be Editor+ for the caller (unreadable → 404 `doc.not_found`; viewer → 403 `acl.editor_required`). Only top-level creates use the workspace-role gate. After creation, if the creator can't edit the new doc but can still edit the parent, they get an `owner` grant (see Decisions). |
| `POST /api/docs/{id}/move` | Destination parent must be Editor+ (unreadable → 404). A boundary crossing (the moved doc's `restriction_root` excluding its own flag, before vs after) needs Owner on the moved doc (403 `acl.restricted_move`). The check runs in storage under the move's group lock (`move_placed_once`), so it is race-free. Anchors the caller can't read map to 422 `doc.bad_anchor`. |
| `POST /api/docs/{id}/duplicate` | See Decisions. Destination (source's parent) must be Editor+. |
| `DELETE /api/docs/{id}` (archive), `POST …/purge` | 409 `doc.hidden_descendants` when the live (or archived) subtree has a doc the actor can't read. An archived doc on which the actor is only a viewer counts as unreadable (viewers can't read archived docs), so archiving a page whose restricted subpage the actor only views is allowed, but purging it afterwards is not. |
| `POST …/restore` | 409 `doc.restore_restricted_parent_archived` when the re-parent-to-top-level fallback would take the doc out of a restricted subtree. |
| `PUT/DELETE /api/docs/{id}/grants/{principal}` | Last-owner guard on restricted docs (409 `grant.last_owner`). A principal must be a current workspace member (422 `grant.not_member`), checked inside `GrantStore::put`'s transaction. |
| `GET /api/workspace/templates` | Filtered by visibility. |
| `GET /api/workspace/archive` | Rows filtered to archive roots where the caller's access is Editor+. `parent_title` and `parent_id` are null when unreadable, and `parent_hidden` is then true (a parent exists). Rows also carry `effective_role` (the caller's role on the page, which gates purge) and `subpage_ids` (the archived subpages the caller can open). `subpage_count` counts only those readable subpages. |
| `GET /api/search` | `SearchStore::search` takes the hidden set (`AND NOT (d.id = ANY($n))`), so `LIMIT` applies after the filter. `parent_id` is nulled when hidden. The per-hit `effective_role` loop stays as defence in depth. |
| `GET /api/workspace/tasks` | `list_for_assignee` takes the hidden set, and titles are joined in SQL (removes the N+1). |
| `GET /api/workspace/export` | Only readable docs. A `parent_id` outside the export is dropped. `DocEntry` gains `#[serde(default)] restricted`, set on boundary docs and on any exported root that is effectively restricted. |
| `POST /api/workspace/import` | Docs with `restricted: true` are created restricted, with the importer as `owner`, in the transaction that inserts them, so their title is never listed to the whole workspace. A page that fails to import is skipped with everything below it when it or a manifest ancestor is restricted (only the boundary carries the flag, so its subpages grafted onto the import target would be unrestricted); otherwise its subpages are grafted onto the import target as before. Skipped pages are left out of `imported_docs`. |
| `POST /api/docs/{id}/shares` | 409 `share.restricted` inside a restricted subtree. |
| `GET /p/{token}`, `/p/{token}/blobs/{id}`, `/p/{token}/boards/{id}/svg` | When the shared doc is in a restricted subtree, each route answers exactly as for a dead (revoked, expired or archived) link: 410 on `/p/{token}`, 404 on the blob and board routes. Doc-link rewriting never targets a doc in a restricted subtree. Responses use `Cache-Control: private, no-cache`. |
| `DELETE /api/docs/{id}/comments/{cid}` | "Delete others' comments" uses the effective doc role Owner, not the workspace role. |
| comment mention broadcast | Recipients filtered by `effective_role` on the doc. |
| `DELETE /api/workspace/members/{id}` | Deletes the member's grants and records an invalidation per affected doc plus one workspace-wide, in one transaction. |
| `PATCH /api/workspace/members/{id}` | Records a workspace-wide invalidation in the role change's transaction, and evicts the local cache. |

### 5. Realtime

- Doc rooms: the restrict toggle's invalidation drives the existing
  `revoke_all_for_doc` path, and the client handles 4403 already.
- Board rooms: `BoardRoomHandle` gains `doc_id` and a `revoked` token,
  `BoardRooms::revoke_for_doc(doc_id)` mirrors the doc-room `Revoke`, and
  `on_invalidate` and purge call it. Today the board shim passes a fresh,
  never-cancelled token.
- Join race: `Rooms` keeps striped ACL epochs (`[AtomicU64; 64]`, indexed by
  hash(doc_id)), bumped by every revoke even when no room is live. The
  upgrade snapshots the epoch before `acl.access`. After `Join`, a changed
  epoch cancels the connection's token, so the client gets 4403 and
  re-checks. Board rooms use the same scheme.
- Invalidating everything (§3): `Rooms::restart_all` and
  `BoardRooms::restart_all` bump every epoch stripe and end every room on
  the replica, so each socket closes with 1012, not 4403. Clients reconnect
  on 1012 by themselves, and the upgrade's access check refuses only those
  who lost their access. The listener's hooks are
  `knot_docs::InvalidationHooks { on_invalidate, on_invalidate_all }`, built
  by `knot_server::revoke_doc_sockets`.

### 6. Break-glass CLI

`knot-server admin restricted-list --workspace <slug>` prints the boundary
docs with id, title and owner grants. It is for the operator only, with
direct DB access.

`knot-server admin grant --doc <id> --email <e> --role owner [--no-inherit]`
goes through `GrantStore::put_break_glass`, so the invalidation fires and
the membership check and last-owner guard apply. There is no acting user:
the grant row's `granted_by` is NULL, and the grant is audited only as
`admin.grant` (actor NULL, `data: {break_glass: true, principal, role,
inherit}`), in the grant's own transaction. No `doc.grant` event names the
recipient as the actor.

### 7. Web UI

- **Validators**: `Doc` gains `restricted`, `within_restricted`,
  `parent_hidden` and `effective_role`. `DocWithRole` gains `restricted`,
  `within_restricted` and `restriction_root_id`. Each uses `v.fallback`, so
  old payloads and fixtures still parse.
- **Sidebar**:
  - Docs with `parent_hidden` render in a **"Shared with you"** section
    under the workspace roots.
  - A lock icon marks restricted boundaries, and a dim lock marks inherited
    restriction. `rowDescription` says "restricted".
  - Row actions (rename, move, archive, add subpage, permissions) are gated
    by the per-doc `effective_role`, not the workspace role.
  - `parent_hidden` rows are not drag sources and can't be keyboard-moved.
  - A drop that crosses a restriction boundary asks for confirmation:
    "Moving 'X' into 'Y' limits who can see it", "…changes who can see it
    to the people with access there" (between two restricted subtrees,
    nested boundaries and "Shared with you" pages included) or "…makes it
    visible to the whole workspace". The client finds each side's nearest
    restricted ancestor from the listed docs; a `parent_hidden` page inside
    a restricted subtree counts as its own boundary.
  - "Same level" create is not offered next to a "Shared with you" page, and
    drops and nested creates are gated on the target's `effective_role`.
  - New error codes get specific toasts.
- **DocPage**:
  - A "Restricted" chip in the header. For owners it links to permissions.
  - Neutral no-access copy: "This page doesn't exist or you don't have
    access to it."
  - On losing access, invalidate `['docs']` before navigating away.
- **PermissionsDialog**:
  - An **Access** section with a "Restrict access" switch, a confirm on
    enabling, and copy that depends on the state.
  - An inherited restriction shows a disabled switch with "Restricted by
    'X'".
  - A "People with access" list built from `/access`.
  - The public link section is disabled inside restricted subtrees.
  - `grant.last_owner` and `grant.not_member` get toasts.
  - Stale copy is fixed.
- **Link picker**: a lock on restricted targets. When the target is
  `within_restricted`, the inserted text is "Restricted page". Link chips
  get a locked style only when the target is not among the docs the client
  knows to be readable (the sidebar list, templates and openable archived
  pages).
- **MembersPage**: the remove confirm says that the member's page grants are
  removed.

### 8. Testing

- **Rule**: table tests for the step function (boundary, nested boundary,
  grant above the boundary, inherit=false, non-member) and the parity test
  between `compute_access` and `resolve`.
- **Storage**: the `chain_for_principal`, `list_for_principal`, `acl_nodes`,
  `set_restricted` and last-owner guard tests, and the move boundary check.
- **Listener and cache**: restricting evicts the subtree and calls
  `on_invalidate` for descendants. A lost connection, a failed subtree walk,
  a malformed payload and a role change each evict everything and call
  `on_invalidate_all`. The generation guard drops a stale insert.
- **HTTP** (one `restricted_integration.rs`, plus extensions of existing
  suites): every changed endpoint in §4 gets a member-without-grant and a
  grantee case. A restricted id and a random uuid get byte-identical
  responses. A board socket is closed on restrict. A removed member who is
  re-provisioned does not regain access.
- **Web**: vitest for validators, tree grouping and lock, per-row gating,
  the PermissionsDialog toggle and last-owner toast, the DocPage chip and
  no-access copy, and the link-picker text.
- **e2e**: `e2e/flows/restricted.spec.ts` covers restricting, the hidden
  tree, the direct URL, granting, and "Shared with you".

## Out of scope

- `group:` principals and OIDC-group grants.
- Per-user kicks. A restrict kicks every socket in the subtree, and grantees
  reconnect transparently.
- Rewriting doc-link text already stored in other documents.
- Marking mention-picker members who have no access.
- An in-app owner break-glass.
