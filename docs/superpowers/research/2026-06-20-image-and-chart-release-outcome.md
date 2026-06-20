# Outcome: image + OCI Helm chart release (deployable from GitHub)

**Date:** 2026-06-20
**Goal:** Cutting a `v*` tag publishes a deployable multi-arch image **and** an installable
Helm chart to GitHub Container Registry, so knot can be deployed straight from ghcr. No CD /
cluster credentials in CI — the operator runs the deploy.

## What landed

`.github/workflows/release.yaml` now has three jobs on a `v*` tag:

1. **image** (unchanged this round): builds multi-arch (amd64/arm64), pushes
   `ghcr.io/<owner>/knot:<version>` (+`<major.minor>`, `latest`), with OCI labels, SBOM,
   max-mode SLSA provenance, and keyless cosign signature. Binary stamped with the tag/sha.
2. **chart** (new): `helm registry login ghcr.io` → `helm package --app-version <stripped-tag>`
   → `helm push` to `oci://ghcr.io/<owner>/charts`. Gains `packages: write`.
3. **release** (new): creates a GitHub Release for the tag with auto-generated notes plus a
   pull/install snippet. Gains `contents: write`.

Chart wiring:
- `deploy/helm/knot/values.yaml` — default `image.repository` → `ghcr.io/christianhuening/knot`
  (was the upstream `trevex/knot`), so a default install pulls the fork's own build.
- `deploy/helm/knot/Chart.yaml` — `version` bumped `0.2.0` → `0.3.0`.
- `deploy/helm/knot/README.md` — Quick install rewritten to the OCI form.

## Versioning model

- **Image tag** = git tag with the leading `v` stripped (`v0.1.0` → `0.1.0`), == `appVersion`.
- **Chart version** = `Chart.yaml: version` (managed by hand; bump when the chart changes).
- **appVersion** is overridden at package time (`helm package --app-version <stripped-tag>`),
  so the published chart always deploys exactly the image that release built. The chart's
  `image.tag` defaults to `.Chart.AppVersion`, so no `--set image.tag` is needed.

## Release runbook

```bash
# 1. commit the working-tree changes (this branch), get them onto main
# 2. ensure deploy/helm/knot/Chart.yaml `version` is > the last published chart version
git tag v0.1.0 && git push origin v0.1.0
# → image  ghcr.io/christianhuening/knot:0.1.0 (+0.1, +latest), signed + attested
# → chart  oci://ghcr.io/christianhuening/charts/knot:0.3.0  (appVersion 0.1.0)
# → a GitHub Release for v0.1.0 with notes
```

Deploy:
```bash
helm install knot oci://ghcr.io/christianhuening/charts/knot --version 0.3.0 \
  -n knot --create-namespace \
  --set database.url='postgres://…' \
  --set session.key="$(openssl rand -base64 32)" \
  --set baseUrl=https://knot.example.com
```

## Operational notes

- **ghcr package visibility:** new packages (`knot`, `charts/knot`) are **private** by default.
  To pull/install without auth, set both public in the repo's ghcr package settings. To keep
  them private, `helm registry login ghcr.io` before pulling the chart and set
  `--set image.pullSecrets[0].name=<secret>` so the cluster can pull the image.
- **Restore `ct install`:** `helm-ci.yaml` skips the deploy-to-kind smoke test with a note that
  the default image isn't published yet. Once the first release publishes an image, that smoke
  test can be restored (kind-load the image + throwaway Postgres + `ct install`).

## Deferred (explicitly out of scope)

- **CD** (workflow running `helm upgrade --install` against a cluster) — would need a kubeconfig
  secret in GitHub.
- **Cosign-signing the chart** OCI artifact for parity with the image — infra is already present
  (`cosign`, `id-token`), can be added later.

## Non-obvious

- `ct lint` (helm-ci.yaml) enforces a chart-version increment vs `main` when any chart file
  changes — hence the `0.2.0 → 0.3.0` bump alongside the values/README edits.
- The chart is pushed to `…/charts/knot`, **not** `…/knot`, to avoid colliding with the image
  repository path in ghcr.
