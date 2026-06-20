# knot Helm chart

Production install of [knot](https://github.com/trevex/knot) — a self-hosted, CRDT-backed collaborative knowledge base.

## Prerequisites

- Kubernetes 1.27+
- Helm 3.13+
- External Postgres 16 (the chart does **not** bundle one). The connection user needs `CREATE TABLE` rights in its database.
- An ingress controller (e.g. ingress-nginx, traefik) if you want external access.
- Optional: cert-manager for TLS, an OIDC provider (Dex, Keycloak, Okta, ...) if you want SSO.

## Quick install

```bash
helm install knot ./deploy/helm/knot \
  --create-namespace --namespace knot \
  --set image.repository=ghcr.io/trevex/knot \
  --set image.tag=v0.1.0 \
  --set database.url='postgres://knot:knot@db.svc.cluster.local:5432/knot' \
  --set session.key="$(openssl rand -base64 32)" \
  --set baseUrl=https://knot.example.com \
  --set ingress.enabled=true \
  --set ingress.hosts[0].host=knot.example.com \
  --set ingress.hosts[0].paths[0].path=/ \
  --set ingress.hosts[0].paths[0].pathType=Prefix
```

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
```

Tested IdPs:
- **Dex** with the `password` connector (the dev-compose setup at `deploy/compose/dex/`).
- Any OIDC-conformant provider exposing `openid email profile groups` (Keycloak, Okta, Auth0, Google).

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

## Upgrading

```bash
helm upgrade knot ./deploy/helm/knot -n knot -f my-values.yaml
```

The `pre-upgrade` Job re-runs `knot-server migrate` before the new pods land, so any new migrations in the release are applied first. Failed migrations halt the upgrade — fix the cause and re-run.

## Operational notes

- **Healthchecks** — readiness `/api/readyz`, liveness `/api/healthz`. Disable with `probes.enabled=false` if you want to debug a wedged pod.
- **Image size** — the runtime image is `scratch` + a static musl binary (mimalloc allocator). Expect ~20 MB.
- **Resources** — defaults `100m/128Mi` requests, `1000m/512Mi` limits. Right-size based on workspace volume.
- **Replicas** — knot's CRDT room actor is per-pod and discovers peers via Postgres `LISTEN/NOTIFY`, so `replicaCount > 1` works without sticky sessions. Each room is hosted by exactly one pod at a time; failover takes one TCP roundtrip.

## Uninstall

```bash
helm uninstall knot -n knot
```

The chart does not delete data — drop the Postgres database manually if you want a clean wipe.
