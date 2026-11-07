# Architecture

A one-page overview. For the design rationale, read the [long-form spec](docs/superpowers/specs/2026-06-01-knot-foundation-design.md). For per-plan history (what landed and why), read `docs/superpowers/research/`.

## System diagram

```
Browser (SPA)
  │ HTTPS + WSS  (single origin in prod via ingress; Vite dev-proxy locally)
  ▼
knot-server (axum, Rust, single binary)
  ├─ /                       SPA fallback (ServeDir on web/dist + index.html)
  ├─ /auth/*                 knot-auth: Argon2id, OIDC, sid + csrf cookies
  ├─ /api/*                  knot-storage: sqlx stores (Postgres); sid cookie or Bearer API key
  ├─ /collab/:doc_id WS      knot-crdt: Room actor (yrs Y.Doc per doc)
  ├─ /api/healthz, /readyz   liveness + readiness
  └─ :9090/metrics           Prometheus exposition
            │
            ▼
       PostgreSQL 18
       ├─ users, workspaces, sessions, workspace_members, api_keys
       ├─ documents, document_grants
       ├─ doc_updates, doc_snapshots          (CRDT persistence)
       ├─ doc_markdown_cache                  (export)
       └─ acl_invalidations                   (LISTEN/NOTIFY for ACL revoke)
```

There is **no Redis**. Cross-pod CRDT update fan-out and ACL revocation both ride Postgres `LISTEN/NOTIFY`.

## Crates

| Crate | Role |
|---|---|
| `knot-server`     | axum router, middleware chain, route handlers, SPA fallback |
| `knot-auth`       | password hashing, session creation, API-key generation + HMAC, OIDC discovery + flow |
| `knot-config`     | figment-based `KNOT_*` env loader (default → file → env) |
| `knot-storage`    | sqlx stores (users, workspaces, docs, grants, snapshots, updates) + migrations |
| `knot-crdt`       | yrs `Engine`, `Rooms` registry, `Room` actor, `PgBus` over LISTEN/NOTIFY |
| `knot-docs`       | ACL evaluation engine (one rule, per doc via `acl::resolve` and per workspace via the cached `Visibility`; restricted subtrees ignore workspace roles) + ACL invalidation listener |
| `knot-markdown`   | Markdown ↔ ProseMirror round-trip (canonical schema) |
| `knot-export`     | Page → DOCX/ODT (pure; walks the Y.Doc) |
| `knot-obs`        | structured logging, OTLP traces, Prometheus exporter, well-known metric describes |
| `knot-test-support` | `fresh_db()` against the dev-compose Postgres (NEVER testcontainers) |
| `tools/schemagen` | generates `schema.rs` (server) + `schema.ts` (client) from `tools/schema.json` |

## Request lifecycle (HTTP)

```
client                middleware (outside → inside)              handler
  │                  ┌─ metrics record (counter + histogram)
  │   HTTP   ─────►  ├─ tower-http TraceLayer  (#[instrument] inside)
  │                  ├─ session loader (Bearer API key on /api/* → AuthContext + ApiPrincipal,
  │                  │                  else sid cookie → AuthContext)
  │                  ├─ require session (/api/* only)
  │                  ├─ scope check (/api/* with an API key: method → scope, override table)
  │                  ├─ CSRF check (unsafe methods; skipped for API keys)
  │                  └─ route handler         ─────►  knot-auth / knot-storage / knot-crdt
  │   HTTP   ◄─────                            ◄─────
```

Outermost layer is the metrics recorder so the histogram includes everything inside. TraceLayer wraps handlers + auth so spans cover the full lifetime minus the metrics recording itself.

API keys (`Authorization: Bearer knot_…`) are resolved by the session loader, and only for paths under `/api/` (`auth/api_key_loader.rs`); `/auth/*`, `/collab/*` and `/p/*` stay cookie-only. A Bearer header that doesn't resolve to a live key of a current member is a 401 `api_key.invalid`, never a fall-back to the cookie. Keys are stored as HMAC-SHA256 under `KNOT_SESSION_KEY`, so rotating that key invalidates every session and every API key. Scopes are enforced by a separate middleware (`auth/scope.rs`) layered between require-session and CSRF on the `/api` router: the HTTP method picks the scope (GET/HEAD/OPTIONS read, POST create, PATCH/PUT update, DELETE delete), and its `OVERRIDES` table is the one place POST routes that aren't creates are remapped. A key lacking the scope gets 403 `api_key.scope_denied` with `details.scope`. Its `SESSION_ONLY` table lists the routes no key may call, whatever its scopes (403 `api_key.session_required`): key management (`/api/auth/api-keys`), member management (invite, role change, removal) and creating or listing public share links, since each would let a key mint a credential that is unscoped or outlives it. Bulk key revocation lives in DB triggers: on a password-hash change (the user's keys and the bot keys the user minted), on leaving the workspace (the member's keys and those it minted) and on losing the owner role (the keys it minted for others). A mint closes the race with them in its own transaction (`PgApiKeyStore::create`): it locks the user rows (a password change sets the hash and ends the sessions in one transaction), re-checks the session that asked, share-locks the membership rows and enforces the 50-live-keys cap before the insert, so a concurrent revoker either refuses the mint or revokes the committed key.

## CRDT data flow

```
client A                            knot-server (Room actor)                     client B (same doc)
   │ edit ─► Y.apply_update                                                            │
   │ ─── WS frame: SyncUpdate ────► on_inbound                                         │
   │                                  ├─► engine.apply_update(&doc, &update)            │
   │                                  ├─► writer task: persist update + maybe snapshot │
   │                                  ├─► bus.publish(payload) ── LISTEN/NOTIFY ──►    │
   │                                  └─► fan_out_local(payload) ─ WS frame ─►  client │
                                                                              SyncUpdate
   │                                                            ◄ WS frame: SyncUpdate │
   │                                                                                   │
```

The room actor wraps **all** outbound CRDT updates in the y-sync v1 protocol framing (`MSG_SYNC` + `SYNC_UPDATE` + varuint length + payload). The frontend `KnotProvider` decodes that frame; raw yrs bytes were being dropped before the framing was added in Plan 8.

## Why these choices

- **Yjs / yrs CRDT** — small wire protocol, mature ecosystem, free JS interop with the same data model on both sides.
- **Postgres LISTEN/NOTIFY over Redis** — one fewer datastore to operate. The notify payload is small (a Uuid + a sequence number) so Postgres handles the volume comfortably for a workspace tool.
- **mimalloc as global allocator** — small static binary, ARM-friendly, friendlier to musl than jemalloc.
- **Static musl + scratch** — operationally simplest container. No userland, no shell, no surprise base image CVEs.
- **Single binary serves SPA** — fewer moving parts in production. Add a separate frontend container later if CDN-edge caching becomes a need.
- **Dex over Keycloak in dev** — Dex is OIDC-only, smaller, easier to embed in compose.

## Where to read more

- **Long-form design spec:** `docs/superpowers/specs/2026-06-01-knot-foundation-design.md`
- **Plan outcome docs (what landed, what's deferred, what was non-obvious):** `docs/superpowers/research/`
- **SLOs and error budget:** `docs/SLO.md`
- **Deploy runbook:** `deploy/helm/knot/README.md`
- **Grafana dashboard:** `deploy/grafana/knot.json`
