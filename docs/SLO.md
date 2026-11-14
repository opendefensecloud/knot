# Service-Level Objectives — knot

This document defines the SLOs an operator should hold knot to in production. The targets are conservative defaults; tighten them once you have a real traffic profile.

## Scope

Targets apply to a single knot deployment serving a single workspace. Multi-region or multi-workspace deployments aggregate the per-instance SLOs.

## Availability

| Service area | Target | Window |
|---|---|---|
| `/api/*` (excluding `/api/healthz`, `/api/readyz`) | **99.5 %** successful (2xx or 3xx) | 30-day rolling |
| `/collab/{doc_id}` WebSocket upgrades | **99.5 %** accepted (101 Switching Protocols) | 30-day rolling |
| `/auth/login`, `/auth/setup`, `/auth/password` | **99 %** successful | 30-day rolling |
| `/auth/oidc/*` | follows the IdP's own SLO, never tighter than 99 % | 30-day rolling |

A response with status `5xx` or a dropped TCP connection counts against the budget. `4xx` does not (user error), and neither does a `429 rate.limited` refusal or a refused collab WebSocket upgrade (429): a client past a rate limit is not an outage. Watch those separately (below).

PromQL — error rate:

```promql
sum(rate(knot_http_requests_total{status_class="5xx",route!~"/api/health.*"}[5m]))
/
clamp_min(sum(rate(knot_http_requests_total{route!~"/api/health.*"}[5m])), 1)
```

## Latency

| Endpoint class | Target | Notes |
|---|---|---|
| `GET /api/docs`, `GET /api/workspace/*`, `GET /auth/session` | **P95 < 100 ms**, **P99 < 250 ms** | Pure read-side; should be a single SQL query |
| `POST/PATCH/DELETE /api/docs/*`, `POST /api/workspace/members*` | **P95 < 250 ms**, **P99 < 500 ms** | Single-row mutation + audit insert |
| `POST /auth/login`, `POST /auth/password` | **P95 < 500 ms**, **P99 < 1 s** | Argon2id verify dominates |
| `POST /auth/setup` | **P95 < 2 s**, **P99 < 5 s** | One-time first-run; Argon2id hash + workspace bootstrap |
| `GET/POST /api/docs/{id}/markdown` | **P95 < 1 s**, **P99 < 3 s** | Markdown round-trip via room actor |

PromQL — overall P95:

```promql
histogram_quantile(0.95, sum(rate(knot_http_request_duration_seconds_bucket[5m])) by (le))
```

Per-route P95 (use the dashboard variable):

```promql
histogram_quantile(0.95,
  sum(rate(knot_http_request_duration_seconds_bucket{route=~"$route"}[5m])) by (le))
```

## CRDT convergence

| Signal | Target |
|---|---|
| Peer update visible on a second connected client | **P95 < 1 s** end-to-end |
| Local update durable in Postgres (write-ahead) | **P95 < 100 ms** after the WS frame is received |

Convergence is not directly measured by a server-side metric — it's bounded by the WS upgrade latency + the bus `LISTEN/NOTIFY` round-trip + the receiving client's render. Treat as a synthetic / end-to-end signal (the `e2e/flows/two-users-converge.spec.ts` guard catches the obvious regressions in CI).

## Resource limits

These aren't SLOs but operating bounds:

- **DB pool busy** — alert if `(pool_size − pool_idle) / pool_size > 0.8` for 5 minutes. Default pool: 16 connections per replica (`KNOT_DB_MAX_CONNECTIONS` / chart `database.maxConnections`).
- **Active rooms per pod** — soft cap at 1000. Beyond that, raise `room_idle_evict_sec` lower so cold rooms unload.
- **Rate-limited requests** — normal use never reaches a rate limit, so a sustained `knot_rate_limited_total{bucket}` means a script or misbehaving client: ticket if any bucket refuses more than 1 request a second for 30 minutes (the chart's `KnotRateLimitedSustained`, threshold `alerting.rateLimitedPerSecond`). Limits: `KNOT_RATE_LIMIT_ENABLED`, `KNOT_RATE_LIMIT_SCALE`, `KNOT_RATE_LIMIT_COLLAB_MAX_SOCKETS` / chart `rateLimit.*`; per replica. `knot_client_ip_unresolved_total{reason="no_trusted_proxies"}` above 0 means knot is behind a proxy and `KNOT_TRUSTED_PROXIES` / chart `trustedProxies` is empty (the per-IP limits are skipped for those requests); `reason="untrusted_peer"` counts a peer outside the list that sent `X-Forwarded-For`, a proxy left out or a client sending the header itself. Both count only while rate limits are on.
- **Quotas** — `knot_quota_exceeded_total{quota,scope}` counts refusals once a quota (`KNOT_QUOTA_*` / chart `quotas.*`, off by default) is reached; it is a capacity signal for the operator, not an error.
- **Collab throttling** — `knot_collab_throttled_total{kind,action}` and `knot_collab_throttle_wait_seconds`: a user's edits waiting for their WebSocket budget. They arrive later, never lost; a steady rate means a user is pasting or scripting far above typing speed.
- **Memory** — knot-server with mimalloc keeps RSS proportional to active rooms + connected clients. The `requests: 128Mi / limits: 512Mi` default in the chart fits a few hundred active rooms; right-size for your scale.

## Error budget

Each 30-day window allows roughly:

| Target | Budget |
|---|---|
| 99.5 % availability | **3.6 hours** of downtime / failure |
| 99 % availability | **7.2 hours** of downtime / failure |

When more than **25 %** of the budget is burned in a rolling 7-day window, freeze non-critical changes (refactors, feature work) until the budget recovers. Bug fixes and reliability improvements always proceed.

## Burn-rate signals

These map to the `PrometheusRule` shipped in the chart
(`deploy/helm/knot/templates/prometheusrule.yaml`, enabled via `alerting.enabled=true`):

- **Fast burn** — error rate × 14.4 (= ~2 % of budget in 1 hour) → page immediately.
- **Slow burn** — error rate × 6 (= ~5 % of budget in 6 hours) → ticket / Slack within business hours.
- **Latency burn** — P95 above target for 30 consecutive minutes → ticket.
- **Sustained rate limiting** — any bucket refusing more than `alerting.rateLimitedPerSecond` (default 1) requests a second for 30 minutes → ticket (`KnotRateLimitedSustained`, group `knot.limits`).

## Pointers

- **Dashboard** — `deploy/grafana/knot.json` (import into Grafana 9+).
- **Metric source of truth** — `crates/knot-obs/src/metrics.rs` (`describe_*` calls list every metric the server emits).
- **Health endpoints** — `/api/healthz` (liveness, no DB), `/api/readyz` (readiness, requires DB).
- **Tracing** — set `KNOT_TRACING_ENABLED=true` + `KNOT_OTLP_ENDPOINT=...` to push spans to an OpenTelemetry collector. The chart's `tracing.enabled` toggle wires these for you.

## Review

Revisit these targets quarterly or after any significant traffic-shape change. Real numbers from production should drive the next iteration — start with what's listed here, then tighten.
