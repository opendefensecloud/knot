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

Rate limits and quotas are abuse guards, not billing-grade accounting: the
rate limits live in each replica's memory (a client spread over N replicas
gets up to N times each limit), and quotas are best effort under concurrent
writes. Behind a reverse proxy, set `trustedProxies` in the Helm chart
(`KNOT_TRUSTED_PROXIES`) to the proxy's addresses only: otherwise the per-IP
limits are skipped for proxied requests and the login throttle can't tell
clients apart. Behind an L4 balancer that adds no `X-Forwarded-For`, every
client shares one address and one sign-in bucket. knot never believes
`X-Forwarded-For` from a peer outside that list. See "Rate limits and
quotas" in [`deploy/helm/knot/README.md`](deploy/helm/knot/README.md).
