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
| `KNOT_SESSION_KEY` | always | 32-byte random key. Rotating it logs everyone out and invalidates every API key |
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

### Upgrading from 0.9.8

The `pre-upgrade` Job applies two migrations, each in its own transaction. The first, `20261104120000_history_keep_updates`, makes the stored edit log (`doc_updates`) append-only and copies every stored snapshot into the new `doc_history_baselines` table. Check `SELECT pg_size_pretty(pg_total_relation_size('doc_snapshots'))` beforehand: the copy takes about as long as reading and writing that much data (for orientation, some 261 MB took 0.87 s on a local SSD; network disks are slower). While the copy runs, saving edits waits until it finishes, and so do creating, renaming, moving, archiving, restoring, duplicating and purging pages, making a page a template and restricting a page. On a busy instance the waiting saves can fill each old pod's database pool, so pages may stop loading and pods may briefly go NotReady until the copy finishes. When `doc_snapshots` is larger than a few hundred MB, upgrade in a quiet window. The second, `20261104120001_history_since`, adds `documents.history_since`. It needs an exclusive lock on `documents`: each attempt to get it pauses all page access, page reads and edit saves included, for at most 100 ms.

Pass `helm upgrade --timeout` of at least the expected copy time plus 6 minutes (Helm waits 5 minutes by default). The Job runs three attempts: with `restartPolicy: OnFailure` and `backoffLimit: 3`, Kubernetes stops the fourth container start as soon as the limit is reached. In the worst case that still succeeds, the migrations spend 4 × 45 s in their lock loops (two attempts give up, and the third uses both loops' full 45 s), the restart backoff between attempts adds about 30 s, and the Job pod's image pull and container starts come on top. Don't re-run the upgrade while the migrate Job is still running: the Job's `before-hook-creation` delete policy deletes the running Job, and the copy starts over. Each migration takes its locks in attempts 0.5 s apart; an attempt of the first pauses page writes for at most 100 ms, an attempt of the second all page access. An ordinary autovacuum of `doc_updates` or `documents` in the way is cancelled: the migration waits for that table's lock for `deadlock_timeout` plus 0.5 s (1.5 s by default), at most every 5 s per autovacuum worker and table, and writes to that table (for the second migration, all access to `documents`) queue meanwhile. If a migration doesn't get its locks within 45 s, it fails with a message naming up to three sessions that hold them, oldest transaction first among those the Job's role can see, and the Job retries; after the last attempt the upgrade halts. Don't upgrade while `pg_dump` or another long transaction runs on the database, or during an anti-wraparound autovacuum, which Postgres never cancels: each can hold the tables for longer than the attempts wait. To see what holds them now, run as a superuser or a role with `pg_read_all_stats` (other roles' sessions are hidden otherwise) in the knot database: `SELECT a.pid, a.backend_type, l.mode, l.granted, a.state, a.xact_start, a.query FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid WHERE l.database = (SELECT oid FROM pg_database WHERE datname = current_database()) AND l.relation IN ('documents'::regclass, 'doc_updates'::regclass) ORDER BY l.granted DESC, a.xact_start`. Rows with `granted` false are waiting, not blocking; the oldest granted ones are the usual culprits. An autovacuum whose query contains `wraparound` is an anti-wraparound one.

To keep the migrate Job's error, follow its log in a second terminal while the upgrade runs: `kubectl -n knot logs -f job/knot-migrate` (the Job is the Deployment's name plus `-migrate`, so `knot-migrate` for the `helm upgrade knot` above; see the naming rule in [Operational notes](#operational-notes)). Run it again after each container restart, or rely on your cluster's log collection: once the Job gives up (Helm reports `BackoffLimitExceeded`), Kubernetes deletes its pod, and the log with it, and the Job itself goes 10 minutes later (`ttlSecondsAfterFinished: 600`). After that the query above shows only what holds the locks now. If the upgrade halts, `SELECT version FROM _sqlx_migrations WHERE version >= 20261104120000` shows how far it got. If the migrate Job is still running (`kubectl -n knot get jobs`), let it finish first.

- No rows: the first migration gave up and nothing was applied. 0.9.8 keeps working, pod restarts included. Find what held the locks (the Job's error names up to three holders, if you followed its log; the query above shows what holds them now), and re-run the upgrade at a quieter time.
- Only `20261104120000`: the second migration gave up, usually behind a long transaction or an anti-wraparound autovacuum on `documents`. The running 0.9.8 pods keep serving, but any that restarts crash-loops: 0.9.8 doesn't start on a database with migrations it doesn't know. End the blocker and re-run the upgrade promptly; its Job applies the second migration, and the new pods replace the old ones.
- `20261104120000` and `20261104120001`: the migrations are done. This is the usual result when Helm's timeout fired while the migrate Job still ran: Helm marks the upgrade failed and rolls out no new pods, but the Job keeps running and commits. 0.9.8 pods that restart crash-loop here too. Re-run the upgrade now: its Job has nothing left to apply, and the new pods replace the old ones.

Rolling back to 0.9.8 needs a database restore once either migration has committed, for the same reason. During a normal upgrade, a 0.9.8 pod that restarts after the first migration has committed crash-loops until a new pod replaces it.

Knot no longer deletes old edits, so the database grows with every edit (see "Storage growth from page history" in [Operational notes](#operational-notes)). During the rolling upgrade, pods still on the old version log a `doc_updates is append-only` warning from their hourly cleanup; that is expected and harmless.

## Operational notes

- **Healthchecks** — readiness `/api/readyz`, liveness `/api/healthz`. Disable with `probes.enabled=false` if you want to debug a wedged pod.
- **Image size** — the runtime image is `scratch` + a static musl binary (mimalloc allocator). Expect ~20 MB.
- **Resources** — defaults `100m/128Mi` requests, `1000m/512Mi` limits. Right-size based on workspace volume.
- **Replicas** — knot's CRDT room actor is per-pod and discovers peers via Postgres `LISTEN/NOTIFY`, so `replicaCount > 1` works without sticky sessions. Each room is hosted by exactly one pod at a time; failover takes one TCP roundtrip.
- **Notifications** — every replica checks for overdue tasks and deletes old notifications every 15 minutes, with nothing to configure and no coordination needed between replicas. Read notifications are deleted after 90 days, all after 180, and a page's go with it when it is deleted for good. They hold short excerpts (comment text, task text), so an excerpt of a deleted comment stays in its recipients' inboxes until then. `knot_notifications_emitted_total{kind}` counts them as they are written.
- **Request body size** — image uploads and Word/OpenDocument exports (which upload the page's diagram pictures) send up to 10 MiB. A reverse proxy or ingress in front of knot must allow request bodies of at least 10 MiB. With ingress-nginx (default 1 MiB), set `nginx.ingress.kubernetes.io/proxy-body-size: 10m` in `ingress.annotations`.
- **Storage growth from page history** — the edit log is kept forever, so size it as rows/day × average row bytes × ~1.6 (tuple, index and TOAST overhead). Backups and point-in-time recovery grow at the same rate. `knot-server history-stats [--days N]` (read-only, no migration; `--days` defaults to 30, minimum 1) prints the inputs. It needs the same `KNOT_*` environment as the server (config validation requires `KNOT_SESSION_KEY`, for one), so run it inside a knot pod: `kubectl exec -n knot deploy/knot -c knot -- /knot-server history-stats`. The Deployment is named as `knot.fullname` in `templates/_helpers.tpl` computes it: `fullnameOverride` when it is set; otherwise `<release>` when the release name contains the chart's name (`nameOverride` when set, else `knot`), as with `helm upgrade knot` above, and `<release>-<name>` when it doesn't; cut to 63 characters, then a trailing `-` removed. Its per-day rates cover the last `--days` days, and before the upgrade the old cleanup deleted edits, so run it at least 30 days after upgrading, or pass a `--days` below the days since the upgrade. To get rows/day, run it twice, some days apart: the difference in `update_rows` divided by the days between the runs is rows/day, and `update_bytes_avg` is the average row bytes. It also prints the current size of `doc_updates`.

## Uninstall

```bash
helm uninstall knot -n knot
```

The chart does not delete data — drop the Postgres database manually if you want a clean wipe.
