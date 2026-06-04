# Plans + outcomes

This directory is knot's planning log. Every non-trivial change lands as a **plan** (an upfront task-by-task implementation document) and, on merge, gets an **outcome doc** capturing what landed, what was non-obvious, and what's still deferred.

## Plans landed

| # | Date | Topic | Plan | Outcome |
|---|---|---|---|---|
| 1 | 2026-06-01 | Spike (yrs + y-websocket round-trip) | — | — |
| 2 | 2026-06-01 | Config + observability skeleton | — | — |
| 3 | 2026-06-01 | Auth (local + OIDC discovery) | — | — |
| 4 | 2026-06-01 | Documents + ACL | — | — |
| 5 | 2026-06-02 | CRDT Room Actor + Persistence | — | [2026-06-02-plan5-outcome.md](research/2026-06-02-plan5-outcome.md) |
| 6 | 2026-06-02 | Frontend Shell | [plans/2026-06-02-frontend-shell.md](plans/2026-06-02-frontend-shell.md) | [2026-06-02-plan6-outcome.md](research/2026-06-02-plan6-outcome.md) |
| 8 | 2026-06-02 | Auth Completion (change pw, invite-with-pw, OIDC e2e) | [plans/2026-06-02-auth-completion.md](plans/2026-06-02-auth-completion.md) | [2026-06-03-plan8-outcome.md](research/2026-06-03-plan8-outcome.md) |
| 9 | 2026-06-03 | Deployment (Helm + multi-arch image) | [plans/2026-06-03-deployment.md](plans/2026-06-03-deployment.md) | [2026-06-03-plan9-outcome.md](research/2026-06-03-plan9-outcome.md) |
| 10 | 2026-06-03 | Observability | [plans/2026-06-03-observability.md](plans/2026-06-03-observability.md) | [2026-06-03-plan10-outcome.md](research/2026-06-03-plan10-outcome.md) |
| 7 | 2026-06-03 | UI Polish | [plans/2026-06-03-ui-polish.md](plans/2026-06-03-ui-polish.md) | [2026-06-03-plan7-outcome.md](research/2026-06-03-plan7-outcome.md) |
| 11 | 2026-06-03 | Developer Experience | [plans/2026-06-03-developer-experience.md](plans/2026-06-03-developer-experience.md) | [2026-06-03-plan11-outcome.md](research/2026-06-03-plan11-outcome.md) |
| 12 | 2026-06-03 | Production Hardening | [plans/2026-06-03-production-hardening.md](plans/2026-06-03-production-hardening.md) | [2026-06-03-plan12-outcome.md](research/2026-06-03-plan12-outcome.md) |
| 13 | 2026-06-03 | File Uploads & Attachments | [plans/2026-06-03-file-uploads.md](plans/2026-06-03-file-uploads.md) | [2026-06-03-plan13-outcome.md](research/2026-06-03-plan13-outcome.md) |
| 13.5 | 2026-06-03 | Runtime-Selected S3 Backend | [plans/2026-06-03-blob-runtime-s3.md](plans/2026-06-03-blob-runtime-s3.md) | [2026-06-03-plan13.5-outcome.md](research/2026-06-03-plan13.5-outcome.md) |
| 14 | 2026-06-03 | Full-Text Search | [plans/2026-06-03-search.md](plans/2026-06-03-search.md) | [2026-06-03-plan14-outcome.md](research/2026-06-03-plan14-outcome.md) |
| 14.5 | 2026-06-03 | DocPage Stale-State Fix | [plans/2026-06-03-docpage-fix.md](plans/2026-06-03-docpage-fix.md) | [2026-06-03-plan14.5-outcome.md](research/2026-06-03-plan14.5-outcome.md) |
| 12.5 | 2026-06-03 | Chaos: WS Reconnect via toxiproxy | [plans/2026-06-03-chaos-ws-reconnect.md](plans/2026-06-03-chaos-ws-reconnect.md) | [2026-06-03-plan12.5-outcome.md](research/2026-06-03-plan12.5-outcome.md) |
| 16 | 2026-06-03 | Prefix Search | [plans/2026-06-03-prefix-search.md](plans/2026-06-03-prefix-search.md) | [2026-06-03-plan16-outcome.md](research/2026-06-03-plan16-outcome.md) |
| 15 | 2026-06-03 | Mobile / Responsive | [plans/2026-06-03-mobile.md](plans/2026-06-03-mobile.md) | [2026-06-03-plan15-outcome.md](research/2026-06-03-plan15-outcome.md) |
| 17 | 2026-06-03 | Public Share Links | [plans/2026-06-03-share-links.md](plans/2026-06-03-share-links.md) | [2026-06-03-plan17-outcome.md](research/2026-06-03-plan17-outcome.md) |
| 20 | 2026-06-03 | Doc History & Restore | [plans/2026-06-03-doc-history.md](plans/2026-06-03-doc-history.md) | [2026-06-03-plan20-outcome.md](research/2026-06-03-plan20-outcome.md) |
| 19 | 2026-06-03 | Comments & Mentions | [plans/2026-06-03-comments.md](plans/2026-06-03-comments.md) | [2026-06-03-plan19-outcome.md](research/2026-06-03-plan19-outcome.md) |
| 22 | 2026-06-03 | UI Modernization | [plans/2026-06-03-ui-modernization.md](plans/2026-06-03-ui-modernization.md) | [2026-06-03-plan22-outcome.md](research/2026-06-03-plan22-outcome.md) |
| 25 | 2026-06-03 | Excalidraw Boards | [plans/2026-06-03-excalidraw-boards.md](plans/2026-06-03-excalidraw-boards.md) | [2026-06-03-plan25-outcome.md](research/2026-06-03-plan25-outcome.md) |

> Plan numbers reflect execution order, not always file date. Plans 1–4 predate the plan-driven workflow and don't have outcome docs.

## On deck

See each outcome doc's "Carryforward" section for what that plan's owner recommended next. Common candidates:

- **Plan 12 — Production hardening.** Rate-limit `/auth/login` + `/auth/password`, NetworkPolicy in chart, image push CI on tag, PrometheusRule template, WS reconnect e2e.
- **Plan 13 — File uploads / images.** Notion-style image embeds. Needs server-side blob storage decision (Postgres LO vs S3-compatible).
- **Plan 14 — Full-text search.** Postgres FTS over markdown cache + tantivy index for richer queries.

## How to add a plan

1. **Brainstorm** scope. Decide what's in and what's deferred.
2. **Write the plan** via the `superpowers:writing-plans` skill → `plans/YYYY-MM-DD-<topic>.md`. Tasks are bite-sized (2–5 minutes of work), show exact code, name exact files.
3. **Execute** via `superpowers:subagent-driven-development`. Fresh implementer per task, two-stage review (spec + code quality).
4. **On merge,** write `research/YYYY-MM-DD-<topic>-outcome.md` capturing status, gates, what's deferred, carryforward.
5. **Add a row** to the table above.
