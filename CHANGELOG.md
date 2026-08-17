# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from 1.0
onward. Commits use [Conventional Commits](https://www.conventionalcommits.org/),
so this log can be regenerated from history (e.g. with `git-cliff`).

## [Unreleased]

_Changes on `main` that have not yet been tagged._

## [0.2.1] - 2026-08-17

**The first release whose Helm chart can actually be installed.** Charts `0.1.0`
and `0.2.0` both fail at the very first step of `helm install`; this release
fixes that and adds the CI test that found it.

No application code changed — the diff is the chart and CI only. The `0.2.1`
image is behaviourally identical to `0.2.0`, so upgrading from `0.2.0` carries
no runtime risk beyond the rollout itself.

### Fixed
- **The chart could never be installed.** The `pre-install`/`pre-upgrade`
  migrate Job injected `KNOT_DATABASE_URL` but not `KNOT_SESSION_KEY`. The binary
  loads and validates its entire config _before_ dispatching the subcommand, and
  validation rejects an empty session key — so the hook exited 2 with
  `KNOT_SESSION_KEY is required` and **every** `helm install` and `helm upgrade`
  failed before reaching the Deployment:

  ```text
  Error: INSTALLATION FAILED: failed pre-install: resource Job/knot-migrate
         not ready. status: Failed, message: Job Failed. failed: 1/1
  config: invalid: KNOT_SESSION_KEY is required (set it in every environment)
  ```

  `migrate` never reads the session key; it only has to survive validation. The
  Job now receives it from the same Secret the Deployment uses, honouring
  `session.existingSecretName`/`existingSecretKey`.

  The bug survived two releases because `ct install` is disabled in chart CI, so
  nothing had ever deployed the chart — `helm lint` cannot see a hook that fails
  at runtime.

### Added
- **Helm upgrade test** (`.github/workflows/helm-upgrade.yaml`). Installs the
  previous _published_ release — chart pulled from ghcr and the real image, not
  the working tree — seeds a document through that release's own API, runs
  `helm upgrade` to the working tree, and then asserts:

  1. `/api/version` changed — the binary actually swapped
  2. the session cookie minted by the **old** version still authorises — the
     session format and signing key survived
  3. the seeded document still reads back — schema and migrations survived

  Runs on chart, migration and `Dockerfile` changes, weekly against `main`, and
  on demand. This is what caught the bug above.

### Operators
- **Upgrading from chart `0.2.0`.** If you worked around the broken hook with
  `--set migrations.enabled=false`, drop that override — `0.2.1` runs migrations
  itself again. Nothing else about your values file changes.
- **Installing fresh.** `helm install` now works with defaults; no manual
  `/knot-server migrate` step is needed.
- The stray `0.3.4` chart package has been deleted from ghcr, so
  `helm install oci://ghcr.io/opendefensecloud/charts/knot` without `--version`
  resolves to the newest real release instead of the mis-versioned `v0.1.0`
  chart. The `--version` advice in the 0.2.0 notes below is no longer required.
- The Prometheus `route` label change noted under 0.2.0 still applies if you are
  coming from `0.1.0`.

## [0.2.0] - 2026-08-16

Maintenance release: a full dependency refresh across every surface, clearing
all outstanding advisories. No user-facing features changed, but the operational
notes below need reading before rollout.

### Security
- `cargo deny check` passes again (previously failing): `ammonia` 4.1.4 fixes
  two XSS issues (RUSTSEC-2026-0193 mXSS via MathML `annotation-xml`,
  RUSTSEC-2026-0213 XSS via SVG `animate`/`set`), plus `anyhow` 1.0.104
  (RUSTSEC-2026-0190), `crossbeam-epoch` 0.9.20 (RUSTSEC-2026-0204) and the
  yanked `spin` 0.9.8.
- `pnpm audit` goes from 53 advisories (1 critical, 26 high, 22 moderate, 4 low)
  to **zero** in `web/`; `e2e/` was and stays clean.
- Dropped `rustls-pemfile` (unmaintained, RUSTSEC-2025-0134) in favour of the
  `PemObject` trait from `rustls-pki-types`, which was already in the graph.
- The pnpm `overrides` moved to `web/pnpm-workspace.yaml`; pnpm ≥ 11 no longer
  reads the `pnpm` field in `package.json`. Their floors were also stale — the
  `nanoid` pin was `^3.3.8` where the advisories require `>= 3.3.18`. `dompurify`
  and `lodash-es` are now patched too; both were previously documented as having
  no available fix.
- The pinned package manager itself, pnpm 9.0.0, carried 24 open advisories with
  no fix anywhere in the 9.x line. Now pnpm 11.22.0.
- Dex 2.41.1 → 2.45.1 in the dev compose stack (GHSA-7qjx-gp9h-65qj).

### Fixed
- The Helm chart's default `image.repository` pointed at
  `ghcr.io/christianhuening/knot` — a real, populated registry belonging to a
  different lineage — while the release workflow publishes to
  `ghcr.io/opendefensecloud/knot`. A default `helm install` therefore ran an
  unrelated image and _appeared to work_. It now points at the published
  repository.
- `Chart.yaml`'s `version: 0.3.4` / `appVersion: "0.1.7"` were inherited from
  that same unrelated lineage and corresponded to no tag in this repo. Both are
  now `0.0.0` and documented as inert placeholders, since `release.yaml` forces
  each from the git tag at package time. `ct.yaml` sets
  `check-version-increment: false` to match.
- Corrected `deny.toml`: the RUSTSEC-2023-0071 entry described the path as
  `openidconnect -> oauth2 -> rsa`, but `oauth2` declares no `rsa` at all — it
  is a direct dependency of `openidconnect`.

### Changed
- Rust toolchain 1.96.0 → 1.97.1; declared MSRV corrected 1.80 → 1.94 (the real
  floor, set by sqlx 0.9).
- axum 0.7 → 0.8, sqlx 0.8 → 0.9, yrs 0.21 → 0.27, OpenTelemetry 0.27 → 0.32,
  rand 0.8 → 0.10, zip 2 → 8, rust-s3 0.36 → 0.37, tower-http 0.6 → 0.7,
  pulldown-cmark 0.12 → 0.13, ipnetwork 0.20 → 0.21,
  metrics-exporter-prometheus 0.16 → 0.18, tokio-postgres-rustls 0.13 → 0.14.
- React 18 → 19, Vite 5 → 7, Vitest 2 → 4, lucide-react 0.460 → 1.31.
- Node 22 → 24 across the Dockerfile, CI and the Nix dev shell; zig 0.13 → 0.16
  in the release image; base images and GitHub Actions refreshed to current
  majors.
- Removed dependencies that nothing imported: `proptest`, `testcontainers` and
  `testcontainers-modules` (declared but absent from `Cargo.lock`), plus
  `y-websocket`, `@tiptap/extension-bubble-menu` and `@tiptap/extension-mention`.

### Operators — read before upgrading
- **Prometheus label values change.** `knot_http_requests_total` and
  `knot_http_request_duration_seconds` carry a `route` label taken from axum's
  `MatchedPath`. axum 0.8 changed path-parameter syntax, so values move from
  `/api/docs/:id` to `/api/docs/{id}`. The bundled Grafana dashboard and
  PrometheusRule do not select on `route` and are unaffected, but any external
  dashboard, alert or recording rule keyed to the old form will silently stop
  matching.
- **Pin the chart version when installing.** The `v0.1.0` release published its
  chart as `0.3.4` (the bug fixed in b45a8bb). OCI registries have no `latest`
  for charts, so `helm install` without `--version` resolves the highest semver
  — still `0.3.4`. Pass `--version 0.2.0` explicitly, or delete the stray
  `0.3.4` package version.

### Deferred
Tailwind 4, TypeScript 7 (blocked: `@typescript-eslint` peers `<6.1.0` and this
repo relies on type-aware linting), Tiptap 3, jsdom 30 (needs Node ≥ 22.22.2),
Vite 8 (Rolldown/Oxc), and `reqwest` 0.13 (pinned by `openidconnect`).

## [0.1.0] - 2026-06-04

First tagged release. Feature-complete for single-workspace teams.

### Added
- Real-time collaborative document editing on Yjs/yrs CRDTs over a single
  WebSocket protocol; cross-pod fan-out via Postgres `LISTEN/NOTIFY`.
- Local (Argon2id) and OIDC authentication; session + CSRF cookies.
- Document tree, ACL grants, public share links, comments with @mentions,
  reactions, tasks/checklists with due dates, full-text + prefix search.
- Excalidraw boards, Mermaid diagrams, tables, image/file attachments,
  Markdown import/export, document templates and history.
- Observability: structured logging, OTLP traces, Prometheus metrics.
- Helm chart with migrate hook, NetworkPolicy, ServiceMonitor, PrometheusRule,
  and multi-arch (amd64 + arm64) scratch image.

[Unreleased]: https://github.com/opendefensecloud/knot/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/opendefensecloud/knot/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/opendefensecloud/knot/releases/tag/v0.1.0
