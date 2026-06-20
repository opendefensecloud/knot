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
  ├─ /api/*                  knot-storage: sqlx stores (Postgres)
  ├─ /collab/:doc_id WS      knot-crdt: Room actor (yrs Y.Doc per doc)
  ├─ /api/healthz, /readyz   liveness + readiness
  └─ :9090/metrics           Prometheus exposition
            │
            ▼
       PostgreSQL 18
       ├─ users, workspaces, sessions, workspace_members
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
| `knot-auth`       | password hashing, session creation, OIDC discovery + flow |
| `knot-config`     | figment-based `KNOT_*` env loader (default → file → env) |
| `knot-storage`    | sqlx stores (users, workspaces, docs, grants, snapshots, updates) + migrations |
| `knot-crdt`       | yrs `Engine`, `Rooms` registry, `Room` actor, `PgBus` over LISTEN/NOTIFY |
| `knot-docs`       | ACL evaluation engine + ACL invalidation listener |
| `knot-markdown`   | Markdown ↔ ProseMirror round-trip (canonical schema) |
| `knot-obs`        | structured logging, OTLP traces, Prometheus exporter, well-known metric describes |
| `knot-test-support` | `fresh_db()` against the dev-compose Postgres (NEVER testcontainers) |
| `tools/schemagen` | generates `schema.rs` (server) + `schema.ts` (client) from `tools/schema.json` |

## Request lifecycle (HTTP)

```
client                middleware (outside → inside)              handler
  │                  ┌─ metrics record (counter + histogram)
  │   HTTP   ─────►  ├─ tower-http TraceLayer  (#[instrument] inside)
  │                  ├─ session loader (sid cookie → AuthContext)
  │                  ├─ CSRF check (unsafe methods)
  │                  └─ route handler         ─────►  knot-auth / knot-storage / knot-crdt
  │   HTTP   ◄─────                            ◄─────
```

Outermost layer is the metrics recorder so the histogram includes everything inside. TraceLayer wraps handlers + auth so spans cover the full lifetime minus the metrics recording itself.

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
