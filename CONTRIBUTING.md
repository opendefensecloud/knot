# Contributing to knot

Thanks for considering a contribution. This file covers the setup, the test infrastructure, and the workflow expectations. Read `ARCHITECTURE.md` first if you want the system overview.

## Setup

```bash
git clone https://github.com/trevex/knot
cd knot
cp .env.example .env
make compose.up                  # boot Postgres + Dex in docker
make dev                         # backend (cargo-watch) + frontend (vite) with live reload
```

Open `http://localhost:5173` — the first visit lands on `/setup`. The Nix flake at `flake.nix` pins the full toolchain (Rust, Node, pnpm) — `direnv allow` is the zero-friction path.

Useful one-shot targets:

```bash
make test                # cargo + vitest
make e2e                 # Playwright (needs compose.up)
make lint                # clippy + cargo fmt --check + tsc + eslint
make fmt                 # cargo fmt + prettier write
```

## Plan-driven workflow

Non-trivial changes land as a **plan** in `docs/superpowers/plans/`. The pattern, end to end:

1. Brainstorm scope. Decide what's in v0.1 and what's deferred.
2. Write the plan via the `superpowers:writing-plans` skill → `docs/superpowers/plans/YYYY-MM-DD-<topic>.md`. Every task is bite-sized (2–5 minutes of work), shows exact code, names exact files.
3. Execute via `superpowers:subagent-driven-development`. Fresh implementer per task, two-stage review (spec + code quality).
4. On merge, write `docs/superpowers/research/YYYY-MM-DD-<topic>-outcome.md` capturing status, gates, what was non-obvious, what's deferred, carryforward.
5. Add a row to `docs/superpowers/README.md`.

Bug fixes and one-line tweaks don't need a plan — open a PR directly.

## Tests

Rust uses `cargo nextest`. Web uses Vitest (unit) + Playwright (e2e). Run `make test` for everything; `make e2e` runs the Playwright suite (needs the dev-compose Postgres + Dex up).

### Test infrastructure

**Integration tests use `knot_test_support::fresh_db()` against the dev-compose Postgres.** Each call creates a unique `t_<uuid>` database on the shared `localhost:5432` instance. **Never use `testcontainers`** — every test binary would spawn its own container; with ~10 binaries × repeated runs this leaked thousands of containers and OOM'd the host twice in the past. The dev-compose stack must be running before `cargo test`.

Leftover `t_*` test databases are reaped by:

```bash
make db.cleanup
```

### Adding a migration

```bash
make migrate.create NAME=add_foo_column
# edits the new file: migrations/<timestamp>_add_foo_column.sql
make migrate.up                  # or just `make dev` — the migrate subcommand runs on startup
```

Migrations are **forward-only**. Never edit a landed migration. If you need to undo, write a follow-up migration.

### Editor schema changes

If you touch `tools/schema.json` (the canonical ProseMirror node/mark schema):

```bash
make schema.gen
```

This regenerates both `crates/knot-markdown/src/schema.rs` and `web/src/features/editor/schema.ts`. Commit both alongside the JSON change.

## Commit style

[Conventional Commits](https://www.conventionalcommits.org/). Common prefixes used in this repo:

- `feat:` — new feature
- `fix:` — bug fix
- `test:` — test-only changes
- `docs:` — documentation
- `chore:` — tooling, deps, non-functional cleanup
- `build:` — build system (Dockerfile, Makefile, Cargo.toml)
- `ci:` — GitHub Actions

## Dependencies

[Renovate](https://docs.renovatebot.com/) (`.github/renovate.json`) keeps the
Cargo workspace, both pnpm projects, the GitHub Actions, and the Dockerfile base
images current. Patch and minor updates merge themselves once CI is green;
everything else waits for a human.

Majors do not open a PR on their own — they are listed on the **Dependency
Dashboard** issue, and ticking a box there tells Renovate to raise that one.
That keeps a major migration (tiptap 2 → 3, tailwind 3 → 4) a deliberate choice
rather than eight red PRs.

Weekly `lockFileMaintenance` runs `cargo update` / `pnpm update` against the
lockfiles. This is the only route by which a patched *transitive* dependency
reaches us — bumping a direct dependency cannot pull one in on its own — so it
is how most RUSTSEC and GHSA advisories actually get closed here.

Three versions are pinned in more than one file and must move together.
Renovate groups each into a single PR; do the same by hand:

| what | pinned in |
|---|---|
| Rust | `rust-toolchain.toml`, `dtolnay/rust-toolchain@…` in `ci.yml`, `rust:…-alpine` in `Dockerfile` |
| pnpm | `packageManager` in `web/` and `e2e/package.json`, `pnpm/action-setup` `version:` |
| Node | `node:…-alpine` in `Dockerfile`, `node-version:` in `ci.yml` |

Left out of automation on purpose: the exact `prosemirror-*` pins in
`web/pnpm-workspace.yaml` (they deduplicate the editor core — see the comment
there for what breaks), the security floors in the same file's `overrides`, and
`@playwright/test`, which ships the browser the e2e suite runs against.

`cargo deny check` gates every PR. When an advisory has no reachable fix, add it
to `deny.toml` with the dependency path and the upstream event that would let us
drop it again.

## Pull requests

Before opening a PR:

- `make lint` clean
- `make test` green
- `make e2e` green (when relevant)
- New user-visible surface includes an e2e
- Plan + outcome docs added/updated if non-trivial

## License grant

By submitting a pull request, you agree to license your contribution under the [Apache License, Version 2.0](LICENSE).

## Code of conduct

Be kind. Disagree on technical substance, not people. If you wouldn't say it in a hallway conversation at work, don't say it in an issue or PR review.
