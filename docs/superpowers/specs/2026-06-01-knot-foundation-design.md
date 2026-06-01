# knot — Foundation design (v0.1)

**Status:** Revised 2026-06-01 — Go→Rust pivot mid-spike (see §13 + memory `project-knot-go-pivot-2026-06-01`).
**Date:** 2026-06-01
**Author:** Niklas Voss (with Claude as brainstorming partner)
**Crate / repo:** `github.com/trevex/knot`

## 1. Overview

knot is a self-hostable FOSS knowledge database — a Confluence / Notion / Outline / Docmost alternative — implemented as a single static Rust binary serving a TypeScript SPA. Documents are real-time collaboratively edited, persisted as Yjs CRDTs via the `yrs` crate, and round-trip cleanly to Markdown. The Foundation spec covers the architecture, tech stack, data model, and the v0.1 walking-skeleton that delivers a usable internal-wiki experience end-to-end.

### Goals of v0.1

- Two users editing the same document see each other's changes live, with cursors and presence.
- A document tree (nested pages, drag-reorder) navigable in a sidebar.
- Local accounts (email + password) and OIDC (Dex in dev, anything that speaks OIDC in prod).
- Permission model: workspace roles (owner/editor/viewer) + per-doc grants with inheritance.
- Markdown import (paste or `.md` upload) and Markdown export (per-doc and bulk).
- Single static Rust binary (musl) embedding the SPA. Docker image (distroless) and Helm chart as wrappers.
- Horizontally scalable: multiple replicas sharing one Postgres primary.

### Explicit non-goals of v0.1

Listed in §16. The headline omissions are: comments, diagrams, search, attachments/images, multi-workspace UI, public links.

## 2. Stack decisions (canonical)

