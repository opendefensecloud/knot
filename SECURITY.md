# Security Policy

## Supported versions

knot is pre-1.0. Security fixes land on `main` and in the latest tagged
release. There is no long-term support branch yet.

| Version | Supported |
|---------|-----------|
| `main` / latest tag | ✅ |
| older tags | ❌ |

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report privately via GitHub's [private vulnerability reporting](https://github.com/trevex/knot/security/advisories/new)
(Security → Report a vulnerability). If that is unavailable, email the
maintainer listed in `Cargo.toml`.

Please include:

- a description of the issue and its impact,
- steps to reproduce (a proof-of-concept if possible),
- affected version / commit, and
- any suggested remediation.

We aim to acknowledge reports within a few business days and to keep you
updated as we triage and fix. Coordinated disclosure is appreciated — we'll
agree on a timeline before any public write-up.

## Scope

In scope: the `knot-server` binary, the crates under `crates/`, the web
client under `web/`, and the Helm chart under `deploy/helm/knot/`.

Out of scope: issues that require a pre-compromised host or database, social
engineering, and findings in third-party dependencies that are already
tracked. Known, accepted dependency advisories are documented with
reachability analysis in [`deny.toml`](deny.toml).

## Hardening status

knot is v0.1 and not yet fully hardened. Known limitations are tracked in the
README "Status" section and in `docs/superpowers/`. Notably: auth throttling
is per-process (not shared across replicas), and Excalidraw boards have no
cross-pod fan-out — run a single replica until those land.
