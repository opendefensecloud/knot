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

## Rate limits and quotas

knot limits how fast one signed-in user can write, and how fast one client
address can sign in or open share links. The limits are abuse guards, sized
far above normal use (seeding a 40-page tree, dropping many files, an hour on
a board, a morning sign-in rush); owners are not exempt. Many clients behind
one NAT or egress proxy share one address and its limits (see
[Client IP behind a proxy](#client-ip-behind-a-proxy)).

```yaml
rateLimit:
  enabled: true          # KNOT_RATE_LIMIT_ENABLED; false turns off every HTTP and collab rate limit
  scale: 1               # KNOT_RATE_LIMIT_SCALE; multiplies every number below; above 0, at most 100
  collabMaxSockets: 64   # KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS; open collab sockets per user per replica; at least 1
trustedProxies: []       # KNOT_TRUSTED_PROXIES; see "Client IP behind a proxy" below
quotas:                  # 0 = off
  pagesPerUser: 0            # KNOT_QUOTA_PAGES_PER_USER
  pagesPerWorkspace: 0       # KNOT_QUOTA_PAGES_PER_WORKSPACE
  blobBytesPerUser: 0        # KNOT_QUOTA_BLOB_BYTES_PER_USER
  blobBytesPerWorkspace: 0   # KNOT_QUOTA_BLOB_BYTES_PER_WORKSPACE
```

### HTTP limits

Each limit is a token bucket: a burst of requests, refilled at a steady rate
(for every HTTP bucket, the burst and the per-minute rate are the same
number). Every limited request takes one token from exactly one bucket:

| Bucket | Per | Requests | Burst and per minute |
|---|---|---|---|
| `expensive` | user | duplicate a page, workspace import, change password | 10 |
| `export` | user | workspace zip export, page (and subtree) zip export, DOCX/ODT export | 20 |
| `upload` | user | file uploads | 60 |
| `board_preview` | user | board preview images (sent shortly after every board change) | 300 |
| `write` | user | every other `POST`, `PUT`, `PATCH` and `DELETE` under `/api` | 120 |
| `sign_in` | client IP | `/auth/login`, `/auth/setup` | 30 |
| `public` | client IP | share links (`/p/…`), their files and board images | 600 |

Reads (page loads, search, history), health and version checks, static
files, `/auth/session`, `/auth/config`, `/auth/logout`, the OIDC sign-in
routes and short links are not limited. `rateLimit.scale` multiplies every
burst and rate, here and on the collab WebSocket below; no scale takes a
bucket below one request a minute. Set a fractional scale in a values file
or with `--set-json rateLimit.scale=0.5`: `--set rateLimit.scale=0.5` passes
the string `"0.5"`, which the chart's schema refuses.
A request over its limit gets `429 Too Many Requests` with a `Retry-After`
header and the body
`{"error":{"code":"rate.limited","message":"too many requests","details":{"retry_after_secs":N}}}`;
the web app shows "You're doing that too fast" with the wait. Requests
refused for a missing session or a failed CSRF check cost nothing. API
scripts that write in bulk should read `Retry-After` and wait.

### Collab WebSocket limits

The editor's and the board's WebSocket never refuse or drop an edit. When a
user sends more than their budget, the server waits before reading that
user's next message (backpressure), so their edits arrive later, all of them,
and in order; the client keeps receiving meanwhile. Pages and boards have
separate budgets per user and replica: pages 32 MiB burst then 1 MiB/s and
1 000 messages burst then 300/s; boards 64 MiB then 4 MiB/s and 2 000 then
600/s. Typing stays far below that. Dragging many board elements at once
(an estimated 5 MB/s for 100 elements) uses up the board burst only after
a minute or more, since the budget refills at 4 MiB/s meanwhile, and the
drag then slows, but nothing fails. A sustained paste flood shows
"Saving…" until the server has read it. Presence (cursors) beyond 120
updates a second per user is coalesced: the latest position always arrives. A client's
repeated request for the page's full state on one socket is ignored.

A user may hold `rateLimit.collabMaxSockets` (default 64) collab sockets
open per replica, one per open page or board tab, and may connect to one
page or board 20 times, then 20 times a minute. Past either, the upgrade
gets 429 and the client backs off and retries, as after any failed connect.
With rate limits on, knot pings every collab socket every 30 s and ends a
session it has heard nothing from (not even the pong browsers send by
themselves) for 75 s, so a socket left half-open by a laptop that slept or
a network change frees its place within about 75 s, behind a proxy too.

A socket waiting off its user's debt doesn't read meanwhile, so it reads the
client's Close only once the wait ends, unless a write to the client fails
first (a revoke, a refused update, the room ending or server shutdown end
the wait at once). A socket reads only once its user's budget is out of
debt, so on its own it owes at most the one frame it read last, at most
4 MiB: its wait is bounded by the time that frame takes to refill, about
4 s for a page and 1 s for a board at the default scale, longer at a lower
scale, and up to 64 s at the lowest byte rate (64 KiB/s). When the same
user floods on several sockets at once, their frames add up, and the wait
grows with the number of those sockets (some 32 s for eight page sockets
at the default scale). In `knot_collab_throttle_wait_seconds`, waits well
past those single-socket bounds mean one user flooding on several sockets.
Until a wait ends, that socket keeps its place under `collabMaxSockets`,
so a user who floods and reloads many tabs can reach the socket cap for
that long.

Keep the idle and read timeouts of every proxy in front of knot above one
minute (ingress-nginx defaults to 60 s; `nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"`
is a common choice for WebSockets): a socket closed by the proxy during a
wait reconnects, and nothing is lost while the tab is open, but the editor
shows the reconnect.

### Client IP behind a proxy

The `sign_in` and `public` buckets, the login and password failure throttles
and the IP recorded on a password sign-in's session use the client's IP.
knot has to be told which proxies to believe:

```yaml
trustedProxies:          # IPs or CIDRs; a list or one comma-separated string
  - 10.42.0.0/16         # the ingress controller's pod CIDR, for example
```

- **Empty (the default):** the client is the TCP peer. Behind an ingress
  that is the ingress pod, the same for everyone, so when a request carries
  `X-Forwarded-For` knot **skips** the per-IP buckets for it instead of
  sharing one bucket among all clients, logs a warning once per process and
  counts it in `knot_client_ip_unresolved_total{reason="no_trusted_proxies"}`.
  The login and password failure throttles then still key on the proxy's
  address, as in earlier releases: failed logins from anyone count together.
- **Set:** for a request from a trusted peer, knot reads `X-Forwarded-For`
  from the right, skipping trusted addresses; the first untrusted one is the
  client. A peer outside the list is the client whatever the header says, so
  nobody else can choose their bucket: when it sends the header anyway,
  knot logs a warning once per process and counts the request in
  `knot_client_ip_unresolved_total{reason="untrusted_peer"}`. That can be a
  proxy left out of the list, or a direct client sending the header itself.
  IPv6 clients are grouped per `/64`. A malformed (or empty) header entry
  from a trusted proxy stops the walk at the last valid address and is
  counted in `knot_client_ip_malformed_forwarded_total`.

Both counters count only while rate limits are on (`rateLimit.enabled`).

**Without `X-Forwarded-For`, every client shares one address.** Behind an L4
(TCP) load balancer or proxy that adds no `X-Forwarded-For`, or a node port
that rewrites the source address, the TCP peer is the balancer or the node
for everyone, and nothing is counted as unresolved: all clients then share
one `sign_in` bucket (30 sign-ins a minute) and one `public` bucket, and
failed logins from anyone count together. The same holds when
`trustedProxies` lists one hop but not the ingress in front of it, which
then becomes the client. Fix it at the balancer: keep the client address
(PROXY protocol into the ingress controller, or `externalTrafficPolicy:
Local` on its Service) and let the ingress add `X-Forwarded-For`, then list
every proxy hop in front of knot in `trustedProxies`.

**Clients behind one NAT or egress proxy share an address too.** knot sees
the one public address an office's NAT or outbound proxy uses, so even with
`trustedProxies` set correctly every client behind it counts against one
`sign_in` bucket (a burst of 30 password sign-ins and setups, then 30 a
minute) and one `public` bucket (600 share-link requests a minute). When
more people than that sign in with a password at once, say 150 within two
minutes as an office starts its day, about 90 get in and the rest get
"You're doing that too fast" with a wait of a few seconds. Five failed
logins from anyone behind the address also hold up password sign-in for
all of them, as before. OIDC sign-in is not limited, and sessions last, so
only password sign-ins of people who aren't signed in count. The only way
to raise the per-IP buckets is `rateLimit.scale`, which raises every bucket
with them (it doesn't change the failed-login throttle); each replica has
its own buckets (see Replicas under [Operational notes](#operational-notes)).

List **only** the addresses of your ingress controller (its pods, or the
nodes when it runs with host networking): any address you list can name any
client IP it likes. `0.0.0.0/0`, `::/0` and every other prefix of length 0
are refused. The chart's schema also refuses characters outside IPs and
CIDRs; knot checks the rest when it starts and refuses to start, with an
error naming `KNOT_TRUSTED_PROXIES` and the entry, on a malformed entry,
host bits after the prefix (`10.0.0.5/8`: write `10.0.0.0/8`, or the bare
address for one proxy) or an IPv4-mapped network shorter than `/96`
(`::ffff:10.0.0.0/104` is read as `10.0.0.0/8`). When you list a pod CIDR,
which other pods share, also enable `networkPolicy` and restrict
`networkPolicy.ingressFrom` to the ingress controller:

```yaml
networkPolicy:
  enabled: true
  ingressFrom:
    - namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: ingress-nginx
      podSelector:
        matchLabels:
          app.kubernetes.io/name: ingress-nginx
```

### Import caps

Always on: one workspace import may create at most 10 000 pages, write at
most 256 MiB of page Markdown and 512 MiB of attachments and board files,
and hold at most 20 000 attachment and board references, checked from the
archive's manifest and declared sizes (a file named by two pages counts
twice) before anything is written. A larger import is refused with
`413 import.too_large` (details `pages`, `markdown_bytes`, `bytes`,
`references` and their `max_*`); split it, or import it in parts. The upload itself is at most 50 MiB compressed. Entries larger than
they declare, or than 32 MiB, are skipped, with one warning per import
(`import: zip entries skipped`, field `skipped_entries`).

### Quotas

Off by default. When set, knot refuses a write that would take a user or the
workspace past its quota with `403 quota.exceeded`, and the web app says
which limit was reached:

- `quotas.pagesPerUser` / `quotas.pagesPerWorkspace` count pages: those the
  user created, or all of the workspace's. Archived pages and templates count
  until they are purged, so restoring a page never goes over a quota.
- `quotas.blobBytesPerUser` / `quotas.blobBytesPerWorkspace` count the bytes
  of uploaded files: those the user uploaded, or all of the workspace's.
  Files count until they are purged. Board preview images don't count.
- Duplicated pages and their copied files count for the user who
  duplicated them. An import counts all its pages and declared attachment
  bytes (not its board files) at once, before it writes anything, and is
  refused whole.

A workspace quota without a per-user one lets one account use all of it:
set both. Quotas are counted in Postgres and hold across replicas, but they
are best effort: requests that run at the same time can each pass the check,
so a quota can be overshot by what runs at once. Each refusal is counted in
`knot_quota_exceeded_total{quota,scope}`; it is audited as `quota.exceeded`
(with the limit, the usage and the request) and logged as `quota exceeded`
at most once per user, quota and scope every 10 minutes on each replica.

### What the limits don't cap

Quotas cover pages and files only, and only when set. Without them, one user
can upload 60 files of up to 10 MiB a minute per replica, and duplicates and
imports fill storage faster still: they share the `expensive` bucket (10 a
minute), and each duplicate copies up to 200 pages and 200 MiB of files,
each import up to 10 000 pages and 512 MiB of files and board images (20 000
references). Set `quotas.*` if that matters. What one user can still add, per
replica, at the default limits, whatever the quotas:

- **Page edits** — up to 1 MiB/s after a 32 MiB burst on page sockets, kept
  forever in the edit log (see "Storage growth from page history" in
  [Operational notes](#operational-notes)).
- **Board edits** — up to 4 MiB/s after a 64 MiB burst on board sockets.
- **Board previews** — 1 MiB each, 300 a minute, but each overwrites the
  board's one preview, so they don't accumulate.
- **Other writes** (comments, titles, tasks, grants, …) — 120 a minute, each
  at most the 2 MiB request body.

Those budgets guard CPU and the fan-out between replicas, not storage: watch
`knot_history_table_bytes` and database size for growth.

### Metrics

- `knot_rate_limited_total{bucket}` — requests refused with 429, by bucket
  (above), plus `collab_upgrade` and `collab_sockets` for refused WebSocket
  upgrades. Normal use keeps it at 0; the `KnotRateLimitedSustained` alert
  (`alerting.rateLimitedPerSecond`, default 1 refusal a second per bucket,
  15-minute average over all replicas, for 30 minutes) fires on a sustained
  rate. The server logs a warning when a user or client starts being
  limited, not one per refused request: `rate limit reached; refusing with
  429 until the bucket refills` (with `bucket` and the user or client),
  `too many open collab sockets for one user; refusing upgrades with 429`
  and `collab socket reconnecting to one target too fast; refusing upgrades
  with 429`.
- `knot_collab_throttled_total{kind,action}` and the histogram
  `knot_collab_throttle_wait_seconds` — collab messages delayed by
  backpressure, presence updates coalesced and repeated full-state requests
  ignored, and how long the waits last.
- `knot_quota_exceeded_total{quota,scope}` — quota refusals.
- `knot_client_ip_unresolved_total{reason}` — requests to a per-IP limited
  route from an untrusted peer that carried `X-Forwarded-For`.
  `reason="no_trusted_proxies"`: `trustedProxies` is empty and knot is behind
  a proxy; set `trustedProxies` when it isn't 0. `reason="untrusted_peer"`:
  the peer isn't in the list, a proxy left out or a client sending the
  header itself. Counted only while rate limits are on.
- `knot_client_ip_malformed_forwarded_total` — requests from a trusted proxy
  whose `X-Forwarded-For` held a malformed entry.

The Grafana dashboard (`deploy/grafana/knot.json`) shows them in its "Limits"
row.

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

### Upgrading from 0.9.9

Upgrade to 0.9.10 from 0.9.9, and don't skip 0.9.10 on the way to a later release: it is the release that starts numbering and labelling every stored edit, and later releases rely on that. The `pre-upgrade` Job applies five migrations, each in its own transaction except the index build (the fourth). While they run, 0.9.9 pods keep serving and saving edits.

1. `20261107120000_history_log_columns` adds columns to `doc_updates` and `doc_snapshots`. It needs an exclusive lock on both: each attempt to get it pauses all access to them, page loads and edit saves included, for at most 100 ms. Once it has the lock it makes catalog-only changes (no scan, no rewrite) and commits at once.
2. `20261107120001_history_heads` creates `doc_history_heads`, one row per page, and counts each page's stored edits into it. It locks `doc_updates` against writes (`SHARE ROW EXCLUSIVE`) for the whole count: page loads go on, but edit saves and duplicating pages wait until the count commits, and on 0.9.9 pods a task toggle, an accepted suggestion, an import or a restore holds up its open page meanwhile. Each waiting save holds a database connection, so on a busy instance the waiting saves can fill each 0.9.9 pod's database pool, after which pages may stop loading and API calls fail until the count commits. Size the pause by the table, not by its index: the count may use an index-only scan, but nothing guarantees that plan, and parts of the table the visibility map does not mark all-visible are read from the heap anyway. Check `SELECT pg_size_pretty(pg_relation_size('doc_updates'))` beforehand, or `EXPLAIN SELECT d.id, count(u.seq) FROM documents d LEFT JOIN doc_updates u ON u.doc_id = d.id GROUP BY d.id` (the migration's count) to see the plan, and when `doc_updates` is large, upgrade in a quiet window.
3. `20261107120002_history_heads_fk` adds the foreign key from `doc_history_heads` to `documents` and a trigger on `doc_snapshots`. It locks `doc_history_heads`, `documents` and `doc_snapshots` against writes for one pass over the per-page rows: meanwhile page writes (creating, renaming, moving, archiving and purging pages, among others), edit saves and snapshot writes pause. Every 0.9.9 save goes through the per-page row, so on 0.9.9 pods an open page waiting on a task toggle, an accepted suggestion, an import or a snapshot is held up too.
4. `20261107120003_history_rev_index` builds a unique index on `doc_updates` with `CREATE UNIQUE INDEX CONCURRENTLY`, while page loads and saves go on. It has no lock loop and no lock timeout: it waits as long as needed behind an anti-wraparound autovacuum or a long transaction on `doc_updates`, so check `pg_stat_activity` for both before upgrading. It also waits for every transaction older than itself, so it does not finish during a `pg_dump`. Don't restart knot pods while it builds: every knot-server start, 0.9.9 or 0.9.10, runs the migrations first and waits on the migration lock (an advisory lock the Job holds for its whole run) with an open snapshot, which the build in turn waits for; Postgres's deadlock detector then aborts one of the two, and if it aborts the build, the Job fails and its retry runs the repair in the fifth migration.
5. `20261107120004_history_rev_index_check` does nothing unless the index build was interrupted or aborted. Then it rebuilds the index under an exclusive lock on `doc_updates`, taken in attempts of at most 100 ms, and all access to page edits, page loads and open 0.9.9 pages included, pauses for one index build of `doc_updates`.

The first, second, third and fifth migration take their locks in attempts 0.5 s apart and give up after 45 s with a message naming up to three sessions that hold them, as in 0.9.9 (see [Upgrading from 0.9.8](#upgrading-from-098)). Their autovacuum handling changed: an ordinary autovacuum in the way is still cancelled, but the migration's wait for it no longer pauses page loads or saves.

Pass `helm upgrade --timeout` of at least the count time plus twice the index build time plus 8 minutes. The count takes about as long as reading `doc_updates` once, and the index build longer: it reads the table, sorts and writes the index. The index may be built twice: an aborted concurrent build, then the repair. The Job runs three attempts (as for 0.9.9); in the worst case that still succeeds, the four lock loops spend up to 45 s each, two attempts give up after 45 s each, the restart backoff between attempts adds about 30 s, and the Job pod's image pull and container starts come on top.

Before upgrading, check:

- No `pg_dump`, other long transaction or anti-wraparound autovacuum runs on the database (`SELECT pid, backend_type, xact_start, state, query FROM pg_stat_activity WHERE xact_start < now() - interval '1 minute' ORDER BY xact_start`; an autovacuum whose query contains `wraparound` is an anti-wraparound one, which Postgres never cancels).
- The size of `doc_updates`, to size the count and the index build (above).

To see what holds the locks now, run as a superuser or a role with `pg_read_all_stats` (other roles' sessions are hidden otherwise) in the knot database: `SELECT a.pid, a.backend_type, l.relation::regclass, l.mode, l.granted, a.state, a.xact_start, a.query FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid WHERE l.database = (SELECT oid FROM pg_database WHERE datname = current_database()) AND l.relation IN (to_regclass('documents'), to_regclass('doc_updates'), to_regclass('doc_snapshots'), to_regclass('doc_history_heads')) ORDER BY l.granted DESC, a.xact_start`. Rows with `granted` false are waiting, not blocking; the oldest granted ones are the usual culprits.

Don't re-run the upgrade while the migrate Job is still running (`kubectl -n knot get jobs`): the Job's `before-hook-creation` delete policy deletes the running Job, and its index build goes on in the database, orphaned. Follow the Job's log during the upgrade as described for 0.9.8. With `migrations.enabled=false`, run `knot-server migrate` to completion before rolling out the new pods: otherwise every new pod runs the migrations at startup, before `/api/healthz` answers, and the liveness probe can kill one in the middle of the index build.

If the upgrade halts, `SELECT version FROM _sqlx_migrations WHERE version >= 20261107120000` shows how far it got:

- No rows: the first migration gave up and nothing was applied. 0.9.9 keeps working, pod restarts included. Find what held the locks and re-run the upgrade later, at a quieter time.
- Some, but not all five: 0.9.9 pods keep serving, but any that restarts crash-loops, since 0.9.9 doesn't start on a database with migrations it doesn't know. End the blocker and re-run the upgrade promptly. First check `SELECT pid, phase FROM pg_stat_progress_create_index` (or `pg_stat_activity` for a query `CREATE UNIQUE INDEX CONCURRENTLY`): a killed Job can leave the index build running in the database, still holding the migration lock. Wait for it to finish, or end it with `SELECT pg_terminate_backend(<pid>)`; otherwise the re-run hangs on that lock or fails without explanation. An interrupted build is repaired by the re-run's fifth migration.
- All five: the migrations are done (usually Helm's timeout fired while the Job still ran). 0.9.9 pods that restart crash-loop here too. Re-run the upgrade now: its Job has nothing left to apply, and the new pods replace the old ones.

Rolling back to 0.9.9 needs a database restore once the first migration has committed, for the same reason: a 0.9.9 pod that restarts after that crash-loops until a new pod replaces it.

While 0.9.9 and 0.9.10 pods run side by side, editors on a 0.9.9 pod may not see edits saved by other pods while the upgrade runs, especially edits made during the third migration's pause, until they reconnect. Nothing is lost, and the page is complete once it opens on 0.9.10. With many concurrent editors, scale the 0.9.9 Deployment to one replica before upgrading, or pick a quiet window. A page's Markdown export, My Tasks entries and search results may stay stale until the page is next edited. An edit that a 0.9.9 pod failed to save during the rollout (0.9.9 drops a save that fails) may be lost unless its author is still connected and resends it.

After the upgrade, the first time each page edited before it opens, knot numbers the page's stored edits once and replays its whole stored history: that first load is slower, and `doc_updates` grows by up to the size of the renumbered rows until autovacuum reclaims the old row versions. A page that no one opens after the upgrade replays its whole stored history on every Markdown export or refresh, and on a duplicate, until it is first opened.

Two log lines and metrics are new and worth an alert:

- `knot_room_row_apply_errors_total` (label `phase`: `boot` or `catch_up`; log line `a log row failed to apply`, with the doc id): a stored edit of that page cannot be applied. The page still opens, with whatever of it applies, but knot no longer refreshes its boot cache (the stored snapshot), so the page loads more slowly over time. Report it to the maintainers with the doc id.
- `knot_history_legacy_mismatch_total` (error log `legacy rows do not match the count taken at upgrade`, with the doc id): the page's stored edits were changed outside knot after the second migration counted them, and the page will not open. The edits themselves are intact. Report it to the maintainers with the doc id; don't edit the tables by hand.

Edits that cannot be saved during a database outage are now kept and retried in order, also when a pod shuts down: after the HTTP drain, a pod waits up to 10 s more to save edits it still holds. The shutdown budget is `preStopSleepSeconds` (default 5) + 30 s (the HTTP request timeout) + 10 s, plus a few seconds of slack, and the chart's default `terminationGracePeriodSeconds` is now 50, which includes that slack. A shorter value can lose buffered edits when a pod stops during a database outage.

### Upgrading from 0.9.10

Upgrade to 0.9.11 from 0.9.10 with a plain `helm upgrade`: it has no migrations, so the `pre-upgrade` Job finds nothing to apply, and rolling back to 0.9.10 needs no database restore. Coming from 0.9.9, upgrade to 0.9.10 first (see [Upgrading from 0.9.9](#upgrading-from-099)).

### Upgrading from 0.9.10 or 0.9.11

Upgrade to 0.9.12 from 0.9.10 or 0.9.11. Coming from 0.9.9, upgrade to 0.9.10 first and let its rollout finish (see [Upgrading from 0.9.9](#upgrading-from-099)): this release builds page history from the numbered edit log that 0.9.10 starts. The `pre-upgrade` Job applies five migrations, each in its own transaction, in this order, while 0.9.10 or 0.9.11 pods keep serving and saving edits: `20261108120000_history_builder` creates three new, empty tables (`doc_history_state`, `doc_history_checkpoints` and `doc_history_entries`) and their indexes, and copies or rewrites nothing; `20261109120000_doc_short_links` creates the empty `doc_short_links` table; `20261112120000_quota_indexes` builds two indexes for the [quotas](#rate-limits-and-quotas) (see below); `20261112120001_api_keys_users_kind` adds the `users.kind` column and `20261112120002_api_keys_and_bots` creates the empty `api_keys` table and its triggers (both below). Each of the five takes its locks first, in its own bounded lock loop.

The new tables' foreign keys need `SHARE ROW EXCLUSIVE` locks on `documents`, `users` and `doc_history_baselines`, so the migration takes those first, in attempts 0.5 s apart: each attempt waits at most 100 ms for `documents` and asks for the other two without waiting. While an attempt waits, and from when it has the locks until the migration commits (at once, since it only creates empty tables), page writes (creating, renaming, moving, archiving, restoring and purging pages, among others) and writes to `users` (new accounts, a first OIDC sign-in, password changes) pause. Edit saves, snapshot writes and page loads go on. As in 0.9.9 and 0.9.10, an ordinary autovacuum in the way is cancelled, and if the migration doesn't get its locks within 45 s it fails with a message naming up to three sessions that hold them, and the Job retries (see [Upgrading from 0.9.8](#upgrading-from-098)). `20261109120000_doc_short_links` needs `SHARE ROW EXCLUSIVE` on `documents` and `users` for its foreign keys and takes them the same way, with the same pauses, and commits at once. Don't upgrade while `pg_dump`, another long transaction or an anti-wraparound autovacuum runs on the database. Follow the Job's log as described for 0.9.8.

The two API-keys migrations take their locks the same way (`knot_lock_tables`, never waiting for one table while holding another, giving up after 45 s). `api_keys_users_kind` adds the `users.kind` column under an `ACCESS EXCLUSIVE` lock on `users` for an instant, which pauses every statement that touches `users`, sign-ins and edit saves included, and scans nothing (its two checks are added `NOT VALID`). `api_keys_and_bots` validates those checks without blocking anyone, then takes `SHARE ROW EXCLUSIVE` on `users`, `document_grants`, `workspace_members` and `workspaces` while it creates the empty `api_keys` table and its triggers, so grant, member and `users` writes wait until it commits; reads, edit saves and page loads go on.

The `--timeout` needs almost no allowance for data, since only `20261112120000_quota_indexes` reads anything (`documents` and `blobs` once; small tables, see below), but each of the five migrations has a lock loop that waits up to 45 s. A migration that committed is not run again, so in the worst case that still succeeds each loop spends up to 45 s getting its locks once, and two Job attempts each give up after another 45 s in a loop: 7 × 45 s in all, plus about 30 s of restart backoff and the Job pod's image pull and container starts. Pass `helm upgrade --timeout` of at least 7 minutes plus the time your rollout usually takes: Helm's default of 5 minutes is too short for that worst case.

If the upgrade halts, `SELECT version FROM _sqlx_migrations WHERE version >= 20261108120000 ORDER BY version` lists the migrations that committed, of `20261108120000`, `20261109120000`, `20261112120000`, `20261112120001` and `20261112120002`. No rows: nothing was applied, 0.9.10 or 0.9.11 keeps working, pod restarts included; find what held the locks and re-run the upgrade. One to four rows: the migrations listed are done and the next one didn't commit (usually its lock loop gave up, or Helm's timeout fired while the Job still ran); find what held its locks and re-run the upgrade now, which applies only what is left. All five rows, `20261112120002` included: every migration is done (usually Helm's timeout fired while the Job still ran); re-run the upgrade now. Re-run it soon in both cases, since 0.9.10 and 0.9.11 pods that restart crash-loop on a database with a migration they don't know. For the same reason, rolling back to 0.9.10 or 0.9.11 needs a database restore once any of the five has committed, and during a normal upgrade a 0.9.10 or 0.9.11 pod that restarts after that crash-loops until a new pod replaces it.

The release adds three chart values, which the [page history builder](#operational-notes) note explains:

```yaml
history:
  workers: 1                # KNOT_HISTORY_WORKERS; 0 turns the builder off on a replica
  replayMaxBytes: 4194304   # KNOT_HISTORY_REPLAY_MAX_BYTES; at least 1048576
  cpuShare: 0.5             # KNOT_HISTORY_CPU_SHARE; above 0, at most 1
```

After the new pods start, every page's history is built once, in the background: each replica's builder replays the stored edits of every page, pages edited longest ago first (pages not edited since the 0.9.10 upgrade come first), and writes its entries and checkpoints. Nothing in the web app changes yet: the History drawer still lists stored snapshots, and the new timeline arrives in the next release. What the first build costs:

- **CPU**: each replica runs `history.workers` replays at a time (default 1), each paced to `history.cpuShare` of one core on average (default 0.5), so about half a core per replica until the build is done, out of the default 1000m limit. For orientation, in a synthetic benchmark one worker replayed about 6 to 13 MiB of stored edits per second of compute (pastes and typing), so paced at 0.5 about 3 to 6 MiB per second per replica; pages with long histories go slower, since each pass resumes from a checkpoint and replays a stretch of edits again. With CPU headroom, raise `history.cpuShare` (memory is unaffected) or `history.workers` (see memory in the builder note first).
- **Database reads**: about one read of `doc_updates` (`SELECT pg_size_pretty(pg_relation_size('doc_updates'))`) and of the baselines, spread over the build.
- **Database growth**: checkpoints of about as much again as `doc_updates`, and entries, which are small (see sizing in the builder note). Backups grow the same.

Watch it with `knot_history_builder_lag_revs` (the largest number of edits any page's history is behind; read it with `max` across replicas): it falls to 0 when the build is done, and then stays near 0, since a page's history trails its edits by about 10 s. Pages given up are left out of it and counted by `knot_history_given_up_pages`. `knot_history_rows_built_total` shows the build's pace and `knot_history_table_bytes{table}` the tables' growth. The first build needs no `--timeout` allowance: it runs after the rollout, and the pods are ready while it runs.

The old snapshot thinning (the 5 newest per page plus one a day for 30 days) is unchanged.

This release also adds [rate limits and quotas](#rate-limits-and-quotas). The migration `20261112120000_quota_indexes` builds two indexes, on `documents` and on `blobs`, holding a `SHARE` lock on both while they build: writes to pages (creating, renaming, moving, archiving pages, among others) and file uploads wait meanwhile; page loads go on. It takes its locks in the same bounded attempts as the migrations above and gives up after 45 s rather than queueing writes behind a long transaction; the `--timeout` above counts its lock loop. The build reads both tables once, which are small: file contents live in a separate table (`blob_bytes`) and are not read. `SELECT pg_size_pretty(pg_relation_size('documents')), pg_size_pretty(pg_relation_size('blobs'))` shows what it reads. It runs third, after `20261109120000_doc_short_links` and before the two API-keys migrations.

The release adds these chart values; the defaults turn the rate limits on and leave quotas off:

```yaml
rateLimit:
  enabled: true          # KNOT_RATE_LIMIT_ENABLED
  scale: 1               # KNOT_RATE_LIMIT_SCALE; above 0, at most 100
  collabMaxSockets: 64   # KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS; at least 1
trustedProxies: []       # KNOT_TRUSTED_PROXIES; your ingress controller's addresses
quotas:                  # 0 = off
  pagesPerUser: 0
  pagesPerWorkspace: 0
  blobBytesPerUser: 0
  blobBytesPerWorkspace: 0
alerting:
  rateLimitedPerSecond: 1  # KnotRateLimitedSustained threshold
```

- **Behind an ingress, set `trustedProxies`** to the ingress controller's addresses (see [Client IP behind a proxy](#client-ip-behind-a-proxy)). Until you do, the per-IP sign-in and share-link limits are skipped for proxied requests (counted in `knot_client_ip_unresolved_total{reason="no_trusted_proxies"}`), and failed logins from all clients still count together in one throttle, as before. Once it is set, each client gets its own sign-in throttle. An entry knot can't use (malformed, or `10.0.0.5/8` with host bits after the prefix) stops it from starting, with an error naming `KNOT_TRUSTED_PROXIES`. Behind an L4 balancer that adds no `X-Forwarded-For`, all clients share one sign-in bucket; see the same section.
- **API clients and scripts** can now get `429 rate.limited` on writes, exports, uploads and sign-in, with `Retry-After`; ones that write in bulk should wait and retry. Workspace imports above 10 000 pages, 256 MiB of page Markdown, 512 MiB of files or 20 000 file and board references are refused with `413 import.too_large`.
- **Several replicas:** the limits are per replica (see Replicas in [Operational notes](#operational-notes)).
- Keep proxy idle and read timeouts above one minute (see [Collab WebSocket limits](#collab-websocket-limits)).

### Upgrading from 0.9.12

Upgrade to the next release from 0.9.12; coming from an older release, upgrade to 0.9.12 first (see [Upgrading from 0.9.10 or 0.9.11](#upgrading-from-0910-or-0911)). The `pre-upgrade` Job applies one migration, while 0.9.12 pods keep serving: `20261113120000_audit_doc_index` builds one index, `audit_events_target_idx` on `audit_events (target_id, id)`, for the per-page [audit log](#operational-notes), and copies or rewrites nothing.

The build holds a `SHARE` lock on `audit_events`, so every write that adds an audit row waits until the migration commits: creating, renaming, moving, archiving, restoring, duplicating and purging pages (the archive retention job's purge too), marking and unmarking templates, restricting pages and changing their grants, member invites, role changes and removals, password changes (their API-key revocation trigger inserts into the table), creating and revoking API keys, from a session or with a key, and quota refusals when quotas are on (the refused request waits before its `403 quota.exceeded`). Edit saves, sign-ins and page loads never touch the table and go on. Exports wait for their audit rows too, and one that waits more than 5 s is refused with `503 audit.unavailable`, so expect refused exports while the index builds. The migration takes its lock through `knot_lock_tables`, in attempts 0.5 s apart that each wait at most 100 ms, and gives up after 45 s rather than queueing those writes behind a long transaction; as in 0.9.12 an ordinary autovacuum in the way is cancelled, and the message names up to three sessions that hold the lock. Don't upgrade while `pg_dump`, another long transaction or an anti-wraparound autovacuum runs on the database.

`SELECT pg_size_pretty(pg_relation_size('audit_events'))` shows what the build reads, once. Until this release the table grew only with the page, member, API key and quota events above, so it is usually small and the build takes moments. If yours is large, build the index by hand before the upgrade, without blocking writes: `CREATE INDEX CONCURRENTLY audit_events_target_idx ON audit_events (target_id, id)`, outside a transaction. If that fails or is cancelled it leaves an invalid index, which the migration would keep (it skips the build when an index of that name exists): `DROP INDEX CONCURRENTLY audit_events_target_idx` and run it again, and check `SELECT indisvalid FROM pg_index WHERE indexrelid = 'audit_events_target_idx'::regclass` says `t` before upgrading. The migration then only takes its lock briefly and commits.

The `--timeout` needs one lock loop: in the worst case that still succeeds the loop spends up to 45 s getting its lock once, and two Job attempts each give up after another 45 s: 3 × 45 s, plus about 30 s of restart backoff, the Job pod's image pull and container starts, and the build itself on a large table. Pass `helm upgrade --timeout` of at least 3 minutes plus the time your rollout usually takes; Helm's default of 5 minutes leaves 2 for the rollout.

If the upgrade halts, `SELECT version FROM _sqlx_migrations WHERE version = 20261113120000` tells whether the migration committed. No row: nothing was applied and 0.9.12 keeps working, pod restarts included; find what held the lock and re-run the upgrade. One row: the migration is done (usually Helm's timeout fired while the Job still ran); re-run the upgrade now, since 0.9.12 pods that restart crash-loop on a database with a migration they don't know. For the same reason, rolling back to 0.9.12 needs a database restore once it has committed, and during a normal upgrade a 0.9.12 pod that restarts after that crash-loops until a new pod replaces it.

From this release every file export adds one `audit_events` row per exported page, and exports fail closed: see **Audit log** in [Operational notes](#operational-notes), and alert on `knot_audit_write_failures_total` before you upgrade.

## Operational notes

- **Healthchecks** — readiness `/api/readyz`, liveness `/api/healthz`. Disable with `probes.enabled=false` if you want to debug a wedged pod.
- **Image size** — the runtime image is `scratch` + a static musl binary (mimalloc allocator). Expect ~20 MB.
- **Resources** — defaults `100m/128Mi` requests, `1000m/512Mi` limits. Right-size based on workspace volume.
- **Replicas** — knot's CRDT room actor is per-pod and discovers peers via Postgres `LISTEN/NOTIFY`, so `replicaCount > 1` works without sticky sessions. Each room is hosted by exactly one pod at a time; failover takes one TCP roundtrip. Rate limits, the login and password throttles and the collab socket cap are kept in each replica's memory: the Service balances requests across pods without affinity, so a user or client spread over N replicas gets up to N times each limit. To hold the rate-limit totals, divide with `rateLimit.scale` (for example `0.5` with two replicas); the scale doesn't change the login and password throttles or the socket cap (lower `rateLimit.collabMaxSockets` for that). Quotas are counted in Postgres and need no change.
- **Notifications** — every replica checks for overdue tasks and deletes old notifications every 15 minutes, with nothing to configure and no coordination needed between replicas. Read notifications are deleted after 90 days, all after 180, and a page's go with it when it is deleted for good. They hold short excerpts (comment text, task text), so an excerpt of a deleted comment stays in its recipients' inboxes until then. `knot_notifications_emitted_total{kind}` counts them as they are written.
- **Rate limits and quotas** — worth an alert: a sustained `knot_rate_limited_total{bucket}` (the chart's `KnotRateLimitedSustained`, ticket severity, at `alerting.rateLimitedPerSecond`, default 1 refusal a second per bucket for 30 minutes); normal use doesn't reach the limits, so look for the script or client in the log line written when a user or client starts being limited before raising `rateLimit.scale`. `knot_client_ip_unresolved_total{reason="no_trusted_proxies"}` above 0 means knot is behind a proxy and `trustedProxies` is empty; `reason="untrusted_peer"` is a peer outside the list sending `X-Forwarded-For` (a proxy left out, or a client sending the header itself). Both count only while rate limits are on. `knot_quota_exceeded_total{quota,scope}` shows users at a quota; `audit_events` has a `quota.exceeded` row per user, quota and scope at most every 10 minutes per replica (on the user, so not on any page's audit log; see **Audit log** below). See [Rate limits and quotas](#rate-limits-and-quotas).
- **Audit log** — knot records who did what in the `audit_events` table, and each page shows its own rows as the page's audit log (`/doc/<id>/audit`, `GET /api/docs/{id}/audit`) to that page's editors and owners (an API key needs `read`, and its user Editor+ on the page). Recorded on the page: every file export with the person who made it (Word and OpenDocument one row each; a Markdown zip one row per page in it, so a subpage export writes one per included page and a workspace export one per page the exporter can read, all sharing one `data->>'export_id'`), creating, renaming, moving, duplicating, archiving, restoring and purging the page, templates, restricting it and changing its grants (from this release with the principal, role and inherit flag). An export made with an API key names the key's user, as every audit row does, and also keeps the key's id in `data->>'api_key_id'`; the page's log shows only that a key was used, and marks bots as bots. Not recorded: page views, the Markdown view and `GET /api/docs/{id}/markdown`, attachment downloads, public link views, failed or refused exports, `HEAD` requests and the diagram preflight. Exports from before this release, and from old pods during the rolling upgrade, were not recorded. Member changes (`workspace.member.*`), API key changes (`api-key.create`, `api-key.revoke`, `api-key.revoke-cascade`) and quota refusals (`quota.exceeded`) are in the table too, on the user, and no page shows them; read them with SQL. File names, IP addresses and user agents are not stored, and API keys never are (only their ids). Growth: one row per exported page per export, on top of the page, access, member and key changes; the exports are capped by the export rate limit (20 a minute per user). There is no retention: rows stay until you delete them, also after a page is purged, and a person's rows keep their user id. Exports fail closed: the file is sent only once its rows are written, so when the database can't take inserts into `audit_events`, every Word, OpenDocument and Markdown zip export, with a session or a key, is refused with `503 audit.unavailable` and no file until it can. An insert that hangs instead of failing (a lock on `audit_events`, a lost synchronous standby) is given up on after 5 s and refused the same way, so it doesn't run the export into the 30 s request timeout. Worth an alert: `knot_audit_write_failures_total{action}`, the failed audit writes, one per failed insert however many rows it held. For `action="doc.export"` each is one refused export, however many pages it covered; for member invites, role changes that don't demote an owner, API key creation and revocation and quota refusals the request went on without its audit row. The warning log names the action. Page and access changes write their row in their own transaction, so a failure there fails the change instead. So do member removals, owner demotions and password changes: a database trigger writes their `api-key.revoke-cascade` row in the same transaction, and its insert runs even when no key is revoked, so while `audit_events` refuses inserts these changes fail (a lock on it makes them wait), and the metric doesn't count them.
- **Request body size** — image uploads and Word/OpenDocument exports (which upload the page's diagram pictures) send up to 10 MiB. A reverse proxy or ingress in front of knot must allow request bodies of at least 10 MiB. With ingress-nginx (default 1 MiB), set `nginx.ingress.kubernetes.io/proxy-body-size: 10m` in `ingress.annotations`.
- **Storage growth from page history** — the edit log is kept forever, so size it as rows/day × average row bytes × ~1.6 (tuple, index and TOAST overhead). Backups and point-in-time recovery grow at the same rate. `knot-server history-stats [--days N]` (read-only, no migration; `--days` defaults to 30, minimum 1) prints the inputs. It needs the same `KNOT_*` environment as the server (config validation requires `KNOT_SESSION_KEY`, for one), so run it inside a knot pod: `kubectl exec -n knot deploy/knot -c knot -- /knot-server history-stats`. The Deployment is named as `knot.fullname` in `templates/_helpers.tpl` computes it: `fullnameOverride` when it is set; otherwise `<release>` when the release name contains the chart's name (`nameOverride` when set, else `knot`), as with `helm upgrade knot` above, and `<release>-<name>` when it doesn't; cut to 63 characters, then a trailing `-` removed. Its per-day rates cover the last `--days` days, and before the upgrade the old cleanup deleted edits, so run it at least 30 days after upgrading, or pass a `--days` below the days since the upgrade. To get rows/day, run it twice, some days apart: the difference in `update_rows` divided by the days between the runs is rows/day, and `update_bytes_avg` is the average row bytes. It also prints the current size of `doc_updates`. From 0.9.10 rows are smaller: each holds only what its edit changed (a reconnecting editor's resend no longer stores again what the page already has), so measure rows/day and row bytes again after upgrading. From the release after 0.9.11 it also prints `checkpoint_rows`, `checkpoint_bytes_total` and `entry_rows`, the page history builder's tables (next note).
- **Page history builder** — every replica runs a background builder that replays each edited page's stored edits outside the editor and records the page's history: one entry per person's editing session (closed after 20 idle minutes, with the words added and removed), plus restores, copies, imports, templates, the versions from before the 0.9.9 upgrade, and changes that reached the page without being stored as an edit, which no one is credited for. A page's history trails its edits by about 10 s (by up to 200 edits while it is edited without a pause). Replicas share the work through a short lease per page in `doc_history_state`, so no two build one page at once, and no lock is held while a page replays: archiving, restoring, moving or purging a page never waits for the builder.
  - **CPU** — `history.workers` (default 1, `KNOT_HISTORY_WORKERS`) is how many pages a replica replays at once. Each worker is a replay thread that can use a whole core of the pod's CPU limit, which it shares with the web server and its editing sessions; hence the default of 1, and each worker's pacing: after each page it sleeps in proportion to its compute time, so that on average it uses `history.cpuShare` of a core (default 0.5, `KNOT_HISTORY_CPU_SHARE`, above 0 and at most 1). Raise either only with CPU headroom.
  - **Memory** — a replay holds up to about 40 times its input in memory, the input being the page's state plus the edits and snapshots it reads. That is the costliest case a synthetic benchmark found: a page of interleaved one-character typing by three people, replayed from its stored state with few edits on top (one byte of such a state takes about 38 bytes in memory; edits cost less, and pastes need about 5 times their bytes). `history.replayMaxBytes` (default 4194304, 4 MiB, `KNOT_HISTORY_REPLAY_MAX_BYTES`, at least 1048576) caps that input, for building history and for word counts alike, so peak builder memory per pod is up to about `workers × 40 × replayMaxBytes`, and `resources.limits.memory` needs at least 256 MiB for the web server on top. At the defaults that is 160 MiB, which fits the default 512Mi limit with one worker only: two workers need 320 MiB, so a limit of at least 576 MiB (say 640Mi). A page whose input exceeds the cap is given up as too large (below): raise `replayMaxBytes` together with `resources.limits.memory`, or lower `history.workers` to make room, then re-enable the page. Snapshots of pages above about half the cap are not checked for unsaved changes (`knot_history_snapshots_unchecked_total{reason="too_large"}`).
  - **Off switch** — `history.workers: 0` turns the builder (and its table-size sampler) off on that replica, for example during an incident in which a page's replay keeps killing pods. Other replicas go on building; with every replica at 0, history waits until the builder is back, and nothing is lost meanwhile.
  - **Pages given up** — a page whose history cannot be built is given up: after ten failed passes, or the third time its stored starting state is found damaged (`reason="failures"`; each earlier time rebuilds the page, and the count runs until the page is given up or a later knot release changes the builder, so rebuilds that worked count too), when its replay input exceeds `replayMaxBytes` (`too_large`), when its replay hangs while writing a checkpoint (at once) or times out twice in a row with nothing to blame at the second (`timeout`), or when it killed the process three times in a row (`crash`, at the third). Each is counted by `knot_history_pages_given_up_total{reason}` and logged at error level as `history page given up`, with the doc id and the reason; `knot_history_given_up_pages` shows how many are given up now, and `SELECT doc_id, last_error FROM doc_history_state WHERE retry_after = 'infinity'` lists them. A given-up page's history so far stays listed (a page given up before its first build still lists its versions from before the 0.9.9 upgrade), but it gets no new entries. Once the cause is fixed, re-enable one with `UPDATE doc_history_state SET failures = 0, timeouts = 0, crashes = 0, retry_after = NULL WHERE doc_id = '…'`. A later knot release whose builder changed retries given-up pages by itself.
  - **Sizing** — on top of the edit log (rows/day × average row bytes × ~1.6, see the note above), the builder stores checkpoints of about as much again (0.83 times the edit log in a synthetic benchmark), plus the entries, which are small. Backups grow the same. `history-stats` prints the edit log's size, the checkpoints' size and the number of entries.
  - **Alerts** — worth one: `knot_history_build_failures_total` (warning log `history build failed`, with the doc id: a pass failed and is retried after a backoff of up to an hour; when it keeps failing, or the error log `history rev gap` appears, report it with the doc id); `knot_history_pages_given_up_total` (above); `knot_history_rows_unapplied_total{reason}` (stored edits the replay could not apply; their content may show up as a change no one is credited for; report it); `knot_history_gap_total` (edits that reached the page without being stored, credited to nobody; a steady rate is worth reporting, together with `knot_room_updates_persisted_raw_total`); `knot_history_snapshots_unchecked_total{reason}` (snapshots the check for unsaved changes could not use; `reason="kept_gap"`, with the error log `history gap checkpoint dropped: its content leaves history` and the doc id, is the one case where such changes leave a page's history; report it); `knot_history_replay_timeouts_total{phase}` (error log `history replay timed out`) and `knot_history_replay_threads_stalled`: a timed-out replay keeps a builder worker and a CPU core busy until it ends or the pod restarts, so restart the pod when the gauge stays above 0; and a `knot_history_builder_lag_revs` that does not fall.
  - **Measure your instance** — the builder's defaults were tuned on synthetic data. A few weeks after upgrading, run `knot-server history-stats` (see the note above) and send its output to the maintainers: real numbers tell whether the defaults fit.

## Uninstall

```bash
helm uninstall knot -n knot
```

The chart does not delete data — drop the Postgres database manually if you want a clean wipe.
