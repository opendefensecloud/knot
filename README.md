# knot

A self-hosted, collaborative knowledge base. Like Notion or Confluence — but your data lives on your hardware, the source is yours to read, and the real-time editor is built on CRDTs (no central operational-transform server to wedge).

- **Backend:** Rust (axum + yrs + sqlx + tokio + mimalloc). Static musl binary, ~20 MB scratch image.
- **Frontend:** React 18 + Tiptap + TanStack Query + Zustand.
- **Storage:** PostgreSQL 18. Single database, no Redis.
- **Auth:** Local credentials (Argon2id) + OIDC (tested against Dex).
- **Deploy:** Helm chart at `deploy/helm/knot/`. Multi-arch image (amd64 + arm64).

## Status

**v0.1** — feature-complete for single-workspace teams; production-ready enough to dogfood. Tagged releases publish a cosign-signed multi-arch image (amd64/arm64) with SBOM + SLSA provenance, plus the Helm chart (PrometheusRule + ServiceMonitor templates). Real-time collaboration — documents and Excalidraw boards — fans out across pods over Postgres `LISTEN/NOTIFY`, so it scales horizontally; the chart defaults to a single replica. See `docs/superpowers/research/` for landed work and scale-out notes.

## Quickstart

```bash
git clone https://github.com/opendefensecloud/knot
cd knot
cp .env.example .env             # local KNOT_* defaults
make compose.up                  # boot Postgres + Dex (+ toxiproxy for e2e)
make install                     # fetch node dependencies
make dev                         # backend + frontend with live reload
```

Open `http://localhost:5173`. The first visit lands on `/setup` — create the workspace owner.

### Requirements

- Rust stable (`rust-toolchain.toml` pins the `stable` channel; edition 2024 needs a recent stable)
- Node 20+
- pnpm 9+ (`corepack enable pnpm` works)
- Docker (for the dev-compose Postgres + Dex)

The Nix flake at `flake.nix` pins all of the above; `direnv allow` is the zero-friction path.

## Run the tests

```bash
make test                # cargo + vitest
make e2e                 # Playwright (needs compose.up)
make lint                # clippy + fmt --check + tsc
```

## Architecture

See `ARCHITECTURE.md` for the one-page system overview. The long-form design spec is at `docs/superpowers/specs/2026-06-01-knot-foundation-design.md`. Plan outcome docs for everything landed since live in `docs/superpowers/research/`.

## API

Everything the web app does goes through a JSON API under `/api/*` — pages,
markdown, search, tasks, comments, history, exports, workspace import/export.
There is no OpenAPI spec; the full curl recipe list is in
[`.claude/skills/knot-api/SKILL.md`](.claude/skills/knot-api/SKILL.md).

Two ways to authenticate:

- **Session cookie** (what the browser uses): `POST /auth/login` sets `sid`
  (HttpOnly) and `csrf`; unsafe methods must echo `csrf` in `X-CSRF-Token`.
- **Personal API token** (for bots and scripts): `Authorization: Bearer knot_…`,
  no cookies and no CSRF header.

Mint tokens in **Settings → API tokens**, or over the API with a session:

```bash
curl -X POST https://knot.example.com/api/tokens \
  -H 'Content-Type: application/json' -H "X-CSRF-Token: $CSRF" -b cookies.txt \
  -d '{"name":"docs-bot","scopes":["read","create","write"]}'
```

The response carries `token_secret` — the `knot_…` string — exactly once; only
its hash is stored. Scopes are any subset of `read`, `create`, `write`,
`delete`. A token acts as its user, so the workspace role and per-page access
(including restricted pages) still apply on top of them. Tokens cannot manage
tokens, and they do not authenticate the collab sockets or `/auth/*`.
`DELETE /api/tokens/{id}` revokes one immediately.

```bash
TOKEN=…                                    # token_secret from above
curl -H "Authorization: Bearer $TOKEN" https://knot.example.com/api/docs
curl -H "Authorization: Bearer $TOKEN" -X POST \
  https://knot.example.com/api/docs/$DOC_ID/markdown \
  -H 'Content-Type: text/markdown' --data-binary @page.md
```

Endpoints worth knowing first: `GET /api/docs` (tree), `POST /api/docs`
(create), `GET|POST /api/docs/{id}/markdown` (read/write a page as markdown),
`GET /api/search?q=`, `GET /api/workspace/tasks`, `GET /api/workspace/export`.

## Deploy

```bash
helm install knot ./deploy/helm/knot \
  --set database.url='postgres://...' \
  --set session.key="$(openssl rand -base64 32)"
```

See `deploy/helm/knot/README.md` for the full install guide, the external-secret pattern, and OIDC setup.

## Observability

- `/api/healthz` — liveness
- `/api/readyz` — readiness (checks DB)
- `:9090/metrics` — Prometheus exposition

Import `deploy/grafana/knot.json` into Grafana 9+. SLOs and error-budget framework at `docs/SLO.md`.

## Contributing

`CONTRIBUTING.md` covers setup, the test infrastructure (no testcontainers — reuse the dev-compose Postgres), the plan-driven workflow, and how to add a migration.

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
