# knot

A self-hosted, collaborative knowledge base. Like Notion or Confluence — but your data lives on your hardware, the source is yours to read, and the real-time editor is built on CRDTs (no central operational-transform server to wedge).

- **Backend:** Rust (axum + yrs + sqlx + tokio + mimalloc). Static musl binary, ~20 MB scratch image.
- **Frontend:** React 18 + Tiptap + TanStack Query + Zustand.
- **Storage:** PostgreSQL 18. Single database, no Redis.
- **Auth:** Local credentials (Argon2id) + OIDC (tested against Dex).
- **Deploy:** Helm chart at `deploy/helm/knot/`. Multi-arch image (amd64 + arm64).

## Status

**v0.1.** Feature-complete for single-workspace teams; production-ready enough to dogfood. The release pipeline publishes a multi-arch image on tag, and the chart ships PrometheusRule + ServiceMonitor templates. Remaining hardening before scale-out: auth throttling is per-process (not shared across replicas), Excalidraw boards have no cross-pod fan-out (HA is documents-only — keep `replicaCount: 1`), and there's no image signing/SBOM yet. See `docs/superpowers/plans/` for the roadmap.

## Quickstart

```bash
git clone https://github.com/trevex/knot
cd knot
cp .env.example .env             # local KNOT_* defaults
make compose.up                  # boot Postgres + Dex
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
make lint                # clippy + fmt --check + tsc + eslint
```

## Architecture

See `ARCHITECTURE.md` for the one-page system overview. The long-form design spec is at `docs/superpowers/specs/2026-06-01-knot-foundation-design.md`. Every plan landed since (Plans 3–11) has an outcome doc at `docs/superpowers/research/`.

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