| Concern | Choice | Reason |
|---|---|---|
| Document source-of-truth | Yjs CRDT (binary blob in DB) | Real-time collab and precise comment anchors come naturally; Markdown is a round-trip format on top. Path Outline / Docmost / Affine took. |
| Editor | Tiptap (ProseMirror + `y-prosemirror`) | Largest extension ecosystem; production-proven for collaborative MD-friendly editors. |
| Backend language | **Rust** (stable, 2024 edition) | Picked over Go after the Foundation Spike showed the Go Yjs ecosystem is thin. `yrs` is the canonical non-JS Yjs implementation, maintained by the Yjs core team. |
| CRDT engine | `yrs` (`y-crdt/y-crdt`, Rust) | Native, production-grade Yjs with full ProseMirror `XmlFragment` support. No CGo, no sidecar. |
| Async runtime | `tokio` (multi-thread) | De-facto standard; the ecosystem above stands on it. |
| Backend HTTP | `axum` 0.7+ | Tower middleware story, type-safe extractors, mature WebSocket + state model. |
| WebSocket | `axum::extract::WebSocketUpgrade` (Tungstenite under the hood) | First-class in `axum`; no separate dep. |
| Database | PostgreSQL 16+ via `sqlx` (compile-time-checked queries) | One first-class target; FTS, JSONB, advisory locks. `sqlx::query!` catches schema drift at `cargo check`. |
| Cross-replica fan-out | Postgres `LISTEN`/`NOTIFY` via dedicated `tokio_postgres` listener (behind `crdt::Bus` trait) | Reuses the existing Postgres dependency; no Redis. Bus trait keeps NATS/Redis viable later. |
| Migrations | `sqlx migrate` (SQL up/down files in `migrations/`) | Native to the chosen DB driver; no extra tooling. |
| Auth | Local email/password (Argon2id via `argon2` crate) + OIDC (`openidconnect` crate). Server-side session rows + `HttpOnly` cookie. | Self-host-friendly without forcing an IdP; OIDC for teams that have one. Dex used in dev. |
| Object storage | Filesystem default; S3-compatible (MinIO) configurable via `object_store` crate. Behind `BlobStore` trait. | Self-host-first defaults. |
| Config | `figment` (defaults < file < env < flags) | Layered, typed, with `serde` derive. |
| Observability | `tracing` + `tracing-subscriber` + `tracing-opentelemetry` + `metrics` + `metrics-exporter-prometheus` | Native Rust observability stack. JSON logs via `tracing-subscriber`. OTLP exporter behind a flag. |
| Frontend stack | React 18 + Vite + TypeScript + Tailwind + shadcn/ui | Tiptap's React adapter is the most mature path; SPA only, no SSR. **Unchanged from pre-pivot.** |
| State management | TanStack Query (server state) + Zustand (UI state) | Document body is owned by Y.Doc, not React state. **Unchanged.** |
| Package manager (frontend) | pnpm | Aligned with the existing Nix flake. **Unchanged.** |
| API style | REST (JSON) + WebSocket (binary, y-protocol) | No GraphQL until something demands it. |
| Tenancy | Singleton workspace in v0.1; `workspace_id` column on workspace-scoped tables so multi-workspace is a future feature-flag flip, not a migration. | Keep scope tight without painting ourselves into a corner. |
| Deployment | Single static binary (Rust musl target) with embedded SPA → distroless container image → Helm chart | Self-host-friendly primitive (smaller image than Go's `CGO_ENABLED=0` build), k8s-friendly wrapper. |
| Allocator | `mimalloc` (global allocator on all non-MSVC targets) | musl's default mallocng has multi-threaded throughput cliffs that hurt the small-allocation churn of Yjs+WS workloads. mimalloc was picked over jemalloc: comparable throughput, much better ARM story (Apple Silicon + Graviton), smaller binary (+200 KB vs +1.5 MB), CMake build is Nix-friendly (jemalloc's autotools configure stumbles under Nix's gcc). |

## 3. Architecture overview

```
                              Browser (React + Vite SPA)
                              ├─ React Router
                              ├─ TanStack Query (REST state)
                              ├─ Zustand (UI state)
                              └─ Tiptap editor ──► Y.Doc ──► y-protocol WS client
                                  │
                          (HTTPS) │ (WSS)
                                  ▼
                  ┌───────────────────────────────────┐
                  │       knot (single Rust binary)   │
                  │                                   │
                  │   HTTP server (axum + tokio)      │
                  │   ├─ /api/*       REST            │
                  │   ├─ /auth/*      sessions, OIDC  │
                  │   ├─ /collab/:id  WS (y-sync v1)  │
                  │   └─ /*           embedded SPA    │
                  │                                   │
                  │   Workspace crates:               │
                  │   ├─ knot-auth      sessions+OIDC │
                  │   ├─ knot-workspace membership    │
                  │   ├─ knot-docs      tree + ACL    │
                  │   ├─ knot-crdt      yrs + rooms + │
                  │   │                 Bus + actor   │
                  │   ├─ knot-markdown  MD ↔ Y.Doc    │
                  │   ├─ knot-storage   DocStore etc. │
                  │   └─ knot-obs       tracing+OTEL  │
                  └────────────┬──────────────────────┘
                               │
                ┌──────────────┼──────────────┐
                ▼              ▼              ▼
           PostgreSQL     Filesystem/S3    Dex (dev only)
           (everything)   (blobs later)    (OIDC IdP)
```

### Component responsibilities

- **SPA frontend** owns presentation, the editor, and the Y.Doc client. Routes are client-side. Auth is a session cookie sent on every fetch and on the WS upgrade.
- **Rust binary** owns everything else: HTTP, WS, auth, ACLs, the CRDT engine, persistence, observability, static-asset serving (SPA is embedded via `rust-embed` at build time).
- **Postgres** is the only stateful dependency for v0.1. Stores users, sessions, workspaces, memberships, documents (tree + ACL + metadata), CRDT updates, snapshots, the Markdown cache, audit events. Doubles as the pub/sub bus via `LISTEN`/`NOTIFY`.
- **Filesystem / S3** is reserved for blobs later (attachments, snapshot exports). Wired through `BlobStore` trait from day 1, but only the filesystem implementation ships in v0.1.

## 4. Repo layout

A Cargo workspace with focused crates. Each crate has one clear responsibility and is testable independently. The single binary `knot-server` is the only `cdylib`/`bin` artifact; the rest are libraries.

```
knot/
├── Cargo.toml                 workspace manifest
├── Cargo.lock
├── crates/
│   ├── knot-server/           binary entrypoint
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs        wires axum + tokio + everything
│   │       ├── http.rs        router, middleware composition
│   │       ├── static_assets.rs   rust-embed for SPA
│   │       └── ws.rs          upgrade, auth, route to crdt::Room
│   ├── knot-core/             shared types: Workspace, User, Doc, Role, ...
│   ├── knot-auth/             sessions, Argon2id, OIDC (openidconnect)
│   ├── knot-workspace/        singleton-but-tenancy-aware
│   ├── knot-docs/             document tree, ACL resolver, invalidation outbox
│   ├── knot-crdt/             yrs adapter, room actor, Bus
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── engine.rs      Engine trait + Yrs-backed impl
│   │       ├── ysync.rs       y-sync v1 protocol encode/decode
│   │       ├── room.rs        per-doc actor task
│   │       ├── rooms.rs       room registry + lifecycle
│   │       ├── persist.rs     snapshot/GC orchestration
│   │       ├── bus.rs         Bus trait
│   │       └── bus_pg.rs      Postgres LISTEN/NOTIFY impl (v0.1)
│   ├── knot-markdown/         Y.Doc ⇄ Markdown serializer
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── schema.rs      generated from tools/schema.json
│   │   │   ├── from_markdown.rs
│   │   │   └── to_markdown.rs
│   │   └── tests/
│   │       └── fixtures/      *.md round-trip corpus
│   ├── knot-storage/          DocStore, SessionStore, BlobStore traits + sqlx impls
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── doc_store.rs
│   │   │   ├── session_store.rs
│   │   │   ├── blob_store.rs
│   │   │   └── postgres/
│   ├── knot-obs/              tracing layers, metrics registration
│   └── knot-version/           build-time version stamps
├── migrations/                sqlx migrations (.sql files)
├── tools/
│   ├── schema.json            canonical ProseMirror schema (single source of truth)
│   └── schemagen/             Rust binary: reads schema.json → emits schema.rs + schema.ts
│       ├── Cargo.toml
│       └── src/main.rs
├── web/                       Vite + React + TS app (unchanged from pre-pivot)
│   ├── src/
│   ├── public/
│   ├── package.json
│   └── vite.config.ts
├── deploy/
│   ├── docker/                Dockerfile (multi-stage; cargo-chef + musl)
│   ├── compose/               docker-compose.yml (knot+pg+dex)
│   └── helm/                  knot chart
├── e2e/                       Playwright suite
├── docs/superpowers/specs/    this file lives here
├── flake.nix
├── rust-toolchain.toml        pins Rust stable channel + components
├── .cargo/config.toml         cargo workspace defaults
└── Makefile                   common dev tasks
```

Frontend layout (`web/src/`):

```
web/src/
├── app/                routing, providers, root layout
├── features/           feature slices (auth, workspace, docs, editor)
│   ├── auth/
│   ├── workspace/
│   ├── docs/
│   └── editor/
│       ├── KnotEditor.tsx
│       ├── schema.ts           generated from canonical JSON
│       ├── extensions/
│       ├── collab/
│       │   ├── KnotProvider.ts y-protocol WS client
│       │   ├── awareness.ts
│       │   └── reconnect.ts
│       └── toolbar/
├── lib/                fetch wrapper, csrf, url helpers
├── ui/                 shadcn primitives
├── stores/             zustand stores (UI only)
├── styles/
└── main.tsx
```

Two organising principles:

- **Features are folders.** Each feature owns its API client, hooks, and components. No central `services/` or `hooks/` dumping grounds.
- **`ui/` is presentation only** — no business logic, no API calls.

## 5. Data model

PostgreSQL 16+. Schemas as DDL sketches; indexes and constraints captured where they matter for correctness.

### 5.1 Identity & tenancy

```sql
-- Singleton in v0.1 (workspace_id everywhere keeps us multi-tenant-ready)
CREATE TABLE workspaces (
  id           uuid PRIMARY KEY,
  slug         text UNIQUE NOT NULL,
  name         text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
  id              uuid PRIMARY KEY,
  email           citext UNIQUE NOT NULL,
  display_name    text NOT NULL,
  password_hash   text NULL,                -- NULL for OIDC-only users
  oidc_subject    text NULL,
  oidc_issuer     text NULL,
  created_at      timestamptz NOT NULL DEFAULT now(),
  UNIQUE (oidc_issuer, oidc_subject)
);

CREATE TABLE workspace_members (
  workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
  user_id      uuid REFERENCES users(id) ON DELETE CASCADE,
  role         text NOT NULL CHECK (role IN ('owner','editor','viewer')),
  added_at     timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (workspace_id, user_id)
);

CREATE TABLE sessions (
  id           bytea PRIMARY KEY,           -- 32 random bytes
  user_id      uuid REFERENCES users(id) ON DELETE CASCADE,
  workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
  created_at   timestamptz NOT NULL DEFAULT now(),
  expires_at   timestamptz NOT NULL,
  last_seen_at timestamptz NOT NULL DEFAULT now(),
  user_agent   text,
  ip           inet
);
CREATE INDEX ON sessions (expires_at);
```

### 5.2 Document tree

Adjacency list with LexoRank-style ordering string. Recursive CTE for tree fetches; trees stay shallow in practice. Move = update `parent_id` + `sort_key`; insert between siblings picks a sort_key between neighbours. No renumbering needed.

```sql
CREATE TABLE documents (
  id            uuid PRIMARY KEY,
  workspace_id  uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  parent_id     uuid NULL REFERENCES documents(id) ON DELETE CASCADE,
  title         text NOT NULL DEFAULT 'Untitled',
  sort_key      text NOT NULL,              -- 'm', 'mh', 'mhc', ...
  icon          text NULL,
  created_by    uuid NOT NULL REFERENCES users(id),
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  archived_at   timestamptz NULL,
  UNIQUE (workspace_id, parent_id, sort_key)
);
CREATE INDEX ON documents (workspace_id, parent_id, sort_key);
CREATE INDEX ON documents (workspace_id) WHERE archived_at IS NULL;
```

### 5.3 ACL inheritance

Effective permission resolved by walking parents until an explicit grant is found, falling back to the workspace role.

```sql
CREATE TABLE document_grants (
  doc_id       uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  principal    text NOT NULL,        -- 'user:<uuid>' or 'group:<oidc-group>'
  role         text NOT NULL CHECK (role IN ('viewer','editor','owner')),
  inherit      boolean NOT NULL DEFAULT true,
  granted_at   timestamptz NOT NULL DEFAULT now(),
  granted_by   uuid REFERENCES users(id),
  PRIMARY KEY (doc_id, principal)
);
```

ACL resolver in `crates/knot-docs/src/acl.rs` caches `(doc_id, user_id) → role` in-process. Invalidations come from the same transactions that mutate `documents` or `document_grants` via a post-commit outbox (§7.5). 5-minute TTL as a belt-and-suspenders defence.

### 5.4 CRDT storage

```sql
CREATE TABLE doc_updates (
  seq          bigserial PRIMARY KEY,        -- global; sparse per doc
  doc_id       uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  update_bytes bytea NOT NULL,               -- Yjs binary update
  by_user_id   uuid NULL REFERENCES users(id),
  created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON doc_updates (doc_id, seq);

CREATE TABLE doc_snapshots (
  doc_id          uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  snapshot_seq    bigint NOT NULL,            -- global seq through which
                                              -- this snapshot integrates
  state_bytes     bytea NOT NULL,             -- Yjs encoded state
  state_vector    bytea NOT NULL,             -- for diff fetches
  created_at      timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (doc_id, snapshot_seq)
);
```

`seq` is a **global** `bigserial` (not per-doc). Per-doc monotonicity (the property we need) is preserved because Postgres serialises sequence allocation. Concurrent INSERTs from different replicas get distinct seq values; replays per doc use `WHERE doc_id=$1 ORDER BY seq`. Per-doc seq is sparse but monotonic, which is all CRDT replay requires.

**Hydration path** when a room boots on any replica:

1. Load latest snapshot for `doc_id`.
2. `engine.ApplyUpdate(doc, snapshot.state_bytes)`.
3. `SELECT update_bytes FROM doc_updates WHERE doc_id=$1 AND seq > $snapshot_seq ORDER BY seq`.
4. Apply each. Record `lastAppliedSeq = max(seq)`.

**Snapshot / GC strategy** (per room actor; not a separate worker):

- After every `KNOT_SNAPSHOT_EVERY_N` updates (default 200) or `KNOT_SNAPSHOT_IDLE_SEC` seconds of idle (default 30), the room writes a new snapshot row.
- After writing snapshot S, delete `doc_updates WHERE doc_id=? AND seq <= S - retention_K` where `retention_K = 2 * KNOT_SNAPSHOT_EVERY_N`. Keeps the previous snapshot's range of raw updates so a corrupt snapshot can be rolled back.
- Old snapshots: keep last 5; keep one per day for 30 days. Background `tokio::task` (`crates/knot-crdt/src/persist.rs`) runs hourly.

### 5.5 Markdown cache

```sql
CREATE TABLE doc_markdown_cache (
  doc_id          uuid PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
  rendered_at_seq bigint NOT NULL,
  markdown_text   text NOT NULL,
  updated_at      timestamptz NOT NULL DEFAULT now()
);
```

Lazily filled by export endpoint. On request: if `rendered_at_seq == lastAppliedSeq` for the doc, return cached text; else re-render and update. Avoids re-serialising on every export.

### 5.6 Audit / activity (skeleton)

```sql
CREATE TABLE audit_events (
  id           bigserial PRIMARY KEY,
  workspace_id uuid NOT NULL,
  actor_id     uuid NULL,
  action       text NOT NULL,    -- 'doc.create','doc.move','doc.grant', ...
  target_kind  text NOT NULL,
  target_id    uuid NOT NULL,
  data         jsonb NOT NULL DEFAULT '{}',
  created_at   timestamptz NOT NULL DEFAULT now()
);
```

Writes are best-effort: failure logs a warning, does not fail the user action. No UI in v0.1.

### 5.7 ACL invalidation outbox

```sql
CREATE TABLE acl_invalidations (
  id           bigserial PRIMARY KEY,
  workspace_id uuid NOT NULL,
  doc_id       uuid NOT NULL,
  reason       text NOT NULL,        -- 'tree-move','grant-change','grant-delete'
  created_at   timestamptz NOT NULL DEFAULT now()
);
```

Written in the same transaction as the mutation. A per-replica listener reads + emits `NOTIFY acl_invalidate` and deletes the row. ACL cache subscribes and evicts affected entries.

## 6. API surface

All JSON, all under `/api` except auth which lives at `/auth`. WS at `/collab`. Embedded SPA at `/`.

### 6.1 HTTP endpoints

```
# Health & meta
GET    /api/healthz                 → 200 always (liveness)
GET    /api/readyz                  → 200 when DB reachable + CRDT engine ready
GET    /api/version                 → build info

# Auth
POST   /auth/login                  body: {email, password} → sets sid cookie
POST   /auth/logout                 deletes sid cookie + session row
GET    /auth/session                → current user, workspace, role  (or 401)
GET    /auth/oidc/login             → 302 to IdP authorize endpoint
GET    /auth/oidc/callback          handles code → sets sid cookie → 302 /
POST   /auth/setup                  first-run: creates the first admin user
                                    (gated; returns 410 once any user exists)

# Workspace (singleton in v0.1, but addressable)
GET    /api/workspace               → workspace + your membership
GET    /api/workspace/members
POST   /api/workspace/members       invite by email (or pre-provision)
PATCH  /api/workspace/members/:id   change role
DELETE /api/workspace/members/:id

# Documents (tree)
GET    /api/docs                    → flat list with id, title, parent_id, sort_key
                                      (client builds tree; cheap for <10k docs)
POST   /api/docs                    body: {title?, parent_id?, after_id?}
GET    /api/docs/:id                → doc metadata + effective_role for caller
PATCH  /api/docs/:id                rename, set icon
POST   /api/docs/:id/move           body: {parent_id?, after_id?, before_id?}
DELETE /api/docs/:id                soft-delete (sets archived_at)
POST   /api/docs/:id/restore

# Documents (permissions)
GET    /api/docs/:id/grants
PUT    /api/docs/:id/grants/:principal   body: {role, inherit}
DELETE /api/docs/:id/grants/:principal

# Documents (content I/O)
GET    /api/docs/:id/markdown       → text/markdown
POST   /api/docs/:id/markdown       Content-Type: text/markdown → imports
```

### 6.2 WebSocket

```
GET    /collab/:doc_id              upgrades to WSS, speaks y-sync v1
```

Auth happens at upgrade. After upgrade, every frame is implicitly authenticated to one `(user, doc, role)` tuple pinned at upgrade time.

### 6.3 Error envelope

All non-2xx JSON responses are:

```json
{ "error": { "code": "doc.not_found", "message": "...", "details": {} } }
```

Codes are stable strings the client switches on; messages are for humans.

## 7. Auth & authorization

### 7.1 Local credentials

- Argon2id with reasonable parameters (m=64 MiB, t=3, p=1; revisit on cost-benchmark in the spike phase).
- Login throttling: per-IP and per-email leaky bucket. 5 failures in 5 min → generic "invalid credentials" + 1s delay. Accounts never lock (DoS prevention).

### 7.2 OIDC

`openidconnect` crate (Ramos Bugs / Sebastian Imlay's `openidconnect-rs`). Standard PKCE-protected authorisation-code flow. Configuration via env (see §11.3). Dex used as the dev IdP, statically configured with seeded users in `deploy/compose/dex/`.

Auto-provisioning policies: `off | always | domain | group`. Group-based role mapping (`KNOT_OIDC_ROLE_FROM_GROUPS`) writes the user's workspace role on first login.

### 7.3 Sessions

Server-side rows keyed by 32-byte random ID, stored in `sid` cookie:

```
Set-Cookie: sid=<urlsafe-base64>; HttpOnly; Secure; SameSite=Lax; Path=/
```

Genuinely revocable (delete the row). No JWT, no rotation footguns.

### 7.4 Middleware chain (axum + tower)

```
RequestId → CatchPanic → TraceLayer(tracing) → OTEL span → Compression
  → SessionLoader  (parse sid cookie → user, workspace, role)
  → Csrf            (double-submit cookie on /api unsafe methods)
  → Route-specific: RequireSession, RequireDocRole(min)
```

For `/collab/:doc_id`:

```
RequestId → CatchPanic → SessionLoader → RequireSession → RequireDocRole
  → axum WebSocketUpgrade  (passes user + doc role into the CRDT room)
```

Each middleware is a `tower::Layer` so the stack is reorderable / composable.

### 7.5 Permission enforcement

REST and WS share one resolver: `acl::resolve(ctx, doc_id, user_id) -> EffectiveRole`. Resolver walks parents until an explicit grant matches (`document_grants.inherit = true` propagates to descendants), falling back to workspace role.

Cache: per-process `moka` cache keyed on `(doc_id, user_id)`, max 60 s TTL. Invalidation: the `acl_invalidations` outbox (§5.7) is published as a `NOTIFY acl_invalidate` event after commit; per-replica listeners walk the affected subtree and evict cache entries.

### 7.6 Permission revocation mid-session

When ACL changes affect an active WS connection (e.g. grant removed during edit), the room emits a close frame with code 4403 to affected sockets. The client reconnects; the upgrade returns 403; the editor surfaces a "you no longer have access" message.

### 7.7 First-run bootstrap

`/auth/setup` accepts `{email, password, display_name}` only when no users exist. Closes after the first user is created. CLI alternative: `knot-server admin create --email --password-stdin` for headless installs (the same binary serves both modes via a `clap` subcommand router).

## 8. CRDT sync server

### 8.1 Protocol

[y-protocol v1](https://github.com/yjs/y-protocols/blob/master/sync.js) over binary WebSocket frames. Message envelope: length-prefixed `messageType` byte + payload.

```
messageSync (0)
  subtype 0  SyncStep1    client sends its state vector; server replies
                          with our missing updates as SyncStep2
  subtype 1  SyncStep2    peer sends us missing updates; we apply
  subtype 2  Update       incremental update; apply + broadcast

messageAwareness (1)      presence (cursors, names, colours)
                          ephemeral; not persisted; fan-out only

messageQueryAwareness (3) reply with current awareness state
```

### 8.2 Engine trait — the swap-out boundary

The CRDT engine is a thin adapter over `yrs`. Every other crate that needs to mutate or read a Y.Doc goes through the `Engine` trait so we can mock it in tests and swap implementations if `yrs` ever evolves incompatibly.

```rust
// crates/knot-crdt/src/engine.rs
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("yrs apply update: {0}")]
    Apply(String),
    #[error("yrs encode: {0}")]
    Encode(String),
    #[error("markdown serialise: {0}")]
    Markdown(String),
}

/// A handle to a CRDT document. Opaque so only the implementation
/// interprets the inner type. In v0.1 this is `yrs::Doc`.
pub struct DocHandle(pub(crate) yrs::Doc);

pub trait Engine: Send + Sync + 'static {
    fn new_doc(&self) -> DocHandle;

    fn apply_update(&self, d: &DocHandle, update: &[u8]) -> Result<(), EngineError>;

    /// Encodes the part of the state missing from a peer with the
    /// given encoded state vector. If `peer_sv` is None, encodes the
    /// full state.
    fn encode_state_as_update(&self, d: &DocHandle, peer_sv: Option<&[u8]>) -> Result<Vec<u8>, EngineError>;

    fn encode_state_vector(&self, d: &DocHandle) -> Result<Vec<u8>, EngineError>;

    // Markdown round-trip against the canonical ProseMirror schema.
    fn to_markdown(&self, d: &DocHandle) -> Result<String, EngineError>;
    fn from_markdown(&self, md: &str) -> Result<(DocHandle, Vec<u8>), EngineError>;
}
```

The persistence layer speaks `&[u8]`; rooms speak `&DocHandle`; the rest of knot speaks Markdown or ProseMirror JSON. No other crate reaches across the boundary. The `yrs` API is internal to `knot-crdt`.

### 8.3 Room actor (one per active doc, per replica)

A `Room` is a `tokio` task that exclusively owns one `DocHandle`. All mutations go through its `mpsc::Receiver` inputs. No `Mutex` on the doc itself; the actor pattern is what gives us Send/Sync-safe access to a non-`Sync` `yrs::Doc`.

```rust
// crates/knot-crdt/src/room.rs
pub struct Room {
    pub doc_id: Uuid,
    doc: DocHandle,
    last_applied_seq: i64,         // watermark for cross-replica catch-up

    conns: HashMap<ConnId, ConnHandle>,
    in_rx: mpsc::Receiver<InMsg>,
    notify_rx: mpsc::Receiver<i64>,      // bus-delivered "new seq available"
    presence_rx: mpsc::Receiver<Vec<u8>>,// bus-delivered presence frames
    leave_rx: mpsc::Receiver<ConnId>,
    shutdown: CancellationToken,
}

pub struct InMsg {
    pub from: ConnId,
    pub bytes: Vec<u8>,
}
```

`Room::run` is one `tokio::select!` loop that's the only task touching `doc`, `conns`, or `last_applied_seq`. Per inbound client frame:

```
decode → match msg.kind {
  SyncStep1: encode SyncStep2 from our state vs their SV → reply
  SyncStep2: engine.apply_update → persist (INSERT ... RETURNING seq)
           → Bus.publish(doc_id, seq) → fan out as Update to other conns
  Update:    same as SyncStep2
  Awareness: Bus.publish_presence(doc_id, payload); fan out locally
}
```

Per bus delivery:

```
notify_rx: if seq <= last_applied_seq → skip
           SELECT update_bytes FROM doc_updates
             WHERE doc_id = $1 AND seq > last_applied_seq ORDER BY seq
           for each → engine.apply_update; fan out to local conns
           last_applied_seq = max(seq)
presence_rx: validate (size cap) → fan out to local conns
```

Why an actor, not a `Mutex<yrs::Doc>`: `yrs::Doc` is `Send` but the natural API for it involves long-lived transactions. Serialising through one task makes write ordering obvious and removes any worry about lock contention. The cost is one `tokio` task per active doc (small task stack), idle when no traffic flows.

### 8.4 Persistence orchestration

Inserts are batched by a writer task sibling to the room:

```
Room task          ── send(update_bytes) ──►   ┌─────────────────────┐
                                               │ writer task         │
                                               │ flush every         │
                                               │   200 updates OR    │
                                               │   250 ms            │
                                               │ INSERT batch into   │
                                               │ doc_updates         │
                                               │ RETURNING seq[]     │
                                               └────────┬────────────┘
                                                        │
                                                        ▼
                                                     Postgres
                                                        │
                                                        ▼
                                            for each seq:
                                              Bus::publish(doc_id, seq)
                                              Room::broadcast(update_bytes)
```

Snapshots run on a `tokio::time::interval` inside the room actor (so they observe `doc` consistently). GC of old snapshots runs hourly in a separate `tokio::task`.

### 8.5 Backpressure

- **WS inbound:** bounded `mpsc` (default capacity 256). When full, `send` returns `Err` and the connection's read task pauses; TCP backpressures the client. No silent drops.
- **WS outbound (per connection):** per-conn outgoing `mpsc` (default 256). When full, the connection is closed with code 4408 (`slow consumer`). A stuck client must not starve other editors.
- **Persist channel:** bounded (default 1024). When full, the room actor `.await`s on send before assigning seqs to new updates; this slows *this doc's* edit throughput but bounds memory.

### 8.6 Room lifecycle & eviction

```
join arrives ──► Rooms::acquire(doc_id)
                 (in-flight dedup via DashMap entry guard)
                 │
       ┌─────────┴─────────┐
       │ not present?      │ present?
       ▼                   ▼
  load latest snapshot   return existing room handle
  replay updates after it
  spawn room tokio::task
  Bus::subscribe(doc_id)
  attach connection

last conn leaves ──► idle timer (KNOT_ROOM_IDLE_EVICT_SEC, default 300)
                     │
                     ▼ timer fires
                     flush remaining updates
                     write final snapshot
                     Bus::unsubscribe(doc_id)
                     remove from registry, drop shutdown token, task exits
```

### 8.7 Awareness (presence)

Held in memory by each room. Each connection has awareness keyed by its Yjs `clientID`. Frames are size-capped (4 KB), validated, then fanned out to local peers *and* across the Bus via the presence channel. On disconnect, the room emits a clearing frame for the departed clientID so cursors disappear on other clients. Cursor positions use Yjs relative positions; they survive concurrent edits with no server logic.

### 8.8 Markdown round-trip

Two paths:

1. **Client-driven** (hot path): the editor has ProseMirror nodes; it serialises locally and POSTs to update the cache. Used whenever a client is online to do it.
2. **Server-driven** (cold path): used for bulk export, restore-from-MD, headless installs, and anytime no client is online. `Engine::to_markdown` walks the Y.Doc `XmlFragment` per the canonical schema. `Engine::from_markdown` does the inverse using a Markdown parser (`pulldown-cmark` or `markdown` crate — TBD in implementation, both are pure-Rust CommonMark).

The serializer crate (`knot-markdown`) is testable in isolation against a fixture corpus (Y.Doc binary + expected Markdown). Fixtures double as round-trip regression tests: load → `to_markdown` → `from_markdown` → `to_markdown` must produce byte-identical output.

The canonical ProseMirror schema lives in one JSON file (`tools/schema.json`). Both `web/src/features/editor/schema.ts` and `crates/knot-markdown/src/schema.rs` are generated from it by `tools/schemagen` (also a Rust binary). A pre-commit hook reruns the generator.

v0.1 schema elements (lossless MD round-trip):

```
paragraph, heading (1-6), code_block, blockquote,
bullet_list, ordered_list, list_item,
horizontal_rule, hard_break,
text marks: bold, italic, code (inline), strike, underline, link
```

Advanced blocks (tables, callouts, embeds, mentions, diagrams) are deferred; each subsequent spec defines its own MD serialization rule.

## 9. Pub/sub Bus

The cross-replica fan-out is hidden behind a trait so v0.1 ships on Postgres `LISTEN`/`NOTIFY` while keeping NATS / Redis viable replacements.

```rust
// crates/knot-crdt/src/bus.rs
use async_trait::async_trait;
use tokio::sync::mpsc;

#[async_trait]
pub trait Bus: Send + Sync + 'static {
    async fn publish(&self, doc_id: Uuid, seq: i64) -> Result<(), BusError>;
    async fn publish_presence(&self, doc_id: Uuid, payload: Vec<u8>) -> Result<(), BusError>;
    async fn subscribe(&self, doc_id: Uuid) -> Result<Subscription, BusError>;
    async fn unsubscribe(&self, doc_id: Uuid) -> Result<(), BusError>;
}

pub struct Subscription {
    pub updates: mpsc::Receiver<i64>,        // new seq available
    pub presence: mpsc::Receiver<Vec<u8>>,   // presence payload
}
```

### 9.1 Postgres impl (`bus_pg.rs`)

Per-replica `Listener` owns ONE dedicated `tokio_postgres` connection running `LISTEN doc:<id>` and `LISTEN presence:<id>` for every doc this replica has rooms for. Demultiplexes received `Notification`s onto the right room's `mpsc::Sender` by `doc_id`. We use raw `tokio_postgres` instead of `sqlx` for this single connection because `sqlx`'s pool semantics don't compose well with long-lived per-channel listening — same pattern Outline/Hocuspocus use with `pg-listen`.

**Update fan-out** carries only `{seq}` — never bytes. Receivers `SELECT update_bytes FROM doc_updates WHERE doc_id = $1 AND seq > $last_applied_seq`. Bytes never travel through NOTIFY → no size cliff (Postgres caps payload at ~8 KB), no branching code, durability before broadcast.

**Presence fan-out** carries the payload inline (size-capped at 4 KB on emit). Best-effort; if dropped, the next ~200 ms emit fixes it.

**Catch-up safety net:** every room polls `doc_updates WHERE seq > last_applied_seq` on a slow tick (default 5 s) so missed NOTIFYs (network blip, Postgres restart) heal automatically. CRDT idempotency makes duplicate apply harmless. The watermark prevents double-fan-out to clients.

### 9.2 Load envelope

For a knowledge-base workload, Postgres handles this comfortably:

- A doc with 10 simultaneous heavy typists sustains ~20–50 ops/sec.
- A workspace with 50 hot docs at that pace: ~2500 ops/sec.
- Each op: one INSERT + one NOTIFY in the originating replica; one SELECT per other replica with the doc open.
- Postgres handles tens of thousands of small INSERTs/sec on a single primary; NOTIFY is similar order of magnitude.

`knot_collab_notify_lag_seconds` metric surfaces pressure before it becomes a fire. When this becomes a real problem (probably never for this product), swap `bus_pg.rs` for `bus_nats.rs` without touching room code.

## 10. Frontend

### 10.1 Routing (React Router v6)

```
/                              redirect to first doc or onboarding
/login                         LoginPage (local) + OIDC button
/auth/oidc/callback            handled by backend; SPA only sees /
/setup                         first-run admin creation
/doc/:id                       DocPage  (editor)
/doc/:id/permissions           dialog mounted over DocPage
/members                       MembersPage
/settings                      SettingsPage
*                              NotFound
```

Auth gate is a route loader: hits `/auth/session`, redirects to `/login` on 401. No client-side token state — the cookie is the truth; the SPA asks the server "who am I".

### 10.2 Data flow — three sources, clear separation

```
   ┌──────────────────────────────────────────────────────────┐
   │              TanStack Query  (server state)              │
   │  session, doc list, doc metadata, members, grants,       │
   │  cached MD export                                        │
   └──────────────────────────────────────────────────────────┘

   ┌──────────────────────────────────────────────────────────┐
   │            Y.Doc + KnotProvider  (doc body)              │
   │  Tiptap binds directly; no copy in React state.          │
   │  Edits flow Y.Doc → WS → server → other clients.         │
   │  React state observes Y.Doc only for affordances         │
   │  (word count, dirty indicator) via cheap selectors.      │
   └──────────────────────────────────────────────────────────┘

   ┌──────────────────────────────────────────────────────────┐
   │                 Zustand  (UI state)                      │
   │  sidebar open/closed, command palette, theme, modals     │
   │  Never holds anything the server cares about.            │
   └──────────────────────────────────────────────────────────┘
```

**Anti-pattern we explicitly avoid:** mirroring the doc body into React state. Tiptap manages its own view; we observe events for badges and toolbars, never store the body.

### 10.3 Tiptap setup

Extensions in three groups:

```ts
const extensions = [
  // base schema — must match server schema.rs byte-for-byte
  Document, Paragraph, Text, Heading, Bold, Italic, Code, Link,
  BulletList, OrderedList, ListItem, Blockquote, CodeBlock,
  HardBreak, HorizontalRule, Strike, Underline,

  // collab (mandatory)
  Collaboration.configure({ document: ydoc }),
  CollaborationCursor.configure({ provider, user }),

  // UX niceties
  Placeholder, Typography, Dropcursor, Gapcursor,
  History.configure({ history: false }),  // Yjs UndoManager owns undo
]
```

`History` is disabled in favour of `Y.UndoManager` because ProseMirror's history doesn't know about remote edits.

### 10.4 KnotProvider — the WS client

Thin wrapper around `y-websocket` (or our own implementation if dependency hygiene wins; the protocol is ~300 lines):

- Open `WSS /collab/:doc_id`, send SyncStep1 with our state vector, receive missing updates.
- Forward incoming binary frames to Y.Doc / awareness.
- Forward Y.Doc updates and awareness emits as binary frames.
- Reconnect with exponential backoff + jitter; on reconnect, resume from current state vector.
- Surface lifecycle: `connecting | connected | offline | unauthorised | conflict`. Toolbar shows a status dot.

### 10.5 API client (`lib/api.ts`)

One fetch wrapper for the app:

- `credentials: 'include'` on every request.
- Read CSRF token from cookie, inject `X-CSRF-Token` on unsafe methods.
- Parse error envelopes into typed `ApiError`.
- Return typed JSON via lightweight runtime validation (hand-written guards or `valibot`).

Per-feature `api.ts` wraps this with typed helpers (`docsApi.list()`, `docsApi.move({id, parentId, afterId})`).

### 10.6 Quality bar

- TS strict mode, `noUncheckedIndexedAccess`, `noImplicitOverride`.
- ESLint with `eslint-plugin-react-hooks`, `eslint-plugin-import`, `@typescript-eslint` recommended-type-checked.
- Bundle budget: 250 KB gzipped main chunk. Editor lazy-loaded on `/doc/:id`.
- Permission-aware UI hides destructive controls per `effective_role`, but the server re-checks on every call — UI permission is a UX nicety, never a security boundary.

## 11. Dev environment, build, deployment

### 11.1 Dev environment (`nix develop`)

The flake (already present) provides Node + pnpm, Chromium, kubectl/kind/helm. Additions for the Rust pivot (replacing prior Go entries):

```
rustup OR fenix overlay      pin via rust-toolchain.toml (stable channel)
                             components: rustfmt, clippy, rust-analyzer, rust-src
                             targets: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl

cargo-watch                  cargo watch -x check / -x run (hot rebuild)
cargo-nextest                fast test runner with better output
cargo-deny                   licence + advisory scanning in CI
sqlx-cli                     sqlx migrate add / run / revert
sccache (optional)           build-time caching for the workspace
mold (or lld)                fast linker, big win on rebuild times

postgresql_16                pg_ctl for local DB option
dex                          OIDC IdP, runnable directly
playwright-cli               for e2e
pre-commit                   hook runner
```

`direnv` already wires this on `cd`.

Postgres + Dex for day-to-day dev run via `docker compose -f deploy/compose/dev.yml up -d`. Tests use ephemeral Postgres via the `testcontainers` crate so they don't depend on the compose stack being up.

### 11.2 Makefile (the only entrypoint contributors learn)

```
make help              discoverable list
make dev               runs vite + cargo watch + dex + postgres in parallel
make build             full single-binary build (pnpm build → cargo build --release)
make test              cargo nextest + vitest
make e2e               playwright against `make dev`
make lint              cargo clippy + cargo fmt --check + eslint + prettier --check
make fmt               cargo fmt + prettier --write
make migrate.up/.down  sqlx migrate run / revert
make schema.gen        regenerate Rust + TS schema from canonical JSON
make compose.up/.down  dev compose stack
make compose.logs
make kind.up           kind cluster + helm install
make clean
```

### 11.3 Configuration

Layered: defaults < `/etc/knot/config.yaml` < env vars < CLI flags. Implementation: `figment` (`serde`-based, supports yaml/env/CLI providers natively).

```
KNOT_ADDR                 :3000
KNOT_BASE_URL             https://knot.example.com
KNOT_DATABASE_URL         postgres://...
KNOT_SESSION_KEY          HMAC for CSRF tokens; 32+ random bytes
KNOT_DATA_DIR             /var/lib/knot                  for blob fs
KNOT_OIDC_ENABLED         false
KNOT_OIDC_ISSUER          http://dex:5556/dex
KNOT_OIDC_CLIENT_ID       knot
KNOT_OIDC_CLIENT_SECRET   <secret>
KNOT_OIDC_REDIRECT_URL    https://knot.example.com/auth/oidc/callback
KNOT_OIDC_AUTO_PROVISION  off | always | domain | group
KNOT_OIDC_ALLOWED_DOMAINS example.com,another.com
KNOT_OIDC_ROLE_FROM_GROUPS {"knot-admin":"owner","knot-edit":"editor"}
KNOT_LOG_LEVEL            info
KNOT_LOG_FORMAT           json | text
KNOT_METRICS_ADDR         :9090
KNOT_TRACING_ENABLED      false
KNOT_OTLP_ENDPOINT        when enabled
KNOT_PPROF_ENABLED        false
KNOT_SNAPSHOT_EVERY_N     200
KNOT_SNAPSHOT_IDLE_SEC    30
KNOT_ROOM_IDLE_EVICT_SEC  300
KNOT_ENV                  development | production
```

Empty `KNOT_SESSION_KEY` at startup: generate-and-warn in dev, refuse-to-start in prod.

### 11.4 Build pipeline (multi-stage Dockerfile with `cargo-chef` + musl)

```dockerfile
# ---- Frontend ----
FROM node:22-slim AS web
WORKDIR /src/web
COPY web/package.json web/pnpm-lock.yaml ./
RUN corepack enable && pnpm install --frozen-lockfile
COPY web/ .
RUN pnpm build       # → web/dist

# ---- Cargo dependency cache (cargo-chef) ----
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN rustup target add x86_64-unknown-linux-musl && \
    apt-get update && apt-get install -y --no-install-recommends musl-tools && \
    rm -rf /var/lib/apt/lists/*
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json
COPY . .
COPY --from=web /src/web/dist ./web/dist
ARG VERSION=dev
ARG COMMIT=unknown
ENV KNOT_VERSION=$VERSION KNOT_COMMIT=$COMMIT
RUN cargo build --release --target x86_64-unknown-linux-musl --bin knot-server && \
    cp target/x86_64-unknown-linux-musl/release/knot-server /out-knot-server

# ---- Runtime ----
FROM gcr.io/distroless/static-debian12:nonroot
USER nonroot:nonroot
COPY --from=builder /out-knot-server /knot
ENTRYPOINT ["/knot"]
```

Image target: ~20–35 MB (musl-static Rust binary, no libc dependency, no shell, no package manager). `cargo-chef` keeps dependency rebuilds out of the hot path so iterative source changes don't trigger a workspace-wide recompile.

### 11.5 CI (GitHub Actions)

Per-PR job DAG:

```
fmt-and-lint  (cargo fmt --check + cargo clippy -D warnings + eslint + prettier --check)
   │
   ├─► unit-rust      cargo nextest run --workspace, testcontainers ephemeral pg
   ├─► unit-web       vitest
   ├─► build          multi-arch image (linux/amd64 + linux/arm64 via cross + musl)
   │      │           push to ghcr.io with PR tag
   │      └─► e2e     playwright against the just-built image,
   │                  docker compose: knot + pg + dex; seeded fixtures
   ├─► helm-lint      helm lint + chart-testing
   └─► deny           cargo deny check (licences + advisories)
```

Release on tag: build multi-arch image to `ghcr.io/trevex/knot:vX.Y.Z`, publish Helm chart to OCI registry, attach binary artifacts (musl-static linux-amd64 / linux-arm64 + macos universal) to GitHub Release.

### 11.6 Deployment artifacts

Three shapes, descending in opinion:

1. **Docker image** (`ghcr.io/trevex/knot`) — primary supported artifact.
2. **Helm chart** (`deploy/helm/knot`) — k8s adopters. Values cover replicas, ingress, postgres connection, OIDC, secrets via existing-secret-ref, resources, persistence.
3. **Static binary** (`knot-server_linux_amd64_musl`, `_arm64_musl`, `_darwin_*`) — hobbyist VM with systemd. Ships an example `knot.service`.

`replicas` defaults to 1 in the chart; **horizontal scaling supported** as long as replicas share one Postgres primary (§9).

### 11.7 Observability

**Logs:** `tracing` + `tracing-subscriber`. JSON layer (`tracing-subscriber::fmt::Json`) or pretty per `KNOT_LOG_FORMAT`. Structured fields via `info!(user_id, doc_id, ...)` and `#[instrument]`. Per-request `request_id` + `user_id` + `doc_id` (when applicable) attached as span fields propagated via `tower-http::trace`. PII redaction by field-name list (passwords, tokens, raw cookies, raw OIDC ID tokens never logged) enforced by a custom `FieldFilter` layer.

**Metrics:** Prometheus on a separate port (`KNOT_METRICS_ADDR`) so unauth scrape doesn't traverse auth middleware:

```
knot_http_requests_total{method,route,code}
knot_http_request_duration_seconds_bucket{method,route}
knot_ws_connections_open{role}
knot_ws_messages_total{direction,kind}
knot_collab_room_count
knot_collab_room_hydration_seconds_bucket
knot_collab_persist_seconds_bucket
knot_collab_persist_batch_size_bucket
knot_collab_notify_lag_seconds_bucket
knot_db_query_duration_seconds_bucket{op}
knot_db_pool_in_use / _idle / _max
knot_auth_logins_total{result,kind}
knot_acl_cache_hits_total / _misses_total
knot_build_info{version,commit}
```

**Traces:** OpenTelemetry via `tracing-opentelemetry`, OTLP exporter; off by default. Spans for HTTP (tower-http TraceLayer), WS upgrade, room run iterations (`#[instrument]`), DB queries (`sqlx` integrates with `tracing` natively), OIDC verify. W3C `traceparent` propagation via `opentelemetry-http`.

**Profiling:** `KNOT_PPROF_ENABLED` (default off), exposed on the metrics port via `pprof-rs`. Produces standard pprof-compatible CPU profiles.

## 12. Testing

### 12.1 Shape

```
                    ┌─────────────────────────┐
                    │ Playwright e2e          │  ~15 happy-path + edge
                    │ (real browser, server,  │  flows, ~60 s each
                    │  Postgres)              │  CI gate on PRs
                    └─────────────────────────┘
              ┌──────────────────────────────────────┐
              │ Integration tests (Rust + TS)        │  many
              │ - Rust: testcontainers Postgres      │
              │ - TS: Vitest + RTL with mocked fetch │
              └──────────────────────────────────────┘
        ┌────────────────────────────────────────────────────┐
        │ Unit tests                                         │  most
        │ ACL resolution, sort_key picker, MD round-trip,    │
        │ tree-builder, csrf helpers, fetch wrapper          │
        └────────────────────────────────────────────────────┘
                       │                              │
                       ▼                              ▼
                ┌──────────────┐              ┌──────────────┐
                │ Property /   │              │ Type checks  │
                │ fuzz tests   │              │ tsc + golangci│
                │ (CRDT only)  │              │ (every CI)   │
                └──────────────┘              └──────────────┘
```

### 12.2 Playwright e2e (the headline)

Lives in `e2e/`. Runs against `make dev` locally and the freshly-built image in CI (compose: knot + Postgres + Dex + a tiny mock SMTP).

```
e2e/
├── playwright.config.ts
├── fixtures/         db.ts, users.ts, auth.ts, tree.ts
├── flows/
│   ├── auth/
│   │   ├── local-login.spec.ts
│   │   ├── oidc-login-dex.spec.ts
│   │   ├── first-run-setup.spec.ts
│   │   └── logout-and-session-revoke.spec.ts
│   ├── docs/
│   │   ├── create-rename-move-delete.spec.ts
│   │   ├── tree-drag-reorder.spec.ts
│   │   ├── markdown-export.spec.ts
│   │   └── markdown-import.spec.ts
│   ├── collab/
│   │   ├── two-users-converge.spec.ts          ← headline test
│   │   ├── presence-cursors.spec.ts
│   │   ├── offline-then-reconnect.spec.ts
│   │   └── permission-revoked-mid-edit.spec.ts
│   └── permissions/
│       ├── viewer-cannot-edit.spec.ts
│       ├── editor-can-share.spec.ts
│       └── grant-inheritance.spec.ts
└── support/
    ├── pages/        page-object models
    └── selectors.ts  data-testid constants
```

Stability conventions:

- `data-testid` selectors only.
- Page-object models per surface (Sidebar, Editor, PermissionsDialog).
- `expect.poll` / web-first assertions — no `waitForTimeout`.
- DB reset per file (worker-scoped fixture); within a file, tests share state if useful.
- Auth via Playwright `storageState` for tests that don't care about login; auth flows themselves test login from scratch.
- Trace + video on first retry only.

### 12.3 Rust integration tests (`tests/` in each crate; gated by the `integration` feature flag)

Real `PgPool` from `testcontainers` Postgres, real `axum::Router` via `axum::serve` on a `tokio::net::TcpListener::bind("127.0.0.1:0")`, real WebSocket via `tokio-tungstenite` client. No mocks for HTTP/DB layers.

Coverage:

- Auth middleware: cookie set, session row, revoke, OIDC code flow (against `wiremock` for the IdP discovery + token endpoints, or a real Dex container via `testcontainers`).
- ACL resolution incl. parent-walk + inheritance + cache invalidation on tree move.
- Tree operations: create, move-between-parents, drag-reorder, delete + restore.
- Markdown import → Y.Doc → export round-trip via HTTP.
- Snapshot + GC happy path: drive N updates, assert snapshot + pruned updates.

### 12.4 Unit tests

Standard `#[test]` in `mod tests` blocks per file. `assert_eq!` + `assert!` from std; no extra assertion crates unless we hit pain. `cargo nextest` as the runner (fast, parallel, better output). Targets per §12.1 list.

### 12.5 CRDT-specific extras

**Property tests** using `proptest`:

```
proptest! {
    fn batches_converge_regardless_of_partition_order(
        initial: Doc,
        batches: Vec<UpdateBatch>,
        order: Vec<usize>,
    ) {
        // applying batches in any partition order from any starting doc
        // produces the same final state as applying them in canonical order
    }
}
```

**Network-partition / chaos integration tests** (nightly, not per-PR):

- Two `knot-server` processes against the same Postgres.
- One WS client per replica, both editing the same doc.
- Inject: drop NOTIFY on replica B for 2 s; kill replica A mid-edit; block A's INSERT for 500 ms.
- Assert: after each scenario, both replicas' Y.Docs encode to identical state vector + bytes.

### 12.6 Frontend tests

- Vitest unit tests for `lib/`, `stores/`, `tree-model.ts`, csrf, fetch wrapper, schema helpers.
- Vitest + Testing Library component tests for primitives and self-contained features. Fetch mocked via MSW.
- No snapshot tests of rendered HTML.
- Visual regression skipped for v0.1.

### 12.7 CI gates

Per-PR (required to merge):

- `make fmt lint` clean (`cargo fmt --check`, `cargo clippy -- -D warnings`, `prettier --check`, `eslint`)
- `cargo nextest run --workspace --features integration` green
- `vitest run` green
- `pnpm tsc --noEmit` clean
- `make e2e` (fast Playwright project, ~5 min) green
- `helm lint deploy/helm/knot` clean
- `cargo deny check` clean

Nightly on main (advisory):

- `make e2e-chaos`
- Property tests with larger N
- Long-soak collab test (10 users, 30 min, assert convergence + no task leaks via `tokio-console` snapshots)

### 12.8 Coverage policy

Not gated on percentage. Gated on:

- Every `knot-markdown` schema element has a round-trip fixture.
- Every public API endpoint has at least one Rust integration test.
- Every Playwright flow in the headline list exists and runs green.

## 13. Risks & mitigations

The original spec listed "Go Yjs binding not viable" as risk #1. That risk drove the Go→Rust pivot on 2026-06-01: with `yrs` as the canonical Rust Yjs implementation, the binding is a `cargo add` rather than a spike. The risk no longer applies; the remaining risks are smaller and well-understood.

| # | Risk | Mitigation |
|---|---|---|
| 1 | MD ↔ ProseMirror round-trip lossy for advanced blocks. | v0.1 schema limited to lossless elements (§8.8). Fixture-driven test suite for every supported node. Advanced blocks added later with explicit MD rules. |
| 2 | LISTEN/NOTIFY misses (replica disconnect, missed payload). | Watermark + catch-up: every room polls `doc_updates WHERE seq > last_applied_seq` on a slow tick (5 s). CRDT idempotency makes double-apply harmless. |
| 3 | Postgres NOTIFY throughput ceiling under heavy multi-doc + multi-replica load. | Built behind `crdt::Bus` trait; swap to NATS/Redis without touching room code. `knot_collab_notify_lag_seconds` metric surfaces pressure. |
| 4 | ACL cache invalidation misses an edge (tree move into permissive subtree, etc.). | Outbox-driven invalidation (§5.7) committed in the same transaction as the mutation. 5-minute TTL belt-and-suspenders. Tests cover tree-move + grant-change cases. |
| 5 | Session theft via XSS in the editor. | Session cookie HttpOnly. CSP forbids inline scripts; nonce-based for our own. ProseMirror schema cannot represent `<script>`. |
| 6 | Tokio task leak in long-lived rooms. | Tests use `tokio::task::tracker` snapshots (or `tokio-console` in CI) to assert no task survives a clean shutdown of a room. `CancellationToken` plumbed everywhere. |
| 7 | Replica clock skew affecting `created_at` ordering. | Ordering relies on `seq` (global bigserial), never on timestamps. `created_at` is human-readable only. |
| 8 | `yrs` version churn breaking on-disk binary format. | Binary updates are durably persisted; if `yrs` ever changes update v1 encoding we pin the version and document the upgrade path. Snapshot table lets us re-encode lazily.|

## 14. Explicitly out of scope for v0.1

Listed exhaustively so the spec can't quietly grow:

```
EDITOR FEATURES        comments / discussions, mentions, @-references,
                       inline reactions, version history UI, suggesting mode
CONTENT                attachments / image uploads beyond a placeholder,
                       diagrams (drawio, excalidraw, mermaid render),
                       embeds (oEmbed, YouTube), tables (no schema node,
                       no UI in v0.1 — added later with its own MD rule)
SEARCH                 full-text search (Postgres FTS reserved; no UI)
NOTIFICATIONS          email, in-app, webhooks
ACCOUNT                2FA / TOTP, magic-link / passwordless,
                       SAML, SCIM provisioning, OAuth-as-IdP,
                       password reset email flow (CLI-only in v0.1)
ADMIN                  audit log UI (rows written, no UI), usage analytics,
                       backup / restore tooling beyond pg_dump
MULTI-TENANCY          multi-workspace UX (data model ready; no switcher)
PUBLISHING             public read-only links, anonymous viewers
APIs                   public REST/Webhook API for integrations,
                       GraphQL, gRPC
MOBILE / DESKTOP       any client other than the bundled SPA
i18n                   localisation (English only; copy in one file
                       so adding later is mechanical)
```

## 15. Acceptance criteria for the Foundation implementation plan

Foundation is "done" when all of the following hold simultaneously on `main`:

1. **Walking-skeleton runs.** A fresh `make dev` brings up knot + Postgres + Dex; a first-run user can sign up, log in (both local and OIDC), create a doc, edit it in two browser tabs concurrently and see live convergence, build a small tree (3-4 nested docs), move a doc, set a per-doc grant on a sibling user, observe that sibling cannot edit a viewer-only doc, export the doc as Markdown, import a `.md` file as a new doc.
2. **Persistence survives restart.** Stop knot, restart, reopen the doc; content is intact and editing resumes.
3. **Two replicas converge.** Two `knot-server` processes against one Postgres; one user on each; concurrent edits converge within 2 s. The `two-users-converge` Playwright test runs in CI against this topology.
4. **CI green.** All per-PR gates (§12.7) green. Nightly chaos run has produced at least one full pass in the week before declaring done.
5. **Helm chart deploys to `kind`.** `make kind.up` from a clean state reaches a working knot URL inside 5 min.
6. **Distroless image < 40 MB** (Rust musl static binary is roughly half the size of a Go equivalent).
7. **Observability live.** Hitting `/metrics` shows the listed counters/histograms; `tracing` JSON output is parseable; turning on `KNOT_TRACING_ENABLED` produces a connected trace for a doc-edit flow when an OTLP collector is reachable.
8. **First-run docs.** A README and a short "first 10 minutes" guide explain `nix develop`, `make dev`, the architecture at a one-page level, and the dev IdP setup.

### 15.1 Spike (the short version)

The original Foundation spec called out a multi-day "library survey + CRDT smoke test" spike because the Go Yjs ecosystem was thin. Post-pivot, the spike collapses dramatically because `yrs` is the answer.

The reduced spike still happens — Plan 1 in `docs/superpowers/plans/` — but it now focuses on three concrete deliverables rather than research:

1. **Bring up `yrs`** with a smoke harness: a tiny `axum` server with one in-memory room serves the y-sync v1 protocol over WebSocket; two Tiptap browsers connect and converge.
2. **Define the canonical ProseMirror schema** (initial set per §8.8) in `tools/schema.json` + codegen (`tools/schemagen` → `crates/knot-markdown/src/schema.rs` + `web/src/features/editor/schema.ts`).
3. **Implement `Engine::to_markdown` / `Engine::from_markdown`** for the v0.1 schema. Ship the fixture corpus.

If any of these three fail, the spike escalates back to the user; otherwise, Foundation proceeds with the full data model, auth, persistence, etc.

## 16. Open questions deferred to later specs

- **Comments anchoring**: ProseMirror marks on Y.Doc relative positions, or sidecar table with rebased positions? (Comments spec.)
- **Search**: Postgres FTS first, or external index (Meili, Tantivy)? (Search spec.)
- **Diagram persistence shape**: drawio/excalidraw embedded as JSON-in-ProseMirror-node, or a separate `diagrams` table referenced by ID? (Diagrams spec.)
- **Attachments dedup**: content-addressed (sha256→blob) vs per-doc copies; affects quota and "image used in two docs" UX. (Attachments spec.)
- **Version history UI**: snapshots already exist for hydration; do we expose them as user-visible versions, or maintain a separate `doc_versions` concept driven by explicit "save as version" intent? (History spec.)
- **HA & multi-region**: cross-region collab is its own problem (Postgres NOTIFY doesn't cross primaries). Deferred until a real adopter asks. (HA spec.)

## 17. Proposed order of subsequent specs

```
1. Foundation                      ← this spec
2. Editor + Markdown round-trip    canonical schema, MD serializer
                                   correctness, schema codegen workflow
3. Diagrams                        excalidraw first (single-file JSON,
                                   pure-frontend renderer), drawio second
4. Comments                        anchored discussions
5. Attachments / images
6. Search                          Postgres FTS v1
7. Notifications
8. Multi-workspace UI              switcher, cross-workspace invite
9. Public links                    read-only sharing
10. HA / horizontal-scaling refinements
```

Product narrative after each step:

- After 2: a single user drags in existing Markdown notes and edits them collaboratively. **Real "Notion alternative" moment.**
- After 3: a team can replace Confluence pages that have draw.io diagrams. **First real wedge against Confluence.**
- After 4: a team can review each other's docs in-thread. **First real Notion-feel for groups.**
- After 5–6: search what they wrote and put images in it. **Self-hosted-replacement-for-Confluence** is now a true statement.
- After 7–9: the operational extras expected at this maturity.
