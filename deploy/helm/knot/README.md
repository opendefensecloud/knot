# knot Helm chart

Production install of [knot](https://github.com/trevex/knot) — a self-hosted, CRDT-backed collaborative knowledge base.

## Prerequisites

- Kubernetes 1.27+
- Helm 3.13+
- External Postgres 18 (the chart does **not** bundle one). The connection user needs `CREATE TABLE` rights in its database.
- An ingress controller (e.g. ingress-nginx, traefik) if you want external access.
- Optional: cert-manager for TLS, an OIDC provider (Dex, Keycloak, Okta, ...) if you want SSO.

## Quick install

The release workflow publishes the chart as an OCI artifact to GitHub Container Registry, so
you can install a released version directly — no `helm repo add` needed (Helm 3.8+):

```bash
helm install knot oci://ghcr.io/christianhuening/charts/knot \
  --version 0.3.0 \
  --create-namespace --namespace knot \
  --set database.url='postgres://knot:knot@db.svc.cluster.local:5432/knot' \
  --set session.key="$(openssl rand -base64 32)" \
  --set baseUrl=https://knot.example.com \
  --set ingress.enabled=true \
  --set ingress.hosts[0].host=knot.example.com \
  --set ingress.hosts[0].paths[0].path=/ \
  --set ingress.hosts[0].paths[0].pathType=Prefix
```

`image.repository`/`image.tag` no longer need to be set — they default to
`ghcr.io/christianhuening/knot` at the chart's appVersion. To install from a local checkout
instead, point Helm at the directory: `helm install knot ./deploy/helm/knot ...`.

If the ghcr packages are private, run `helm registry login ghcr.io` before pulling the chart,
and set `--set image.pullSecrets[0].name=<secret>` so the cluster can pull the image.

The chart will:

1. Run `knot-server migrate` as a `pre-install` Helm Job to apply pending schema migrations.
2. Roll out the `knot-server` Deployment.
3. Expose it via a ClusterIP Service (port 80 → container 3000).
4. Optionally create an Ingress.

`helm test knot` runs a curl pod that hits `/api/healthz` to confirm the rollout is live.

## Using an external Secret

Production deployments should hold credentials in an externally-managed Secret (e.g. via SOPS, External Secrets, or a managed KMS).

Create a Secret with these keys:

| Key | Required when | Value |
|-----|---------------|-------|
| `KNOT_DATABASE_URL` | always | Postgres connection URL |
| `KNOT_SESSION_KEY` | always | 32-byte random key |
| `KNOT_OIDC_CLIENT_SECRET` | OIDC enabled | OIDC client secret |

Then point the chart at it:

```yaml
database:
  existingSecretName: knot-secrets
  existingSecretKey: KNOT_DATABASE_URL
session:
  existingSecretName: knot-secrets
  existingSecretKey: KNOT_SESSION_KEY
oidc:
  enabled: true
  existingSecretName: knot-secrets
```

> When `existingSecretName` is set the chart will **not** render its own Secret — you take full responsibility for providing all required `KNOT_*` keys.

## OIDC

> **First run:** knot needs a workspace before anyone can log in. Bootstrap one
> owner **before the first OIDC login** with `knot-server admin create`
> (or `POST /auth/setup`). If you OIDC-login first, knot returns
> `auth.not_initialized` until you bootstrap. `admin create` also works later as
> a break-glass local admin.

`knot-server` discovers the provider via OIDC issuer URL and uses Authorization Code + PKCE.

```yaml
oidc:
  enabled: true
  issuer: https://idp.example.com
  clientId: knot
  clientSecret: changeme            # or set existingSecretName above
  redirectUrl: https://knot.example.com/auth/oidc/callback
  autoProvision: domain              # off | always | domain | group
  allowedDomains: "example.com,example.org"
  roleFromGroups: '{"engineers":"editor","admins":"owner"}'
  extraAudiences: ""                 # comma-separated; see "Extra audiences" below
```

Tested IdPs:
- **Dex** with the `password` connector (the dev-compose setup at `deploy/compose/dex/`).
- Any OIDC-conformant provider exposing `openid email profile groups` (Keycloak, Okta, Auth0, Google).
- **Zitadel** — see "Extra audiences" below.

### Extra audiences

Per [OIDC Core §3.1.3.7](https://openid.net/specs/openid-connect-core-1_0.html#IDTokenValidation),
an ID token is rejected if it lists an audience the client does not trust. Some
IdPs add audiences beyond the client id: **Zitadel**, for example, puts the
**project id** — and sometimes several other numeric ids — in `aud` next to the
client id, which makes login fail with `auth.oidc.exchange_failed` (the server
log shows `Invalid audiences: \`<id>\` is not a trusted audience`).

`oidc.extraAudiences` is a comma-separated list of **regex patterns**, each
matched against the whole audience. A bare id matches only itself; `\d{18}`
matches any 18-digit id. Trusting these is safe — the client id must still be
present in `aud` and Zitadel's `azp` must still equal it. For Zitadel, match any
snowflake id rather than chasing them one by one:

```yaml
oidc:
  extraAudiences: '^\d{18}$'   # trust any 18-digit Zitadel id (project, grants, …)
```

## Archive retention

Deleting a document moves it, with its subpages, to the archive. Editors
can open and restore archived documents; owners can also delete them
permanently ahead of time. An hourly job on one replica (Postgres advisory
lock) permanently deletes whatever has been in the archive for longer than
`archive.retentionDays` (default 30, `KNOT_ARCHIVE_RETENTION_DAYS`),
including the documents' attachments in the S3 bucket.

```yaml
archive:
  retentionDays: 30
```

Permanent deletion covers the live database and blob storage only. Database
backups and point-in-time recovery keep purged content for as long as you
keep them; size their retention accordingly if you rely on deletions
taking effect.

## S3 blob backend

knot stores attachments in either Postgres `bytea` (default, 10 MB hard cap, no extra infrastructure) or an S3-compatible bucket (recommended for workspaces with more than a few hundred attachments). The backend is selected at runtime — the same image handles both — and you can switch by changing `blob.backend` and rolling the deployment.

```yaml
blob:
  backend: s3
  s3:
    bucket: knot-blobs
    region: eu-central-1
    endpoint: ""                # empty for native AWS S3
    prefix: ""                  # optional key prefix
    existingSecretName: knot-aws-creds
```

Built on the `rust-s3` crate, so any S3-compatible provider works: AWS S3, **MinIO**, **Cloudflare R2**, **Backblaze B2**, **Wasabi**, **Hetzner Object Storage**, etc. For non-AWS providers set `endpoint` to the provider's S3 endpoint URL.

When `blob.backend=s3`, the chart writes the non-secret S3 config (bucket, endpoint, region, prefix) into the ConfigMap. AWS credentials should come from a Secret you maintain separately. The chart `envFrom`s that Secret when `blob.s3.existingSecretName` is set. Expected keys:

| Key | Purpose |
|-----|---------|
| `AWS_ACCESS_KEY_ID` | static access key |
| `AWS_SECRET_ACCESS_KEY` | static secret |
| `AWS_SESSION_TOKEN` | optional, for STS / IRSA |

EKS users with IRSA: knot uses static creds out of the box. For IRSA, expose the role's STS-derived credentials via a tool like `eks-iam-injector` that writes them to env, or use a sidecar that refreshes the secret.

The credentials need `s3:DeleteObject` as well as read and write: knot deletes the files of the archived pages it purges (with "Delete permanently" or after the retention period), and the hourly job deletes the files of a Duplicate that didn't finish. When the bucket refuses a delete, the purge keeps the page and the hourly job tries again; until it gets through, "Delete permanently" fails for that page and it can't be restored.

## Upgrading

```bash
helm upgrade knot ./deploy/helm/knot -n knot -f my-values.yaml
```

The `pre-upgrade` Job re-runs `knot-server migrate` before the new pods land, so any new migrations in the release are applied first. Failed migrations halt the upgrade — fix the cause and re-run.

Before rolling out a release that changes page export, run `knot-server export-check` against a staging copy of the production database: it exports every page to DOCX and ODT offline (no migration, no writes), prints the fallback counts, and exits 1 if any page fails.

Before rolling out a release that changes the Markdown writer, page order keys or the task list, run the real-data compatibility check from a source checkout against a restored backup or staging copy, taken before the upgrade or after it: `KNOT_COMPAT_DATABASE_URL=postgres://… cargo test -p knot-server --test markdown_compat -- --ignored --nocapture`. It connects read-only and runs no migrations. For every live page it compares the Markdown export and the `/tasks` listing of the release before with this one's, and reports the pages that change by bug class, with the first diff of each. It also reports the page-order groups the upgrade migration repairs, how many pages the background refresh renders again, and how many pages have a parent in another workspace, with the first few of them. Earlier releases let a create, move or import write such a link; this release refuses new ones but leaves existing ones as they are, so they are reported only and don't fail the check. It also lists, reported only, the pages nested deeper than this release follows: those whose export writes a part nested more than 128 levels of quotes, list items and table cells as one paragraph of text, and those nested so deep that deleting their deep part (in the editor, or by a Markdown replace or a history restore) can still overflow the server's stack, as in earlier releases. It fails if a page that exported correctly before, or a task that was listed correctly, would change outside the named bug classes, if a page whose export changes would lose any of its text, if a page's new export fails, if the `/tasks` listing would no longer list one task per checkbox, or if a page can't be read (its stored state doesn't decode, or the database user lacks a privilege the check needs), since that page wasn't compared; the report lists the failing pages.

### Upgrading from 0.8.1

The `pre-upgrade` Job applies five migrations:

- `20261023120000_user_last_seen_version` records the release each user last saw, for the "knot was updated" card.
- `20261024120000_heal_sort_keys` repairs page order keys where two pages at one level share a key, or a key is invalid or over 64 characters, keeping the order the sidebar shows, and then makes keys unique at the top level too (before, two top-level pages could share a key). It rewrites only the sibling groups that need it, changes nothing else about those pages, and rewrites no key on a database that needs no repair.
- `20261024120100_markdown_renderer_version` records which Markdown writer made each cached render, and marks every existing one as the old writer's.
- `20261024120200_blob_copy_pending` adds the table where Duplicate records the files it copies to S3 until its copy commits. The hourly purge job deletes copies that never committed.
- `20261025120000_markdown_sweep_attempts` adds the table where the one-time refresh below notes each page it starts rendering. A page whose render never finishes (it panicked, or the process died during it) is tried once more and then skipped until someone edits it, so a page that crashes the server can't crash every replica on every start.

After the new pods start, one replica (Postgres advisory lock) refreshes, once, the cached Markdown and the task listing of every page outside the archive, about ten pages every two seconds (at most some 18,000 an hour), without opening the pages for editing. Search, share links, zip export and My Tasks catch up as it runs. A healthy page's Markdown doesn't change, and its tasks keep their place in My Tasks. Once it finds no more pages it looks again every 10 minutes, which also picks up renders that old pods write during the rollout.

During the rollout, pods still on 0.8.1 can answer `409 doc.sort_key_conflict` when someone creates or moves a page, because the new constraint also refuses the repeated top-level keys they can write. A retry can succeed once a new pod answers it. Don't run workspace imports until every pod runs the new version.

With the S3 blob backend, make sure knot's credentials allow `s3:DeleteObject` (see [S3 blob backend](#s3-blob-backend)). Earlier releases ignored a refused delete when they purged an archived page, deleting the page and leaving its files in the bucket; this release keeps the page and retries.

## Operational notes

- **Healthchecks** — readiness `/api/readyz`, liveness `/api/healthz`. Disable with `probes.enabled=false` if you want to debug a wedged pod.
- **Image size** — the runtime image is `scratch` + a static musl binary (mimalloc allocator). Expect ~20 MB.
- **Resources** — defaults `100m/128Mi` requests, `1000m/512Mi` limits. Right-size based on workspace volume.
- **Replicas** — knot's CRDT room actor is per-pod and discovers peers via Postgres `LISTEN/NOTIFY`, so `replicaCount > 1` works without sticky sessions. Each room is hosted by exactly one pod at a time; failover takes one TCP roundtrip.
- **Request body size** — image uploads and Word/OpenDocument exports (which upload the page's diagram pictures) send up to 10 MiB. A reverse proxy or ingress in front of knot must allow request bodies of at least 10 MiB. With ingress-nginx (default 1 MiB), set `nginx.ingress.kubernetes.io/proxy-body-size: 10m` in `ingress.annotations`.

## Uninstall

```bash
helm uninstall knot -n knot
```

The chart does not delete data — drop the Postgres database manually if you want a clean wipe.
